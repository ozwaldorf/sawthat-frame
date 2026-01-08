//! Image processing for 6-color E Ink display
//!
//! - Resize to target dimensions
//! - Sierra-3 dithering to 6-color palette (entirely in OKLab color space)
//! - Render concert info text
//! - Encode as indexed PNG

use crate::error::AppError;
use crate::palette::{Oklab, OklabPalette, PaletteIndex, PNG_PALETTE};
use crate::text::{self, ConcertInfo};
use image::{DynamicImage, GenericImageView, RgbImage};
use png::{BitDepth, ColorType, Encoder};
use std::io::Cursor;

/// Height reserved for text info at bottom
const TEXT_AREA_HEIGHT: u32 = 120;

/// Process a source image for the e-paper display
///
/// 1. Resize to cover width (fill width, center crop any height overflow)
/// 2. Apply Sierra-3 dithering in OKLab space to 6-color palette (image only)
/// 3. Compose final canvas: dithered image + solid black text area
/// 4. Render concert info text (white on solid black for crisp text)
/// 5. Encode as indexed PNG
pub fn process_image(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
    concert_info: Option<&ConcertInfo>,
) -> Result<Vec<u8>, AppError> {
    // Decode source image
    let img = image::load_from_memory(image_data)
        .map_err(|e| AppError::ImageProcessing(format!("Failed to decode image: {}", e)))?;

    // Calculate image area (leave room for text)
    let image_area_height = target_height - TEXT_AREA_HEIGHT;

    // Resize to cover image area (fill width, center crop height)
    let resized = resize_cover(&img, target_width, image_area_height);

    // Apply Sierra-3 dithering in OKLab space to just the image
    let dithered_image = sierra3_dither(&resized);

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

/// Apply Sierra-3 dithering to convert RGB image to 6-color indexed
/// All operations performed in OKLab color space for perceptual uniformity
fn sierra3_dither(img: &RgbImage) -> Vec<u8> {
    let (width, height) = img.dimensions();
    let mut indexed = vec![0u8; (width * height) as usize];

    // Precompute OKLab palette for faster lookups
    let oklab_palette = OklabPalette::new();

    // Working buffer in OKLab space for error accumulation
    let mut buffer: Vec<Oklab> = img
        .pixels()
        .map(|p| Oklab::from_rgb(p[0], p[1], p[2]))
        .collect();

    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) as usize;

            // Get current pixel in OKLab space
            let current = buffer[idx];

            // Find nearest palette color using OKLab perceptual distance
            let palette_idx = oklab_palette.nearest(&current);
            indexed[idx] = palette_idx.as_u8();

            // Get the palette color in OKLab space
            let target = oklab_palette.get_oklab(palette_idx);

            // Calculate quantization error in OKLab space
            let err_l = current.l - target.l;
            let err_a = current.a - target.a;
            let err_b = current.b - target.b;

            // Distribute error to neighboring pixels (Sierra-3 pattern)
            // Sierra-3 (Sierra Lite): simpler pattern with 3 neighbors
            //       * 2/4
            //   1/4 1/4

            // Right: 2/4
            if x + 1 < width {
                let right_idx = idx + 1;
                buffer[right_idx].l += err_l * 0.5;
                buffer[right_idx].a += err_a * 0.5;
                buffer[right_idx].b += err_b * 0.5;
            }

            if y + 1 < height {
                // Bottom-left: 1/4
                if x > 0 {
                    let bl_idx = idx + width as usize - 1;
                    buffer[bl_idx].l += err_l * 0.25;
                    buffer[bl_idx].a += err_a * 0.25;
                    buffer[bl_idx].b += err_b * 0.25;
                }

                // Bottom: 1/4
                let bottom_idx = idx + width as usize;
                buffer[bottom_idx].l += err_l * 0.25;
                buffer[bottom_idx].a += err_a * 0.25;
                buffer[bottom_idx].b += err_b * 0.25;
            }
        }
    }

    indexed
}

/// Encode indexed pixel data as PNG with 6-color palette
fn encode_indexed_png(indexed: &[u8], width: u32, height: u32) -> Result<Vec<u8>, AppError> {
    let mut output = Vec::new();

    {
        let mut encoder = Encoder::new(Cursor::new(&mut output), width, height);
        encoder.set_color(ColorType::Indexed);
        encoder.set_depth(BitDepth::Eight);
        encoder.set_palette(PNG_PALETTE.to_vec());

        let mut writer = encoder
            .write_header()
            .map_err(|e| AppError::ImageProcessing(format!("PNG header error: {}", e)))?;

        writer
            .write_image_data(indexed)
            .map_err(|e| AppError::ImageProcessing(format!("PNG write error: {}", e)))?;
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nearest_color() {
        let palette = OklabPalette::new();
        assert_eq!(palette.nearest(&Oklab::from_rgb(0, 0, 0)), PaletteIndex::Black);
        assert_eq!(palette.nearest(&Oklab::from_rgb(255, 255, 255)), PaletteIndex::White);
        assert_eq!(palette.nearest(&Oklab::from_rgb(200, 50, 50)), PaletteIndex::Red);
    }
}
