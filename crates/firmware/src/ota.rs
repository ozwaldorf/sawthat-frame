//! Signed firmware updates.
//!
//! Each release's web installer manifest names its app image with the image's size and SHA-256,
//! signed with the release key (see `release.yml`). An image is installed only when the signature
//! verifies and the downloaded image matches, so the download itself needs no trust.
//!
//! The image goes into the app slot that is not running, which boots next. A new image confirms
//! itself after a successful screen refresh; one that crashes, or restarts too often before
//! confirming, switches back to the previous slot.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

use ed25519_dalek::{Signature, VerifyingKey};
use embassy_futures::yield_now;
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{self, PARTITION_TABLE_MAX_LEN};
use esp_storage::FlashStorage;
use log::info;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::crash::FIRMWARE_VERSION;

/// Ed25519 public key of the release signing key
const PUBLIC_KEY: [u8; 32] = [
    0x10, 0x89, 0x26, 0x14, 0x9e, 0x51, 0xa1, 0x9d, 0xac, 0xe1, 0xbb, 0x7a, 0xe6, 0x40, 0xee, 0xe1,
    0x76, 0x67, 0x59, 0x13, 0xdb, 0x89, 0x9b, 0x9a, 0x2a, 0x68, 0x11, 0xc2, 0xb0, 0x0a, 0xc3, 0x29,
];

/// ESP image header magic, and the ESP32-S3 chip ID in the extended header
const IMAGE_MAGIC: u8 = 0xE9;
const ESP32S3_CHIP_ID: u16 = 9;

/// Boots of an unconfirmed image before it is rolled back
const MAX_UNCONFIRMED_BOOTS: u8 = 3;

/// Boots of the running image since it was installed, while unconfirmed
#[esp_hal::ram(unstable(rtc_fast))]
static mut UNCONFIRMED_BOOTS: u8 = 0;

/// Whether the running image is an update that has not confirmed itself yet, set at boot
static UNCONFIRMED: AtomicBool = AtomicBool::new(false);

/// A signed app image
#[derive(Debug)]
pub struct Release {
    /// Path of the image, relative to the manifest
    pub path: String,
    pub size: usize,
    sha256: [u8; 32],
}

/// The latest release named by a manifest
#[derive(Debug)]
pub struct Update {
    pub version: [u16; 3],
    /// The image, if the manifest has one with a valid signature
    pub release: Option<Release>,
}

impl Update {
    /// Whether the release is newer than this firmware
    pub fn is_newer(&self) -> bool {
        is_newer(self.version)
    }
}

/// Whether `version` is newer than this firmware
pub fn is_newer(version: [u16; 3]) -> bool {
    parse_version(FIRMWARE_VERSION).is_some_and(|current| version > current)
}

/// Parse a `major.minor.patch` version, with an optional `v` prefix and suffix after the patch
pub fn parse_version(version: &str) -> Option<[u16; 3]> {
    let mut parts = version.strip_prefix('v').unwrap_or(version).splitn(3, '.');
    let mut next = || {
        let part = parts.next()?;
        let end = part
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(part.len());
        part[..end].parse().ok()
    };
    Some([next()?, next()?, next()?])
}

/// The message a release signature covers
fn signed_message(version: &str, size: usize, sha256: &str) -> String {
    format!("sawthat-frame {version} {size} {sha256}")
}

/// Parse a web installer manifest, keeping its image only if the signature verifies
pub fn parse_manifest(json: &[u8]) -> Option<Update> {
    #[derive(Deserialize)]
    struct Manifest<'a> {
        version: &'a str,
        #[serde(borrow)]
        ota: Option<Image<'a>>,
    }
    #[derive(Deserialize)]
    struct Image<'a> {
        path: &'a str,
        size: usize,
        sha256: &'a str,
        signature: &'a str,
    }

    let manifest: Manifest = serde_json::from_slice(json).ok()?;
    let version = parse_version(manifest.version)?;
    let release = manifest.ota.and_then(|image| {
        let sha256 = hex::<32>(image.sha256)?;
        let signature = Signature::from_bytes(&hex::<64>(image.signature)?);
        let message = signed_message(manifest.version, image.size, image.sha256);
        let key = VerifyingKey::from_bytes(&PUBLIC_KEY).ok()?;
        match key.verify_strict(message.as_bytes(), &signature) {
            Ok(()) => Some(Release {
                path: String::from(image.path),
                size: image.size,
                sha256,
            }),
            Err(_) => {
                info!("Release {} has an invalid signature", manifest.version);
                None
            }
        }
    });
    Some(Update { version, release })
}

/// Decode `N` bytes of lowercase or uppercase hex
fn hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.as_bytes();
    if text.len() != 2 * N {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = digit(text[2 * i])? << 4 | digit(text[2 * i + 1])?;
    }
    Some(out)
}

#[derive(Debug)]
pub enum OtaError {
    /// The download does not match the signed size and hash
    Mismatch,
    /// Not an ESP32-S3 app image
    NotAnImage,
    /// The image is larger than the app slot
    TooLarge,
    /// The image read back from flash differs from the download
    Verify,
    Flash(partitions::Error),
}

impl From<partitions::Error> for OtaError {
    fn from(e: partitions::Error) -> Self {
        Self::Flash(e)
    }
}

/// Check a downloaded image against the signed release
pub fn check_image(release: &Release, image: &[u8]) -> Result<(), OtaError> {
    if image.len() != release.size || Sha256::digest(image)[..] != release.sha256 {
        return Err(OtaError::Mismatch);
    }
    if image.len() < 24
        || image[0] != IMAGE_MAGIC
        || u16::from_le_bytes([image[12], image[13]]) != ESP32S3_CHIP_ID
    {
        return Err(OtaError::NotAnImage);
    }
    Ok(())
}

/// Write a checked image to the app slot that is not running and boot it next. Yields between
/// sectors, so other tasks keep running while the flash is busy.
pub async fn install(flash: &mut FlashStorage<'static>, image: &[u8]) -> Result<(), OtaError> {
    const CHUNK: usize = FlashStorage::SECTOR_SIZE as usize;
    let mut table = Box::new([0u8; PARTITION_TABLE_MAX_LEN]);
    let mut ota = OtaUpdater::new(flash, &mut table)?;
    let (mut slot, kind) = ota.next_partition()?;
    if image.len() > slot.capacity() {
        return Err(OtaError::TooLarge);
    }
    info!("Writing {} bytes to {:?}", image.len(), kind);
    for (i, chunk) in image.chunks(CHUNK).enumerate() {
        let offset = (i * CHUNK) as u32;
        slot.erase(offset, offset + CHUNK as u32)?;
        slot.write(offset, chunk)?;
        yield_now().await;
    }

    info!("Verifying {:?}", kind);
    let mut hash = Sha256::new();
    let mut buf = Box::new([0u8; CHUNK]);
    for (i, chunk) in image.chunks(CHUNK).enumerate() {
        let buf = &mut buf[..chunk.len()];
        slot.read((i * CHUNK) as u32, buf)?;
        hash.update(&*buf);
        yield_now().await;
    }
    if hash.finalize() != Sha256::digest(image) {
        return Err(OtaError::Verify);
    }

    ota.activate_next_partition()?;
    ota.set_current_ota_state(OtaImageState::New)?;
    unsafe { UNCONFIRMED_BOOTS = 0 };
    info!("Installed the update in {:?}", kind);
    Ok(())
}

/// The OTA state of the running image, or `None` if no slot was ever selected
fn running_state(flash: &mut FlashStorage<'static>) -> Option<OtaImageState> {
    let mut table = Box::new([0u8; PARTITION_TABLE_MAX_LEN]);
    let mut ota = OtaUpdater::new(flash, &mut table)
        .map_err(|e| info!("No OTA partitions: {:?}", e))
        .ok()?;
    ota.current_ota_state().ok()
}

fn is_unconfirmed(state: Option<OtaImageState>) -> bool {
    matches!(
        state,
        Some(OtaImageState::New | OtaImageState::PendingVerify)
    )
}

/// At boot: count the boots of an unconfirmed image, and roll it back to the previous slot if it
/// crashed or restarted too often. Returns only if the running image stays.
pub fn check_boot(flash: &mut FlashStorage<'static>, crashed: bool) {
    let state = running_state(flash);
    info!("OTA state: {:?}", state);
    if !is_unconfirmed(state) {
        unsafe { UNCONFIRMED_BOOTS = 0 };
        return;
    }
    let boots = unsafe {
        UNCONFIRMED_BOOTS = UNCONFIRMED_BOOTS.saturating_add(1);
        UNCONFIRMED_BOOTS
    };
    info!("Unconfirmed update, boot {}", boots);
    if !crashed && boots <= MAX_UNCONFIRMED_BOOTS {
        UNCONFIRMED.store(true, Ordering::Relaxed);
        return;
    }

    info!("Rolling back the update");
    let mut table = Box::new([0u8; PARTITION_TABLE_MAX_LEN]);
    let rollback = OtaUpdater::new(flash, &mut table).and_then(|mut ota| {
        ota.set_current_ota_state(OtaImageState::Invalid)?;
        ota.activate_next_partition()?;
        ota.set_current_ota_state(OtaImageState::Valid)
    });
    match rollback {
        Ok(()) => {
            unsafe { UNCONFIRMED_BOOTS = 0 };
            esp_hal::system::software_reset()
        }
        Err(e) => info!("Rollback failed: {:?}", e),
    }
}

/// Whether the running image is a new update that has not shown a screen yet
pub fn is_new() -> bool {
    UNCONFIRMED.load(Ordering::Relaxed)
}

/// Confirm the running image once it works, so it is not rolled back
pub fn confirm(flash: &mut FlashStorage<'static>) {
    if !UNCONFIRMED.swap(false, Ordering::Relaxed) {
        return;
    }
    let mut table = Box::new([0u8; PARTITION_TABLE_MAX_LEN]);
    match OtaUpdater::new(flash, &mut table)
        .and_then(|mut ota| ota.set_current_ota_state(OtaImageState::Valid))
    {
        Ok(()) => {
            unsafe { UNCONFIRMED_BOOTS = 0 };
            info!("Confirmed the update");
        }
        Err(e) => info!("Failed to confirm the update: {:?}", e),
    }
}
