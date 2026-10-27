//! 7z: solid blocks are independent, so blocks decode in parallel; LZMA2 streams written with
//! multithreading can additionally split across threads inside one block.
//!
//! B-Rar reads the header itself ([`header`]), and when every block is one LZMA2, LZMA or
//! stored stream, maybe behind the x86 BCJ filter (7-Zip's default), decodes the whole archive
//! with its own decoders ([`crate::lzma`], [`crate::bcj`]). Otherwise (other methods and
//! filters, encryption, split archives) sevenz-rust2 reads the archive, and B-Rar's decoders
//! still take the blocks they can.

mod header;

use std::{
    io::{Cursor, Read, Seek},
    time::SystemTime,
};

use rayon::prelude::*;
use sevenz_rust2::{Archive, ArchiveEntry, BlockDecoder, EncoderMethod, Password};

use self::header::{Entry, Kind, Method};
use crate::{
    EntryInfo, Error, Options, Result, bcj, lzma, mmap,
    sink::{FileOut, SMALL_FILE, Sink},
    time,
    volumes::Input,
};

fn password(opts: &Options) -> Password {
    opts.password.as_deref().map_or_else(Password::empty, Password::from)
}

/// B-Rar's own reading of the archive's header (`None`: sevenz-rust2 reads it).
/// BRAR_NO_OWN_7Z=1 leaves everything to sevenz-rust2 (for comparing).
fn own_header(map: &[u8]) -> Option<header::Archive> {
    if std::env::var_os("BRAR_NO_OWN_7Z").is_some() {
        return None;
    }
    header::Archive::read(map).ok()
}

pub(crate) fn list(input: &Input, opts: &Options) -> Result<Vec<EntryInfo>> {
    match &input.split {
        None => {
            let map = mmap(&input.first)?;
            if let Some(a) = own_header(&map) {
                return Ok(a
                    .entries
                    .into_iter()
                    .filter(|e| !e.is_anti)
                    .map(|e| EntryInfo {
                        name: e.name,
                        is_dir: e.is_dir,
                        size: e.size,
                        packed: None,
                        modified: e.mtime,
                    })
                    .collect());
            }
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
            if let Some(a) = own_header(&map).filter(|a| a.folders.iter().all(|f| f.method().is_some())) {
                return extract_own(&map, &a, sink, opts);
            }
            extract_from(&|| Cursor::new(&map[..]), Some(&map[..]), sink, opts)
        }
        Some(parts) => extract_from(&|| parts.clone(), None, sink, opts),
    }
}

/// Extracts an archive whose header B-Rar read and whose every block it can decode.
fn extract_own(map: &[u8], a: &header::Archive, sink: &Sink, opts: &Options) -> Result<()> {
    let picked: Vec<bool> = a.entries.iter().map(|e| sink.selected(&e.name)).collect();
    let total = a.entries.iter().zip(&picked).filter(|(e, p)| **p && !e.is_dir);
    sink.set_total(total.map(|(e, _)| e.size).sum());
    // Each block's files, and whether any is picked (blocks holding none aren't decoded).
    let mut files: Vec<Vec<&Entry>> = a.folders.iter().map(|_| Vec::new()).collect();
    let mut wanted = vec![false; a.folders.len()];
    for (e, p) in a.entries.iter().zip(&picked) {
        match e.folder {
            Some(f) => {
                files[f].push(e);
                wanted[f] |= *p;
            }
            None if e.is_anti => {}
            None if e.is_dir => sink.dir(&e.name)?,
            None => sink.file_owned(&e.name, e.mtime, Vec::new())?,
        }
    }
    let blocks = a.folders.len();
    let threads = opts.thread_count();
    let per_block = (threads / blocks.clamp(1, threads)).max(1);
    opts.pool()?.install(|| {
        (0..blocks).into_par_iter().with_max_len(1).filter(|&f| wanted[f]).try_for_each(|f| {
            let folder = &a.folders[f];
            let method = folder.method().expect("checked before");
            let damaged = || Error::Damaged(files[f].first().map_or("", |e| e.name.as_str()).to_owned());
            let packed = a.packed(map, folder).map_err(|_| damaged())?;
            decode_block(packed, &method, folder.size(), folder.crc, &files[f], per_block, sink)
        })
    })
}

/// `new_reader` hands out independent readers over the archive, one per decoding thread;
/// `map` is the whole archive, when it's one file (for B-Rar's own LZMA2 decoder).
fn extract_from<R: Read + Seek>(new_reader: &(dyn Fn() -> R + Sync), map: Option<&[u8]>, sink: &Sink, opts: &Options) -> Result<()> {
    let password = password(opts);
    let archive = Archive::read(&mut new_reader(), &password)?;

    let picked: Vec<bool> = archive.files.iter().map(|f| sink.selected(&f.name)).collect();
    let total = archive.files.iter().zip(&picked).filter(|(f, p)| **p && !f.is_directory);
    sink.set_total(total.map(|(f, _)| f.size).sum());
    // Blocks holding no picked file aren't decoded at all.
    let mut wanted = vec![false; archive.blocks.len()];
    for (block, _) in archive.stream_map.file_block_index.iter().zip(&picked).filter(|(_, p)| **p) {
        if let Some(b) = block {
            wanted[*b] = true;
        }
    }

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
    // BRAR_NO_OWN_7Z=1 leaves every block to sevenz-rust2 (for comparing the decoders).
    let own = map.filter(|_| std::env::var_os("BRAR_NO_OWN_7Z").is_none());

    opts.pool()?.install(|| {
        // One block per task, so big blocks never queue behind each other on one thread.
        (0..blocks).into_par_iter().with_max_len(1).filter(|&b| wanted[b]).try_for_each(|block| {
            let encrypted = opts.password.is_some()
                && archive.blocks[block]
                    .coders
                    .iter()
                    .any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256);
            if let Some(map) = own
                && let Some(done) = own_block(map, &archive, block, per_block as usize, sink)
            {
                return done;
            }
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

/// Decodes block `b` with B-Rar's own decoders if it is one LZMA2 or LZMA stream, maybe behind
/// the x86 BCJ filter, without encryption (LZMA2 on up to `threads` threads), writing its
/// files to the sink. `None`: it isn't, so sevenz-rust2 decodes it.
fn own_block(map: &[u8], archive: &Archive, b: usize, threads: usize, sink: &Sink) -> Option<Result<()>> {
    let block = &archive.blocks[b];
    let (coder, bcj) = match &block.coders[..] {
        [c] => (c, false),
        [f, c] | [c, f] if f.encoder_method_id() == EncoderMethod::ID_BCJ_X86 => (c, true),
        _ => return None,
    };
    let size = block.get_unpack_size();
    let kind = match coder.encoder_method_id() {
        // A window no bigger than the output.
        EncoderMethod::ID_LZMA2 => Kind::Lzma2(lzma::lzma2_dict(*coder.properties().first()?).ok()?),
        EncoderMethod::ID_LZMA => Kind::Lzma,
        _ => return None,
    };
    let method = Method {
        kind,
        props: coder.properties(),
        bcj,
    };
    let first = archive.stream_map.block_first_pack_stream_index()[b];
    let at = 32 + archive.pack_pos() + archive.stream_map.pack_stream_offsets()[first];
    let packed = map
        .get(usize::try_from(at).ok()?..)?
        .get(..usize::try_from(archive.pack_sizes()[first]).ok()?)?;

    // The block's files, in order: those after its first that belong to it (files without
    // data, in between, belong to no block).
    let start = archive.stream_map.block_first_file_index[b];
    let files: Vec<Entry> = (start..archive.files.len())
        .take_while(|&i| archive.stream_map.file_block_index[i].is_none_or(|x| x == b))
        .filter(|&i| archive.stream_map.file_block_index[i] == Some(b))
        .map(|i| {
            let f = &archive.files[i];
            Entry {
                name: f.name.clone(),
                size: f.size,
                crc: f.has_crc.then_some(f.crc as u32),
                is_dir: f.is_directory,
                is_anti: f.is_anti_item,
                mtime: mtime(f),
                folder: Some(b),
            }
        })
        .collect();
    let files: Vec<&Entry> = files.iter().collect();
    let crc = block.has_crc.then_some(block.crc as u32);
    Some(decode_block(packed, &method, size, crc, &files, threads, sink))
}

/// Decodes one block (`packed`, with `method`; `size` bytes out, `crc` over them all) into its
/// `files`, in order, LZMA2 on up to `threads` threads.
fn decode_block(packed: &[u8], method: &Method, size: u64, crc: Option<u32>, files: &[&Entry], threads: usize, sink: &Sink) -> Result<()> {
    let mut out = BlockOut {
        sink,
        files,
        next: 0,
        left: 0,
        crc: crc32fast::Hasher::new(),
        open: None,
        block_crc: crc.map(|_| crc32fast::Hasher::new()),
    };
    let result = (|| {
        out.empty_files()?;
        let mut filter = method.bcj.then(bcj::X86::new);
        let mut write = |data: &[u8]| match &mut filter {
            Some(f) => f.write(data, &mut |d| out.write(d)),
            None => out.write(data),
        };
        let decoded = match method.kind {
            // A window no bigger than the output.
            Kind::Lzma2(dict) => lzma::decode_lzma2_mt(packed, dict.min(usize::try_from(size).unwrap_or(usize::MAX)), threads, &mut write),
            Kind::Lzma => lzma::decode_lzma(packed, method.props, size, &mut write),
            Kind::Copy => write(packed).map(|()| packed.len() as u64).map_err(lzma::Failure::Stop),
        };
        if let Some(f) = &mut filter {
            f.finish(&mut |d| out.write(d))?;
        }
        match decoded {
            Ok(n) if n == size && out.next == out.files.len() => {}
            Ok(_) | Err(lzma::Failure::Bad) => return Err(Out::Bad),
            Err(lzma::Failure::Stop(e)) => return Err(e),
        }
        match (out.block_crc.take().map(crc32fast::Hasher::finalize), crc) {
            (Some(got), Some(crc)) if got != crc => Err(Out::Bad),
            _ => Ok(()),
        }
    })();
    result.map_err(|e| {
        let name = out.files.get(out.next).or(out.files.last()).map_or("", |f| f.name.as_str());
        if let Some(Open::Stream(f)) = out.open.take() {
            f.discard();
        }
        match e {
            Out::Bad => Error::SevenZ(sevenz_rust2::Error::ChecksumVerificationFailed).for_entry(name, false),
            Out::Stop(e) => e.for_entry(name, false),
        }
    })
}

/// Why writing a block's output stopped.
enum Out {
    /// The data is damaged (a CRC didn't match, or there's more of it than the files hold).
    Bad,
    Stop(Error),
}

impl From<Error> for Out {
    fn from(e: Error) -> Self {
        Out::Stop(e)
    }
}

/// A block's output, cut into its files. Small files are collected and written on the sink's
/// writer pool once their CRC matched; big ones stream to disk.
struct BlockOut<'a> {
    sink: &'a Sink,
    files: &'a [&'a Entry],
    /// The file being written (or next), its bytes still to come, and its CRC so far.
    next: usize,
    left: u64,
    crc: crc32fast::Hasher,
    open: Option<Open>,
    block_crc: Option<crc32fast::Hasher>,
}

enum Open {
    Stream(FileOut),
    Buffer(Vec<u8>),
    /// Not picked: only checked.
    Skip,
}

impl BlockOut<'_> {
    fn write(&mut self, mut data: &[u8]) -> std::result::Result<(), Out> {
        if let Some(h) = &mut self.block_crc {
            h.update(data);
        }
        while !data.is_empty() {
            if self.open.is_none() {
                self.start()?;
            }
            let n = data.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
            let (piece, rest) = data.split_at(n);
            self.crc.update(piece);
            match self.open.as_mut().expect("a file is open") {
                Open::Stream(f) => f.write(piece)?,
                Open::Buffer(v) => v.extend_from_slice(piece),
                Open::Skip => {}
            }
            self.left -= n as u64;
            data = rest;
            if self.left == 0 {
                self.finish()?;
                self.empty_files()?;
            }
        }
        Ok(())
    }

    fn start(&mut self) -> std::result::Result<(), Out> {
        let e = *self.files.get(self.next).ok_or(Out::Bad)?;
        self.left = e.size;
        self.crc = crc32fast::Hasher::new();
        self.open = Some(if !self.sink.selected(&e.name) {
            Open::Skip
        } else if e.size <= SMALL_FILE {
            Open::Buffer(Vec::with_capacity(e.size as usize))
        } else {
            Open::Stream(self.sink.begin_file(&e.name, Some(e.size), e.mtime)?)
        });
        Ok(())
    }

    fn finish(&mut self) -> std::result::Result<(), Out> {
        let e = self.files[self.next];
        let crc = std::mem::replace(&mut self.crc, crc32fast::Hasher::new()).finalize();
        if e.crc.is_some_and(|c| c != crc) {
            return Err(Out::Bad);
        }
        match self.open.take().expect("a file is open") {
            Open::Stream(f) => f.finish(),
            Open::Buffer(data) => self.sink.file_owned(&e.name, e.mtime, data)?,
            Open::Skip => {}
        }
        self.next += 1;
        Ok(())
    }

    /// Writes the files with no bytes that come next.
    fn empty_files(&mut self) -> std::result::Result<(), Out> {
        while self.files.get(self.next).is_some_and(|e| e.size == 0) {
            self.start()?;
            self.finish()?;
        }
        Ok(())
    }
}

fn mtime(f: &ArchiveEntry) -> Option<SystemTime> {
    f.has_last_modified_date.then(|| time::filetime(f.last_modified_date.into())).flatten()
}
