//! In-memory cache with TTL expiration
//!
//! Provides concert data caching with 24-hour expiration.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::sawthat::SawThatBand;
use crate::widget::Orientation;

/// TTL for all cache entries (24 hours)
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A cached entry with expiration time
struct CacheEntry<V> {
    value: V,
    expires_at: Instant,
}

impl<V> CacheEntry<V> {
    fn new(value: V) -> Self {
        Self {
            value,
            expires_at: Instant::now() + CACHE_TTL,
        }
    }

    fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }
}

/// Cached data for a single concert
#[derive(Clone)]
pub struct ConcertEntry {
    /// Band/artist name
    pub band_name: String,
    /// Venue and location
    pub venue: String,
    /// Formatted date string (e.g., "July 17th, 2025")
    pub formatted_date: String,
    /// Source image bytes (for rendering other orientations)
    pub source_image: Arc<Vec<u8>>,
    /// Primary color extracted from image
    pub primary_color: PrimaryColor,
    /// Rendered horizontal image
    pub image_horiz: Option<Arc<Vec<u8>>>,
    /// Rendered vertical image
    pub image_vert: Option<Arc<Vec<u8>>>,
}

impl ConcertEntry {
    /// Get rendered image for orientation if cached
    pub fn get_image(&self, orientation: Orientation) -> Option<&Arc<Vec<u8>>> {
        match orientation {
            Orientation::Horiz => self.image_horiz.as_ref(),
            Orientation::Vert => self.image_vert.as_ref(),
        }
    }

    /// Set rendered image for orientation
    pub fn set_image(&mut self, orientation: Orientation, image: Arc<Vec<u8>>) {
        match orientation {
            Orientation::Horiz => self.image_horiz = Some(image),
            Orientation::Vert => self.image_vert = Some(image),
        }
    }
}

/// Primary color with RGB values and lightness info
#[derive(Clone, Copy)]
pub struct PrimaryColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub is_light: bool,
}

/// Concert cache holding all cached data
pub struct ConcertCache {
    /// Cached bands list from SawThat API
    bands: RwLock<Option<CacheEntry<Vec<SawThatBand>>>>,
    /// Cached concert entries keyed by "{band_id}/{date}"
    concerts: RwLock<HashMap<String, CacheEntry<ConcertEntry>>>,
}

impl ConcertCache {
    pub fn new() -> Self {
        Self {
            bands: RwLock::new(None),
            concerts: RwLock::new(HashMap::new()),
        }
    }

    /// Get cached bands list if not expired
    pub async fn get_bands(&self) -> Option<Vec<SawThatBand>> {
        let cache = self.bands.read().await;
        cache.as_ref().and_then(|entry| {
            if entry.is_expired() {
                None
            } else {
                Some(entry.value.clone())
            }
        })
    }

    /// Store bands list in cache
    pub async fn set_bands(&self, bands: Vec<SawThatBand>) {
        let mut cache = self.bands.write().await;
        *cache = Some(CacheEntry::new(bands));
    }

    /// Get cached concert entry if not expired
    pub async fn get_concert(&self, key: &str) -> Option<ConcertEntry> {
        let cache = self.concerts.read().await;
        cache.get(key).and_then(|entry| {
            if entry.is_expired() {
                None
            } else {
                Some(entry.value.clone())
            }
        })
    }

    /// Store a concert entry, only if no entry exists (or existing is expired)
    ///
    /// If an entry already exists, keeps the existing one to preserve any
    /// rendered images from concurrent requests.
    pub async fn set_or_update_concert(&self, key: String, entry: ConcertEntry) {
        let mut cache = self.concerts.write().await;
        match cache.get(&key) {
            Some(existing) if !existing.is_expired() => {
                // Entry exists and is valid - don't overwrite
            }
            _ => {
                // No entry or expired - insert new one
                cache.insert(key, CacheEntry::new(entry));
            }
        }
    }

    /// Update a concert entry's rendered image for a specific orientation
    pub async fn set_concert_image(
        &self,
        key: &str,
        orientation: Orientation,
        image: Arc<Vec<u8>>,
    ) {
        let mut cache = self.concerts.write().await;
        if let Some(entry) = cache.get_mut(key) {
            if !entry.is_expired() {
                entry.value.set_image(orientation, image);
            }
        }
    }
}

impl Default for ConcertCache {
    fn default() -> Self {
        Self::new()
    }
}
