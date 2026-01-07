//! Image processing for 6-color E Ink display
//!
//! - Resize to target dimensions
//! - Render concert info text
//! - Floyd-Steinberg dithering to 6-color palette (using Oklab color space)
//! - Encode as indexed PNG

use crate::palette::{OklabPalette, Rgb, PaletteIndex, PNG_PALETTE, PALETTE};
use crate::text::{self, ConcertInfo};
use fastly::Error;
use image::{DynamicImage, GenericImageView, RgbImage};
use png::{BitDepth, ColorType, Encoder};
use std::io::Cursor;

/// Display dimensions
pub const DISPLAY_WIDTH: u32 = 800;
pub const DISPLAY_HEIGHT: u32 = 480;
pub const HALF_WIDTH: u32 = 400;

/// Height reserved for text info at bottom
pub const TEXT_AREA_HEIGHT: u32 = 120;
/// Image area height (total height - text area)
pub const IMAGE_AREA_HEIGHT: u32 = DISPLAY_HEIGHT - TEXT_AREA_HEIGHT;

/// Process a source image for the e-paper display
///
/// 1. Resize to cover width (fill width, center crop any height overflow)
/// 2. Apply Floyd-Steinberg dithering to 6-color palette (image only)
/// 3. Compose final canvas: dithered image + solid black text area
/// 4. Render concert info text (white on solid black for crisp text)
/// 5. Encode as indexed PNG
pub fn process_image(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
    concert_info: Option<&ConcertInfo>,
) -> Result<Vec<u8>, Error> {
    // Decode source image
    let img = image::load_from_memory(image_data)
        .map_err(|e| Error::msg(format!("Failed to decode image: {}", e)))?;

    // Calculate image area (leave room for text)
    let image_area_height = target_height - TEXT_AREA_HEIGHT;

    // Resize to cover image area (fill width, center crop height)
    let resized = resize_cover(&img, target_width, image_area_height);

    // Apply Floyd-Steinberg dithering to just the image
    let dithered_image = floyd_steinberg_dither(&resized);

    // Compose final canvas: dithered image at top, solid black text area at bottom
    let mut indexed = vec![PaletteIndex::Black.as_u8(); (target_width * target_height) as usize];

    // Copy dithered image to top of canvas
    let image_pixels = (target_width * image_area_height) as usize;
    indexed[..image_pixels].copy_from_slice(&dithered_image);

    // Render concert info text on the solid black text area
    if let Some(info) = concert_info {
        text::render_concert_info_indexed(&mut indexed, target_width, info, image_area_height);
    }

    // Encode as indexed PNG
    encode_indexed_png(&indexed, target_width, target_height)
}

/// Resize image to cover the target area (fill width, center crop height)
/// Returns an image of exactly target_width x target_height
fn resize_cover(img: &DynamicImage, target_width: u32, target_height: u32) -> RgbImage {
    let (src_width, src_height) = img.dimensions();

    // Calculate scale to cover the target area (larger of the two scales)
    let scale_x = target_width as f32 / src_width as f32;
    let scale_y = target_height as f32 / src_height as f32;
    let scale = scale_x.max(scale_y);

    // Calculate new size (will be >= target in at least one dimension)
    let new_width = (src_width as f32 * scale).round() as u32;
    let new_height = (src_height as f32 * scale).round() as u32;

    // Resize (use Triangle/bilinear for speed - good enough for dithered output)
    let resized = img.resize_exact(new_width, new_height, image::imageops::FilterType::Triangle);
    let resized_rgb = resized.to_rgb8();

    // Create output image
    let mut output = RgbImage::new(target_width, target_height);

    // Calculate crop offsets to center the image
    let crop_x = new_width.saturating_sub(target_width) / 2;
    let crop_y = new_height.saturating_sub(target_height) / 2;

    // Copy the center portion of the resized image to output
    for out_y in 0..target_height {
        for out_x in 0..target_width {
            let src_x = out_x + crop_x;
            let src_y = out_y + crop_y;
            if src_x < new_width && src_y < new_height {
                let pixel = resized_rgb.get_pixel(src_x, src_y);
                output.put_pixel(out_x, out_y, *pixel);
            }
        }
    }

    output
}

/// Apply Floyd-Steinberg dithering to convert RGB image to 6-color indexed
/// Uses Oklab color space for perceptually uniform color matching
fn floyd_steinberg_dither(img: &RgbImage) -> Vec<u8> {
    let (width, height) = img.dimensions();
    let mut indexed = vec![0u8; (width * height) as usize];

    // Precompute Oklab palette for faster lookups
    let oklab_palette = OklabPalette::new();

    // Working buffer with signed integers for error accumulation
    // Store as (r, g, b) with extra precision
    let mut buffer: Vec<(i32, i32, i32)> = img
        .pixels()
        .map(|p| (p[0] as i32, p[1] as i32, p[2] as i32))
        .collect();

    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) as usize;

            // Get current pixel (clamped to valid range)
            let (r, g, b) = buffer[idx];
            let current = Rgb::new(
                r.clamp(0, 255) as u8,
                g.clamp(0, 255) as u8,
                b.clamp(0, 255) as u8,
            );

            // Find nearest palette color using Oklab perceptual distance
            let palette_idx = oklab_palette.nearest(&current);
            indexed[idx] = palette_idx.as_u8();

            // Get the actual palette color
            let target = &PALETTE[palette_idx.as_u8() as usize];

            // Calculate quantization error in RGB space
            // (error diffusion still works well in RGB)
            let err_r = r - target.r as i32;
            let err_g = g - target.g as i32;
            let err_b = b - target.b as i32;

            // Distribute error to neighboring pixels (Floyd-Steinberg pattern)
            // Right: 7/16
            if x + 1 < width {
                let right_idx = idx + 1;
                buffer[right_idx].0 += err_r * 7 / 16;
                buffer[right_idx].1 += err_g * 7 / 16;
                buffer[right_idx].2 += err_b * 7 / 16;
            }

            if y + 1 < height {
                // Bottom-left: 3/16
                if x > 0 {
                    let bl_idx = idx + width as usize - 1;
                    buffer[bl_idx].0 += err_r * 3 / 16;
                    buffer[bl_idx].1 += err_g * 3 / 16;
                    buffer[bl_idx].2 += err_b * 3 / 16;
                }

                // Bottom: 5/16
                let bottom_idx = idx + width as usize;
                buffer[bottom_idx].0 += err_r * 5 / 16;
                buffer[bottom_idx].1 += err_g * 5 / 16;
                buffer[bottom_idx].2 += err_b * 5 / 16;

                // Bottom-right: 1/16
                if x + 1 < width {
                    let br_idx = idx + width as usize + 1;
                    buffer[br_idx].0 += err_r / 16;
                    buffer[br_idx].1 += err_g / 16;
                    buffer[br_idx].2 += err_b / 16;
                }
            }
        }
    }

    indexed
}

/// Encode indexed pixel data as PNG with 6-color palette
fn encode_indexed_png(indexed: &[u8], width: u32, height: u32) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();

    {
        let mut encoder = Encoder::new(Cursor::new(&mut output), width, height);
        encoder.set_color(ColorType::Indexed);
        encoder.set_depth(BitDepth::Eight);
        encoder.set_palette(PNG_PALETTE.to_vec());

        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::msg(format!("PNG header error: {}", e)))?;

        writer
            .write_image_data(indexed)
            .map_err(|e| Error::msg(format!("PNG write error: {}", e)))?;
    }

    Ok(output)
}

/// Create a simple placeholder image for testing
pub fn create_placeholder_image(
    width: u32,
    height: u32,
    _label: &str,
) -> Result<Vec<u8>, Error> {
    // Create a simple gradient pattern with text-like blocks
    let mut indexed = vec![PaletteIndex::White.as_u8(); (width * height) as usize];

    // Draw a border
    for x in 0..width {
        indexed[x as usize] = PaletteIndex::Black.as_u8();
        indexed[((height - 1) * width + x) as usize] = PaletteIndex::Black.as_u8();
    }
    for y in 0..height {
        indexed[(y * width) as usize] = PaletteIndex::Black.as_u8();
        indexed[(y * width + width - 1) as usize] = PaletteIndex::Black.as_u8();
    }

    // Draw some colored blocks to represent content
    let colors = [
        PaletteIndex::Red,
        PaletteIndex::Yellow,
        PaletteIndex::Blue,
        PaletteIndex::Green,
    ];

    let block_size = 40u32;
    let start_x = (width - block_size * 4) / 2;
    let start_y = height / 2 - block_size / 2;

    for (i, color) in colors.iter().enumerate() {
        let bx = start_x + i as u32 * block_size;
        for y in start_y..(start_y + block_size).min(height - 1) {
            for x in bx..(bx + block_size - 4).min(width - 1) {
                indexed[(y * width + x) as usize] = color.as_u8();
            }
        }
    }

    encode_indexed_png(&indexed, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nearest_color() {
        let palette = OklabPalette::new();
        // Pure black
        assert_eq!(palette.nearest(&Rgb::new(0, 0, 0)), PaletteIndex::Black);
        // Pure white
        assert_eq!(palette.nearest(&Rgb::new(255, 255, 255)), PaletteIndex::White);
        // Red-ish
        assert_eq!(palette.nearest(&Rgb::new(200, 50, 50)), PaletteIndex::Red);
    }

    #[test]
    fn test_placeholder_image() {
        let result = create_placeholder_image(400, 480, "test");
        assert!(result.is_ok());
        let png = result.unwrap();
        // PNG magic bytes
        assert_eq!(&png[0..4], &[137, 80, 78, 71]);
    }
}
