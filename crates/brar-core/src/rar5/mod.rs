//! B-Rar's own RAR5 reader: the container (headers) here, the decompressor in [`unpack`].
//!
//! It handles the common case, files in a single-volume, non-solid RAR5 archive without
//! encryption, and marks everything else for the unRAR fallback. The container layout follows
//! RARLAB's published "RAR 5.0 archive format" description.

pub(crate) mod unpack;

use std::{path::Path, time::SystemTime};

use crate::{Result, mmap, time};

const SIGNATURE: &[u8; 8] = b"Rar!\x1a\x07\x01\x00";

/// Header types.
const HEAD_MAIN: u64 = 1;
const HEAD_FILE: u64 = 2;
const HEAD_CRYPT: u64 = 4;
const HEAD_END: u64 = 5;

/// One file or directory, in archive order. `index` counts file headers only, which is how
/// unRAR's header iteration counts them, so the fallback can pick the same entries.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub index: usize,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub data_start: u64,
    pub packed: u64,
    pub crc: u32,
    /// 0 = stored, 1-5 = compressed.
    pub method: u8,
    /// RAR 7 ("v70") stream: larger distance alphabet.
    pub extra_dist: bool,
    pub modified: Option<SystemTime>,
    /// Can B-Rar's own decoder handle this entry?
    pub supported: bool,
}

pub(crate) struct Archive {
    pub entries: Vec<Entry>,
}

/// Parses a RAR5 archive's headers. `None` means "leave the whole archive to unRAR": not RAR5,
/// multi-volume, encrypted headers, or anything that doesn't parse cleanly.
pub(crate) fn parse(path: &Path) -> Result<Option<Archive>> {
    let map = mmap(path)?;
    Ok(parse_bytes(&map))
}

/// One header block: CRC32 (4) | header size (vint) | body (type, flags, [extra size],
/// [data size], type-specific fields, extra area), followed by its data area.
struct Header<'a> {
    kind: u64,
    flags: u64,
    body: &'a [u8],
    /// Offset in `body` of the type-specific fields.
    fields: usize,
    extra: &'a [u8],
    data_start: u64,
    data_size: u64,
    /// Offset of the next header.
    next: usize,
}

/// Reads and CRC-checks the header at `pos`.
fn header_at(b: &[u8], pos: usize) -> Option<Header<'_>> {
    let crc = u32::from_le_bytes(b.get(pos..pos + 4)?.try_into().ok()?);
    let mut p = pos + 4;
    let size = vint(b, &mut p)? as usize;
    let body_end = p.checked_add(size)?;
    let body = b.get(p..body_end)?;
    if crc32fast::hash(b.get(pos + 4..body_end)?) != crc {
        return None;
    }
    let mut q = 0usize;
    let kind = vint(body, &mut q)?;
    let flags = vint(body, &mut q)?;
    let extra_size = if flags & 0x1 != 0 { vint(body, &mut q)? as usize } else { 0 };
    let data_size = if flags & 0x2 != 0 { vint(body, &mut q)? } else { 0 };
    let data_start = body_end as u64;
    Some(Header {
        kind,
        flags,
        body,
        fields: q,
        extra: body.get(body.len().checked_sub(extra_size)?..)?,
        data_start,
        data_size,
        next: usize::try_from(data_start.checked_add(data_size)?).ok()?,
    })
}

fn parse_bytes(b: &[u8]) -> Option<Archive> {
    if b.get(..8)? != SIGNATURE {
        return None;
    }
    let mut pos = 8usize;
    let mut entries = Vec::new();
    loop {
        let h = header_at(b, pos)?;
        match h.kind {
            HEAD_MAIN => {
                let archive_flags = vint(h.body, &mut h.fields.clone())?;
                // Volumes (0x1) and solid archives (0x4) go to unRAR as a whole.
                if archive_flags & (0x1 | 0x4) != 0 {
                    return None;
                }
            }
            HEAD_FILE => {
                // Split across volumes (0x8 / 0x10): not in a single-volume archive.
                if h.flags & (0x8 | 0x10) != 0 {
                    return None;
                }
                let index = entries.len();
                entries.push(file_entry(h.body, h.fields, h.extra, index, h.data_start, h.data_size)?);
            }
            HEAD_CRYPT => return None,
            HEAD_END => break,
            _ => {} // service headers (comments, quick open, ACLs...) and unknown blocks
        }
        pos = h.next;
        if pos >= b.len() {
            break;
        }
    }
    Some(Archive { entries })
}

/// For one RAR5 volume: does its end-of-archive record say another volume follows?
/// `false` when unsure (not RAR5, unreadable, no end record).
pub(crate) fn more_volumes_follow(path: &Path) -> bool {
    let Ok(map) = mmap(path) else { return false };
    if map.get(..8) != Some(&SIGNATURE[..]) {
        return false;
    }
    let mut pos = 8usize;
    while let Some(h) = header_at(&map, pos) {
        if h.kind == HEAD_END {
            // End-of-archive flags: 0x1 = "volume, and not the last one".
            return vint(h.body, &mut h.fields.clone()).is_some_and(|f| f & 0x1 != 0);
        }
        pos = h.next;
    }
    false
}

fn file_entry(body: &[u8], mut q: usize, extra: &[u8], index: usize, data_start: u64, packed: u64) -> Option<Entry> {
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
    let solid = comp & 0x40 != 0;
    let method = ((comp >> 7) & 7) as u8;
    let is_dir = file_flags & 0x1 != 0;
    let size_unknown = file_flags & 0x8 != 0;

    let mut special = false; // encryption, BLAKE2 hash, links: unRAR's job
    let mut r = 0usize;
    while r < extra.len() {
        let rec_size = vint(extra, &mut r)? as usize;
        let rec_end = r.checked_add(rec_size)?;
        let rec = extra.get(r..rec_end)?;
        let mut s = 0usize;
        match vint(rec, &mut s)? {
            0x01 | 0x02 | 0x05 => special = true, // encryption, file hash, redirection
            0x03 => {
                if let Some(t) = high_precision_mtime(rec, s) {
                    modified = Some(t);
                }
            }
            _ => {}
        }
        r = rec_end;
    }

    let supported = !is_dir && !special && !solid && !size_unknown && crc.is_some() && version <= 1 && method <= 5;
    Some(Entry {
        index,
        name,
        is_dir,
        size,
        data_start,
        packed,
        crc: crc.unwrap_or(0),
        method,
        extra_dist: version == 1,
        modified,
        supported,
    })
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

    /// Decoder timing breakdown on the largest entry of $BRAR_BENCH_RAR (run with
    /// `cargo test --release -p brar-core rar5_bench -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn rar5_bench() {
        use std::time::Instant;
        let path = std::env::var("BRAR_BENCH_RAR").expect("set BRAR_BENCH_RAR");
        let arc = parse(Path::new(&path)).unwrap().expect("not a supported RAR5 archive");
        let e = arc.entries.iter().filter(|e| e.supported).max_by_key(|e| e.size).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mut input = vec![0u8; e.packed as usize + unpack::INPUT_PADDING];
        crate::whole::read_at(&file, e.data_start, &mut input[..e.packed as usize]).unwrap();
        println!("{}: {} MB packed -> {} MB", e.name, e.packed >> 20, e.size >> 20);

        let size = e.size as usize;
        let t = Instant::now();
        let mut out = vec![0u8; size + unpack::OUTPUT_SLACK];
        out[..].fill(1);
        println!("allocate + touch output      {:.3} s", t.elapsed().as_secs_f64());
        let mut u = unpack::Unpacker::default();
        for round in 1..=3 {
            let t = Instant::now();
            u.unpack(&input, e.packed as usize, &mut out, size, e.extra_dist).unwrap();
            let s = t.elapsed().as_secs_f64();
            println!("round {round}: decode only           {s:.3} s  {:.0} MB/s", e.size as f64 / 1048576.0 / s);
        }
        let t = Instant::now();
        assert_eq!(crc32fast::hash(&out[..size]), e.crc);
        println!("crc32                        {:.3} s", t.elapsed().as_secs_f64());
    }

    #[test]
    fn rejects_non_rar5() {
        assert!(parse_bytes(b"Rar!\x1a\x07\x00rest-of-a-rar4").is_none());
        assert!(parse_bytes(b"PK\x03\x04").is_none());
    }
}
