//! SD card-based image cache
//!
//! Stores PNG images directly on the SD card's FAT filesystem.
//! Filenames encode the item path and orientation for easy lookup.
//!
//! Directory structure:
//! /cache/
//!   YYYY-MM-DD-horiz-band-id.png   - horizontal orientation image
//!   YYYY-MM-DD-vert-band-id.png    - vertical orientation image
//!
//! Item paths can be reconstructed from filenames by removing orientation suffix.

use core::fmt::Write as FmtWrite;

use embedded_hal::spi::SpiDevice;
use embedded_sdmmc::{Mode, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_println::println;
use heapless::String;

use crate::widget::{Orientation, WidgetData, MAX_PATH_LEN};

/// Cache directory name
const CACHE_DIR: &str = "cache";

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
/// Format: {item_path}-{orientation}.png
/// Example: 2024-06-15-horiz-band-id.png
fn cache_filename(path: &str, orientation: Orientation) -> String<64> {
    let suffix = match orientation {
        Orientation::Horizontal => "horiz",
        Orientation::Vertical => "vert",
    };
    let mut name: String<64> = String::new();
    // Insert orientation before band-id: YYYY-MM-DD-{orientation}-band-id.png
    // Path format: YYYY-MM-DD-band-id
    // We need to insert orientation after the date part (first 10 chars)
    if path.len() >= 10 {
        let _ = write!(name, "{}-{}-{}.png", &path[..10], suffix, &path[11..]);
    } else {
        let _ = write!(name, "{}-{}.png", path, suffix);
    }
    name
}

/// Parse cache filename to extract item path
/// Input: 2024-06-15-horiz-band-id.png
/// Output: 2024-06-15-band-id
fn parse_cache_filename(filename: &str) -> Option<String<MAX_PATH_LEN>> {
    // Remove .png suffix
    let name = filename.strip_suffix(".PNG").or_else(|| filename.strip_suffix(".png"))?;

    // Find orientation in the middle (after YYYY-MM-DD-)
    // Format: YYYY-MM-DD-{horiz|vert}-band-id
    if name.len() < 16 {
        return None;
    }

    let date_part = &name[..10]; // YYYY-MM-DD
    let rest = &name[11..]; // {horiz|vert}-band-id

    // Find orientation and extract band-id
    let band_id = if let Some(stripped) = rest.strip_prefix("horiz-") {
        stripped
    } else if let Some(stripped) = rest.strip_prefix("vert-") {
        stripped
    } else {
        return None;
    };

    // Reconstruct item path: YYYY-MM-DD-band-id
    let mut path: String<MAX_PATH_LEN> = String::new();
    if write!(path, "{}-{}", date_part, band_id).is_ok() {
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

    /// Initialize cache directory
    pub fn init(&mut self) -> Result<(), CacheError> {
        // Open volume (partition 0)
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        // Open root directory
        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        // Check if cache directory exists by trying to open it
        let dir_exists = root_dir.open_dir(CACHE_DIR).is_ok();

        if dir_exists {
            println!("Cache directory found");
        } else {
            // Create cache directory
            root_dir
                .make_dir_in_dir(CACHE_DIR)
                .map_err(|_| CacheError::Filesystem)?;
            println!("Created cache directory");
        }

        // Volume, dirs are dropped automatically
        Ok(())
    }

    /// Check if an image is cached
    pub fn has_image(&mut self, path: &str, orientation: Orientation) -> bool {
        let filename = cache_filename(path, orientation);

        let Ok(mut volume) = self.volume_mgr.open_volume(VolumeIdx(0)) else {
            return false;
        };

        let Ok(mut root_dir) = volume.open_root_dir() else {
            return false;
        };

        let Ok(mut cache_dir) = root_dir.open_dir(CACHE_DIR) else {
            return false;
        };

        // Try to open the file - if it succeeds, it exists
        cache_dir
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
        let filename = cache_filename(path, orientation);

        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        let mut cache_dir = root_dir
            .open_dir(CACHE_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        let mut file = cache_dir
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

        println!("Read {} bytes from cache: {}", total_read, filename);
        Ok(total_read)
    }

    /// Write image to cache
    pub fn write_image(
        &mut self,
        path: &str,
        orientation: Orientation,
        data: &[u8],
    ) -> Result<(), CacheError> {
        let filename = cache_filename(path, orientation);

        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        let mut cache_dir = root_dir
            .open_dir(CACHE_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        // Create/truncate file
        let mut file = cache_dir
            .open_file_in_dir(filename.as_str(), Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| CacheError::Write)?;

        // Write data
        file.write(data).map_err(|_| CacheError::Write)?;

        println!("Wrote {} bytes to cache: {}", data.len(), filename);
        Ok(())
    }

    /// Load widget data from cache (JSON array of item paths)
    pub fn load_widget_data(&mut self) -> Option<WidgetData> {
        let mut volume = self.volume_mgr.open_volume(VolumeIdx(0)).ok()?;
        let mut root_dir = volume.open_root_dir().ok()?;
        let mut cache_dir = root_dir.open_dir(CACHE_DIR).ok()?;

        let mut file = cache_dir
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

        let mut cache_dir = root_dir
            .open_dir(CACHE_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        let mut file = cache_dir
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

    /// List all cached items (unique item paths from directory listing)
    pub fn list_cached_items(&mut self) -> WidgetData {
        let mut items = WidgetData::new();

        let Ok(mut volume) = self.volume_mgr.open_volume(VolumeIdx(0)) else {
            return items;
        };

        let Ok(mut root_dir) = volume.open_root_dir() else {
            return items;
        };

        let Ok(mut cache_dir) = root_dir.open_dir(CACHE_DIR) else {
            return items;
        };

        // Collect unique item paths from cache files
        cache_dir
            .iterate_dir(|entry| {
                if entry.attributes.is_archive() {
                    // Convert ShortFileName to string
                    let name = entry.name.base_name();
                    if let Ok(name_str) = core::str::from_utf8(name) {
                        // Get full filename including extension
                        let ext = entry.name.extension();
                        let mut full_name: String<64> = String::new();
                        if let Ok(ext_str) = core::str::from_utf8(ext) {
                            if !ext_str.is_empty() && ext_str.trim() != "" {
                                let _ = write!(full_name, "{}.{}", name_str.trim(), ext_str.trim());
                            } else {
                                let _ = write!(full_name, "{}", name_str.trim());
                            }
                        }

                        // Parse filename to extract item path
                        if let Some(path) = parse_cache_filename(full_name.as_str()) {
                            // Check if we already have this path (avoid duplicates from horiz/vert)
                            if !items.iter().any(|p| p.as_str() == path.as_str()) {
                                let _ = items.push(path);
                            }
                        }
                    }
                }
            })
            .ok();

        println!("Found {} cached items", items.len());
        items
    }

    /// Remove cache entries not in the valid items list
    pub fn cleanup_stale(&mut self, valid_items: &WidgetData) -> Result<u32, CacheError> {
        let mut volume = self
            .volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| CacheError::Filesystem)?;

        let mut root_dir = volume.open_root_dir().map_err(|_| CacheError::Filesystem)?;

        let mut cache_dir = root_dir
            .open_dir(CACHE_DIR)
            .map_err(|_| CacheError::Filesystem)?;

        let mut removed = 0u32;
        let mut to_delete: heapless::Vec<heapless::String<64>, 64> = heapless::Vec::new();

        // Find stale files
        cache_dir
            .iterate_dir(|entry| {
                if entry.attributes.is_archive() {
                    let name = entry.name.base_name();
                    if let Ok(name_str) = core::str::from_utf8(name) {
                        let ext = entry.name.extension();
                        let mut full_name: heapless::String<64> = heapless::String::new();
                        if let Ok(ext_str) = core::str::from_utf8(ext) {
                            if !ext_str.is_empty() && ext_str.trim() != "" {
                                let _ = write!(full_name, "{}.{}", name_str.trim(), ext_str.trim());
                            } else {
                                let _ = write!(full_name, "{}", name_str.trim());
                            }
                        }

                        // Parse to get item path and check if valid
                        if let Some(path) = parse_cache_filename(full_name.as_str()) {
                            if !valid_items.iter().any(|p| p.as_str() == path.as_str()) {
                                let _ = to_delete.push(full_name);
                            }
                        }
                    }
                }
            })
            .ok();

        // Delete stale files
        for filename in to_delete.iter() {
            if cache_dir.delete_file_in_dir(filename.as_str()).is_ok() {
                println!("Removed stale cache: {}", filename);
                removed += 1;
            }
        }

        Ok(removed)
    }
}
