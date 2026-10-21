// On windows-gnu, std also links a small default manifest, so ld warns "multiple non-default
// manifests". Ours (linked first) is the one Windows uses; it requests Common Controls v6,
// per-monitor DPI awareness and long paths.
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=../../assets/brar.ico");
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let rc = resources(
        &dir.join("../../assets/brar.ico"),
        Some(&dir.join("app.manifest")),
        "B-Rar",
        "B-Rar-GUI.exe",
    );
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("app.rc");
    fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_required().unwrap();
}

/// Icon, optional manifest and a version block (what Task Manager and Properties show).
fn resources(icon: &std::path::Path, manifest: Option<&std::path::Path>, description: &str, file: &str) -> String {
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u16> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    let commas = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    let path = |p: &std::path::Path| p.display().to_string().replace('\\', "/");
    let mut rc = format!("1 ICON \"{}\"\n", path(icon));
    if let Some(m) = manifest {
        rc += &format!("1 24 \"{}\"\n", path(m));
    }
    rc += &format!(
        r#"1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Ahmad Anjum"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{file}"
      VALUE "LegalCopyright", "Copyright 2026 Ahmad Anjum. PolyForm Noncommercial 1.0.0."
      VALUE "OriginalFilename", "{file}"
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
    rc
}
