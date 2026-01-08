//! Driver for Good Display GDEP073E01 / Waveshare 7.3inch e-Paper HAT (E)
//! using E Ink Spectra 6 technology (6-color e-paper).
//!
//! Based on the Good Display reference implementation with fast refresh mode support.

mod color;
mod command;

pub use color::Color;

use command::Command;
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal::spi::SpiDevice;

/// Display width in pixels
pub const WIDTH: u32 = 800;
/// Display height in pixels
pub const HEIGHT: u32 = 480;
/// Buffer size: 4 bits per pixel, 2 pixels per byte
pub const BUFFER_SIZE: usize = (WIDTH as usize * HEIGHT as usize) / 2;

/// Rectangle defining a partial update region
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    /// X coordinate of top-left corner (must be even for pixel alignment)
    pub x: u16,
    /// Y coordinate of top-left corner
    pub y: u16,
    /// Width in pixels (must be even for byte alignment)
    pub width: u16,
    /// Height in pixels
    pub height: u16,
}

impl Rect {
    /// Create a new rectangle
    ///
    /// `x` and `width` will be adjusted to even values for byte alignment.
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x: x & !1, // Round down to even
            y,
            width: (width + 1) & !1, // Round up to even
            height,
        }
    }

    /// Calculate buffer size needed for this region (bytes)
    pub const fn buffer_size(&self) -> usize {
        (self.width as usize * self.height as usize) / 2
    }

    /// Check if rectangle is within display bounds
    pub const fn is_valid(&self) -> bool {
        self.x < WIDTH as u16
            && self.y < HEIGHT as u16
            && self.x + self.width <= WIDTH as u16
            && self.y + self.height <= HEIGHT as u16
            && self.width > 0
            && self.height > 0
    }
}

/// Initialization/refresh mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshMode {
    /// Standard refresh (~15-20s) - best image quality
    #[default]
    Standard,
    /// Fast refresh (~5-8s) - slightly reduced quality
    Fast,
}

/// Driver for the 7.3" Spectra 6 e-paper display
pub struct Epd7in3e<SPI, BUSY, DC, RST> {
    spi: SPI,
    busy: BUSY,
    dc: DC,
    rst: RST,
    refresh_mode: RefreshMode,
}

impl<SPI, BUSY, DC, RST> Epd7in3e<SPI, BUSY, DC, RST>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    DC: OutputPin,
    RST: OutputPin,
{
    /// Create a new display driver instance.
    ///
    /// Performs hardware reset and initialization.
    pub fn new<DELAY: DelayNs>(
        spi: SPI,
        busy: BUSY,
        dc: DC,
        rst: RST,
        delay: &mut DELAY,
        refresh_mode: RefreshMode,
    ) -> Result<Self, SPI::Error> {
        let mut epd = Self {
            spi,
            busy,
            dc,
            rst,
            refresh_mode,
        };

        epd.hardware_reset(delay);
        epd.init(delay)?;

        Ok(epd)
    }

    /// Hardware reset sequence
    fn hardware_reset<DELAY: DelayNs>(&mut self, delay: &mut DELAY) {
        let _ = self.rst.set_high();
        delay.delay_ms(10);
        let _ = self.rst.set_low();
        delay.delay_ms(10);
        let _ = self.rst.set_high();
        delay.delay_ms(10);
    }

    /// Wait for the display to become idle (BUSY pin high)
    pub fn wait_until_idle<DELAY: DelayNs>(&mut self, delay: &mut DELAY) {
        // BUSY is active low on this display
        while self.busy.is_low().unwrap_or(true) {
            delay.delay_ms(10);
        }
    }

    /// Send a command to the display
    fn send_command(&mut self, command: Command) -> Result<(), SPI::Error> {
        let _ = self.dc.set_low();
        self.spi.write(&[command.addr()])
    }

    /// Send data to the display
    fn send_data(&mut self, data: &[u8]) -> Result<(), SPI::Error> {
        let _ = self.dc.set_high();
        self.spi.write(data)
    }

    /// Send command followed by data
    fn cmd_with_data(&mut self, command: Command, data: &[u8]) -> Result<(), SPI::Error> {
        self.send_command(command)?;
        self.send_data(data)
    }

    /// Initialize the display with standard mode settings
    fn init_standard<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        // Command header
        self.cmd_with_data(Command::CMDH, &[0x49, 0x55, 0x20, 0x08, 0x09, 0x18])?;

        // Power setting
        self.cmd_with_data(Command::PWRR, &[0x3F])?;

        // Panel setting
        self.cmd_with_data(Command::PSR, &[0x5F, 0x69])?;

        // Power off sequence
        self.cmd_with_data(Command::POFS, &[0x00, 0x54, 0x00, 0x44])?;

        // Booster soft start 1
        self.cmd_with_data(Command::BTST1, &[0x40, 0x1F, 0x1F, 0x2C])?;

        // Booster soft start 2
        self.cmd_with_data(Command::BTST2, &[0x6F, 0x1F, 0x17, 0x49])?;

        // Booster soft start 3
        self.cmd_with_data(Command::BTST3, &[0x6F, 0x1F, 0x1F, 0x22])?;

        // PLL control
        self.cmd_with_data(Command::PLL, &[0x08])?;

        // VCOM and data interval
        self.cmd_with_data(Command::CDI, &[0x3F])?;

        // TCON setting
        self.cmd_with_data(Command::TCON, &[0x02, 0x00])?;

        // Resolution: 800x480 (0x0320 x 0x01E0)
        self.cmd_with_data(Command::TRES, &[0x03, 0x20, 0x01, 0xE0])?;

        // Temperature VCOM DC
        self.cmd_with_data(Command::T_VDCS, &[0x01])?;

        // Power saving
        self.cmd_with_data(Command::PWS, &[0x2F])?;

        // Power on
        self.send_command(Command::PON)?;
        self.wait_until_idle(delay);

        Ok(())
    }

    /// Initialize the display with fast mode settings
    fn init_fast<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        // Command header
        self.cmd_with_data(Command::CMDH, &[0x49, 0x55, 0x20, 0x08, 0x09, 0x18])?;

        // Power setting (extended for fast mode)
        self.cmd_with_data(Command::PWRR, &[0x3F, 0x00, 0x32, 0x2A, 0x0E, 0x2A])?;

        // Panel setting
        self.cmd_with_data(Command::PSR, &[0x5F, 0x69])?;

        // Power off sequence
        self.cmd_with_data(Command::POFS, &[0x00, 0x54, 0x00, 0x44])?;

        // Booster soft start 1
        self.cmd_with_data(Command::BTST1, &[0x40, 0x1F, 0x1F, 0x2C])?;

        // Booster soft start 2 (different values for fast mode)
        self.cmd_with_data(Command::BTST2, &[0x6F, 0x1F, 0x16, 0x25])?;

        // Booster soft start 3
        self.cmd_with_data(Command::BTST3, &[0x6F, 0x1F, 0x1F, 0x22])?;

        // IPC (fast mode specific)
        self.cmd_with_data(Command::IPC, &[0x00, 0x04])?;

        // PLL control (faster)
        self.cmd_with_data(Command::PLL, &[0x02])?;

        // TSE - Temperature sensor enable
        self.cmd_with_data(Command::TSE, &[0x00])?;

        // VCOM and data interval
        self.cmd_with_data(Command::CDI, &[0x3F])?;

        // TCON setting
        self.cmd_with_data(Command::TCON, &[0x02, 0x00])?;

        // Resolution: 800x480
        self.cmd_with_data(Command::TRES, &[0x03, 0x20, 0x01, 0xE0])?;

        // VCOM DC setting
        self.cmd_with_data(Command::VDCS, &[0x1E])?;

        // Temperature VCOM DC
        self.cmd_with_data(Command::T_VDCS, &[0x01])?;

        // AGID (fast mode specific)
        self.cmd_with_data(Command::AGID, &[0x00])?;

        // Power saving
        self.cmd_with_data(Command::PWS, &[0x2F])?;

        // CCSET (fast mode specific)
        self.cmd_with_data(Command::CCSET, &[0x00])?;

        // TSSET (fast mode specific)
        self.cmd_with_data(Command::TSSET, &[0x00])?;

        // Power on
        self.send_command(Command::PON)?;
        self.wait_until_idle(delay);

        Ok(())
    }

    /// Initialize the display
    fn init<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        match self.refresh_mode {
            RefreshMode::Standard => self.init_standard(delay),
            RefreshMode::Fast => self.init_fast(delay),
        }
    }

    /// Clear the display to a single color
    pub fn clear<DELAY: DelayNs>(
        &mut self,
        color: Color,
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        self.clear_start(color, delay)?;
        self.refresh_wait(delay)
    }

    /// Start clearing the display (non-blocking after refresh starts)
    /// Call `refresh_wait()` before the next display operation.
    pub fn clear_start<DELAY: DelayNs>(
        &mut self,
        color: Color,
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        let color_byte = color.to_dual_pixel();

        self.send_command(Command::DTM)?;
        let _ = self.dc.set_high();

        // Send in chunks to avoid stack issues
        let chunk = [color_byte; 1000];
        for _ in 0..(BUFFER_SIZE / 1000) {
            self.spi.write(&chunk)?;
        }
        // Remainder
        let remainder = BUFFER_SIZE % 1000;
        if remainder > 0 {
            self.spi.write(&chunk[..remainder])?;
        }

        self.refresh_start(delay)
    }

    /// Display a raw buffer (must be BUFFER_SIZE bytes, 4bpp packed)
    pub fn display<DELAY: DelayNs>(
        &mut self,
        buffer: &[u8],
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        self.send_command(Command::DTM)?;
        self.send_data(buffer)?;
        self.refresh(delay)
    }

    /// Start displaying a raw buffer (non-blocking).
    /// Call `is_busy()` to poll, then `finish_display()` when done.
    pub fn display_start<DELAY: DelayNs>(
        &mut self,
        buffer: &[u8],
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        self.send_command(Command::DTM)?;
        self.send_data(buffer)?;
        self.refresh_start(delay)
    }

    /// Check if display is still busy refreshing.
    pub fn is_busy(&mut self) -> bool {
        self.busy.is_low().unwrap_or(true)
    }

    /// Finish display refresh after polling `is_busy()` returns false.
    pub fn finish_display<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        // Power off
        self.cmd_with_data(Command::POF, &[0x00])?;
        self.wait_until_idle(delay);
        Ok(())
    }

    /// Trigger display refresh (blocking)
    fn refresh<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        self.refresh_start(delay)?;
        self.refresh_wait(delay)
    }

    /// Start display refresh (non-blocking)
    /// Call `refresh_wait()` to complete the refresh before the next operation.
    fn refresh_start<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        // Power on (required before refresh - display may be off from previous operation)
        self.send_command(Command::PON)?;
        self.wait_until_idle(delay);

        // For standard mode, need to set BTST2 before refresh
        if self.refresh_mode == RefreshMode::Standard {
            self.cmd_with_data(Command::BTST2, &[0x6F, 0x1F, 0x17, 0x49])?;
        } else {
            // Fast mode also needs BTST2 but with different values
            self.cmd_with_data(Command::BTST2, &[0x6F, 0x1F, 0x16, 0x25])?;
        }

        // Display refresh
        self.cmd_with_data(Command::DRF, &[0x00])?;
        delay.delay_ms(1); // Required delay (min 200us)

        // Returns immediately - display is now refreshing
        Ok(())
    }

    /// Wait for refresh to complete and power off
    /// Must be called after `refresh_start()` or `clear_start()` before the next display operation.
    pub fn refresh_wait<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        self.wait_until_idle(delay);

        // Power off
        self.cmd_with_data(Command::POF, &[0x00])?;
        self.wait_until_idle(delay);

        Ok(())
    }

    /// Put the display into sleep mode
    pub fn sleep<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        self.cmd_with_data(Command::POF, &[0x00])?;
        self.wait_until_idle(delay);

        self.cmd_with_data(Command::DSLP, &[0xA5])?;
        delay.delay_ms(100);

        Ok(())
    }

    /// Wake the display from sleep (requires full re-init)
    pub fn wake_up<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        self.hardware_reset(delay);
        self.init(delay)
    }

    /// Change refresh mode (requires re-init to take effect)
    pub fn set_refresh_mode(&mut self, mode: RefreshMode) {
        self.refresh_mode = mode;
    }

    /// Get current refresh mode
    pub fn refresh_mode(&self) -> RefreshMode {
        self.refresh_mode
    }

    // ==================== Partial Update Methods ====================

    /// Set the partial window region for subsequent partial updates.
    ///
    /// Coordinates are specified as inclusive start/end positions.
    fn set_partial_window(&mut self, rect: &Rect) -> Result<(), SPI::Error> {
        let x_start = rect.x;
        let x_end = rect.x + rect.width - 1;
        let y_start = rect.y;
        let y_end = rect.y + rect.height - 1;

        self.send_command(Command::PTLW)?;
        // X coordinates (10-bit: MSB [9:8], LSB [7:0])
        self.send_data(&[(x_start >> 8) as u8 & 0x03])?;
        self.send_data(&[(x_start & 0xFF) as u8])?;
        self.send_data(&[(x_end >> 8) as u8 & 0x03])?;
        self.send_data(&[(x_end & 0xFF) as u8])?;
        // Y coordinates (10-bit: MSB [9:8], LSB [7:0])
        self.send_data(&[(y_start >> 8) as u8 & 0x03])?;
        self.send_data(&[(y_start & 0xFF) as u8])?;
        self.send_data(&[(y_end >> 8) as u8 & 0x03])?;
        self.send_data(&[(y_end & 0xFF) as u8])?;
        // Enable partial window mode
        self.send_data(&[0x01])
    }

    /// Perform a partial update on a rectangular region (blocking).
    ///
    /// The buffer must contain exactly `rect.buffer_size()` bytes of 4bpp packed pixel data
    /// for the specified region.
    pub fn partial_update<DELAY: DelayNs>(
        &mut self,
        rect: &Rect,
        buffer: &[u8],
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        debug_assert!(rect.is_valid(), "Partial update rect out of bounds");
        debug_assert_eq!(
            buffer.len(),
            rect.buffer_size(),
            "Buffer size mismatch for partial update"
        );

        // Set partial window
        self.set_partial_window(rect)?;
        self.wait_until_idle(delay);

        // Send pixel data
        self.send_command(Command::DTM)?;
        self.send_data(buffer)?;

        // Refresh the partial region
        self.partial_refresh(delay)
    }

    /// Fill a rectangular region with a solid color (blocking).
    pub fn partial_fill<DELAY: DelayNs>(
        &mut self,
        rect: &Rect,
        color: Color,
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        debug_assert!(rect.is_valid(), "Partial fill rect out of bounds");

        // Set partial window
        self.set_partial_window(rect)?;
        self.wait_until_idle(delay);

        // Send solid color data
        let color_byte = color.to_dual_pixel();
        let row_bytes = rect.width as usize / 2;

        self.send_command(Command::DTM)?;
        let _ = self.dc.set_high();

        // Send row by row to avoid large stack allocations
        let chunk = [color_byte; 100];
        for _ in 0..rect.height {
            let mut remaining = row_bytes;
            while remaining > 0 {
                let send = remaining.min(100);
                self.spi.write(&chunk[..send])?;
                remaining -= send;
            }
        }

        // Refresh the partial region
        self.partial_refresh(delay)
    }

    /// Start a partial update (non-blocking after refresh starts).
    /// Call `refresh_wait()` before the next display operation.
    pub fn partial_update_start<DELAY: DelayNs>(
        &mut self,
        rect: &Rect,
        buffer: &[u8],
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        debug_assert!(rect.is_valid(), "Partial update rect out of bounds");
        debug_assert_eq!(
            buffer.len(),
            rect.buffer_size(),
            "Buffer size mismatch for partial update"
        );

        // Set partial window
        self.set_partial_window(rect)?;
        self.wait_until_idle(delay);

        // Send pixel data
        self.send_command(Command::DTM)?;
        self.send_data(buffer)?;

        // Start refresh (non-blocking)
        self.partial_refresh_start(delay)
    }

    /// Refresh after partial data transmission (blocking).
    fn partial_refresh<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        self.partial_refresh_start(delay)?;
        self.refresh_wait(delay)
    }

    /// Start refresh after partial data transmission (non-blocking).
    fn partial_refresh_start<DELAY: DelayNs>(
        &mut self,
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        self.wait_until_idle(delay);

        // Power on
        self.send_command(Command::PON)?;
        self.wait_until_idle(delay);

        // Booster settings (same as standard refresh)
        if self.refresh_mode == RefreshMode::Standard {
            self.cmd_with_data(Command::BTST2, &[0x6F, 0x1F, 0x17, 0x49])?;
        } else {
            self.cmd_with_data(Command::BTST2, &[0x6F, 0x1F, 0x16, 0x25])?;
        }

        // Trigger display refresh
        self.cmd_with_data(Command::DRF, &[0x00])?;
        delay.delay_ms(1);

        Ok(())
    }

    // ==================== Test Pattern Methods ====================

    /// Display a 6-color test pattern
    /// Layout (2 rows x 3 cols):
    /// ```text
    /// | Black  | White  | Yellow |
    /// | Red    | Blue   | Green  |
    /// ```
    pub fn show_6block<DELAY: DelayNs>(&mut self, delay: &mut DELAY) -> Result<(), SPI::Error> {
        self.show_6block_internal(None, delay)
    }

    /// Display 6-color pattern with one block replaced
    /// `replace`: (block_index 0-5, new_color)
    /// Block indices:
    /// ```text
    /// | 0 | 1 | 2 |
    /// | 3 | 4 | 5 |
    /// ```
    pub fn show_6block_replaced<DELAY: DelayNs>(
        &mut self,
        block_index: usize,
        new_color: Color,
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        self.show_6block_internal(Some((block_index, new_color)), delay)
    }

    fn show_6block_internal<DELAY: DelayNs>(
        &mut self,
        replace: Option<(usize, Color)>,
        delay: &mut DELAY,
    ) -> Result<(), SPI::Error> {
        let mut colors = [
            Color::Black,
            Color::White,
            Color::Yellow,
            Color::Red,
            Color::Blue,
            Color::Green,
        ];

        // Replace one block color if specified
        if let Some((idx, color)) = replace
            && idx < 6
        {
            colors[idx] = color;
        }

        self.send_command(Command::DTM)?;
        let _ = self.dc.set_high();

        let block_height = HEIGHT as usize / 2;
        let block_width = WIDTH as usize / 3;

        for row in 0..HEIGHT as usize {
            let color_row = if row < block_height { 0 } else { 3 };

            for col in 0..(WIDTH as usize / 2) {
                let pixel_col = col * 2;
                let color_col = pixel_col / block_width;

                let color1 = colors[color_row + color_col.min(2)];
                let color2 = colors[color_row + ((pixel_col + 1) / block_width).min(2)];

                let byte = (color1.to_4bit() << 4) | color2.to_4bit();
                self.spi.write(&[byte])?;
            }
        }

        self.refresh(delay)
    }
}
