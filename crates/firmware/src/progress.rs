//! Sync progress dialogs: a small box in the top-left screen corner, or a larger centered
//! welcome box with usage instructions for a first sync, and a setup box for a missing or
//! invalid configuration.
//!
//! The dialog ("Syncing N concerts..." and a progress bar) is drawn once, either as part of a
//! full refresh or as its own partial refresh. Later updates only refresh the newly filled
//! slice of the bar, and [`Progress::restore`] erases the dialog when the sync is done.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use qrcodegen_no_heap::{QrCode, QrCodeEcc, Version};
use sawthat_render::palette::{BLACK, BLUE, GREEN, RED, WHITE};
use sawthat_render::text::{Canvas, draw_text, font, text_width};

use crate::epd::{HEIGHT, Rect, WIDTH};
use crate::framebuffer::Framebuffer;
use crate::orientation::Orientation;

/// Inner padding and corner box distance from the screen corner, in screen coordinates
const PADDING: u32 = 8;
const MARGIN: u32 = 16;
const TEXT_SIZE: f32 = 20.0;

/// Progress bar height (including its 1px border) and gap below the label
const BAR_H: u32 = 8;
const BAR_GAP: u32 = 4;

/// Welcome box: title size, instruction size, and line spacing
const TITLE_SIZE: f32 = 28.0;
const NOTE_SIZE: f32 = 18.0;
const LINE_GAP: u32 = 6;
const SECTION_GAP: u32 = 18;

/// Usage instructions on the welcome box, around the refresh interval line
const INSTRUCTIONS: [&str; 2] = [
    "Tap KEY for the next concert",
    "Hold KEY to rotate the screen",
];
const RESYNC_NOTE: &str = "resyncs after every concert is shown";

/// Setup box contents after the error line
const SETUP_LINES: [&str; 10] = [
    "Set these in CONFIG.TXT on the SD card:",
    "",
    "WIFI_SSID=your network",
    "WIFI_PASS=your password",
    "SAWTHAT_USER_ID=your user id",
    "",
    "Connect USB to a computer to edit the",
    "card as a drive, then eject it.",
    "Or edit the file on the SD card, then",
    "press KEY to retry.",
];

/// Drive mode box contents after the title
const DRIVE_LINES: [&str; 3] = [
    "The SD card is shown as a USB drive.",
    "Eject it or press BOOT to finish,",
    "then the frame restarts and syncs.",
];

/// Panel partial window alignment in framebuffer x
const ALIGN: u32 = 8;

/// WiFi icon size and gap before the label, in box coordinates
const ICON_W: u32 = 24;
const ICON_H: u32 = 16;
const ICON_GAP: u32 = 6;
/// Icon top for a label at the top padding, roughly centered on its cap height
const ICON_Y: u32 = 6;

/// Signal bars (0-3) for a WiFi RSSI in dBm
fn signal_bars(rssi: i32) -> u32 {
    match rssi {
        -60.. => 3,
        -70.. => 2,
        -80.. => 1,
        _ => 0,
    }
}

/// Draw a WiFi icon with `bars` lit arcs at (x, y); unlit arcs are a black/white
/// checkerboard so they read as grey
fn draw_wifi_icon(canvas: &mut Canvas, x: u32, y: u32, bars: u32) {
    // Arcs fan out from a point at the bottom center, within 45 degrees of vertical
    let (cx, cy) = (x as i32 + ICON_W as i32 / 2, y as i32 + ICON_H as i32 - 1);
    // Squared radius bands: dot, then three arcs
    const BANDS: [(i32, i32); 4] = [(0, 6), (25, 49), (81, 121), (169, 225)];

    for py in y..y + ICON_H {
        for px in x..x + ICON_W {
            let (dx, dy) = (px as i32 - cx, cy - py as i32);
            if dy < 0 || (dx.abs() > dy && dx * dx + dy * dy > BANDS[0].1) {
                continue;
            }
            let r2 = dx * dx + dy * dy;
            let Some(band) = BANDS.iter().position(|&(lo, hi)| r2 >= lo && r2 < hi) else {
                continue;
            };
            let lit = band == 0 || band as u32 <= bars;
            let color = if lit || (px + py).is_multiple_of(2) {
                BLACK
            } else {
                WHITE
            };
            canvas.set(px, py, color);
        }
    }
}

/// Screen size (width, height) for an orientation
fn screen_size(orientation: Orientation) -> (u32, u32) {
    match orientation {
        Orientation::Horizontal => (WIDTH, HEIGHT),
        Orientation::Vertical => (HEIGHT, WIDTH),
    }
}

/// Map a screen coordinate to framebuffer coordinates.
///
/// Horizontal screens are rotated 180 degrees in the framebuffer; vertical screens
/// (480x800) are rotated 90 degrees.
fn to_framebuffer(orientation: Orientation, x: u32, y: u32) -> (u32, u32) {
    match orientation {
        Orientation::Horizontal => (WIDTH - 1 - x, HEIGHT - 1 - y),
        Orientation::Vertical => (y, HEIGHT - 1 - x),
    }
}

/// Framebuffer rect covering a screen-coordinate rectangle, aligned for the panel
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

/// 2px black border around a canvas
fn draw_border(canvas: &mut Canvas) {
    let (w, h) = (canvas.width, canvas.height);
    for x in 0..w {
        for y in [0, 1, h - 2, h - 1] {
            canvas.set(x, y, BLACK);
        }
    }
    for y in 0..h {
        for x in [0, 1, w - 2, w - 1] {
            canvas.set(x, y, BLACK);
        }
    }
}

/// Draw a centered setup box explaining `error` and the expected config file
pub fn draw_setup(framebuffer: &mut Framebuffer, orientation: Orientation, error: &str) {
    draw_notice(
        framebuffer,
        orientation,
        "SawThat Frame Setup",
        Some(error),
        &SETUP_LINES,
    );
}

/// Draw a centered box saying the SD card is shown as a USB drive, returning its
/// framebuffer region for a partial refresh
pub fn draw_drive_mode(framebuffer: &mut Framebuffer, orientation: Orientation) -> Rect {
    draw_notice(
        framebuffer,
        orientation,
        "USB Drive Mode",
        None,
        &DRIVE_LINES,
    )
}

/// Crash screen margin, text sizes, and gap between the text and the QR code
const CRASH_MARGIN: u32 = 32;
const CRASH_FACE_SIZE: f32 = 96.0;
const CRASH_TEXT_SIZE: f32 = 18.0;
const CRASH_TRACE_SIZE: f32 = 14.0;
const CRASH_RESTART_SIZE: f32 = 22.0;
/// Crash screen title, and the note by the QR code
const CRASH_TITLE: &str = "Uh oh! We've encountered an error";
const CRASH_SCAN_NOTE: &str = "Scan to submit the crash report";
/// Gap between the QR code and its notes in portrait
const CRASH_NOTE_GAP: u32 = 16;
/// Baseline of a line drawn with `draw_text`, as a fraction of its size below its top
const BASELINE: f32 = 0.8;
const CRASH_QR_GAP: u32 = 24;
/// Most space for the QR code on the crash screen, quiet zone included
const CRASH_QR_SIDE: u32 = 200;
/// Quiet zone around a QR code, in modules
const QR_BORDER: u32 = 4;
/// Smallest module size, in pixels, that still scans from the panel
const QR_MIN_SCALE: u32 = 2;

/// Fill the screen with a blue crash report: the panic `message` (lines separated by `\n`), the
/// `trace` lines, and a QR code linking to the first of `urls` that fits, above the firmware
/// `version`. Lines that do not fit are dropped; the link holds the report.
pub fn draw_crash(
    framebuffer: &mut Framebuffer,
    orientation: Orientation,
    message: &str,
    version: &str,
    trace: &[String],
    urls: &[String],
) {
    let font = font();
    let (sw, sh) = screen_size(orientation);
    let mut data = vec![(BLUE << 4) | BLUE; Canvas::len(sw, sh)];
    let mut canvas = Canvas {
        data: &mut data,
        width: sw,
        height: sh,
    };
    let firmware = format!("Firmware {version}");
    let restart = "Press KEY to restart";
    let width = |text: &str, size| text_width(&font, text, size) as u32;
    // The font is monospaced, so lines wrap at a fixed character count
    let chars = |width: u32, size| (width as f32 / text_width(&font, "M", size)) as usize;
    let right = sw - CRASH_MARGIN;

    // Landscape puts the QR code and its notes in a column right of the text, with the restart
    // hint in the bottom right corner. Portrait puts the text across the top, the QR code in the
    // bottom left corner, and the notes right-aligned in the bottom right corner beside it.
    let qr = fit_qr(urls, CRASH_QR_SIDE);
    let qr_side = qr.map_or(0, |(_, side)| side);
    let (text_w, text_bottom) = match orientation {
        Orientation::Horizontal => {
            let notes = [CRASH_SCAN_NOTE, firmware.as_str()];
            let col_w = notes
                .iter()
                .map(|note| width(note, CRASH_TRACE_SIZE))
                .chain([qr_side, width(restart, CRASH_RESTART_SIZE)])
                .max()
                .unwrap_or(qr_side);
            if let Some((url, side)) = qr {
                draw_qr(Some(&mut canvas), url, right - side, CRASH_MARGIN, side);
            }
            let mut y = CRASH_MARGIN + qr_side + LINE_GAP;
            for note in notes {
                let x = right - width(note, CRASH_TRACE_SIZE);
                draw_text(
                    &mut canvas,
                    &font,
                    note,
                    CRASH_TRACE_SIZE,
                    (x as f32, y as f32),
                    WHITE,
                );
                y += line_h(CRASH_TRACE_SIZE);
            }
            let (x, y) = (
                right - width(restart, CRASH_RESTART_SIZE),
                sh - CRASH_MARGIN - (CRASH_RESTART_SIZE * BASELINE) as u32,
            );
            draw_text(
                &mut canvas,
                &font,
                restart,
                CRASH_RESTART_SIZE,
                (x as f32, y as f32),
                WHITE,
            );
            (
                right - col_w - CRASH_QR_GAP - CRASH_MARGIN,
                sh - CRASH_MARGIN,
            )
        }
        Orientation::Vertical => {
            // The QR code and the notes keep the screen margin to what they draw: the QR code's
            // white square, and the notes' right edge and last baseline
            let (right, bottom) = (sw - CRASH_MARGIN, sh - CRASH_MARGIN);
            let top = bottom - qr_side;
            if let Some((url, side)) = qr {
                draw_qr(Some(&mut canvas), url, CRASH_MARGIN, top, side);
            }
            let notes_w = right - (CRASH_MARGIN + qr_side + CRASH_NOTE_GAP);
            // Each line as large as fits beside the QR code, wrapped only if none does
            let fit = |text: &str, sizes: &[f32]| -> Vec<(String, f32)> {
                match sizes.iter().find(|&&size| width(text, size) <= notes_w) {
                    Some(&size) => vec![(String::from(text), size)],
                    None => {
                        let size = sizes[sizes.len() - 1];
                        wrap(text, chars(notes_w, size))
                            .into_iter()
                            .map(|line| (line, size))
                            .collect()
                    }
                }
            };
            let note_sizes = [CRASH_TRACE_SIZE, 13.0];
            let mut lines = fit(CRASH_SCAN_NOTE, &note_sizes);
            lines.extend(fit(&firmware, &note_sizes));
            lines.extend(fit(
                restart,
                &[CRASH_RESTART_SIZE, CRASH_TEXT_SIZE, 16.0, 14.0],
            ));
            // Stacked up from the bottom right corner by baseline
            let (mut baseline, mut notes_top) = (bottom, bottom);
            for (line, size) in lines.iter().rev() {
                notes_top = baseline - (size * BASELINE) as u32;
                let x = right - width(line, *size);
                draw_text(
                    &mut canvas,
                    &font,
                    line,
                    *size,
                    (x as f32, notes_top as f32),
                    WHITE,
                );
                baseline -= line_h(*size);
            }
            (sw - 2 * CRASH_MARGIN, top.min(notes_top) - SECTION_GAP)
        }
    };

    let mut lines: Vec<(String, f32, u32)> = vec![(String::from(":("), CRASH_FACE_SIZE, 0)];
    for line in wrap(CRASH_TITLE, chars(text_w, TITLE_SIZE)) {
        lines.push((line, TITLE_SIZE, 0));
    }
    let mut gap = SECTION_GAP;
    for line in message.lines() {
        for part in wrap(line, chars(text_w, CRASH_TEXT_SIZE)) {
            lines.push((part, CRASH_TEXT_SIZE, core::mem::take(&mut gap)));
        }
    }
    if !trace.is_empty() {
        lines.push((String::from("Stack trace:"), CRASH_TEXT_SIZE, SECTION_GAP));
        // Lines too long for the column are cut, keeping the address and the start of the name
        let max = chars(text_w, CRASH_TRACE_SIZE);
        for line in trace {
            let text = if line.chars().count() > max {
                let kept: String = line.chars().take(max.saturating_sub(2)).collect();
                format!("{kept}..")
            } else {
                line.clone()
            };
            lines.push((text, CRASH_TRACE_SIZE, 0));
        }
    }

    // Lines that do not fit above the bottom are dropped
    let mut y = CRASH_MARGIN;
    for (text, size, gap) in &lines {
        let top = y + gap;
        if top + line_h(*size) > text_bottom {
            break;
        }
        draw_text(
            &mut canvas,
            &font,
            text,
            *size,
            (CRASH_MARGIN as f32, top as f32),
            WHITE,
        );
        y = top + line_h(*size);
    }

    for y in 0..sh {
        for x in 0..sw {
            let (fx, fy) = to_framebuffer(orientation, x, y);
            framebuffer.set_pixel_indexed(fx, fy, canvas.get(x, y));
        }
    }
}

/// The first of `texts` whose QR code fits a `side` square with modules large enough to scan,
/// with the side it is drawn at
fn fit_qr(texts: &[String], side: u32) -> Option<(&str, u32)> {
    texts
        .iter()
        .find_map(|text| draw_qr(None, text, 0, 0, side).map(|drawn| (text.as_str(), drawn)))
}

/// Draw `text` as a black-on-white QR code with its quiet zone, from (x, y) and at most `side`
/// wide, returning the side it takes; or only measure it without a canvas. Returns `None` if its
/// modules would be too small to scan.
fn draw_qr(canvas: Option<&mut Canvas>, text: &str, x: u32, y: u32, side: u32) -> Option<u32> {
    let len = Version::MAX.buffer_len();
    let (mut temp, mut out) = (vec![0u8; len], vec![0u8; len]);
    let qr = QrCode::encode_text(
        text,
        &mut temp,
        &mut out,
        QrCodeEcc::Low,
        Version::MIN,
        Version::MAX,
        None,
        true,
    )
    .ok()?;
    let modules = qr.size() as u32 + 2 * QR_BORDER;
    let scale = side / modules;
    if scale < QR_MIN_SCALE {
        return None;
    }
    if let Some(canvas) = canvas {
        for my in 0..modules {
            for mx in 0..modules {
                let dark =
                    qr.get_module(mx as i32 - QR_BORDER as i32, my as i32 - QR_BORDER as i32);
                let color = if dark { BLACK } else { WHITE };
                for py in 0..scale {
                    for px in 0..scale {
                        canvas.set(x + mx * scale + px, y + my * scale + py, color);
                    }
                }
            }
        }
    }
    Some(modules * scale)
}

/// Split `text` into lines of at most `max_chars`, breaking at spaces where possible
fn wrap(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let mut word = word;
        loop {
            let len = line.chars().count();
            let sep = usize::from(len > 0);
            if len + sep + word.chars().count() <= max_chars {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                break;
            }
            if len > 0 {
                lines.push(core::mem::take(&mut line));
                continue;
            }
            // A word longer than a line is split
            let split = word
                .char_indices()
                .nth(max_chars)
                .map_or(word.len(), |(i, _)| i);
            lines.push(String::from(&word[..split]));
            word = &word[split..];
            if word.is_empty() {
                break;
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Draw a centered box with a title, an optional red line, and notes, returning its
/// framebuffer region
fn draw_notice(
    framebuffer: &mut Framebuffer,
    orientation: Orientation,
    title: &str,
    alert: Option<&str>,
    notes: &[&str],
) -> Rect {
    let font = font();
    let mut lines = Vec::new();
    let mut y = PADDING;
    lines.push((String::from(title), TITLE_SIZE, BLACK, y));
    y += line_h(TITLE_SIZE) + SECTION_GAP / 2;
    if let Some(alert) = alert {
        lines.push((String::from(alert), NOTE_SIZE, RED, y));
        y += line_h(NOTE_SIZE) + SECTION_GAP - LINE_GAP;
    }
    for &text in notes {
        lines.push((String::from(text), NOTE_SIZE, BLACK, y));
        y += line_h(NOTE_SIZE);
    }
    let box_h = y + PADDING - LINE_GAP;

    let content_w = lines
        .iter()
        .map(|(text, size, _, _)| text_width(&font, text, *size) as u32)
        .max()
        .unwrap_or(0);
    let (sw, sh) = screen_size(orientation);
    let box_w = ((content_w + 2 * PADDING).div_ceil(ALIGN) * ALIGN).min(sw);

    let mut data = vec![(WHITE << 4) | WHITE; Canvas::len(box_w, box_h)];
    let mut canvas = Canvas {
        data: &mut data,
        width: box_w,
        height: box_h,
    };
    draw_border(&mut canvas);
    for (text, size, color, y) in &lines {
        draw_text(
            &mut canvas,
            &font,
            text,
            *size,
            (PADDING as f32, *y as f32 - 3.0),
            *color,
        );
    }

    let origin = (sw.saturating_sub(box_w) / 2, sh.saturating_sub(box_h) / 2);
    for y in 0..box_h {
        for x in 0..box_w {
            let (fx, fy) = to_framebuffer(orientation, origin.0 + x, origin.1 + y);
            framebuffer.set_pixel_indexed(fx, fy, canvas.get(x, y));
        }
    }
    framebuffer_rect(orientation, origin.0, origin.1, box_w, box_h)
}

/// A line of text in the dialog: text, size, and top in box coordinates
struct Line {
    text: String,
    size: f32,
    y: u32,
}

/// Progress dialog state
pub struct Progress {
    orientation: Orientation,
    total: usize,
    /// Text lines; the first is the "Syncing" label, drawn after the WiFi icon
    lines: Vec<Line>,
    /// Index of the "Syncing" label in `lines`
    label: usize,
    /// WiFi signal bars shown before the label, if the RSSI is known
    wifi_bars: Option<u32>,
    /// Box top-left corner in screen coordinates
    origin: (u32, u32),
    /// Box size, sized to its content
    box_w: u32,
    box_h: u32,
    /// Progress bar top in box coordinates
    bar_y: u32,
    /// Framebuffer contents under the box, saved when it is drawn
    saved: Option<(Rect, Vec<u8>)>,
    /// Fill width (px) currently on the panel, `None` until the box is shown
    shown: Option<u32>,
    /// Fill width drawn by the last `draw` or `update`
    drawn: u32,
}

/// Line height for a text size
fn line_h(size: f32) -> u32 {
    size as u32 + LINE_GAP
}

impl Progress {
    /// Small box in the top-left corner.
    ///
    /// `rssi` is the WiFi signal strength in dBm, shown as an icon before the label.
    pub fn new(orientation: Orientation, total: usize, rssi: Option<i32>) -> Self {
        Self::build(orientation, total, rssi, None)
    }

    /// Larger centered box with a welcome title and usage instructions
    pub fn welcome(
        orientation: Orientation,
        total: usize,
        rssi: Option<i32>,
        refresh_minutes: u32,
    ) -> Self {
        Self::build(orientation, total, rssi, Some(refresh_minutes))
    }

    /// `welcome` holds the refresh interval for a welcome box
    fn build(
        orientation: Orientation,
        total: usize,
        rssi: Option<i32>,
        welcome: Option<u32>,
    ) -> Self {
        let font = font();
        let wifi_bars = rssi.map(signal_bars);
        let mut lines = Vec::new();
        let mut y = PADDING;

        if welcome.is_some() {
            lines.push(Line {
                text: String::from("Welcome to SawThat Frame"),
                size: TITLE_SIZE,
                y,
            });
            y += line_h(TITLE_SIZE) + SECTION_GAP / 2;
        }

        let label = lines.len();
        lines.push(Line {
            text: format!(
                "Syncing {} concert{}...",
                total,
                if total == 1 { "" } else { "s" }
            ),
            size: TEXT_SIZE,
            y,
        });
        y += line_h(TEXT_SIZE) + BAR_GAP - LINE_GAP;
        let bar_y = y;
        y += BAR_H + PADDING;

        if let Some(minutes) = welcome {
            y += SECTION_GAP - PADDING;
            let interval = format!(
                "Artwork changes every {} minute{} and",
                minutes,
                if minutes == 1 { "" } else { "s" }
            );
            let notes = INSTRUCTIONS
                .iter()
                .map(|&text| String::from(text))
                .chain([interval, String::from(RESYNC_NOTE)]);
            for text in notes {
                lines.push(Line {
                    text,
                    size: NOTE_SIZE,
                    y,
                });
                y += line_h(NOTE_SIZE);
            }
            y += PADDING - LINE_GAP;
        }

        let content_w = lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let icon = if i == label {
                    wifi_bars.map_or(0, |_| ICON_W + ICON_GAP)
                } else {
                    0
                };
                text_width(&font, &line.text, line.size) as u32 + icon
            })
            .max()
            .unwrap_or(0);
        let box_w = (content_w + 2 * PADDING).div_ceil(ALIGN) * ALIGN;
        let box_h = y;

        let origin = if welcome.is_some() {
            let (sw, sh) = screen_size(orientation);
            (sw.saturating_sub(box_w) / 2, sh.saturating_sub(box_h) / 2)
        } else {
            (MARGIN, MARGIN)
        };

        Self {
            orientation,
            total,
            lines,
            label,
            wifi_bars,
            origin,
            box_w,
            box_h,
            bar_y,
            saved: None,
            shown: None,
            drawn: 0,
        }
    }

    /// Framebuffer coordinates of a box coordinate
    fn to_fb(&self, x: u32, y: u32) -> (u32, u32) {
        to_framebuffer(self.orientation, self.origin.0 + x, self.origin.1 + y)
    }

    /// Framebuffer rect of a box-coordinate rectangle
    fn rect(&self, x: u32, y: u32, w: u32, h: u32) -> Rect {
        framebuffer_rect(self.orientation, self.origin.0 + x, self.origin.1 + y, w, h)
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
        let rect = self.rect(0, 0, self.box_w, self.box_h);
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

        let (x, y) = (PADDING + 1 + shown, self.bar_y + 1);
        for by in y..y + BAR_H - 2 {
            for bx in x..PADDING + 1 + fill {
                let (fx, fy) = self.to_fb(bx, by);
                framebuffer.set_pixel_indexed(fx, fy, GREEN);
            }
        }
        self.drawn = fill;
        let rect = self.rect(x, y, fill - shown, BAR_H - 2);
        Some((rect, framebuffer.extract_rect(&rect)))
    }

    /// Whether the dialog is currently on the panel
    pub fn is_shown(&self) -> bool {
        self.shown.is_some()
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
        let (box_w, box_h, bar_w, bar_y, fill) =
            (self.box_w, self.box_h, self.bar_w(), self.bar_y, self.drawn);
        // White is palette index 1 in both nibbles
        let mut data = vec![(WHITE << 4) | WHITE; Canvas::len(box_w, box_h)];
        let mut canvas = Canvas {
            data: &mut data,
            width: box_w,
            height: box_h,
        };

        draw_border(&mut canvas);

        // Bar outline and fill
        for y in bar_y..bar_y + BAR_H {
            for x in PADDING..PADDING + bar_w {
                let edge = y == bar_y
                    || y == bar_y + BAR_H - 1
                    || x == PADDING
                    || x == PADDING + bar_w - 1;
                let color = if edge {
                    BLACK
                } else if x - PADDING - 1 < fill {
                    GREEN
                } else {
                    WHITE
                };
                canvas.set(x, y, color);
            }
        }

        let font = font();
        for (i, line) in self.lines.iter().enumerate() {
            let mut text_x = PADDING;
            if i == self.label
                && let Some(bars) = self.wifi_bars
            {
                draw_wifi_icon(&mut canvas, PADDING, line.y + ICON_Y - PADDING, bars);
                text_x += ICON_W + ICON_GAP;
            }
            draw_text(
                &mut canvas,
                &font,
                &line.text,
                line.size,
                (text_x as f32, line.y as f32 - 3.0),
                BLACK,
            );
        }

        for y in 0..box_h {
            for x in 0..box_w {
                let (fx, fy) = self.to_fb(x, y);
                framebuffer.set_pixel_indexed(fx, fy, canvas.get(x, y));
            }
        }
    }
}
