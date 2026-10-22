//! Embeds the app icon and version information in the executables, so
//! Explorer, shortcuts and the taskbar show the Patina icon.
//!
//! The icon is drawn by the same code the app uses at runtime (src/icon.rs)
//! and written as a multi-size .ico.

use std::path::{Path, PathBuf};

mod icon {
    include!("src/icon.rs");
}

const ICON_SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico = out.join("patina.ico");
    std::fs::write(&ico, ico_bytes()).expect("write icon");

    let rc = out.join("app.rc");
    std::fs::write(&rc, resource_script(&ico)).expect("write resource script");
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().expect("compile resources");
}

/// An .ico with one 32-bit BMP image per size.
fn ico_bytes() -> Vec<u8> {
    let images: Vec<Vec<u8>> = ICON_SIZES.iter().map(|&size| bmp_image(size)).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(ICON_SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * ICON_SIZES.len();
    for (&size, image) in ICON_SIZES.iter().zip(&images) {
        let dimension = if size >= 256 { 0 } else { size as u8 }; // 0 means 256
        out.extend_from_slice(&[dimension, dimension, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // color planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(image.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += image.len();
    }
    for image in images {
        out.extend_from_slice(&image);
    }
    out
}

/// BITMAPINFOHEADER + bottom-up BGRA pixels + an (empty) AND mask, as icons expect.
fn bmp_image(size: u32) -> Vec<u8> {
    let rgba = icon::rgba(size);
    let mask_row = ((size + 31) / 32 * 4) as usize;
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // color + mask
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]); // no compression, sizes and palette unused
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.resize(out.len() + mask_row * size as usize, 0);
    out
}

fn resource_script(ico: &Path) -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut parts: Vec<u16> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    let numeric = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    let icon_path = ico.display().to_string().replace('\\', "\\\\");
    format!(
        r#"1 ICON "{icon_path}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Patina"
      VALUE "ProductName", "Patina"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "LegalCopyright", "MIT OR Apache-2.0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}
