//! Screen orientation.

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
