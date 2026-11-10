//! Triangle (bilinear) resampling matching `image::imageops::resize` with `FilterType::Triangle`.
//!
//! Computes only a window of the resized image, row by row, so a full-size intermediate
//! buffer is never allocated.

use alloc::vec;
use alloc::vec::Vec;
use libm::{ceilf, floorf, roundf};

/// Packed RGB8 image
pub struct RgbImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl RgbImage {
    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }
}

/// Filter taps for one output coordinate
struct Taps {
    start: u32,
    weights: Vec<f32>,
}

fn triangle(x: f32) -> f32 {
    let x = x.abs();
    if x < 1.0 { 1.0 - x } else { 0.0 }
}

/// Compute taps for output coordinates `range` when resampling `src` -> `dst`
fn taps(src: u32, dst: u32, range: core::ops::Range<u32>) -> Vec<Taps> {
    let ratio = src as f32 / dst as f32;
    let sratio = ratio.max(1.0);
    let support = sratio;

    range
        .map(|out| {
            let center = (out as f32 + 0.5) * ratio;
            let left = (floorf(center - support) as i64).clamp(0, src as i64 - 1) as u32;
            let right = (ceilf(center + support) as i64).clamp(left as i64 + 1, src as i64) as u32;
            let center = center - 0.5;

            let mut weights: Vec<f32> = (left..right)
                .map(|i| triangle((i as f32 - center) / sratio))
                .collect();
            let sum: f32 = weights.iter().sum();
            if sum != 0.0 {
                weights.iter_mut().for_each(|w| *w /= sum);
            }
            Taps {
                start: left,
                weights,
            }
        })
        .collect()
}

/// Resample `src` to `new_w` x `new_h` and return the window at (`x0`, `y0`) of size `w` x `h`.
///
/// `map` is applied to every source pixel before filtering.
#[allow(clippy::too_many_arguments)]
pub fn resize_window<F: Fn([u8; 3]) -> [u8; 3]>(
    src: &RgbImage,
    new_w: u32,
    new_h: u32,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    map: F,
) -> RgbImage {
    let xtaps = taps(src.width, new_w, x0..x0 + w);
    let ytaps = taps(src.height, new_h, y0..y0 + h);

    // Columns of the source needed by the requested output window
    let col_lo = xtaps.first().map_or(0, |t| t.start);
    let col_hi = xtaps.last().map_or(0, |t| t.start + t.weights.len() as u32);
    let cols = (col_hi - col_lo) as usize;

    let mut acc = vec![0.0f32; cols * 3];
    let mut out = Vec::with_capacity((w * h * 3) as usize);

    for yt in &ytaps {
        // Vertical pass: weighted sum of source rows into `acc`
        acc.iter_mut().for_each(|v| *v = 0.0);
        for (k, &wy) in yt.weights.iter().enumerate() {
            let sy = yt.start + k as u32;
            for c in 0..cols {
                let p = map(src.pixel(col_lo + c as u32, sy));
                let a = &mut acc[c * 3..c * 3 + 3];
                a[0] += p[0] as f32 * wy;
                a[1] += p[1] as f32 * wy;
                a[2] += p[2] as f32 * wy;
            }
        }

        // Horizontal pass over the accumulated row
        for xt in &xtaps {
            let mut sum = [0.0f32; 3];
            let base = (xt.start - col_lo) as usize;
            for (k, &wx) in xt.weights.iter().enumerate() {
                let a = &acc[(base + k) * 3..(base + k) * 3 + 3];
                sum[0] += a[0] * wx;
                sum[1] += a[1] * wx;
                sum[2] += a[2] * wx;
            }
            out.extend(sum.map(|v| roundf(v).clamp(0.0, 255.0) as u8));
        }
    }

    RgbImage {
        width: w,
        height: h,
        data: out,
    }
}

/// Resize to cover `w` x `h` (scale to fill, center crop)
pub fn resize_cover(src: &RgbImage, w: u32, h: u32) -> RgbImage {
    let scale = (w as f32 / src.width as f32).max(h as f32 / src.height as f32);
    let new_w = (roundf(src.width as f32 * scale) as u32).max(w);
    let new_h = (roundf(src.height as f32 * scale) as u32).max(h);
    let x0 = (new_w - w) / 2;
    let y0 = (new_h - h) / 2;

    // Already at the covering size: center crop only
    if (new_w, new_h) == (src.width, src.height) {
        let mut data = Vec::with_capacity((w * h * 3) as usize);
        for y in y0..y0 + h {
            let start = ((y * src.width + x0) * 3) as usize;
            data.extend_from_slice(&src.data[start..start + (w * 3) as usize]);
        }
        return RgbImage {
            width: w,
            height: h,
            data,
        };
    }

    resize_window(src, new_w, new_h, x0, y0, w, h, |p| p)
}
