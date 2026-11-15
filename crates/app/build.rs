//! Windows only: puts the icon and the version info into rongo.exe, through
//! mingw's windres (or rc.exe on an MSVC toolchain). On every other target
//! this does nothing.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../assets/icon.ico").canonicalize().expect("assets/icon.ico (python3 scripts/make-icon.py writes it)");
    println!("cargo:rerun-if-changed={}", icon.display());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    // FILEVERSION wants four numbers: 0.1.4 -> 0,1,4,0 (a pre-release suffix is dropped).
    let mut numbers = version.split('.').map(|p| p.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("0").parse::<u16>().unwrap_or(0)).collect::<Vec<_>>();
    numbers.resize(4, 0);
    let numeric = numbers.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    // RC strings are C-like; the path has no quote in it, but backslashes (Windows host) must be doubled.
    let icon = icon.display().to_string().replace('\\', "/");
    let rc = format!(
        r#"#include <winver.h>
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "ProductName", "rongo"
      VALUE "FileDescription", "rongo"
      VALUE "ProductVersion", "{version}"
      VALUE "FileVersion", "{version}"
      VALUE "OriginalFilename", "rongo.exe"
      VALUE "InternalName", "rongo"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("rongo.rc");
    std::fs::write(&out, rc).expect("write rongo.rc");
    embed_resource::compile(&out, embed_resource::NONE).manifest_required().expect("embed the icon and version info");
}
