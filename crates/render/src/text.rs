//! Text rendering onto packed palette-indexed buffers.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};

use crate::palette::{BLACK, WHITE};

static FONT_DATA: &[u8] = include_bytes!("../assets/IBMPlexMono-Bold.ttf");

/// Band name sizes, largest first
const BAND_SIZES: &[f32] = &[48.0, 40.0, 32.0, 24.0, 20.0];
/// Venue sizes, largest first
const VENUE_SIZES: &[f32] = &[24.0, 20.0, 16.0];

/// Parse the embedded font; keep the result around rather than calling this per draw
pub fn font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT_DATA).expect("embedded font is invalid")
}

/// Parse a TTF/OTF font, or `None` if it is invalid
pub fn parse_font(data: &'static [u8]) -> Option<FontRef<'static>> {
    FontRef::try_from_slice(data).ok()
}

/// Concert info shown below the artwork
pub struct ConcertInfo<'a> {
    pub band_name: &'a str,
    pub date: &'a str,
    pub venue: &'a str,
}

/// Packed 4bpp palette-indexed image (high nibble first, rows not padded)
pub struct Canvas<'a> {
    pub data: &'a mut [u8],
    pub width: u32,
    pub height: u32,
}

impl Canvas<'_> {
    /// Bytes needed for a `width` x `height` canvas
    pub const fn len(width: u32, height: u32) -> usize {
        (width as usize * height as usize).div_ceil(2)
    }

    #[inline]
    pub fn set(&mut self, x: u32, y: u32, color: u8) {
        let i = (y * self.width + x) as usize;
        let b = &mut self.data[i / 2];
        *b = if i % 2 == 0 {
            (*b & 0x0F) | (color << 4)
        } else {
            (*b & 0xF0) | color
        };
    }

    #[inline]
    pub fn get(&self, x: u32, y: u32) -> u8 {
        let i = (y * self.width + x) as usize;
        let b = self.data[i / 2];
        if i % 2 == 0 { b >> 4 } else { b & 0x0F }
    }
}

/// Width of `text` at `size` px
pub fn text_width(font: &FontRef, text: &str, size: f32) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    text.chars()
        .map(|c| scaled.h_advance(font.glyph_id(c)))
        .sum()
}

/// Draw `text` with its top at `y`, starting at `x`
pub fn draw_text(
    canvas: &mut Canvas,
    font: &FontRef,
    text: &str,
    size: f32,
    (x, y): (f32, f32),
    color: u8,
) {
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
                    canvas.set(px as u32, py as u32, color);
                }
            });
        }
        cursor += scaled.h_advance(id);
    }
}

/// Draw `text` horizontally centered with its top at `y`
fn draw_text_centered(
    canvas: &mut Canvas,
    font: &FontRef,
    text: &str,
    size: f32,
    y: f32,
    color: u8,
) {
    let x = ((canvas.width as f32 - text_width(font, text, size)) / 2.0).max(0.0);
    // Snap to whole pixels for consistent glyph placement
    draw_text(canvas, font, text, size, ((x as u32) as f32, y), color);
}

/// Largest size from `sizes` that fits `max_width`, with its vertical offset
fn fit(font: &FontRef, text: &str, max_width: f32, sizes: &[f32]) -> (f32, u32) {
    for &size in sizes {
        if text_width(font, text, size) <= max_width {
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
pub fn render_concert_info(
    canvas: &mut Canvas,
    font: &FontRef,
    info: &ConcertInfo,
    top: u32,
    is_light_bg: bool,
) {
    let color = if is_light_bg { BLACK } else { WHITE };
    let max_width = canvas.width.saturating_sub(16) as f32;

    let (band_size, band_offset) = fit(font, info.band_name, max_width, BAND_SIZES);
    let band_y = top + band_offset;
    draw_text_centered(
        canvas,
        font,
        info.band_name,
        band_size,
        band_y as f32,
        color,
    );

    let date_y = band_y + (band_size * 1.1) as u32;
    draw_text_centered(canvas, font, info.date, 24.0, date_y as f32, color);

    let (venue_size, _) = fit(font, info.venue, max_width, VENUE_SIZES);
    draw_text_centered(
        canvas,
        font,
        info.venue,
        venue_size,
        (date_y + 28) as f32,
        color,
    );
}
