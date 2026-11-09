//! On-device data sync: fetch concerts from SawThat, resolve album art via Deezer,
//! and render artwork for both orientations.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

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
const MAX_JSON: usize = 512 * 1024;
const MAX_IMAGE: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub enum SyncError {
    Network,
    Timeout,
    Http(u16),
    TooLarge,
    Image,
    NoImage,
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
    info!("Fetching concerts from {}", url);
    net.get(&url, MAX_JSON).await
}

/// Parse a SawThat bands response into concerts, most recent first
pub fn parse_concerts(json: &[u8]) -> Option<Vec<Concert>> {
    let concerts = concerts::parse_concerts(json).ok()?;
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

/// Look up Deezer albums for `bands` over one api.deezer.com connection
async fn lookup_albums<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
    albums: &mut AlbumCache,
    bands: &[&str],
) {
    let (mut k, mut failures) = (0, 0);
    while k < bands.len() {
        let tls = TlsConfig::new(TLS_SEED, net.tls_read, net.tls_write, TlsVerify::None);
        let mut client = HttpClient::new_with_tls(net.tcp, net.dns, tls);
        let mut resource = match stage(client.resource(concerts::DEEZER_API)).await {
            Ok(Ok(resource)) => resource,
            _ => {
                failures += 1;
                if failures >= ATTEMPTS {
                    info!("Deezer unreachable, skipping lookup for {}", bands[k]);
                    albums.insert(String::from(bands[k]), Vec::new());
                    (k, failures) = (k + 1, 0);
                }
                continue;
            }
        };

        while k < bands.len() {
            match lookup_band(&mut resource, bands[k]).await {
                Ok(list) => {
                    albums.insert(String::from(bands[k]), list);
                    (k, failures) = (k + 1, 0);
                }
                Err(e) => {
                    failures += 1;
                    if !retryable(&e) || failures >= ATTEMPTS {
                        info!("Deezer lookup failed for {}: {:?}", bands[k], e);
                        albums.insert(String::from(bands[k]), Vec::new());
                        (k, failures) = (k + 1, 0);
                    }
                    // Reconnect for the next attempt
                    break;
                }
            }
        }
    }
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

/// Download covers for `batch` (indices into `concerts`), reusing connections.
///
/// Deezer lookups for the batch share one api.deezer.com connection; covers are then
/// downloaded over a connection kept open while consecutive covers share a host.
/// `on_cover` receives each concert index with its JPEG or the failure, in order.
pub async fn fetch_covers<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
    albums: &mut AlbumCache,
    concerts: &[Concert],
    batch: &[usize],
    size: (u32, u32),
    mut on_cover: impl AsyncFnMut(usize, Result<Vec<u8>, SyncError>),
) {
    let mut bands: Vec<&str> = Vec::new();
    for &i in batch {
        let band = concerts[i].band_name.as_str();
        if !albums.contains_key(band) && !bands.contains(&band) {
            bands.push(band);
        }
    }
    lookup_albums(net, albums, &bands).await;

    let covers: Vec<(usize, Option<String>)> = batch
        .iter()
        .map(|&i| (i, cover_url(albums, &concerts[i], size)))
        .collect();

    let (mut k, mut failures) = (0, 0);
    while k < covers.len() {
        let (i, Some(url)) = &covers[k] else {
            on_cover(covers[k].0, Err(SyncError::NoImage)).await;
            k += 1;
            continue;
        };
        let origin = split_url(url).0;

        let tls = TlsConfig::new(TLS_SEED, net.tls_read, net.tls_write, TlsVerify::None);
        let mut client = HttpClient::new_with_tls(net.tcp, net.dns, tls);
        let mut resource = match stage(client.resource(origin)).await {
            Ok(Ok(resource)) => resource,
            _ => {
                failures += 1;
                if failures >= ATTEMPTS {
                    on_cover(*i, Err(SyncError::Network)).await;
                    (k, failures) = (k + 1, 0);
                }
                continue;
            }
        };

        // Download consecutive covers from this host over the open connection
        while let Some((i, url)) = covers.get(k) {
            let Some(url) = url else {
                on_cover(*i, Err(SyncError::NoImage)).await;
                k += 1;
                continue;
            };
            let (url_origin, path) = split_url(url);
            if url_origin != origin {
                break;
            }
            match resource_get(&mut resource, path, MAX_IMAGE).await {
                Ok(jpeg) => {
                    on_cover(*i, Ok(jpeg)).await;
                    (k, failures) = (k + 1, 0);
                }
                Err(e) => {
                    failures += 1;
                    if !retryable(&e) || failures >= ATTEMPTS {
                        on_cover(*i, Err(e)).await;
                        (k, failures) = (k + 1, 0);
                    }
                    // Reconnect for the next attempt
                    break;
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
