//! 6-color palette for E Ink Spectra 6 display
//!
//! Uses RGB Euclidean distance for fast color matching.
//! Palette values tuned from epdoptimize project.

/// RGB color representation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Palette index for the 6-color E Ink display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PaletteIndex {
    Black = 0,
    White = 1,
    Red = 2,
    Yellow = 3,
    Blue = 4,
    Green = 5,
}

impl PaletteIndex {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Tuned Spectra 6 palette (from epdoptimize)
/// These values are optimized for how the display actually renders colors
pub const PALETTE: [Rgb; 6] = [
    Rgb::new(25, 30, 33),     // Black  #191E21
    Rgb::new(232, 232, 232),  // White  #e8e8e8
    Rgb::new(178, 19, 24),    // Red    #b21318
    Rgb::new(239, 222, 68),   // Yellow #efde44
    Rgb::new(33, 87, 186),    // Blue   #2157ba
    Rgb::new(18, 95, 32),     // Green  #125f20
];

/// PNG palette bytes (RGB triplets) - same tuned values
pub const PNG_PALETTE: [u8; 18] = [
    25, 30, 33,      // Black
    232, 232, 232,   // White
    178, 19, 24,     // Red
    239, 222, 68,    // Yellow
    33, 87, 186,     // Blue
    18, 95, 32,      // Green
];

/// Simple palette matcher using RGB Euclidean distance
pub struct OklabPalette;

impl OklabPalette {
    pub fn new() -> Self {
        Self
    }

    /// Find nearest palette color using RGB distance
    #[inline]
    pub fn nearest(&self, color: &Rgb) -> PaletteIndex {
        let mut best_index = 0;
        let mut best_dist = i32::MAX;

        for (i, p) in PALETTE.iter().enumerate() {
            let dr = color.r as i32 - p.r as i32;
            let dg = color.g as i32 - p.g as i32;
            let db = color.b as i32 - p.b as i32;
            let dist = dr * dr + dg * dg + db * db;

            if dist < best_dist {
                best_dist = dist;
                best_index = i;
            }
        }

        match best_index {
            0 => PaletteIndex::Black,
            1 => PaletteIndex::White,
            2 => PaletteIndex::Red,
            3 => PaletteIndex::Yellow,
            4 => PaletteIndex::Blue,
            _ => PaletteIndex::Green,
        }
    }
}

impl Default for OklabPalette {
    fn default() -> Self {
        Self::new()
    }
}
