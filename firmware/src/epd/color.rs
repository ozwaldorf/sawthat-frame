//! Color definitions for Spectra 6 (6-color) e-paper display

/// 6-color palette for Spectra 6 e-paper
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Color {
    /// Black (0x00)
    Black = 0x00,
    /// White (0x01)
    #[default]
    White = 0x01,
    /// Yellow (0x02)
    Yellow = 0x02,
    /// Red (0x03)
    Red = 0x03,
    /// Blue (0x05)
    Blue = 0x05,
    /// Green (0x06)
    Green = 0x06,
    /// Clean/Clear (0x07) - used for clearing artifacts
    Clean = 0x07,
}

impl Color {
    /// Get the 4-bit color value
    #[inline]
    pub const fn to_4bit(self) -> u8 {
        self as u8
    }

    /// Get a byte with this color in both pixel positions (for fills)
    #[inline]
    pub const fn to_dual_pixel(self) -> u8 {
        let c = self as u8;
        (c << 4) | c
    }

    /// Pack two colors into a single byte
    #[inline]
    pub const fn pack(high: Color, low: Color) -> u8 {
        ((high as u8) << 4) | (low as u8)
    }

    /// Create from 4-bit value (clamped to valid colors)
    pub const fn from_4bit(value: u8) -> Self {
        match value & 0x0F {
            0x00 => Color::Black,
            0x01 => Color::White,
            0x02 => Color::Yellow,
            0x03 => Color::Red,
            0x05 => Color::Blue,
            0x06 => Color::Green,
            0x07 => Color::Clean,
            // Map invalid values to nearest
            0x04 => Color::Red, // between red and blue
            _ => Color::Green,  // 0x08+ map to green
        }
    }

    /// Convert from RGB332 (rough approximation for dithering input)
    pub const fn from_rgb332(rgb: u8) -> Self {
        let r = (rgb >> 5) & 0x07; // 3 bits red
        let g = (rgb >> 2) & 0x07; // 3 bits green
        let b = rgb & 0x03; // 2 bits blue

        // Simple nearest-color matching
        if r < 2 && g < 2 && b < 1 {
            Color::Black
        } else if r > 5 && g > 5 && b > 2 {
            Color::White
        } else if r > 5 && g > 4 && b < 2 {
            Color::Yellow
        } else if r > 4 && g < 3 && b < 2 {
            Color::Red
        } else if r < 3 && g < 3 && b > 1 {
            Color::Blue
        } else if r < 3 && g > 4 && b < 2 {
            Color::Green
        } else if r > g && r > (b * 2) {
            if g > 3 {
                Color::Yellow
            } else {
                Color::Red
            }
        } else if g > r && g > (b * 2) {
            Color::Green
        } else if (b * 2) > r && (b * 2) > g {
            Color::Blue
        } else {
            Color::White
        }
    }
}

// embedded-graphics integration
use embedded_graphics_core::pixelcolor::{PixelColor, raw::RawU4};
use embedded_graphics_core::prelude::RawData;

impl PixelColor for Color {
    type Raw = RawU4;
}

impl From<RawU4> for Color {
    fn from(raw: RawU4) -> Self {
        Color::from_4bit(raw.into_inner())
    }
}

impl From<Color> for RawU4 {
    fn from(color: Color) -> Self {
        RawU4::new(color.to_4bit())
    }
}
