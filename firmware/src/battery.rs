//! Battery indicator for e-paper display
//!
//! Draws a battery icon with fill level and color based on percentage, and a power symbol.

use sawthat_render::OklabPalette;
use sawthat_render::dither::floyd_steinberg;

use crate::epd::{Color, WIDTH};
use crate::framebuffer::remap_color;

/// Battery icon dimensions (horizontal mode)
pub const BATTERY_WIDTH_H: u16 = 48;
pub const BATTERY_HEIGHT_H: u16 = 24;

/// Battery icon dimensions (vertical mode - rotated 90° clockwise)
pub const BATTERY_WIDTH_V: u16 = 24;
pub const BATTERY_HEIGHT_V: u16 = 48;

/// Buffer size for battery icon region (4bpp, 2 pixels per byte)
/// Same for both orientations since total pixels is the same
pub const BATTERY_BUFFER_SIZE: usize = (BATTERY_WIDTH_H as usize * BATTERY_HEIGHT_H as usize) / 2;

/// Get battery dimensions for given orientation
pub fn battery_dimensions(vertical: bool) -> (u16, u16) {
    if vertical {
        (BATTERY_WIDTH_V, BATTERY_HEIGHT_V)
    } else {
        (BATTERY_WIDTH_H, BATTERY_HEIGHT_H)
    }
}

/// Mid-level fill colors (sRGB), dithered to the panel palette like the artwork: a darker
/// orange for the lower half of the range
const ORANGE_LOW: [u8; 3] = [170, 60, 0];
const ORANGE_HIGH: [u8; 3] = [190, 70, 0];

/// Orange fill color for a mid-level battery percentage
fn orange(percentage: u8) -> [u8; 3] {
    if percentage < 33 {
        ORANGE_LOW
    } else {
        ORANGE_HIGH
    }
}

/// Draw the fill rectangle at (x0, y0) for a battery percentage.
///
/// Low and high levels are solid red and green; mid levels Floyd-Steinberg dither
/// `orange`, normally [`orange`] for the percentage.
fn draw_fill(
    percentage: u8,
    orange: [u8; 3],
    (x0, y0, w, h): (u16, u16, u16, u16),
    mut set: impl FnMut(u16, u16, Color),
) {
    let solid = match percentage {
        0..=15 => Color::Red,
        50.. => Color::Green,
        _ => {
            let palette = OklabPalette::new();
            let color = palette.to_oklab(orange[0], orange[1], orange[2]);
            floyd_steinberg(
                w.into(),
                h.into(),
                &palette,
                |_, _| color,
                |x, y, idx| {
                    set(
                        x0 + x as u16,
                        y0 + y as u16,
                        Color::from_4bit(remap_color(idx)),
                    )
                },
            );
            return;
        }
    };
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            set(x, y, solid);
        }
    }
}

/// Symbol drawn over the battery fill
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerSymbol {
    None,
    /// Charging from a charger
    Bolt,
    /// Connected to a computer
    Usb,
    /// Plugged in and fully charged
    Full,
}

/// Symbols in screen orientation, '#' pixels drawn black with a white outline. Rasterized at
/// 16px from the Phosphor `lightning` (fill), `usb` (bold) and `check` (bold) icons (MIT,
/// phosphoricons.com); the bolt is turned 90 degrees clockwise to lie along the battery.
const BOLT: [&str; 10] = [
    "......##......",
    ".....####.....",
    ".....#####....",
    "###########...",
    ".###########..",
    "..###########.",
    "...###########",
    "....#####.....",
    ".....####.....",
    "......##......",
];
const USB: [&str; 12] = [
    ".......###......",
    "....####.#......",
    "...#####.#......",
    "...##..###..#...",
    "...##......####.",
    "################",
    "################",
    "...##......####.",
    "...##.####..#...",
    "...#####.##.....",
    "....####.##.....",
    "......####......",
];

const CHECK: [&str; 9] = [
    "...........##",
    "..........##.",
    ".........##..",
    "........##...",
    "#......##....",
    "##....##.....",
    ".##..##......",
    "..####.......",
    "...##........",
];

/// Draw a symbol bitmap centered on the battery body.
///
/// The bitmap is in screen orientation: horizontal screens are rotated 180 degrees in the
/// framebuffer and vertical screens 90 degrees, so the symbol is mapped to match.
fn draw_symbol<F>(fb: &mut [u8], set_pixel: &F, vertical: bool, bitmap: &[&str])
where
    F: Fn(&mut [u8], u16, u16, Color),
{
    let (w, h) = (bitmap[0].len() as i32, bitmap.len() as i32);
    let lit = |x: i32, y: i32| {
        (0..w).contains(&x)
            && (0..h).contains(&y)
            && bitmap[y as usize].as_bytes()[x as usize] == b'#'
    };
    // Body center in icon coordinates
    let (cx, cy) = if vertical {
        (BATTERY_WIDTH_V as i32 / 2, 6 + 42 / 2)
    } else {
        (42 / 2, BATTERY_HEIGHT_H as i32 / 2)
    };
    // Map a bitmap pixel to icon coordinates
    let to_icon = |x: i32, y: i32| -> (u16, u16) {
        let (dx, dy) = if vertical {
            (y - h / 2, (w - 1 - x) - w / 2)
        } else {
            ((w - 1 - x) - w / 2, (h - 1 - y) - h / 2)
        };
        ((cx + dx) as u16, (cy + dy) as u16)
    };

    for y in -1..=h {
        for x in -1..=w {
            let color = if lit(x, y) {
                Color::Black
            } else if (-1..=1).any(|dy| (-1..=1).any(|dx| lit(x + dx, y + dy))) {
                Color::White
            } else {
                continue;
            };
            let (ix, iy) = to_icon(x, y);
            set_pixel(fb, ix, iy, color);
        }
    }
}

/// Draw battery icon directly into framebuffer
///
/// - `framebuffer`: The main display framebuffer to draw into
/// - `fb_x`, `fb_y`: Position in framebuffer where icon will be drawn
/// - `percentage`: Battery level 0-100
/// - `vertical`: If true, draw vertical battery (tip on top), else horizontal (tip on right)
/// - `symbol`: Power symbol drawn over the fill
pub fn draw_battery(
    framebuffer: &mut [u8],
    fb_x: u16,
    fb_y: u16,
    percentage: u8,
    vertical: bool,
    symbol: PowerSymbol,
) {
    let orange = orange(percentage);
    let (buf_width, buf_height) = battery_dimensions(vertical);

    // Helper to set a pixel in the framebuffer
    let set_pixel = |fb: &mut [u8], x: u16, y: u16, color: Color| {
        let px = fb_x + x;
        let py = fb_y + y;
        if px >= WIDTH as u16 || py >= crate::epd::HEIGHT as u16 {
            return;
        }
        let byte_idx = (py as usize * (WIDTH as usize / 2)) + (px as usize / 2);
        let is_high_nibble = px.is_multiple_of(2);
        if byte_idx < fb.len() {
            if is_high_nibble {
                fb[byte_idx] = (fb[byte_idx] & 0x0F) | (color.to_4bit() << 4);
            } else {
                fb[byte_idx] = (fb[byte_idx] & 0xF0) | color.to_4bit();
            }
        }
    };

    if vertical {
        draw_battery_vertical(
            framebuffer,
            &set_pixel,
            buf_width,
            buf_height,
            percentage,
            orange,
        );
    } else {
        draw_battery_horizontal(
            framebuffer,
            &set_pixel,
            buf_width,
            buf_height,
            percentage,
            orange,
        );
    }

    match symbol {
        PowerSymbol::None => {}
        PowerSymbol::Bolt => draw_symbol(framebuffer, &set_pixel, vertical, &BOLT),
        PowerSymbol::Usb => draw_symbol(framebuffer, &set_pixel, vertical, &USB),
        PowerSymbol::Full => draw_symbol(framebuffer, &set_pixel, vertical, &CHECK),
    }
}

fn draw_battery_vertical<F>(
    fb: &mut [u8],
    set_pixel: &F,
    _buf_width: u16,
    _buf_height: u16,
    percentage: u8,
    orange: [u8; 3],
) where
    F: Fn(&mut [u8], u16, u16, Color),
{
    // Vertical battery: tip on top, fill goes bottom to top
    let body_width: u16 = BATTERY_WIDTH_V;
    let body_height: u16 = 42;
    let body_y_start: u16 = 6; // Leave room for tip at top

    // Draw main body outline and white interior
    for x in 0..body_width {
        for y in body_y_start..(body_y_start + body_height) {
            let is_border = y < body_y_start + 2
                || y >= body_y_start + body_height - 2
                || x < 2
                || x >= body_width - 2;
            set_pixel(
                fb,
                x,
                y,
                if is_border {
                    Color::Black
                } else {
                    Color::White
                },
            );
        }
    }

    // Draw tip on top
    let tip_width: u16 = 12;
    let tip_height: u16 = 6;
    let tip_x_start = (body_width - tip_width) / 2;

    for x in tip_x_start..(tip_x_start + tip_width) {
        for y in 0..tip_height {
            let is_border = x < tip_x_start + 2 || x >= tip_x_start + tip_width - 2 || y < 2;
            set_pixel(
                fb,
                x,
                y,
                if is_border {
                    Color::Black
                } else {
                    Color::White
                },
            );
        }
    }

    // Fill area (fills from bottom to top)
    let fill_x_start: u16 = 4;
    let fill_width: u16 = body_width - 8;
    let fill_max_height: u16 = body_height - 8;
    let fill_y_end: u16 = body_y_start + body_height - 4;
    let fill_height = ((fill_max_height as u32 * percentage.min(100) as u32) / 100) as u16;
    let fill_y_start = fill_y_end - fill_height;

    draw_fill(
        percentage,
        orange,
        (fill_x_start, fill_y_start, fill_width, fill_height),
        |x, y, c| set_pixel(fb, x, y, c),
    );
}

fn draw_battery_horizontal<F>(
    fb: &mut [u8],
    set_pixel: &F,
    _buf_width: u16,
    _buf_height: u16,
    percentage: u8,
    orange: [u8; 3],
) where
    F: Fn(&mut [u8], u16, u16, Color),
{
    // Horizontal battery: tip on right, fill goes left to right
    let body_width: u16 = 42;
    let body_height: u16 = BATTERY_HEIGHT_H;

    // Draw main body outline and white interior
    for x in 0..body_width {
        for y in 0..body_height {
            let is_border = y < 2 || y >= body_height - 2 || x < 2 || x >= body_width - 2;
            set_pixel(
                fb,
                x,
                y,
                if is_border {
                    Color::Black
                } else {
                    Color::White
                },
            );
        }
    }

    // Draw tip on right
    let tip_x = body_width;
    let tip_width: u16 = 6;
    let tip_height: u16 = 12;
    let tip_y_start = (body_height - tip_height) / 2;

    for x in tip_x..(tip_x + tip_width) {
        for y in tip_y_start..(tip_y_start + tip_height) {
            let is_border = y < tip_y_start + 2
                || y >= tip_y_start + tip_height - 2
                || x >= tip_x + tip_width - 2;
            set_pixel(
                fb,
                x,
                y,
                if is_border {
                    Color::Black
                } else {
                    Color::White
                },
            );
        }
    }

    // Fill area (fills from left to right)
    let fill_x_start: u16 = 4;
    let fill_y_start: u16 = 4;
    let fill_max_width: u16 = body_width - 8;
    let fill_height: u16 = body_height - 8;
    let fill_width = ((fill_max_width as u32 * percentage.min(100) as u32) / 100) as u16;

    draw_fill(
        percentage,
        orange,
        (fill_x_start, fill_y_start, fill_width, fill_height),
        |x, y, c| set_pixel(fb, x, y, c),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(percentage: u8) -> [usize; 8] {
        let mut counts = [0; 8];
        draw_fill(percentage, orange(percentage), (0, 0, 30, 30), |_, _, c| {
            counts[c as usize] += 1
        });
        counts
    }

    #[test]
    fn test_fill_color() {
        assert_eq!(fill(15)[Color::Red as usize], 900);
        assert_eq!(fill(50)[Color::Green as usize], 900);

        // Orange is mostly red with some yellow
        let orange = fill(30);
        assert!(orange[Color::Red as usize] > orange[Color::Yellow as usize]);
        assert!(orange[Color::Yellow as usize] > 100);
    }
}
