//! Framebuffer management for 7.3" e-paper display
//!
//! The display is 800x480 pixels with 4 bits per pixel (6 colors).
//! Two pixels are packed per byte: high nibble = left pixel, low nibble = right pixel.
//!
//! The framebuffer is allocated dynamically from PSRAM to avoid exhausting internal SRAM.

use crate::epd::{BUFFER_SIZE, Color, HEIGHT, WIDTH};
use alloc::boxed::Box;

extern crate alloc;

/// Color index remapping table: PNG palette index -> EPD 4-bit value
/// PNG: 0=Black, 1=White, 2=Red, 3=Yellow, 4=Blue, 5=Green
/// EPD: 0=Black, 1=White, 2=Yellow, 3=Red, 5=Blue, 6=Green
const COLOR_REMAP: [u8; 6] = [0x00, 0x01, 0x03, 0x02, 0x05, 0x06];

/// Remap a PNG palette index to EPD color value
#[inline]
fn remap_color(palette_idx: u8) -> u8 {
    if palette_idx < 6 {
        COLOR_REMAP[palette_idx as usize]
    } else {
        0x01 // Default to white for invalid indices
    }
}

/// Framebuffer for the 800x480 4-bit display
/// Uses heap allocation to avoid static memory exhaustion
pub struct Framebuffer {
    buffer: Box<[u8; BUFFER_SIZE]>,
}

impl Framebuffer {
    /// Create a new framebuffer initialized to white
    /// Allocates from heap (should be called after PSRAM heap is initialized)
    pub fn new() -> Self {
        let mut buffer = Box::new([0u8; BUFFER_SIZE]);
        buffer.fill(Color::White.to_dual_pixel());
        Self { buffer }
    }

    /// Clear the entire framebuffer to a single color
    pub fn clear(&mut self, color: Color) {
        let byte = color.to_dual_pixel();
        self.buffer.fill(byte);
    }

    /// Get the raw buffer slice for sending to the display
    pub fn as_slice(&self) -> &[u8] {
        &self.buffer[..]
    }

    /// Get mutable access to the raw buffer
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buffer[..]
    }

    /// Write a single pixel at (x, y) with the given color
    #[inline]
    pub fn set_pixel(&mut self, x: u32, y: u32, color: Color) {
        if x >= WIDTH || y >= HEIGHT {
            return;
        }

        let byte_idx = (y as usize * (WIDTH as usize / 2)) + (x as usize / 2);
        let is_high_nibble = (x % 2) == 0;

        if is_high_nibble {
            self.buffer[byte_idx] = (self.buffer[byte_idx] & 0x0F) | (color.to_4bit() << 4);
        } else {
            self.buffer[byte_idx] = (self.buffer[byte_idx] & 0xF0) | color.to_4bit();
        }
    }

    /// Write a row of pixels from PNG palette indices
    ///
    /// - `x_offset`: Starting x position (0 for left half, 400 for right half)
    /// - `y`: Row index
    /// - `pixels`: Slice of PNG palette indices (0-5)
    pub fn write_row(&mut self, x_offset: u32, y: u32, pixels: &[u8]) {
        if y >= HEIGHT {
            return;
        }

        let row_start = y as usize * (WIDTH as usize / 2);
        let byte_offset = x_offset as usize / 2;

        // Process pixels in pairs
        let mut i = 0;
        while i + 1 < pixels.len() {
            let p1 = remap_color(pixels[i]);
            let p2 = remap_color(pixels[i + 1]);
            let byte = (p1 << 4) | p2;

            let idx = row_start + byte_offset + (i / 2);
            if idx < self.buffer.len() {
                self.buffer[idx] = byte;
            }

            i += 2;
        }

        // Handle odd pixel at end
        if i < pixels.len() {
            let p1 = remap_color(pixels[i]);
            let idx = row_start + byte_offset + (i / 2);
            if idx < self.buffer.len() {
                // Set high nibble, preserve low nibble
                self.buffer[idx] = (self.buffer[idx] & 0x0F) | (p1 << 4);
            }
        }
    }

    /// Fill a rectangular region with a color
    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        for row in y..(y + height).min(HEIGHT) {
            for col in x..(x + width).min(WIDTH) {
                self.set_pixel(col, row, color);
            }
        }
    }

    /// Fill the left half of the display (400x480) with a color
    pub fn fill_left_half(&mut self, color: Color) {
        self.fill_rect(0, 0, 400, HEIGHT, color);
    }

    /// Fill the right half of the display (400x480) with a color
    pub fn fill_right_half(&mut self, color: Color) {
        self.fill_rect(400, 0, 400, HEIGHT, color);
    }
}

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}
