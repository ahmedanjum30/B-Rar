//! ZIP: every entry is independently compressed, so all of them decode in parallel.
//!
//! Fast path (plain Deflate or stored entries, unencrypted): read the compressed bytes with
//! positioned reads, decode in one libdeflate call, check the CRC, write in 1 MiB blocks. A
//! Deflate entry of [`PARALLEL_MIN`] or more decodes on all cores instead
//! ([`crate::deflate::parallel`]), written as it decodes; libdeflate is its fallback.
//! Measured on one core for a 545 MB entry: 1.33 s, against 2.36 s through the zip crate's
//! reader over a memory map (page faults on the map cost 0.5 s, zlib-rs vs libdeflate 0.37 s,
//! the reader layers 0.16 s). Everything else goes through the zip crate.

use std::{
    cell::RefCell,
    fs::File,
    io::{self, Cursor, Read, Seek},
    sync::Mutex,
    time::SystemTime,
};

use libdeflater::Decompressor;
use zip::{
    CompressionMethod, ZipArchive,
    extra_fields::ExtraField,
    read::{ArchiveOffset, Config, ZipFile},
};

use crate::{
    EntryInfo, Error, IoContext, Options, Result,
    deflate::{
        inflate::INPUT_PADDING,
        parallel::{Failure, inflate_parallel},
    },
    mmap,
    sink::{SMALL_FILE, Sink},
    time,
    volumes::Input,
    whole::{BUDGET as FAST_PATH_BUDGET, Budget, read_at},
};

thread_local! {
    static DECOMPRESSOR: RefCell<Decompressor> = RefCell::new(Decompressor::new());
}

/// Positioned read of the archive's bytes (one file, or all parts of a split archive).
type ReadAt<'a> = &'a (dyn Fn(u64, &mut [u8]) -> io::Result<()> + Sync);

pub(crate) fn list(input: &Input) -> Result<Vec<EntryInfo>> {
    match &input.split {
        None => {
            let map = mmap(&input.first)?;
            list_from(Cursor::new(&map[..]))
        }
        Some(parts) => list_from(parts.clone()),
    }
}

fn list_from<R: Read + Seek + Clone>(reader: R) -> Result<Vec<EntryInfo>> {
    let mut archive = open(reader)?;
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
fn open<R: Read + Seek + Clone>(reader: R) -> Result<ZipArchive<R>> {
    let known = Config {
        archive_offset: ArchiveOffset::Known(0),
    };
    match ZipArchive::with_config(known, reader.clone()) {
        Ok(archive) => Ok(archive),
        Err(_) => Ok(ZipArchive::new(reader)?),
    }
}

/// Deflate entries this big decode on all cores; smaller ones on one, through libdeflate.
const PARALLEL_MIN: u64 = 64 << 20;

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

pub(crate) fn extract(input: &Input, sink: &Sink, opts: &Options) -> Result<()> {
    match &input.split {
        None => {
            let map = mmap(&input.first)?;
            let file = File::open(&input.first).at(&input.first)?;
            extract_from(
                Cursor::new(&map[..]),
                &|offset, buf| read_at(&file, offset, buf),
                Some(&map[..]),
                sink,
                opts,
            )
        }
        Some(parts) => extract_from(parts.clone(), &|offset, buf| parts.read_at(offset, buf), None, sink, opts),
    }
}

/// `map`: the whole archive in memory (a single-file archive), which big entries decode from
/// directly.
fn extract_from<R: Read + Seek + Clone + Send + Sync>(reader: R, read_at: ReadAt, map: Option<&[u8]>, sink: &Sink, opts: &Options) -> Result<()> {
    let archive = open(reader)?;

    let mut jobs = Vec::with_capacity(archive.len());
    let mut total = 0;
    {
        let mut archive = archive.clone();
        for i in 0..archive.len() {
            let f = archive.by_index_raw(i)?;
            if !sink.selected(&f.name()?) {
                continue;
            }
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
    jobs.sort_unstable_by_key(|j| std::cmp::Reverse(j.0));

    // One FIFO job per entry: jobs start strictly in that order, one per free thread. (A
    // parallel iterator splits the list into ranges instead, which let the two largest
    // entries of an archive run back to back on one thread.)
    let password = opts.password.as_deref().map(str::as_bytes);
    let budget = Budget::new(FAST_PATH_BUDGET);
    let threads = opts.thread_count();
    let failure = Mutex::new(None);
    opts.pool()?.scope_fifo(|s| {
        for (_, job) in &jobs {
            let (archive, failure, budget) = (&archive, &failure, &budget);
            s.spawn_fifo(move |_| {
                if failure.lock().unwrap().is_some() {
                    return;
                }
                let result = match job {
                    Job::Fast(entry) => extract_fast(read_at, map, entry, sink, budget, threads),
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

fn extract_fast(read_at: ReadAt, map: Option<&[u8]>, e: &Plain, sink: &Sink, budget: &Budget, threads: usize) -> Result<()> {
    sink.check()?;
    sink.start(&e.name);
    let parallel = !e.stored && e.size >= PARALLEL_MIN && threads >= 4;
    if parallel {
        // Straight from the archive's memory map: no serial read of the whole entry first, the
        // workers fault in their own pieces. The decoder reads up to INPUT_PADDING bytes past
        // the data; a ZIP always has its central directory after it.
        let (start, len) = (e.data_start as usize, e.packed as usize);
        if let Some(input) = map.and_then(|m| m.get(start..start + len + INPUT_PADDING))
            && extract_parallel(input, e, sink, threads)?
        {
            return Ok(());
        }
    }
    let _lease = budget.take(e.packed + if e.stored { 0 } else { e.size });
    // The parallel decoder reads past the end in 8-byte loads: padding for it.
    let mut packed = vec![0u8; e.packed as usize + if parallel { INPUT_PADDING } else { 0 }];
    read_at(e.data_start, &mut packed[..e.packed as usize]).map_err(|_| Error::Damaged(e.name.clone()))?;
    if parallel && extract_parallel(&packed, e, sink, threads)? {
        return Ok(());
    }
    packed.truncate(e.packed as usize);
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

/// A big Deflate entry on all cores, written as it decodes. `Ok(false)`: the parallel decoder
/// gave up or the CRC didn't match (the partial file is removed); libdeflate takes over, and
/// reports real damage.
fn extract_parallel(packed: &[u8], e: &Plain, sink: &Sink, threads: usize) -> Result<bool> {
    let mut file = sink.begin_file(&e.name, Some(e.size), e.modified)?;
    let mut crc = crc32fast::Hasher::new();
    let result = inflate_parallel(packed, e.packed as usize, threads, &mut |b: &[u8]| {
        crc.update(b);
        file.write(b)
    });
    match result {
        Ok(total) if total == e.size && crc.finalize() == e.crc => {
            file.finish();
            Ok(true)
        }
        Ok(_) | Err(Failure::Bad) => {
            file.discard();
            Ok(false)
        }
        Err(Failure::Stop(err)) => {
            file.discard();
            Err(err)
        }
    }
}

fn extract_entry<R: Read + Seek>(mut archive: ZipArchive<R>, i: usize, password: Option<&[u8]>, sink: &Sink) -> Result<()> {
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

#[cfg(test)]
mod tests {
    /// libdeflate's decode-only cost on the largest Deflate entry of $BRAR_BENCH_ZIP, in CPU
    /// cycles per output byte (the thread's own cycle count), best of 3 rounds: the yardstick
    /// for B-Rar's RAR5 decoder (`rar5_bench`). Run with
    /// `cargo test --release -p brar-core --lib zip_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn zip_bench() {
        use std::time::Instant;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> isize;
            fn QueryThreadCycleTime(thread: isize, cycles: *mut u64) -> i32;
        }
        let cycles = || {
            let mut c = 0u64;
            // SAFETY: the pseudo-handle of the current thread and a valid out pointer.
            unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
            c
        };
        let path = std::env::var("BRAR_BENCH_ZIP").expect("set BRAR_BENCH_ZIP");
        let file = std::fs::File::open(&path).unwrap();
        let mut archive = ::zip::ZipArchive::new(&file).unwrap();
        let (mut best, mut start, mut packed, mut size, mut crc) = (String::new(), 0u64, 0u64, 0u64, 0u32);
        for i in 0..archive.len() {
            let f = archive.by_index_raw(i).unwrap();
            if f.compression() == ::zip::CompressionMethod::Deflated && f.size() > size {
                (best, start, packed, size, crc) = (
                    f.name().unwrap().into_owned(),
                    f.data_start().unwrap(),
                    f.compressed_size(),
                    f.size(),
                    f.crc32(),
                );
            }
        }
        let mut input = vec![0u8; packed as usize];
        crate::whole::read_at(&file, start, &mut input).unwrap();
        let mut out = vec![1u8; size as usize];
        let mut d = libdeflater::Decompressor::new();
        let (mut s_min, mut c_min) = (f64::MAX, u64::MAX);
        for _ in 0..3 {
            let (t, c0) = (Instant::now(), cycles());
            let n = d.deflate_decompress(&input, &mut out).unwrap();
            c_min = c_min.min(cycles() - c0);
            s_min = s_min.min(t.elapsed().as_secs_f64());
            assert_eq!(n as u64, size);
        }
        assert_eq!(crc32fast::hash(&out), crc);
        println!(
            "libdeflate {best}: {:.0} MB packed -> {:.0} MB, {:.0} MB/s, {:.2} cycles/out B, {:.2} cycles/packed B, {:.2} GHz",
            packed as f64 / 1048576.0,
            size as f64 / 1048576.0,
            size as f64 / 1048576.0 / s_min,
            c_min as f64 / size as f64,
            c_min as f64 / packed as f64,
            c_min as f64 / s_min / 1e9
        );
    }
}
