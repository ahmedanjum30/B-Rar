//! RAR via the official unRAR library.
//!
//! Non-solid archives compress every file independently, so several unRAR handles each
//! decode their own share of the files in parallel (WinRAR decodes one file at a time).
//! Solid and multi-volume archives fall back to one sequential pass, where small files are
//! decoded into memory and written on the background pool while unRAR moves on.

use std::{cmp::Reverse, path::Path};

use rayon::prelude::*;
use unrar::VolumeInfo;

use crate::{EntryInfo, Error, IoContext, Options, Result, sink::{SMALL_FILE, Sink}, time};

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
    let probe = open(path, opts).open_for_processing()?;
    let independent = !probe.is_solid() && matches!(probe.volume_info(), VolumeInfo::None);
    drop(probe);

    // Header-only pass (cheap): sizes for the progress total and for load balancing.
    let sizes: Vec<Option<u64>> = open(path, opts)
        .open_for_listing()?
        .map(|h| h.map(|h| (!h.is_directory() && !h.is_split_before()).then_some(h.unpacked_size)))
        .collect::<std::result::Result<_, _>>()?;
    sink.set_total(sizes.iter().flatten().sum());

    let files = sizes.iter().flatten().count();
    let workers = if independent { opts.thread_count().min(files) } else { 1 };
    if workers < 2 {
        return extract_where(path, sink, opts, |_| true);
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
