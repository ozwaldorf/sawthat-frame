//! 6-color palette for E Ink Spectra 6 display
//!
//! Uses OKLab color space for perceptually uniform color matching.
//! Palette values from aitjcize/esp32-photoframe (measured e-paper colors).

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

/// Measured Spectra 6 palette (from aitjcize/esp32-photoframe)
/// These values are actual measured e-paper display colors
pub const PALETTE: [Rgb; 6] = [
    Rgb::new(2, 2, 2),        // Black
    Rgb::new(232, 232, 232),  // White
    Rgb::new(135, 19, 0),     // Red
    Rgb::new(205, 202, 0),    // Yellow
    Rgb::new(5, 64, 158),     // Blue
    Rgb::new(39, 102, 60),    // Green
];

/// PNG palette bytes (RGB triplets) - same measured values
pub const PNG_PALETTE: [u8; 18] = [
    2, 2, 2,         // Black
    232, 232, 232,   // White
    135, 19, 0,      // Red
    205, 202, 0,     // Yellow
    5, 64, 158,      // Blue
    39, 102, 60,     // Green
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

/// Extracted dominant color with RGB values and lightness info
pub struct DominantColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub is_light: bool,
}

/// Extract dominant color from an image, excluding the center region.
/// First resizes to 50x50 using bilinear interpolation (like ossian.dev).
/// Weights pixels toward the bottom of the image (where the gradient will blend).
/// Returns the actual dominant color (dithering will approximate it).
pub fn extract_dominant_color(img: &image::RgbImage) -> DominantColor {
    use image::imageops::FilterType;
    use std::collections::HashMap;

    // Resize to 50x50 using bilinear (Triangle) filter for speed and natural color averaging
    let small = image::imageops::resize(img, 50, 50, FilterType::Triangle);
    let (width, height) = (50u32, 50u32);

    // Define center exclusion zone (middle 66%)
    let margin_x = width / 6;
    let margin_y = height / 6;

    // Collect unique colors and their accumulated weights
    // Key: RGB as u32, Value: (Oklab, accumulated_weight)
    let mut color_weights: HashMap<u32, (Oklab, f32)> = HashMap::new();

    for y in 0..height {
        // Linear weight favoring bottom (where gradient blends)
        let y_weight = (y + 1) as f32 / height as f32;

        for x in 0..width {
            // Skip center region (often contains text/subject)
            if x >= margin_x && x < width - margin_x && y >= margin_y && y < height - margin_y {
                continue;
            }

            let pixel = small.get_pixel(x, y);
            let rgb_key = ((pixel[0] as u32) << 16) | ((pixel[1] as u32) << 8) | (pixel[2] as u32);

            color_weights
                .entry(rgb_key)
                .and_modify(|(_, w)| *w += y_weight)
                .or_insert_with(|| {
                    let oklab = Oklab::from_rgb(pixel[0], pixel[1], pixel[2]);
                    (oklab, y_weight)
                });
        }
    }

    // Weighted average in Oklab space with sharpness applied to accumulated weights
    let sharpness = 4.0_f32;
    let mut sum_l = 0.0_f32;
    let mut sum_a = 0.0_f32;
    let mut sum_b = 0.0_f32;
    let mut total_weight = 0.0_f32;

    for (oklab, weight) in color_weights.values() {
        let w = weight.powf(sharpness);
        sum_l += oklab.l * w;
        sum_a += oklab.a * w;
        sum_b += oklab.b * w;
        total_weight += w;
    }

    // Compute weighted average
    let avg_l = sum_l / total_weight;
    let avg_a = sum_a / total_weight;
    let avg_b = sum_b / total_weight;

    // Convert back to RGB
    let oklab = Oklab::new(avg_l, avg_a, avg_b);
    let rgb = oklab.to_rgb();

    // Lightness threshold for text contrast (L > 0.6 in Oklab)
    let is_light = avg_l > 0.6;

    DominantColor {
        r: rgb.r,
        g: rgb.g,
        b: rgb.b,
        is_light,
    }
}
