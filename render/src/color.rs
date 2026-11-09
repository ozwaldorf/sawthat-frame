//! Background color extraction from the bottom edge of an image.

use alloc::vec::Vec;

use crate::adjust::Adjuster;
use crate::palette::Oklab;
use crate::resize::{RgbImage, resize_window};

/// Dominant color used for the text area background
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrimaryColor {
    pub rgb: [u8; 3],
    pub is_light: bool,
}

/// Extract the dominant color from the bottom 10% of the adjusted image.
///
/// Adjusts, downscales to 100x100, then takes the 3 most common colors
/// in the last 10 rows and averages them in OKLab, weighted by count.
pub fn primary_color(src: &RgbImage, adjuster: &Adjuster) -> PrimaryColor {
    let rows = resize_window(src, 100, 100, 0, 90, 100, 10, |p| adjuster.apply(p));

    // (rgb, count) in first-seen order
    let mut counts: Vec<([u8; 3], u32)> = Vec::new();
    for px in rows.data.chunks_exact(3) {
        let rgb = [px[0], px[1], px[2]];
        match counts.iter_mut().find(|(c, _)| *c == rgb) {
            Some((_, n)) => *n += 1,
            None => counts.push((rgb, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));

    let mut sum = Oklab::default();
    let mut total = 0u32;
    for &([r, g, b], n) in counts.iter().take(3) {
        let c = Oklab::from_rgb(r, g, b);
        sum.l += c.l * n as f32;
        sum.a += c.a * n as f32;
        sum.b += c.b * n as f32;
        total += n;
    }
    let avg = Oklab::new(
        sum.l / total as f32,
        sum.a / total as f32,
        sum.b / total as f32,
    );

    PrimaryColor {
        rgb: avg.to_rgb(),
        is_light: avg.l > 0.6,
    }
}
