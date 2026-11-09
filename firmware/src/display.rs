//! Rendering stored artwork into the framebuffer and item ordering.

use log::info;
use sawthat_render::{HORIZ_SIZE, PackedImage, VERT_SIZE};

use crate::framebuffer::Framebuffer;
use crate::widget::{Orientation, WidgetData};

/// Display error types
#[derive(Debug)]
pub enum DisplayError {
    /// Stored image is missing or malformed
    Image,
    /// Panel communication failed
    Panel,
}

/// Draw a stored image into the framebuffer.
///
/// Horizontal images (400x480) are rotated 180 degrees into slot 0 (x 0-399) or
/// slot 1 (x 400-799). Vertical images (480x800) are rotated 90 degrees to fill the screen.
pub fn render_stored(
    data: &[u8],
    framebuffer: &mut Framebuffer,
    slot: u8,
    orientation: Orientation,
) -> Result<(), DisplayError> {
    let img = PackedImage::parse(data).ok_or(DisplayError::Image)?;
    let expected = match orientation {
        Orientation::Horizontal => HORIZ_SIZE,
        Orientation::Vertical => VERT_SIZE,
    };
    if (img.width, img.height) != expected {
        info!(
            "Stored image is {}x{}, expected {}x{}",
            img.width, img.height, expected.0, expected.1
        );
        return Err(DisplayError::Image);
    }

    let (w, h) = (img.width, img.height);
    match orientation {
        Orientation::Horizontal => {
            let x_offset = if slot == 0 { 0 } else { 400 };
            let mut row = [0u8; 400];
            for y in 0..h {
                for x in 0..w {
                    row[(w - 1 - x) as usize] = img.get(x, y);
                }
                framebuffer.write_row(x_offset, h - 1 - y, &row[..w as usize]);
            }
        }
        Orientation::Vertical => {
            for y in 0..h {
                for x in 0..w {
                    framebuffer.set_pixel_indexed(y, w - 1 - x, img.get(x, y));
                }
            }
        }
    }
    Ok(())
}

/// Shuffle widget items in-place using a simple xorshift RNG
pub fn shuffle_items(items: &mut WidgetData, seed: u64) {
    let len = items.len();
    if len <= 1 {
        return;
    }

    let mut state = if seed == 0 { 0x853c49e6748fea9b } else { seed };

    // Fisher-Yates shuffle
    for i in (1..len).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;

        let j = (state as usize) % (i + 1);
        items.swap(i, j);
    }

    info!("Shuffled {} items", len);
}
