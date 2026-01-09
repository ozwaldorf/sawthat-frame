//! SD card-based image cache
//!
//! Stores PNG images directly on the SD card's FAT filesystem.
//! Directory structure mirrors the API paths:
//!
//! /concerts/
//!   widget.json              - JSON array of item paths
//!   horiz/
//!     {item-path}.png        - horizontal orientation images
//!   vert/
//!     {item-path}.png        - vertical orientation images

use core::fmt::Write as FmtWrite;

use embedded_hal::spi::SpiDevice;
use embedded_sdmmc::{Mode, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_println::println;
use heapless::String;

use crate::widget::{Orientation, WidgetData, MAX_PATH_LEN};

/// Root directory (mirrors API path)
const ROOT_DIR: &str = "concerts";

/// Horizontal orientation subdirectory
const HORIZ_DIR: &str = "horiz";

/// Vertical orientation subdirectory
const VERT_DIR: &str = "vert";

/// Widget data filename (JSON array of item paths)
const WIDGET_FILE: &str = "widget.json";

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

/// Generate cache filename for an image
/// Format: {item_path}.png (orientation is in directory path)
/// Example: 2024-06-15-band-id.png
fn cache_filename(path: &str) -> String<64> {
    let mut name: String<64> = String::new();
    let _ = write!(name, "{}.png", path);
    name
}

/// Get orientation subdirectory name
fn orientation_dir(orientation: Orientation) -> &'static str {
    match orientation {
        Orientation::Horizontal => HORIZ_DIR,
        Orientation::Vertical => VERT_DIR,
    }
}

/// Parse cache filename to extract item path
/// Input: 2024-06-15-band-id.png
/// Output: 2024-06-15-band-id
fn parse_cache_filename(filename: &str) -> Option<String<MAX_PATH_LEN>> {
    // Remove .png suffix (FAT filesystems uppercase extensions)
    let name = filename
        .strip_suffix(".PNG")
        .or_else(|| filename.strip_suffix(".png"))?;

    let mut path: String<MAX_PATH_LEN> = String::new();
    if path.push_str(name).is_ok() {
        Some(path)
    } else {
        None
    }
}

/// SD card image cache
pub struct SdCache<SPI: SpiDevice, DELAY: embedded_hal::delay::DelayNs> {
    volume_mgr: VolumeManager<SdCard<SPI, DELAY>, DummyTimesource>,
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
            Ok(size) => println!("SD card size: {} MB", size / 1024 / 1024),
            Err(_) => {
                println!("Failed to read SD card size");
                return Err(CacheError::SdCard);
            }
        }

        let volume_mgr = VolumeManager::new(sd_card, DummyTimesource);

        Ok(Self { volume_mgr })
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
            println!("Created {} directory", ROOT_DIR);
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
            println!("Created {}/{} directory", ROOT_DIR, HORIZ_DIR);
        }

        // Create /concerts/vert/ if it doesn't exist
        if concerts_dir.open_dir(VERT_DIR).is_err() {
            concerts_dir
                .make_dir_in_dir(VERT_DIR)
                .map_err(|_| CacheError::Filesystem)?;
            println!("Created {}/{} directory", ROOT_DIR, VERT_DIR);
        }

        println!("Cache directory structure ready");
        Ok(())
    }

    /// Check if an image is cached
    pub fn has_image(&mut self, path: &str, orientation: Orientation) -> bool {
        let filename = cache_filename(path);

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
        let filename = cache_filename(path);
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

        println!(
            "Read {} bytes from cache: {}/{}/{}",
            total_read, ROOT_DIR, orient, filename
        );
        Ok(total_read)
    }

    /// Write image to cache
    pub fn write_image(
        &mut self,
        path: &str,
        orientation: Orientation,
        data: &[u8],
    ) -> Result<(), CacheError> {
        let filename = cache_filename(path);
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

        println!(
            "Wrote {} bytes to cache: {}/{}/{}",
            data.len(),
            ROOT_DIR,
            orient,
            filename
        );
        Ok(())
    }

    /// Load widget data from cache (JSON array of item paths)
    pub fn load_widget_data(&mut self) -> Option<WidgetData> {
        let mut volume = self.volume_mgr.open_volume(VolumeIdx(0)).ok()?;
        let mut root_dir = volume.open_root_dir().ok()?;
        let mut concerts_dir = root_dir.open_dir(ROOT_DIR).ok()?;

        let mut file = concerts_dir
            .open_file_in_dir(WIDGET_FILE, Mode::ReadOnly)
            .ok()?;

        // Read file into buffer (max ~6KB for 128 items)
        let mut buf = [0u8; 6144];
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
            println!("Loaded {} cached widget items from JSON", data.len());
            Some(data)
        }
    }

    /// Store widget data to cache (JSON array of item paths)
    pub fn store_widget_data(&mut self, items: &WidgetData) -> Result<(), CacheError> {
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        let mut concerts_dir = root_dir
            .open_dir(ROOT_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        let mut file = concerts_dir
            .open_file_in_dir(WIDGET_FILE, Mode::ReadWriteCreateOrTruncate)
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

        println!("Stored {} widget items to cache JSON", items.len());
        Ok(())
    }

    /// Remove cache entries not in the valid items list
    pub fn cleanup_stale(&mut self, valid_items: &WidgetData) -> Result<u32, CacheError> {
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

            let mut to_delete: heapless::Vec<heapless::String<64>, 64> = heapless::Vec::new();

            // Find stale files
            orient_dir
                .iterate_dir(|entry| {
                    if entry.attributes.is_archive() {
                        let name = entry.name.base_name();
                        if let Ok(name_str) = core::str::from_utf8(name) {
                            let ext = entry.name.extension();
                            let mut full_name: heapless::String<64> = heapless::String::new();
                            if let Ok(ext_str) = core::str::from_utf8(ext) {
                                if !ext_str.is_empty() && ext_str.trim() != "" {
                                    let _ =
                                        write!(full_name, "{}.{}", name_str.trim(), ext_str.trim());
                                } else {
                                    let _ = write!(full_name, "{}", name_str.trim());
                                }
                            }

                            // Parse to get item path and check if valid
                            if let Some(path) = parse_cache_filename(full_name.as_str())
                                && !valid_items.iter().any(|p| p.as_str() == path.as_str()) {
                                    let _ = to_delete.push(full_name);
                                }
                        }
                    }
                })
                .ok();

            // Delete stale files from this orientation directory
            for filename in to_delete.iter() {
                if orient_dir.delete_file_in_dir(filename.as_str()).is_ok() {
                    println!("Removed stale cache: {}/{}/{}", ROOT_DIR, orient, filename);
                    removed += 1;
                }
            }
        }

        Ok(removed)
    }
}
