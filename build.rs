//! Draws the icon into an .ico and embeds it, with version info, into the .exe.

#[path = "src/icon.rs"]
mod icon;

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // ICO with PNG-encoded images (supported since Windows Vista).
    let sizes = [16u32, 24, 32, 48, 64, 256];
    let pngs: Vec<Vec<u8>> = sizes.iter().map(|&s| icon::pixmap(s).encode_png().expect("png")).collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (size, png) in sizes.iter().zip(&pngs) {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in &pngs {
        ico.extend_from_slice(png);
    }
    let ico_path = out.join("dozer.ico");
    std::fs::write(&ico_path, ico).unwrap();

    let version = env!("CARGO_PKG_VERSION");
    let mut parts: Vec<&str> = version.split(['.', '-']).take(3).collect();
    parts.resize(3, "0");
    let comma = format!("{},{},{},0", parts[0], parts[1], parts[2]);
    let rc = format!(
        r#"#include <winver.h>
1 ICON "{ico}"
VS_VERSION_INFO VERSIONINFO
FILEVERSION     {comma}
PRODUCTVERSION  {comma}
FILEOS          VOS_NT_WINDOWS32
FILETYPE        VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName",      "Dozer"
      VALUE "FileDescription",  "Dozer - frees RAM and runs MCP servers on demand"
      VALUE "FileVersion",      "{version}"
      VALUE "InternalName",     "dozer"
      VALUE "OriginalFilename", "dozer.exe"
      VALUE "ProductName",      "Dozer"
      VALUE "ProductVersion",   "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = ico_path.display().to_string().replace('\\', "/"),
    );
    let rc_path = out.join("dozer.rc");
    std::fs::write(&rc_path, rc).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed Windows resources");
}
