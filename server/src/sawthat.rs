//! SawThat.band API integration
//!
//! Fetches concert history from sawthat.band API and generates widget items.
//! Uses Deezer API to find album art matching each concert date.

use reqwest::Client;
use serde::Deserialize;

use crate::deezer;
use crate::error::AppError;
use crate::image_processing;
use crate::text::ConcertInfo;
use crate::widget::{Orientation, WidgetData, WidgetItem, WidgetWidth};

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
        .map(|(band, concert, _)| {
            let cache_key = hash_concert(&band.id, &concert.date);
            let path = format!("{}/{}", band.id, urlencoding::encode(&concert.date));

            WidgetItem {
                width: WidgetWidth::Full,
                cache_key,
                path,
            }
        })
        .collect()
}

/// Fetch and process an image for a band
///
/// Tries to find album art from Deezer matching the concert date,
/// falls back to the band's Spotify picture if not found.
pub async fn fetch_band_image(
    client: &Client,
    bands: &[SawThatBand],
    band_id: &str,
    date: Option<&str>,
    orientation: Orientation,
) -> Result<Vec<u8>, AppError> {
    // Find the band
    let band = bands
        .iter()
        .find(|b| b.id == band_id)
        .ok_or_else(|| AppError::BandNotFound(band_id.to_string()))?;

    // Try to get album art from Deezer if we have a date
    let image_url = if let Some(concert_date) = date {
        match deezer::fetch_album_art_for_concert(client, &band.band, concert_date).await {
            Ok(Some(url)) => {
                tracing::info!(
                    "Using Deezer album art for {} at {}: {}",
                    band.band,
                    concert_date,
                    url
                );
                url
            }
            Ok(None) => {
                tracing::info!(
                    "No Deezer album found for {} at {}, using Spotify picture",
                    band.band,
                    concert_date
                );
                band.picture.clone()
            }
            Err(e) => {
                tracing::warn!(
                    "Deezer API error for {} at {}: {}, using Spotify picture",
                    band.band,
                    concert_date,
                    e
                );
                band.picture.clone()
            }
        }
    } else {
        tracing::info!("No date provided for {}, using Spotify picture", band.band);
        band.picture.clone()
    };

    tracing::info!("Fetching image for band: {} from {}", band.band, image_url);

    // Fetch the image
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

    let image_data = response.bytes().await?;

    // Build concert info for text rendering
    let concert_info = date.and_then(|d| {
        // Find the concert matching this date
        band.concerts.iter().find(|c| c.date == d).map(|concert| {
            // Format date from DD-MM-YYYY to more readable format
            let formatted_date = format_date(&concert.date);
            ConcertInfo {
                band_name: band.band.clone(),
                date: formatted_date,
                venue: concert.location.clone(),
            }
        })
    });

    // Get dimensions based on orientation (using Half width as default)
    let (target_width, target_height) = orientation.dimensions(WidgetWidth::Half);

    let processed = image_processing::process_image(&image_data, target_width, target_height, concert_info.as_ref())?;

    Ok(processed)
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

/// Generate a cache key for a concert
fn hash_concert(band_id: &str, date: &str) -> u32 {
    let key = format!("sawthat:{}:{}", band_id, date);
    let mut hash: u32 = 5381;
    for byte in key.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u32);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_concert() {
        let hash1 = hash_concert("abc123", "01-01-2024");
        let hash2 = hash_concert("abc123", "01-01-2024");
        let hash3 = hash_concert("abc123", "02-01-2024");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

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
        assert!(items[0].path.contains("test-id"));
    }
}
