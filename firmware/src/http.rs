//! Simple HTTP/1.1 client for no_std environments
//!
//! Provides basic GET requests with streaming response body support.
//! Supports both HTTP and HTTPS connections.

use core::fmt::Write as FmtWrite;
use core::str;
use embassy_net::tcp::TcpSocket;
use embedded_io_async::Write;
use heapless::String;

/// HTTP client error types
#[derive(Debug)]
pub enum HttpError {
    /// Failed to connect to server
    Connect,
    /// Failed to write request
    Write,
    /// Failed to read response
    Read,
    /// Invalid URL format
    InvalidUrl,
    /// Response parsing error
    Parse,
    /// HTTP error status code
    Status(u16),
    /// Response too large
    TooLarge,
    /// TLS error
    Tls,
}

/// URL scheme
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scheme {
    Http,
    Https,
}

/// Parsed URL components
pub struct Url<'a> {
    pub scheme: Scheme,
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
}

impl<'a> Url<'a> {
    /// Parse a URL string into components
    /// Supports: http://host:port/path, https://host:port/path
    pub fn parse(url: &'a str) -> Result<Self, HttpError> {
        // Determine scheme and strip prefix
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else {
            return Err(HttpError::InvalidUrl);
        };

        let default_port = match scheme {
            Scheme::Http => 80,
            Scheme::Https => 443,
        };

        // Find path separator
        let (host_port, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };

        // Parse host and optional port
        let (host, port) = match host_port.find(':') {
            Some(idx) => {
                let port_str = &host_port[idx + 1..];
                let port = port_str.parse().map_err(|_| HttpError::InvalidUrl)?;
                (&host_port[..idx], port)
            }
            None => (host_port, default_port),
        };

        Ok(Url { scheme, host, port, path })
    }
}

/// HTTP response with streaming body
pub struct HttpResponse {
    pub status: u16,
    pub content_length: Option<usize>,
    pub content_type: heapless::String<64>,
    pub body_read: usize,
}

impl HttpResponse {
    /// Create a new response from parsed headers
    pub fn new(status: u16, content_length: Option<usize>, content_type: &str) -> Self {
        let mut ct = heapless::String::new();
        let _ = ct.push_str(content_type);
        Self {
            status,
            content_length,
            content_type: ct,
            body_read: 0,
        }
    }

    /// Get remaining body bytes to read
    pub fn remaining(&self) -> Option<usize> {
        self.content_length.map(|len| len.saturating_sub(self.body_read))
    }
}

/// Perform an HTTP GET request and stream the response
///
/// The `on_body_chunk` callback is called with each chunk of body data.
/// Returns the HTTP response headers.
pub async fn get<'a, F>(
    socket: &mut TcpSocket<'a>,
    url: &Url<'_>,
    rx_buf: &mut [u8],
    mut on_body_chunk: F,
) -> Result<HttpResponse, HttpError>
where
    F: FnMut(&[u8]),
{
    // Build request
    let mut request: String<256> = String::new();
    write!(
        &mut request,
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        url.path, url.host
    )
    .map_err(|_| HttpError::TooLarge)?;

    // Send request
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(|_| HttpError::Write)?;

    // Read response headers
    let mut total_read = 0;

    // Read until we find \r\n\r\n
    let headers_end = loop {
        if total_read >= rx_buf.len() {
            return Err(HttpError::TooLarge);
        }

        let n = socket
            .read(&mut rx_buf[total_read..])
            .await
            .map_err(|_| HttpError::Read)?;

        if n == 0 {
            return Err(HttpError::Read);
        }

        total_read += n;

        // Look for end of headers
        if let Some(pos) = find_header_end(&rx_buf[..total_read]) {
            break pos;
        }
    };
    let header_bytes = &rx_buf[..headers_end];
    let header_str = str::from_utf8(header_bytes).map_err(|_| HttpError::Parse)?;

    // Parse status line
    let status = parse_status(header_str)?;

    // Parse headers
    let content_length = parse_header(header_str, "content-length")
        .and_then(|v| v.parse().ok());
    let content_type = parse_header(header_str, "content-type").unwrap_or("");

    let body_start = headers_end + 4; // Skip \r\n\r\n
    let mut response = HttpResponse::new(status, content_length, content_type);

    // Check status
    if status >= 400 {
        return Err(HttpError::Status(status));
    }

    // Process any body data already read
    if total_read > body_start {
        let initial_body = &rx_buf[body_start..total_read];
        on_body_chunk(initial_body);
        response.body_read += initial_body.len();
    }

    // Continue reading body
    loop {
        // Check if we've read everything
        if let Some(remaining) = response.remaining() {
            if remaining == 0 {
                break;
            }
        }

        let n = socket.read(rx_buf).await.map_err(|_| HttpError::Read)?;
        if n == 0 {
            break;
        }

        on_body_chunk(&rx_buf[..n]);
        response.body_read += n;
    }

    Ok(response)
}

/// Find the position of \r\n\r\n in the buffer
pub fn find_header_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

/// Parse HTTP status code from status line
pub fn parse_status(headers: &str) -> Result<u16, HttpError> {
    // HTTP/1.1 200 OK
    let line = headers.lines().next().ok_or(HttpError::Parse)?;
    let parts: heapless::Vec<&str, 3> = line.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return Err(HttpError::Parse);
    }
    parts[1].parse().map_err(|_| HttpError::Parse)
}

/// Parse a header value (case-insensitive)
pub fn parse_header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    for line in headers.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case(name) {
                return Some(value.trim());
            }
        }
    }
    None
}

/// Resolve hostname to IP address
/// For now, requires numeric IP addresses. DNS resolution would need embassy-net-dns.
pub fn parse_ipv4(host: &str) -> Result<core::net::Ipv4Addr, HttpError> {
    let parts: heapless::Vec<&str, 4> = host.split('.').collect();
    if parts.len() != 4 {
        return Err(HttpError::InvalidUrl);
    }

    let a: u8 = parts[0].parse().map_err(|_| HttpError::InvalidUrl)?;
    let b: u8 = parts[1].parse().map_err(|_| HttpError::InvalidUrl)?;
    let c: u8 = parts[2].parse().map_err(|_| HttpError::InvalidUrl)?;
    let d: u8 = parts[3].parse().map_err(|_| HttpError::InvalidUrl)?;

    Ok(core::net::Ipv4Addr::new(a, b, c, d))
}
