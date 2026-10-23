//! B-Rar's own RAR5 reader: the container (headers) here, the decompressor in [`unpack`].
//!
//! It reads RAR5 archives in one or several volumes, with or without encryption (of the data,
//! or of the headers too), and marks what it can't decode for the unRAR fallback. The
//! container layout follows RARLAB's published "RAR 5.0 archive format" description.

pub(crate) mod crypt;
pub(crate) mod parallel;
pub(crate) mod unpack;

use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use crate::{IoContext, Result, mmap, time, volumes::RarSet, whole};
use crypt::Keys;

const SIGNATURE: &[u8; 8] = b"Rar!\x1a\x07\x01\x00";

/// Header types.
const HEAD_MAIN: u64 = 1;
const HEAD_FILE: u64 = 2;
const HEAD_CRYPT: u64 = 4;
const HEAD_END: u64 = 5;

/// A file's packed data within one volume. A file split across volumes has one per volume.
#[derive(Clone)]
pub(crate) struct Piece {
    pub volume: usize,
    pub start: u64,
    pub len: u64,
}

/// The checksum a file's data must have: CRC32 or BLAKE2sp, stored plainly or as a MAC
/// (encrypted archives).
#[derive(Clone)]
pub(crate) struct Check {
    sum: Sum,
    mac: Option<Arc<Keys>>,
}

#[derive(Clone, Copy)]
enum Sum {
    Crc(u32),
    Blake2([u8; 32]),
}

/// A checksum being computed, for [`Check::ok`].
pub(crate) enum Hasher {
    Crc(crc32fast::Hasher),
    Blake2(Box<blake2s_simd::blake2sp::State>),
}

impl Hasher {
    pub(crate) fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Crc(h) => h.update(data),
            Hasher::Blake2(h) => {
                h.update(data);
            }
        }
    }
}

impl Check {
    pub(crate) fn hasher(&self) -> Hasher {
        match self.sum {
            Sum::Crc(_) => Hasher::Crc(crc32fast::Hasher::new()),
            Sum::Blake2(_) => Hasher::Blake2(Box::new(blake2s_simd::blake2sp::State::new())),
        }
    }

    pub(crate) fn ok(&self, h: Hasher) -> bool {
        match (self.sum, h) {
            (Sum::Crc(want), Hasher::Crc(h)) => {
                let crc = h.finalize();
                match &self.mac {
                    Some(keys) => keys.mac(crc) == want,
                    None => crc == want,
                }
            }
            (Sum::Blake2(want), Hasher::Blake2(h)) => {
                let digest: [u8; 32] = *h.finalize().as_array();
                match &self.mac {
                    Some(keys) => keys.mac_digest(&digest) == want,
                    None => digest == want,
                }
            }
            _ => false,
        }
    }

    /// Checks data that's all in memory.
    pub(crate) fn matches(&self, data: &[u8]) -> bool {
        let mut h = self.hasher();
        h.update(data);
        self.ok(h)
    }
}

/// One file or directory, in archive order. `index` counts file headers only (a file split
/// across volumes once), which is how unRAR's header iteration counts them, so the fallback
/// can pick the same entries.
#[derive(Clone)]
pub(crate) struct Entry {
    pub index: usize,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub pieces: Vec<Piece>,
    /// Total packed size: all pieces.
    pub packed: u64,
    /// Encrypted: the keys and the IV. The ciphertext is one CBC stream over all pieces (the
    /// split points needn't fall on AES blocks).
    pub crypt: Option<(Arc<Keys>, [u8; 16])>,
    pub check: Check,
    /// 0 = stored, 1-5 = compressed.
    pub method: u8,
    /// RAR 7 ("v70") stream: larger distance alphabet.
    pub extra_dist: bool,
    /// Dictionary size: how far back matches may reach.
    pub dict: u64,
    pub modified: Option<SystemTime>,
    /// Can B-Rar's own decoder handle this entry?
    pub supported: bool,
}

pub(crate) struct Archive {
    pub entries: Vec<Entry>,
    /// One compressed stream across all files.
    pub solid: bool,
    /// The volumes, in order (just the archive itself, usually).
    pub volumes: Vec<PathBuf>,
}

/// The archive's volumes, open for reading packed data with positioned reads.
pub(crate) struct Source {
    files: Vec<File>,
}

impl Source {
    pub(crate) fn open(archive: &Archive) -> Result<Self> {
        let files = archive.volumes.iter().map(|p| File::open(p).at(p)).collect::<Result<_>>()?;
        Ok(Self { files })
    }
}

/// Small reads (block headers) through a memory-mapped section of a volume that moves along
/// with them: a syscall per header costs ~3-10 us, a page touched in a mapping much less. Each
/// section is unmapped when the reader moves past it, on the reading thread: mapping the
/// whole archive left ~20 ms of unmapping at the very end.
pub(crate) struct Views<'a> {
    src: &'a Source,
    /// Volume, offset of the section, the mapping.
    cur: Option<(usize, u64, memmap2::Mmap)>,
}

/// Section size: a multiple of the 64 KB mapping granularity.
const VIEW: u64 = 64 << 20;

impl<'a> Views<'a> {
    pub(crate) fn new(src: &'a Source) -> Self {
        Self { src, cur: None }
    }

    /// `len` bytes at `off` in `volume`, or `None` if they can't be mapped (then read them).
    fn get(&mut self, volume: usize, off: u64, len: usize) -> Option<&[u8]> {
        let end = off.checked_add(len as u64)?;
        let fits = matches!(&self.cur, Some((v, base, m)) if *v == volume && off >= *base && end <= base + m.len() as u64);
        if !fits {
            self.cur = None;
            let file = self.src.files.get(volume)?;
            let base = off & !0xffff;
            let n = VIEW.min(file.metadata().ok()?.len().checked_sub(base)?);
            if end > base + n {
                return None;
            }
            // SAFETY: read-only, like the whole-archive maps elsewhere.
            let map = unsafe { memmap2::MmapOptions::new().offset(base).len(n as usize).map(file) }.ok()?;
            self.cur = Some((volume, base, map));
        }
        let (_, base, map) = self.cur.as_ref()?;
        let at = (off - base) as usize;
        map.get(at..at + len)
    }
}

impl Entry {
    /// [`Entry::read`] for a few bytes (block headers), through `views` where it can.
    pub(crate) fn read_small(&self, views: &mut Views, src: &Source, from: u64, to: u64, out: &mut Vec<u8>) -> unpack::Res<()> {
        let Some((keys, iv)) = &self.crypt else {
            match self.peek(views, from, to) {
                Some(bytes) => out.extend_from_slice(bytes),
                None => self.read_raw(src, from, to, out)?,
            }
            return Ok(());
        };
        // Whole AES blocks plus the ciphertext block before them (or the IV), decrypted here.
        let (b0, b1) = (from / 16 * 16, to.div_ceil(16) * 16);
        let Some(raw) = self.peek(views, b0.saturating_sub(16), b1) else {
            return self.read(src, from, to, out);
        };
        let (prev, data) = if b0 == 0 {
            (*iv, raw)
        } else {
            (raw[..16].try_into().unwrap(), &raw[16..])
        };
        let start = out.len();
        out.extend_from_slice(data);
        keys.decrypt(prev, &mut out[start..]);
        let skip = (from - b0) as usize;
        out.copy_within(start + skip..start + skip + (to - from) as usize, start);
        out.truncate(start + (to - from) as usize);
        Ok(())
    }

    /// Raw bytes `from..to` of the packed data straight from a mapped section, when they're
    /// within one piece.
    fn peek<'v>(&self, views: &'v mut Views, from: u64, to: u64) -> Option<&'v [u8]> {
        if to > self.packed {
            return None;
        }
        let mut at = 0;
        for p in &self.pieces {
            if from >= at && to <= at + p.len {
                return views.get(p.volume, p.start + from - at, (to - from) as usize);
            }
            at += p.len;
        }
        None
    }

    /// Appends bytes `from..to` of this file's packed data (decrypted, across volumes) to
    /// `out`.
    pub(crate) fn read(&self, src: &Source, from: u64, to: u64, out: &mut Vec<u8>) -> unpack::Res<()> {
        let Some((keys, iv)) = &self.crypt else {
            return self.read_raw(src, from, to, out);
        };
        // Whole 16-byte blocks, plus the ciphertext block before them.
        let (b0, b1) = (from / 16 * 16, to.div_ceil(16) * 16);
        let mut prev = *iv;
        if b0 > 0 {
            let mut p = Vec::with_capacity(16);
            self.read_raw(src, b0 - 16, b0, &mut p)?;
            prev.copy_from_slice(&p);
        }
        let start = out.len();
        self.read_raw(src, b0, b1, out)?;
        keys.decrypt(prev, &mut out[start..]);
        let (skip, n) = ((from - b0) as usize, (to - from) as usize);
        out.copy_within(start + skip..start + skip + n, start);
        out.truncate(start + n);
        Ok(())
    }

    /// Like [`Entry::read`], without decrypting.
    fn read_raw(&self, src: &Source, from: u64, to: u64, out: &mut Vec<u8>) -> unpack::Res<()> {
        if to > self.packed {
            return Err(unpack::Corrupt);
        }
        let mut at = 0u64;
        for p in &self.pieces {
            let (lo, hi) = (from.max(at) - at, to.min(at + p.len).saturating_sub(at));
            at += p.len;
            if lo < hi {
                let file = src.files.get(p.volume).ok_or(unpack::Corrupt)?;
                let start = out.len();
                out.resize(start + (hi - lo) as usize, 0);
                whole::read_at(file, p.start + lo, &mut out[start..]).map_err(|_| unpack::Corrupt)?;
            }
        }
        Ok(())
    }
}

/// Parses a RAR5 archive's headers, following its volumes from `path` (the first). `None`
/// means "leave the whole archive to unRAR": not RAR5, a volume missing, encrypted headers
/// without the right password, or anything that doesn't parse cleanly. Entries B-Rar can't
/// decode (no or wrong password among them: unRAR reports those) aren't `supported`.
pub(crate) fn parse(path: &Path, password: Option<&str>) -> Result<Option<Archive>> {
    let mut p = Parser {
        password,
        keys: HashMap::new(),
        entries: Vec::new(),
        open: None,
        solid: false,
    };
    let set = RarSet::of(path);
    let mut volumes = vec![path.to_path_buf()];
    loop {
        let map = mmap(volumes.last().unwrap())?;
        let Some(more) = p.volume(&map, volumes.len() - 1) else {
            return Ok(None);
        };
        if !more {
            break;
        }
        let Some(set) = &set else { return Ok(None) };
        let next = set.part(volumes.len() + 1);
        if !next.is_file() {
            return Ok(None); // unRAR names the missing volume
        }
        volumes.push(next);
    }
    if p.open.is_some() {
        return Ok(None); // the last file continues in a volume that isn't there
    }
    Ok(Some(Archive {
        entries: p.entries,
        solid: p.solid,
        volumes,
    }))
}

/// One header block: CRC32 (4) | header size (vint) | body (type, flags, [extra size],
/// [data size], type-specific fields, extra area), followed by its data area. With encrypted
/// headers, a 16-byte IV comes first and the block is padded to 16 bytes.
struct Header {
    kind: u64,
    flags: u64,
    body: Vec<u8>,
    /// Offset in `body` of the type-specific fields, and of the extra area.
    fields: usize,
    extra_at: usize,
    data_start: u64,
    data_size: u64,
    /// Offset of the next header.
    next: usize,
}

impl Header {
    fn extra(&self) -> &[u8] {
        &self.body[self.extra_at..]
    }
}

/// Reads (decrypting with `keys`) and CRC-checks the header at `pos`.
fn header_at(b: &[u8], pos: usize, keys: Option<&Keys>) -> Option<Header> {
    let (body, body_end) = match keys {
        None => {
            let crc = u32::from_le_bytes(b.get(pos..pos + 4)?.try_into().ok()?);
            let mut p = pos + 4;
            let size = vint(b, &mut p)? as usize;
            let body_end = p.checked_add(size)?;
            if crc32fast::hash(b.get(pos + 4..body_end)?) != crc {
                return None;
            }
            (b[p..body_end].to_vec(), body_end)
        }
        Some(keys) => {
            let iv: [u8; 16] = b.get(pos..pos + 16)?.try_into().ok()?;
            let mut first = b.get(pos + 16..pos + 32)?.to_vec();
            keys.decrypt(iv, &mut first);
            let mut p = 4;
            let size = vint(&first, &mut p)? as usize;
            let total = p.checked_add(size)?;
            let mut h = b.get(pos + 16..(pos + 16).checked_add(total.div_ceil(16) * 16)?)?.to_vec();
            keys.decrypt(iv, &mut h);
            if crc32fast::hash(&h[4..total]) != u32::from_le_bytes(h[..4].try_into().ok()?) {
                return None;
            }
            let body_end = pos + 16 + h.len();
            (h[p..total].to_vec(), body_end)
        }
    };
    let mut q = 0usize;
    let kind = vint(&body, &mut q)?;
    let flags = vint(&body, &mut q)?;
    let extra_size = if flags & 0x1 != 0 { vint(&body, &mut q)? as usize } else { 0 };
    let data_size = if flags & 0x2 != 0 { vint(&body, &mut q)? } else { 0 };
    let data_start = body_end as u64;
    Some(Header {
        kind,
        flags,
        extra_at: body.len().checked_sub(extra_size)?,
        body,
        fields: q,
        data_start,
        data_size,
        next: usize::try_from(data_start.checked_add(data_size)?).ok()?,
    })
}

struct Parser<'a> {
    password: Option<&'a str>,
    /// Derived keys by (salt, iteration count): deriving takes milliseconds, and archives
    /// usually use one salt throughout.
    keys: HashMap<([u8; 16], u8), Arc<Keys>>,
    entries: Vec<Entry>,
    /// A file continued in the next volume.
    open: Option<Entry>,
    solid: bool,
}

impl Parser<'_> {
    /// The keys for this salt and count, if there's a password.
    fn keys(&mut self, salt: [u8; 16], lg2_count: u8) -> Option<Arc<Keys>> {
        let password = self.password?;
        if lg2_count > crypt::MAX_LG2_COUNT {
            return None;
        }
        Some(Arc::clone(
            self.keys
                .entry((salt, lg2_count))
                .or_insert_with(|| Arc::new(Keys::derive(password, &salt, lg2_count))),
        ))
    }

    /// Parses one volume; `Some(true)` when another volume follows.
    fn volume(&mut self, b: &[u8], volume: usize) -> Option<bool> {
        if b.get(..8)? != SIGNATURE {
            return None;
        }
        let mut pos = 8usize;
        let mut header_keys: Option<Arc<Keys>> = None;
        loop {
            let h = header_at(b, pos, header_keys.as_deref())?;
            match h.kind {
                HEAD_CRYPT => {
                    // Everything after it is encrypted: version | flags | count | salt | check.
                    let mut q = h.fields;
                    let (version, flags) = (vint(&h.body, &mut q)?, vint(&h.body, &mut q)?);
                    let lg2_count = *h.body.get(q)?;
                    let salt: [u8; 16] = h.body.get(q + 1..q + 17)?.try_into().ok()?;
                    let keys = self.keys(salt, lg2_count).filter(|_| version == 0)?;
                    if flags & 0x1 != 0 && h.body.get(q + 17..q + 25)? != keys.check {
                        return None; // wrong password
                    }
                    header_keys = Some(keys);
                }
                HEAD_MAIN => {
                    let archive_flags = vint(&h.body, &mut h.fields.clone())?;
                    self.solid = archive_flags & 0x4 != 0;
                }
                HEAD_FILE => self.file(&h, volume)?,
                // End-of-archive flags: 0x1 = "volume, and not the last one".
                HEAD_END => return Some(vint(&h.body, &mut h.fields.clone())? & 0x1 != 0),
                _ => {} // service headers (comments, quick open, ACLs...) and unknown blocks
            }
            pos = h.next;
            if pos >= b.len() {
                return Some(false);
            }
        }
    }

    /// A file header: a new entry, or the next piece of one split across volumes (0x8 =
    /// continued from the previous volume, 0x10 = continues in the next).
    fn file(&mut self, h: &Header, volume: usize) -> Option<()> {
        let (split_before, split_after) = (h.flags & 0x8 != 0, h.flags & 0x10 != 0);
        let (part, has_check) = self.file_entry(h, volume)?;
        let mut e = match self.open.take() {
            Some(mut e) if split_before => {
                if e.name != part.name {
                    return None;
                }
                e.packed += part.packed;
                e.pieces.extend(part.pieces);
                // Same keys throughout; the CBC chain carries on from the first piece's IV.
                if e.crypt.is_some() != part.crypt.is_some() {
                    return None;
                }
                e.supported &= part.supported;
                // The file's checksum is in its last piece's header.
                e.check = part.check;
                e
            }
            None if !split_before => {
                let mut e = part;
                e.index = self.entries.len();
                e
            }
            _ => return None,
        };
        if split_after {
            self.open = Some(e);
        } else {
            e.supported &= has_check;
            self.entries.push(e);
        }
        Some(())
    }

    /// The entry for one file header (one piece, if split), and whether it has a checksum.
    fn file_entry(&mut self, h: &Header, volume: usize) -> Option<(Entry, bool)> {
        let (body, mut q, extra) = (&h.body[..], h.fields, h.extra());
        let file_flags = vint(body, &mut q)?;
        let size = vint(body, &mut q)?;
        let _attributes = vint(body, &mut q)?;
        let mut modified = None;
        if file_flags & 0x2 != 0 {
            modified = time::unix(u32::from_le_bytes(body.get(q..q + 4)?.try_into().ok()?).into());
            q += 4;
        }
        let crc = if file_flags & 0x4 != 0 {
            let c = u32::from_le_bytes(body.get(q..q + 4)?.try_into().ok()?);
            q += 4;
            Some(c)
        } else {
            None
        };
        let comp = vint(body, &mut q)?;
        let _host_os = vint(body, &mut q)?;
        let name_len = vint(body, &mut q)? as usize;
        let name = String::from_utf8_lossy(body.get(q..q.checked_add(name_len)?)?).into_owned();

        let version = comp & 0x3f;
        let method = ((comp >> 7) & 7) as u8;
        let is_dir = file_flags & 0x1 != 0;
        let size_unknown = file_flags & 0x8 != 0;

        let mut special = false; // links, encryption B-Rar can't open: unRAR's job
        let mut blake2 = None;
        let mut crypt = None;
        let mut mac = None;
        let mut r = 0usize;
        while r < extra.len() {
            let rec_size = vint(extra, &mut r)? as usize;
            let rec_end = r.checked_add(rec_size)?;
            let rec = extra.get(r..rec_end)?;
            let mut s = 0usize;
            match vint(rec, &mut s)? {
                0x01 => {
                    // Encryption: version | flags | count | salt | IV | [check value].
                    let (version, flags) = (vint(rec, &mut s)?, vint(rec, &mut s)?);
                    let lg2_count = *rec.get(s)?;
                    let salt: [u8; 16] = rec.get(s + 1..s + 17)?.try_into().ok()?;
                    let iv: [u8; 16] = rec.get(s + 17..s + 33)?.try_into().ok()?;
                    // No password, a wrong one, an unknown version: unRAR reports it.
                    match self.keys(salt, lg2_count).filter(|_| version == 0) {
                        Some(keys) if flags & 0x1 == 0 || rec.get(s + 33..s + 41)? == keys.check => {
                            // 0x2: the CRC is stored as a MAC.
                            if flags & 0x2 != 0 {
                                mac = Some(Arc::clone(&keys));
                            }
                            crypt = Some((keys, iv));
                        }
                        _ => special = true,
                    }
                }
                0x02 => {
                    // File hash: type 0 = BLAKE2sp, 32 bytes.
                    match vint(rec, &mut s)? {
                        0 => blake2 = Some(<[u8; 32]>::try_from(rec.get(s..s + 32)?).ok()?),
                        _ => special = true,
                    }
                }
                0x05 => special = true, // redirection (links)
                0x03 => {
                    if let Some(t) = high_precision_mtime(rec, s) {
                        modified = Some(t);
                    }
                }
                _ => {}
            }
            r = rec_end;
        }

        // 128 KB << n; RAR7 allows n up to 31 and adds a fraction in 32nds.
        let n = (comp >> 10) & if version == 0 { 0x0f } else { 0x1f };
        let mut dict = 0x20000u64.checked_shl(n as u32).unwrap_or(u64::MAX);
        if version == 1 {
            dict = dict.saturating_add(dict / 32 * ((comp >> 15) & 0x1f));
        }
        let supported = !is_dir && !special && !size_unknown && version <= 1 && method <= 5;
        let entry = Entry {
            index: 0,
            name,
            is_dir,
            size,
            pieces: vec![Piece {
                volume,
                start: h.data_start,
                len: h.data_size,
            }],
            packed: h.data_size,
            crypt,
            // CRC32 when there is one: cheaper to compute.
            check: Check {
                sum: match (crc, blake2) {
                    (Some(c), _) => Sum::Crc(c),
                    (None, Some(d)) => Sum::Blake2(d),
                    (None, None) => Sum::Crc(0),
                },
                mac,
            },
            method,
            extra_dist: version == 1,
            dict,
            modified,
            supported,
        };
        Some((entry, crc.is_some() || blake2.is_some()))
    }
}

/// For one RAR5 volume: does its end-of-archive record say another volume follows?
/// `false` when unsure (not RAR5, unreadable, encrypted headers, no end record).
pub(crate) fn more_volumes_follow(path: &Path) -> bool {
    let Ok(map) = mmap(path) else { return false };
    if map.get(..8) != Some(&SIGNATURE[..]) {
        return false;
    }
    let mut pos = 8usize;
    while let Some(h) = header_at(&map, pos, None) {
        if h.kind == HEAD_END {
            return vint(&h.body, &mut h.fields.clone()).is_some_and(|f| f & 0x1 != 0);
        }
        pos = h.next;
    }
    false
}

/// The "file time" extra record: Windows FILETIME, or Unix seconds plus optional nanoseconds.
fn high_precision_mtime(rec: &[u8], mut s: usize) -> Option<SystemTime> {
    let flags = vint(rec, &mut s)?;
    let unix = flags & 0x1 != 0;
    if flags & 0x2 == 0 {
        return None;
    }
    if !unix {
        return time::filetime(u64::from_le_bytes(rec.get(s..s + 8)?.try_into().ok()?));
    }
    let secs = u32::from_le_bytes(rec.get(s..s + 4)?.try_into().ok()?);
    let mut t = time::unix(secs.into())?;
    if flags & 0x10 != 0 {
        // Nanoseconds follow all the second fields, in the same order (mtime first).
        let times = [0x2, 0x4, 0x8].iter().filter(|&&f| flags & f != 0).count();
        let ns_at = s + 4 * times;
        let ns = u32::from_le_bytes(rec.get(ns_at..ns_at + 4)?.try_into().ok()?);
        t += std::time::Duration::from_nanos(ns.min(999_999_999).into());
    }
    Some(t)
}

/// RAR5 variable-length integer: 7 bits per byte, low bits first, high bit = more.
fn vint(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let byte = *b.get(*p)?;
        *p += 1;
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vints() {
        let mut p = 0;
        assert_eq!(vint(&[0x05], &mut p), Some(5));
        p = 0;
        assert_eq!(vint(&[0x80 | 0x2c, 0x02], &mut p), Some(300));
        assert_eq!(p, 2);
        p = 0;
        assert_eq!(vint(&[0x80], &mut p), None);
    }

    /// Decode-only cost of B-Rar's RAR5 decoder: every supported entry of each archive in
    /// $BRAR_BENCH_RAR (separated by `;`), best of 3 rounds, in CPU cycles per byte (the
    /// thread's own cycle count, so clock speed and turbo don't matter). Run with
    /// `cargo test --release -p brar-core --lib rar5_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn rar5_bench() {
        use std::time::Instant;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> isize;
            fn QueryThreadCycleTime(thread: isize, cycles: *mut u64) -> i32;
        }
        let cycles = || {
            let mut c = 0u64;
            // SAFETY: the pseudo-handle of the current thread and a valid out pointer.
            unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
            c
        };
        let paths = std::env::var("BRAR_BENCH_RAR").expect("set BRAR_BENCH_RAR");
        println!(
            "{:<34} {:>9} {:>9} {:>7} {:>9} {:>9} {:>6}",
            "archive", "packed MB", "out MB", "MB/s", "cyc/out B", "cyc/in B", "GHz"
        );
        for path in paths.split(';').filter(|p| !p.is_empty()) {
            let arc = parse(Path::new(path), None).unwrap().expect("not a supported RAR5 archive");
            let src = Source::open(&arc).unwrap();
            let entries: Vec<_> = arc.entries.iter().filter(|e| e.supported).collect();
            let mut u = unpack::Unpacker::default();
            let (mut packed, mut size, mut best_s, mut best_c) = (0u64, 0u64, 0f64, 0u64);
            for e in &entries {
                let (p, n) = (e.packed as usize, e.size as usize);
                let mut input = Vec::new();
                e.read(&src, 0, e.packed, &mut input).unwrap();
                input.resize(p + unpack::INPUT_PADDING, 0);
                let mut out = vec![1u8; n + unpack::OUTPUT_SLACK];
                let (mut s_min, mut c_min) = (f64::MAX, u64::MAX);
                for _ in 0..7 {
                    let (t, c0) = (Instant::now(), cycles());
                    if e.method == 0 {
                        out[..n].copy_from_slice(&input[..n]);
                    } else {
                        u.unpack(&input, p, &mut out, n, e.extra_dist).unwrap();
                    }
                    c_min = c_min.min(cycles() - c0);
                    s_min = s_min.min(t.elapsed().as_secs_f64());
                }
                assert!(e.check.matches(&out[..n]), "{}", e.name);
                (packed, size, best_s, best_c) = (packed + e.packed, size + e.size, best_s + s_min, best_c + c_min);
            }
            let name = Path::new(path).file_name().unwrap().to_string_lossy();
            println!(
                "{:<34} {:>9.0} {:>9.0} {:>7.0} {:>9.2} {:>9.2} {:>6.2}",
                format!("{name} ({} files)", entries.len()),
                packed as f64 / 1048576.0,
                size as f64 / 1048576.0,
                size as f64 / 1048576.0 / best_s,
                best_c as f64 / size as f64,
                best_c as f64 / packed as f64,
                best_c as f64 / best_s / 1e9
            );
        }
    }
    /// Where B-Rar's RAR5 decoder spends its time: a sampling profiler. A second thread pauses
    /// the decoding thread about 2,000 times a second and records its instruction pointer,
    /// while every supported entry of $BRAR_PROFILE_RAR is decoded for $BRAR_PROFILE_SECS
    /// seconds (default 8). Writes "address count" lines (module-relative, hottest first) to
    /// $BRAR_PROFILE_OUT; map them to source lines with addr2line on the test binary, built
    /// with line tables (CARGO_PROFILE_RELEASE_DEBUG=line-tables-only, ..._STRIP=none).
    #[test]
    #[ignore]
    fn rar5_profile() {
        use std::{
            collections::HashMap,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            time::{Duration, Instant},
        };
        #[repr(C, align(16))]
        struct Context([u8; 1232]);
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadId() -> u32;
            fn OpenThread(access: u32, inherit: i32, id: u32) -> isize;
            fn SuspendThread(thread: isize) -> u32;
            fn ResumeThread(thread: isize) -> u32;
            fn GetThreadContext(thread: isize, context: *mut Context) -> i32;
            fn GetModuleHandleW(name: *const u16) -> isize;
        }
        const CONTEXT_CONTROL: u32 = 0x0010_0001;
        const THREAD_ALL: u32 = 0x001F_FFFF;
        let path = std::env::var("BRAR_PROFILE_RAR").expect("set BRAR_PROFILE_RAR");
        let out_path = std::env::var("BRAR_PROFILE_OUT").expect("set BRAR_PROFILE_OUT");
        let secs: u64 = std::env::var("BRAR_PROFILE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);

        let arc = parse(Path::new(&path), None).unwrap().expect("not a supported RAR5 archive");
        let src = Source::open(&arc).unwrap();
        let jobs: Vec<_> = arc
            .entries
            .iter()
            .filter(|e| e.supported && e.method != 0)
            .map(|e| {
                let mut input = Vec::new();
                e.read(&src, 0, e.packed, &mut input).unwrap();
                input.resize(e.packed as usize + unpack::INPUT_PADDING, 0);
                (e, input)
            })
            .collect();

        // SAFETY: plain Win32 calls; the handle refers to this (live) thread.
        let me = unsafe { OpenThread(THREAD_ALL, 0, GetCurrentThreadId()) };
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as u64;
        let done = Arc::new(AtomicBool::new(false));
        let sampler = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let mut hits: HashMap<u64, u64> = HashMap::new();
                let mut ctx = Box::new(Context([0; 1232]));
                while !done.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    while t.elapsed() < Duration::from_micros(500) {
                        std::hint::spin_loop();
                    }
                    // ContextFlags at offset 0x30, Rip at 0xF8 (x64 CONTEXT).
                    ctx.0[0x30..0x34].copy_from_slice(&CONTEXT_CONTROL.to_le_bytes());
                    // SAFETY: the thread is suspended while its context is read; ctx is a
                    // correctly aligned, correctly sized CONTEXT.
                    let rip = unsafe {
                        SuspendThread(me);
                        let ok = GetThreadContext(me, &mut *ctx);
                        ResumeThread(me);
                        if ok == 0 {
                            continue;
                        } else {
                            u64::from_le_bytes(ctx.0[0xF8..0x100].try_into().unwrap())
                        }
                    };
                    *hits.entry(rip.wrapping_sub(base)).or_default() += 1;
                }
                hits
            })
        };
        // BRAR_PROFILE_ENGINE=1: the two-phase engine instead (all files of a solid archive, or
        // the biggest file), sampling this thread, which runs its serial phase B.
        struct Null;
        impl parallel::Output for Null {
            fn start(&mut self, _: usize) -> Result<()> {
                Ok(())
            }
            fn write(&mut self, _: usize, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn finish(&mut self, _: usize) -> Result<()> {
                Ok(())
            }
        }
        let engine: Vec<&Entry> = if arc.solid {
            arc.entries.iter().filter(|e| !e.is_dir).collect()
        } else {
            arc.entries.iter().filter(|e| e.method != 0).max_by_key(|e| e.size).into_iter().collect()
        };
        let use_engine = std::env::var_os("BRAR_PROFILE_ENGINE").is_some();
        let window = engine.iter().map(|e| e.dict).max().unwrap_or(0).min(engine.iter().map(|e| e.size).sum()) as usize;
        let mut u = unpack::Unpacker::default();
        let start = Instant::now();
        let mut rounds = 0;
        while start.elapsed() < Duration::from_secs(secs) {
            if use_engine {
                // BRAR_PROFILE_ENGINE=A: phase A alone, on this thread.
                if std::env::var("BRAR_PROFILE_ENGINE").as_deref() == Ok("A") {
                    assert!(parallel::phase_a_only(&src, &engine) > 0);
                } else {
                    assert!(parallel::decode(&src, &engine, window, 16, &mut Null).is_ok());
                }
                rounds += 1;
                continue;
            }
            for (e, input) in &jobs {
                let n = e.size as usize;
                let mut out = vec![0u8; n + unpack::OUTPUT_SLACK];
                u.unpack(input, e.packed as usize, &mut out, n, e.extra_dist).unwrap();
            }
            rounds += 1;
        }
        done.store(true, Ordering::Relaxed);
        let mut hits: Vec<_> = sampler.join().unwrap().into_iter().collect();
        hits.sort_by_key(|h| std::cmp::Reverse(h.1));
        let total: u64 = hits.iter().map(|h| h.1).sum();
        let text: String = hits.iter().map(|(a, c)| format!("{a:x} {c}\n")).collect();
        std::fs::write(&out_path, text).unwrap();
        println!("{rounds} rounds, {total} samples, {} distinct addresses -> {out_path}", hits.len());
    }
    #[test]
    fn rejects_non_rar5() {
        for bytes in [&b"Rar!\x1a\x07\x00rest-of-a-rar4"[..], b"PK\x03\x04"] {
            let mut p = Parser {
                password: None,
                keys: HashMap::new(),
                entries: Vec::new(),
                open: None,
                solid: false,
            };
            assert!(p.volume(bytes, 0).is_none());
        }
    }
}
