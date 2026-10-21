//! RAR: B-Rar's own RAR5 decoder first, the official unRAR library for everything else.
//!
//! Fast path ([`crate::rar5`]): files in non-solid RAR5 archives (in one or several volumes,
//! encrypted or not) decode in parallel, one whole file per job, each checked against its
//! CRC32. Big files, and solid archives, go through the two-phase engine ([`rar5::parallel`]),
//! which spreads one stream over all cores. Any file B-Rar's decoder can't handle or that fails its check goes to
//! unRAR, so a decoder bug costs speed, never correctness.
//!
//! unRAR path: several unRAR handles each decode their own share of a non-solid archive in
//! parallel (WinRAR decodes one file at a time). Solid and multi-volume archives get one
//! sequential pass, where small files are decoded into memory and written on the background
//! pool while unRAR moves on.

use std::{cell::RefCell, cmp::Reverse, collections::HashSet, path::Path, sync::Mutex};

use rayon::prelude::*;
use unrar::VolumeInfo;

use crate::{
    EntryInfo, Error, IoContext, Options, Result,
    rar5::{
        self, Source,
        unpack::{INPUT_PADDING, OUTPUT_SLACK, Unpacker},
    },
    sink::{FileOut, SMALL_FILE, Sink},
    time,
    whole::{BUDGET, Budget},
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
            // A file continued in the next volume only reports this volume's share.
            packed: (!h.is_directory() && !h.is_split_after()).then_some(h.packed_size),
            modified: mtime(&h),
        });
    }
    Ok(entries)
}

pub(crate) fn extract(path: &Path, sink: &Sink, opts: &Options) -> Result<()> {
    // BRAR_NO_RAR5=1 forces the unRAR path (for comparing the two decoders).
    if std::env::var_os("BRAR_NO_RAR5").is_none()
        && let Some(archive) = rar5::parse(path, opts.password.as_deref())?
    {
        return if archive.solid {
            extract_solid(path, archive, sink, opts)
        } else {
            extract_rar5(path, archive, sink, opts)
        };
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

/// Files at least this big may go to the two-phase engine, which spreads one file over all
/// cores (see [`extract_rar5`]).
const BIG_FILE: u64 = 64 << 20;
/// The engine's window holds min(dictionary, output) bytes; past this, unRAR (memory).
const MAX_WINDOW: u64 = 4 << 30;

fn extract_rar5(path: &Path, archive: rar5::Archive, sink: &Sink, opts: &Options) -> Result<()> {
    let picked: Vec<&rar5::Entry> = archive.entries.iter().filter(|e| sink.selected(&e.name)).collect();
    sink.set_total(picked.iter().filter(|e| !e.is_dir).map(|e| e.size).sum());
    let mut candidates = Vec::new();
    let mut retry = Vec::new();
    let mut big = Vec::new();
    for e in picked {
        if e.is_dir {
            sink.dir(&e.name)?;
        } else if !e.supported {
            retry.push(e.index);
        } else if e.packed.saturating_add(e.size) > BUDGET {
            // Too big to decode in memory: the engine streams it (stored data is unRAR's).
            if e.method == 0 { retry.push(e.index) } else { big.push(e) }
        } else {
            candidates.push(e);
        }
    }

    // A file that alone outweighs its fair share of the work (and is big) goes to the engine,
    // which spreads it over all cores; everything else is decoded one whole file per thread,
    // both at once. On one core everything takes the one-file-per-thread path.
    let threads = opts.thread_count() as u64;
    let work: u64 = candidates.iter().map(|e| e.size).sum();
    let cut = if threads > 1 { (work / threads).max(BIG_FILE) } else { u64::MAX };
    let (more_big, mut fast): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|e| e.size >= cut && e.method != 0);
    big.extend(more_big);
    // Biggest first, one FIFO job each (see zip.rs for why not a parallel iterator).
    fast.sort_unstable_by_key(|e| Reverse(e.packed));

    let src = Source::open(&archive)?;
    let budget = Budget::new(BUDGET);
    let failed = Mutex::new(Vec::new());
    let stop = Mutex::new(None);
    let big_result = std::thread::scope(|scope| -> Result<Result<Vec<usize>>> {
        // The engine runs on its own threads (it has its own workers). Several big files at
        // once, sharing the cores: each file's in-order phase is one thread, so one file at a
        // time would leave the others' decoding waiting on it.
        let big_job = (!big.is_empty()).then(|| {
            scope.spawn(|| {
                let at_once = big.len().min(4).min((threads as usize / 2).max(1));
                let per_file = (threads as usize / at_once).max(1);
                let next = std::sync::atomic::AtomicUsize::new(0);
                let retry = Mutex::new(Vec::new());
                let stop = Mutex::new(None);
                std::thread::scope(|s| {
                    for _ in 0..at_once {
                        s.spawn(|| {
                            loop {
                                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if i >= big.len() || stop.lock().unwrap().is_some() {
                                    return;
                                }
                                match run_engine(&src, &[big[i]], sink, per_file) {
                                    Ok(()) => {}
                                    Err(Fail::Retry) => retry.lock().unwrap().push(big[i].index),
                                    Err(Fail::Stop(err)) => {
                                        stop.lock().unwrap().get_or_insert(err);
                                    }
                                }
                            }
                        });
                    }
                });
                match stop.into_inner().unwrap() {
                    Some(err) => Err(err),
                    None => Ok(retry.into_inner().unwrap()),
                }
            })
        });
        opts.pool()?.scope_fifo(|s| {
            for e in &fast {
                let (src, budget, failed, stop) = (&src, &budget, &failed, &stop);
                s.spawn_fifo(move |_| {
                    if stop.lock().unwrap().is_some() {
                        return;
                    }
                    match extract_fast(src, e, sink, budget) {
                        Ok(()) => {}
                        Err(Fail::Retry) => failed.lock().unwrap().push(e.index),
                        Err(Fail::Stop(err)) => {
                            stop.lock().unwrap().get_or_insert(err);
                        }
                    }
                });
            }
        });
        Ok(big_job.map_or(Ok(Vec::new()), |job| job.join().expect("engine thread panicked")))
    })?;
    if let Some(err) = stop.into_inner().unwrap() {
        return Err(err);
    }
    let big_failed = big_result?;
    let failed = failed.into_inner().unwrap();
    // BRAR_STATS=1: how many files each decoder handled (to catch silent fallbacks in tests).
    if std::env::var_os("BRAR_STATS").is_some() {
        eprintln!(
            "rar5: {} files by B-Rar's decoder, {} big files by the engine (multithreaded), {} failed the check, {} not supported -> unRAR",
            fast.len() - failed.len(),
            big.len() - big_failed.len(),
            failed.len() + big_failed.len(),
            retry.len()
        );
    }
    retry.extend(failed);
    retry.extend(big_failed);
    if retry.is_empty() {
        return Ok(());
    }
    let only: HashSet<usize> = retry.into_iter().collect();
    extract_unrar(path, sink, opts, Some(&only))
}

/// A solid archive: one stream across all files, decoded by the engine in parallel. Anything
/// it can't do (stored files, no or a wrong password, damage) sends the whole archive to unRAR.
fn extract_solid(path: &Path, archive: rar5::Archive, sink: &Sink, opts: &Options) -> Result<()> {
    let files: Vec<&rar5::Entry> = archive.entries.iter().filter(|e| !e.is_dir).collect();
    if files.iter().any(|e| !e.supported || e.method == 0) {
        return extract_unrar(path, sink, opts, None);
    }
    sink.set_total(files.iter().filter(|e| sink.selected(&e.name)).map(|e| e.size).sum());
    for e in archive.entries.iter().filter(|e| e.is_dir) {
        sink.dir(&e.name)?;
    }
    let result = run_engine(&Source::open(&archive)?, &files, sink, opts.thread_count());
    if std::env::var_os("BRAR_STATS").is_some() {
        let how = if result.is_ok() { "the engine" } else { "unRAR" };
        eprintln!("rar5: solid archive, {} files by {how}", files.len());
    }
    match result {
        Ok(()) => Ok(()),
        Err(Fail::Stop(err)) => Err(err),
        Err(Fail::Retry) => extract_unrar(path, sink, opts, None),
    }
}

/// The two-phase engine over `files` (one big file, or a solid archive's files in order),
/// with `threads` workers.
fn run_engine(src: &Source, files: &[&rar5::Entry], sink: &Sink, threads: usize) -> std::result::Result<(), Fail> {
    let total: u64 = files.iter().map(|e| e.size).sum();
    let window = files.iter().map(|e| e.dict).max().unwrap_or(0).min(total);
    if window > MAX_WINDOW {
        return Err(Fail::Retry);
    }
    let mut out = EngineOutput {
        sink,
        files,
        open: None,
        written: Vec::new(),
    };
    match rar5::parallel::decode(src, files, window as usize, threads, &mut out) {
        Ok(()) => Ok(()),
        Err(rar5::parallel::Failure::Corrupt) => {
            out.discard();
            Err(Fail::Retry)
        }
        Err(rar5::parallel::Failure::Stop(err)) => {
            if let Some(Open::Stream(f)) = out.open.take() {
                f.discard();
            }
            Err(Fail::Stop(err))
        }
    }
}

/// The engine's output, into the sink, file by file. Small files are collected and written
/// on the sink's writer pool once their CRC matched (creating files costs more than decoding
/// them, so a solid archive of many small files would otherwise be bound by one thread's file
/// creation); big ones stream straight to disk.
struct EngineOutput<'a> {
    sink: &'a Sink,
    files: &'a [&'a rar5::Entry],
    open: Option<Open>,
    /// Files written completely, removed again if a later file fails (unRAR redoes them all).
    written: Vec<std::path::PathBuf>,
}

enum Open {
    Stream(FileOut),
    Buffer(Vec<u8>),
}

impl EngineOutput<'_> {
    fn discard(&mut self) {
        if let Some(Open::Stream(f)) = self.open.take() {
            f.discard();
        }
        self.sink.flush();
        for p in self.written.drain(..) {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl rar5::parallel::Output for EngineOutput<'_> {
    fn start(&mut self, file: usize) -> Result<()> {
        let e = self.files[file];
        self.open = Some(if e.size <= SMALL_FILE {
            Open::Buffer(Vec::with_capacity(e.size as usize))
        } else {
            Open::Stream(self.sink.begin_file(&e.name, Some(e.size), e.modified)?)
        });
        Ok(())
    }

    fn write(&mut self, _file: usize, data: &[u8]) -> Result<()> {
        match self.open.as_mut().expect("a file is open") {
            Open::Stream(f) => f.write(data),
            Open::Buffer(v) => {
                v.extend_from_slice(data);
                Ok(())
            }
        }
    }

    fn finish(&mut self, file: usize) -> Result<()> {
        match self.open.take().expect("a file is open") {
            Open::Stream(f) => {
                if let Some(p) = f.path() {
                    self.written.push(p.to_path_buf());
                }
                f.finish();
            }
            Open::Buffer(data) => {
                let e = self.files[file];
                if self.sink.selected(&e.name)
                    && let Some(p) = self.sink.target(&e.name)?
                {
                    self.written.push(p);
                }
                self.sink.file_owned(&e.name, e.modified, data)?;
            }
        }
        Ok(())
    }
}

fn extract_fast(src: &Source, e: &rar5::Entry, sink: &Sink, budget: &Budget) -> std::result::Result<(), Fail> {
    sink.check().map_err(Fail::Stop)?;
    sink.start(&e.name);
    let (packed, size) = (e.packed as usize, e.size as usize);
    let _lease = budget.take(e.packed + e.size);

    let mut input = Vec::with_capacity(packed + INPUT_PADDING);
    e.read(src, 0, e.packed, &mut input).map_err(|_| Fail::Retry)?;
    input.resize(packed + INPUT_PADDING, 0);
    let data = if e.method == 0 {
        // Encrypted data is padded to whole AES blocks.
        if packed != size && !(e.crypt.is_some() && packed >= size && packed - size < 16) {
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
    if !e.check.ok(crc32fast::hash(&data)) {
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
    // With encrypted headers unRAR can't see the archive flags yet and reports neither solid
    // nor volume, so the name counts too: a volume set needs the one sequential pass.
    let independent = !probe.is_solid() && matches!(probe.volume_info(), VolumeInfo::None) && crate::volumes::RarSet::of(path).is_none();
    drop(probe);

    // Header-only pass (cheap): sizes for the progress total and for load balancing.
    let sizes: Vec<Option<u64>> = open(path, opts)
        .open_for_listing()?
        .enumerate()
        .map(|(i, h)| {
            h.map(|h| {
                let picked = wanted(i) && sink.selected(&h.filename.to_string_lossy());
                (picked && !h.is_directory() && !h.is_split_before()).then_some(h.unpacked_size)
            })
        })
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
    // Directories go to worker 0; they're just a mkdir. Entries outside `only` keep owner 0
    // too, so `wanted` must still filter: another decoder may be writing them right now.
    opts.pool()?.install(|| {
        (0..workers)
            .into_par_iter()
            .try_for_each(|w| extract_where(path, sink, opts, |i| wanted(i) && owner.get(i) == Some(&w)))
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
        if !sink.selected(&name) {
            archive = header.skip()?;
            continue;
        }
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
