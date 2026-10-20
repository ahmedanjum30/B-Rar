//! ZIP: every entry is independently compressed, so all of them decode in parallel straight
//! out of a memory map.

use std::{io::Cursor, path::Path, sync::Mutex, time::SystemTime};

use zip::{
    ZipArchive,
    extra_fields::ExtraField,
    read::{ArchiveOffset, Config, ZipFile},
};

use crate::{EntryInfo, Options, Result, mmap, sink::Sink, time};

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

pub(crate) fn extract(path: &Path, sink: &Sink, opts: &Options) -> Result<()> {
    let map = mmap(path)?;
    let archive = open(&map)?;

    let mut files = Vec::with_capacity(archive.len());
    let mut total = 0;
    {
        let mut archive = archive.clone();
        for i in 0..archive.len() {
            let f = archive.by_index_raw(i)?;
            if f.is_dir() {
                sink.dir(&f.name()?)?;
            } else {
                total += f.size();
                files.push((f.compressed_size(), i));
            }
        }
    }
    sink.set_total(total);
    // Biggest first, so a huge entry never starts last and leaves the other cores idle.
    files.sort_unstable_by(|a, b| b.cmp(a));

    // One FIFO job per entry: jobs start strictly in that order, one per free thread. (A
    // parallel iterator splits the list into ranges instead, which let the two largest
    // entries of an archive run back to back on one thread.)
    let password = opts.password.as_deref().map(str::as_bytes);
    let failure = Mutex::new(None);
    opts.pool()?.scope_fifo(|s| {
        for &(_, i) in &files {
            let (archive, failure) = (&archive, &failure);
            s.spawn_fifo(move |_| {
                if failure.lock().unwrap().is_some() {
                    return;
                }
                if let Err(e) = extract_entry(archive.clone(), i, password, sink) {
                    failure.lock().unwrap().get_or_insert(e);
                }
            });
        }
    });
    failure.into_inner().unwrap().map_or(Ok(()), Err)
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
