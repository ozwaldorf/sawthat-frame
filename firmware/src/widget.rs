//! Widget data types matching the edge service API
//!
//! JSON format from edge service:
//! ```json
//! ["band-id/01-01-2024", "band-id/02-01-2024"]
//! ```

use heapless::{String, Vec};

/// Maximum number of widget items we support
pub const MAX_ITEMS: usize = 128;

/// Maximum path string length (UUID + date = ~47 chars)
pub const MAX_PATH_LEN: usize = 48;

/// Display orientation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Orientation {
    /// Horizontal: 400x480 (half) or 800x480 (full)
    #[default]
    Horizontal = 0,
    /// Vertical: 480x800
    Vertical = 1,
}

impl Orientation {
    /// Get the path segment for this orientation
    pub fn as_str(&self) -> &'static str {
        match self {
            Orientation::Horizontal => "horiz",
            Orientation::Vertical => "vert",
        }
    }

    /// Toggle between orientations
    pub fn toggle(&self) -> Self {
        match self {
            Orientation::Horizontal => Orientation::Vertical,
            Orientation::Vertical => Orientation::Horizontal,
        }
    }

    /// Convert from u8 (for RTC memory)
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Orientation::Vertical,
            _ => Orientation::Horizontal,
        }
    }
}

/// Widget data response (array of image paths)
pub type WidgetData = Vec<String<MAX_PATH_LEN>, MAX_ITEMS>;

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
        let json = r#"["band-id/01-01-2024", "band-id/02-01-2024"]"#;

        let result = parse_widget_data(json);
        assert!(result.is_ok());
        let items = result.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_str(), "band-id/01-01-2024");
        assert_eq!(items[1].as_str(), "band-id/02-01-2024");
    }
}
