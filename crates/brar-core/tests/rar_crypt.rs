//! Encrypted and multi-volume RAR5 archives through B-Rar's own decoder: encrypted data,
//! encrypted headers, files split across volumes, solid streams across volumes (the two-phase
//! engine), stored files padded to AES blocks. Needs WinRAR's Rar.exe; skipped without it.

use std::{fs, path::Path, process::Command};

use brar_core::{ConflictPolicy, Options, Resolution};

const RAR: &str = r"C:\Program Files\WinRAR\Rar.exe";

#[test]
fn encrypted_volumes_extract_exactly() {
    if !Path::new(RAR).exists() {
        eprintln!("skipping: {RAR} not installed");
        return;
    }
    let root = std::env::temp_dir().join(format!("brar-rar-crypt-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    fs::create_dir_all(src.join("docs")).unwrap();
    for i in 0..30 {
        let text = format!("note {i}: pack my box with five dozen liquor jugs {}\n", i % 5).repeat(100 + i * 17);
        fs::write(src.join("docs").join(format!("n{i:02}.txt")), text).unwrap();
    }
    // Bigger than a volume even compressed (random bytes, with some repeats), so they're
    // split across several.
    for (name, mut x) in [("a.bin", 13u64), ("b.bin", 41)] {
        let data: Vec<u8> = (0..3u32 << 20)
            .map(|i| {
                if i % 4096 < 1024 {
                    return (i % 7) as u8;
                }
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        fs::write(src.join(name), data).unwrap();
    }
    // Odd sizes, so stored data needs padding.
    fs::write(src.join("odd.txt"), b"seventeen bytes!!").unwrap();

    let cases: [(&str, &[&str]); 4] = [
        ("headers", &["-hpsecret", "-v700k", "-m3"]),
        ("solid", &["-psecret", "-s", "-v700k", "-m3"]),
        ("stored", &["-psecret", "-v700k", "-m0"]),
        ("plain-volumes", &["-v500k", "-m2"]),
    ];
    for (name, switches) in cases {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let made = Command::new(RAR)
            .args(["a", "-ma5", "-r", "-idq", "-ep1"])
            .args(switches)
            .arg(dir.join("t.rar"))
            .arg(src.join("*"))
            .status()
            .unwrap();
        assert!(made.success(), "{name}: Rar.exe failed");
        let mut parts: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        parts.sort();
        assert!(parts.len() > 2, "{name}: expected several volumes");
        let first = parts[0].clone(); // t.part01.rar

        let opts = Options {
            threads: 4,
            password: Some("secret".into()),
            on_conflict: ConflictPolicy::Always(Resolution::Cancel),
            ..Options::default()
        };
        let out = dir.join("out");
        assert_eq!(brar_core::extract(&first, &out, &opts).unwrap().files, 33, "{name}");
        for rel in ["a.bin", "b.bin", "odd.txt", "docs/n00.txt", "docs/n17.txt", "docs/n29.txt"] {
            assert!(
                fs::read(src.join(rel)).unwrap() == fs::read(out.join(rel)).unwrap(),
                "{name}: {rel} differs"
            );
        }
        assert_eq!(fs::read_dir(out.join("docs")).unwrap().count(), 30, "{name}");
    }
    let _ = fs::remove_dir_all(&root);
}
