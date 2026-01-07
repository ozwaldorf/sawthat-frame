//! 6-color palette for E Ink Spectra 6 display
//!
//! Uses OKLab color space for perceptually uniform color matching.
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

    /// Convert to OKLab color space
    pub fn to_oklab(&self) -> Oklab {
        Oklab::from_rgb(self.r, self.g, self.b)
    }
}

/// OKLab color representation for perceptually uniform operations
#[derive(Debug, Clone, Copy)]
pub struct Oklab {
    pub l: f32,
    pub a: f32,
    pub b: f32,
}

impl Oklab {
    pub fn new(l: f32, a: f32, b: f32) -> Self {
        Self { l, a, b }
    }

    /// Convert sRGB byte to linear
    #[inline]
    fn srgb_to_linear(c: u8) -> f32 {
        let c = c as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// Convert linear to sRGB byte
    #[inline]
    fn linear_to_srgb(c: f32) -> u8 {
        let c = if c <= 0.0031308 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        (c * 255.0).clamp(0.0, 255.0) as u8
    }

    /// Convert from RGB to OKLab
    pub fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        let r = Self::srgb_to_linear(r);
        let g = Self::srgb_to_linear(g);
        let b = Self::srgb_to_linear(b);

        let l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
        let m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
        let s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;

        let l_ = l.cbrt();
        let m_ = m.cbrt();
        let s_ = s.cbrt();

        Self {
            l: 0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_,
            a: 1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_,
            b: 0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_,
        }
    }

    /// Convert from OKLab to RGB
    pub fn to_rgb(&self) -> Rgb {
        let l_ = self.l + 0.3963377774 * self.a + 0.2158037573 * self.b;
        let m_ = self.l - 0.1055613458 * self.a - 0.0638541728 * self.b;
        let s_ = self.l - 0.0894841775 * self.a - 1.2914855480 * self.b;

        let l = l_ * l_ * l_;
        let m = m_ * m_ * m_;
        let s = s_ * s_ * s_;

        let r = 4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s;
        let g = -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s;
        let b = -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s;

        Rgb::new(
            Self::linear_to_srgb(r),
            Self::linear_to_srgb(g),
            Self::linear_to_srgb(b),
        )
    }

    /// Squared distance to another OKLab color
    #[inline]
    pub fn distance_squared(&self, other: &Oklab) -> f32 {
        let dl = self.l - other.l;
        let da = self.a - other.a;
        let db = self.b - other.b;
        dl * dl + da * da + db * db
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

/// Palette matcher using OKLab perceptual distance
pub struct OklabPalette {
    /// Precomputed OKLab values for each palette color
    palette_oklab: [Oklab; 6],
}

impl OklabPalette {
    pub fn new() -> Self {
        Self {
            palette_oklab: [
                PALETTE[0].to_oklab(),
                PALETTE[1].to_oklab(),
                PALETTE[2].to_oklab(),
                PALETTE[3].to_oklab(),
                PALETTE[4].to_oklab(),
                PALETTE[5].to_oklab(),
            ],
        }
    }

    /// Find nearest palette color using OKLab perceptual distance
    #[inline]
    pub fn nearest(&self, color: &Oklab) -> PaletteIndex {
        let mut best_index = 0;
        let mut best_dist = f32::MAX;

        for (i, p) in self.palette_oklab.iter().enumerate() {
            let dist = color.distance_squared(p);
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

    /// Get the OKLab color for a palette index
    #[inline]
    pub fn get_oklab(&self, idx: PaletteIndex) -> &Oklab {
        &self.palette_oklab[idx.as_u8() as usize]
    }
}

impl Default for OklabPalette {
    fn default() -> Self {
        Self::new()
    }
}
