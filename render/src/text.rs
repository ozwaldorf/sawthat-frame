//! Text rendering onto palette-indexed buffers using the embedded font.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};

use crate::palette::{BLACK, WHITE};

static FONT_DATA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/font.ttf"));

/// Band name sizes, largest first
const BAND_SIZES: &[f32] = &[48.0, 40.0, 32.0, 24.0, 20.0];
/// Venue sizes, largest first
const VENUE_SIZES: &[f32] = &[24.0, 20.0, 16.0];

fn font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT_DATA).expect("embedded font is invalid")
}

/// Concert info shown below the artwork
pub struct ConcertInfo<'a> {
    pub band_name: &'a str,
    pub date: &'a str,
    pub venue: &'a str,
}

/// Indexed image buffer view
pub struct Canvas<'a> {
    pub pixels: &'a mut [u8],
    pub width: u32,
    pub height: u32,
}

/// Width of `text` at `size` px
pub fn text_width(text: &str, size: f32) -> f32 {
    let font = font();
    let scaled = font.as_scaled(PxScale::from(size));
    text.chars()
        .map(|c| scaled.h_advance(font.glyph_id(c)))
        .sum()
}

/// Draw `text` with its top at `y`, starting at `x`
pub fn draw_text(canvas: &mut Canvas, text: &str, size: f32, x: f32, y: f32, color: u8) {
    let font = font();
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let mut cursor = x;

    for c in text.chars() {
        let id = font.glyph_id(c);
        let glyph = id.with_scale_and_position(scale, point(cursor, y + size * 0.8));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if coverage > 0.5
                    && px >= 0
                    && py >= 0
                    && (px as u32) < canvas.width
                    && (py as u32) < canvas.height
                {
                    canvas.pixels[py as usize * canvas.width as usize + px as usize] = color;
                }
            });
        }
        cursor += scaled.h_advance(id);
    }
}

/// Draw `text` horizontally centered with its top at `y`
pub fn draw_text_centered(canvas: &mut Canvas, text: &str, size: f32, y: f32, color: u8) {
    let x = ((canvas.width as f32 - text_width(text, size)) / 2.0).max(0.0);
    // Snap to whole pixels like the server's integer positioning
    draw_text(canvas, text, size, (x as u32) as f32, y, color);
}

/// Largest size from `sizes` that fits `max_width`, with its vertical offset
fn fit(text: &str, max_width: f32, sizes: &[f32]) -> (f32, u32) {
    for &size in sizes {
        if text_width(text, size) <= max_width {
            let offset = match size as u32 {
                48 => 0,
                40 => 4,
                32 => 8,
                24 => 12,
                _ => 16,
            };
            return (size, offset);
        }
    }
    (sizes.last().copied().unwrap_or(20.0), 16)
}

/// Render band name, date, and venue below `top`; black on light backgrounds, white on dark
pub fn render_concert_info(canvas: &mut Canvas, info: &ConcertInfo, top: u32, is_light_bg: bool) {
    let color = if is_light_bg { BLACK } else { WHITE };
    let max_width = canvas.width.saturating_sub(16) as f32;

    let (band_size, band_offset) = fit(info.band_name, max_width, BAND_SIZES);
    let band_y = top + band_offset;
    draw_text_centered(canvas, info.band_name, band_size, band_y as f32, color);

    let date_y = band_y + (band_size * 1.1) as u32;
    draw_text_centered(canvas, info.date, 24.0, date_y as f32, color);

    let (venue_size, _) = fit(info.venue, max_width, VENUE_SIZES);
    draw_text_centered(canvas, info.venue, venue_size, (date_y + 28) as f32, color);
}
