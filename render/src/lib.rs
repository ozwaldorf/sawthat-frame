//! On-device concert artwork renderer for the 6-color e-paper frame.
//!
//! Pipeline per orientation:
//! 1. Resize source to cover the image area (center crop)
//! 2. Apply exposure/saturation/s-curve adjustments
//! 3. Compose image, gradient, and solid background text area
//! 4. Floyd-Steinberg dither to the 6-color palette in OKLab
//! 5. Render concert info text

#![no_std]

extern crate alloc;

pub mod adjust;
pub mod color;
pub mod concerts;
pub mod dither;
pub mod palette;
pub mod resize;
pub mod text;

use ab_glyph::FontRef;
use alloc::vec;
use alloc::vec::Vec;

pub use adjust::Adjuster;
pub use color::{PrimaryColor, primary_color};
pub use palette::OklabPalette;
pub use resize::RgbImage;
pub use text::{Canvas, ConcertInfo};

/// Height reserved for text below the artwork
pub const TEXT_AREA_HEIGHT: u32 = 120;

/// Horizontal (half-width) slot dimensions
pub const HORIZ_SIZE: (u32, u32) = (400, 480);
/// Vertical (full-screen) dimensions
pub const VERT_SIZE: (u32, u32) = (480, 800);

/// Square source size that covers the artwork area of a layout without upscaling
pub const fn cover_size((width, height): (u32, u32)) -> u32 {
    let image_height = height - TEXT_AREA_HEIGHT;
    if width > image_height {
        width
    } else {
        image_height
    }
}

#[derive(Debug)]
pub enum RenderError {
    Jpeg,
}

/// Decode a JPEG to packed RGB8
pub fn decode_jpeg(data: &[u8]) -> Result<RgbImage, RenderError> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(data, options);
    let pixels = decoder.decode().map_err(|_| RenderError::Jpeg)?;
    let (w, h) = decoder.dimensions().ok_or(RenderError::Jpeg)?;
    if pixels.len() != w * h * 3 {
        return Err(RenderError::Jpeg);
    }
    Ok(RgbImage {
        width: w as u32,
        height: h as u32,
        data: pixels,
    })
}

/// Shared state for rendering several images
pub struct Renderer {
    pub adjuster: Adjuster,
    pub palette: OklabPalette,
    pub font: FontRef<'static>,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            adjuster: Adjuster::new(),
            palette: OklabPalette::new(),
            font: text::font(),
        }
    }

    /// Render `src` at `width` x `height` in the stored image format (see [`PackedImage`])
    pub fn render(
        &self,
        src: &RgbImage,
        color: &PrimaryColor,
        info: &ConcertInfo,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let image_height = height - TEXT_AREA_HEIGHT;
        let mut img = resize::resize_cover(src, width, image_height);
        self.adjuster.apply_rgb(&mut img.data);

        let mut out = vec![0u8; IMAGE_HEADER + Canvas::len(width, height)];
        out[0..2].copy_from_slice(&(width as u16).to_le_bytes());
        out[2..4].copy_from_slice(&(height as u16).to_le_bytes());
        let mut canvas = Canvas {
            data: &mut out[IMAGE_HEADER..],
            width,
            height,
        };

        dither::compose_and_dither(&img, color.rgb, &mut canvas, &self.palette);
        drop(img);
        text::render_concert_info(&mut canvas, &self.font, info, image_height, color.is_light);
        out
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Size of the stored image header (width, height as u16 LE)
pub const IMAGE_HEADER: usize = 4;

/// Stored image: 4-byte header (width, height as u16 LE) then 4bpp palette indices
pub struct PackedImage<'a> {
    pub width: u32,
    pub height: u32,
    data: &'a [u8],
}

impl<'a> PackedImage<'a> {
    /// Parse a stored image, validating its length
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        let width = u16::from_le_bytes(bytes.get(0..2)?.try_into().ok()?) as u32;
        let height = u16::from_le_bytes(bytes.get(2..4)?.try_into().ok()?) as u32;
        let data = &bytes[IMAGE_HEADER..];
        (Canvas::len(width, height) == data.len()).then_some(Self {
            width,
            height,
            data,
        })
    }

    /// Palette index at (x, y)
    #[inline]
    pub fn get(&self, x: u32, y: u32) -> u8 {
        let i = (y * self.width + x) as usize;
        let b = self.data[i / 2];
        if i % 2 == 0 { b >> 4 } else { b & 0x0F }
    }
}
