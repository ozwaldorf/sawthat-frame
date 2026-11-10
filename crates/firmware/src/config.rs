//! Runtime configuration read from `CONFIG.TXT` on the SD card root.
//!
//! The file holds `KEY=VALUE` lines. Blank lines and lines starting with `#` are ignored, and
//! whitespace around keys and values is trimmed.
//!
//! | Key               | Required | Meaning                                              |
//! |-------------------|----------|------------------------------------------------------|
//! | `WIFI_SSID`       | yes      | WiFi network name                                    |
//! | `WIFI_PASS`       | no       | WiFi password; empty or missing for an open network  |
//! | `SAWTHAT_USER_ID` | yes      | SawThat user whose concerts are shown                |
//! | `REFRESH_MINUTES` | no       | Minutes between artwork changes (default 15)         |
//! | `ORIENTATION`     | no       | `horizontal` or `vertical`; holding KEY rewrites it   |

use alloc::string::{String, ToString};
use core::fmt::Write;

use crate::widget::Orientation;

/// Config filename on the SD card root (8.3 format)
pub const CONFIG_FILE: &str = "CONFIG.TXT";

/// Template written to the SD root when there is no config
pub const TEMPLATE: &str = include_str!("../../../docs/CONFIG.TXT");

/// Key of the screen orientation, rewritten when KEY is held
pub const ORIENTATION_KEY: &str = "ORIENTATION";

/// Largest accepted config file
pub const MAX_CONFIG: usize = 4096;

/// Default minutes between artwork changes
const DEFAULT_REFRESH_MINUTES: u32 = 15;

/// Longest accepted refresh interval (one day)
const MAX_REFRESH_MINUTES: u32 = 24 * 60;

/// Runtime configuration
#[derive(Debug, Clone)]
pub struct Config {
    pub wifi_ssid: String,
    pub wifi_pass: String,
    pub user_id: String,
    pub refresh_minutes: u32,
    /// Screen orientation, if set
    pub orientation: Option<Orientation>,
}

impl Config {
    pub fn refresh_secs(&self) -> u64 {
        self.refresh_minutes as u64 * 60
    }
}

/// Why the configuration could not be used, shown on the setup screen
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// No SD card was found
    NoSdCard,
    /// The SD card has no readable FAT filesystem
    BadFormat,
    /// `CONFIG.TXT` is missing from the SD root
    Missing,
    /// `CONFIG.TXT` could not be read, or is not UTF-8
    Unreadable,
    /// A required key is missing or empty
    MissingKey(&'static str),
    /// A key has an unusable value
    Invalid(&'static str),
    /// A line is not `KEY=VALUE`, or the key is not known
    BadLine(usize),
    /// Could not connect to the configured WiFi network (SSID and cause)
    WifiFailed(String, WifiCause),
}

/// Why a WiFi connection failed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiCause {
    /// No access point with the SSID (or a compatible security mode) was found
    NotFound,
    /// Authentication or the WPA handshake failed, usually a wrong password
    Auth,
    /// Joined the network but got no address over DHCP
    NoIp,
    /// Another ESP-IDF disconnect reason, or none was reported
    Other(u8),
}

impl WifiCause {
    /// Classify the last ESP-IDF station disconnect reason; `linked` is whether the station
    /// joined the network, and `auth_failed` whether an earlier attempt failed authentication
    pub fn classify(reason: u8, linked: bool, auth_failed: bool) -> Self {
        match reason {
            _ if linked => Self::NoIp,
            201 | 210 | 211 => Self::NotFound,
            2 | 15 | 202 | 204 => Self::Auth,
            // A wrong password reports a handshake timeout, then generic connection failures
            205 if auth_failed => Self::Auth,
            other => Self::Other(other),
        }
    }
}

impl ConfigError {
    /// Short description for the setup screen
    pub fn message(&self) -> String {
        match self {
            Self::NoSdCard => "Missing FAT SD card".to_string(),
            Self::BadFormat => "Incorrect SD card format detected".to_string(),
            Self::Missing => alloc::format!("{} not found on the SD card", CONFIG_FILE),
            Self::Unreadable => alloc::format!("{} could not be read", CONFIG_FILE),
            Self::MissingKey(key) => alloc::format!("{} is not set", key),
            Self::Invalid(key) => alloc::format!("{} is invalid", key),
            Self::BadLine(line) => alloc::format!("Line {} is not a known KEY=VALUE", line),
            Self::WifiFailed(ssid, cause) => {
                let ssid: String = ssid.chars().take(20).collect();
                match cause {
                    WifiCause::NotFound => alloc::format!("Network not found: {}", ssid),
                    WifiCause::Auth => alloc::format!("Wrong password for {}", ssid),
                    WifiCause::NoIp => alloc::format!("No IP address from {}", ssid),
                    WifiCause::Other(0) => alloc::format!("WiFi failed: {}", ssid),
                    WifiCause::Other(n) => alloc::format!("WiFi error {}: {}", n, ssid),
                }
            }
        }
    }
}

/// Parse `CONFIG.TXT` contents
pub fn parse(data: &[u8]) -> Result<Config, ConfigError> {
    let text = core::str::from_utf8(data).map_err(|_| ConfigError::Unreadable)?;
    // Editors on some systems add a byte order mark
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut ssid = None;
    let mut pass = None;
    let mut user_id = None;
    let mut refresh = None;
    let mut orientation = None;

    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(ConfigError::BadLine(n + 1))?;
        let value = value.trim();
        let slot = match key.trim().to_ascii_uppercase().as_str() {
            "WIFI_SSID" => &mut ssid,
            "WIFI_PASS" => &mut pass,
            "SAWTHAT_USER_ID" => &mut user_id,
            "REFRESH_MINUTES" => &mut refresh,
            ORIENTATION_KEY => &mut orientation,
            _ => return Err(ConfigError::BadLine(n + 1)),
        };
        *slot = Some(value);
    }

    let required = |value: Option<&str>, key| match value {
        Some(v) if !v.is_empty() => Ok(v.to_string()),
        _ => Err(ConfigError::MissingKey(key)),
    };

    let wifi_ssid = required(ssid, "WIFI_SSID")?;
    if wifi_ssid.len() > 32 {
        return Err(ConfigError::Invalid("WIFI_SSID"));
    }
    let wifi_pass = pass.unwrap_or_default().to_string();
    if wifi_pass.len() > 64 || (!wifi_pass.is_empty() && wifi_pass.len() < 8) {
        return Err(ConfigError::Invalid("WIFI_PASS"));
    }

    let user_id = required(user_id, "SAWTHAT_USER_ID")?;
    if !user_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(ConfigError::Invalid("SAWTHAT_USER_ID"));
    }

    let refresh_minutes = match refresh.filter(|v| !v.is_empty()) {
        None => DEFAULT_REFRESH_MINUTES,
        Some(v) => v
            .parse()
            .ok()
            .filter(|m| (1..=MAX_REFRESH_MINUTES).contains(m))
            .ok_or(ConfigError::Invalid("REFRESH_MINUTES"))?,
    };

    let orientation = match orientation.filter(|v| !v.is_empty()) {
        None => None,
        Some(v) => Some(match v.to_ascii_lowercase().as_str() {
            "horizontal" => Orientation::Horizontal,
            "vertical" => Orientation::Vertical,
            _ => return Err(ConfigError::Invalid(ORIENTATION_KEY)),
        }),
    };

    Ok(Config {
        wifi_ssid,
        wifi_pass,
        user_id,
        refresh_minutes,
        orientation,
    })
}

/// Config value for an orientation
pub fn orientation_value(orientation: Orientation) -> &'static str {
    match orientation {
        Orientation::Horizontal => "horizontal",
        Orientation::Vertical => "vertical",
    }
}

/// Set `key` to `value` in config file text, keeping comments, other keys and line endings.
///
/// Every uncommented line for `key` is replaced (parsing uses the last one); without one,
/// the entry is appended.
pub fn set_value(text: &str, key: &str, value: &str) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len() + key.len() + value.len() + 4);
    let mut found = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let trimmed = body.trim_start();
        let is_key = !trimmed.starts_with('#')
            && trimmed
                .split_once('=')
                .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(key));
        if is_key {
            let _ = write!(out, "{}={}{}", key, value, &line[body.len()..]);
            found = true;
        } else {
            out.push_str(line);
        }
    }
    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        let _ = write!(out, "{}={}{}", key, value, eol);
    }
    out
}
