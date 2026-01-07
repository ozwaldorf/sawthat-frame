//! Display manager for orchestrating edge service integration
//!
//! Handles the fetch → decode → display flow using a single HTTP connection:
//! 1. Fetch widget data JSON from edge service
//! 2. Parse and shuffle widget items
//! 3. Fetch PNG images for each item (reusing connection)
//! 4. Decode and write to framebuffer
//! 5. Refresh the e-paper display

extern crate alloc;

use alloc::boxed::Box;
use core::fmt::Write as FmtWrite;
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal::spi::SpiDevice;
use embedded_io_async::Read;
use embedded_nal_async::{Dns, TcpConnect};
use esp_println::println;
use heapless::String;
use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::request::Method;

use crate::epd::{Color, Epd7in3e};
use crate::framebuffer::Framebuffer;
use crate::widget::{parse_widget_data, WidgetData};

/// Size of PNG receive buffer (128KB - enough for processed e-paper images)
const PNG_BUF_SIZE: usize = 128 * 1024;
/// Size of decoded pixel buffer (400x480 * 4 bytes for RGBA)
const DECODE_BUF_SIZE: usize = 400 * 480 * 4;

/// TLS buffer sizes
pub const TLS_READ_BUF_SIZE: usize = 16640;
pub const TLS_WRITE_BUF_SIZE: usize = 4096;

/// TLS seed for random number generation
const TLS_SEED: u64 = 0x1234567890abcdef;

/// Display manager error types
#[derive(Debug)]
pub enum DisplayError {
    Network,
    Http(u16),
    Png(&'static str),
    Json(&'static str),
    NoItems,
}

/// Fetch widget data and display items using a single persistent HTTP connection.
///
/// This function:
/// 1. Establishes one HTTP(S) connection to the edge server
/// 2. Fetches widget data JSON
/// 3. Fetches all PNG images (reusing the same connection)
/// 4. Decodes and renders to framebuffer
/// 5. Updates the e-paper display
pub async fn fetch_and_display<SPI, BUSY, DC, RST, DELAY, T, D>(
    tcp: &T,
    dns: &D,
    tls_read_buf: &mut [u8],
    tls_write_buf: &mut [u8],
    epd: &mut Epd7in3e<SPI, BUSY, DC, RST>,
    delay: &mut DELAY,
    framebuffer: &mut Framebuffer,
    edge_url: &str,
    widget_name: &str,
    items: &WidgetData,
    start_index: usize,
) -> Result<(), DisplayError>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    DC: OutputPin,
    RST: OutputPin,
    DELAY: DelayNs,
    T: TcpConnect,
    D: Dns,
{
    // Clear framebuffer to white
    framebuffer.clear(Color::White);

    let total_items = items.len();
    println!("Displaying items starting at index {} (connection reuse enabled)", start_index);

    // Create HTTP client with TLS - single connection for all requests
    let tls_config = TlsConfig::new(TLS_SEED, tls_read_buf, tls_write_buf, TlsVerify::None);
    let mut client = HttpClient::new_with_tls(tcp, dns, tls_config);

    // Establish persistent connection to edge server
    let mut resource = client
        .resource(edge_url)
        .await
        .map_err(|_| DisplayError::Network)?;

    // Allocate buffers from PSRAM heap (reused for each image)
    let mut png_buf: Box<[u8; PNG_BUF_SIZE]> = Box::new([0u8; PNG_BUF_SIZE]);
    let mut decode_buf: Box<[u8; DECODE_BUF_SIZE]> = Box::new([0u8; DECODE_BUF_SIZE]);
    let mut rx_buf = [0u8; 2048];

    // Display 2 items starting from start_index (wrapping around)
    let items_to_display = total_items.min(2);

    for display_slot in 0..items_to_display {
        let item_idx = (start_index + display_slot) % total_items;
        let item = &items[item_idx];
        let x_offset = if display_slot == 0 { 0 } else { 400 };

        println!("Fetching image {}: {}", item_idx, item.path.as_str());

        // Build relative path for image
        let mut path: String<256> = String::new();
        if write!(&mut path, "/api/widget/{}/{}", widget_name, item.path.as_str()).is_err() {
            println!("Path too long, skipping image");
            fill_half(framebuffer, x_offset);
            continue;
        }

        // Fetch PNG using existing connection
        let result = async {
            let response = resource
                .request(Method::GET, path.as_str())
                .send(&mut rx_buf)
                .await
                .map_err(|_| DisplayError::Network)?;

            let status = response.status.0;
            if status >= 400 {
                return Err(DisplayError::Http(status));
            }

            // Read PNG body
            let mut png_len = 0;
            let mut body_reader = response.body().reader();
            loop {
                match body_reader.read(&mut png_buf[png_len..]).await {
                    Ok(0) => break,
                    Ok(n) => png_len += n,
                    Err(_) => break,
                }
            }

            Ok(png_len)
        }.await;

        match result {
            Ok(png_len) => {
                println!("Received {} bytes of PNG data", png_len);
                if let Err(e) = decode_png_to_framebuffer(&png_buf[..png_len], framebuffer, x_offset, &mut *decode_buf) {
                    println!("Error decoding PNG: {:?}", e);
                    fill_half(framebuffer, x_offset);
                }
            }
            Err(e) => {
                println!("Error fetching image {}: {:?}", item_idx, e);
                fill_half(framebuffer, x_offset);
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
        .map_err(|_| DisplayError::Network)?;

    println!("Display updated!");

    Ok(())
}

/// Fetch widget data from edge service
pub async fn fetch_widget_data<T, D>(
    tcp: &T,
    dns: &D,
    tls_read_buf: &mut [u8],
    tls_write_buf: &mut [u8],
    edge_url: &str,
    widget_name: &str,
) -> Result<WidgetData, DisplayError>
where
    T: TcpConnect,
    D: Dns,
{
    // Create HTTP client with TLS
    let tls_config = TlsConfig::new(TLS_SEED, tls_read_buf, tls_write_buf, TlsVerify::None);
    let mut client = HttpClient::new_with_tls(tcp, dns, tls_config);

    // Build path
    let mut path: String<256> = String::new();
    write!(&mut path, "/api/widget/{}", widget_name).map_err(|_| DisplayError::Network)?;

    println!("Fetching widget data from {}{}", edge_url, path.as_str());

    // Establish connection and make request
    let mut resource = client
        .resource(edge_url)
        .await
        .map_err(|_| DisplayError::Network)?;

    let mut rx_buf = [0u8; 4096];
    let response = resource
        .request(Method::GET, path.as_str())
        .send(&mut rx_buf)
        .await
        .map_err(|_| DisplayError::Network)?;

    let status = response.status.0;
    if status >= 400 {
        return Err(DisplayError::Http(status));
    }

    // Read response body
    let mut json_buf = [0u8; 8192];
    let mut json_len = 0;

    let mut body_reader = response.body().reader();
    loop {
        match body_reader.read(&mut json_buf[json_len..]).await {
            Ok(0) => break,
            Ok(n) => json_len += n,
            Err(_) => break,
        }
    }

    let json_str =
        core::str::from_utf8(&json_buf[..json_len]).map_err(|_| DisplayError::Json("invalid utf8"))?;
    println!("Received {} bytes of JSON", json_len);

    let items = parse_widget_data(json_str).map_err(DisplayError::Json)?;

    if items.is_empty() {
        return Err(DisplayError::NoItems);
    }

    println!("Got {} widget items", items.len());
    Ok(items)
}

/// Shuffle widget items in-place using a simple xorshift RNG
pub fn shuffle_items(items: &mut WidgetData, seed: u64) {
    let len = items.len();
    if len <= 1 {
        return;
    }

    let mut state = if seed == 0 {
        0x853c49e6748fea9b
    } else {
        seed
    };

    // Fisher-Yates shuffle
    for i in (1..len).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;

        let j = (state as usize) % (i + 1);
        items.swap(i, j);
    }

    println!("Shuffled {} items", len);
}

fn fill_half(framebuffer: &mut Framebuffer, x_offset: u32) {
    if x_offset == 0 {
        framebuffer.fill_left_half(Color::White);
    } else {
        framebuffer.fill_right_half(Color::White);
    }
}

/// Decode a PNG image into the framebuffer at the given x offset
fn decode_png_to_framebuffer(
    png_data: &[u8],
    framebuffer: &mut Framebuffer,
    x_offset: u32,
    decode_buf: &mut [u8],
) -> Result<(), DisplayError> {
    let header =
        minipng::decode_png_header(png_data).map_err(|_| DisplayError::Png("invalid PNG header"))?;

    println!(
        "PNG: {}x{} {:?}",
        header.width(),
        header.height(),
        header.color_type()
    );

    let image = minipng::decode_png(png_data, decode_buf).map_err(|e| {
        println!("minipng error: {:?}", e);
        DisplayError::Png("PNG decode failed")
    })?;

    let width = image.width() as usize;
    let height = image.height() as usize;
    let pixels = image.pixels();

    let mut row_buf = [0u8; 400];

    for y in 0..height {
        let row_start = y * width;
        let row_end = row_start + width;
        if row_end <= pixels.len() {
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

/// TLS buffer size constants for external allocation
pub const fn tls_read_buffer_size() -> usize {
    TLS_READ_BUF_SIZE
}

pub const fn tls_write_buffer_size() -> usize {
    TLS_WRITE_BUF_SIZE
}
