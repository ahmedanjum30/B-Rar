//! Tar archives and single compressed files (.gz, .xz, .zst, .bz2, and their .tar.* forms).
//!
//! These are inherently sequential, so the codec runs on its own thread ahead of the tar
//! parser, and small files go to the background writer pool.

use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::Path,
    sync::mpsc::{Receiver, sync_channel},
    thread,
};

use crate::{
    EntryInfo, IoContext, Options, Result,
    format::{Codec, is_tar},
    sink::Sink,
    time,
};

const CHUNK: usize = 1 << 20;

fn decoder(path: &Path, codec: Codec, opts: &Options) -> Result<Box<dyn Read + Send>> {
    let file = File::open(path).at(path)?;
    if codec == Codec::None {
        return Ok(Box::new(file));
    }
    let buffered = || BufReader::with_capacity(CHUNK, file);
    Ok(match codec {
        Codec::None => unreachable!(),
        Codec::Gzip => Box::new(flate2::bufread::MultiGzDecoder::new(buffered())),
        Codec::Bzip2 => Box::new(bzip2::bufread::MultiBzDecoder::new(buffered())),
        Codec::Zstd => Box::new(zstd::stream::read::Decoder::with_buffer(buffered()).at(path)?),
        Codec::Xz => {
            // Multithreaded when the file has several blocks (`xz -T`), otherwise one thread.
            let stream = liblzma::stream::MtStreamBuilder::new()
                .threads(opts.thread_count().min(u32::MAX as usize) as u32)
                .memlimit_threading(u64::MAX)
                .memlimit_stop(u64::MAX)
                .decoder()
                .map_err(io::Error::from)
                .at(path)?;
            Box::new(liblzma::bufread::XzDecoder::new_stream(buffered(), stream))
        }
    })
}

/// Opens the stream and peeks at the first tar header to tell `x.tar.gz` from `x.gz`.
fn open(path: &Path, codec: Codec, opts: &Options) -> Result<(bool, Box<dyn Read + Send>)> {
    let mut reader = read_ahead(decoder(path, codec, opts)?);
    let mut head = Vec::with_capacity(512);
    (&mut reader).take(512).read_to_end(&mut head).at(path)?;
    let tar = codec == Codec::None || is_tar(&head);
    Ok((tar, Box::new(io::Cursor::new(head).chain(reader))))
}

/// Name for a single compressed file: the archive name minus its codec extension.
fn single_name(path: &Path) -> String {
    let stem = path.file_stem().unwrap_or(path.as_os_str()).to_string_lossy();
    match path.extension() {
        Some(_) => stem.into_owned(),
        None => format!("{stem}.out"),
    }
}

pub(crate) fn list(path: &Path, codec: Codec, opts: &Options) -> Result<Vec<EntryInfo>> {
    let (is_tar, mut reader) = open(path, codec, opts)?;
    if !is_tar {
        let size = io::copy(&mut reader, &mut io::sink()).at(path)?;
        return Ok(vec![EntryInfo {
            name: single_name(path),
            is_dir: false,
            size,
            packed: std::fs::metadata(path).ok().map(|m| m.len()),
            modified: None,
        }]);
    }
    let mut archive = tar::Archive::new(reader);
    let mut entries = Vec::new();
    for entry in archive.entries().at(path)? {
        let entry = entry.at(path)?;
        let header = entry.header();
        entries.push(EntryInfo {
            name: String::from_utf8_lossy(&entry.path_bytes()).into_owned(),
            is_dir: header.entry_type().is_dir(),
            size: entry.size(),
            packed: None,
            modified: header.mtime().ok().and_then(time::unix),
        });
    }
    Ok(entries)
}

pub(crate) fn extract(path: &Path, codec: Codec, sink: &Sink, opts: &Options) -> Result<()> {
    let (is_tar, mut reader) = open(path, codec, opts)?;
    if !is_tar {
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        return sink.file(&single_name(path), None, modified, &mut reader);
    }
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().at(path)? {
        let mut entry = entry.at(path)?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let kind = entry.header().entry_type();
        let modified = entry.header().mtime().ok().and_then(time::unix);
        if kind.is_dir() {
            sink.dir(&name)?;
        } else if kind.is_file() || kind.is_contiguous() {
            let size = entry.size();
            sink.file_buffered(&name, size, modified, &mut entry)?;
        } else {
            // Symlinks, hard links and devices are skipped: following them safely is a
            // feature of its own.
            sink.skip();
        }
    }
    Ok(())
}

/// Runs `inner` on its own thread, handing over 1 MiB chunks, so decompression overlaps
/// with parsing and writing.
fn read_ahead(mut inner: Box<dyn Read + Send>) -> ReadAhead {
    let (tx, rx) = sync_channel(16);
    thread::Builder::new()
        .name("brar-inflate".into())
        .spawn(move || {
            loop {
                let mut buf = vec![0; CHUNK];
                let mut filled = 0;
                let mut eof = false;
                while filled < CHUNK {
                    match inner.read(&mut buf[filled..]) {
                        Ok(0) => {
                            eof = true;
                            break;
                        }
                        Ok(n) => filled += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    }
                }
                buf.truncate(filled);
                if (filled > 0 && tx.send(Ok(buf)).is_err()) || eof {
                    return;
                }
            }
        })
        .expect("failed to spawn decoder thread");
    ReadAhead { rx, cur: Vec::new(), pos: 0 }
}

struct ReadAhead {
    rx: Receiver<io::Result<Vec<u8>>>,
    cur: Vec<u8>,
    pos: usize,
}

impl Read for ReadAhead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.cur.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    self.cur = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.cur.len() - self.pos);
        out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
