use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let font_out = out_dir.join("font.ttf");

    // Try to find Berkeley Mono using fontconfig
    let font_path = find_font("Berkeley Mono:style=Bold")
        .or_else(|| find_font("Berkeley Mono"))
        .or_else(|| find_font("IBM Plex Mono:style=Bold"))
        .or_else(|| find_font("IBM Plex Sans:style=Bold"))
        .or_else(|| find_font("DejaVu Sans:style=Bold"))
        .or_else(|| find_font("Liberation Sans:style=Bold"));

    match font_path {
        Some(path) => {
            println!("cargo:warning=Using system font: {}", path.display());
            // Remove existing file if present (may be read-only from previous build)
            let _ = fs::remove_file(&font_out);
            fs::copy(&path, &font_out).expect("Failed to copy font to OUT_DIR");
        }
        None => {
            panic!("No suitable font found. Install Berkeley Mono or a fallback (IBM Plex, DejaVu Sans, Liberation Sans)");
        }
    }

    println!("cargo:rerun-if-changed=build.rs");
}

/// Use fc-match to find a font by pattern
fn find_font(pattern: &str) -> Option<PathBuf> {
    let output = Command::new("fc-match")
        .args(["--format=%{file}", pattern])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let path_str = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(path_str.trim());

    // Verify the file exists and is a TTF/OTF
    if path.exists()
        && path
            .extension()
            .map(|e| e == "ttf" || e == "otf")
            .unwrap_or(false)
    {
        Some(path)
    } else {
        None
    }
}
