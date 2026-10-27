//! 7z, all B-Rar's own: the header ([`header`]), and the blocks. Solid blocks are independent,
//! so blocks decode in parallel. A block that's one LZMA2, LZMA or stored stream, maybe behind
//! the x86 BCJ filter (7-Zip's default), in a one-file archive, decodes straight from the
//! mapped file ([`decode_block`]; LZMA2 written on several threads splits across threads
//! inside the block too). Any other block (other methods and filters, BCJ2, encryption, split
//! archives) decodes as a chain of readers ([`coders`]).

mod coders;
mod header;

use std::io::{self, Read};

use rayon::prelude::*;

use self::{
    coders::{Keys, Source, folder_reader},
    header::{Entry, Kind, Method},
};
use crate::{
    EntryInfo, Error, Options, Result, bcj, lzma, mmap,
    sink::{FileOut, SMALL_FILE, Sink},
    volumes::Input,
};

/// Runs `f` on the archive's bytes: the file mapped, or the parts of a split archive.
fn with_source<T>(input: &Input, f: impl FnOnce(Source) -> Result<T>) -> Result<T> {
    match &input.split {
        None => {
            let map = mmap(&input.first)?;
            f(Source::Map(&map))
        }
        Some(parts) => f(Source::Parts(parts)),
    }
}

pub(crate) fn list(input: &Input, opts: &Options) -> Result<Vec<EntryInfo>> {
    with_source(input, |src| {
        let a = header::Archive::read(src, &Keys::new(opts.password.as_deref()))?;
        Ok(a.entries
            .into_iter()
            .filter(|e| !e.is_anti)
            .map(|e| EntryInfo {
                name: e.name,
                is_dir: e.is_dir,
                size: e.size,
                packed: None,
                modified: e.mtime,
            })
            .collect())
    })
}

pub(crate) fn extract(input: &Input, sink: &Sink, opts: &Options) -> Result<()> {
    with_source(input, |src| {
        let keys = Keys::new(opts.password.as_deref());
        let a = header::Archive::read(src, &keys)?;
        extract_all(src, &a, &keys, sink, opts)
    })
}

fn extract_all(src: Source, a: &header::Archive, keys: &Keys, sink: &Sink, opts: &Options) -> Result<()> {
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
    // With fewer blocks than cores, spend the spare cores inside each LZMA2 block.
    let per_block = (threads / blocks.clamp(1, threads)).max(1);
    opts.pool()?.install(|| {
        // One block per task, so big blocks never queue behind each other on one thread.
        (0..blocks).into_par_iter().with_max_len(1).filter(|&f| wanted[f]).try_for_each(|f| {
            let folder = &a.folders[f];
            let encrypted = folder.encrypted() && keys.has_password();
            let fast = folder.method().and_then(|m| {
                let &(at, len) = a.packs.get(folder.first_pack)?;
                Some((m, src.slice(at, len)?))
            });
            // An LZMA2 block in a split archive: read in parts, decoded in parallel.
            let split = folder
                .method()
                .filter(|m| matches!(m.kind, Kind::Lzma2(_)))
                .and_then(|m| Some((m, *a.packs.get(folder.first_pack)?)));
            let result = match (fast, split) {
                (Some((method, packed)), _) => decode_block(Packed::Map(packed), &method, folder.size(), folder.crc, &files[f], per_block, sink),
                (None, Some((method, (at, len)))) if matches!(src, Source::Parts(_)) => {
                    decode_block(Packed::Read(src, at, len), &method, folder.size(), folder.crc, &files[f], per_block, sink)
                }
                _ => folder_reader(src, &a.packs, folder, keys)
                    .and_then(|reader| decode_reader(reader, folder.size(), folder.crc, &files[f], encrypted, sink)),
            };
            result.map_err(|e| e.wrong_password_if(encrypted))
        })
    })
}

/// A block's packed bytes: in the mapped archive, or to read from `.0` at `.1` (`.2` of them).
#[derive(Clone, Copy)]
enum Packed<'a> {
    Map(&'a [u8]),
    Read(Source<'a>, u64, u64),
}

/// Decodes one block (`packed`, with `method`; `size` bytes out, `crc` over them all) into its
/// `files`, in order, LZMA2 on up to `threads` threads. (To read: LZMA2 only.)
fn decode_block(packed: Packed, method: &Method, size: u64, crc: Option<u32>, files: &[&Entry], threads: usize, sink: &Sink) -> Result<()> {
    let mut out = BlockOut::new(sink, files, crc, false);
    let result = (|| {
        out.empty_files()?;
        let mut filter = method.bcj.then(|| bcj::Filter::new(bcj::Kind::X86));
        let mut write = |data: &[u8]| match &mut filter {
            Some(f) => f.write(data, &mut |d| out.write(d)),
            None => out.write(data),
        };
        // A stream decoded on one thread: the filter, the CRCs and the writing run on this
        // one, beside it.
        let serial = match (packed, &method.kind) {
            (Packed::Map(p), Kind::Lzma2(_)) => !lzma::lzma2_splits(p, threads),
            (Packed::Map(_), _) => true,
            (Packed::Read(..), _) => false,
        };
        let decoded = if serial {
            pipe(|send| decode_to(packed, method, size, threads, send), &mut write)
        } else {
            decode_to(packed, method, size, threads, &mut write)
        };
        if let Some(f) = &mut filter {
            f.finish(&mut |d| out.write(d))?;
        }
        match decoded {
            Ok(n) => out.done(n == size),
            Err(lzma::Failure::Bad) => Err(Out::Bad),
            Err(lzma::Failure::Stop(e)) => Err(e),
        }
    })();
    result.map_err(|e| out.fail(e))
}

/// Decodes a block's packed bytes, handing the output to `out`.
fn decode_to(
    packed: Packed,
    method: &Method,
    size: u64,
    threads: usize,
    out: &mut dyn FnMut(&[u8]) -> std::result::Result<(), Out>,
) -> std::result::Result<u64, lzma::Failure<Out>> {
    // A window no bigger than the output.
    let window = |dict: usize| dict.min(usize::try_from(size).unwrap_or(usize::MAX));
    match (packed, &method.kind) {
        (Packed::Map(p), Kind::Lzma2(dict)) => lzma::decode_lzma2_mt(p, window(*dict), threads, out),
        (Packed::Map(p), Kind::Lzma) => lzma::decode_lzma(p, method.props, size, out),
        (Packed::Map(p), Kind::Copy) => out(p).map(|()| p.len() as u64).map_err(lzma::Failure::Stop),
        (Packed::Read(src, at, len), Kind::Lzma2(dict)) => {
            match lzma::decode_lzma2_mt_read(&|o, buf| src.read_at(at + o, buf), len, threads, out) {
                Some(r) => r,
                // In one part: as a reader.
                None => {
                    let mut r = lzma::Lzma2Reader::new(src.reader(at, len), window(*dict));
                    let mut buf = vec![0u8; 1 << 20];
                    let mut total = 0u64;
                    loop {
                        match r.read(&mut buf) {
                            Ok(0) => break Ok(total),
                            Ok(n) => {
                                total += n as u64;
                                if let Err(e) = out(&buf[..n]) {
                                    break Err(lzma::Failure::Stop(e));
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::InvalidData => break Err(lzma::Failure::Bad),
                            Err(e) => break Err(lzma::Failure::Stop(Out::Stop(e.into()))),
                        }
                    }
                }
            }
        }
        (Packed::Read(..), _) => Err(lzma::Failure::Bad),
    }
}

/// Runs `decode` on a thread of its own, its output going to `write` on this one: a decoder
/// on one core then never waits for the filter, the CRCs or the disk. The output goes across
/// in batches of [`BATCH`] bytes, a few at a time, their buffers coming back for reuse.
fn pipe(
    decode: impl FnOnce(&mut dyn FnMut(&[u8]) -> std::result::Result<(), Out>) -> std::result::Result<u64, lzma::Failure<Out>> + Send,
    write: &mut dyn FnMut(&[u8]) -> std::result::Result<(), Out>,
) -> std::result::Result<u64, lzma::Failure<Out>> {
    const BATCH: usize = 4 << 20;
    let (full, batches) = std::sync::mpsc::sync_channel::<Vec<u8>>(3);
    let (empty, spares) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::scope(|scope| {
        let decoder = scope.spawn(move || {
            let mut batch = Vec::with_capacity(BATCH);
            let result = decode(&mut |data| {
                batch.extend_from_slice(data);
                if batch.len() >= BATCH {
                    let mut next = spares.try_recv().unwrap_or_else(|_| Vec::with_capacity(BATCH));
                    next.clear();
                    // The other side gone: it failed, and its error is the one reported.
                    full.send(std::mem::replace(&mut batch, next)).map_err(|_| Out::Bad)?;
                }
                Ok(())
            });
            if result.is_ok() && !batch.is_empty() {
                let _ = full.send(batch);
            }
            result
        });
        let mut written = Ok(());
        for batch in &batches {
            written = write(&batch);
            if written.is_err() {
                break;
            }
            let _ = empty.send(batch);
        }
        // Stops the decoder, if this side failed.
        drop(batches);
        let decoded = decoder.join().expect("the decoder thread panicked");
        match written {
            Err(e) => Err(lzma::Failure::Stop(e)),
            Ok(()) => decoded,
        }
    })
}

/// Decodes a block from `reader` (see [`coders`]) into its `files`, in order.
fn decode_reader(mut reader: Box<dyn Read + '_>, size: u64, crc: Option<u32>, files: &[&Entry], encrypted: bool, sink: &Sink) -> Result<()> {
    let mut out = BlockOut::new(sink, files, crc, encrypted);
    let result = (|| {
        out.empty_files()?;
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if matches!(e.kind(), io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof) => return Err(Out::Bad),
                Err(e) => return Err(Out::Stop(e.into())),
            };
            total += n as u64;
            out.write(&buf[..n])?;
        }
        out.done(total == size)
    })();
    result.map_err(|e| out.fail(e))
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
    /// The CRC of the whole block, if it has one.
    want_crc: Option<u32>,
    /// Encrypted, with a password given: bad data means a wrong password.
    encrypted: bool,
}

enum Open {
    Stream(FileOut),
    Buffer(Vec<u8>),
    /// Not picked: only checked.
    Skip,
}

impl<'a> BlockOut<'a> {
    fn new(sink: &'a Sink, files: &'a [&'a Entry], crc: Option<u32>, encrypted: bool) -> Self {
        Self {
            sink,
            files,
            next: 0,
            left: 0,
            crc: crc32fast::Hasher::new(),
            open: None,
            block_crc: crc.map(|_| crc32fast::Hasher::new()),
            want_crc: crc,
            encrypted,
        }
    }

    /// The block's output ended (`whole`: with as many bytes as it should have).
    fn done(&mut self, whole: bool) -> std::result::Result<(), Out> {
        if !whole || self.next != self.files.len() {
            return Err(Out::Bad);
        }
        match (self.block_crc.take().map(crc32fast::Hasher::finalize), self.want_crc) {
            (Some(got), Some(crc)) if got != crc => Err(Out::Bad),
            _ => Ok(()),
        }
    }

    /// The error for a block that failed, naming the file it failed in; removes that file's
    /// partly written copy.
    fn fail(&mut self, e: Out) -> Error {
        let name = self.files.get(self.next).or(self.files.last()).map_or("", |f| f.name.as_str());
        if let Some(Open::Stream(f)) = self.open.take() {
            f.discard();
        }
        let e = match e {
            Out::Bad => Error::RawIo(io::Error::new(io::ErrorKind::InvalidData, "checksum mismatch")),
            Out::Stop(e) => e,
        };
        e.for_entry(name, self.encrypted)
    }

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
