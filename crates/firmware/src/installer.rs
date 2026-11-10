//! Files written by the web installer.
//!
//! The installer can flash two things past the app, for the firmware to apply at boot:
//!
//! - Config settings, as `KEY=VALUE` lines in the last sector of the flash, applied to
//!   `CONFIG.TXT` on the SD card.
//! - A font for the artwork, in the end of the app partition, copied to the SD card.
//!
//! Each is erased once applied, so it applies once and the WiFi password does not stay in flash.
//! Each starts with a magic, so anything else there, such as erased flash, holds nothing.

use alloc::string::String;
use alloc::vec::Vec;

use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;

const SECTOR: u32 = FlashStorage::SECTOR_SIZE;

/// Start of the settings sector: the last 4 KB of the 16 MB flash. Layout, little endian: the
/// magic, the length of the lines, an FNV-1a hash of them, then the lines.
pub const SETTINGS_OFFSET: u32 = 0xFF_F000;
const SETTINGS_MAGIC: [u8; 16] = *b"SAWTHAT-CONFIG-1";
const SETTINGS_HEADER: usize = SETTINGS_MAGIC.len() + 8;

/// Start of the font area, 11 MB into the app partition, which ends at 0xFB0000. Layout, little
/// endian: the magic, the file extension padded with zeros, the font length, an FNV-1a hash of
/// the font, then the font.
pub const FONT_OFFSET: u32 = 0xB0_0000;
const FONT_END: u32 = 0xFB_0000;
const FONT_MAGIC: [u8; 16] = *b"SAWTHAT-FONT-001";
const FONT_HEADER: usize = FONT_MAGIC.len() + 12;

/// Config keys the installer may set
const KEYS: [&str; 5] = [
    "WIFI_SSID",
    "WIFI_PASS",
    "SAWTHAT_USER_ID",
    "REFRESH_MINUTES",
    "ORIENTATION",
];

/// Font file names the installer may write
pub const FONT_NAMES: [&str; 2] = ["FONT.TTF", "FONT.OTF"];

/// The flash areas the installer writes
pub struct Installer(FlashStorage<'static>);

impl Installer {
    pub fn new(flash: FLASH<'static>) -> Self {
        // The render task runs on the other core, which must pause while the flash is busy
        Self(FlashStorage::new(flash).multicore_auto_park())
    }

    /// Read `buf.len()` bytes at `offset`, logging a failure
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Option<()> {
        self.0
            .read(offset, buf)
            .map_err(|e| log::info!("Failed to read installer data at {:#x}: {:?}", offset, e))
            .ok()
    }

    /// The settings as (key, value) pairs, or `None` if the sector holds none
    pub fn settings(&mut self) -> Option<Vec<(String, String)>> {
        let mut header = [0u8; SETTINGS_HEADER];
        self.read(SETTINGS_OFFSET, &mut header)?;
        if header[..SETTINGS_MAGIC.len()] != SETTINGS_MAGIC {
            return None;
        }
        let len = le_u32(&header, SETTINGS_MAGIC.len()) as usize;
        if len > SECTOR as usize - SETTINGS_HEADER {
            return None;
        }
        let mut lines = alloc::vec![0u8; len];
        self.read(SETTINGS_OFFSET + SETTINGS_HEADER as u32, &mut lines)?;
        if fnv1a(&lines) != le_u32(&header, SETTINGS_MAGIC.len() + 4) {
            return None;
        }
        let lines = core::str::from_utf8(&lines).ok()?;
        Some(
            lines
                .lines()
                .filter_map(|line| line.split_once('='))
                .filter(|(key, _)| KEYS.contains(&key.trim()))
                .map(|(key, value)| (String::from(key.trim()), String::from(value.trim())))
                .collect(),
        )
    }

    /// Erase the settings once applied
    pub fn clear_settings(&mut self) {
        self.erase(SETTINGS_OFFSET, SETTINGS_OFFSET + SECTOR);
    }

    /// The font as its file name on the SD card and its contents, or `None` if there is none
    pub fn font(&mut self) -> Option<(&'static str, Vec<u8>)> {
        let mut header = [0u8; FONT_HEADER];
        self.read(FONT_OFFSET, &mut header)?;
        if header[..FONT_MAGIC.len()] != FONT_MAGIC {
            return None;
        }
        // "TTF\0" or "OTF\0", the extension of one of FONT_NAMES
        let ext = &header[FONT_MAGIC.len()..FONT_MAGIC.len() + 4];
        let name = FONT_NAMES
            .into_iter()
            .find(|name| name.as_bytes()[5..] == ext[..3] && ext[3] == 0)?;
        let len = le_u32(&header, FONT_MAGIC.len() + 4);
        if len as u64 + (FONT_OFFSET as usize + FONT_HEADER) as u64 > FONT_END as u64 {
            return None;
        }
        let mut font = alloc::vec![0u8; len as usize];
        self.read(FONT_OFFSET + FONT_HEADER as u32, &mut font)?;
        if fnv1a(&font) != le_u32(&header, FONT_MAGIC.len() + 8) {
            return None;
        }
        Some((name, font))
    }

    /// Erase a font of `len` bytes once copied
    pub fn clear_font(&mut self, len: usize) {
        let end = (FONT_OFFSET + (FONT_HEADER + len) as u32).div_ceil(SECTOR) * SECTOR;
        self.erase(FONT_OFFSET, end.min(FONT_END));
    }

    fn erase(&mut self, from: u32, to: u32) {
        if let Err(e) = self.0.erase(from, to) {
            log::info!("Failed to clear installer data at {:#x}: {:?}", from, e);
        }
    }
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// 32-bit FNV-1a hash
fn fnv1a(data: &[u8]) -> u32 {
    data.iter().fold(0x811c_9dc5, |hash, &b| {
        (hash ^ b as u32).wrapping_mul(0x0100_0193)
    })
}
