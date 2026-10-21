use std::{env, fs, path::PathBuf};

// A version block, so the DLL reads "B-Rar context menu" in Properties.
fn main() {
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u16> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    let commas = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    let rc = format!(
        r#"1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILETYPE 0x2
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Ahmad Anjum"
      VALUE "FileDescription", "B-Rar context menu"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "B-Rar-Shell.dll"
      VALUE "LegalCopyright", "Copyright 2026 Ahmad Anjum. PolyForm Noncommercial 1.0.0."
      VALUE "OriginalFilename", "B-Rar-Shell.dll"
      VALUE "ProductName", "B-Rar"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("shell.rc");
    fs::write(&out, rc).unwrap();
    embed_resource::compile_for_cdylib(&out, embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
