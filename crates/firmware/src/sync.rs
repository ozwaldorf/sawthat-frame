//! On-device data sync: fetch concerts from SawThat, resolve album art via Deezer,
//! and render artwork for both orientations.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embassy_futures::join::join;
use embassy_time::{Duration, Instant, with_timeout};
use embedded_io_async::{Read, Write};
use embedded_nal_async::{Dns, TcpConnect};
use log::info;
use reqwless::TryBufRead;
use reqwless::client::{HttpClient, HttpResource, TlsConfig, TlsVerify};
use reqwless::request::{Method, RequestBuilder};
use reqwless::response::Response;
use sawthat_render::concerts::{self, Album, Concert};
use sawthat_render::{ConcertInfo, Renderer, cover_size, decode_jpeg, primary_color};

use crate::{config, ota};

/// TLS buffer sizes
pub const TLS_READ_BUF_SIZE: usize = 16640;
pub const TLS_WRITE_BUF_SIZE: usize = 4096;

/// TLS seed for random number generation
const TLS_SEED: u64 = 0x1234567890abcdef;

/// Maximum time a request stage (connect, response headers, one body read) may stall
const STAGE_TIMEOUT: Duration = Duration::from_secs(8);
/// Attempts per request before giving up
const ATTEMPTS: usize = 3;

/// Response size limits
const MAX_JSON: usize = 1024 * 1024;
const MAX_IMAGE: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub enum SyncError {
    Network,
    Timeout,
    Http(u16),
    TooLarge,
    Image,
    NoImage,
    /// The SawThat profile page has no user ID
    NoUserId,
    /// The installer manifest has no valid version
    BadManifest,
}

/// Run one request stage, failing if it makes no progress within `STAGE_TIMEOUT`
async fn stage<F: core::future::Future>(f: F) -> Result<F::Output, SyncError> {
    with_timeout(STAGE_TIMEOUT, f)
        .await
        .map_err(|_| SyncError::Timeout)
}

/// HTTP(S) client resources
pub struct Net<'a, T, D> {
    pub tcp: &'a T,
    pub dns: &'a D,
    pub tls_read: &'a mut [u8],
    pub tls_write: &'a mut [u8],
    /// Second TLS buffer pair, for a cover connection open alongside the API one
    pub cover_tls_read: &'a mut [u8],
    pub cover_tls_write: &'a mut [u8],
}

impl<T: TcpConnect, D: Dns> Net<'_, T, D> {
    /// GET `url` and return the body, retrying stalled or failed connections
    pub async fn get(&mut self, url: &str, max: usize) -> Result<Vec<u8>, SyncError> {
        let mut last = SyncError::Network;
        for attempt in 1..=ATTEMPTS {
            match self.get_once(url, max).await {
                Ok(body) => return Ok(body),
                Err(e @ (SyncError::Http(_) | SyncError::TooLarge)) => return Err(e),
                Err(e) => {
                    info!(
                        "GET {} attempt {}/{} failed: {:?}",
                        url, attempt, ATTEMPTS, e
                    );
                    last = e;
                }
            }
        }
        Err(last)
    }

    async fn get_once(&mut self, url: &str, max: usize) -> Result<Vec<u8>, SyncError> {
        let start = Instant::now();
        let tls = TlsConfig::new(TLS_SEED, self.tls_read, self.tls_write, TlsVerify::None);
        let mut client = HttpClient::new_with_tls(self.tcp, self.dns, tls);

        let headers = [("User-Agent", "sawthat-frame/0.1"), ("Accept", "*/*")];
        let mut request = stage(client.request(Method::GET, url))
            .await?
            .map_err(|e| {
                info!("Connect failed: {:?}", e);
                SyncError::Network
            })?
            .headers(&headers);

        let mut rx: Box<[u8; 4096]> = Box::new([0; 4096]);
        let response = stage(request.send(&mut *rx)).await?.map_err(|e| {
            info!("Request failed: {:?}", e);
            SyncError::Network
        })?;
        let body = read_body(response, max).await?;
        info!(
            "GET {}: {} bytes in {}ms",
            url,
            body.len(),
            start.elapsed().as_millis()
        );
        Ok(body)
    }
}

/// Fetch the raw SawThat bands response
pub async fn fetch_bands<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
    user_id: &str,
) -> Result<Vec<u8>, SyncError> {
    let url = format!("{}{}", concerts::SAWTHAT_BANDS_URL, user_id);
    net.get(&url, MAX_JSON).await
}

/// Web installer manifest, naming the latest release and its signed app image. Test builds may
/// point elsewhere.
const MANIFEST_URL: &str = match option_env!("SAWTHAT_MANIFEST_URL") {
    Some(url) => url,
    None => "https://ozwaldorf.github.io/sawthat-frame/manifest.json",
};
const MAX_MANIFEST: usize = 16 * 1024;
/// Largest app image, the size of an app slot
const MAX_IMAGE_SIZE: usize = 4 * 1024 * 1024;

/// Fetch the latest release from the web installer manifest, in one attempt
pub async fn fetch_manifest<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
) -> Result<ota::Update, SyncError> {
    let body = net.get_once(MANIFEST_URL, MAX_MANIFEST).await?;
    ota::parse_manifest(&body).ok_or(SyncError::BadManifest)
}

/// Download a release's app image
pub async fn download_release<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
    release: &ota::Release,
) -> Result<Vec<u8>, SyncError> {
    let base = &MANIFEST_URL[..MANIFEST_URL.rfind('/').unwrap_or(0) + 1];
    let url = format!("{base}{}", release.path);
    net.get(&url, release.size.min(MAX_IMAGE_SIZE)).await
}

/// Look up the user ID of SawThat user `name` from the avatar URL on their profile page
/// (`.../Avatar/<user id>/...`)
pub async fn resolve_user_id<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
    name: &str,
) -> Result<String, SyncError> {
    let url = format!("https://{name}.sawthat.band/");
    let page = net.get(&url, MAX_JSON).await?;
    const MARKER: &[u8] = b"/Avatar/";
    page.windows(MARKER.len())
        .position(|window| window == MARKER)
        .and_then(|at| page.get(at + MARKER.len()..at + MARKER.len() + 36))
        .and_then(|id| core::str::from_utf8(id).ok())
        .filter(|id| config::is_uuid(id))
        .map(String::from)
        .ok_or(SyncError::NoUserId)
}

/// Parse a SawThat bands response into concerts of `user_id`, most recent first
pub fn parse_concerts(json: &[u8], user_id: &str) -> Option<Vec<Concert>> {
    let concerts = concerts::parse_concerts(json, user_id).ok()?;
    info!("Parsed {} concerts ({} bytes)", concerts.len(), json.len());
    Some(concerts)
}

/// Deezer albums per band name, fetched once per sync
pub type AlbumCache = BTreeMap<String, Vec<Album>>;

/// Split an absolute URL into its origin (`https://host`) and path
fn split_url(url: &str) -> (&str, &str) {
    let host_start = url.find("://").map_or(0, |i| i + 3);
    match url[host_start..].find('/') {
        Some(i) => url.split_at(host_start + i),
        None => (url, "/"),
    }
}

/// Read a response body, failing if it exceeds `max` bytes or stalls
async fn read_body<C>(response: Response<'_, '_, C>, max: usize) -> Result<Vec<u8>, SyncError>
where
    C: Read + TryBufRead,
{
    if response.status.0 >= 400 {
        return Err(SyncError::Http(response.status.0));
    }
    if response.content_length.is_some_and(|len| len > max) {
        return Err(SyncError::TooLarge);
    }

    let mut body = Vec::with_capacity(response.content_length.unwrap_or(16 * 1024));
    let mut chunk: Box<[u8; 4096]> = Box::new([0; 4096]);
    let mut reader = response.body().reader();
    loop {
        match stage(reader.read(&mut *chunk)).await? {
            Ok(0) => return Ok(body),
            Ok(n) if body.len() + n > max => return Err(SyncError::TooLarge),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e) => {
                info!("Body read failed: {:?}", e);
                return Err(SyncError::Network);
            }
        }
    }
}

/// GET `path` over an open connection
async fn resource_get<C>(
    resource: &mut HttpResource<'_, C>,
    path: &str,
    max: usize,
) -> Result<Vec<u8>, SyncError>
where
    C: Read + Write,
{
    let start = Instant::now();
    let headers = [("User-Agent", "sawthat-frame/0.1"), ("Accept", "*/*")];
    let mut rx: Box<[u8; 4096]> = Box::new([0; 4096]);
    let response = stage(resource.get(path).headers(&headers).send(&mut *rx))
        .await?
        .map_err(|e| {
            info!("Request failed: {:?}", e);
            SyncError::Network
        })?;
    let body = read_body(response, max).await?;
    info!(
        "GET {}: {} bytes in {}ms (reused connection)",
        path,
        body.len(),
        start.elapsed().as_millis()
    );
    Ok(body)
}

/// Whether a failed request is worth retrying on a new connection
fn retryable(e: &SyncError) -> bool {
    !matches!(e, SyncError::Http(_) | SyncError::TooLarge)
}

/// Deezer search and albums for one band over an open api.deezer.com connection
async fn lookup_band<C: Read + Write>(
    resource: &mut HttpResource<'_, C>,
    band: &str,
) -> Result<Vec<Album>, SyncError> {
    let url = concerts::deezer_search_url(band);
    let body = resource_get(resource, split_url(&url).1, MAX_JSON).await?;
    let Some(id) = concerts::parse_artist_id(&body) else {
        return Ok(Vec::new());
    };
    let url = concerts::deezer_albums_url(id);
    let body = resource_get(resource, split_url(&url).1, MAX_JSON).await?;
    Ok(concerts::parse_albums(&body))
}

/// Cover URL for a concert: the album closest to (not after) the concert date, or the
/// artist picture; Deezer covers are requested at the layout's covering size
fn cover_url(albums: &AlbumCache, concert: &Concert, size: (u32, u32)) -> Option<String> {
    albums
        .get(&concert.band_name)
        .and_then(|list| concerts::closest_album(list, &concert.date))
        .map(|album| album.cover.clone())
        .or_else(|| (!concert.picture.is_empty()).then(|| concert.picture.clone()))
        .map(|url| concerts::sized_cover_url(&url, cover_size(size)))
}

/// Download covers for `order` (indices into `concerts`) one concert at a time.
///
/// Two connections stay open: one to api.deezer.com for band lookups and one for cover
/// downloads, reopened only when the cover host changes or a request fails. Each cover
/// downloads while the next concert's band is looked up on the other connection.
/// `on_cover` receives each concert index with its JPEG or the failure, in order, as soon
/// as it downloads, and returns false to stop.
pub async fn fetch_covers<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
    albums: &mut AlbumCache,
    concerts: &[Concert],
    order: &[usize],
    size: (u32, u32),
    mut on_cover: impl AsyncFnMut(usize, Result<Vec<u8>, SyncError>) -> bool,
) {
    let (mut k, mut failures, mut api_down) = (0, 0, false);

    // Hand a result to `on_cover`, returning if it asks to stop
    macro_rules! cover {
        ($i:expr, $result:expr) => {
            if !on_cover($i, $result).await {
                return;
            }
        };
    }

    // Deezer API connection, reopened after a failed lookup
    'api: while k < order.len() {
        let tls = TlsConfig::new(TLS_SEED, net.tls_read, net.tls_write, TlsVerify::None);
        let mut api_client = HttpClient::new_with_tls(net.tcp, net.dns, tls);
        let mut api = None;
        if !api_down {
            match stage(api_client.resource(concerts::DEEZER_API)).await {
                Ok(Ok(resource)) => api = Some(resource),
                _ => {
                    failures += 1;
                    if failures >= ATTEMPTS {
                        info!("Deezer unreachable, using artist pictures");
                        (api_down, failures) = (true, 0);
                    }
                    continue 'api;
                }
            }
        }

        // Look up the band of `order[k]`; evaluates to false when the API connection
        // must be reopened before retrying
        macro_rules! lookup {
            () => {{
                let band = &concerts[order[k]].band_name;
                if albums.contains_key(band) {
                    true
                } else {
                    let result = match api.as_mut() {
                        Some(api) => lookup_band(api, band).await,
                        None => Ok(Vec::new()),
                    };
                    match result {
                        Ok(list) => {
                            albums.insert(band.clone(), list);
                            failures = 0;
                            true
                        }
                        Err(e) if retryable(&e) && failures + 1 < ATTEMPTS => {
                            failures += 1;
                            false
                        }
                        Err(e) => {
                            info!("Deezer lookup failed for {}: {:?}", band, e);
                            albums.insert(band.clone(), Vec::new());
                            failures = 0;
                            true
                        }
                    }
                }
            }};
        }

        // Cover connection, reopened when the host changes or a download fails
        'cover: while k < order.len() {
            if !lookup!() {
                continue 'api;
            }
            let Some(mut url) = cover_url(albums, &concerts[order[k]], size) else {
                cover!(order[k], Err(SyncError::NoImage));
                k += 1;
                continue 'cover;
            };

            let origin = String::from(split_url(&url).0);
            let tls = TlsConfig::new(
                TLS_SEED,
                net.cover_tls_read,
                net.cover_tls_write,
                TlsVerify::None,
            );
            let mut cover_client = HttpClient::new_with_tls(net.tcp, net.dns, tls);
            let mut cover = match stage(cover_client.resource(&origin)).await {
                Ok(Ok(resource)) => resource,
                _ => {
                    failures += 1;
                    if failures >= ATTEMPTS {
                        cover!(order[k], Err(SyncError::Network));
                        (k, failures) = (k + 1, 0);
                    }
                    continue 'cover;
                }
            };

            loop {
                // Download this cover while looking up the next concert's band on the
                // API connection; a failed lookup is retried by `lookup!` before use
                let next_band = order
                    .get(k + 1)
                    .map(|&i| &concerts[i].band_name)
                    .filter(|band| !albums.contains_key(*band));
                let (download, prefetched) = join(
                    resource_get(&mut cover, split_url(&url).1, MAX_IMAGE),
                    async {
                        match (next_band, api.as_mut()) {
                            (Some(band), Some(api)) => Some(lookup_band(api, band).await),
                            _ => None,
                        }
                    },
                )
                .await;
                if let (Some(band), Some(Ok(list))) = (next_band, prefetched) {
                    albums.insert(band.clone(), list);
                }

                match download {
                    Ok(jpeg) => {
                        cover!(order[k], Ok(jpeg));
                        (k, failures) = (k + 1, 0);
                    }
                    Err(e) => {
                        failures += 1;
                        if !retryable(&e) || failures >= ATTEMPTS {
                            cover!(order[k], Err(e));
                            (k, failures) = (k + 1, 0);
                        }
                        continue 'cover;
                    }
                }

                // Advance to the next concert that has a cover
                loop {
                    if k >= order.len() {
                        break 'api;
                    }
                    if !lookup!() {
                        continue 'api;
                    }
                    match cover_url(albums, &concerts[order[k]], size) {
                        Some(next) => {
                            url = next;
                            break;
                        }
                        None => {
                            cover!(order[k], Err(SyncError::NoImage));
                            k += 1;
                        }
                    }
                }

                // Keep the connection only while covers come from the same host
                if split_url(&url).0 != origin {
                    continue 'cover;
                }
            }
        }
    }
}

/// Decode and render a cover in the stored image format
pub fn render_cover(
    renderer: &Renderer,
    jpeg: &[u8],
    concert: &Concert,
    (width, height): (u32, u32),
) -> Result<Vec<u8>, SyncError> {
    let start = Instant::now();
    let src = decode_jpeg(jpeg).map_err(|_| SyncError::Image)?;
    let color = primary_color(&src, &renderer.adjuster);
    let date = concert.formatted_date();
    let info = ConcertInfo {
        band_name: &concert.band_name,
        date: &date,
        venue: &concert.venue,
    };
    let stored = renderer.render(&src, &color, &info, width, height);
    info!(
        "Rendered {}x{} from {}x{} in {}ms",
        width,
        height,
        src.width,
        src.height,
        start.elapsed().as_millis()
    );
    Ok(stored)
}
