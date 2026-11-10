//! SawThat concert data and Deezer album selection.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use serde::Deserialize;

/// SawThat API endpoint (append the user id)
pub const SAWTHAT_BANDS_URL: &str = "https://server.sawthat.band/api/bands?id=";
/// Deezer API base
pub const DEEZER_API: &str = "https://api.deezer.com";

#[derive(Deserialize)]
struct Band {
    band: String,
    #[serde(default)]
    picture: Option<String>,
    concerts: Vec<BandConcert>,
    id: String,
    /// SawThat user the band belongs to
    #[serde(default)]
    user_id: Option<String>,
}

#[derive(Deserialize)]
struct BandConcert {
    /// DD-MM-YYYY
    date: String,
    location: String,
}

/// A displayable concert
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Concert {
    /// Item path: YYYY-MM-DD-band-id
    pub path: String,
    pub band_name: String,
    /// Concert date as DD-MM-YYYY
    pub date: String,
    pub venue: String,
    /// Fallback artist picture URL
    pub picture: String,
}

impl Concert {
    /// Date formatted as "Month DDth, YYYY"
    pub fn formatted_date(&self) -> String {
        format_date(&self.date)
    }
}

/// Parse the SawThat bands response into concerts of `user_id`, most recent first.
///
/// Bands of another user are skipped, so a stored response for a previous user yields
/// nothing; bands without a user ID are kept.
pub fn parse_concerts(json: &[u8], user_id: &str) -> Result<Vec<Concert>, serde_json::Error> {
    let bands: Vec<Band> = serde_json::from_slice(json)?;

    let mut concerts: Vec<(String, Concert)> = bands
        .into_iter()
        .filter(|band| band.user_id.as_deref().is_none_or(|id| id == user_id))
        .flat_map(|band| {
            let picture = band.picture.unwrap_or_default();
            band.concerts
                .into_iter()
                .filter_map(|c| {
                    let mut parts = c.date.split('-');
                    let (day, month, year) = (parts.next()?, parts.next()?, parts.next()?);
                    if parts.next().is_some() {
                        return None;
                    }
                    let iso = format!("{}-{}-{}", year, month, day);
                    Some((
                        iso.clone(),
                        Concert {
                            path: format!("{}-{}", iso, band.id),
                            band_name: band.band.clone(),
                            date: c.date,
                            venue: c.location,
                            picture: picture.clone(),
                        },
                    ))
                })
                .collect::<Vec<_>>()
        })
        .collect();

    concerts.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(concerts.into_iter().map(|(_, c)| c).collect())
}

/// Percent-encode a query parameter value
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Deezer artist search URL
pub fn deezer_search_url(band_name: &str) -> String {
    format!(
        "{}/search/artist?q={}&limit=1",
        DEEZER_API,
        url_encode(band_name)
    )
}

/// Deezer artist albums URL
pub fn deezer_albums_url(artist_id: u64) -> String {
    format!("{}/artist/{}/albums?limit=100", DEEZER_API, artist_id)
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<SearchArtist>,
}

#[derive(Deserialize)]
struct SearchArtist {
    id: u64,
}

/// First artist id from a Deezer search response
pub fn parse_artist_id(json: &[u8]) -> Option<u64> {
    serde_json::from_slice::<SearchResponse>(json)
        .ok()?
        .data
        .first()
        .map(|a| a.id)
}

/// Album release date and cover URL
#[derive(Debug, Clone)]
pub struct Album {
    /// YYYYMMDD
    pub release: u32,
    pub cover: String,
}

#[derive(Deserialize)]
struct AlbumsResponse {
    #[serde(default)]
    data: Option<Vec<DeezerAlbum>>,
}

#[derive(Deserialize)]
struct DeezerAlbum {
    release_date: Option<String>,
    cover_xl: Option<String>,
    cover_big: Option<String>,
}

fn parse_ymd(date: &str) -> Option<u32> {
    let mut p = date.split('-').map(|s| s.parse::<u32>().ok());
    let (y, m, d) = (p.next()??, p.next()??, p.next()??);
    Some(y * 10000 + m * 100 + d)
}

fn parse_dmy(date: &str) -> Option<u32> {
    let mut p = date.split('-').map(|s| s.parse::<u32>().ok());
    let (d, m, y) = (p.next()??, p.next()??, p.next()??);
    Some(y * 10000 + m * 100 + d)
}

/// Albums with a release date and cover from a Deezer albums response
pub fn parse_albums(json: &[u8]) -> Vec<Album> {
    let Ok(resp) = serde_json::from_slice::<AlbumsResponse>(json) else {
        return Vec::new();
    };
    resp.data
        .unwrap_or_default()
        .into_iter()
        .filter_map(|a| {
            Some(Album {
                release: parse_ymd(a.release_date.as_deref()?)?,
                cover: a.cover_xl.or(a.cover_big)?,
            })
        })
        .collect()
}

/// Rewrite a Deezer CDN cover URL to request a `size` x `size` image.
///
/// Deezer covers end in `/{w}x{h}-000000-80-0-0.jpg` and the CDN serves any size,
/// so fetching only the needed resolution saves download and decode time.
pub fn sized_cover_url(url: &str, size: u32) -> String {
    let Some((base, file)) = url.rsplit_once('/') else {
        return String::from(url);
    };
    match file.split_once('-') {
        Some((dims, rest)) if url.contains("dzcdn.net/images/cover/") && dims.contains('x') => {
            format!("{}/{}x{}-{}", base, size, size, rest)
        }
        _ => String::from(url),
    }
}

/// Cover of the album released closest to, but not after, the concert date (DD-MM-YYYY)
pub fn closest_album<'a>(albums: &'a [Album], concert_date: &str) -> Option<&'a Album> {
    let target = parse_dmy(concert_date)?;
    albums
        .iter()
        .filter(|a| a.release <= target)
        .min_by_key(|a| target - a.release)
}

/// Format DD-MM-YYYY as "Month DDth, YYYY"
pub fn format_date(date: &str) -> String {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return String::from(date);
    }
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
        _ => return String::from(date),
    };
    let suffix = match day {
        1 | 21 | 31 => "st",
        2 | 22 => "nd",
        3 | 23 => "rd",
        _ => "th",
    };
    format!("{} {}{}, {}", month, day, suffix, parts[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concerts_sorted_with_paths() {
        let json = br#"[
            {"band":"A","picture":"p","concerts":[{"date":"15-06-2024","location":"V"}],"id":"a-id","genre":[]},
            {"band":"B","picture":"","concerts":[{"date":"01-01-2025","location":"W"}],"id":"b-id"}
        ]"#;
        let c = parse_concerts(json, "u").unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].path, "2025-01-01-b-id");
        assert_eq!(c[1].path, "2024-06-15-a-id");
        assert_eq!(c[1].formatted_date(), "June 15th, 2024");
    }

    #[test]
    fn concerts_of_other_users_skipped() {
        let json = br#"[
            {"band":"A","concerts":[{"date":"15-06-2024","location":"V"}],"id":"a-id","user_id":"me"},
            {"band":"B","concerts":[{"date":"01-01-2025","location":"W"}],"id":"b-id","user_id":"other"}
        ]"#;
        let c = parse_concerts(json, "me").unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].path, "2024-06-15-a-id");
        assert!(parse_concerts(json, "nobody").unwrap().is_empty());
    }

    #[test]
    fn closest_album_before_concert() {
        let albums = parse_albums(
            br#"{"data":[
                {"release_date":"2018-01-01","cover_xl":"early"},
                {"release_date":"2020-06-15","cover_xl":"middle"},
                {"release_date":"2023-01-01","cover_xl":"late"}
            ]}"#,
        );
        assert_eq!(
            closest_album(&albums, "01-03-2021").unwrap().cover,
            "middle"
        );
        assert_eq!(closest_album(&albums, "15-06-2024").unwrap().cover, "late");
        assert!(closest_album(&albums, "01-01-2017").is_none());
    }

    #[test]
    fn resizes_deezer_covers_only() {
        assert_eq!(
            sized_cover_url(
                "https://cdn-images.dzcdn.net/images/cover/abc/1000x1000-000000-80-0-0.jpg",
                680
            ),
            "https://cdn-images.dzcdn.net/images/cover/abc/680x680-000000-80-0-0.jpg"
        );
        let spotify = "https://i.scdn.co/image/ab6761610000e5eb0af1c77a29684d5f10cc19a6";
        assert_eq!(sized_cover_url(spotify, 680), spotify);
    }

    #[test]
    fn encodes_query() {
        assert_eq!(url_encode("$uicideboy$ & co"), "%24uicideboy%24%20%26%20co");
    }
}
