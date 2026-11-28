//! Cancelling: the extraction stops soon, ends as cancelled, and the files already extracted
//! stay where they are.

use std::{
    fs,
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

use brar_core::{Error, Options, Progress};
use zip::write::{SimpleFileOptions, ZipWriter};

#[test]
fn cancel_stops_and_keeps_what_was_extracted() {
    let dir = std::env::temp_dir().join(format!("brar-cancel-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let archive = dir.join("many.zip");
    let mut w = ZipWriter::new(fs::File::create(&archive).unwrap());
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut x = 1u32;
    for i in 0..400 {
        w.start_file(format!("f{i:03}.bin"), stored).unwrap();
        let data: Vec<u8> = (0..1 << 20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        w.write_all(&data).unwrap();
    }
    w.finish().unwrap();

    let progress = Arc::new(Progress::default());
    let opts = Options {
        threads: 2,
        progress: Some(progress.clone()),
        ..Options::default()
    };
    let out = dir.join("out");
    let watcher = {
        let progress = progress.clone();
        std::thread::spawn(move || {
            while progress.files() < 5 {
                std::thread::sleep(Duration::from_millis(1));
            }
            progress.cancel();
            Instant::now()
        })
    };
    let result = brar_core::extract(&archive, &out, &opts);
    let stopped = Instant::now();
    let cancelled_at = watcher.join().unwrap();
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(
        stopped.duration_since(cancelled_at) < Duration::from_secs(5),
        "took {:?} to stop",
        stopped - cancelled_at
    );
    // What was done before the cancel is kept, whole; the rest isn't there.
    let kept: Vec<_> = fs::read_dir(&out).unwrap().map(|e| e.unwrap()).collect();
    assert!(kept.len() >= 5 && kept.len() < 400, "{} files kept", kept.len());
    for f in &kept {
        assert_eq!(f.metadata().unwrap().len(), 1 << 20, "{:?} isn't whole", f.file_name());
    }
    let _ = fs::remove_dir_all(&dir);
}
