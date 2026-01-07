//! HTTPS client using embedded-tls
//!
//! Wraps TCP socket with TLS for secure connections.

use core::fmt::Write as FmtWrite;
use embassy_net::tcp::TcpSocket;
use embedded_io_async::{Read, Write};
use embedded_tls::{Aes128GcmSha256, TlsConnection, TlsContext, TlsConfig, UnsecureProvider};
use esp_println::println;
use heapless::String;
use rand_core::{RngCore, CryptoRng};

use crate::http::{HttpError, Url, find_header_end, parse_status, parse_header, HttpResponse};

/// Simple RNG using a seed
pub struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    pub fn new(seed: u64) -> Self {
        Self { state: if seed == 0 { 0x853c49e6748fea9b } else { seed } }
    }
}

impl RngCore for SimpleRng {
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        let mut i = 0;
        while i < dest.len() {
            let val = self.next_u64().to_le_bytes();
            let remaining = dest.len() - i;
            let to_copy = remaining.min(8);
            dest[i..i + to_copy].copy_from_slice(&val[..to_copy]);
            i += to_copy;
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for SimpleRng {}

/// Check if HTTPS is supported
pub fn is_supported() -> bool {
    true
}

/// Return error for HTTPS requests (not used when supported)
pub fn not_supported_error() -> HttpError {
    HttpError::Tls
}

/// TLS buffer sizes
pub const TLS_READ_BUF_SIZE: usize = 16640;
pub const TLS_WRITE_BUF_SIZE: usize = 4096;

/// Perform an HTTPS GET request
pub async fn get<'a, F>(
    socket: TcpSocket<'a>,
    url: &Url<'_>,
    rx_buf: &mut [u8],
    tls_read_buf: &mut [u8],
    tls_write_buf: &mut [u8],
    seed: u64,
    mut on_body_chunk: F,
) -> Result<HttpResponse, HttpError>
where
    F: FnMut(&[u8]),
{
    println!("Starting TLS handshake with {}", url.host);

    let mut rng = SimpleRng::new(seed);

    // Create TLS config
    let config = TlsConfig::new().with_server_name(url.host);

    // Create TLS connection (takes ownership of socket)
    let mut tls: TlsConnection<'_, TcpSocket<'a>, Aes128GcmSha256> = TlsConnection::new(
        socket,
        tls_read_buf,
        tls_write_buf,
    );

    // Perform TLS handshake (skip certificate verification with UnsecureProvider)
    tls.open(TlsContext::new(&config, UnsecureProvider::new(&mut rng)))
        .await
        .map_err(|e| {
            println!("TLS handshake failed: {:?}", e);
            HttpError::Tls
        })?;

    println!("TLS handshake complete");

    // Build HTTP request
    let mut request: String<512> = String::new();
    write!(
        &mut request,
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: ESP32\r\n\r\n",
        url.path, url.host
    )
    .map_err(|_| HttpError::TooLarge)?;

    println!("Sending HTTP request ({} bytes)...", request.len());

    // Send request over TLS
    tls.write_all(request.as_bytes())
        .await
        .map_err(|_| HttpError::Write)?;

    // Flush to ensure data is sent
    tls.flush()
        .await
        .map_err(|_| HttpError::Write)?;

    println!("Request sent, waiting for response...");

    // Read response headers
    let mut total_read = 0;

    let headers_end = loop {
        if total_read >= rx_buf.len() {
            return Err(HttpError::TooLarge);
        }

        let n = tls
            .read(&mut rx_buf[total_read..])
            .await
            .map_err(|_| HttpError::Read)?;

        if n == 0 {
            return Err(HttpError::Read);
        }

        total_read += n;

        if let Some(pos) = find_header_end(&rx_buf[..total_read]) {
            break pos;
        }
    };

    let header_bytes = &rx_buf[..headers_end];
    let header_str = core::str::from_utf8(header_bytes).map_err(|_| HttpError::Parse)?;

    // Parse response
    let status = parse_status(header_str)?;
    let content_length = parse_header(header_str, "content-length").and_then(|v| v.parse().ok());
    let content_type = parse_header(header_str, "content-type").unwrap_or("");

    println!("HTTPS response: {} ({:?} bytes)", status, content_length);

    let body_start = headers_end + 4;
    let mut response = HttpResponse::new(status, content_length, content_type);

    if status >= 400 {
        return Err(HttpError::Status(status));
    }

    // Process initial body data
    if total_read > body_start {
        let initial_body = &rx_buf[body_start..total_read];
        on_body_chunk(initial_body);
        response.body_read += initial_body.len();
    }

    // Continue reading body
    loop {
        if let Some(remaining) = response.remaining() {
            if remaining == 0 {
                break;
            }
        }

        let n = tls.read(rx_buf).await.map_err(|_| HttpError::Read)?;
        if n == 0 {
            break;
        }

        on_body_chunk(&rx_buf[..n]);
        response.body_read += n;
    }

    // Close TLS connection
    let _ = tls.close().await;

    Ok(response)
}
