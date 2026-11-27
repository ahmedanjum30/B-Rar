//! Parts of a multi-part archive in different folders: the missing part is asked for once,
//! later parts are found next to it without asking, and declining stops with the part named.
//! A RAR part that isn't at hand is only asked for once everything before it is extracted (it
//! may still be downloading). .001 split files go through B-Rar's reader.
//! The RAR cases need WinRAR's Rar.exe and are skipped without it.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use brar_core::{Error, Options, PartFinder};

const RAR: &str = r"C:\Program Files\WinRAR\Rar.exe";

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("brar-parts-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

/// Pseudo-random bytes, so the archive doesn't shrink below several volumes.
fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// Moves every part from the second on to `elsewhere`, and returns a finder that answers
/// with `elsewhere/<name>` and records what it was asked for, and whether `ready` (a file
/// wholly in part 1) was already extracted by then.
fn scatter(parts: &[PathBuf], elsewhere: &Path, ready: PathBuf) -> (PartFinder, Arc<Mutex<Vec<(PathBuf, bool)>>>) {
    fs::create_dir_all(elsewhere).unwrap();
    for p in &parts[1..] {
        fs::rename(p, elsewhere.join(p.file_name().unwrap())).unwrap();
    }
    let asked = Arc::new(Mutex::new(Vec::new()));
    let (log, dir) = (Arc::clone(&asked), elsewhere.to_path_buf());
    let finder = PartFinder::new(move |expected: &Path| {
        // Small files are written in the background: give the write a moment.
        let out = (0..300).any(|_| {
            let there = ready.is_file();
            if !there {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            there
        });
        log.lock().unwrap().push((expected.to_path_buf(), out));
        Some(dir.join(expected.file_name()?))
    });
    (finder, asked)
}

fn rar_parts(dir: &Path) -> Vec<PathBuf> {
    let mut parts: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy().to_ascii_lowercase();
            n.ends_with(".rar") || (n.len() > 4 && n.as_bytes()[n.len() - 4] == b'.' && n[n.len() - 3..].starts_with('r'))
        })
        .collect();
    parts.sort();
    parts
}

fn check_rar(name: &str, format: &str, first: &str) {
    if !Path::new(RAR).exists() {
        eprintln!("skipping: {RAR} not installed");
        return;
    }
    let root = root(name);
    let (src, here) = (root.join("src"), root.join("here"));
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&here).unwrap();
    let data = noise(3 << 20, 99);
    // `a.bin` comes first and fits in part 1; `data.bin` runs on through the other parts.
    let first_file = noise(200_000, 5);
    fs::write(src.join("a.bin"), &first_file).unwrap();
    fs::write(src.join("data.bin"), &data).unwrap();
    fs::write(src.join("note.txt"), "hello\n").unwrap();
    let made = Command::new(RAR)
        .args(["a", format, "-m0", "-v1m", "-idq", "-ep1"])
        .arg(here.join("t.rar"))
        .arg(src.join("*"))
        .status()
        .unwrap();
    assert!(made.success());
    let parts = rar_parts(&here);
    assert!(parts.len() >= 3, "{name}: expected several volumes, got {parts:?}");
    let out = root.join("out");
    let (finder, asked) = scatter(&parts, &root.join("elsewhere"), out.join("a.bin"));

    let opts = Options {
        find_part: finder,
        ..Options::default()
    };
    brar_core::extract(&here.join(first), &out, &opts).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(fs::read(out.join("data.bin")).unwrap(), data, "{name}");
    assert_eq!(fs::read(out.join("a.bin")).unwrap(), first_file, "{name}");
    // Asked once, for part 2, after part 1's file was out; part 3 on was found next to it.
    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 1, "{name}: asked {asked:?}");
    assert_eq!(asked[0].0, parts[1], "{name}");
    assert!(asked[0].1, "{name}: part 2 was asked for before part 1's file was extracted");

    // Declining stops, naming the part.
    let opts = Options {
        find_part: PartFinder::new(|_| None),
        ..Options::default()
    };
    match brar_core::extract(&here.join(first), &root.join("out2"), &opts) {
        Err(Error::MissingVolume(p)) => assert_eq!(p, parts[1], "{name}"),
        other => panic!("{name}: expected a missing part, got {other:?}"),
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rar5_parts_elsewhere() {
    check_rar("rar5", "-ma5", "t.part1.rar");
}

#[test]
fn rar4_parts_elsewhere() {
    // RAR4 goes to unRAR, which asks through its volume callback.
    check_rar("rar4", "-ma4", "t.part1.rar");
}

#[test]
fn split_parts_elsewhere() {
    let root = root("split");
    let here = root.join("here");
    fs::create_dir_all(&here).unwrap();
    // A tar split into three .001 pieces; the highest one stays next to the first, so the
    // gap is known and asked for.
    let mut tar = tar::Builder::new(Vec::new());
    let data = noise(300_000, 7);
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_cksum();
    tar.append_data(&mut h, "data.bin", &data[..]).unwrap();
    let tar = tar.into_inner().unwrap();
    let third = tar.len().div_ceil(3);
    for (i, piece) in tar.chunks(third).enumerate() {
        fs::write(here.join(format!("t.tar.{:03}", i + 1)), piece).unwrap();
    }
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::rename(here.join("t.tar.002"), elsewhere.join("t.tar.002")).unwrap();
    let asked = Arc::new(Mutex::new(0));
    let (log, dir) = (Arc::clone(&asked), elsewhere.clone());
    let opts = Options {
        find_part: PartFinder::new(move |expected: &Path| {
            *log.lock().unwrap() += 1;
            Some(dir.join(expected.file_name()?))
        }),
        ..Options::default()
    };
    let out = root.join("out");
    brar_core::extract(&here.join("t.tar.001"), &out, &opts).unwrap();
    assert_eq!(fs::read(out.join("data.bin")).unwrap(), data);
    assert_eq!(*asked.lock().unwrap(), 1);
    let _ = fs::remove_dir_all(&root);
}
