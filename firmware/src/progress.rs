//! Sync progress dialog drawn in the top-left screen corner.
//!
//! The dialog ("Syncing N concerts..." and a progress bar) is drawn once, either as part of a
//! full refresh or as its own partial refresh. Later updates only refresh the newly filled
//! slice of the bar, and [`Progress::restore`] erases the dialog when the sync is done.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use sawthat_render::palette::{BLACK, GREEN, WHITE};
use sawthat_render::text::{Canvas, draw_text, text_width};

use crate::epd::{HEIGHT, Rect, WIDTH};
use crate::framebuffer::Framebuffer;
use crate::widget::Orientation;

/// Box height, inner padding, and distance from the screen corner, in screen coordinates
const BOX_H: u32 = 48;
const PADDING: u32 = 8;
const MARGIN: u32 = 16;
const TEXT_SIZE: f32 = 20.0;

/// Progress bar position within the box (including its 1px border)
const BAR_X: u32 = PADDING;
const BAR_Y: u32 = 32;
const BAR_H: u32 = 8;

/// Panel partial window alignment in framebuffer x
const ALIGN: u32 = 8;

/// Map a box coordinate to framebuffer coordinates.
///
/// Horizontal screens are rotated 180 degrees in the framebuffer; vertical screens
/// (480x800) are rotated 90 degrees.
fn to_framebuffer(orientation: Orientation, x: u32, y: u32) -> (u32, u32) {
    let (x, y) = (MARGIN + x, MARGIN + y);
    match orientation {
        Orientation::Horizontal => (WIDTH - 1 - x, HEIGHT - 1 - y),
        Orientation::Vertical => (y, HEIGHT - 1 - x),
    }
}

/// Framebuffer rect covering a box-coordinate rectangle, aligned for the panel
fn framebuffer_rect(orientation: Orientation, x: u32, y: u32, w: u32, h: u32) -> Rect {
    let (ax, ay) = to_framebuffer(orientation, x, y);
    let (bx, by) = to_framebuffer(orientation, x + w - 1, y + h - 1);
    let x0 = ax.min(bx) / ALIGN * ALIGN;
    let x1 = (ax.max(bx) + 1).div_ceil(ALIGN) * ALIGN;
    let (y0, y1) = (ay.min(by), ay.max(by) + 1);
    Rect::new(
        x0 as u16,
        y0 as u16,
        (x1.min(WIDTH) - x0) as u16,
        (y1 - y0) as u16,
    )
}

/// Progress dialog state
pub struct Progress {
    orientation: Orientation,
    total: usize,
    label: String,
    /// Box width, sized to the label
    box_w: u32,
    /// Framebuffer contents under the box, saved when it is drawn
    saved: Option<(Rect, Vec<u8>)>,
    /// Fill width (px) currently on the panel, `None` until the box is shown
    shown: Option<u32>,
    /// Fill width drawn by the last `draw` or `update`
    drawn: u32,
}

impl Progress {
    pub fn new(orientation: Orientation, total: usize) -> Self {
        let label = format!(
            "Syncing {} concert{}...",
            total,
            if total == 1 { "" } else { "s" }
        );
        let box_w = (text_width(&label, TEXT_SIZE) as u32 + 2 * PADDING).div_ceil(ALIGN) * ALIGN;
        Self {
            orientation,
            total,
            label,
            box_w,
            saved: None,
            shown: None,
            drawn: 0,
        }
    }

    fn bar_w(&self) -> u32 {
        self.box_w - 2 * PADDING
    }

    fn fill(&self, done: usize) -> u32 {
        ((self.bar_w() - 2) as usize * done / self.total.max(1)) as u32
    }

    /// Draw the whole dialog for `done` items, saving the framebuffer contents under it.
    ///
    /// Returns the region for a partial refresh; a following full refresh also shows it.
    /// Call [`Self::confirm`] once the refresh has started.
    pub fn draw(&mut self, framebuffer: &mut Framebuffer, done: usize) -> (Rect, Vec<u8>) {
        let rect = framebuffer_rect(self.orientation, 0, 0, self.box_w, BOX_H);
        self.saved = Some((rect, framebuffer.extract_rect(&rect)));
        self.drawn = self.fill(done);
        self.draw_box(framebuffer);
        (rect, framebuffer.extract_rect(&rect))
    }

    /// Draw the bar for `done` items, returning the newly filled slice to refresh, or
    /// `None` if the dialog is not shown or nothing visible changed.
    pub fn update(
        &mut self,
        framebuffer: &mut Framebuffer,
        done: usize,
    ) -> Option<(Rect, Vec<u8>)> {
        let shown = self.shown?;
        let fill = self.fill(done);
        if fill <= shown {
            return None;
        }

        let (x, y) = (BAR_X + 1 + shown, BAR_Y + 1);
        for by in y..y + BAR_H - 2 {
            for bx in x..BAR_X + 1 + fill {
                let (fx, fy) = to_framebuffer(self.orientation, bx, by);
                framebuffer.set_pixel_indexed(fx, fy, GREEN);
            }
        }
        self.drawn = fill;
        let rect = framebuffer_rect(self.orientation, x, y, fill - shown, BAR_H - 2);
        Some((rect, framebuffer.extract_rect(&rect)))
    }

    /// Record that the last `draw` or `update` is on the panel
    pub fn confirm(&mut self) {
        self.shown = Some(self.drawn);
    }

    /// Restore the contents under the dialog, returning the region to refresh if it was shown
    pub fn restore(&mut self, framebuffer: &mut Framebuffer) -> Option<(Rect, Vec<u8>)> {
        self.shown?;
        let (rect, data) = self.saved.take()?;
        framebuffer.write_rect(&rect, &data);
        self.shown = None;
        Some((rect, data))
    }

    fn draw_box(&self, framebuffer: &mut Framebuffer) {
        let (box_w, bar_w, fill) = (self.box_w, self.bar_w(), self.drawn);
        let mut pixels = vec![WHITE; (box_w * BOX_H) as usize];
        let mut set = |x: u32, y: u32, c: u8| pixels[(y * box_w + x) as usize] = c;

        // 2px box border
        for x in 0..box_w {
            for y in [0, 1, BOX_H - 2, BOX_H - 1] {
                set(x, y, BLACK);
            }
        }
        for y in 0..BOX_H {
            for x in [0, 1, box_w - 2, box_w - 1] {
                set(x, y, BLACK);
            }
        }

        // Bar outline and fill
        for y in BAR_Y..BAR_Y + BAR_H {
            for x in BAR_X..BAR_X + bar_w {
                let edge =
                    y == BAR_Y || y == BAR_Y + BAR_H - 1 || x == BAR_X || x == BAR_X + bar_w - 1;
                let color = if edge {
                    BLACK
                } else if x - BAR_X - 1 < fill {
                    GREEN
                } else {
                    WHITE
                };
                set(x, y, color);
            }
        }

        draw_text(
            &mut Canvas {
                pixels: &mut pixels,
                width: box_w,
                height: BOX_H,
            },
            &self.label,
            TEXT_SIZE,
            PADDING as f32,
            5.0,
            BLACK,
        );

        for y in 0..BOX_H {
            for x in 0..box_w {
                let (fx, fy) = to_framebuffer(self.orientation, x, y);
                framebuffer.set_pixel_indexed(fx, fy, pixels[(y * box_w + x) as usize]);
            }
        }
    }
}
