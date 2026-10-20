//! ZIP: every entry is independently compressed, so all of them decode in parallel straight
//! out of a memory map.

use std::{io::Cursor, path::Path, time::SystemTime};

use rayon::prelude::*;
use zip::{ZipArchive, extra_fields::ExtraField, read::ZipFile};

use crate::{EntryInfo, Options, Result, mmap, sink::Sink, time};

pub(crate) fn list(path: &Path) -> Result<Vec<EntryInfo>> {
    let map = mmap(path)?;
    let mut archive = ZipArchive::new(Cursor::new(&map[..]))?;
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

pub(crate) fn extract(path: &Path, sink: &Sink, opts: &Options) -> Result<()> {
    let map = mmap(path)?;
    let archive = ZipArchive::new(Cursor::new(&map[..]))?;

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
    // Biggest first, so one huge entry doesn't start last and leave the other cores idle.
    files.sort_unstable_by(|a, b| b.cmp(a));

    let password = opts.password.as_deref().map(str::as_bytes);
    opts.pool()?.install(|| {
        files
            .par_iter()
            .try_for_each_init(|| archive.clone(), |archive, &(_, i)| {
                let mut f = match password {
                    Some(pw) => archive.by_index_decrypt(i, pw)?,
                    None => archive.by_index(i)?,
                };
                let name = f.name()?.into_owned();
                let (size, modified, encrypted) = (f.size(), mtime(&f), f.encrypted());
                sink.file(&name, Some(size), modified, &mut f)
                    .map_err(|e| e.for_entry(&name, encrypted && password.is_some()))
            })
    })
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
