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

/// Draw the fill rectangle at (x0, y0) for a battery percentage, with rounded corners.
///
/// Low and high levels are solid red and green; mid levels Floyd-Steinberg dither
/// `orange`, normally [`orange`] for the percentage.
fn draw_fill(
    percentage: u8,
    orange: [u8; 3],
    (x0, y0, w, h): (u16, u16, u16, u16),
    mut set: impl FnMut(u16, u16, Color),
) {
    // Round the fill's corners; pixels outside keep the white body interior
    let rect = (x0 as i32, y0 as i32, w as i32, h as i32);
    let mut set = |x: u16, y: u16, color: Color| {
        if in_rounded(x as i32, y as i32, rect, FILL_RADIUS) {
            set(x, y, color);
        }
    };
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
    // Body center in icon coordinates; the horizontal body is mirrored (see `draw_battery`)
    let (cx, cy) = if vertical {
        (BATTERY_WIDTH_V as i32 / 2, 6 + 42 / 2)
    } else {
        (
            BATTERY_WIDTH_H as i32 - 1 - 42 / 2,
            BATTERY_HEIGHT_H as i32 / 2,
        )
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
/// - `vertical`: If true, draw vertical battery (tip on top), else horizontal (tip on the
///   right of the screen)
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
        // Mirrored in the framebuffer so the 180 degree rotated screen shows the tip on the
        // right and the fill growing from the left
        let mirrored = |fb: &mut [u8], x: u16, y: u16, color: Color| {
            set_pixel(fb, BATTERY_WIDTH_H - 1 - x, y, color)
        };
        draw_battery_horizontal(
            framebuffer,
            &mirrored,
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

/// Outline thickness, outer corner radius of the body, radius of the tip's free corners, and
/// corner radius of the fill
const BORDER: i32 = 2;
const BODY_RADIUS: i32 = 4;
const TIP_RADIUS: i32 = 2;
const FILL_RADIUS: i32 = 2;

/// Whether pixel (x, y) is inside the rect (x0, y0, w, h) with corners rounded to `r`
fn in_rounded(x: i32, y: i32, (x0, y0, w, h): (i32, i32, i32, i32), r: i32) -> bool {
    let (x, y) = (x - x0, y - y0);
    if x < 0 || y < 0 || x >= w || y >= h {
        return false;
    }
    let r = r.min(w / 2).min(h / 2);
    let dx = if x < r {
        r - 1 - x
    } else if x >= w - r {
        x - (w - r)
    } else {
        return true;
    };
    let dy = if y < r {
        r - 1 - y
    } else if y >= h - r {
        y - (h - r)
    } else {
        return true;
    };
    // Pixel center within the corner circle
    (2 * dx + 1).pow(2) + (2 * dy + 1).pow(2) <= 4 * r * r
}

/// Draw a rounded rect `outer` with a `BORDER` thick black outline and white inside, limited to
/// pixels where `clip` holds
fn draw_outline<F>(
    fb: &mut [u8],
    set_pixel: &F,
    outer: (i32, i32, i32, i32),
    r: i32,
    clip: impl Fn(i32, i32) -> bool,
) where
    F: Fn(&mut [u8], u16, u16, Color),
{
    let (x0, y0, w, h) = outer;
    let inner = (x0 + BORDER, y0 + BORDER, w - 2 * BORDER, h - 2 * BORDER);
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            if !clip(x, y) || !in_rounded(x, y, outer, r) {
                continue;
            }
            let color = if in_rounded(x, y, inner, (r - BORDER).max(0)) {
                Color::White
            } else {
                Color::Black
            };
            set_pixel(fb, x as u16, y as u16, color);
        }
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

    let body = (
        0,
        body_y_start as i32,
        body_width as i32,
        body_height as i32,
    );
    draw_outline(fb, set_pixel, body, BODY_RADIUS, |_, _| true);

    // Tip on top: the outline extends into the body so only its free corners are rounded
    let (tip_width, tip_height) = (12, body_y_start as i32);
    let tip_x = (body_width as i32 - tip_width) / 2;
    let tip = (tip_x, 0, tip_width, tip_height + 2 * TIP_RADIUS);
    draw_outline(fb, set_pixel, tip, TIP_RADIUS, |_, y| y < tip_height);

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
    // Horizontal battery before mirroring: tip on right, fill goes left to right
    let body_width: u16 = 42;
    let body_height: u16 = BATTERY_HEIGHT_H;

    let body = (0, 0, body_width as i32, body_height as i32);
    draw_outline(fb, set_pixel, body, BODY_RADIUS, |_, _| true);

    // Tip on right: the outline extends into the body so only its free corners are rounded
    let (tip_width, tip_height) = (6, 12);
    let tip_x = body_width as i32;
    let tip_y = (body_height as i32 - tip_height) / 2;
    let tip = (
        tip_x - 2 * TIP_RADIUS,
        tip_y,
        tip_width + 2 * TIP_RADIUS,
        tip_height,
    );
    draw_outline(fb, set_pixel, tip, TIP_RADIUS, |x, _| x >= tip_x);

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
