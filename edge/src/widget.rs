//! Widget types and handlers
//!
//! Widgets are data sources that provide items to display on the e-paper frame.

use fastly::http::StatusCode;
use fastly::{Error, Response};
use serde::{Deserialize, Serialize};

use crate::datasource::DataSourceRegistry;

/// Display orientation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// Horizontal: 400x480 (half) or 800x480 (full)
    Horizontal,
    /// Vertical: 480x800
    Vertical,
}

impl Orientation {
    /// Parse from path segment
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "horiz" => Some(Orientation::Horizontal),
            "vert" => Some(Orientation::Vertical),
            _ => None,
        }
    }

    /// Get dimensions for this orientation and width
    pub fn dimensions(&self, width: WidgetWidth) -> (u32, u32) {
        match (self, width) {
            (Orientation::Horizontal, WidgetWidth::Half) => (400, 480),
            (Orientation::Horizontal, WidgetWidth::Full) => (800, 480),
            (Orientation::Vertical, WidgetWidth::Half) => (480, 800),
            (Orientation::Vertical, WidgetWidth::Full) => (480, 800), // vertical is always 480x800
        }
    }
}

/// Widget item width
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(into = "u8", try_from = "u8")]
pub enum WidgetWidth {
    /// Half width: 400x480 pixels
    Half = 1,
    /// Full width: 800x480 pixels
    Full = 2,
}

impl WidgetWidth {
    pub fn pixels(&self) -> u32 {
        match self {
            WidgetWidth::Half => 400,
            WidgetWidth::Full => 800,
        }
    }
}

impl From<WidgetWidth> for u8 {
    fn from(w: WidgetWidth) -> u8 {
        w as u8
    }
}

impl TryFrom<u8> for WidgetWidth {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(WidgetWidth::Half),
            2 => Ok(WidgetWidth::Full),
            _ => Err("Invalid width: must be 1 or 2"),
        }
    }
}

/// Cache policy for widget items
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CachePolicy {
    /// Cache indefinitely
    #[serde(rename = "max")]
    Max,
    /// TTL in seconds
    Ttl(u32),
}

impl std::fmt::Display for CachePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CachePolicy::Max => write!(f, "max"),
            CachePolicy::Ttl(secs) => write!(f, "{}", secs),
        }
    }
}

/// A single item in a widget response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetItem {
    /// Display width (1 = 400px half, 2 = 800px full)
    pub width: WidgetWidth,
    /// Cache policy for this item's image (not sent to firmware)
    #[serde(skip_serializing)]
    pub cache_policy: CachePolicy,
    /// Cache key for deduplication (u32)
    pub cache_key: u32,
    /// Path to fetch the image (relative to widget)
    pub path: String,
}

/// Widget data response (array of items)
pub type WidgetData = Vec<WidgetItem>;

/// Handle a request for widget data
pub fn handle_widget_data(widget_name: &str) -> Result<Response, Error> {
    let registry = DataSourceRegistry::new();

    let source = match registry.get(widget_name) {
        Some(s) => s,
        None => {
            return Ok(Response::from_status(StatusCode::NOT_FOUND)
                .with_body_text_plain("Unknown widget"));
        }
    };

    let items = source.fetch_data()?;
    let cache_policy = source.data_cache_policy();
    let body = serde_json::to_string(&items)?;

    Ok(Response::from_status(StatusCode::OK)
        .with_header("Content-Type", "application/json")
        .with_header("X-Cache-Policy", cache_policy.to_string())
        .with_body(body))
}

/// Handle a request for a widget image
pub fn handle_widget_image(widget_name: &str, orientation_str: &str, image_path: &str) -> Result<Response, Error> {
    log::info!("Image request: widget={}, orientation={}, path={}", widget_name, orientation_str, image_path);

    let orientation = match Orientation::from_str(orientation_str) {
        Some(o) => o,
        None => {
            return Ok(Response::from_status(StatusCode::BAD_REQUEST)
                .with_body_text_plain("Invalid orientation: use 'horiz' or 'vert'"));
        }
    };

    let registry = DataSourceRegistry::new();

    let source = match registry.get(widget_name) {
        Some(s) => s,
        None => {
            return Ok(Response::from_status(StatusCode::NOT_FOUND)
                .with_body_text_plain("Unknown widget"));
        }
    };

    let png_data = source.fetch_image(image_path, orientation)?;

    Ok(Response::from_status(StatusCode::OK)
        .with_header("Content-Type", "image/png")
        .with_header("Cache-Control", "public, max-age=31536000, immutable")
        .with_body(png_data))
}

