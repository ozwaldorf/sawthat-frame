//! Data source traits and implementations for widgets
//!
//! Data sources fetch and transform data from external APIs into widget items.

use crate::sawthat::{self, SawThatBand};
use crate::widget::{CachePolicy, Orientation, WidgetData};
use fastly::Error;
use std::cell::RefCell;

/// SawThat user ID - configured via environment or hardcoded
/// TODO: Make this configurable via Fastly config store
const SAWTHAT_USER_ID: &str = "a320940a-b493-4515-9f25-d393ebb540e6";

/// A data source that provides widget items
pub trait DataSource {
    /// The name of this widget (used in URL routing)
    fn name(&self) -> &'static str;

    /// Cache policy for the widget data (list of items)
    fn data_cache_policy(&self) -> CachePolicy;

    /// Fetch widget data from the source
    fn fetch_data(&self) -> Result<WidgetData, Error>;

    /// Fetch and process an image for a widget item
    fn fetch_image(&self, path: &str, orientation: Orientation) -> Result<Vec<u8>, Error>;
}

/// Concert data source - fetches concert history from SawThat.band
pub struct ConcertDataSource {
    /// Cached bands data (to avoid re-fetching for image requests)
    bands_cache: RefCell<Option<Vec<SawThatBand>>>,
}

impl ConcertDataSource {
    pub fn new() -> Self {
        Self {
            bands_cache: RefCell::new(None),
        }
    }

    /// Get bands, fetching from API if not cached
    fn get_bands(&self) -> Result<Vec<SawThatBand>, Error> {
        // Check cache first
        if let Some(bands) = self.bands_cache.borrow().as_ref() {
            return Ok(bands.clone());
        }

        // Fetch from API
        let bands = sawthat::fetch_bands(SAWTHAT_USER_ID)?;

        // Cache for subsequent image requests
        *self.bands_cache.borrow_mut() = Some(bands.clone());

        Ok(bands)
    }
}

impl Default for ConcertDataSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DataSource for ConcertDataSource {
    fn name(&self) -> &'static str {
        "concerts"
    }

    fn data_cache_policy(&self) -> CachePolicy {
        // Refresh concert list daily (new concerts might be added)
        CachePolicy::Ttl(86400)
    }

    fn fetch_data(&self) -> Result<WidgetData, Error> {
        let bands = self.get_bands()?;

        // Convert to widget items (most recent concerts first)
        let items = sawthat::bands_to_widget_items(&bands, 100);

        if items.is_empty() {
            log::warn!("No concerts found in SawThat data");
        } else {
            log::info!("Generated {} concert widget items", items.len());
        }

        Ok(items)
    }

    fn fetch_image(&self, path: &str, orientation: Orientation) -> Result<Vec<u8>, Error> {
        // Path format: {band_id}/{date}
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        let band_id = parts.first().ok_or_else(|| Error::msg("Invalid path: missing band_id"))?;
        let date = parts.get(1).map(|d| urlencoding::decode(d).unwrap_or_default().into_owned());

        log::info!("Fetching image for band_id: {}, date: {:?}", band_id, date);

        let bands = self.get_bands()?;
        sawthat::fetch_band_image(&bands, band_id, date.as_deref(), orientation)
    }
}

/// Registry of available data sources
pub struct DataSourceRegistry {
    sources: Vec<Box<dyn DataSource>>,
}

impl DataSourceRegistry {
    pub fn new() -> Self {
        Self {
            sources: vec![
                Box::new(ConcertDataSource::new()),
            ],
        }
    }

    pub fn get(&self, name: &str) -> Option<&dyn DataSource> {
        self.sources.iter().find(|s| s.name() == name).map(|s| s.as_ref())
    }
}

impl Default for DataSourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

