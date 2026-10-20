//! Where decoded entries go: files on disk, or nowhere (test mode).
//!
//! On Windows, creating and closing files often costs more than decompressing them, so small
//! files can be handed to a dedicated writer pool while the decoder keeps going.
//!
//! Files are created with "fail if it exists", so the overwrite prompt costs nothing unless
//! there really is a conflict.

use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::SystemTime,
};

use crate::{
    Conflict, ConflictPolicy, Error, IoContext, Options, Progress, Resolution, Result, Summary,
    path::safe_join,
};

/// Files up to this size are buffered in memory and written in the background.
pub(crate) const SMALL_FILE: u64 = 8 << 20;
/// Cap on bytes buffered for background writes, so memory stays bounded.
const MAX_IN_FLIGHT: u64 = 512 << 20;
const COPY_BUF: usize = 1 << 20;

thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0; COPY_BUF]);
}

pub(crate) struct Sink {
    dest: Option<PathBuf>,
    writers: rayon::ThreadPool,
    shared: Arc<Shared>,
}

struct Shared {
    progress: Option<Arc<Progress>>,
    policy: ConflictPolicy,
    /// An "apply to all" answer, once given.
    sticky: Mutex<Option<Resolution>>,
    /// Serializes questions so only one prompt is open at a time.
    asking: Mutex<()>,
    cancelled: AtomicBool,
    /// (pending jobs, pending bytes)
    in_flight: Mutex<(usize, u64)>,
    drained: Condvar,
    error: Mutex<Option<Error>>,
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    skipped: AtomicU64,
    kept: AtomicU64,
}

impl Shared {
    fn record_file(&self, bytes: u64) {
        self.files.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        if let Some(p) = &self.progress {
            p.file_done();
        }
    }

    fn advance(&self, bytes: u64) {
        if let Some(p) = &self.progress {
            p.advance(bytes);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed) || self.progress.as_ref().is_some_and(|p| p.is_cancelled())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(p) = &self.progress {
            p.cancel();
        }
    }

    /// Fails after a cancel, and blocks while paused.
    fn check_running(&self) -> Result<()> {
        let running = self.progress.as_ref().is_none_or(|p| p.wait_if_paused());
        if !running || self.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }

    /// Decides what to do with an existing `path`: `Some(path to write)` or `None` to skip.
    fn resolve(&self, path: &Path, size: Option<u64>, mtime: Option<SystemTime>) -> Result<Option<PathBuf>> {
        let resolution = match &self.policy {
            ConflictPolicy::Always(r) => *r,
            ConflictPolicy::Ask(ask) => {
                let _one_at_a_time = self.asking.lock().unwrap();
                let sticky = *self.sticky.lock().unwrap();
                match sticky {
                    Some(r) => r,
                    None => {
                        self.check_running()?;
                        let decision = ask(&Conflict { path, new_size: size, new_modified: mtime });
                        if decision.apply_to_all {
                            *self.sticky.lock().unwrap() = Some(decision.resolution);
                        }
                        decision.resolution
                    }
                }
            }
        };
        match resolution {
            Resolution::Overwrite => {
                make_writable(path);
                Ok(Some(path.to_path_buf()))
            }
            Resolution::Rename => Ok(Some(free_name(path))),
            Resolution::Skip => {
                self.kept.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
            Resolution::Cancel => {
                self.cancel();
                Err(Error::Cancelled)
            }
        }
    }

    /// Opens `path` for a new file, consulting the conflict policy if it exists.
    /// `None` means the user chose to keep the existing file.
    fn open(&self, path: &Path, size: Option<u64>, mtime: Option<SystemTime>) -> Result<Option<File>> {
        match create_new(path) {
            Ok(f) => return Ok(Some(f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).at(path),
        }
        let Some(target) = self.resolve(path, size, mtime)? else {
            return Ok(None);
        };
        if target == path {
            return File::create(path).map(Some).at(path);
        }
        // A renamed target can race with another thread picking the same free name.
        let mut target = target;
        loop {
            match create_new(&target) {
                Ok(f) => return Ok(Some(f)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => target = free_name(path),
                Err(e) => return Err(e).at(&target),
            }
        }
    }
}

impl Sink {
    pub(crate) fn new(dest: Option<&Path>, opts: &Options) -> Result<Self> {
        if let Some(dest) = dest {
            fs::create_dir_all(dest).at(dest)?;
        }
        let writers = rayon::ThreadPoolBuilder::new()
            .num_threads(opts.thread_count())
            .thread_name(|i| format!("B-Rar-write-{i}"))
            .build()
            .map_err(|e| Error::RawIo(io::Error::other(e)))?;
        Ok(Self {
            dest: dest.map(Path::to_path_buf),
            writers,
            shared: Arc::new(Shared {
                progress: opts.progress.clone(),
                policy: opts.on_conflict.clone(),
                sticky: Mutex::default(),
                asking: Mutex::default(),
                cancelled: AtomicBool::default(),
                in_flight: Mutex::default(),
                drained: Condvar::new(),
                error: Mutex::default(),
                files: AtomicU64::default(),
                dirs: AtomicU64::default(),
                bytes: AtomicU64::default(),
                skipped: AtomicU64::default(),
                kept: AtomicU64::default(),
            }),
        })
    }

    pub(crate) fn progress(&self) -> Option<&Arc<Progress>> {
        self.shared.progress.as_ref()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.shared.is_cancelled()
    }

    /// Tells the UI how many uncompressed bytes the archive holds.
    pub(crate) fn set_total(&self, bytes: u64) {
        if let Some(p) = self.progress() {
            p.set_total(bytes);
        }
    }

    /// Marks `name` as the entry being processed.
    pub(crate) fn start(&self, name: &str) {
        if let Some(p) = self.progress() {
            p.start_file(name);
        }
    }

    /// Resolves `name` under the destination; `None` in test mode.
    pub(crate) fn target(&self, name: &str) -> Result<Option<PathBuf>> {
        self.dest.as_deref().map(|d| safe_join(d, name)).transpose()
    }

    /// For writers outside the sink (unRAR): where to write `path`, after the conflict
    /// policy has had its say. `None` means keep the existing file.
    pub(crate) fn claim(&self, path: &Path, size: Option<u64>, mtime: Option<SystemTime>) -> Result<Option<PathBuf>> {
        if fs::symlink_metadata(path).is_err() {
            return Ok(Some(path.to_path_buf()));
        }
        self.shared.resolve(path, size, mtime)
    }

    pub(crate) fn dir(&self, name: &str) -> Result<()> {
        self.check()?;
        if let Some(path) = self.target(name)? {
            fs::create_dir_all(&path).at(&path)?;
        }
        self.shared.dirs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn skip(&self) {
        self.shared.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a file that something else (e.g. unrar) wrote or verified. Its bytes are
    /// expected to have been reported to [`Progress`] by the caller as they were decoded.
    pub(crate) fn count_file(&self, bytes: u64) {
        self.shared.record_file(bytes);
    }

    /// Streams `reader` to the entry's file on the calling thread. Fully reading the reader
    /// is what lets zip/7z verify their CRCs, so test mode and skipped files drain it too.
    pub(crate) fn file(
        &self,
        name: &str,
        size: Option<u64>,
        mtime: Option<SystemTime>,
        reader: &mut dyn Read,
    ) -> Result<()> {
        self.check()?;
        self.start(name);
        let shared = &*self.shared;
        let Some(path) = self.target(name)? else {
            let n = copy(reader, &mut io::sink(), shared)?;
            shared.record_file(n);
            return Ok(());
        };
        let Some(mut file) = shared.open(&path, size, mtime)? else {
            // Keeping the existing file; the stream still has to be consumed in order.
            copy(reader, &mut io::sink(), shared)?;
            return Ok(());
        };
        let n = copy(reader, &mut file, shared).at(&path)?;
        set_mtime(&file, mtime);
        shared.record_file(n);
        Ok(())
    }

    /// Like [`Sink::file`], but small files are read into memory here and written on the
    /// writer pool, so a sequential decoder never waits on file creation.
    pub(crate) fn file_buffered(
        &self,
        name: &str,
        size: u64,
        mtime: Option<SystemTime>,
        reader: &mut dyn Read,
    ) -> Result<()> {
        if self.dest.is_none() || size > SMALL_FILE {
            return self.file(name, Some(size), mtime, reader);
        }
        let mut data = Vec::with_capacity(size as usize);
        reader.read_to_end(&mut data)?;
        self.file_owned(name, mtime, data)
    }

    /// Writes already-decoded bytes on the writer pool.
    pub(crate) fn file_owned(&self, name: &str, mtime: Option<SystemTime>, data: Vec<u8>) -> Result<()> {
        self.write_owned(name, mtime, data, true)
    }

    /// [`Sink::file_owned`] for bytes the decoder already reported to [`Progress`].
    pub(crate) fn file_owned_reported(&self, name: &str, mtime: Option<SystemTime>, data: Vec<u8>) -> Result<()> {
        self.write_owned(name, mtime, data, false)
    }

    fn write_owned(&self, name: &str, mtime: Option<SystemTime>, data: Vec<u8>, report: bool) -> Result<()> {
        self.check()?;
        self.start(name);
        let len = data.len() as u64;
        let Some(path) = self.target(name)? else {
            if report {
                self.shared.advance(len);
            }
            self.count_file(len);
            return Ok(());
        };
        {
            let mut guard = self.shared.in_flight.lock().unwrap();
            while guard.0 > 0 && guard.1 + len > MAX_IN_FLIGHT {
                guard = self.shared.drained.wait(guard).unwrap();
            }
            guard.0 += 1;
            guard.1 += len;
        }
        let shared = Arc::clone(&self.shared);
        self.writers.spawn(move || {
            let result = shared.open(&path, Some(len), mtime).and_then(|file| {
                let Some(mut file) = file else {
                    return Ok(false);
                };
                file.write_all(&data).at(&path)?;
                set_mtime(&file, mtime);
                Ok(true)
            });
            if report {
                shared.advance(len);
            }
            match result {
                Ok(true) => shared.record_file(len),
                Ok(false) => {}
                Err(e) => {
                    shared.error.lock().unwrap().get_or_insert(e);
                }
            }
            drop(data);
            let mut guard = shared.in_flight.lock().unwrap();
            guard.0 -= 1;
            guard.1 -= len;
            shared.drained.notify_all();
        });
        Ok(())
    }

    /// Fails fast after a cancel or a failed background write; blocks while paused.
    pub(crate) fn check(&self) -> Result<()> {
        self.shared.check_running()?;
        match self.shared.error.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Waits for background writes and returns totals.
    pub(crate) fn finish(&self) -> Result<Summary> {
        let mut guard = self.shared.in_flight.lock().unwrap();
        while guard.0 > 0 {
            guard = self.shared.drained.wait(guard).unwrap();
        }
        drop(guard);
        if let Some(e) = self.shared.error.lock().unwrap().take() {
            return Err(e);
        }
        let s = &self.shared;
        Ok(Summary {
            files: s.files.load(Ordering::Relaxed),
            dirs: s.dirs.load(Ordering::Relaxed),
            bytes: s.bytes.load(Ordering::Relaxed),
            skipped: s.skipped.load(Ordering::Relaxed),
            kept_existing: s.kept.load(Ordering::Relaxed),
        })
    }
}

/// `create_new`, making parent directories only when the first attempt fails: that skips a
/// directory syscall per file in the common case.
fn create_new(path: &Path) -> io::Result<File> {
    let open = || OpenOptions::new().write(true).create_new(true).open(path);
    match open() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            open()
        }
        other => other,
    }
}

/// "report.pdf" -> "report (1).pdf", the first name that doesn't exist yet.
fn free_name(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .unwrap()
}

/// Overwriting a read-only file would fail; clear the flag first, as WinRAR does.
fn make_writable(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }
}

fn set_mtime(file: &File, mtime: Option<SystemTime>) {
    // Best effort: a timestamp the filesystem rejects is not worth failing the extraction.
    if let Some(t) = mtime {
        let _ = file.set_modified(t);
    }
}

fn copy(reader: &mut dyn Read, writer: &mut dyn Write, shared: &Shared) -> io::Result<u64> {
    BUF.with_borrow_mut(|buf| {
        let mut total = 0;
        loop {
            // Decoders hand back small pieces (~9 KB for Deflate); fill the whole buffer before
            // writing, or a big file costs ~100,000 write calls instead of ~1,000.
            let mut n = 0;
            while n < buf.len() {
                match reader.read(&mut buf[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if n == 0 {
                return Ok(total);
            }
            writer.write_all(&buf[..n])?;
            total += n as u64;
            shared.advance(n as u64);
            if shared.check_running().is_err() {
                return Err(io::Error::other("cancelled"));
            }
        }
    })
}
