//! Symbolic links in archives (Unix only: Windows skips them). Zipped apps and frameworks
//! need theirs (`Versions/Current`); links that would point outside the destination are
//! refused.
#![cfg(unix)]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use brar_core::Options;
use zip::write::{SimpleFileOptions, ZipWriter};

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("brar-links-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A zip of `files` (name, contents) and `links` (name, target), in that order.
fn zip(path: &Path, files: &[(&str, &str)], links: &[(&str, &str)]) {
    let mut w = ZipWriter::new(fs::File::create(path).unwrap());
    let opts = SimpleFileOptions::default();
    for (name, data) in files {
        w.start_file(*name, opts).unwrap();
        w.write_all(data.as_bytes()).unwrap();
    }
    for (name, target) in links {
        w.add_symlink(*name, *target, opts).unwrap();
    }
    w.finish().unwrap();
}

fn link(path: &Path) -> Option<PathBuf> {
    fs::read_link(path).ok()
}

#[test]
fn a_framework_keeps_its_links() {
    let dir = temp("framework");
    let archive = dir.join("app.zip");
    zip(
        &archive,
        &[
            ("Lib.framework/Versions/A/Lib", "code"),
            ("Lib.framework/Versions/A/Resources/info", "plist"),
        ],
        &[
            ("Lib.framework/Versions/Current", "A"),
            ("Lib.framework/Lib", "Versions/Current/Lib"),
            ("Lib.framework/Resources", "Versions/Current/Resources"),
        ],
    );
    let out = dir.join("out");
    let summary = brar_core::extract(&archive, &out, &Options::default()).unwrap();
    let fw = out.join("Lib.framework");
    assert_eq!(link(&fw.join("Versions/Current")), Some(PathBuf::from("A")));
    assert_eq!(link(&fw.join("Lib")), Some(PathBuf::from("Versions/Current/Lib")));
    // Through the links, to the real files.
    assert_eq!(fs::read_to_string(fw.join("Lib")).unwrap(), "code");
    assert_eq!(fs::read_to_string(fw.join("Resources/info")).unwrap(), "plist");
    assert_eq!(summary.files, 5);
    assert_eq!(summary.skipped, 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn links_out_of_the_destination_are_refused() {
    let dir = temp("escape");
    let archive = dir.join("evil.zip");
    zip(
        &archive,
        &[("ok/file", "x")],
        &[
            ("up", "../outside"),
            ("abs", "/etc/passwd"),
            ("climb", "ok/../../outside"),
            // A link to the destination itself is fine...
            ("here", "."),
            // ...but can't be used to climb out from "inside" it.
            ("here/sneak", "../outside"),
            ("deep/ok/back", "../../ok/file"),
        ],
    );
    let out = dir.join("out");
    let summary = brar_core::extract(&archive, &out, &Options::default()).unwrap();
    for refused in ["up", "abs", "climb"] {
        assert!(fs::symlink_metadata(out.join(refused)).is_err(), "{refused} was made");
    }
    assert_eq!(fs::read_to_string(out.join("deep/ok/back")).unwrap(), "x");
    // Entries are extracted in parallel, so "here" is either the link or (when "here/sneak"
    // came first) a folder; either way one of the two is refused, and nothing that was made
    // points outside.
    assert_eq!(summary.skipped, 4);
    let root = out.canonicalize().unwrap();
    for made in links_under(&out) {
        let target = fs::read_link(&made).unwrap();
        let mut resolved = made.parent().unwrap().canonicalize().unwrap().join(&target);
        // As far as it exists, the way the system would follow it.
        while !resolved.exists() {
            resolved = resolved.parent().unwrap().to_path_buf();
        }
        let resolved = resolved.canonicalize().unwrap();
        assert!(
            resolved.starts_with(&root),
            "{} -> {} leaves the destination",
            made.display(),
            target.display()
        );
    }
    assert!(!dir.join("outside").exists());
    let _ = fs::remove_dir_all(dir);
}

fn links_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let kind = fs::symlink_metadata(&path).unwrap().file_type();
        if kind.is_symlink() {
            found.push(path);
        } else if kind.is_dir() {
            found.extend(links_under(&path));
        }
    }
    found
}

#[test]
fn tar_links_too() {
    let dir = temp("tar");
    let archive = dir.join("t.tar");
    {
        let mut b = tar::Builder::new(fs::File::create(&archive).unwrap());
        let mut h = tar::Header::new_gnu();
        h.set_size(4);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "pkg/real.txt", &b"data"[..]).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "pkg/alias.txt", "real.txt").unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "pkg/escape", "../../outside").unwrap();
        b.finish().unwrap();
    }
    let out = dir.join("out");
    let summary = brar_core::extract(&archive, &out, &Options::default()).unwrap();
    assert_eq!(fs::read_to_string(out.join("pkg/alias.txt")).unwrap(), "data");
    assert!(fs::symlink_metadata(out.join("pkg/escape")).is_err());
    assert_eq!(summary.skipped, 1);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn testing_reads_links_without_writing() {
    let dir = temp("test");
    let archive = dir.join("a.zip");
    zip(&archive, &[("f", "x")], &[("l", "f")]);
    let summary = brar_core::test(&archive, &Options::default()).unwrap();
    assert_eq!(summary.files, 2);
    let _ = fs::remove_dir_all(dir);
}
