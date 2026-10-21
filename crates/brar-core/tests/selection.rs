//! Extracting picked entries only, in every format. The archives are made with the 7-Zip and
//! WinRAR command-line tools; a format whose tool isn't installed is skipped.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use brar_core::{Options, Selection};

const SEVEN_ZIP: &str = r"C:\Program Files\7-Zip\7z.exe";
const RAR: &str = r"C:\Program Files\WinRAR\Rar.exe";

/// Relative path -> contents. `big` is over the sink's in-memory size limit.
fn tree() -> Vec<(&'static str, Vec<u8>)> {
    let big: Vec<u8> = (0..12u32 << 20).map(|i| (i % 251) as u8 ^ (i >> 13) as u8).collect();
    vec![
        ("a.txt", b"top level".to_vec()),
        ("docs/b.txt", b"in docs".to_vec()),
        ("docs/sub/c.bin", big),
        ("docs/sub/deeper/d.txt", b"deep".to_vec()),
        ("docs2/e.txt", b"prefix of docs, but not inside it".to_vec()),
        ("other/f.txt", b"elsewhere".to_vec()),
    ]
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("brar-selection-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Builds the test tree and one archive per available tool.
fn archives(tag: &str) -> (PathBuf, Vec<PathBuf>) {
    let root = scratch(tag);
    let src = root.join("tree");
    for (rel, data) in tree() {
        let p = src.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
    let mut made = Vec::new();
    let mut run = |tool: &str, args: &[&str], out: &str| {
        if !Path::new(tool).exists() {
            eprintln!("skipping {out}: {tool} not installed");
            return;
        }
        let ok = Command::new(tool)
            .args(args)
            .arg(root.join(out))
            .arg("*")
            .current_dir(&src)
            .output()
            .unwrap();
        assert!(ok.status.success(), "{tool} failed: {}", String::from_utf8_lossy(&ok.stdout));
        made.push(root.join(out));
    };
    run(SEVEN_ZIP, &["a", "-tzip", "-bso0"], "t.zip");
    run(SEVEN_ZIP, &["a", "-t7z", "-bso0"], "t.7z");
    run(SEVEN_ZIP, &["a", "-t7z", "-ms=off", "-bso0"], "t-nonsolid.7z");
    run(RAR, &["a", "-r", "-ma5", "-idq"], "t.rar");
    run(RAR, &["a", "-r", "-ma5", "-s", "-idq"], "t-solid.rar");
    let tar = root.join("t.tar");
    if Path::new(SEVEN_ZIP).exists() {
        let ok = Command::new(SEVEN_ZIP)
            .args(["a", "-ttar", "-bso0"])
            .arg(&tar)
            .arg("*")
            .current_dir(&src)
            .status();
        assert!(ok.unwrap().success());
        let ok = Command::new(SEVEN_ZIP)
            .args(["a", "-tgzip", "-bso0"])
            .arg(root.join("t.tar.gz"))
            .arg(&tar)
            .status();
        assert!(ok.unwrap().success());
        made.push(root.join("t.tar.gz"));
    }
    (root, made)
}

/// Every file under `dir`, as sorted relative paths with `/`.
fn files_in(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    if dir.exists() {
        walk(dir, dir, &mut out);
    }
    out.sort();
    out
}

fn extract(archive: &Path, out: &Path, items: &[&str], base: &str) -> brar_core::Summary {
    let opts = Options {
        selection: Some(Selection {
            items: items.iter().map(|s| s.to_string()).collect(),
            base: base.into(),
        }),
        ..Options::default()
    };
    brar_core::extract(archive, out, &opts).unwrap_or_else(|e| panic!("{}: {e}", archive.display()))
}

fn check(archive: &Path, got: &Path, expect: &[(&str, &str)]) {
    let contents: Vec<_> = tree().into_iter().collect();
    let want: Vec<String> = expect.iter().map(|(rel, _)| rel.to_string()).collect();
    let mut sorted = want.clone();
    sorted.sort();
    assert_eq!(files_in(got), sorted, "{}", archive.display());
    for (rel, orig) in expect {
        let data = &contents.iter().find(|(n, _)| n == orig).unwrap().1;
        assert!(fs::read(got.join(rel)).unwrap() == *data, "{}: {rel} differs", archive.display());
    }
}

#[test]
fn picked_entries_only() {
    let (root, archives) = archives("extract");
    for archive in &archives {
        let stem = archive.file_name().unwrap().to_string_lossy().replace('.', "_");

        // A file and a folder, full paths. "docs2" must not count as inside "docs".
        let out = root.join(format!("{stem}-full"));
        let s = extract(archive, &out, &["a.txt", "docs/sub"], "");
        check(
            archive,
            &out,
            &[
                ("a.txt", "a.txt"),
                ("docs/sub/c.bin", "docs/sub/c.bin"),
                ("docs/sub/deeper/d.txt", "docs/sub/deeper/d.txt"),
            ],
        );
        assert_eq!(s.files, 3, "{}", archive.display());

        // Picked from inside "docs": written relative to it; backslashes work too.
        let out = root.join(format!("{stem}-base"));
        extract(archive, &out, &["docs\\b.txt", "docs/sub/deeper"], "docs");
        check(archive, &out, &[("b.txt", "docs/b.txt"), ("sub/deeper/d.txt", "docs/sub/deeper/d.txt")]);

        // Just the big file.
        let out = root.join(format!("{stem}-big"));
        extract(archive, &out, &["docs/sub/c.bin"], "docs/sub");
        check(archive, &out, &[("c.bin", "docs/sub/c.bin")]);

        // A name that isn't in the archive extracts nothing.
        let out = root.join(format!("{stem}-none"));
        let s = extract(archive, &out, &["missing.txt"], "");
        assert_eq!(s.files, 0);
        assert!(files_in(&out).is_empty());
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn test_mode_with_selection() {
    let (root, archives) = archives("test");
    for archive in &archives {
        let opts = Options {
            selection: Some(Selection {
                items: vec!["docs".into()],
                base: String::new(),
            }),
            ..Options::default()
        };
        let s = brar_core::test(archive, &opts).unwrap();
        assert_eq!(s.files, 3, "{}", archive.display());
    }
    let _ = fs::remove_dir_all(&root);
}
