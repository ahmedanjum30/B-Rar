//! Solid RAR5 archives through B-Rar's two-phase engine: one compressed stream across files,
//! with small files (written on the writer pool) and big ones (streamed) mixed, and
//! extracting only some files out of the middle of the stream. Needs WinRAR's Rar.exe;
//! skipped without it.

use std::{fs, path::Path, process::Command};

use brar_core::{ConflictPolicy, Options, Resolution, Selection};

const RAR: &str = r"C:\Program Files\WinRAR\Rar.exe";

#[test]
fn solid_archive_extracts_exactly() {
    if !Path::new(RAR).exists() {
        eprintln!("skipping: {RAR} not installed");
        return;
    }
    let root = std::env::temp_dir().join(format!("brar-rar-solid-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    fs::create_dir_all(src.join("docs")).unwrap();
    // Small files that repeat each other (so matches reach back into earlier files), and two
    // files over the 8 MB in-memory limit.
    for i in 0..60 {
        let text = format!("file {i}: the quick brown fox jumps over the lazy dog {}\n", i % 7).repeat(200 + i * 13);
        fs::write(src.join("docs").join(format!("note{i:02}.txt")), text).unwrap();
    }
    for (name, seed) in [("big1.bin", 11u32), ("big2.bin", 29)] {
        let data: Vec<u8> = (0..20u32 << 20).map(|i| (i.wrapping_mul(seed) >> 11) as u8 ^ (i % 251) as u8).collect();
        fs::write(src.join(name), data).unwrap();
    }
    let archive = root.join("t.rar");
    let made = Command::new(RAR)
        .args(["a", "-ma5", "-s", "-r", "-m3", "-idq", "-ep1"])
        .arg(&archive)
        .arg(src.join("*"))
        .status()
        .unwrap();
    assert!(made.success());

    let opts = Options {
        threads: 4,
        on_conflict: ConflictPolicy::Always(Resolution::Cancel),
        ..Options::default()
    };
    assert_eq!(brar_core::test(&archive, &opts).unwrap().files, 62);

    let out = root.join("out");
    assert_eq!(brar_core::extract(&archive, &out, &opts).unwrap().files, 62);
    for rel in ["big1.bin", "big2.bin", "docs/note00.txt", "docs/note33.txt", "docs/note59.txt"] {
        assert!(fs::read(src.join(rel)).unwrap() == fs::read(out.join(rel)).unwrap(), "{rel} differs");
    }
    let count = fs::read_dir(out.join("docs")).unwrap().count();
    assert_eq!(count, 60);

    // Two files from the middle of the stream: everything before them is decoded, only they
    // are written.
    let picked = root.join("picked");
    let opts = Options {
        selection: Some(Selection {
            items: vec!["docs/note40.txt".into(), "big2.bin".into()],
            base: String::new(),
        }),
        ..opts
    };
    assert_eq!(brar_core::extract(&archive, &picked, &opts).unwrap().files, 2);
    assert!(fs::read(src.join("docs/note40.txt")).unwrap() == fs::read(picked.join("docs/note40.txt")).unwrap());
    assert!(fs::read(src.join("big2.bin")).unwrap() == fs::read(picked.join("big2.bin")).unwrap());
    assert!(!picked.join("big1.bin").exists());
    let _ = fs::remove_dir_all(&root);
}
