//! brar-core, B-Rar's engine: fast, parallel archive reading (zip, 7z, rar, tar and single-stream codecs).

mod format;
mod path;
mod rar;
mod rar5;
mod sevenz;
mod sink;
mod stream;
mod time;
mod volumes;
mod whole;
mod zip;

use std::{
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

pub use format::{Codec, Format, detect};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    RawIo(#[from] io::Error),
    #[error("zip: {0}")]
    Zip(#[from] ::zip::result::ZipError),
    #[error("7z: {0}")]
    SevenZ(#[from] sevenz_rust2::Error),
    #[error("rar: {0}")]
    Rar(#[from] unrar::error::UnrarError),
    #[error("unrecognized archive format")]
    UnknownFormat,
    #[error("unsafe path in archive: {0:?}")]
    UnsafePath(String),
    #[error("cancelled")]
    Cancelled,
    #[error("the archive is encrypted; a password is required")]
    PasswordRequired,
    #[error("wrong password")]
    WrongPassword,
    #[error("\"{0}\" is damaged (checksum mismatch); the archive is corrupt or incomplete")]
    Damaged(String),
    #[error("missing volume: {} (all parts must be in the same folder)", .0.display())]
    MissingVolume(PathBuf),
    #[error("not supported: {0}")]
    Unsupported(&'static str),
}

impl Error {
    /// Maps each library's way of saying "password needed / wrong password" onto ours.
    fn normalize(self) -> Self {
        use unrar::error::Code;
        match self {
            Error::Zip(::zip::result::ZipError::UnsupportedArchive(msg)) if msg == ::zip::result::ZipError::PASSWORD_REQUIRED => {
                Error::PasswordRequired
            }
            Error::Zip(::zip::result::ZipError::InvalidPassword) => Error::WrongPassword,
            Error::SevenZ(sevenz_rust2::Error::PasswordRequired) => Error::PasswordRequired,
            Error::SevenZ(sevenz_rust2::Error::MaybeBadPassword(_)) => Error::WrongPassword,
            Error::Rar(e) if matches!(e.code, Code::MissingPassword) => Error::PasswordRequired,
            Error::Rar(e) if matches!(e.code, Code::BadPassword) => Error::WrongPassword,
            other => other,
        }
    }
}

impl Error {
    /// Windows refused to create or write a file: typically a folder only administrators can
    /// write to, such as Program Files. Extracting elevated would succeed.
    pub fn is_access_denied(&self) -> bool {
        const ERROR_ACCESS_DENIED: i32 = 5;
        let denied = |e: &io::Error| e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(ERROR_ACCESS_DENIED);
        match self {
            Error::Io { source, .. } | Error::RawIo(source) => denied(source),
            // unRAR writes big files itself and only reports "couldn't create".
            Error::Rar(e) => matches!(e.code, unrar::error::Code::ECreate),
            _ => false,
        }
    }

    /// Corrupt-data errors. On an encrypted entry, with a password supplied, these almost
    /// always mean the password is wrong (ZipCrypto and RAR4 can't check it up front).
    pub(crate) fn is_bad_data(&self) -> bool {
        use unrar::error::Code;
        match self {
            Error::Io { source, .. } | Error::RawIo(source) => source.kind() == io::ErrorKind::InvalidData,
            Error::SevenZ(sevenz_rust2::Error::ChecksumVerificationFailed) => true,
            Error::SevenZ(sevenz_rust2::Error::Io(e, _)) => e.kind() == io::ErrorKind::InvalidData,
            Error::Zip(::zip::result::ZipError::InvalidArchive(_)) => true,
            Error::Rar(e) => matches!(e.code, Code::BadData),
            _ => false,
        }
    }

    pub(crate) fn wrong_password_if(self, encrypted_with_password: bool) -> Self {
        if encrypted_with_password && self.is_bad_data() {
            Error::WrongPassword
        } else {
            self
        }
    }

    /// Names the damaged `entry` for corrupt-data errors (or blames the password, see above).
    pub(crate) fn for_entry(self, entry: &str, encrypted_with_password: bool) -> Self {
        match self.wrong_password_if(encrypted_with_password) {
            e if e.is_bad_data() => Error::Damaged(entry.to_owned()),
            e => e,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Worker threads; 0 means one per logical CPU.
    pub threads: usize,
    pub password: Option<String>,
    /// Live counters for a UI; also how a running extraction is paused or cancelled.
    pub progress: Option<Arc<Progress>>,
    /// What to do when an extracted file already exists.
    pub on_conflict: ConflictPolicy,
    /// Extract only these entries; `None` extracts everything.
    pub selection: Option<Selection>,
}

/// Part of an archive to extract, as chosen in a file browser.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// Archive paths of files and folders; a folder includes everything under it. Either
    /// slash works.
    pub items: Vec<String>,
    /// Folder the paths are made relative to when writing (the folder being browsed), so
    /// extracting `docs/a.txt` from inside `docs` writes `a.txt`. Empty keeps full paths.
    pub base: String,
}

/// Archive paths compare with `/` separators, no leading slash and no leading `./` (common
/// in tarballs), whichever the format uses.
pub fn normalize_name(name: &str) -> String {
    let name = name.replace('\\', "/");
    let mut rest = name.trim_matches('/');
    while let Some(r) = rest.strip_prefix("./") {
        rest = r.trim_start_matches('/');
    }
    if rest == "." { String::new() } else { rest.to_owned() }
}

impl Options {
    pub(crate) fn thread_count(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        }
    }

    pub(crate) fn pool(&self) -> Result<rayon::ThreadPool> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(self.thread_count())
            .thread_name(|i| format!("B-Rar-decode-{i}"))
            .build()
            .map_err(|e| Error::RawIo(io::Error::other(e)))
    }
}

/// An extracted file whose destination already exists.
#[derive(Debug)]
pub struct Conflict<'a> {
    pub path: &'a Path,
    /// Size and time of the file in the archive, when known.
    pub new_size: Option<u64>,
    pub new_modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Replace the existing file.
    Overwrite,
    /// Keep the existing file and don't extract this one.
    Skip,
    /// Extract under a free name: "name (1).ext".
    Rename,
    /// Stop the whole extraction.
    Cancel,
}

#[derive(Debug, Clone, Copy)]
pub struct Decision {
    pub resolution: Resolution,
    /// Use this answer for every later conflict without asking again.
    pub apply_to_all: bool,
}

pub type AskFn = dyn Fn(&Conflict<'_>) -> Decision + Send + Sync;

/// How conflicts are resolved. `Ask` callbacks are called one at a time, from worker threads,
/// and may block (e.g. on a dialog).
#[derive(Clone)]
pub enum ConflictPolicy {
    Always(Resolution),
    Ask(Arc<AskFn>),
}

impl Default for ConflictPolicy {
    fn default() -> Self {
        ConflictPolicy::Always(Resolution::Overwrite)
    }
}

impl fmt::Debug for ConflictPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConflictPolicy::Always(r) => write!(f, "Always({r:?})"),
            ConflictPolicy::Ask(_) => f.write_str("Ask(..)"),
        }
    }
}

/// Counters updated while an archive is extracted or tested. Share it with a UI thread
/// through [`Options::progress`] and poll it; it also pauses and cancels the run.
#[derive(Debug, Default)]
pub struct Progress {
    total: AtomicU64,
    done: AtomicU64,
    files: AtomicU64,
    cancelled: AtomicBool,
    paused: AtomicBool,
    current: Mutex<String>,
}

impl Progress {
    /// Uncompressed bytes to process; 0 while unknown (e.g. tar streams).
    pub fn total_bytes(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn done_bytes(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn files(&self) -> u64 {
        self.files.load(Ordering::Relaxed)
    }

    /// The entry most recently started.
    pub fn current_file(&self) -> String {
        self.current.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Blocks the calling worker while paused; returns `false` once cancelled.
    pub(crate) fn wait_if_paused(&self) -> bool {
        while self.is_paused() && !self.is_cancelled() {
            std::thread::sleep(Duration::from_millis(50));
        }
        !self.is_cancelled()
    }

    pub(crate) fn set_total(&self, bytes: u64) {
        self.total.store(bytes, Ordering::Relaxed);
    }

    pub(crate) fn advance(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn file_done(&self) {
        self.files.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn start_file(&self, name: &str) {
        let mut current = self.current.lock().unwrap();
        current.clear();
        current.push_str(name);
    }
}

/// The folder "extract to folder" uses: the archive's name without any archive
/// extensions, next to it ("src.tar.gz" -> "src").
pub fn suggested_dir(archive: &Path) -> PathBuf {
    let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.split('.').next().filter(|s| !s.is_empty()).unwrap_or("extracted");
    archive.with_file_name(stem)
}

#[derive(Debug, Clone)]
pub struct EntryInfo {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub packed: Option<u64>,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Entries such as symlinks or devices that were not extracted.
    pub skipped: u64,
    /// Files left alone because they already existed.
    pub kept_existing: u64,
}

pub fn list(archive: &Path, opts: &Options) -> Result<Vec<EntryInfo>> {
    let (input, format) = open(archive)?;
    let result = match format {
        Format::Zip => zip::list(&input),
        Format::SevenZ => sevenz::list(&input, opts),
        Format::Rar => rar::list(&input.first, opts).map_err(|e| missing_rar_part(&input, e)),
        Format::Stream(codec) => stream::list(&input, codec, opts),
    };
    result.map_err(Error::normalize)
}

/// unRAR reports a missing volume as a generic open or CRC error; name the part instead.
fn missing_rar_part(input: &volumes::Input, e: Error) -> Error {
    let e = e.normalize();
    if matches!(e, Error::Cancelled | Error::PasswordRequired | Error::WrongPassword) {
        return e;
    }
    match volumes::RarSet::of(&input.first).and_then(|set| set.missing()) {
        Some(part) => Error::MissingVolume(part),
        None => e,
    }
}

/// Resolves which part(s) to read and detects the format from the (combined) data.
fn open(archive: &Path) -> Result<(volumes::Input, Format)> {
    let input = volumes::resolve(archive)?;
    let format = format::detect_bytes(&input.head(512)?).ok_or(Error::UnknownFormat)?;
    if format == Format::Rar && input.split.is_some() {
        // A RAR cut into .001/.002 pieces by a file splitter (not RAR's own volumes): unRAR
        // reads from a real file, so it would need joining first.
        return Err(Error::Unsupported(
            "a RAR archive split into .001/.002 files; join the parts into one file first",
        ));
    }
    Ok((input, format))
}

/// Extracts `archive` into `dest`, creating it if needed.
pub fn extract(archive: &Path, dest: &Path, opts: &Options) -> Result<Summary> {
    run(archive, Some(dest), opts)
}

/// Decodes every entry and verifies checksums without writing anything.
pub fn test(archive: &Path, opts: &Options) -> Result<Summary> {
    run(archive, None, opts)
}

fn run(archive: &Path, dest: Option<&Path>, opts: &Options) -> Result<Summary> {
    let (input, format) = open(archive)?;
    let sink = sink::Sink::new(dest, opts)?;
    let result = match format {
        Format::Zip => zip::extract(&input, &sink, opts),
        Format::SevenZ => sevenz::extract(&input, &sink, opts),
        Format::Rar => rar::extract(&input.first, &sink, opts).map_err(|e| missing_rar_part(&input, e)),
        Format::Stream(codec) => stream::extract(&input, codec, &sink, opts),
    };
    // Always drain background writes, but report the decoder's error first.
    let summary = sink.finish();
    // After a cancel, whatever error the decoder hit on the way out is just the cancel.
    if sink.is_cancelled() {
        return Err(Error::Cancelled);
    }
    result.map_err(Error::normalize)?;
    summary
}

pub(crate) fn mmap(path: &Path) -> Result<memmap2::Mmap> {
    let file = std::fs::File::open(path).at(path)?;
    // SAFETY: the archive is opened read-only; if another process truncates it while we read,
    // the worst case is a crash, the same trade-off every mmap-based reader makes.
    unsafe { memmap2::Mmap::map(&file) }.at(path)
}

#[cfg(test)]
mod tests {
    use super::normalize_name;

    #[test]
    fn names_normalize() {
        assert_eq!(normalize_name(r"docs\sub\a.txt"), "docs/sub/a.txt");
        assert_eq!(normalize_name("/docs/sub/"), "docs/sub");
        assert_eq!(normalize_name("./docs/a.txt"), "docs/a.txt");
        assert_eq!(normalize_name(".//./a"), "a");
        assert_eq!(normalize_name("./"), "");
        assert_eq!(normalize_name(".hidden/x"), ".hidden/x");
    }
}
