//! Display manager for orchestrating edge service integration
//!
//! Handles the fetch → decode → display flow:
//! 1. Fetch widget data JSON from edge service
//! 2. Parse widget items
//! 3. Fetch PNG images for each item
//! 4. Decode and write to framebuffer
//! 5. Refresh the e-paper display

extern crate alloc;

use alloc::boxed::Box;
use core::fmt::Write as FmtWrite;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal::spi::SpiDevice;
use esp_println::println;
use heapless::String;

use crate::epd::{Color, Epd7in3e};
use crate::framebuffer::Framebuffer;

use crate::http::{self, HttpError, Scheme, Url};
use crate::https;
use crate::widget::{parse_widget_data, WidgetItem};

/// Size of PNG receive buffer (128KB - enough for processed e-paper images)
const PNG_BUF_SIZE: usize = 128 * 1024;
/// Size of decoded pixel buffer (400x480 * 4 bytes for RGBA)
const DECODE_BUF_SIZE: usize = 400 * 480 * 4;

/// TLS seed for random number generation
const TLS_SEED: u64 = 0x1234567890abcdef;

/// Display manager error types
#[derive(Debug)]
pub enum DisplayError {
    Http(HttpError),
    Png(&'static str),
    Json(&'static str),
    NoItems,
}

impl From<HttpError> for DisplayError {
    fn from(e: HttpError) -> Self {
        DisplayError::Http(e)
    }
}

/// Refresh the display from the edge service
///
/// Fetches widget data and images, decodes them, and updates the e-paper display.
/// Returns the total number of items available for rotation tracking.
pub async fn refresh_from_edge<'a, SPI, BUSY, DC, RST, DELAY>(
    stack: Stack<'a>,
    epd: &mut Epd7in3e<SPI, BUSY, DC, RST>,
    delay: &mut DELAY,
    framebuffer: &mut Framebuffer,
    edge_url: &str,
    widget_name: &str,
    start_index: usize,
) -> Result<usize, DisplayError>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    DC: OutputPin,
    RST: OutputPin,
    DELAY: DelayNs,
{

    // Clear framebuffer to white
    framebuffer.clear(Color::White);

    // Parse base URL
    let base_url = Url::parse(edge_url)?;

    // Build widget data URL
    let mut data_path: String<128> = String::new();
    write!(&mut data_path, "/api/widget/{}", widget_name).map_err(|_| HttpError::TooLarge)?;

    let data_url = Url {
        scheme: base_url.scheme,
        host: base_url.host,
        port: base_url.port,
        path: &data_path,
    };

    // Fetch widget data
    println!("Fetching widget data from {}:{}{}", data_url.host, data_url.port, data_url.path);

    let mut json_buf = [0u8; 8192];
    let mut json_len = 0;

    fetch_url(stack, &data_url, |chunk| {
        let remaining = json_buf.len() - json_len;
        let to_copy = chunk.len().min(remaining);
        json_buf[json_len..json_len + to_copy].copy_from_slice(&chunk[..to_copy]);
        json_len += to_copy;
    })
    .await?;

    // Parse JSON
    let json_str = core::str::from_utf8(&json_buf[..json_len]).map_err(|_| DisplayError::Json("invalid utf8"))?;
    println!("Received {} bytes of JSON", json_len);

    let items = parse_widget_data(json_str).map_err(DisplayError::Json)?;

    if items.is_empty() {
        return Err(DisplayError::NoItems);
    }

    let total_items = items.len();
    println!("Got {} widget items, showing from index {}", total_items, start_index);

    // Allocate buffers from PSRAM heap (reused for each image)
    let mut png_buf: Box<[u8; PNG_BUF_SIZE]> = Box::new([0u8; PNG_BUF_SIZE]);
    let mut decode_buf: Box<[u8; DECODE_BUF_SIZE]> = Box::new([0u8; DECODE_BUF_SIZE]);

    // Display 2 items starting from start_index (wrapping around)
    let items_to_display = total_items.min(2);

    for display_slot in 0..items_to_display {
        let item_idx = (start_index + display_slot) % total_items;
        let item = &items[item_idx];
        let x_offset = if display_slot == 0 { 0 } else { 400 };

        println!("Fetching image {}: {}", item_idx, item.path.as_str());

        if let Err(e) = fetch_and_decode_image(
            stack,
            &base_url,
            widget_name,
            item,
            framebuffer,
            x_offset,
            &mut png_buf,
            &mut decode_buf,
        )
        .await
        {
            println!("Error fetching image {}: {:?}", item_idx, e);
            // Fill this half with white on error
            if x_offset == 0 {
                framebuffer.fill_left_half(Color::White);
            } else {
                framebuffer.fill_right_half(Color::White);
            }
        }
    }

    // If only one item, fill right half with white
    if items_to_display == 1 {
        framebuffer.fill_right_half(Color::White);
    }

    // Send framebuffer to display
    println!("Updating display...");
    epd.display(framebuffer.as_slice(), delay)
        .map_err(|_| DisplayError::Http(HttpError::Write))?;

    println!("Display updated!");

    Ok(total_items)
}

/// Fetch and decode a single PNG image into the framebuffer
async fn fetch_and_decode_image<'a>(
    stack: Stack<'a>,
    base_url: &Url<'_>,
    widget_name: &str,
    item: &WidgetItem,
    framebuffer: &mut Framebuffer,
    x_offset: u32,
    png_buf: &mut [u8; PNG_BUF_SIZE],
    decode_buf: &mut [u8; DECODE_BUF_SIZE],
) -> Result<(), DisplayError> {
    // Build image URL
    let mut image_path: String<192> = String::new();
    write!(
        &mut image_path,
        "/api/widget/{}/{}",
        widget_name,
        item.path.as_str()
    )
    .map_err(|_| HttpError::TooLarge)?;

    let image_url = Url {
        scheme: base_url.scheme,
        host: base_url.host,
        port: base_url.port,
        path: &image_path,
    };

    // Fetch PNG data
    let mut png_len = 0;

    fetch_url(stack, &image_url, |chunk| {
        let remaining = png_buf.len() - png_len;
        let to_copy = chunk.len().min(remaining);
        png_buf[png_len..png_len + to_copy].copy_from_slice(&chunk[..to_copy]);
        png_len += to_copy;
    })
    .await?;

    println!("Received {} bytes of PNG data", png_len);

    // Decode PNG
    decode_png_to_framebuffer(&png_buf[..png_len], framebuffer, x_offset, decode_buf)?;

    Ok(())
}

/// Decode a PNG image into the framebuffer at the given x offset
fn decode_png_to_framebuffer(
    png_data: &[u8],
    framebuffer: &mut Framebuffer,
    x_offset: u32,
    decode_buf: &mut [u8],
) -> Result<(), DisplayError> {
    // Decode PNG header first
    let header = minipng::decode_png_header(png_data)
        .map_err(|_| DisplayError::Png("invalid PNG header"))?;

    println!(
        "PNG: {}x{} {:?}",
        header.width(),
        header.height(),
        header.color_type()
    );

    // Decode full image
    let image = minipng::decode_png(png_data, decode_buf)
        .map_err(|e| {
            println!("minipng error: {:?}", e);
            DisplayError::Png("PNG decode failed")
        })?;

    let width = image.width() as usize;
    let height = image.height() as usize;
    let pixels = image.pixels();

    // Temporary buffer for reversed row
    let mut row_buf = [0u8; 400];

    // Write rows to framebuffer (flipped vertically and horizontally)
    for y in 0..height {
        let row_start = y * width;
        let row_end = row_start + width;
        if row_end <= pixels.len() {
            // Reverse the row horizontally
            let row = &pixels[row_start..row_end];
            for (i, &px) in row.iter().enumerate() {
                if i < row_buf.len() {
                    row_buf[width - 1 - i] = px;
                }
            }

            let flipped_y = (height - 1 - y) as u32;
            framebuffer.write_row(x_offset, flipped_y, &row_buf[..width]);
        }
    }

    println!("PNG decode complete, {} rows processed", height);

    Ok(())
}

/// Resolve a hostname to an IPv4 address
/// Tries parsing as IPv4 first, falls back to DNS lookup
async fn resolve_host<'a>(stack: Stack<'a>, host: &str) -> Result<core::net::Ipv4Addr, HttpError> {
    // Try parsing as IPv4 first
    if let Ok(ip) = http::parse_ipv4(host) {
        return Ok(ip);
    }

    // DNS lookup
    println!("Resolving hostname: {}", host);
    let addrs = stack.dns_query(host, DnsQueryType::A).await
        .map_err(|_| HttpError::InvalidUrl)?;

    // Convert smoltcp IpAddress to core::net::Ipv4Addr
    if let Some(embassy_net::IpAddress::Ipv4(v4)) = addrs.first() {
        let octets = v4.octets();
        return Ok(core::net::Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]));
    }

    Err(HttpError::InvalidUrl)
}

/// Fetch data from a URL, supporting both HTTP and HTTPS
async fn fetch_url<'a, F>(
    stack: Stack<'a>,
    url: &Url<'_>,
    on_chunk: F,
) -> Result<(), HttpError>
where
    F: FnMut(&[u8]),
{
    // Resolve hostname
    let ip = resolve_host(stack, url.host).await?;
    let endpoint = (ip, url.port);

    match url.scheme {
        Scheme::Http => {
            // Plain HTTP
            let mut rx_buf = [0u8; 4096];
            let mut tx_buf = [0u8; 1024];
            let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
            socket.set_timeout(Some(embassy_time::Duration::from_secs(30)));

            socket.connect(endpoint).await.map_err(|_| HttpError::Connect)?;

            let mut http_rx_buf = [0u8; 2048];
            http::get(&mut socket, url, &mut http_rx_buf, on_chunk).await?;

            socket.close();
        }
        Scheme::Https => {
            // HTTPS with TLS
            let mut rx_buf = [0u8; 4096];
            let mut tx_buf = [0u8; 4096];
            let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
            socket.set_timeout(Some(embassy_time::Duration::from_secs(60)));

            socket.connect(endpoint).await.map_err(|_| HttpError::Connect)?;

            let mut http_rx_buf = [0u8; 2048];
            let mut tls_read_buf = [0u8; https::TLS_READ_BUF_SIZE];
            let mut tls_write_buf = [0u8; https::TLS_WRITE_BUF_SIZE];

            https::get(
                socket,
                url,
                &mut http_rx_buf,
                &mut tls_read_buf,
                &mut tls_write_buf,
                TLS_SEED,
                on_chunk,
            ).await?;
        }
    }

    Ok(())
}
