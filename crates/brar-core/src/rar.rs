//! RAR: B-Rar's own RAR5 decoder first, the official unRAR library for everything else.
//!
//! Fast path ([`crate::rar5`]): files in non-solid, single-volume, unencrypted RAR5 archives
//! decode in parallel, one whole file per job, each checked against its CRC32. Any file the
//! fast path can't handle or that fails its check goes to unRAR, so a decoder bug costs speed,
//! never correctness.
//!
//! unRAR path: several unRAR handles each decode their own share of a non-solid archive in
//! parallel (WinRAR decodes one file at a time). Solid and multi-volume archives get one
//! sequential pass, where small files are decoded into memory and written on the background
//! pool while unRAR moves on.

use std::{cell::RefCell, cmp::Reverse, collections::HashSet, fs::File, path::Path, sync::Mutex};

use rayon::prelude::*;
use unrar::VolumeInfo;

use crate::{
    EntryInfo, Error, IoContext, Options, Result,
    rar5::{
        self,
        unpack::{INPUT_PADDING, OUTPUT_SLACK, Unpacker},
    },
    sink::{SMALL_FILE, Sink},
    time,
    whole::{BUDGET, Budget, read_at},
};

thread_local! {
    static UNPACKER: RefCell<Unpacker> = RefCell::new(Unpacker::default());
}

fn open(path: &Path, opts: &Options) -> unrar::Archive<'static> {
    // unrar borrows its inputs for the archive's lifetime; leaking two small values per
    // opened archive keeps the API simple.
    let path: &'static Path = Box::leak(path.to_path_buf().into_boxed_path());
    match &opts.password {
        Some(pw) => unrar::Archive::with_password(path, Box::leak(pw.clone().into_boxed_str())),
        None => unrar::Archive::new(path),
    }
}

pub(crate) fn list(path: &Path, opts: &Options) -> Result<Vec<EntryInfo>> {
    let mut entries = Vec::new();
    for header in open(path, opts).open_for_listing()? {
        let h = header?;
        // Split entries repeat once per volume; keep the first.
        if h.is_split_before() {
            continue;
        }
        entries.push(EntryInfo {
            name: h.filename.to_string_lossy().into_owned(),
            is_dir: h.is_directory(),
            size: h.unpacked_size,
            packed: None,
            modified: mtime(&h),
        });
    }
    Ok(entries)
}

pub(crate) fn extract(path: &Path, sink: &Sink, opts: &Options) -> Result<()> {
    // BRAR_NO_RAR5=1 forces the unRAR path (for comparing the two decoders).
    if std::env::var_os("BRAR_NO_RAR5").is_none()
        && let Some(archive) = rar5::parse(path)?
    {
        return extract_rar5(path, archive, sink, opts);
    }
    extract_unrar(path, sink, opts, None)
}

/// Why a fast-path job stopped.
enum Fail {
    /// The fast path couldn't produce this file (unsupported data, decode error, CRC
    /// mismatch): hand it to unRAR, which reports real damage properly.
    Retry,
    /// Writing, cancelling, a conflict prompt answered with "cancel": stop everything.
    Stop(Error),
}

/// Files at least this big may go to unRAR's multithreaded decoder (see [`extract_rar5`]).
const BIG_FILE: u64 = 64 << 20;

fn extract_rar5(path: &Path, archive: rar5::Archive, sink: &Sink, opts: &Options) -> Result<()> {
    sink.set_total(archive.entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum());
    let mut candidates = Vec::new();
    let mut retry = Vec::new();
    for e in &archive.entries {
        if e.is_dir {
            sink.dir(&e.name)?;
        } else if e.supported && e.packed.saturating_add(e.size) <= BUDGET {
            candidates.push(e);
        } else {
            retry.push(e.index);
        }
    }

    // Per file, the faster decoder: B-Rar's own decodes one file per thread and beats unRAR per
    // core; unRAR splits a single file across threads. So a file that alone outweighs its fair
    // share of the work (and is big) goes to unRAR, everything else to B-Rar's decoder, both at
    // once. On one core B-Rar's decoder takes everything.
    let threads = opts.thread_count() as u64;
    let work: u64 = candidates.iter().map(|e| e.size).sum();
    let cut = if threads > 1 { (work / threads).max(BIG_FILE) } else { u64::MAX };
    let (big, mut fast): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|e| e.size >= cut);
    let big: HashSet<usize> = big.iter().map(|e| e.index).collect();
    // Biggest first, one FIFO job each (see zip.rs for why not a parallel iterator).
    fast.sort_unstable_by_key(|e| Reverse(e.packed));

    let file = File::open(path).at(path)?;
    let budget = Budget::new(BUDGET);
    let failed = Mutex::new(Vec::new());
    let stop = Mutex::new(None);
    let big_result = std::thread::scope(|scope| -> Result<Result<()>> {
        let big_job = (!big.is_empty()).then(|| scope.spawn(|| extract_unrar(path, sink, opts, Some(&big))));
        opts.pool()?.scope_fifo(|s| {
            for e in &fast {
                let (file, budget, failed, stop) = (&file, &budget, &failed, &stop);
                s.spawn_fifo(move |_| {
                    if stop.lock().unwrap().is_some() {
                        return;
                    }
                    match extract_fast(file, e, sink, budget) {
                        Ok(()) => {}
                        Err(Fail::Retry) => failed.lock().unwrap().push(e.index),
                        Err(Fail::Stop(err)) => {
                            stop.lock().unwrap().get_or_insert(err);
                        }
                    }
                });
            }
        });
        Ok(big_job.map_or(Ok(()), |job| job.join().expect("unRAR thread panicked")))
    })?;
    if let Some(err) = stop.into_inner().unwrap() {
        return Err(err);
    }
    big_result?;
    let failed = failed.into_inner().unwrap();
    // BRAR_STATS=1: how many files each decoder handled (to catch silent fallbacks in tests).
    if std::env::var_os("BRAR_STATS").is_some() {
        eprintln!(
            "rar5: {} files by B-Rar's decoder, {} big files by unRAR (multithreaded), {} failed the check, {} not supported -> unRAR",
            fast.len() - failed.len(),
            big.len(),
            failed.len(),
            retry.len()
        );
    }
    retry.extend(failed);
    if retry.is_empty() {
        return Ok(());
    }
    let only: HashSet<usize> = retry.into_iter().collect();
    extract_unrar(path, sink, opts, Some(&only))
}

fn extract_fast(file: &File, e: &rar5::Entry, sink: &Sink, budget: &Budget) -> std::result::Result<(), Fail> {
    sink.check().map_err(Fail::Stop)?;
    sink.start(&e.name);
    let (packed, size) = (e.packed as usize, e.size as usize);
    let _lease = budget.take(e.packed + e.size);

    let mut input = vec![0u8; packed + INPUT_PADDING];
    read_at(file, e.data_start, &mut input[..packed]).map_err(|_| Fail::Retry)?;
    let data = if e.method == 0 {
        if packed != size {
            return Err(Fail::Retry);
        }
        input.truncate(size);
        input
    } else {
        let mut out = vec![0u8; size + OUTPUT_SLACK];
        UNPACKER
            .with_borrow_mut(|u| u.unpack(&input, packed, &mut out, size, e.extra_dist))
            .map_err(|_| Fail::Retry)?;
        drop(input);
        out.truncate(size);
        out
    };
    if crc32fast::hash(&data) != e.crc {
        return Err(Fail::Retry);
    }
    let written = if e.size <= SMALL_FILE {
        sink.file_owned(&e.name, e.modified, data)
    } else {
        sink.file_bytes(&e.name, e.modified, &data)
    };
    written.map_err(Fail::Stop)
}

/// The unRAR path, for the whole archive or (`only`) just the given file-header indices.
fn extract_unrar(path: &Path, sink: &Sink, opts: &Options, only: Option<&HashSet<usize>>) -> Result<()> {
    let wanted = |i: usize| only.is_none_or(|set| set.contains(&i));
    let probe = open(path, opts).open_for_processing()?;
    let independent = !probe.is_solid() && matches!(probe.volume_info(), VolumeInfo::None);
    drop(probe);

    // Header-only pass (cheap): sizes for the progress total and for load balancing.
    let sizes: Vec<Option<u64>> = open(path, opts)
        .open_for_listing()?
        .enumerate()
        .map(|(i, h)| h.map(|h| (wanted(i) && !h.is_directory() && !h.is_split_before()).then_some(h.unpacked_size)))
        .collect::<std::result::Result<_, _>>()?;
    if only.is_none() {
        sink.set_total(sizes.iter().flatten().sum());
    }

    let files = sizes.iter().flatten().count();
    let workers = if independent { opts.thread_count().min(files) } else { 1 };
    if workers < 2 {
        return extract_where(path, sink, opts, wanted);
    }

    // Longest-processing-time-first: hand the biggest remaining file to the least-loaded
    // worker, so one large file doesn't leave the other cores idle at the end.
    let mut owner = vec![0usize; sizes.len()];
    let mut load = vec![0u64; workers];
    let mut order: Vec<usize> = (0..sizes.len()).filter(|&i| sizes[i].is_some()).collect();
    order.sort_by_key(|&i| Reverse(sizes[i]));
    for i in order {
        let w = (0..workers).min_by_key(|&w| load[w]).unwrap();
        owner[i] = w;
        // Floor the cost: each file also pays fixed open/close overhead.
        load[w] += sizes[i].unwrap_or(0).max(64 << 10);
    }
    // Directories go to worker 0; they're just a mkdir.
    opts.pool()?.install(|| {
        (0..workers)
            .into_par_iter()
            .try_for_each(|w| extract_where(path, sink, opts, |i| owner[i] == w))
    })
}

/// One sequential pass over the archive, unpacking the entries `keep` selects by header
/// index and skipping the rest (a seek, in a non-solid archive).
fn extract_where(path: &Path, sink: &Sink, opts: &Options, keep: impl Fn(usize) -> bool) -> Result<()> {
    let _hook = ProgressHook::install(sink);
    let mut archive = open(path, opts).open_for_processing()?;
    let mut index = 0;
    while let Some(header) = archive.read_header()? {
        sink.check()?;
        let entry = header.entry();
        let wanted = keep(index);
        index += 1;
        if !wanted {
            archive = header.skip()?;
            continue;
        }
        let name = entry.filename.to_string_lossy().into_owned();
        let size = entry.unpacked_size;
        let modified = mtime(entry);
        // RAR4 can't verify a password up front; a wrong one shows up as a CRC error.
        let bad_data_means_wrong_password = entry.is_encrypted() && opts.password.is_some();
        let wrong_pw = |e: unrar::error::UnrarError| Error::from(e).for_entry(&name, bad_data_means_wrong_password);

        archive = if entry.is_directory() {
            sink.dir(&name)?;
            header.skip()?
        } else if let Some(target) = sink.target(&name)? {
            if size <= SMALL_FILE {
                let (data, next) = header.read().map_err(wrong_pw)?;
                sink.file_owned_reported(&name, modified, data)?;
                next
            } else {
                sink.start(&name);
                match sink.claim(&target, Some(size), modified)? {
                    Some(target) => {
                        if let Some(parent) = target.parent() {
                            std::fs::create_dir_all(parent).at(parent)?;
                        }
                        let next = header.extract_to(&target).map_err(wrong_pw)?;
                        sink.count_file(size);
                        next
                    }
                    None => header.skip()?,
                }
            }
        } else {
            sink.start(&name);
            let next = header.test().map_err(wrong_pw)?;
            sink.count_file(size);
            next
        };
    }
    Ok(())
}

/// Routes unRAR's per-chunk callback on this thread into [`crate::Progress`], so large files
/// report progress as they unpack and a cancel stops them mid-file.
struct ProgressHook(bool);

impl ProgressHook {
    fn install(sink: &Sink) -> Self {
        let Some(progress) = sink.progress().cloned() else {
            return Self(false);
        };
        unrar::set_progress_hook(Some(Box::new(move |n| {
            progress.advance(n as u64);
            // Pausing blocks unRAR right here, mid-file; false stops it.
            progress.wait_if_paused()
        })));
        Self(true)
    }
}

impl Drop for ProgressHook {
    fn drop(&mut self) {
        if self.0 {
            unrar::set_progress_hook(None);
        }
    }
}

fn mtime(h: &unrar::FileHeader) -> Option<std::time::SystemTime> {
    match h.mtime_filetime {
        0 => time::dos(h.file_time),
        ticks => time::filetime(ticks),
    }
}
