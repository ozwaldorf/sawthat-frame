//! Widget data types matching the edge service API
//!
//! JSON format from edge service:
//! ```json
//! ["2024-01-01-band-id", "2024-01-02-band-id"]
//! ```

extern crate alloc;

use alloc::boxed::Box;
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

/// Parse widget data JSON into a heap-allocated vector of items
pub fn parse_widget_data(json: &str) -> Result<Box<WidgetData>, &'static str> {
    // Allocate on heap first to avoid stack overflow
    let mut data: Box<WidgetData> = Box::new(Vec::new());

    // Parse JSON array manually to avoid large stack allocation
    let json = json.trim();
    if !json.starts_with('[') || !json.ends_with(']') {
        return Err("expected JSON array");
    }

    let inner = &json[1..json.len() - 1];
    if inner.trim().is_empty() {
        return Ok(data);
    }

    // Split by comma, handling quoted strings
    let mut in_string = false;
    let mut start = 0;
    let bytes = inner.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_string = !in_string,
            b',' if !in_string => {
                if let Some(s) = parse_string_value(&inner[start..i]) {
                    let mut item = String::new();
                    if item.push_str(s).is_ok() {
                        let _ = data.push(item);
                    }
                }
                start = i + 1;
            }
            _ => {}
        }
    }

    // Last item
    if start < inner.len()
        && let Some(s) = parse_string_value(&inner[start..])
    {
        let mut item = String::new();
        if item.push_str(s).is_ok() {
            let _ = data.push(item);
        }
    }

    Ok(data)
}

/// Parse a JSON string value, returning the unquoted content
fn parse_string_value(s: &str) -> Option<&str> {
    let s = s.trim();
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_widget_data() {
        let json = r#"["2024-01-01-band-id", "2024-01-02-band-id"]"#;

        let result = parse_widget_data(json);
        assert!(result.is_ok());
        let items = result.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_str(), "2024-01-01-band-id");
        assert_eq!(items[1].as_str(), "2024-01-02-band-id");
    }

    #[test]
    fn test_parse_empty_array() {
        let json = r#"[]"#;
        let result = parse_widget_data(json);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }
}
