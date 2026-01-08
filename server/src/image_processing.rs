//! Image processing for 6-color E Ink display
//!
//! Pipeline:
//! 1. Resize to target dimensions
//! 2. Apply exposure/saturation/s-curve adjustments
//! 3. Extract dominant color from image edges
//! 4. Compose canvas: image + gradient + solid color text area
//! 5. Floyd-Steinberg dithering to 6-color palette (OKLab color space)
//! 6. Render concert info text (black or white based on background)
//! 7. Encode as indexed PNG

use crate::error::AppError;
use crate::palette::{extract_dominant_color, Oklab, OklabPalette, PaletteIndex, PNG_PALETTE};
use crate::text::{self, ConcertInfo};
use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
use png::{BitDepth, ColorType, Encoder};
use std::io::Cursor;

/// Height reserved for text info at bottom
const TEXT_AREA_HEIGHT: u32 = 120;

/// Height of the gradient transition zone
const GRADIENT_HEIGHT: u32 = 80;

// Image adjustment parameters (aitjcize/esp32-photoframe style)
const EXPOSURE: f32 = 0.8;
const SATURATION: f32 = 2.0;
const SCURVE_STRENGTH: f32 = 1.0;
const SCURVE_SHADOW_BOOST: f32 = 0.0;
const SCURVE_HIGHLIGHT_COMPRESS: f32 = 2.0;
const SCURVE_MIDPOINT: f32 = 0.5;

/// Apply exposure adjustment to a single channel value
#[inline]
fn apply_exposure(value: u8) -> u8 {
    (value as f32 * EXPOSURE).min(255.0) as u8
}

/// Apply S-curve tone mapping to a normalized [0,1] value
#[inline]
fn apply_scurve(normalized: f32) -> f32 {
    if normalized <= SCURVE_MIDPOINT {
        // Shadows region
        let shadow_val = normalized / SCURVE_MIDPOINT;
        let exponent = 1.0 - SCURVE_STRENGTH * SCURVE_SHADOW_BOOST;
        shadow_val.powf(exponent) * SCURVE_MIDPOINT
    } else {
        // Highlights region
        let highlight_val = (normalized - SCURVE_MIDPOINT) / (1.0 - SCURVE_MIDPOINT);
        let exponent = 1.0 + SCURVE_STRENGTH * SCURVE_HIGHLIGHT_COMPRESS;
        SCURVE_MIDPOINT + highlight_val.powf(exponent) * (1.0 - SCURVE_MIDPOINT)
    }
}

/// Apply saturation adjustment using HSL color space
fn apply_saturation(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    // Convert RGB to HSL
    let r_norm = r as f32 / 255.0;
    let g_norm = g as f32 / 255.0;
    let b_norm = b as f32 / 255.0;

    let max = r_norm.max(g_norm).max(b_norm);
    let min = r_norm.min(g_norm).min(b_norm);
    let delta = max - min;

    let l = (max + min) / 2.0;

    if delta < 1e-6 {
        // Achromatic (gray)
        return (r, g, b);
    }

    // Calculate hue
    let h = if (max - r_norm).abs() < 1e-6 {
        ((g_norm - b_norm) / delta) % 6.0
    } else if (max - g_norm).abs() < 1e-6 {
        (b_norm - r_norm) / delta + 2.0
    } else {
        (r_norm - g_norm) / delta + 4.0
    };
    let h = if h < 0.0 { h + 6.0 } else { h };

    // Calculate saturation
    let s = if l < 1e-6 || l > 1.0 - 1e-6 {
        0.0
    } else {
        delta / (1.0 - (2.0 * l - 1.0).abs())
    };

    // Apply saturation multiplier
    let new_s = (s * SATURATION).clamp(0.0, 1.0);

    // Convert HSL back to RGB
    let c = (1.0 - (2.0 * l - 1.0).abs()) * new_s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
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

    (
        ((r1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).clamp(0.0, 255.0) as u8,
    )
}

/// Apply all image adjustments (exposure, saturation, s-curve) to an RGB image
fn apply_adjustments(img: &mut RgbImage) {
    for pixel in img.pixels_mut() {
        // 1. Exposure adjustment
        let r = apply_exposure(pixel[0]);
        let g = apply_exposure(pixel[1]);
        let b = apply_exposure(pixel[2]);

        // 2. Saturation adjustment (HSL-based)
        let (r, g, b) = apply_saturation(r, g, b);

        // 3. S-curve tone mapping (per channel)
        let r = (apply_scurve(r as f32 / 255.0) * 255.0).clamp(0.0, 255.0) as u8;
        let g = (apply_scurve(g as f32 / 255.0) * 255.0).clamp(0.0, 255.0) as u8;
        let b = (apply_scurve(b as f32 / 255.0) * 255.0).clamp(0.0, 255.0) as u8;

        pixel[0] = r;
        pixel[1] = g;
        pixel[2] = b;
    }
}

/// Process a source image for the e-paper display
///
/// Pipeline:
/// 1. Extract dominant color from original image edges
/// 2. Resize to cover width (fill width, center crop any height overflow)
/// 3. Apply adjustments: exposure (0.8), saturation (2.0), s-curve
/// 4. Compose RGB canvas: image + gradient transition + solid color text area
/// 5. Apply Floyd-Steinberg dithering in OKLab space to 6-color palette
/// 6. Render concert info text (black/white based on background lightness)
/// 7. Encode as indexed PNG
pub fn process_image(
    image_data: &[u8],
    target_width: u32,
    target_height: u32,
    concert_info: Option<&ConcertInfo>,
) -> Result<Vec<u8>, AppError> {
    // Decode source image
    let img = image::load_from_memory(image_data)
        .map_err(|e| AppError::ImageProcessing(format!("Failed to decode image: {}", e)))?;

    // 1. Extract dominant color from original image (before resize/adjustments)
    let dominant = extract_dominant_color(&img.to_rgb8());
    tracing::info!(
        "Extracted dominant color: RGB({}, {}, {}), light_bg: {}",
        dominant.r,
        dominant.g,
        dominant.b,
        dominant.is_light
    );

    // Calculate image area (leave room for text)
    let image_area_height = target_height - TEXT_AREA_HEIGHT;

    // 2. Resize to cover image area (fill width, center crop height)
    let mut resized = resize_cover(&img, target_width, image_area_height);

    // 3. Apply image adjustments (exposure, saturation, s-curve)
    apply_adjustments(&mut resized);

    // 4. Compose full RGB canvas with gradient
    let canvas = compose_canvas_with_gradient(
        &resized,
        target_width,
        target_height,
        image_area_height,
        dominant.r,
        dominant.g,
        dominant.b,
    );

    // 5. Apply Floyd-Steinberg dithering to entire canvas
    let mut indexed = floyd_steinberg_dither(&canvas);

    // 6. Render concert info text
    if let Some(info) = concert_info {
        text::render_concert_info_indexed(
            &mut indexed,
            target_width,
            info,
            image_area_height,
            dominant.is_light,
        );
    }

    // 7. Encode as indexed PNG
    encode_indexed_png(&indexed, target_width, target_height)
}

/// Compose the full canvas with image, gradient transition, and solid background
fn compose_canvas_with_gradient(
    img: &RgbImage,
    target_width: u32,
    target_height: u32,
    image_area_height: u32,
    bg_r: u8,
    bg_g: u8,
    bg_b: u8,
) -> RgbImage {
    let mut canvas = RgbImage::new(target_width, target_height);

    // Gradient starts this many pixels above the image/text boundary
    let gradient_start = image_area_height.saturating_sub(GRADIENT_HEIGHT);

    for y in 0..target_height {
        for x in 0..target_width {
            let pixel = if y < gradient_start {
                // Pure image region
                *img.get_pixel(x, y)
            } else if y < image_area_height {
                // Gradient transition zone (blend image into background color)
                let img_pixel = img.get_pixel(x, y);
                let t = (y - gradient_start) as f32 / GRADIENT_HEIGHT as f32;
                // Smooth easing (ease-in-out)
                let t = t * t * (3.0 - 2.0 * t);
                Rgb([
                    lerp_u8(img_pixel[0], bg_r, t),
                    lerp_u8(img_pixel[1], bg_g, t),
                    lerp_u8(img_pixel[2], bg_b, t),
                ])
            } else {
                // Solid background for text area
                Rgb([bg_r, bg_g, bg_b])
            };
            canvas.put_pixel(x, y, pixel);
        }
    }

    canvas
}

/// Linear interpolation between two u8 values
#[inline]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let a = a as f32;
    let b = b as f32;
    (a + (b - a) * t).clamp(0.0, 255.0) as u8
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
/// All operations performed in OKLab color space for perceptual uniformity
fn floyd_steinberg_dither(img: &RgbImage) -> Vec<u8> {
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

            // Floyd-Steinberg error diffusion pattern:
            //       *  7/16
            // 3/16 5/16 1/16

            // Right: 7/16
            if x + 1 < width {
                let right_idx = idx + 1;
                buffer[right_idx].l += err_l * (7.0 / 16.0);
                buffer[right_idx].a += err_a * (7.0 / 16.0);
                buffer[right_idx].b += err_b * (7.0 / 16.0);
            }

            if y + 1 < height {
                // Bottom-left: 3/16
                if x > 0 {
                    let bl_idx = idx + width as usize - 1;
                    buffer[bl_idx].l += err_l * (3.0 / 16.0);
                    buffer[bl_idx].a += err_a * (3.0 / 16.0);
                    buffer[bl_idx].b += err_b * (3.0 / 16.0);
                }

                // Bottom: 5/16
                let bottom_idx = idx + width as usize;
                buffer[bottom_idx].l += err_l * (5.0 / 16.0);
                buffer[bottom_idx].a += err_a * (5.0 / 16.0);
                buffer[bottom_idx].b += err_b * (5.0 / 16.0);

                // Bottom-right: 1/16
                if x + 1 < width {
                    let br_idx = idx + width as usize + 1;
                    buffer[br_idx].l += err_l * (1.0 / 16.0);
                    buffer[br_idx].a += err_a * (1.0 / 16.0);
                    buffer[br_idx].b += err_b * (1.0 / 16.0);
                }
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
