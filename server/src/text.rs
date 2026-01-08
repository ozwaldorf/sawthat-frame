//! Text rendering for e-paper display
//!
//! Renders text onto indexed images using embedded fonts.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

/// Font found at build time via fontconfig (see build.rs)
const FONT_DATA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/font.ttf"));

/// Palette indices
const BLACK_INDEX: u8 = 0;
const WHITE_INDEX: u8 = 1;

/// Font size steps for band name (largest to smallest)
const BAND_SIZES: &[f32] = &[48.0, 40.0, 32.0, 24.0, 20.0];

/// Font size steps for venue (largest to smallest)
const VENUE_SIZES: &[f32] = &[24.0, 20.0, 16.0];

/// Concert info to render
pub struct ConcertInfo {
    pub band_name: String,
    pub date: String,
    pub venue: String,
}

/// Render concert info text onto an indexed buffer (post-dithering)
/// Places text in the bottom area (below the image)
/// Uses black text on light backgrounds, white text on dark backgrounds
pub fn render_concert_info_indexed(
    indexed: &mut [u8],
    width: u32,
    info: &ConcertInfo,
    text_area_top: u32,
    is_light_bg: bool,
) {
    let font = FontRef::try_from_slice(FONT_DATA).expect("Failed to load font");
    let text_color = if is_light_bg {
        BLACK_INDEX
    } else {
        WHITE_INDEX
    };

    // Leave some horizontal padding (8px each side)
    let max_width = width.saturating_sub(16) as f32;

    // Band name - find largest font size that fits
    let (band_scale, band_y_offset) = fit_text_size(&font, &info.band_name, max_width, BAND_SIZES);
    let band_y = text_area_top + band_y_offset;
    draw_text_indexed_centered(
        indexed,
        width,
        &font,
        &info.band_name,
        band_scale,
        band_y,
        text_color,
    );

    // Calculate remaining space and position date/venue accordingly
    let band_height = (band_scale.y * 1.1) as u32;

    // Date - fixed size (24px)
    let date_scale = PxScale::from(24.0);
    let date_y = band_y + band_height;
    draw_text_indexed_centered(
        indexed, width, &font, &info.date, date_scale, date_y, text_color,
    );

    // Venue - scale to fit if needed
    let (venue_scale, _) = fit_text_size(&font, &info.venue, max_width, VENUE_SIZES);
    let venue_y = date_y + 28;
    draw_text_indexed_centered(
        indexed,
        width,
        &font,
        &info.venue,
        venue_scale,
        venue_y,
        text_color,
    );
}

/// Find the largest font size that fits the text within max_width
fn fit_text_size(font: &FontRef, text: &str, max_width: f32, sizes: &[f32]) -> (PxScale, u32) {
    for &size in sizes {
        let scale = PxScale::from(size);
        let text_width = measure_text_width(font, text, scale);
        if text_width <= max_width {
            // Y offset decreases as font gets smaller to keep text vertically centered
            let y_offset = match size as u32 {
                48 => 0,
                40 => 4,
                32 => 8,
                24 => 12,
                _ => 16,
            };
            return (scale, y_offset);
        }
    }
    // Fallback to smallest size
    let smallest = sizes.last().copied().unwrap_or(20.0);
    (PxScale::from(smallest), 16)
}

/// Measure the width of text at a given scale
fn measure_text_width(font: &FontRef, text: &str, scale: PxScale) -> f32 {
    let scaled_font = font.as_scaled(scale);
    text.chars()
        .map(|c| {
            let glyph_id = font.glyph_id(c);
            scaled_font.h_advance(glyph_id)
        })
        .sum()
}

/// Draw text centered horizontally onto indexed buffer
fn draw_text_indexed_centered(
    indexed: &mut [u8],
    width: u32,
    font: &FontRef,
    text: &str,
    scale: PxScale,
    y: u32,
    color: u8,
) {
    let scaled_font = font.as_scaled(scale);

    // Calculate text width
    let text_width: f32 = text
        .chars()
        .map(|c| {
            let glyph_id = font.glyph_id(c);
            scaled_font.h_advance(glyph_id)
        })
        .sum();

    // Center horizontally
    let x = ((width as f32 - text_width) / 2.0).max(0.0) as u32;

    draw_text_indexed(indexed, width, font, text, scale, x, y, color);
}

/// Draw text at a specific position onto indexed buffer
fn draw_text_indexed(
    indexed: &mut [u8],
    width: u32,
    font: &FontRef,
    text: &str,
    scale: PxScale,
    x: u32,
    y: u32,
    color: u8,
) {
    let scaled_font = font.as_scaled(scale);
    let mut cursor_x = x as f32;
    let height = indexed.len() as u32 / width;

    for c in text.chars() {
        let glyph_id = font.glyph_id(c);
        let glyph = glyph_id
            .with_scale_and_position(scale, ab_glyph::point(cursor_x, y as f32 + scale.y * 0.8));

        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x as u32 + gx;
                let py = bounds.min.y as u32 + gy;

                // Hard edge threshold (0.5 for clean edges with bold fonts)
                if px < width && py < height && coverage > 0.5 {
                    let idx = (py * width + px) as usize;
                    if idx < indexed.len() {
                        indexed[idx] = color;
                    }
                }
            });
        }

        cursor_x += scaled_font.h_advance(glyph_id);
    }
}
