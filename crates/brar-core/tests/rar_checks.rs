//! RAR5 cases that used to go to unRAR: BLAKE2sp checksums instead of CRC32 (plain and as a
//! MAC), stored files inside a solid archive (outside its compressed stream), and stored
//! files too big to keep in memory (streamed). Needs WinRAR's Rar.exe; skipped without it.

use std::{fs, path::Path, process::Command};

use brar_core::{ConflictPolicy, Options, Resolution};

const RAR: &str = r"C:\Program Files\WinRAR\Rar.exe";

#[test]
fn blake2_and_stored_files_extract_exactly() {
    if !Path::new(RAR).exists() {
        eprintln!("skipping: {RAR} not installed");
        return;
    }
    let root = std::env::temp_dir().join(format!("brar-rar-checks-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    fs::create_dir_all(src.join("docs")).unwrap();
    for i in 0..20 {
        let text = format!("line {i}: sphinx of black quartz, judge my vow {}\n", i % 3).repeat(150 + i * 11);
        fs::write(src.join("docs").join(format!("d{i:02}.txt")), text).unwrap();
    }
    // Stored by -ms (a "compressed" type), between compressed files in the solid stream; one
    // small, one over the in-memory limit for small files.
    let mut x = 7u64;
    let mut noise = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    };
    fs::write(src.join("docs").join("d05.zip"), noise(300_000)).unwrap();
    fs::write(src.join("big.jpg"), noise(12 << 20)).unwrap();
    fs::write(src.join("text.log"), "a log line that repeats\n".repeat(400_000)).unwrap();

    let cases: [(&str, &[&str]); 5] = [
        ("blake2", &["-htb", "-m3"]),
        ("blake2-solid-mac", &["-htb", "-s", "-psecret", "-m3"]),
        ("solid-stored", &["-s", "-ms", "-m3"]),
        ("solid-stored-enc", &["-s", "-ms", "-hpsecret", "-m3"]),
        ("all-stored-enc", &["-m0", "-psecret", "-htb"]),
    ];
    for (name, switches) in cases {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("t.rar");
        let made = Command::new(RAR)
            .args(["a", "-ma5", "-r", "-idq", "-ep1"])
            .args(switches)
            .arg(&archive)
            .arg(src.join("*"))
            .status()
            .unwrap();
        assert!(made.success(), "{name}: Rar.exe failed");
        let opts = Options {
            threads: 4,
            password: Some("secret".into()),
            on_conflict: ConflictPolicy::Always(Resolution::Cancel),
            ..Options::default()
        };
        let out = dir.join("out");
        assert_eq!(brar_core::extract(&archive, &out, &opts).unwrap().files, 23, "{name}");
        for rel in ["big.jpg", "text.log", "docs/d00.txt", "docs/d05.zip", "docs/d19.txt"] {
            assert!(
                fs::read(src.join(rel)).unwrap() == fs::read(out.join(rel)).unwrap(),
                "{name}: {rel} differs"
            );
        }
    }
    let _ = fs::remove_dir_all(&root);
}
