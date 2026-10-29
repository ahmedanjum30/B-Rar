use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../../assets/brar.ico");
    // The icon and version block are Windows resources; Linux builds have neither.
    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = dir.join("../../assets/brar.ico").display().to_string().replace('\\', "/");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u16> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    let commas = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    // Icon plus a version block (what Task Manager and Properties show).
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Ahmad Anjum"
      VALUE "FileDescription", "B-Rar (command line)"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "B-Rar.exe"
      VALUE "LegalCopyright", "Copyright 2026 Ahmad Anjum. PolyForm Noncommercial 1.0.0."
      VALUE "OriginalFilename", "B-Rar.exe"
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
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("cli.rc");
    fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().unwrap();
}
