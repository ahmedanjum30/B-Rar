//! Any 7z block decoded as a chain of readers, one per coder, wired as the block's bind pairs
//! say: for blocks the fast path doesn't take (other methods, filters, BCJ2's four inputs,
//! encryption, split archives).

use std::{
    io::{self, BufRead, BufReader, Read},
    sync::Mutex,
};

use aes::{Aes256, cipher::KeyInit};
use sha2::{Digest, Sha256};

use super::header::Folder;
use crate::{Error, Result, bcj, lzma, rar5::crypt::decrypt_cbc, volumes::Volumes};

/// The archive's bytes: one file, mapped, or the parts of a split archive.
#[derive(Clone, Copy)]
pub(crate) enum Source<'a> {
    Map(&'a [u8]),
    Parts(&'a Volumes),
}

impl<'a> Source<'a> {
    pub(crate) fn len(&self) -> u64 {
        match self {
            Source::Map(m) => m.len() as u64,
            Source::Parts(v) => v.len(),
        }
    }

    pub(crate) fn read_at(&self, at: u64, buf: &mut [u8]) -> io::Result<()> {
        match self {
            Source::Map(m) => {
                let s = usize::try_from(at)
                    .ok()
                    .and_then(|at| m.get(at..at.checked_add(buf.len())?))
                    .ok_or(io::ErrorKind::UnexpectedEof)?;
                buf.copy_from_slice(s);
                Ok(())
            }
            Source::Parts(v) => v.read_at(at, buf),
        }
    }

    /// `len` bytes from `at`, without copying (a mapped file only).
    pub(crate) fn slice(&self, at: u64, len: u64) -> Option<&'a [u8]> {
        match self {
            Source::Map(m) => m.get(usize::try_from(at).ok()?..)?.get(..usize::try_from(len).ok()?),
            Source::Parts(_) => None,
        }
    }

    pub(super) fn reader(self, at: u64, len: u64) -> SourceReader<'a> {
        SourceReader {
            src: self,
            at,
            end: at.saturating_add(len),
        }
    }
}

pub(super) struct SourceReader<'a> {
    src: Source<'a>,
    at: u64,
    end: u64,
}

impl Read for SourceReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(usize::try_from(self.end - self.at).unwrap_or(usize::MAX)).min(1 << 20);
        if n == 0 {
            return Ok(0);
        }
        self.src.read_at(self.at, &mut buf[..n])?;
        self.at += n as u64;
        Ok(n)
    }
}

/// The password, and the keys derived from it so far (7-Zip derives each with 2^19 rounds of
/// SHA-256 by default, and most archives use one for every block).
pub(crate) struct Keys {
    /// The password as UTF-16LE, as 7z hashes it.
    password: Option<Vec<u8>>,
    known: Mutex<Vec<(Vec<u8>, [u8; 32])>>,
}

impl Keys {
    pub(crate) fn new(password: Option<&str>) -> Self {
        Self {
            password: password.map(|p| p.encode_utf16().flat_map(u16::to_le_bytes).collect()),
            known: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn has_password(&self) -> bool {
        self.password.is_some()
    }

    /// The AES key and IV for a 7z AES coder's properties.
    fn key(&self, props: &[u8]) -> Result<([u8; 32], [u8; 16])> {
        let password = self.password.as_deref().ok_or(Error::PasswordRequired)?;
        let bad = || Error::Unsupported("7z encryption settings");
        let b0 = *props.first().ok_or_else(bad)?;
        let cycles = u32::from(b0 & 0x3f);
        let (salt, iv) = if b0 & 0xc0 == 0 {
            (&[][..], &[][..])
        } else {
            let b1 = *props.get(1).ok_or_else(bad)?;
            let salt_len = usize::from((b0 >> 7) & 1) + usize::from(b1 >> 4);
            let iv_len = usize::from((b0 >> 6) & 1) + usize::from(b1 & 0x0f);
            let salt = props.get(2..2 + salt_len).ok_or_else(bad)?;
            let iv = props.get(2 + salt_len..2 + salt_len + iv_len).ok_or_else(bad)?;
            (salt, iv)
        };
        let mut iv16 = [0u8; 16];
        iv16[..iv.len().min(16)].copy_from_slice(&iv[..iv.len().min(16)]);
        // The key depends on the salt and the cycles (the first bytes), not on the IV.
        let id = props[..props.len() - iv.len()].to_vec();
        if let Some((_, k)) = self.known.lock().unwrap().iter().find(|(p, _)| *p == id) {
            return Ok((*k, iv16));
        }
        let mut key = [0u8; 32];
        if cycles == 0x3f {
            // No hashing: the salt and the password, as they are.
            for (k, b) in key.iter_mut().zip(salt.iter().chain(password)) {
                *k = *b;
            }
        } else {
            if cycles > 24 {
                return Err(bad());
            }
            let mut h = Sha256::new();
            for round in 0..1u64 << cycles {
                h.update(salt);
                h.update(password);
                h.update(round.to_le_bytes());
            }
            key.copy_from_slice(&h.finalize());
        }
        self.known.lock().unwrap().push((id, key));
        Ok((key, iv16))
    }
}

/// A block's output as a reader.
pub(crate) fn folder_reader<'a>(src: Source<'a>, packs: &[(u64, u64)], f: &Folder, keys: &Keys) -> Result<Box<dyn Read + 'a>> {
    let main = f.main_out().ok_or_else(damaged)?;
    out_reader(src, packs, f, keys, main, 0)
}

fn damaged() -> Error {
    Error::RawIo(io::Error::new(io::ErrorKind::InvalidData, "damaged 7z block"))
}

/// The reader of the block's out stream `out` (the coder that writes it, fed its inputs).
fn out_reader<'a>(src: Source<'a>, packs: &[(u64, u64)], f: &Folder, keys: &Keys, out: usize, depth: usize) -> Result<Box<dyn Read + 'a>> {
    if depth > f.coders.len() {
        return Err(damaged());
    }
    let (mut first_in, mut first_out) = (0, 0);
    let c = f
        .coders
        .iter()
        .find(|c| {
            if out < first_out + c.outs {
                return true;
            }
            first_in += c.ins;
            first_out += c.outs;
            false
        })
        .ok_or_else(damaged)?;
    if c.outs != 1 {
        return Err(Error::Unsupported("7z coder with several outputs"));
    }
    let mut inputs = Vec::with_capacity(c.ins);
    for i in first_in..first_in + c.ins {
        inputs.push(match f.packed.iter().position(|&p| p == i) {
            Some(k) => {
                let &(at, len) = packs.get(f.first_pack + k).ok_or_else(damaged)?;
                Box::new(src.reader(at, len)) as Box<dyn Read + 'a>
            }
            None => {
                let &(_, from) = f.binds.iter().find(|b| b.0 == i).ok_or_else(damaged)?;
                out_reader(src, packs, f, keys, from, depth + 1)?
            }
        });
    }
    let size = *f.sizes.get(out).ok_or_else(damaged)?;
    Ok(Box::new(coder(&c.id, &c.props, inputs, size, keys)?.take(size)))
}

/// One coder's output, reading from its `inputs`.
fn coder<'a>(id: &[u8], props: &[u8], mut inputs: Vec<Box<dyn Read + 'a>>, size: u64, keys: &Keys) -> Result<Box<dyn Read + 'a>> {
    let bad = |e: io::Error| Error::RawIo(io::Error::new(io::ErrorKind::InvalidData, e));
    if id == [3, 3, 1, 0x1b] {
        let [main, call, jump, rc]: [Box<dyn Read + 'a>; 4] = inputs.try_into().map_err(|_| damaged())?;
        return Ok(Box::new(Bcj2Reader::new(main, call, jump, rc)));
    }
    let [input]: [Box<dyn Read + 'a>; 1] = std::mem::take(&mut inputs).try_into().map_err(|_| damaged())?;
    Ok(match id {
        [0] => input,
        [0x21] => {
            let dict = lzma::lzma2_dict(*props.first().ok_or_else(damaged)?).map_err(|_| damaged())?;
            Box::new(lzma::Lzma2Reader::new(input, dict.min(usize::try_from(size).unwrap_or(usize::MAX))))
        }
        [3, 1, 1] => Box::new(lzma::LzmaReader::new(input, props, size).map_err(bad)?),
        [3, 4, 1] => {
            let (&order, mem) = props.split_first().ok_or_else(damaged)?;
            let mem = u32::from_le_bytes(mem.get(..4).ok_or_else(damaged)?.try_into().unwrap());
            Box::new(ppmd_rust::Ppmd7Decoder::new(input, u32::from(order), mem).map_err(|e| bad(io::Error::other(e.to_string())))?)
        }
        [4, 2, 2] => Box::new(bzip2::read::MultiBzDecoder::new(input)),
        [4, 1, 8] => Box::new(flate2::read::DeflateDecoder::new(input)),
        [4, 1, 9] => Box::new(deflate64::Deflate64Decoder::new(BufReader::new(input))),
        [4, 0xf7, 0x11, 1] => Box::new(zstd::stream::read::Decoder::new(input).map_err(bad)?),
        [4, 0xf7, 0x11, 2] => Box::new(Framed::new(input, Frames::Brotli)),
        [4, 0xf7, 0x11, 4] => Box::new(Framed::new(input, Frames::Lz4)),
        [6, 0xf1, 7, 1] => {
            let (key, iv) = keys.key(props)?;
            Box::new(AesReader {
                inner: input,
                aes: Aes256::new(&key.into()),
                prev: iv,
                buf: Vec::new(),
                at: 0,
            })
        }
        _ => match bcj::Kind::of_7z(id, props) {
            Some(kind) => Box::new(bcj::FilterReader::new(input, kind)),
            None => return Err(Error::Unsupported("7z compression method")),
        },
    })
}

/// AES-256-CBC, 7z's encryption.
struct AesReader<R> {
    inner: R,
    aes: Aes256,
    /// The last ciphertext block decrypted (the IV at first).
    prev: [u8; 16],
    buf: Vec<u8>,
    at: usize,
}

impl<R: Read> Read for AesReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.buf.len() {
            self.buf.resize(64 << 10, 0);
            let mut n = 0;
            while n < self.buf.len() {
                match self.inner.read(&mut self.buf[n..])? {
                    0 => break,
                    k => n += k,
                }
            }
            // Whole blocks only (the stream is a whole number of them).
            let n = n & !15;
            self.buf.truncate(n);
            self.at = 0;
            if n == 0 {
                return Ok(0);
            }
            let last: [u8; 16] = self.buf[n - 16..].try_into().unwrap();
            decrypt_cbc(&self.aes, self.prev, &mut self.buf);
            self.prev = last;
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// BCJ2 (x86 calls and jumps, their targets in streams of their own), after the LZMA SDK's
/// `Bcj2_Decode`: `main` is the code with the targets taken out, `call` and `jump` the
/// targets of E8 and E9/Jcc, big-endian, and `rc` a range-coded stream saying which E8/E9/Jcc
/// were converted.
struct Bcj2Reader<'a> {
    main: BufReader<Box<dyn Read + 'a>>,
    call: BufReader<Box<dyn Read + 'a>>,
    jump: BufReader<Box<dyn Read + 'a>>,
    rc: BufReader<Box<dyn Read + 'a>>,
    probs: [u16; 258],
    range: u32,
    code: u32,
    started: bool,
    prev: u8,
    /// Output position (counting from 0, wrapping).
    pos: u32,
    /// A target's bytes not read yet.
    pending: [u8; 4],
    pending_at: usize,
}

impl<'a> Bcj2Reader<'a> {
    fn new(main: Box<dyn Read + 'a>, call: Box<dyn Read + 'a>, jump: Box<dyn Read + 'a>, rc: Box<dyn Read + 'a>) -> Self {
        Self {
            main: BufReader::with_capacity(1 << 16, main),
            call: BufReader::new(call),
            jump: BufReader::new(jump),
            rc: BufReader::new(rc),
            probs: [1024; 258],
            range: u32::MAX,
            code: 0,
            started: false,
            prev: 0,
            pos: 0,
            pending: [0; 4],
            pending_at: 4,
        }
    }

    fn rc_byte(&mut self) -> io::Result<u8> {
        let mut b = [0];
        self.rc.read_exact(&mut b)?;
        Ok(b[0])
    }
}

impl Read for Bcj2Reader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.started {
            self.started = true;
            for _ in 0..5 {
                self.code = (self.code << 8) | u32::from(self.rc_byte()?);
            }
        }
        let mut n = 0;
        while n < buf.len() {
            if self.pending_at < 4 {
                buf[n] = self.pending[self.pending_at];
                self.pending_at += 1;
                self.pos = self.pos.wrapping_add(1);
                n += 1;
                continue;
            }
            // Copy up to the next E8/E9/Jcc.
            let avail = self.main.fill_buf()?;
            if avail.is_empty() {
                break;
            }
            let room = (buf.len() - n).min(avail.len());
            let mut k = 0;
            let mut jump = false;
            while k < room {
                let b = avail[k];
                buf[n + k] = b;
                k += 1;
                let is_j = b & 0xfe == 0xe8 || (self.prev == 0x0f && b & 0xf0 == 0x80);
                if is_j {
                    jump = true;
                    break;
                }
                self.prev = b;
            }
            self.main.consume(k);
            n += k;
            self.pos = self.pos.wrapping_add(k as u32);
            if !jump {
                continue;
            }
            let b = buf[n - 1];
            let p = if b == 0xe8 {
                usize::from(self.prev)
            } else if b == 0xe9 {
                256
            } else {
                257
            };
            let prob = u32::from(self.probs[p]);
            let bound = (self.range >> 11) * prob;
            if self.code < bound {
                self.range = bound;
                self.probs[p] = (prob + ((2048 - prob) >> 5)) as u16;
                self.prev = b;
            } else {
                self.range -= bound;
                self.code -= bound;
                self.probs[p] = (prob - (prob >> 5)) as u16;
                let mut v = [0u8; 4];
                if b == 0xe8 {
                    self.call.read_exact(&mut v)?
                } else {
                    self.jump.read_exact(&mut v)?
                }
                let dest = u32::from_be_bytes(v).wrapping_sub(self.pos.wrapping_add(4));
                self.pending = dest.to_le_bytes();
                self.pending_at = 0;
                self.prev = (dest >> 24) as u8;
            }
            if self.range < 1 << 24 {
                self.range <<= 8;
                self.code = (self.code << 8) | u32::from(self.rc_byte()?);
            }
        }
        Ok(n)
    }
}

/// The 7-Zip "zstd" fork's framing of LZ4 and Brotli: frames each behind a skippable frame
/// that gives its size (or a plain stream, without one).
#[derive(Clone, Copy, PartialEq)]
enum Frames {
    Lz4,
    Brotli,
}

struct Framed<R> {
    inner: R,
    kind: Frames,
    out: Vec<u8>,
    at: usize,
    done: bool,
    first: bool,
}

impl<R: Read> Framed<R> {
    fn new(inner: R, kind: Frames) -> Self {
        Self {
            inner,
            kind,
            out: Vec::new(),
            at: 0,
            done: false,
            first: true,
        }
    }

    /// Decodes the next frame into `out`; `false` at the end.
    fn next(&mut self) -> io::Result<bool> {
        const SKIPPABLE: u32 = 0x184d_2a50;
        let mut head = [0u8; 4];
        let mut n = 0;
        while n < 4 {
            match self.inner.read(&mut head[n..])? {
                0 => break,
                k => n += k,
            }
        }
        if n == 0 {
            return Ok(false);
        }
        let mut payload = Vec::new();
        if n == 4 && u32::from_le_bytes(head) == SKIPPABLE {
            let mut size = [0u8; 4];
            self.inner.read_exact(&mut size)?;
            let extra = u32::from_le_bytes(size) as usize;
            let mut info = vec![0u8; extra];
            self.inner.read_exact(&mut info)?;
            let len = u32::from_le_bytes(info.get(..4).ok_or(io::ErrorKind::InvalidData)?.try_into().unwrap());
            payload.resize(len as usize, 0);
            self.inner.read_exact(&mut payload)?;
        } else if self.first {
            // A plain stream: all of it.
            payload.extend_from_slice(&head[..n]);
            self.inner.read_to_end(&mut payload)?;
            self.done = true;
        } else {
            // Something after the frames: not ours.
            return Ok(false);
        }
        self.first = false;
        self.out.clear();
        self.at = 0;
        match self.kind {
            Frames::Lz4 => lz4_flex::frame::FrameDecoder::new(&payload[..]).read_to_end(&mut self.out)?,
            Frames::Brotli => brotli::Decompressor::new(&payload[..], 4096).read_to_end(&mut self.out)?,
        };
        Ok(true)
    }
}

impl<R: Read> Read for Framed<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.out.len() {
            if self.done || !self.next()? {
                self.done = true;
                return Ok(0);
            }
        }
        let n = buf.len().min(self.out.len() - self.at);
        buf[..n].copy_from_slice(&self.out[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}
