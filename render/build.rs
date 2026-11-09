//! Selects the embedded font: `SAWTHAT_FONT` (path to a TTF/OTF) or the bundled IBM Plex Mono Bold.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=SAWTHAT_FONT");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let font = env::var("SAWTHAT_FONT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("assets/IBMPlexMono-Bold.ttf"));
    println!("cargo:rerun-if-changed={}", font.display());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("font.ttf");
    fs::copy(&font, &out)
        .unwrap_or_else(|e| panic!("failed to copy font {}: {}", font.display(), e));
}
