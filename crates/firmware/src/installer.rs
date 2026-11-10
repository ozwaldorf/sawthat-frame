//! Files written by the web installer.
//!
//! The installer can flash two things besides the firmware, for it to apply at boot, each in its
//! own partition (see `partitions.csv`):
//!
//! - Config settings, as `KEY=VALUE` lines in the `settings` partition, applied to `CONFIG.TXT`
//!   on the SD card.
//! - A font for the artwork, in the `font` partition, copied to the SD card.
//!
//! Each is erased once applied, so it applies once and the WiFi password does not stay in flash.
//! Each starts with a magic, so anything else there, such as erased flash, holds nothing.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use esp_bootloader_esp_idf::partitions::{PARTITION_TABLE_MAX_LEN, read_partition_table};
use esp_storage::FlashStorage;

const SECTOR: u32 = FlashStorage::SECTOR_SIZE;

/// Settings layout, little endian: the magic, the length of the lines, an FNV-1a hash of them,
/// then the lines
const SETTINGS_MAGIC: [u8; 16] = *b"SAWTHAT-CONFIG-1";
const SETTINGS_HEADER: usize = SETTINGS_MAGIC.len() + 8;

/// Font layout, little endian: the magic, the file extension padded with zeros, the font length,
/// an FNV-1a hash of the font, then the font
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

/// A partition's offset and length in flash
#[derive(Clone, Copy)]
struct Area {
    offset: u32,
    len: u32,
}

/// The flash partitions the installer writes
pub struct Installer<'a> {
    flash: &'a mut FlashStorage<'static>,
    settings: Option<Area>,
    font: Option<Area>,
}

impl<'a> Installer<'a> {
    /// Find the installer partitions in the partition table
    pub fn new(flash: &'a mut FlashStorage<'static>) -> Self {
        let mut buf = Box::new([0u8; PARTITION_TABLE_MAX_LEN]);
        let (settings, font) = match read_partition_table(flash, &mut *buf) {
            Ok(table) => {
                let find = |label: &str| {
                    table
                        .iter()
                        .find(|part| part.label_as_str() == label)
                        .map(|part| Area {
                            offset: part.offset(),
                            len: part.len(),
                        })
                };
                (find("settings"), find("font"))
            }
            Err(e) => {
                log::info!("Failed to read the partition table: {:?}", e);
                (None, None)
            }
        };
        Self {
            flash,
            settings,
            font,
        }
    }

    /// Read `buf.len()` bytes at `offset`, logging a failure
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> Option<()> {
        self.flash
            .read(offset, buf)
            .map_err(|e| log::info!("Failed to read installer data at {:#x}: {:?}", offset, e))
            .ok()
    }

    /// The settings as (key, value) pairs, or `None` if the partition holds none
    pub fn settings(&mut self) -> Option<Vec<(String, String)>> {
        let area = self.settings?;
        let mut header = [0u8; SETTINGS_HEADER];
        self.read(area.offset, &mut header)?;
        if header[..SETTINGS_MAGIC.len()] != SETTINGS_MAGIC {
            return None;
        }
        let len = le_u32(&header, SETTINGS_MAGIC.len()) as usize;
        if len > area.len as usize - SETTINGS_HEADER {
            return None;
        }
        let mut lines = alloc::vec![0u8; len];
        self.read(area.offset + SETTINGS_HEADER as u32, &mut lines)?;
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
        if let Some(area) = self.settings {
            self.erase(area.offset, area.offset + area.len);
        }
    }

    /// The font as its file name on the SD card and its contents, or `None` if there is none
    pub fn font(&mut self) -> Option<(&'static str, Vec<u8>)> {
        let area = self.font?;
        let mut header = [0u8; FONT_HEADER];
        self.read(area.offset, &mut header)?;
        if header[..FONT_MAGIC.len()] != FONT_MAGIC {
            return None;
        }
        // "TTF\0" or "OTF\0", the extension of one of FONT_NAMES
        let ext = &header[FONT_MAGIC.len()..FONT_MAGIC.len() + 4];
        let name = FONT_NAMES
            .into_iter()
            .find(|name| name.as_bytes()[5..] == ext[..3] && ext[3] == 0)?;
        let len = le_u32(&header, FONT_MAGIC.len() + 4);
        if len as u64 + FONT_HEADER as u64 > area.len as u64 {
            return None;
        }
        let mut font = alloc::vec![0u8; len as usize];
        self.read(area.offset + FONT_HEADER as u32, &mut font)?;
        if fnv1a(&font) != le_u32(&header, FONT_MAGIC.len() + 8) {
            return None;
        }
        Some((name, font))
    }

    /// Erase a font of `len` bytes once copied
    pub fn clear_font(&mut self, len: usize) {
        if let Some(area) = self.font {
            let end = (FONT_HEADER as u32 + len as u32).div_ceil(SECTOR) * SECTOR;
            self.erase(area.offset, area.offset + end.min(area.len));
        }
    }

    fn erase(&mut self, from: u32, to: u32) {
        if let Err(e) = self.flash.erase(from, to) {
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
