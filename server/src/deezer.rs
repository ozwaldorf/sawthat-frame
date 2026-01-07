//! Deezer API integration
//!
//! Fetches artist and album data to find album art matching concert dates.

use reqwest::Client;
use serde::Deserialize;

use crate::error::AppError;

const DEEZER_BASE: &str = "https://api.deezer.com";

/// Deezer artist search response
#[derive(Debug, Deserialize)]
struct ArtistSearchResponse {
    data: Vec<DeezerArtist>,
}

/// Deezer artist
#[derive(Debug, Deserialize)]
struct DeezerArtist {
    id: u64,
}

/// Deezer albums response
#[derive(Debug, Deserialize)]
struct AlbumsResponse {
    data: Option<Vec<DeezerAlbum>>,
}

/// Deezer album
#[derive(Debug, Clone, Deserialize)]
pub struct DeezerAlbum {
    pub title: String,
    pub release_date: Option<String>,
    pub cover_xl: Option<String>,
    pub cover_big: Option<String>,
}

impl DeezerAlbum {
    /// Get the best available cover URL
    pub fn cover_url(&self) -> Option<&str> {
        self.cover_xl
            .as_deref()
            .or(self.cover_big.as_deref())
    }
}

/// Search for an artist on Deezer and return their ID
pub async fn search_artist(client: &Client, name: &str) -> Result<Option<u64>, AppError> {
    let url = format!(
        "{}/search/artist?q={}&limit=1",
        DEEZER_BASE,
        urlencoding::encode(name)
    );

    let response: ArtistSearchResponse = client
        .get(&url)
        .send()
        .await?
        .json()
        .await?;

    Ok(response.data.first().map(|a| a.id))
}

/// Fetch all albums for an artist
pub async fn fetch_albums(client: &Client, artist_id: u64) -> Result<Vec<DeezerAlbum>, AppError> {
    let url = format!("{}/artist/{}/albums?limit=100", DEEZER_BASE, artist_id);

    let response: AlbumsResponse = client
        .get(&url)
        .send()
        .await?
        .json()
        .await?;

    Ok(response.data.unwrap_or_default())
}

/// Parse a DD-MM-YYYY date string to a comparable integer (YYYYMMDD)
fn parse_concert_date(date: &str) -> Option<u32> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() == 3 {
        let day: u32 = parts[0].parse().ok()?;
        let month: u32 = parts[1].parse().ok()?;
        let year: u32 = parts[2].parse().ok()?;
        Some(year * 10000 + month * 100 + day)
    } else {
        None
    }
}

/// Parse a YYYY-MM-DD date string to a comparable integer (YYYYMMDD)
fn parse_release_date(date: &str) -> Option<u32> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() == 3 {
        let year: u32 = parts[0].parse().ok()?;
        let month: u32 = parts[1].parse().ok()?;
        let day: u32 = parts[2].parse().ok()?;
        Some(year * 10000 + month * 100 + day)
    } else {
        None
    }
}

/// Find the album released closest to (but before) the concert date
pub fn find_closest_album<'a>(albums: &'a [DeezerAlbum], concert_date: &str) -> Option<&'a DeezerAlbum> {
    let target = parse_concert_date(concert_date)?;

    let mut best_match: Option<&DeezerAlbum> = None;
    let mut best_diff: u32 = u32::MAX;

    for album in albums {
        if let Some(release) = album.release_date.as_deref().and_then(parse_release_date) {
            // Only consider albums released before or on the concert date
            if release <= target {
                let diff = target - release;
                if diff < best_diff {
                    best_diff = diff;
                    best_match = Some(album);
                }
            }
        }
    }

    best_match
}

/// Fetch the best album art URL for a band at a specific concert date
///
/// Returns the cover art URL for the album closest to the concert date,
/// or None if no suitable album is found.
pub async fn fetch_album_art_for_concert(
    client: &Client,
    band_name: &str,
    concert_date: &str,
) -> Result<Option<String>, AppError> {
    // Search for the artist
    let artist_id = match search_artist(client, band_name).await? {
        Some(id) => id,
        None => {
            tracing::debug!("Artist not found on Deezer: {}", band_name);
            return Ok(None);
        }
    };

    // Fetch their albums
    let albums = fetch_albums(client, artist_id).await?;

    // Find the closest album
    let album = match find_closest_album(&albums, concert_date) {
        Some(a) => a,
        None => {
            tracing::debug!("No matching album found for {} at {}", band_name, concert_date);
            return Ok(None);
        }
    };

    tracing::debug!(
        "Found album '{}' for {} at {}",
        album.title,
        band_name,
        concert_date
    );

    Ok(album.cover_url().map(String::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_concert_date() {
        assert_eq!(parse_concert_date("15-06-2024"), Some(20240615));
        assert_eq!(parse_concert_date("01-01-2020"), Some(20200101));
        assert_eq!(parse_concert_date("invalid"), None);
    }

    #[test]
    fn test_parse_release_date() {
        assert_eq!(parse_release_date("2024-06-15"), Some(20240615));
        assert_eq!(parse_release_date("2020-01-01"), Some(20200101));
        assert_eq!(parse_release_date("invalid"), None);
    }

    #[test]
    fn test_find_closest_album() {
        let albums = vec![
            DeezerAlbum {
                title: "Early Album".to_string(),
                release_date: Some("2018-01-01".to_string()),
                cover_xl: Some("https://example.com/early.jpg".to_string()),
                cover_big: None,
            },
            DeezerAlbum {
                title: "Middle Album".to_string(),
                release_date: Some("2020-06-15".to_string()),
                cover_xl: Some("https://example.com/middle.jpg".to_string()),
                cover_big: None,
            },
            DeezerAlbum {
                title: "Late Album".to_string(),
                release_date: Some("2023-01-01".to_string()),
                cover_xl: Some("https://example.com/late.jpg".to_string()),
                cover_big: None,
            },
        ];

        // Concert in 2021 should match Middle Album (2020)
        let result = find_closest_album(&albums, "01-03-2021");
        assert_eq!(result.map(|a| a.title.as_str()), Some("Middle Album"));

        // Concert in 2019 should match Early Album (2018)
        let result = find_closest_album(&albums, "01-06-2019");
        assert_eq!(result.map(|a| a.title.as_str()), Some("Early Album"));

        // Concert in 2024 should match Late Album (2023)
        let result = find_closest_album(&albums, "15-06-2024");
        assert_eq!(result.map(|a| a.title.as_str()), Some("Late Album"));

        // Concert before all albums should return None
        let result = find_closest_album(&albums, "01-01-2017");
        assert!(result.is_none());
    }
}
