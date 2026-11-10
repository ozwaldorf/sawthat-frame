//! SD card-based image cache
//!
//! Stores pre-rendered artwork directly on the SD card's FAT filesystem.
//! Directory structure mirrors the API paths:
//!
//! /concerts/
//!   HORIZ.JSN / VERT.JSN     - JSON arrays of items with artwork per orientation
//!   BANDS.JSN                - raw SawThat bands response
//!   horiz/
//!     {hash}.IMG             - horizontal artwork (see sawthat_render::pack_image)
//!   vert/
//!     {hash}.IMG             - vertical artwork

use core::fmt::Write as FmtWrite;

use embedded_hal::spi::SpiDevice;
use embedded_sdmmc::{Mode, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use heapless::String;
use log::info;

use crate::widget::{Orientation, WidgetData};

/// Root directory (mirrors API path)
const ROOT_DIR: &str = "concerts";

/// Horizontal orientation subdirectory
const HORIZ_DIR: &str = "horiz";

/// Vertical orientation subdirectory
const VERT_DIR: &str = "vert";

/// Ready item list filename per orientation (JSON array of item paths) - 8.3 format
fn items_file(orientation: Orientation) -> &'static str {
    match orientation {
        Orientation::Horizontal => "HORIZ.JSN",
        Orientation::Vertical => "VERT.JSN",
    }
}

/// Orientation file of older firmware, migrated into CONFIG.TXT - 8.3 format
const ORIENT_FILE: &str = "ORIENT.DAT";

/// Raw SawThat bands response - 8.3 format
const BANDS_FILE: &str = "BANDS.JSN";

/// Dummy time source (SD cards need timestamps but we don't care)
pub struct DummyTimesource;

impl TimeSource for DummyTimesource {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 55, // 2025
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

/// Cache error types
#[derive(Debug)]
pub enum CacheError {
    /// SD card error
    SdCard,
    /// File not found
    NotFound,
    /// Filesystem error
    Filesystem,
    /// File too large
    TooLarge,
    /// Write error
    Write,
    /// Read error
    Read,
}

/// djb2 hash of `bytes`, continuing from `hash`
fn djb2(hash: u32, bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(hash, |h, &b| h.wrapping_mul(33).wrapping_add(b as u32))
}

/// Image identity: the item path, and the font it is rendered with (0 for the embedded font,
/// which keeps the hash of the path alone)
fn image_hash(path: &str, font_id: u32) -> u32 {
    let hash = djb2(5381, path.as_bytes());
    if font_id == 0 {
        hash
    } else {
        djb2(hash, &font_id.to_le_bytes())
    }
}

/// Cache filename for an image: 8-char hash + .IMG (FAT 8.3 compatible)
fn cache_filename(path: &str, font_id: u32) -> String<16> {
    let mut name: String<16> = String::new();
    let _ = write!(name, "{:08X}.IMG", image_hash(path, font_id));
    name
}

/// Get orientation subdirectory name
fn orientation_dir(orientation: Orientation) -> &'static str {
    match orientation {
        Orientation::Horizontal => HORIZ_DIR,
        Orientation::Vertical => VERT_DIR,
    }
}

/// Parse cache filename to extract hash value
/// Input: ABCD1234.IMG
/// Output: hash value as u32
fn parse_cache_filename(filename: &str) -> Option<u32> {
    // FAT filesystems uppercase extensions
    let name = filename
        .strip_suffix(".IMG")
        .or_else(|| filename.strip_suffix(".img"))?;

    // Parse hex string
    u32::from_str_radix(name.trim(), 16).ok()
}

/// Read the rest of an open file
fn read_all<D, T, const A: usize, const B: usize, const C: usize>(
    file: &mut embedded_sdmmc::File<'_, D, T, A, B, C>,
) -> Result<alloc::vec::Vec<u8>, CacheError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut buf = alloc::vec![0u8; file.length() as usize];
    let mut total = 0;
    while total < buf.len() {
        match file.read(&mut buf[total..]).map_err(|_| CacheError::Read)? {
            0 => break,
            n => total += n,
        }
    }
    buf.truncate(total);
    Ok(buf)
}

/// SD card image cache
pub struct SdCache<SPI: SpiDevice, DELAY: embedded_hal::delay::DelayNs> {
    volume_mgr: VolumeManager<SdCard<SPI, DELAY>, DummyTimesource>,
    /// Identity of the font artwork is rendered with, mixed into image filenames (see
    /// [`SdCache::detect_font`])
    font_id: u32,
}

impl<SPI, DELAY> SdCache<SPI, DELAY>
where
    SPI: SpiDevice,
    DELAY: embedded_hal::delay::DelayNs,
{
    /// Create SD card and cache
    pub fn new(spi: SPI, delay: DELAY) -> Result<Self, CacheError> {
        let sd_card = SdCard::new(spi, delay);

        // Get card size to verify it's working
        match sd_card.num_bytes() {
            Ok(size) => info!("SD card size: {} MB", size / 1024 / 1024),
            Err(_) => {
                info!("Failed to read SD card size");
                return Err(CacheError::SdCard);
            }
        }

        let volume_mgr = VolumeManager::new(sd_card, DummyTimesource);

        Ok(Self {
            volume_mgr,
            font_id: 0,
        })
    }

    /// Initialize cache directory structure: /concerts/horiz/ and /concerts/vert/
    pub fn init(&mut self) -> Result<(), CacheError> {
        // Open volume (partition 0)
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        // Open root directory
        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        // Create /concerts/ if it doesn't exist
        if root_dir.open_dir(ROOT_DIR).is_err() {
            root_dir
                .make_dir_in_dir(ROOT_DIR)
                .map_err(|_| CacheError::Filesystem)?;
            info!("Created {} directory", ROOT_DIR);
        }

        // Open concerts directory
        let mut concerts_dir = root_dir
            .open_dir(ROOT_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        // Create /concerts/horiz/ if it doesn't exist
        if concerts_dir.open_dir(HORIZ_DIR).is_err() {
            concerts_dir
                .make_dir_in_dir(HORIZ_DIR)
                .map_err(|_| CacheError::Filesystem)?;
            info!("Created {}/{} directory", ROOT_DIR, HORIZ_DIR);
        }

        // Create /concerts/vert/ if it doesn't exist
        if concerts_dir.open_dir(VERT_DIR).is_err() {
            concerts_dir
                .make_dir_in_dir(VERT_DIR)
                .map_err(|_| CacheError::Filesystem)?;
            info!("Created {}/{} directory", ROOT_DIR, VERT_DIR);
        }

        info!("Cache directory structure ready");
        Ok(())
    }

    /// Check if an image is cached
    pub fn has_image(&mut self, path: &str, orientation: Orientation) -> bool {
        let filename = cache_filename(path, self.font_id);

        let Ok(mut volume) = self.volume_mgr.open_volume(VolumeIdx(0)) else {
            return false;
        };

        let Ok(mut root_dir) = volume.open_root_dir() else {
            return false;
        };

        let Ok(mut concerts_dir) = root_dir.open_dir(ROOT_DIR) else {
            return false;
        };

        let Ok(mut orient_dir) = concerts_dir.open_dir(orientation_dir(orientation)) else {
            return false;
        };

        // Try to open the file - if it succeeds, it exists
        orient_dir
            .open_file_in_dir(filename.as_str(), Mode::ReadOnly)
            .is_ok()
    }

    /// Read cached image into buffer, returns bytes read
    pub fn read_image(
        &mut self,
        path: &str,
        orientation: Orientation,
        buf: &mut [u8],
    ) -> Result<usize, CacheError> {
        let filename = cache_filename(path, self.font_id);
        let orient = orientation_dir(orientation);

        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        let mut concerts_dir = root_dir
            .open_dir(ROOT_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        let mut orient_dir = concerts_dir
            .open_dir(orient)
            .map_err(|_| CacheError::Filesystem)?;

        let mut file = orient_dir
            .open_file_in_dir(filename.as_str(), Mode::ReadOnly)
            .map_err(|_| CacheError::NotFound)?;

        let mut total_read = 0;
        loop {
            match file.read(&mut buf[total_read..]) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(_) => return Err(CacheError::Read),
            }
        }

        info!(
            "Read {} bytes from cache: {}/{}/{}",
            total_read, ROOT_DIR, orient, filename
        );
        Ok(total_read)
    }

    /// Delete a cached image
    pub fn remove_image(&mut self, path: &str, orientation: Orientation) -> Result<(), CacheError> {
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;
        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;
        let mut concerts_dir = root_dir
            .open_dir(ROOT_DIR)
            .map_err(|_| CacheError::Filesystem)?;
        let mut orient_dir = concerts_dir
            .open_dir(orientation_dir(orientation))
            .map_err(|_| CacheError::Filesystem)?;
        orient_dir
            .delete_file_in_dir(cache_filename(path, self.font_id).as_str())
            .map_err(|_| CacheError::NotFound)
    }

    /// Write image to cache
    pub fn write_image(
        &mut self,
        path: &str,
        orientation: Orientation,
        data: &[u8],
    ) -> Result<(), CacheError> {
        let filename = cache_filename(path, self.font_id);
        let orient = orientation_dir(orientation);

        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        let mut concerts_dir = root_dir
            .open_dir(ROOT_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        let mut orient_dir = concerts_dir
            .open_dir(orient)
            .map_err(|_| CacheError::Filesystem)?;

        // Create/truncate file
        let mut file = orient_dir
            .open_file_in_dir(filename.as_str(), Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| CacheError::Write)?;

        // Write data
        file.write(data).map_err(|_| CacheError::Write)?;

        info!(
            "Wrote {} bytes to cache: {}/{}/{}",
            data.len(),
            ROOT_DIR,
            orient,
            filename
        );
        Ok(())
    }

    /// Load the list of items with artwork for an orientation
    pub fn load_widget_data(&mut self, orientation: Orientation) -> Option<WidgetData> {
        let mut volume = self.volume_mgr.open_volume(VolumeIdx(0)).ok()?;
        let mut root_dir = volume.open_root_dir().ok()?;
        let mut concerts_dir = root_dir.open_dir(ROOT_DIR).ok()?;

        let mut file = concerts_dir
            .open_file_in_dir(items_file(orientation), Mode::ReadOnly)
            .ok()?;

        // Read file into buffer (128 items of up to 48 chars plus quoting)
        let mut buf = alloc::vec![0u8; 8192];
        let mut total_read = 0;
        loop {
            match file.read(&mut buf[total_read..]) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(_) => return None,
            }
        }

        // Parse JSON
        let json_str = core::str::from_utf8(&buf[..total_read]).ok()?;
        let data: WidgetData = serde_json_core::from_str(json_str).ok()?.0;

        if data.is_empty() {
            None
        } else {
            info!("Loaded {} cached widget items from JSON", data.len());
            Some(data)
        }
    }

    /// Store the list of items with artwork for an orientation
    pub fn store_widget_data(
        &mut self,
        orientation: Orientation,
        items: &WidgetData,
    ) -> Result<(), CacheError> {
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        let mut concerts_dir = root_dir
            .open_dir(ROOT_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        let mut file = concerts_dir
            .open_file_in_dir(items_file(orientation), Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| CacheError::Write)?;

        // Write JSON array manually (simple format)
        file.write(b"[").map_err(|_| CacheError::Write)?;
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                file.write(b",").map_err(|_| CacheError::Write)?;
            }
            file.write(b"\"").map_err(|_| CacheError::Write)?;
            file.write(item.as_bytes()).map_err(|_| CacheError::Write)?;
            file.write(b"\"").map_err(|_| CacheError::Write)?;
        }
        file.write(b"]").map_err(|_| CacheError::Write)?;

        info!("Stored {} widget items to cache JSON", items.len());
        Ok(())
    }

    /// Store the raw SawThat bands response
    pub fn store_bands(&mut self, json: &[u8]) -> Result<(), CacheError> {
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;
        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;
        let mut concerts_dir = root_dir
            .open_dir(ROOT_DIR)
            .map_err(|_| CacheError::Filesystem)?;
        let mut file = concerts_dir
            .open_file_in_dir(BANDS_FILE, Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| CacheError::Write)?;
        file.write(json).map_err(|_| CacheError::Write)?;
        info!("Stored {} bytes of band data", json.len());
        Ok(())
    }

    /// Load the stored SawThat bands response
    pub fn load_bands(&mut self) -> Option<alloc::vec::Vec<u8>> {
        let mut volume = self.volume_mgr.open_volume(VolumeIdx(0)).ok()?;
        let mut root_dir = volume.open_root_dir().ok()?;
        let mut concerts_dir = root_dir.open_dir(ROOT_DIR).ok()?;
        let mut file = concerts_dir
            .open_file_in_dir(BANDS_FILE, Mode::ReadOnly)
            .ok()?;
        let buf = read_all(&mut file).ok()?;
        info!("Loaded {} bytes of stored band data", buf.len());
        Some(buf)
    }

    /// Identify the font images are rendered with: the first of `names` on the SD root, by
    /// name, size and modification time, or the embedded font without one. Changing the font
    /// changes every image filename, so all artwork is rendered again.
    pub fn detect_font(&mut self, names: &[&str]) {
        let found = (|| {
            let mut volume = self.volume_mgr.open_volume(VolumeIdx(0)).ok()?;
            let mut root_dir = volume.open_root_dir().ok()?;
            names.iter().find_map(|&name| {
                let entry = root_dir.find_directory_entry(name).ok()?;
                let t = entry.mtime;
                let stamp = [
                    t.year_since_1970,
                    t.zero_indexed_month,
                    t.zero_indexed_day,
                    t.hours,
                    t.minutes,
                    t.seconds,
                ];
                let id = djb2(djb2(5381, name.as_bytes()), &entry.size.to_le_bytes());
                Some((name, djb2(id, &stamp)))
            })
        })();
        // 0 is reserved for the embedded font
        self.font_id = found.map_or(0, |(_, id)| id.max(1));
        match found {
            Some((name, id)) => info!("Artwork font: {} ({:08X})", name, id),
            None => info!("Artwork font: embedded"),
        }
    }

    /// Release the SD card for raw block access
    pub fn into_card(self) -> SdCard<SPI, DELAY> {
        self.volume_mgr.free().0
    }

    /// Create or replace a file on the SD root
    pub fn write_root_file(&mut self, name: &str, data: &[u8]) -> Result<(), CacheError> {
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;
        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;
        let mut file = root_dir
            .open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| CacheError::Write)?;
        file.write(data).map_err(|_| CacheError::Write)
    }

    /// Append to a file on the SD root, writing `header` first when it is new or empty
    pub fn append_root_file(
        &mut self,
        name: &str,
        header: &[u8],
        data: &[u8],
    ) -> Result<(), CacheError> {
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;
        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;
        let mut file = root_dir
            .open_file_in_dir(name, Mode::ReadWriteCreateOrAppend)
            .map_err(|_| CacheError::Write)?;
        if file.length() == 0 {
            file.write(header).map_err(|_| CacheError::Write)?;
        }
        file.write(data).map_err(|_| CacheError::Write)
    }

    /// Read a file on the SD root, up to `max` bytes
    pub fn read_root_file(
        &mut self,
        name: &str,
        max: usize,
    ) -> Result<alloc::vec::Vec<u8>, CacheError> {
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;
        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;
        let mut file = root_dir
            .open_file_in_dir(name, Mode::ReadOnly)
            .map_err(|_| CacheError::NotFound)?;
        if file.length() as usize > max {
            return Err(CacheError::TooLarge);
        }
        read_all(&mut file)
    }

    /// Read the orientation file of older firmware, which stored it outside `CONFIG.TXT`
    pub fn load_legacy_orientation(&mut self) -> Option<Orientation> {
        let mut volume = self.volume_mgr.open_volume(VolumeIdx(0)).ok()?;
        let mut root_dir = volume.open_root_dir().ok()?;
        let mut concerts_dir = root_dir.open_dir(ROOT_DIR).ok()?;
        let mut file = concerts_dir
            .open_file_in_dir(ORIENT_FILE, Mode::ReadOnly)
            .ok()?;
        let mut buf = [0u8; 1];
        file.read(&mut buf).ok()?;
        Some(Orientation::from_u8(buf[0]))
    }

    /// Delete the orientation file of older firmware, if present
    pub fn remove_legacy_orientation(&mut self) {
        let Ok(mut volume) = self.volume_mgr.open_volume(VolumeIdx(0)) else {
            return;
        };
        let Ok(mut root_dir) = volume.open_root_dir() else {
            return;
        };
        if let Ok(mut concerts_dir) = root_dir.open_dir(ROOT_DIR)
            && concerts_dir.delete_file_in_dir(ORIENT_FILE).is_ok()
        {
            info!("Removed {}", ORIENT_FILE);
        }
    }

    /// Remove cache entries not in the valid items list
    pub fn cleanup_stale(&mut self, valid_items: &WidgetData) -> Result<u32, CacheError> {
        // Pre-compute hashes of valid items
        let mut valid_hashes: heapless::Vec<u32, 128> = heapless::Vec::new();
        for item in valid_items.iter() {
            let _ = valid_hashes.push(image_hash(item.as_str(), self.font_id));
        }

        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        let mut concerts_dir = root_dir
            .open_dir(ROOT_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        let mut removed = 0u32;

        // Clean up stale files in both orientation directories
        for orient in [HORIZ_DIR, VERT_DIR] {
            let Ok(mut orient_dir) = concerts_dir.open_dir(orient) else {
                continue;
            };

            let mut to_delete: alloc::vec::Vec<heapless::String<16>> = alloc::vec::Vec::new();

            // Find stale files
            orient_dir
                .iterate_dir(|entry| {
                    if entry.attributes.is_archive() {
                        let name = entry.name.base_name();
                        if let Ok(name_str) = core::str::from_utf8(name) {
                            let ext = entry.name.extension();
                            let mut full_name: heapless::String<16> = heapless::String::new();
                            if let Ok(ext_str) = core::str::from_utf8(ext) {
                                if !ext_str.is_empty() && ext_str.trim() != "" {
                                    let _ =
                                        write!(full_name, "{}.{}", name_str.trim(), ext_str.trim());
                                } else {
                                    let _ = write!(full_name, "{}", name_str.trim());
                                }
                            }

                            // Remove anything that is not a current artwork file
                            if !parse_cache_filename(full_name.as_str())
                                .is_some_and(|h| valid_hashes.contains(&h))
                            {
                                to_delete.push(full_name);
                            }
                        }
                    }
                })
                .ok();

            // Delete stale files from this orientation directory
            for filename in to_delete.iter() {
                if orient_dir.delete_file_in_dir(filename.as_str()).is_ok() {
                    info!("Removed stale cache: {}/{}/{}", ROOT_DIR, orient, filename);
                    removed += 1;
                }
            }
        }

        Ok(removed)
    }
}
