//! On Unix, extracted files keep the permissions stored in the archive (a script stays
//! executable), and ZIPs B-Rar makes store them.
#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use brar_core::{CreateOptions, Options, create, extract};

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("brar-modes-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn tar_keeps_modes() {
    let dir = temp("tar");
    let archive = dir.join("a.tar");
    let mut b = tar::Builder::new(fs::File::create(&archive).unwrap());
    for (name, mode) in [("run.sh", 0o755), ("notes.txt", 0o640)] {
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_mode(mode);
        h.set_cksum();
        b.append_data(&mut h, name, &b"hi\n"[..]).unwrap();
    }
    b.into_inner().unwrap();

    let out = dir.join("out");
    extract(&archive, &out, &Options::default()).unwrap();
    assert_eq!(mode(&out.join("run.sh")), 0o755);
    assert_eq!(mode(&out.join("notes.txt")), 0o640);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn zip_keeps_modes() {
    let dir = temp("zip");
    let archive = dir.join("a.zip");
    let mut w = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    w.start_file("run.sh", opts.unix_permissions(0o755)).unwrap();
    w.write_all(b"#!/bin/sh\n").unwrap();
    w.start_file("notes.txt", opts.unix_permissions(0o600)).unwrap();
    w.write_all(b"hi\n").unwrap();
    w.finish().unwrap();

    let out = dir.join("out");
    extract(&archive, &out, &Options::default()).unwrap();
    assert_eq!(mode(&out.join("run.sh")), 0o755);
    assert_eq!(mode(&out.join("notes.txt")), 0o600);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn made_zip_keeps_modes() {
    let dir = temp("made");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(src.join("notes.txt"), "hi\n").unwrap();
    fs::set_permissions(src.join("notes.txt"), fs::Permissions::from_mode(0o644)).unwrap();

    let archive = dir.join("made.zip");
    create(&archive, &[src.clone()], &CreateOptions::default()).unwrap();
    let out = dir.join("out");
    extract(&archive, &out, &Options::default()).unwrap();
    assert_eq!(mode(&out.join("src/run.sh")), 0o755);
    assert_eq!(mode(&out.join("src/notes.txt")), 0o644);
    fs::remove_dir_all(&dir).unwrap();
}
