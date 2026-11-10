//! 6-color Spectra 6 palette and OKLab color math.
//!
//! Palette values from aitjcize/esp32-photoframe (measured e-paper colors).

use libm::powf;

/// Cube root in pure f32 (libm's `cbrtf` uses f64, which is software-emulated on the ESP32-S3).
///
/// Bit-level initial estimate refined with two Newton iterations; relative error < 1e-6.
#[inline]
fn cbrtf(x: f32) -> f32 {
    if x == 0.0 {
        return 0.0;
    }
    let a = x.abs();
    let mut y = f32::from_bits(a.to_bits() / 3 + 0x2A51_4067);
    y = (2.0 * y + a / (y * y)) * (1.0 / 3.0);
    y = (2.0 * y + a / (y * y)) * (1.0 / 3.0);
    if x < 0.0 { -y } else { y }
}

/// Palette indices
pub const BLACK: u8 = 0;
pub const WHITE: u8 = 1;
pub const RED: u8 = 2;
pub const YELLOW: u8 = 3;
pub const BLUE: u8 = 4;
pub const GREEN: u8 = 5;

/// Measured Spectra 6 palette (RGB), indexed by palette index
pub const PALETTE: [[u8; 3]; 6] = [
    [2, 2, 2],       // Black
    [232, 232, 232], // White
    [135, 19, 0],    // Red
    [205, 202, 0],   // Yellow
    [5, 64, 158],    // Blue
    [39, 102, 60],   // Green
];

/// OKLab color
#[derive(Debug, Clone, Copy, Default)]
pub struct Oklab {
    pub l: f32,
    pub a: f32,
    pub b: f32,
}

#[allow(clippy::excessive_precision)]
impl Oklab {
    pub const fn new(l: f32, a: f32, b: f32) -> Self {
        Self { l, a, b }
    }

    fn srgb_to_linear(c: u8) -> f32 {
        let c = c as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            powf((c + 0.055) / 1.055, 2.4)
        }
    }

    fn linear_to_srgb(c: f32) -> u8 {
        let c = if c <= 0.0031308 {
            c * 12.92
        } else {
            1.055 * powf(c, 1.0 / 2.4) - 0.055
        };
        (c * 255.0).clamp(0.0, 255.0) as u8
    }

    /// Convert from linear RGB
    pub fn from_linear(r: f32, g: f32, b: f32) -> Self {
        let l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
        let m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
        let s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;

        let l_ = cbrtf(l);
        let m_ = cbrtf(m);
        let s_ = cbrtf(s);

        Self {
            l: 0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_,
            a: 1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_,
            b: 0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_,
        }
    }

    /// Convert from sRGB bytes
    pub fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self::from_linear(
            Self::srgb_to_linear(r),
            Self::srgb_to_linear(g),
            Self::srgb_to_linear(b),
        )
    }

    /// Convert to sRGB bytes
    pub fn to_rgb(self) -> [u8; 3] {
        let l_ = self.l + 0.3963377774 * self.a + 0.2158037573 * self.b;
        let m_ = self.l - 0.1055613458 * self.a - 0.0638541728 * self.b;
        let s_ = self.l - 0.0894841775 * self.a - 1.2914855480 * self.b;

        let l = l_ * l_ * l_;
        let m = m_ * m_ * m_;
        let s = s_ * s_ * s_;

        [
            Self::linear_to_srgb(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
            Self::linear_to_srgb(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
            Self::linear_to_srgb(-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s),
        ]
    }

    #[inline]
    pub fn distance_squared(&self, other: &Oklab) -> f32 {
        let dl = self.l - other.l;
        let da = self.a - other.a;
        let db = self.b - other.b;
        dl * dl + da * da + db * db
    }
}

/// Palette matcher with a precomputed sRGB-to-linear lookup table
pub struct OklabPalette {
    palette: [Oklab; 6],
    linear: [f32; 256],
}

impl OklabPalette {
    pub fn new() -> Self {
        let mut linear = [0.0; 256];
        for (i, v) in linear.iter_mut().enumerate() {
            *v = Oklab::srgb_to_linear(i as u8);
        }
        Self {
            palette: PALETTE.map(|[r, g, b]| Oklab::from_rgb(r, g, b)),
            linear,
        }
    }

    /// Fast sRGB to OKLab using the lookup table
    #[inline]
    pub fn to_oklab(&self, r: u8, g: u8, b: u8) -> Oklab {
        Oklab::from_linear(
            self.linear[r as usize],
            self.linear[g as usize],
            self.linear[b as usize],
        )
    }

    /// Nearest palette index by OKLab distance
    #[inline]
    pub fn nearest(&self, color: &Oklab) -> u8 {
        let mut best = 0;
        let mut best_dist = f32::MAX;
        for (i, p) in self.palette.iter().enumerate() {
            let dist = color.distance_squared(p);
            if dist < best_dist {
                best_dist = dist;
                best = i;
            }
        }
        best as u8
    }

    #[inline]
    pub fn get(&self, idx: u8) -> &Oklab {
        &self.palette[idx as usize]
    }
}

impl Default for OklabPalette {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_cbrt_matches_libm() {
        let mut x = 1e-6f32;
        while x < 2.0 {
            let (fast, exact) = (cbrtf(x), libm::cbrtf(x));
            assert!(
                (fast - exact).abs() <= exact * 1e-5,
                "{x}: {fast} vs {exact}"
            );
            assert_eq!(cbrtf(-x), -fast);
            x *= 1.01;
        }
        assert_eq!(cbrtf(0.0), 0.0);
    }

    #[test]
    fn nearest_color() {
        let p = OklabPalette::new();
        assert_eq!(p.nearest(&Oklab::from_rgb(0, 0, 0)), BLACK);
        assert_eq!(p.nearest(&Oklab::from_rgb(255, 255, 255)), WHITE);
        assert_eq!(p.nearest(&Oklab::from_rgb(200, 50, 50)), RED);
    }
}
