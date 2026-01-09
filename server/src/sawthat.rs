//! SawThat.band API integration
//!
//! Fetches concert history from sawthat.band API and generates widget items.
//! Uses Deezer API to find album art matching each concert date.

use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;

use crate::cache::{ConcertCache, ConcertEntry};
use crate::deezer;
use crate::error::AppError;
use crate::image_processing;
use crate::text::ConcertInfo;
use crate::widget::{Orientation, WidgetData, WidgetWidth};

/// SawThat API base URL
const SAWTHAT_API_URL: &str = "https://server.sawthat.band/api/bands";

/// A band from the SawThat API
#[derive(Debug, Clone, Deserialize)]
pub struct SawThatBand {
    /// Band/artist name
    pub band: String,
    /// Spotify image URL
    pub picture: String,
    /// Concert history
    pub concerts: Vec<SawThatConcert>,
    /// Band UUID
    pub id: String,
    // Note: genre and user_id fields exist in API but are ignored
}

/// A concert from the SawThat API
#[derive(Debug, Clone, Deserialize)]
pub struct SawThatConcert {
    /// Date in DD-MM-YYYY format
    pub date: String,
    /// Venue and location
    pub location: String,
}

/// Fetch bands from SawThat API
pub async fn fetch_bands(client: &Client, user_id: &str) -> Result<Vec<SawThatBand>, AppError> {
    let url = format!("{}?id={}", SAWTHAT_API_URL, user_id);

    tracing::info!("Fetching SawThat bands from: {}", url);

    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(AppError::ExternalApi(format!(
            "SawThat API returned status: {}",
            response.status()
        )));
    }

    let bands: Vec<SawThatBand> = response.json().await?;

    tracing::info!("Fetched {} bands from SawThat", bands.len());

    Ok(bands)
}

/// Convert SawThat bands to widget items
///
/// Returns all concerts sorted by date (most recent first).
pub fn bands_to_widget_items(bands: &[SawThatBand], limit: usize) -> WidgetData {
    // Flatten all concerts from all bands
    let mut all_concerts: Vec<_> = bands
        .iter()
        .flat_map(|band| {
            band.concerts.iter().map(move |concert| {
                let date_parts: Vec<&str> = concert.date.split('-').collect();
                let sort_key = if date_parts.len() == 3 {
                    // Convert DD-MM-YYYY to YYYYMMDD for sorting
                    format!(
                        "{}{}{}",
                        date_parts[2], // year
                        date_parts[1], // month
                        date_parts[0]  // day
                    )
                } else {
                    concert.date.clone()
                };
                (band, concert, sort_key)
            })
        })
        .collect();

    // Sort by date descending (most recent first)
    all_concerts.sort_by(|a, b| b.2.cmp(&a.2));

    // Take the most recent concerts
    all_concerts
        .into_iter()
        .take(limit)
        .map(|(band, concert, _)| format!("{}/{}", band.id, urlencoding::encode(&concert.date)))
        .collect()
}

/// Fetch and process an image for a band
///
/// Uses cached data when available. Caches:
/// - Resolved image URL (Deezer or Spotify fallback)
/// - Source image bytes
/// - Primary color
/// - Rendered images per orientation
pub async fn fetch_band_image(
    client: &Client,
    bands: &[SawThatBand],
    band_id: &str,
    date: Option<&str>,
    orientation: Orientation,
    cache_key: &str,
    cache: &ConcertCache,
) -> Result<Vec<u8>, AppError> {
    // Check if we have a cached entry
    if let Some(entry) = cache.get_concert(cache_key).await {
        // Check if we have this orientation's image
        if let Some(cached_image) = entry.get_image(orientation) {
            tracing::debug!(
                "Using fully cached image for {} ({:?})",
                cache_key,
                orientation
            );
            return Ok((**cached_image).clone());
        }

        // We have cached data but need to render this orientation
        tracing::info!(
            "Rendering {:?} for {} using cached data",
            orientation,
            cache_key
        );
        let (target_width, target_height) = orientation.dimensions(WidgetWidth::Half);
        let rendered = image_processing::process_image_with_color(
            &entry.source_image,
            target_width,
            target_height,
            Some(&ConcertInfo {
                band_name: entry.band_name.clone(),
                date: entry.formatted_date.clone(),
                venue: entry.venue.clone(),
            }),
            &entry.primary_color,
        )?;

        // Cache this orientation
        cache
            .set_concert_image(cache_key, orientation, Arc::new(rendered.clone()))
            .await;

        return Ok(rendered);
    }

    // No cached entry - fetch everything from scratch
    let band = bands
        .iter()
        .find(|b| b.id == band_id)
        .ok_or_else(|| AppError::BandNotFound(band_id.to_string()))?;

    // Resolve image URL (Deezer or fallback)
    let image_url = resolve_image_url(client, band, date).await;

    // Fetch the source image
    tracing::info!("Fetching source image from: {}", image_url);
    let response = client
        .get(&image_url)
        .header("Accept", "image/*")
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(AppError::ExternalApi(format!(
            "Failed to fetch image: {}",
            response.status()
        )));
    }
    let source_image = Arc::new(response.bytes().await?.to_vec());

    // Extract primary color
    let primary_color = image_processing::extract_primary_color(&source_image)?;

    // Build concert info
    let (formatted_date, venue) = date
        .and_then(|d| {
            band.concerts
                .iter()
                .find(|c| c.date == d)
                .map(|c| (format_date(&c.date), c.location.clone()))
        })
        .unwrap_or_else(|| ("".to_string(), "".to_string()));

    // Create and cache the entry data
    cache
        .set_or_update_concert(
            cache_key.to_string(),
            ConcertEntry {
                band_name: band.band.clone(),
                venue: venue.clone(),
                formatted_date: formatted_date.clone(),
                source_image: source_image.clone(),
                primary_color,
                image_horiz: None,
                image_vert: None,
            },
        )
        .await;

    // Render the image
    let (target_width, target_height) = orientation.dimensions(WidgetWidth::Half);
    let rendered = image_processing::process_image_with_color(
        &source_image,
        target_width,
        target_height,
        Some(&ConcertInfo {
            band_name: band.band.clone(),
            date: formatted_date.clone(),
            venue: venue.clone(),
        }),
        &primary_color,
    )?;

    // Add the rendered image
    cache
        .set_concert_image(cache_key, orientation, Arc::new(rendered.clone()))
        .await;

    Ok(rendered)
}

/// Resolve the image URL for a band/concert
///
/// Tries Deezer album art first, falls back to Spotify picture.
async fn resolve_image_url(client: &Client, band: &SawThatBand, date: Option<&str>) -> String {
    if let Some(concert_date) = date {
        match deezer::fetch_album_art_for_concert(client, &band.band, concert_date).await {
            Ok(Some(url)) => {
                tracing::info!(
                    "Using Deezer album art for {} at {}: {}",
                    band.band,
                    concert_date,
                    url
                );
                return url;
            }
            Ok(None) => {
                tracing::info!(
                    "No Deezer album found for {} at {}, using Spotify picture",
                    band.band,
                    concert_date
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Deezer API error for {} at {}: {}, using Spotify picture",
                    band.band,
                    concert_date,
                    e
                );
            }
        }
    } else {
        tracing::info!("No date provided for {}, using Spotify picture", band.band);
    }

    band.picture.clone()
}

/// Format date from DD-MM-YYYY to "Month DDth, YYYY" (e.g., "July 17th, 2025")
fn format_date(date: &str) -> String {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() == 3 {
        let day: u32 = parts[0].parse().unwrap_or(0);
        let month = match parts[1] {
            "01" => "January",
            "02" => "February",
            "03" => "March",
            "04" => "April",
            "05" => "May",
            "06" => "June",
            "07" => "July",
            "08" => "August",
            "09" => "September",
            "10" => "October",
            "11" => "November",
            "12" => "December",
            _ => return date.to_string(),
        };
        let suffix = match day {
            1 | 21 | 31 => "st",
            2 | 22 => "nd",
            3 | 23 => "rd",
            _ => "th",
        };
        let year = parts[2];
        format!("{} {}{}, {}", month, day, suffix, year)
    } else {
        date.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bands_to_widget_items() {
        let bands = vec![SawThatBand {
            band: "Test Band".to_string(),
            picture: "https://example.com/image.jpg".to_string(),
            concerts: vec![SawThatConcert {
                date: "15-06-2024".to_string(),
                location: "Test Venue".to_string(),
            }],
            id: "test-id".to_string(),
        }];

        let items = bands_to_widget_items(&bands, 10);
        assert_eq!(items.len(), 1);
        assert!(items[0].contains("test-id"));
    }
}
