//! Creating ZIP archives, on every core.
//!
//! Every file is cut into pieces of [`PIECE`] bytes (a small file is one piece), and workers
//! read and compress pieces at once with zlib-rs (Deflate). A piece after a file's first starts
//! from the 32 KB before it as its dictionary, and every piece but the last ends with a sync
//! flush, so a file's pieces join into one Deflate stream that compresses almost as well as
//! one made in one go (as pigz does). This thread writes the pieces in order, each entry's
//! local header first (patched with the sizes and CRC once its last piece is written), and the
//! central directory at the end. Files that are already compressed (by their extension) are
//! stored as they are, and so is a one-piece file that wouldn't get smaller. With a password,
//! files are encrypted with WinZip's AES-256 (AE-2).

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::SystemTime,
};

use aes::{
    Aes256, Block,
    cipher::{BlockCipherEncrypt, KeyInit},
};
use flate2::{Compress, Compression, FlushCompress, Status};
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::{Error, IoContext, Progress, Result, Summary, time, zip_own::winzip_keys};

/// The bytes compressed as one job.
const PIECE: usize = 1 << 20;
/// Deflate's window: what a piece takes from before it.
const DICT: usize = 32 << 10;
/// Extensions of files already compressed: stored, not compressed again.
const STORED: &[&str] = &[
    "zip", "7z", "rar", "gz", "tgz", "xz", "txz", "bz2", "tbz", "tbz2", "zst", "tzst", "lz4", "lzma", "br", "cab", "jpg", "jpeg", "png", "gif",
    "webp", "heic", "heif", "avif", "jxl", "mp3", "aac", "ogg", "opus", "flac", "m4a", "wma", "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv",
    "flv", "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", "apk", "jar", "msix", "appx", "nupkg", "whl", "pdf",
];

/// How to create an archive.
#[derive(Clone, Debug)]
pub struct CreateOptions {
    /// Worker threads; 0 means one per logical CPU.
    pub threads: usize,
    /// Deflate level: 0 (store, no compression) to 9 (smallest, slowest).
    pub level: u32,
    /// Encrypt the files (WinZip AES-256) with this password.
    pub password: Option<String>,
    /// Live counters for a UI; also how a running job is paused or cancelled.
    pub progress: Option<Arc<Progress>>,
    /// Replace an existing archive of that name (else it's an error).
    pub overwrite: bool,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            threads: 0,
            level: 6,
            password: None,
            progress: None,
            overwrite: false,
        }
    }
}

/// A file or folder to add.
struct Item {
    path: PathBuf,
    /// Its name in the archive, with `/` between folders (and at the end of a folder's).
    name: String,
    size: u64,
    modified: SystemTime,
    /// FILETIMEs: modified, accessed, created.
    times: [u64; 3],
    /// Windows file attributes.
    attributes: u32,
    is_dir: bool,
    /// Already compressed: stored as it is.
    store: bool,
}

/// A piece of a file to compress.
struct Job {
    item: usize,
    offset: u64,
    len: usize,
    first: bool,
    last: bool,
}

/// A compressed piece.
struct Piece {
    data: Vec<u8>,
    crc: crc32fast::Hasher,
    /// A one-piece file that didn't get smaller: `data` is the file itself.
    stored: bool,
    /// The first piece of an encrypted file: its salt and keys (derived here, on a worker: it
    /// takes a few milliseconds).
    keys: Option<([u8; 16], Vec<u8>)>,
}

/// Creates the ZIP archive `archive` from `inputs` (files and folders, with all they hold),
/// each named in the archive after itself, as it is in its folder.
pub fn create(archive: &Path, inputs: &[PathBuf], o: &CreateOptions) -> Result<Summary> {
    if archive.exists() && !o.overwrite {
        return Err(Error::Io {
            path: archive.to_path_buf(),
            source: io::Error::new(io::ErrorKind::AlreadyExists, "the archive already exists"),
        });
    }
    let mut summary = Summary::default();
    let own = fs::canonicalize(archive).ok();
    let mut items = Vec::new();
    for input in inputs {
        let meta = fs::symlink_metadata(input).at(input)?;
        let name = match input.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => fs::canonicalize(input)
                .at(input)?
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "drive".to_owned()),
        };
        walk(input, name, &meta, own.as_deref(), &mut items, &mut summary)?;
    }
    let level = o.level.min(9);
    let mut jobs = Vec::new();
    for (i, item) in items.iter_mut().enumerate() {
        if item.is_dir {
            continue;
        }
        item.store |= level == 0;
        let pieces = item.size.div_ceil(PIECE as u64).max(1);
        for p in 0..pieces {
            let offset = p * PIECE as u64;
            jobs.push(Job {
                item: i,
                offset,
                len: (item.size - offset).min(PIECE as u64) as usize,
                first: p == 0,
                last: p + 1 == pieces,
            });
        }
    }
    if let Some(p) = &o.progress {
        p.set_total(items.iter().map(|i| i.size).sum());
    }

    let temp = archive.with_file_name(format!("{}.b-rar-tmp", archive.file_name().unwrap_or_default().to_string_lossy()));
    let file = File::create(&temp).at(&temp)?;
    let result = write(file, &items, &jobs, level, o, &mut summary);
    match result {
        Ok(()) => {
            fs::rename(&temp, archive).at(archive)?;
            Ok(summary)
        }
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// Adds `path` (named `name`) and, for a folder, all it holds, sorted by name. Links aren't
/// followed (they're counted as skipped), and the archive being made isn't added to itself.
fn walk(path: &Path, name: String, meta: &fs::Metadata, own: Option<&Path>, items: &mut Vec<Item>, summary: &mut Summary) -> Result<()> {
    #[cfg(windows)]
    use std::os::windows::fs::MetadataExt;
    if meta.file_type().is_symlink() {
        summary.skipped += 1;
        return Ok(());
    }
    #[cfg(windows)]
    let (attributes, times) = (
        meta.file_attributes(),
        [meta.last_write_time(), meta.last_access_time(), meta.creation_time()],
    );
    #[cfg(not(windows))]
    let (attributes, times) = (if meta.is_dir() { 0x10 } else { 0x20 }, [0; 3]);
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if meta.is_dir() {
        items.push(Item {
            path: path.to_path_buf(),
            name: format!("{name}/"),
            size: 0,
            modified,
            times,
            attributes,
            is_dir: true,
            store: true,
        });
        summary.dirs += 1;
        let mut children: Vec<_> = fs::read_dir(path).at(path)?.collect::<io::Result<_>>().at(path)?;
        children.sort_by_key(|c| c.file_name());
        for c in children {
            let p = c.path();
            let meta = fs::symlink_metadata(&p).at(&p)?;
            walk(&p, format!("{name}/{}", c.file_name().to_string_lossy()), &meta, own, items, summary)?;
        }
        return Ok(());
    }
    if own.is_some_and(|own| fs::canonicalize(path).is_ok_and(|p| p == own)) {
        return Ok(());
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    items.push(Item {
        path: path.to_path_buf(),
        name,
        size: meta.len(),
        modified,
        times,
        attributes,
        is_dir: false,
        store: STORED.contains(&ext.as_str()),
    });
    Ok(())
}

/// Reads and compresses one piece.
fn piece(job: &Job, item: &Item, level: u32, password: Option<&[u8]>) -> Result<Piece> {
    let back = if job.first { 0 } else { DICT.min(job.offset as usize) };
    let mut buf = vec![0u8; back + job.len];
    if job.len > 0 {
        let mut f = File::open(&item.path).at(&item.path)?;
        f.seek(SeekFrom::Start(job.offset - back as u64)).at(&item.path)?;
        f.read_exact(&mut buf)
            .map_err(|e| match e.kind() {
                io::ErrorKind::UnexpectedEof => io::Error::other("the file changed while it was being added"),
                _ => e,
            })
            .at(&item.path)?;
    }
    let data = &buf[back..];
    let mut crc = crc32fast::Hasher::new();
    crc.update(data);
    let keys = match password {
        Some(pw) if job.first => {
            let mut salt = [0u8; 16];
            getrandom::fill(&mut salt).map_err(|e| Error::RawIo(io::Error::other(e.to_string())))?;
            Some((salt, winzip_keys(pw, &salt, 32)))
        }
        _ => None,
    };
    if item.store {
        return Ok(Piece {
            data: data.to_vec(),
            crc,
            stored: true,
            keys,
        });
    }
    let mut c = Compress::new(Compression::new(level), false);
    if back > 0 {
        c.set_dictionary(&buf[..back])
            .map_err(|e| Error::RawIo(io::Error::other(e.to_string())))?;
    }
    let flush = if job.last { FlushCompress::Finish } else { FlushCompress::Sync };
    let mut out = Vec::with_capacity(job.len + job.len / 64 + 256);
    let mut input = data;
    loop {
        if out.capacity() - out.len() < 4096 {
            out.reserve(64 << 10);
        }
        let before = c.total_in();
        let status = c
            .compress_vec(input, &mut out, flush)
            .map_err(|e| Error::RawIo(io::Error::other(e.to_string())))?;
        input = &input[(c.total_in() - before) as usize..];
        let done = match flush {
            FlushCompress::Finish => status == Status::StreamEnd,
            // Flushed once there was room to spare.
            _ => input.is_empty() && out.capacity() - out.len() >= 4096,
        };
        if done {
            break;
        }
    }
    // A one-piece file that didn't get smaller: stored instead.
    if job.first && job.last && out.len() >= data.len() {
        return Ok(Piece {
            data: data.to_vec(),
            crc,
            stored: true,
            keys,
        });
    }
    Ok(Piece {
        data: out,
        crc,
        stored: false,
        keys,
    })
}

/// The archive's file, written at known offsets through a buffer.
struct Out {
    file: File,
    buf: Vec<u8>,
    /// Where `buf` starts in the file.
    flushed: u64,
}

impl Out {
    fn pos(&self) -> u64 {
        self.flushed + self.buf.len() as u64
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 8 << 20 {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        write_at(&self.file, self.flushed, &self.buf)?;
        self.flushed += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    /// Overwrites what's at `at` (earlier in the file).
    fn patch(&mut self, at: u64, data: &[u8]) -> io::Result<()> {
        let mut data = data;
        let mut at = at;
        if at < self.flushed {
            let n = data.len().min((self.flushed - at) as usize);
            write_at(&self.file, at, &data[..n])?;
            data = &data[n..];
            at += n as u64;
        }
        if !data.is_empty() {
            let i = (at - self.flushed) as usize;
            self.buf[i..i + data.len()].copy_from_slice(data);
        }
        Ok(())
    }
}

fn write_at(file: &File, mut at: u64, mut data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;
    while !data.is_empty() {
        #[cfg(windows)]
        let n = file.seek_write(data, at)?;
        #[cfg(unix)]
        let n = file.write_at(data, at)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        data = &data[n..];
        at += n as u64;
    }
    Ok(())
}

/// AES-256 in CTR mode (a little-endian counter from 1) and the HMAC-SHA1 of the ciphertext:
/// WinZip AES, writing.
struct Encrypt {
    aes: Aes256,
    mac: Hmac<Sha1>,
    counter: u64,
    /// Keystream bytes not used yet.
    stream: [u8; 16],
    used: usize,
}

impl Encrypt {
    fn new(keys: &[u8]) -> Self {
        Self {
            aes: Aes256::new_from_slice(&keys[..32]).expect("a 32-byte key"),
            mac: Hmac::<Sha1>::new_from_slice(&keys[32..64]).expect("HMAC takes any key length"),
            counter: 0,
            stream: [0; 16],
            used: 16,
        }
    }

    fn apply(&mut self, data: &mut [u8]) {
        let mut i = 0;
        // What's left of the last block's keystream, then whole blocks, eight at a time.
        while self.used < 16 && i < data.len() {
            data[i] ^= self.stream[self.used];
            self.used += 1;
            i += 1;
        }
        let mut blocks = [Block::default(); 8];
        while data.len() - i >= 16 {
            let n = ((data.len() - i) / 16).min(8);
            for b in &mut blocks[..n] {
                self.counter += 1;
                *b = Block::default();
                b[..8].copy_from_slice(&self.counter.to_le_bytes());
            }
            self.aes.encrypt_blocks(&mut blocks[..n]);
            for b in &blocks[..n] {
                for (d, k) in data[i..i + 16].iter_mut().zip(b.iter()) {
                    *d ^= k;
                }
                i += 16;
            }
        }
        while i < data.len() {
            if self.used == 16 {
                self.counter += 1;
                let mut b = Block::default();
                b[..8].copy_from_slice(&self.counter.to_le_bytes());
                self.aes.encrypt_block(&mut b);
                self.stream.copy_from_slice(&b);
                self.used = 0;
            }
            data[i] ^= self.stream[self.used];
            self.used += 1;
            i += 1;
        }
        self.mac.update(data);
    }
}

/// A file's entry in the central directory.
struct Record {
    item: usize,
    method: u16,
    crc: u32,
    packed: u64,
    offset: u64,
}

/// The bytes of a field list, little-endian.
#[derive(Default)]
struct Bytes(Vec<u8>);

impl Bytes {
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
}

const STORE: u16 = 0;
const DEFLATE: u16 = 8;
const AES: u16 = 99;
/// "Too big for 32 bits: see the ZIP64 field".
const MAX32: u64 = 0xffff_ffff;

/// Finished pieces by job, how far the writer is, and whether it stopped.
type Shared = Mutex<(HashMap<usize, Result<Piece>>, usize, bool)>;

/// Runs the workers and writes the archive.
fn write(file: File, items: &[Item], jobs: &[Job], level: u32, o: &CreateOptions, summary: &mut Summary) -> Result<()> {
    let threads = match o.threads {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    };
    let password = o.password.as_deref().filter(|p| !p.is_empty()).map(str::as_bytes);
    let progress = o.progress.as_deref();
    // Pieces compressed ahead of the writer, at most.
    let ahead = threads * 4 + 4;
    let next = AtomicUsize::new(0);
    let state: Shared = Mutex::new((HashMap::new(), 0, false));
    let changed = Condvar::new();
    std::thread::scope(|scope| {
        for _ in 0..threads.min(jobs.len()).max(1) {
            scope.spawn(|| {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= jobs.len() {
                        return;
                    }
                    {
                        let mut s = state.lock().unwrap();
                        while k >= s.1 + ahead && !s.2 {
                            s = changed.wait(s).unwrap();
                        }
                        if s.2 {
                            return;
                        }
                    }
                    let r = match progress {
                        Some(p) if !p.wait_if_paused() => Err(Error::Cancelled),
                        _ => piece(&jobs[k], &items[jobs[k].item], level, password),
                    };
                    state.lock().unwrap().0.insert(k, r);
                    changed.notify_all();
                }
            });
        }
        let mut out = Out {
            file,
            buf: Vec::with_capacity(9 << 20),
            flushed: 0,
        };
        let result = write_entries(&mut out, items, jobs, password.is_some(), progress, summary, &state, &changed);
        let mut s = state.lock().unwrap();
        s.2 = true;
        s.0.clear();
        drop(s);
        changed.notify_all();
        result
    })
}

/// The writer: entries in order, then the central directory.
#[allow(clippy::too_many_arguments)]
fn write_entries(
    out: &mut Out,
    items: &[Item],
    jobs: &[Job],
    encrypt: bool,
    progress: Option<&Progress>,
    summary: &mut Summary,
    state: &Shared,
    changed: &Condvar,
) -> Result<()> {
    let at = |e: io::Error| Error::RawIo(e);
    let mut records = Vec::with_capacity(items.len());
    let mut k = 0;
    for (i, item) in items.iter().enumerate() {
        if let Some(p) = progress {
            p.start_file(&item.name);
        }
        let offset = out.pos();
        if item.is_dir {
            local_header(out, item, STORE, 0, 0, 0, false, false).map_err(at)?;
            records.push(Record {
                item: i,
                method: STORE,
                crc: 0,
                packed: 0,
                offset,
            });
            continue;
        }
        // The entry's pieces: the first decides the method and how the header is written.
        let mut crc = crc32fast::Hasher::new();
        let mut packed = 0u64;
        let mut method = DEFLATE;
        let mut enc = None;
        let mut sized_header = false;
        let mut zip64 = false;
        loop {
            let job = &jobs[k];
            let mut piece = {
                let mut s = state.lock().unwrap();
                loop {
                    if let Some(p) = s.0.remove(&k) {
                        break p;
                    }
                    s = changed.wait(s).unwrap();
                }
            }?;
            if job.first {
                method = if piece.stored { STORE } else { DEFLATE };
                let whole = job.last;
                // Sizes known now (one piece) go in the header; else it's patched at the end,
                // with room for 64-bit sizes if they could need it.
                zip64 = !whole && item.size >= 0xffff_0000;
                sized_header = whole;
                let extra_aes = 16 + 2 + 10;
                let packed_now = piece.data.len() as u64 + if encrypt { extra_aes } else { 0 };
                // AE-2 entries carry no CRC (their authentication code checks the data).
                let (c, p, s) = match whole {
                    true if encrypt => (0, packed_now, item.size),
                    true => (piece.crc.clone().finalize(), packed_now, item.size),
                    false => (0, 0, 0),
                };
                local_header(out, item, method, c, p, s, zip64, encrypt).map_err(at)?;
                if let Some((salt, keys)) = &piece.keys {
                    out.write(salt).map_err(at)?;
                    out.write(&keys[64..66]).map_err(at)?;
                    enc = Some(Encrypt::new(keys));
                }
            }
            crc.combine(&piece.crc);
            if let Some(e) = &mut enc {
                e.apply(&mut piece.data);
            }
            out.write(&piece.data).map_err(at)?;
            packed += piece.data.len() as u64;
            if let Some(p) = progress {
                p.advance(job.len as u64);
            }
            k += 1;
            {
                let mut s = state.lock().unwrap();
                s.1 = k;
            }
            changed.notify_all();
            if job.last {
                break;
            }
        }
        if let Some(e) = enc {
            let code = e.mac.finalize().into_bytes();
            out.write(&code[..10]).map_err(at)?;
            packed += 16 + 2 + 10;
        }
        // AE-2 entries carry no CRC (their authentication code checks the data).
        let crc = if encrypt { 0 } else { crc.finalize() };
        if !sized_header {
            // CRC, then the sizes: in the header, or in its ZIP64 field.
            out.patch(offset + 14, &crc.to_le_bytes()).map_err(at)?;
            if zip64 {
                let field = offset + 30 + item.name.len() as u64 + 4;
                out.patch(field, &item.size.to_le_bytes()).map_err(at)?;
                out.patch(field + 8, &packed.to_le_bytes()).map_err(at)?;
            } else {
                out.patch(offset + 18, &(packed as u32).to_le_bytes()).map_err(at)?;
                out.patch(offset + 22, &(item.size as u32).to_le_bytes()).map_err(at)?;
            }
        }
        records.push(Record {
            item: i,
            method,
            crc,
            packed,
            offset,
        });
        summary.files += 1;
        summary.bytes += item.size;
        if let Some(p) = progress {
            p.file_done();
        }
    }
    central_directory(out, items, &records, encrypt).map_err(at)?;
    out.flush().map_err(at)
}

/// Bit 11 of the flags: the name is UTF-8 (set when it isn't plain ASCII).
fn flags(item: &Item, encrypted: bool) -> u16 {
    let utf8 = if item.name.is_ascii() { 0 } else { 1 << 11 };
    utf8 | u16::from(encrypted)
}

/// The AES field: AE-2, AES-256, the real method.
fn aes_field(method: u16) -> Vec<u8> {
    let mut b = Bytes::default();
    b.u16(0x9901).u16(7).u16(2).bytes(b"AE").bytes(&[3]).u16(method);
    b.0
}

fn needed(zip64: bool, encrypted: bool) -> u16 {
    if encrypted {
        51
    } else if zip64 {
        45
    } else {
        20
    }
}

#[allow(clippy::too_many_arguments)]
fn local_header(out: &mut Out, item: &Item, method: u16, crc: u32, packed: u64, size: u64, zip64: bool, encrypted: bool) -> io::Result<()> {
    let encrypted = encrypted && !item.is_dir;
    let (time, date) = time::to_dos(item.modified);
    let mut extra = Bytes::default();
    if zip64 {
        extra.u16(1).u16(16).u64(size).u64(packed);
    }
    if encrypted {
        extra.bytes(&aes_field(method));
    }
    let (packed32, size32) = if zip64 { (MAX32, MAX32) } else { (packed, size) };
    let mut h = Bytes::default();
    h.u32(0x0403_4b50)
        .u16(needed(zip64, encrypted))
        .u16(flags(item, encrypted))
        .u16(if encrypted { AES } else { method })
        .u16(time)
        .u16(date)
        .u32(crc)
        .u32(packed32 as u32)
        .u32(size32 as u32)
        .u16(item.name.len() as u16)
        .u16(extra.0.len() as u16)
        .bytes(item.name.as_bytes())
        .bytes(&extra.0);
    out.write(&h.0)
}

fn central_directory(out: &mut Out, items: &[Item], records: &[Record], encrypt: bool) -> io::Result<()> {
    let start = out.pos();
    for r in records {
        let item = &items[r.item];
        let encrypted = encrypt && !item.is_dir;
        let (time, date) = time::to_dos(item.modified);
        let mut zip64 = Bytes::default();
        if item.size >= MAX32 {
            zip64.u64(item.size);
        }
        if r.packed >= MAX32 {
            zip64.u64(r.packed);
        }
        if r.offset >= MAX32 {
            zip64.u64(r.offset);
        }
        let mut extra = Bytes::default();
        if !zip64.0.is_empty() {
            extra.u16(1).u16(zip64.0.len() as u16).bytes(&zip64.0);
        }
        // NTFS times (modified, accessed, created), to the 100 ns.
        if item.times != [0; 3] {
            extra
                .u16(0x000a)
                .u16(32)
                .u32(0)
                .u16(1)
                .u16(24)
                .u64(item.times[0])
                .u64(item.times[1])
                .u64(item.times[2]);
        }
        if encrypted {
            extra.bytes(&aes_field(r.method));
        }
        let mut h = Bytes::default();
        h.u32(0x0201_4b50)
            // Made by: MS-DOS attributes (Windows), ZIP 6.3.
            .u16(63)
            .u16(needed(!zip64.0.is_empty(), encrypted))
            .u16(flags(item, encrypted))
            .u16(if encrypted { AES } else { r.method })
            .u16(time)
            .u16(date)
            .u32(r.crc)
            .u32(r.packed.min(MAX32) as u32)
            .u32(item.size.min(MAX32) as u32)
            .u16(item.name.len() as u16)
            .u16(extra.0.len() as u16)
            .u16(0)
            .u16(0)
            .u16(0)
            .u32(item.attributes)
            .u32(r.offset.min(MAX32) as u32)
            .bytes(item.name.as_bytes())
            .bytes(&extra.0);
        out.write(&h.0)?;
    }
    let end = out.pos();
    let (count, size) = (records.len() as u64, end - start);
    let mut e = Bytes::default();
    if count >= 0xffff || size >= MAX32 || start >= MAX32 {
        // ZIP64 end of central directory, and its locator.
        e.u32(0x0606_4b50)
            .u64(44)
            .u16(45)
            .u16(45)
            .u32(0)
            .u32(0)
            .u64(count)
            .u64(count)
            .u64(size)
            .u64(start);
        e.u32(0x0706_4b50).u32(0).u64(end).u32(1);
    }
    e.u32(0x0605_4b50)
        .u16(0)
        .u16(0)
        .u16(count.min(0xffff) as u16)
        .u16(count.min(0xffff) as u16)
        .u32(size.min(MAX32) as u32)
        .u32(start.min(MAX32) as u32)
        .u16(0);
    out.write(&e.0)
}
