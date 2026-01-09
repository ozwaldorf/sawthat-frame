//! Data source traits and implementations for widgets
//!
//! Data sources fetch and transform data from external APIs into widget items.

use crate::cache::ConcertCache;
use crate::error::AppError;
use crate::sawthat::{self, SawThatBand};
use crate::widget::{CachePolicy, Orientation, WidgetData, WidgetName};
use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;

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
    /// In-memory cache with 24-hour TTL
    cache: Arc<ConcertCache>,
}

impl ConcertDataSource {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            cache: Arc::new(ConcertCache::new()),
        }
    }

    /// Get bands, fetching from API if not cached
    async fn get_bands(&self) -> Result<Vec<SawThatBand>, AppError> {
        // Check cache first
        if let Some(bands) = self.cache.get_bands().await {
            tracing::debug!("Using cached bands data");
            return Ok(bands);
        }

        // Fetch from API
        tracing::info!("Fetching bands from API (cache miss)");
        let bands = sawthat::fetch_bands(&self.client, SAWTHAT_USER_ID).await?;

        // Cache for subsequent requests
        self.cache.set_bands(bands.clone()).await;

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
        let items = sawthat::bands_to_widget_items(&bands, 128);

        if items.is_empty() {
            tracing::warn!("No concerts found in SawThat data");
        } else {
            tracing::info!("Generated {} concert widget items", items.len());
        }

        Ok(items)
    }

    async fn fetch_image(&self, path: &str, orientation: Orientation) -> Result<Vec<u8>, AppError> {
        // Path format: YYYY-MM-DD-band-id
        let (band_id, date) = sawthat::parse_item_path(path)
            .ok_or_else(|| AppError::InvalidPath(format!("invalid path format: {}", path)))?;

        // Check concert cache for existing rendered image
        if let Some(entry) = self.cache.get_concert(path).await {
            if let Some(cached_image) = entry.get_image(orientation) {
                tracing::debug!("Using cached image for {} ({:?})", path, orientation);
                return Ok((**cached_image).clone());
            }
        }

        tracing::info!(
            "Fetching image for band_id: {}, date: {} (cache miss)",
            band_id,
            date
        );

        let bands = self.get_bands().await?;
        let image = sawthat::fetch_band_image(
            &self.client,
            &bands,
            &band_id,
            Some(&date),
            orientation,
            path,
            &self.cache,
        )
        .await?;

        Ok(image)
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
