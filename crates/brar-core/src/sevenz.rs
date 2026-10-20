//! 7z: solid blocks are independent, so blocks decode in parallel; LZMA2 streams written with
//! multithreading can additionally split across threads inside one block.

use std::{
    io::{Cursor, Read, Seek},
    time::SystemTime,
};

use rayon::prelude::*;
use sevenz_rust2::{Archive, ArchiveEntry, BlockDecoder, EncoderMethod, Password};

use crate::{EntryInfo, Error, Options, Result, mmap, sink::Sink, time, volumes::Input};

fn password(opts: &Options) -> Password {
    opts.password.as_deref().map_or_else(Password::empty, Password::from)
}

pub(crate) fn list(input: &Input, opts: &Options) -> Result<Vec<EntryInfo>> {
    match &input.split {
        None => {
            let map = mmap(&input.first)?;
            list_from(&mut Cursor::new(&map[..]), opts)
        }
        Some(parts) => list_from(&mut parts.clone(), opts),
    }
}

fn list_from<R: Read + Seek>(reader: &mut R, opts: &Options) -> Result<Vec<EntryInfo>> {
    let archive = Archive::read(reader, &password(opts))?;
    Ok(archive
        .files
        .iter()
        .filter(|f| !f.is_anti_item)
        .map(|f| EntryInfo {
            name: f.name.clone(),
            is_dir: f.is_directory,
            size: f.size,
            packed: None,
            modified: mtime(f),
        })
        .collect())
}

pub(crate) fn extract(input: &Input, sink: &Sink, opts: &Options) -> Result<()> {
    match &input.split {
        None => {
            let map = mmap(&input.first)?;
            extract_from(&|| Cursor::new(&map[..]), sink, opts)
        }
        Some(parts) => extract_from(&|| parts.clone(), sink, opts),
    }
}

/// `new_reader` hands out independent readers over the archive, one per decoding thread.
fn extract_from<R: Read + Seek>(new_reader: &(dyn Fn() -> R + Sync), sink: &Sink, opts: &Options) -> Result<()> {
    let password = password(opts);
    let archive = Archive::read(&mut new_reader(), &password)?;

    sink.set_total(archive.files.iter().filter(|f| !f.is_directory).map(|f| f.size).sum());

    // Directories and empty files have no data stream and belong to no block.
    for f in archive.files.iter().filter(|f| !f.has_stream && !f.is_anti_item) {
        if f.is_directory {
            sink.dir(&f.name)?;
        } else {
            sink.file_owned(&f.name, mtime(f), Vec::new())?;
        }
    }

    let blocks = archive.blocks.len();
    let threads = opts.thread_count();
    // With fewer blocks than cores, spend the spare cores inside each LZMA2 block.
    let per_block = (threads / blocks.clamp(1, threads)).max(1) as u32;

    opts.pool()?.install(|| {
        // One block per task, so big blocks never queue behind each other on one thread.
        (0..blocks).into_par_iter().with_max_len(1).try_for_each(|block| {
            let encrypted = opts.password.is_some()
                && archive.blocks[block]
                    .coders
                    .iter()
                    .any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256);
            let mut source = new_reader();
            let decoder = BlockDecoder::new(per_block, block, &archive, &password, &mut source);
            let mut failure = None;
            let result = decoder.for_each_entries(&mut |entry, reader| {
                if entry.is_directory || !entry.has_stream || entry.is_anti_item {
                    return Ok(true);
                }
                match sink.file_buffered(&entry.name, entry.size, mtime(entry), reader) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        failure = Some(e.for_entry(&entry.name, encrypted));
                        Ok(false)
                    }
                }
            });
            let error = match (failure, result) {
                (Some(e), _) => e,
                (None, Err(e)) => Error::from(e),
                (None, Ok(_)) => return Ok(()),
            };
            Err(error.wrong_password_if(encrypted))
        })
    })
}

fn mtime(f: &ArchiveEntry) -> Option<SystemTime> {
    f.has_last_modified_date
        .then(|| time::filetime(f.last_modified_date.into()))
        .flatten()
}
