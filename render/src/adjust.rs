//! Exposure, saturation, and S-curve adjustments (aitjcize/esp32-photoframe style).

use libm::powf;

const EXPOSURE: f32 = 0.8;
const SATURATION: f32 = 2.0;
const SCURVE_STRENGTH: f32 = 1.0;
const SCURVE_SHADOW_BOOST: f32 = 0.0;
const SCURVE_HIGHLIGHT_COMPRESS: f32 = 2.0;
const SCURVE_MIDPOINT: f32 = 0.5;

fn scurve(v: f32) -> f32 {
    if v <= SCURVE_MIDPOINT {
        let exponent = 1.0 - SCURVE_STRENGTH * SCURVE_SHADOW_BOOST;
        powf(v / SCURVE_MIDPOINT, exponent) * SCURVE_MIDPOINT
    } else {
        let exponent = 1.0 + SCURVE_STRENGTH * SCURVE_HIGHLIGHT_COMPRESS;
        let h = (v - SCURVE_MIDPOINT) / (1.0 - SCURVE_MIDPOINT);
        SCURVE_MIDPOINT + powf(h, exponent) * (1.0 - SCURVE_MIDPOINT)
    }
}

/// HSL-based saturation boost
fn saturate(r: u8, g: u8, b: u8) -> [u8; 3] {
    let rn = r as f32 / 255.0;
    let gn = g as f32 / 255.0;
    let bn = b as f32 / 255.0;

    let max = rn.max(gn).max(bn);
    let min = rn.min(gn).min(bn);
    let delta = max - min;
    let l = (max + min) / 2.0;

    if delta < 1e-6 {
        return [r, g, b];
    }

    let h = if (max - rn).abs() < 1e-6 {
        // In [-1, 1], so the modulo 6 of the HSL formula is a no-op
        (gn - bn) / delta
    } else if (max - gn).abs() < 1e-6 {
        (bn - rn) / delta + 2.0
    } else {
        (rn - gn) / delta + 4.0
    };
    let h = if h < 0.0 { h + 6.0 } else { h };

    let s = if !(1e-6..=1.0 - 1e-6).contains(&l) {
        0.0
    } else {
        delta / (1.0 - (2.0 * l - 1.0).abs())
    };
    let s = (s * SATURATION).clamp(0.0, 1.0);

    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    // h is in [0, 6): truncation gives h mod 2 without a libm fmodf call
    let h_mod2 = h - 2.0 * ((h * 0.5) as u32 as f32);
    let x = c * (1.0 - (h_mod2 - 1.0).abs());
    let m = l - c / 2.0;

    let (r1, g1, b1) = if h < 1.0 {
        (c, x, 0.0)
    } else if h < 2.0 {
        (x, c, 0.0)
    } else if h < 3.0 {
        (0.0, c, x)
    } else if h < 4.0 {
        (0.0, x, c)
    } else if h < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    [
        ((r1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).clamp(0.0, 255.0) as u8,
    ]
}

/// Precomputed adjustment pipeline
pub struct Adjuster {
    exposure: [u8; 256],
    scurve: [u8; 256],
}

impl Adjuster {
    pub fn new() -> Self {
        let mut exposure = [0u8; 256];
        let mut curve = [0u8; 256];
        for i in 0..256 {
            exposure[i] = (i as f32 * EXPOSURE).min(255.0) as u8;
            curve[i] = (scurve(i as f32 / 255.0) * 255.0).clamp(0.0, 255.0) as u8;
        }
        Self {
            exposure,
            scurve: curve,
        }
    }

    /// Apply exposure, saturation, then S-curve to one pixel
    #[inline]
    pub fn apply(&self, [r, g, b]: [u8; 3]) -> [u8; 3] {
        let e = &self.exposure;
        let [r, g, b] = saturate(e[r as usize], e[g as usize], e[b as usize]);
        let s = &self.scurve;
        [s[r as usize], s[g as usize], s[b as usize]]
    }

    /// Apply to a packed RGB buffer in place
    pub fn apply_rgb(&self, rgb: &mut [u8]) {
        for px in rgb.chunks_exact_mut(3) {
            let out = self.apply([px[0], px[1], px[2]]);
            px.copy_from_slice(&out);
        }
    }
}

impl Default for Adjuster {
    fn default() -> Self {
        Self::new()
    }
}
