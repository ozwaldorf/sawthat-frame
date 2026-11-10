//! Canvas composition and Floyd-Steinberg dithering to the 6-color palette in OKLab space.
//!
//! Processes one row at a time with two error rows, writing packed 4bpp output directly.

use alloc::vec;

use crate::palette::{Oklab, OklabPalette};
use crate::resize::RgbImage;
use crate::text::Canvas;

/// Height of the gradient blending the image into the text background
pub const GRADIENT_HEIGHT: u32 = 80;

#[inline]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).clamp(0.0, 255.0) as u8
}

/// Canvas pixel: image, gradient into `bg`, then solid `bg` below the image
#[inline]
fn canvas_pixel(img: &RgbImage, bg: [u8; 3], x: u32, y: u32) -> [u8; 3] {
    let gradient_start = img.height.saturating_sub(GRADIENT_HEIGHT);
    if y < gradient_start {
        img.pixel(x, y)
    } else if y < img.height {
        let p = img.pixel(x, y);
        let t = (y - gradient_start) as f32 / GRADIENT_HEIGHT as f32;
        let t = t * t * (3.0 - 2.0 * t);
        [
            lerp_u8(p[0], bg[0], t),
            lerp_u8(p[1], bg[1], t),
            lerp_u8(p[2], bg[2], t),
        ]
    } else {
        bg
    }
}

/// Compose `img` (full width, top of `canvas`) over `bg` and dither into the canvas.
pub fn compose_and_dither(
    img: &RgbImage,
    bg: [u8; 3],
    canvas: &mut Canvas,
    palette: &OklabPalette,
) {
    // Neighbouring pixels often share a color (the solid text area especially)
    let mut last_rgb = None;
    let mut last_oklab = Oklab::default();
    floyd_steinberg(
        img.width,
        canvas.height,
        palette,
        |x, y| {
            let rgb = canvas_pixel(img, bg, x, y);
            if last_rgb != Some(rgb) {
                last_oklab = palette.to_oklab(rgb[0], rgb[1], rgb[2]);
                last_rgb = Some(rgb);
            }
            last_oklab
        },
        |x, y, idx| canvas.set(x, y, idx),
    );
}

/// Floyd-Steinberg dither a `width` x `height` area to the palette in OKLab space.
///
/// `pixel` gives the source color at (x, y) and `out` receives the palette index; both are
/// called in row-major order.
pub fn floyd_steinberg(
    width: u32,
    height: u32,
    palette: &OklabPalette,
    mut pixel: impl FnMut(u32, u32) -> Oklab,
    mut out: impl FnMut(u32, u32, u8),
) {
    let width = width as usize;
    let mut cur = vec![Oklab::default(); width];
    let mut next = vec![Oklab::default(); width];

    for y in 0..height {
        for x in 0..width {
            let (base, e) = (pixel(x as u32, y), cur[x]);
            let px = Oklab::new(base.l + e.l, base.a + e.a, base.b + e.b);

            let idx = palette.nearest(&px);
            out(x as u32, y, idx);

            let t = palette.get(idx);
            let err = Oklab::new(px.l - t.l, px.a - t.a, px.b - t.b);
            let spread = |c: &mut Oklab, f: f32| {
                c.l += err.l * f;
                c.a += err.a * f;
                c.b += err.b * f;
            };

            if x + 1 < width {
                spread(&mut cur[x + 1], 7.0 / 16.0);
                spread(&mut next[x + 1], 1.0 / 16.0);
            }
            if x > 0 {
                spread(&mut next[x - 1], 3.0 / 16.0);
            }
            spread(&mut next[x], 5.0 / 16.0);
        }

        core::mem::swap(&mut cur, &mut next);
        next.iter_mut().for_each(|e| *e = Oklab::default());
    }
}
