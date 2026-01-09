//! Widget types and handlers
//!
//! Widgets are data sources that provide items to display on the e-paper frame.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Available widgets
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum WidgetName {
    /// Concert history from SawThat.band
    Concerts,
}

/// Display orientation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    /// Horizontal: 400x480 (half) or 800x480 (full)
    Horiz,
    /// Vertical: 480x800
    Vert,
}

impl Orientation {
    /// Get dimensions for this orientation and width
    pub fn dimensions(&self, width: WidgetWidth) -> (u32, u32) {
        match (self, width) {
            (Orientation::Horiz, WidgetWidth::Half) => (400, 480),
            (Orientation::Horiz, WidgetWidth::Full) => (800, 480),
            (Orientation::Vert, WidgetWidth::Half) => (480, 800),
            (Orientation::Vert, WidgetWidth::Full) => (480, 800), // vertical is always 480x800
        }
    }
}

impl std::fmt::Display for Orientation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Orientation::Horiz => write!(f, "horiz"),
            Orientation::Vert => write!(f, "vert"),
        }
    }
}

/// Widget item width
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

/// Widget data response (array of image paths)
pub type WidgetData = Vec<String>;
