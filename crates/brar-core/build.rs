use std::{env, path::PathBuf, process::Command};

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // unrar_sys uses registry and crypto APIs but doesn't declare advapi32.
    println!("cargo:rustc-link-lib=advapi32");
    // In this crate's own test binary the line above lands before libunrar; repeat it last.
    println!("cargo:rustc-link-arg=-ladvapi32");

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        shadow_dynamic_pthread();
    }
}

/// unrar_sys emits a plain `-lpthread`, which MinGW resolves to the winpthread DLL import
/// library, while Rust's std links `libpthread.a` statically: the link fails on duplicate
/// symbols. A search directory holding only the static archive, placed ahead of the
/// toolchain's, makes both references resolve to the same static library.
fn shadow_dynamic_pthread() {
    let cc = env::var("CC").unwrap_or_else(|_| "gcc".into());
    let Ok(out) = Command::new(&cc).arg("-print-file-name=libpthread.a").output() else {
        return;
    };
    let lib = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if !lib.is_absolute() || !lib.exists() {
        return;
    }
    let dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("static-pthread");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&lib, dir.join("libpthread.a")).unwrap();
    println!("cargo:rustc-link-search=native={}", dir.display());
}
