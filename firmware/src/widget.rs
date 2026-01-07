//! Widget data types matching the edge service API
//!
//! JSON format from edge service:
//! ```json
//! [
//!   {
//!     "width": 1,
//!     "cache_policy": "max",
//!     "cache_key": 12345678,
//!     "path": "/concerts/abc123"
//!   }
//! ]
//! ```

use heapless::{String, Vec};
use serde::Deserialize;

/// Maximum number of widget items we support
pub const MAX_ITEMS: usize = 64;

/// Maximum path string length
pub const MAX_PATH_LEN: usize = 64;

/// Widget item width
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WidgetWidth {
    /// Half width: 400x480 pixels
    #[default]
    Half,
    /// Full width: 800x480 pixels
    Full,
}

impl WidgetWidth {
    /// Get width in pixels
    pub fn pixels(&self) -> u32 {
        match self {
            WidgetWidth::Half => 400,
            WidgetWidth::Full => 800,
        }
    }

    /// Get width in framebuffer bytes (4bpp, 2 pixels per byte)
    pub fn bytes(&self) -> usize {
        (self.pixels() / 2) as usize
    }
}

impl<'de> Deserialize<'de> for WidgetWidth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        match value {
            1 => Ok(WidgetWidth::Half),
            2 => Ok(WidgetWidth::Full),
            _ => Err(serde::de::Error::custom("invalid width: must be 1 or 2")),
        }
    }
}

/// A single widget item from the edge service
#[derive(Debug, Clone, Deserialize)]
pub struct WidgetItem {
    /// Display width (1 = 400px half, 2 = 800px full)
    pub width: WidgetWidth,
    /// Cache key for deduplication
    pub cache_key: u32,
    /// Path to fetch the image (relative to widget)
    pub path: String<MAX_PATH_LEN>,
}

/// Widget data response (array of items)
pub type WidgetData = Vec<WidgetItem, MAX_ITEMS>;

/// Parse widget data JSON into a vector of items
pub fn parse_widget_data(json: &str) -> Result<WidgetData, &'static str> {
    serde_json_core::from_str(json)
        .map(|(data, _)| data)
        .map_err(|_| "JSON parse error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_widget_data() {
        let json = r#"[
            {"width": 1, "cache_policy": "max", "cache_key": 1001, "path": "concert/test"},
            {"width": 2, "cache_policy": "max", "cache_key": 1002, "path": "concert/test2"}
        ]"#;

        let result = parse_widget_data(json);
        assert!(result.is_ok());
        let items = result.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].width, WidgetWidth::Half);
        assert_eq!(items[1].width, WidgetWidth::Full);
    }
}
