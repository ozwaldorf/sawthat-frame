//! Data source traits and implementations for widgets
//!
//! Data sources fetch and transform data from external APIs into widget items.

use crate::error::AppError;
use crate::sawthat::{self, SawThatBand};
use crate::widget::{CachePolicy, Orientation, WidgetData, WidgetName};
use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::RwLock;

/// SawThat user ID - configured via environment or hardcoded
/// TODO: Make this configurable via environment variable
const SAWTHAT_USER_ID: &str = "a320940a-b493-4515-9f25-d393ebb540e6";

/// A data source that provides widget items
#[async_trait]
pub trait DataSource: Send + Sync {
    /// Cache policy for the widget data (list of items)
    fn data_cache_policy(&self) -> CachePolicy;

    /// Fetch widget data from the source
    async fn fetch_data(&self) -> Result<WidgetData, AppError>;

    /// Fetch and process an image for a widget item
    async fn fetch_image(&self, path: &str, orientation: Orientation) -> Result<Vec<u8>, AppError>;
}

/// Concert data source - fetches concert history from SawThat.band
pub struct ConcertDataSource {
    client: Client,
    /// Cached bands data (to avoid re-fetching for image requests)
    bands_cache: Arc<RwLock<Option<Vec<SawThatBand>>>>,
}

impl ConcertDataSource {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            bands_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Get bands, fetching from API if not cached
    async fn get_bands(&self) -> Result<Vec<SawThatBand>, AppError> {
        // Check cache first
        {
            let cache = self.bands_cache.read().await;
            if let Some(bands) = cache.as_ref() {
                return Ok(bands.clone());
            }
        }

        // Fetch from API
        let bands = sawthat::fetch_bands(&self.client, SAWTHAT_USER_ID).await?;

        // Cache for subsequent image requests
        {
            let mut cache = self.bands_cache.write().await;
            *cache = Some(bands.clone());
        }

        Ok(bands)
    }
}

#[async_trait]
impl DataSource for ConcertDataSource {
    fn data_cache_policy(&self) -> CachePolicy {
        // Refresh concert list daily (new concerts might be added)
        CachePolicy::Ttl(86400)
    }

    async fn fetch_data(&self) -> Result<WidgetData, AppError> {
        let bands = self.get_bands().await?;

        // Convert to widget items (most recent concerts first)
        let items = sawthat::bands_to_widget_items(&bands, 100);

        if items.is_empty() {
            tracing::warn!("No concerts found in SawThat data");
        } else {
            tracing::info!("Generated {} concert widget items", items.len());
        }

        Ok(items)
    }

    async fn fetch_image(&self, path: &str, orientation: Orientation) -> Result<Vec<u8>, AppError> {
        // Path format: {band_id}/{date}
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        let band_id = parts.first().ok_or_else(|| AppError::InvalidPath("missing band_id".to_string()))?;
        let date = parts.get(1).map(|d| urlencoding::decode(d).unwrap_or_default().into_owned());

        tracing::info!("Fetching image for band_id: {}, date: {:?}", band_id, date);

        let bands = self.get_bands().await?;
        sawthat::fetch_band_image(&self.client, &bands, band_id, date.as_deref(), orientation).await
    }
}

/// Registry of available data sources
pub struct DataSourceRegistry {
    concerts: Arc<ConcertDataSource>,
}

impl DataSourceRegistry {
    pub fn new(client: Client) -> Self {
        Self {
            concerts: Arc::new(ConcertDataSource::new(client)),
        }
    }

    pub fn get(&self, name: WidgetName) -> Arc<dyn DataSource> {
        match name {
            WidgetName::Concerts => self.concerts.clone(),
        }
    }
}
