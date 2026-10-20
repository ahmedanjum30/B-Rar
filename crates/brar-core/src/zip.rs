//! ZIP: every entry is independently compressed, so all of them decode in parallel.
//!
//! Fast path (plain Deflate or stored entries, unencrypted): read the compressed bytes with
//! positioned reads, decode in one libdeflate call, check the CRC, write in 1 MiB blocks.
//! Measured on one core for a 545 MB entry: 1.33 s, against 2.36 s through the zip crate's
//! reader over a memory map (page faults on the map cost 0.5 s, zlib-rs vs libdeflate 0.37 s,
//! the reader layers 0.16 s). Everything else goes through the zip crate.

use std::{cell::RefCell, fs::File, io::Cursor, path::Path, sync::Mutex, time::SystemTime};

use libdeflater::Decompressor;
use zip::{
    CompressionMethod, ZipArchive,
    extra_fields::ExtraField,
    read::{ArchiveOffset, Config, ZipFile},
};

use crate::{
    EntryInfo, Error, IoContext, Options, Result, mmap,
    sink::{SMALL_FILE, Sink},
    time,
    whole::{BUDGET as FAST_PATH_BUDGET, Budget, read_at},
};

thread_local! {
    static DECOMPRESSOR: RefCell<Decompressor> = RefCell::new(Decompressor::new());
}

pub(crate) fn list(path: &Path) -> Result<Vec<EntryInfo>> {
    let map = mmap(path)?;
    let mut archive = open(&map)?;
    (0..archive.len())
        .map(|i| {
            let f = archive.by_index_raw(i)?;
            Ok(EntryInfo {
                name: f.name()?.into_owned(),
                is_dir: f.is_dir(),
                size: f.size(),
                packed: Some(f.compressed_size()),
                modified: mtime(&f),
            })
        })
        .collect()
}

/// Opens the archive assuming the usual layout (central directory exactly where the end record
/// says). The zip crate's default auto-detection scans the file for the directory, which took
/// 0.6 s on a 1.1 GB archive; it remains the fallback for ZIPs with data in front (e.g.
/// self-extracting .exe files).
fn open(map: &[u8]) -> Result<ZipArchive<Cursor<&[u8]>>> {
    let known = Config { archive_offset: ArchiveOffset::Known(0) };
    match ZipArchive::with_config(known, Cursor::new(map)) {
        Ok(archive) => Ok(archive),
        Err(_) => Ok(ZipArchive::new(Cursor::new(map))?),
    }
}

/// What the fast path needs to know about an entry, taken from the central directory.
struct Plain {
    name: String,
    data_start: u64,
    packed: u64,
    size: u64,
    crc: u32,
    stored: bool,
    modified: Option<SystemTime>,
}

enum Job {
    Fast(Plain),
    Fallback(usize),
}

pub(crate) fn extract(path: &Path, sink: &Sink, opts: &Options) -> Result<()> {
    let map = mmap(path)?;
    let archive = open(&map)?;
    let file = File::open(path).at(path)?;

    let mut jobs = Vec::with_capacity(archive.len());
    let mut total = 0;
    {
        let mut archive = archive.clone();
        for i in 0..archive.len() {
            let f = archive.by_index_raw(i)?;
            if f.is_dir() {
                sink.dir(&f.name()?)?;
                continue;
            }
            total += f.size();
            let method = f.compression();
            let fast = !f.encrypted()
                && matches!(method, CompressionMethod::Deflated | CompressionMethod::Stored)
                && f.compressed_size() + f.size() <= FAST_PATH_BUDGET;
            let job = match f.data_start() {
                Some(data_start) if fast => Job::Fast(Plain {
                    name: f.name()?.into_owned(),
                    data_start,
                    packed: f.compressed_size(),
                    size: f.size(),
                    crc: f.crc32(),
                    stored: method == CompressionMethod::Stored,
                    modified: mtime(&f),
                }),
                _ => Job::Fallback(i),
            };
            jobs.push((f.compressed_size(), job));
        }
    }
    sink.set_total(total);
    // Biggest first, so a huge entry never starts last and leaves the other cores idle.
    jobs.sort_unstable_by(|a, b| b.0.cmp(&a.0));

    // One FIFO job per entry: jobs start strictly in that order, one per free thread. (A
    // parallel iterator splits the list into ranges instead, which let the two largest
    // entries of an archive run back to back on one thread.)
    let password = opts.password.as_deref().map(str::as_bytes);
    let budget = Budget::new(FAST_PATH_BUDGET);
    let failure = Mutex::new(None);
    opts.pool()?.scope_fifo(|s| {
        for (_, job) in &jobs {
            let (archive, failure, file, budget) = (&archive, &failure, &file, &budget);
            s.spawn_fifo(move |_| {
                if failure.lock().unwrap().is_some() {
                    return;
                }
                let result = match job {
                    Job::Fast(entry) => extract_fast(file, entry, sink, budget),
                    Job::Fallback(i) => extract_entry(archive.clone(), *i, password, sink),
                };
                if let Err(e) = result {
                    failure.lock().unwrap().get_or_insert(e);
                }
            });
        }
    });
    failure.into_inner().unwrap().map_or(Ok(()), Err)
}

fn extract_fast(file: &File, e: &Plain, sink: &Sink, budget: &Budget) -> Result<()> {
    sink.check()?;
    sink.start(&e.name);
    let _lease = budget.take(e.packed + if e.stored { 0 } else { e.size });

    let mut packed = vec![0u8; e.packed as usize];
    read_at(file, e.data_start, &mut packed).map_err(|_| Error::Damaged(e.name.clone()))?;
    let data = if e.stored {
        packed
    } else {
        let mut out = vec![0u8; e.size as usize];
        let n = DECOMPRESSOR
            .with_borrow_mut(|d| d.deflate_decompress(&packed, &mut out))
            .map_err(|_| Error::Damaged(e.name.clone()))?;
        if n as u64 != e.size {
            return Err(Error::Damaged(e.name.clone()));
        }
        out
    };
    if crc32fast::hash(&data) != e.crc {
        return Err(Error::Damaged(e.name.clone()));
    }
    if e.size <= SMALL_FILE {
        sink.file_owned(&e.name, e.modified, data)
    } else {
        sink.file_bytes(&e.name, e.modified, &data)
    }
}

fn extract_entry(mut archive: ZipArchive<Cursor<&[u8]>>, i: usize, password: Option<&[u8]>, sink: &Sink) -> Result<()> {
    let mut f = match password {
        Some(pw) => archive.by_index_decrypt(i, pw)?,
        None => archive.by_index(i)?,
    };
    let name = f.name()?.into_owned();
    let (size, modified, encrypted) = (f.size(), mtime(&f), f.encrypted());
    sink.file(&name, Some(size), modified, &mut f)
        .map_err(|e| e.for_entry(&name, encrypted && password.is_some()))
}

fn mtime<R: std::io::Read>(f: &ZipFile<'_, R>) -> Option<SystemTime> {
    for field in f.extra_data_fields() {
        match field {
            ExtraField::Ntfs(ntfs) => return time::filetime(ntfs.mtime()),
            ExtraField::ExtendedTimestamp(ts) => {
                if let Some(secs) = ts.mod_time() {
                    return time::unix(secs.into());
                }
            }
            _ => {}
        }
    }
    let dt = f.last_modified()?;
    time::local(dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute(), dt.second())
}
