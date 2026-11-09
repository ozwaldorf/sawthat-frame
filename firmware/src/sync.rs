//! On-device data sync: fetch concerts from SawThat, resolve album art via Deezer,
//! and render artwork for both orientations.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embassy_time::{Duration, Instant, with_timeout};
use embedded_io_async::Read;
use embedded_nal_async::{Dns, TcpConnect};
use log::info;
use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::request::{Method, RequestBuilder};
use sawthat_render::concerts::{self, Album, Concert};
use sawthat_render::{
    ConcertInfo, Renderer, RgbImage, cover_size, decode_jpeg, pack_image, primary_color,
};

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
                Ok(0) => break,
                Ok(n) if body.len() + n > max => return Err(SyncError::TooLarge),
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    info!("Body read failed: {:?}", e);
                    return Err(SyncError::Network);
                }
            }
        }
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

/// Album art closest to the concert date, falling back to the artist picture
async fn cover_url<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
    albums: &mut AlbumCache,
    concert: &Concert,
) -> Option<String> {
    if !albums.contains_key(&concert.band_name) {
        let list = match net
            .get(&concerts::deezer_search_url(&concert.band_name), MAX_JSON)
            .await
        {
            Ok(body) => match concerts::parse_artist_id(&body) {
                Some(id) => match net.get(&concerts::deezer_albums_url(id), MAX_JSON).await {
                    Ok(body) => concerts::parse_albums(&body),
                    Err(e) => {
                        info!("Deezer albums failed for {}: {:?}", concert.band_name, e);
                        Vec::new()
                    }
                },
                None => Vec::new(),
            },
            Err(e) => {
                info!("Deezer search failed for {}: {:?}", concert.band_name, e);
                Vec::new()
            }
        };
        albums.insert(concert.band_name.clone(), list);
    }

    concerts::closest_album(&albums[&concert.band_name], &concert.date)
        .map(|a| a.cover.clone())
        .or_else(|| (!concert.picture.is_empty()).then(|| concert.picture.clone()))
}

/// Download and render a concert for one layout, returning the packed image
pub async fn render_concert<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
    albums: &mut AlbumCache,
    renderer: &Renderer,
    concert: &Concert,
    (width, height): (u32, u32),
) -> Result<Vec<u8>, SyncError> {
    let url = cover_url(net, albums, concert)
        .await
        .ok_or(SyncError::NoImage)?;

    // Deezer covers are fetched at the layout's covering size; others are used as-is
    let url = concerts::sized_cover_url(&url, cover_size((width, height)));
    info!("Artwork for {}: {}", concert.path, url);
    let src = fetch_image(net, &url).await?;

    let start = Instant::now();
    let color = primary_color(&src, &renderer.adjuster);
    let date = concert.formatted_date();
    let info = ConcertInfo {
        band_name: &concert.band_name,
        date: &date,
        venue: &concert.venue,
    };
    let packed = pack_image(
        &renderer.render(&src, &color, &info, width, height),
        width,
        height,
    );
    info!(
        "Rendered {}x{} from {}x{} in {}ms",
        width,
        height,
        src.width,
        src.height,
        start.elapsed().as_millis()
    );
    Ok(packed)
}

/// Download and decode a JPEG
async fn fetch_image<T: TcpConnect, D: Dns>(
    net: &mut Net<'_, T, D>,
    url: &str,
) -> Result<RgbImage, SyncError> {
    let jpeg = net.get(url, MAX_IMAGE).await?;
    let start = Instant::now();
    let src = decode_jpeg(&jpeg).map_err(|_| SyncError::Image)?;
    info!(
        "Decoded {}x{} in {}ms",
        src.width,
        src.height,
        start.elapsed().as_millis()
    );
    embassy_futures::yield_now().await;
    Ok(src)
}
