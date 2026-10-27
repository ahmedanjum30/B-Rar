//! ZIP entries compressed with LZMA (and XZ, if 7-Zip writes it), plain and encrypted
//! (ZipCrypto, WinZip AES): B-Rar decrypts and decodes these itself. The archives are made with
//! 7-Zip's command-line tool; skipped if it isn't installed.

use std::{fs, path::Path, process::Command};

use brar_core::{Error, Options};

const SEVEN_ZIP: &str = r"C:\Program Files\7-Zip\7z.exe";

fn files() -> Vec<(&'static str, Vec<u8>)> {
    let big: Vec<u8> = (0..12u32 << 20).map(|i| (i % 251) as u8 ^ (i >> 13) as u8).collect();
    vec![
        ("a.txt", "LZMA in a ZIP. ".repeat(5000).into_bytes()),
        ("sub/big.bin", big),
        ("empty.txt", Vec::new()),
    ]
}

fn opts(password: Option<&str>) -> Options {
    Options {
        password: password.map(str::to_owned),
        ..Default::default()
    }
}

#[test]
fn lzma_and_xz_entries() {
    if !Path::new(SEVEN_ZIP).exists() {
        eprintln!("7-Zip not installed: skipped");
        return;
    }
    let root = std::env::temp_dir().join(format!("brar-zip-lzma-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    for (name, data) in files() {
        let p = src.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
    let cases: [(&str, &[&str], Option<&str>); 5] = [
        ("lzma", &["-mm=LZMA"], None),
        ("zipcrypto", &["-mm=LZMA", "-pabc"], Some("abc")),
        ("aes128", &["-mm=LZMA", "-pabc", "-mem=AES128"], Some("abc")),
        ("aes256", &["-mm=LZMA", "-pabc", "-mem=AES256"], Some("abc")),
        ("xz", &["-mm=XZ"], None),
    ];
    for (name, args, password) in cases {
        let archive = root.join(format!("{name}.zip"));
        let made = Command::new(SEVEN_ZIP)
            .arg("a")
            .arg("-tzip")
            .args(args)
            .arg("-bso0")
            .arg(&archive)
            .arg("*")
            .current_dir(&src)
            .output()
            .unwrap();
        if !made.status.success() {
            // 7-Zip versions differ in the ZIP methods they write (XZ).
            eprintln!("7-Zip can't write {name}: skipped");
            continue;
        }
        let out = root.join(format!("out-{name}"));
        brar_core::extract(&archive, &out, &opts(password)).unwrap_or_else(|e| panic!("{name}: {e}"));
        for (file, data) in files() {
            assert!(fs::read(out.join(file)).unwrap() == data, "{name}: {file}");
        }
        if password.is_some() {
            assert!(
                matches!(brar_core::test(&archive, &opts(None)), Err(Error::PasswordRequired)),
                "{name}: no password"
            );
            let wrong = brar_core::test(&archive, &opts(Some("abd")));
            assert!(matches!(wrong, Err(Error::WrongPassword)), "{name}: wrong password: {:?}", wrong.err());
        }
    }
    fs::remove_dir_all(&root).unwrap();
}
