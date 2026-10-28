//! Creating ZIP archives: every level, files in many pieces, stored and empty files, folders,
//! non-ASCII names and a password. Each archive is extracted with B-Rar and compared with the
//! files, and checked with 7-Zip (`7z t`) where it's installed.

use std::{fs, path::Path, process::Command};

use brar_core::{CreateOptions, Error, Options};

const SEVEN_ZIP: &str = r"C:\Program Files\7-Zip\7z.exe";

/// Relative path -> contents.
fn tree() -> Vec<(&'static str, Vec<u8>)> {
    let mut x = 7u64;
    let noise: Vec<u8> = (0..2_500_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    vec![
        // Compressible, over the piece size: several pieces in one Deflate stream.
        ("docs/text.txt", "B-Rar makes ZIP archives on every core. ".repeat(80_000).into_bytes()),
        // Incompressible, several pieces.
        ("docs/noise.bin", noise),
        ("docs/sub/small.txt", b"small".to_vec()),
        ("docs/photo.jpg", b"not really a jpeg, but stored by its extension".repeat(100)),
        ("empty.txt", Vec::new()),
        ("ünïcödé 名前.txt", "non-ASCII name".into()),
    ]
}

fn make_tree(root: &Path) {
    for (name, data) in tree() {
        let p = root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
    fs::create_dir_all(root.join("docs/empty dir")).unwrap();
}

fn check(archive: &Path, out: &Path, password: Option<&str>) {
    let opts = Options {
        password: password.map(str::to_owned),
        ..Default::default()
    };
    brar_core::extract(archive, out, &opts).unwrap_or_else(|e| panic!("{}: {e}", archive.display()));
    for (name, data) in tree() {
        assert!(fs::read(out.join("src").join(name)).unwrap() == data, "{}: {name}", archive.display());
    }
    assert!(out.join("src/docs/empty dir").is_dir(), "{}: empty folder", archive.display());
    // 7-Zip takes only ASCII passwords for ZIP archives ("The parameter is incorrect"); B-Rar
    // uses UTF-8 for others, as WinZip does.
    if Path::new(SEVEN_ZIP).exists() && password.is_none_or(str::is_ascii) {
        let pw = format!("-p{}", password.unwrap_or(""));
        let t = Command::new(SEVEN_ZIP).args(["t", "-bso0"]).arg(pw).arg(archive).output().unwrap();
        assert!(
            t.status.success(),
            "7-Zip rejects {}: {}",
            archive.display(),
            String::from_utf8_lossy(&t.stderr)
        );
    }
}

#[test]
fn creates_zip_archives() {
    let root = std::env::temp_dir().join(format!("brar-create-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    make_tree(&src);
    let inputs = vec![src.clone()];
    for (name, level, threads, password) in [
        ("store", 0, 0, None),
        ("fast", 1, 0, None),
        ("normal", 6, 0, None),
        ("best-one-thread", 9, 1, None),
        ("password", 6, 0, Some("password")),
        ("password-unicode", 6, 0, Some("pässwörd")),
    ] {
        let archive = root.join(format!("{name}.zip"));
        let opts = CreateOptions {
            level,
            threads,
            password: password.map(str::to_owned),
            ..Default::default()
        };
        let s = brar_core::create(&archive, &inputs, &opts).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(s.files, tree().len() as u64, "{name}");
        check(&archive, &root.join(format!("out-{name}")), password);
        if password.is_some() {
            let wrong = brar_core::test(
                &archive,
                &Options {
                    password: Some("wrong".into()),
                    ..Default::default()
                },
            );
            assert!(matches!(wrong, Err(Error::WrongPassword)), "{name}: {:?}", wrong.err());
        }
    }
    // Compressing in pieces costs little: the text file, in pieces, against one stream.
    let sizes: Vec<u64> = ["fast", "normal"]
        .iter()
        .map(|n| fs::metadata(root.join(format!("{n}.zip"))).unwrap().len())
        .collect();
    assert!(sizes[1] <= sizes[0], "level 6 bigger than level 1: {sizes:?}");
    // An existing archive is left alone unless replacing it was asked for.
    let normal = root.join("normal.zip");
    assert!(brar_core::create(&normal, &inputs, &CreateOptions::default()).is_err());
    brar_core::create(
        &normal,
        &inputs,
        &CreateOptions {
            overwrite: true,
            ..Default::default()
        },
    )
    .unwrap();
    fs::remove_dir_all(&root).unwrap();
}
