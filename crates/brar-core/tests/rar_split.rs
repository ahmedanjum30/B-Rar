//! A RAR5 archive with several big files: those go to unRAR (split across its worker threads)
//! while B-Rar's own decoder handles the small ones at the same time. Each file must be
//! written exactly once. Needs WinRAR's Rar.exe; skipped without it.

use std::{fs, path::Path, process::Command};

use brar_core::{ConflictPolicy, Options, Resolution};

const RAR: &str = r"C:\Program Files\WinRAR\Rar.exe";

#[test]
fn big_and_small_files_each_written_once() {
    if !Path::new(RAR).exists() {
        eprintln!("skipping: {RAR} not installed");
        return;
    }
    let root = std::env::temp_dir().join(format!("brar-rar-split-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    // Two files over the 64 MB "big file" line, so unRAR's share needs two workers.
    for (name, seed) in [("big1.bin", 7u32), ("big2.bin", 13)] {
        let data: Vec<u8> = (0..70u32 << 20).map(|i| (i.wrapping_mul(seed) >> 9) as u8 ^ (i % 253) as u8).collect();
        fs::write(src.join(name), data).unwrap();
    }
    for i in 0..40 {
        fs::write(src.join(format!("small{i}.txt")), format!("small file {i}\n").repeat(500)).unwrap();
    }
    let archive = root.join("t.rar");
    let made = Command::new(RAR)
        .args(["a", "-ma5", "-m1", "-idq", "-ep1"])
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
    let tested = brar_core::test(&archive, &opts).unwrap();
    assert_eq!(tested.files, 42, "test mode counted some files twice");

    let out = root.join("out");
    // Cancel-on-conflict: a second writer for the same file fails the extraction.
    let summary = brar_core::extract(&archive, &out, &opts).unwrap();
    assert_eq!(summary.files, 42);
    for e in fs::read_dir(&src).unwrap() {
        let e = e.unwrap();
        assert!(
            fs::read(e.path()).unwrap() == fs::read(out.join(e.file_name())).unwrap(),
            "{:?} differs",
            e.file_name()
        );
    }
    let _ = fs::remove_dir_all(&root);
}
