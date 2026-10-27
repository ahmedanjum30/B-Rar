//! B-Rar's reader of 7z headers (after 7-Zip's `7zFormat.txt`): the packed streams, the
//! folders (solid blocks) and their coders, and the files. Headers that are themselves packed
//! (7-Zip's default), and encrypted, are decoded like any block ([`super::coders`]).

use std::{io::Read, time::SystemTime};

use super::coders::{Keys, Source, folder_reader};
use crate::{
    Error,
    deflate::inflate::{Bad, Res},
    lzma, time,
};

const SIGNATURE: [u8; 6] = [b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c];

pub(crate) const COPY: &[u8] = &[0];
pub(crate) const LZMA: &[u8] = &[3, 1, 1];
pub(crate) const LZMA2: &[u8] = &[0x21];
pub(crate) const BCJ_X86: &[u8] = &[3, 3, 1, 3];
pub(crate) const AES: &[u8] = &[6, 0xf1, 7, 1];

/// One coder of a folder.
pub(crate) struct Coder {
    pub(crate) id: Vec<u8>,
    pub(crate) props: Vec<u8>,
    pub(super) ins: usize,
    pub(super) outs: usize,
}

/// A folder (a solid block): its coders, how they connect, and its output.
pub(crate) struct Folder {
    pub(crate) coders: Vec<Coder>,
    /// (in stream, out stream) pairs: the out stream feeds the in stream.
    pub(super) binds: Vec<(usize, usize)>,
    /// The in streams fed by packed streams, in order.
    pub(super) packed: Vec<usize>,
    /// Every coder's out stream sizes, in order.
    pub(super) sizes: Vec<u64>,
    pub(crate) crc: Option<u32>,
    /// Its first packed stream, in [`Archive::packs`].
    pub(super) first_pack: usize,
}

impl Folder {
    /// The out stream no coder takes in: the folder's output.
    pub(super) fn main_out(&self) -> Option<usize> {
        (0..self.sizes.len()).find(|o| !self.binds.iter().any(|b| b.1 == *o))
    }

    /// The size of the folder's output.
    pub(crate) fn size(&self) -> u64 {
        self.main_out().map_or(0, |o| self.sizes[o])
    }

    /// Whether it's encrypted.
    pub(crate) fn encrypted(&self) -> bool {
        self.coders.iter().any(|c| c.id == AES)
    }

    /// The method: one LZMA/LZMA2/copy coder, maybe behind x86 BCJ. `None` for anything else.
    pub(crate) fn method(&self) -> Option<Method<'_>> {
        if self.packed.len() != 1 {
            return None;
        }
        let (codec, bcj) = match &self.coders[..] {
            [c] => (c, false),
            [f, c] | [c, f] if f.id == BCJ_X86 && f.props.is_empty() && f.ins == 1 && f.outs == 1 => (c, true),
            _ => return None,
        };
        if codec.ins != 1 || codec.outs != 1 {
            return None;
        }
        let kind = match &codec.id[..] {
            LZMA2 => Kind::Lzma2(lzma::lzma2_dict(*codec.props.first()?).ok()?),
            LZMA => Kind::Lzma,
            COPY => Kind::Copy,
            _ => return None,
        };
        Some(Method {
            kind,
            props: &codec.props,
            bcj,
        })
    }
}

pub(crate) struct Method<'a> {
    pub(crate) kind: Kind,
    pub(crate) props: &'a [u8],
    pub(crate) bcj: bool,
}

pub(crate) enum Kind {
    /// With its dictionary size.
    Lzma2(usize),
    Lzma,
    Copy,
}

/// A file (or directory) in the archive.
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) crc: Option<u32>,
    pub(crate) is_dir: bool,
    pub(crate) is_anti: bool,
    pub(crate) mtime: Option<SystemTime>,
    /// The folder its data is in; `None` for directories and empty files.
    pub(crate) folder: Option<usize>,
}

pub(crate) struct Archive {
    /// Each packed stream: where it is in the archive, and its size.
    pub(super) packs: Vec<(u64, u64)>,
    pub(crate) folders: Vec<Folder>,
    pub(crate) entries: Vec<Entry>,
}

impl Archive {
    /// Reads the header of the 7z archive in `src` (`keys` for an encrypted one).
    pub(crate) fn read(src: Source, keys: &Keys) -> crate::Result<Self> {
        let damaged = || Error::Unsupported("damaged or unknown 7z header");
        let mut start = [0u8; 32];
        src.read_at(0, &mut start).map_err(|_| damaged())?;
        if start[..6] != SIGNATURE || crc(&start[12..32]) != u32::from_le_bytes(start[8..12].try_into().unwrap()) {
            return Err(damaged());
        }
        let offset = u64::from_le_bytes(start[12..20].try_into().unwrap());
        let size = u64::from_le_bytes(start[20..28].try_into().unwrap());
        let at = offset.checked_add(32).ok_or_else(damaged)?;
        if size > src.len().saturating_sub(at) {
            return Err(damaged());
        }
        let mut data = vec![0u8; size as usize];
        src.read_at(at, &mut data).map_err(|_| damaged())?;
        if crc(&data) != u32::from_le_bytes(start[28..32].try_into().unwrap()) {
            return Err(damaged());
        }
        if data.is_empty() {
            // An empty archive.
            return Ok(Self {
                packs: Vec::new(),
                folders: Vec::new(),
                entries: Vec::new(),
            });
        }
        // A packed header unpacks to the real one (or, in principle, to another packed one).
        for _ in 0..4 {
            let mut r = Reader { b: &data, at: 0 };
            match r.byte().map_err(|_| damaged())? {
                0x01 => return header_body(&mut r).map_err(|_| damaged()),
                0x17 => {
                    let s = streams(&mut r).map_err(|_| damaged())?;
                    let f = s.folders.first().ok_or_else(damaged)?;
                    let encrypted = f.encrypted();
                    let mut unpacked = Vec::new();
                    let read = folder_reader(src, &s.packs, f, keys).and_then(|mut r| Ok(r.read_to_end(&mut unpacked)?));
                    let ok = read.is_ok() && unpacked.len() as u64 == f.size() && f.crc.is_none_or(|c| c == crc(&unpacked));
                    match read {
                        Err(e @ (Error::PasswordRequired | Error::Unsupported(_))) => return Err(e),
                        _ if ok => {}
                        _ if encrypted => return Err(Error::WrongPassword),
                        _ => return Err(damaged()),
                    }
                    data = unpacked;
                }
                _ => return Err(damaged()),
            }
        }
        Err(damaged())
    }
}

fn crc(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Res<u8> {
        let b = *self.b.get(self.at).ok_or(Bad)?;
        self.at += 1;
        Ok(b)
    }

    fn bytes(&mut self, n: u64) -> Res<&'a [u8]> {
        let n = usize::try_from(n).map_err(|_| Bad)?;
        let b = self.b.get(self.at..).and_then(|b| b.get(..n)).ok_or(Bad)?;
        self.at += n;
        Ok(b)
    }

    /// 7z's variable-length number: the leading 1 bits of the first byte count the bytes that
    /// follow (little-endian), its other bits are the top of the value.
    fn number(&mut self) -> Res<u64> {
        let first = self.byte()?;
        let mut value = 0u64;
        let mut mask = 0x80u8;
        for i in 0..8 {
            if first & mask == 0 {
                let high = u64::from(first & mask.wrapping_sub(1));
                return Ok(value | (high << (8 * i)));
            }
            value |= u64::from(self.byte()?) << (8 * i);
            mask >>= 1;
        }
        Ok(value)
    }

    /// A count of things, each at least a byte or a bit of the rest of the header: bounds
    /// the allocations damaged numbers could ask for.
    fn count(&mut self) -> Res<usize> {
        let n = self.number()?;
        if n > (self.b.len() as u64) * 8 + 8 {
            return Err(Bad);
        }
        Ok(n as usize)
    }

    fn u32(&mut self) -> Res<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Res<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    /// `n` bits, high bit of each byte first.
    fn bits(&mut self, n: usize) -> Res<Vec<bool>> {
        let b = self.bytes(n.div_ceil(8) as u64)?;
        Ok((0..n).map(|i| b[i / 8] & (0x80 >> (i % 8)) != 0).collect())
    }

    /// `n` bits, or all set ("all defined").
    fn defined(&mut self, n: usize) -> Res<Vec<bool>> {
        if self.byte()? != 0 { Ok(vec![true; n]) } else { self.bits(n) }
    }

    fn crcs(&mut self, n: usize) -> Res<Vec<Option<u32>>> {
        self.defined(n)?.into_iter().map(|d| d.then(|| self.u32()).transpose()).collect()
    }
}

#[derive(Default)]
struct Streams {
    packs: Vec<(u64, u64)>,
    folders: Vec<Folder>,
    /// Per folder, how many files' data it holds (1 if not given).
    counts: Vec<usize>,
    /// Those files' sizes and CRCs, folder by folder.
    sizes: Vec<u64>,
    crcs: Vec<Option<u32>>,
}

fn streams(r: &mut Reader) -> Res<Streams> {
    let mut s = Streams::default();
    let mut subs = false;
    loop {
        match r.byte()? {
            0 => break,
            0x06 => {
                let pos = r.number()?;
                let n = r.count()?;
                let mut sizes = vec![0; n];
                loop {
                    match r.byte()? {
                        0 => break,
                        0x09 => {
                            for x in &mut sizes {
                                *x = r.number()?;
                            }
                        }
                        0x0a => {
                            r.crcs(n)?;
                        }
                        _ => return Err(Bad),
                    }
                }
                let mut at = pos.checked_add(32).ok_or(Bad)?;
                for size in sizes {
                    s.packs.push((at, size));
                    at = at.checked_add(size).ok_or(Bad)?;
                }
            }
            0x07 => {
                if r.byte()? != 0x0b {
                    return Err(Bad);
                }
                let n = r.count()?;
                if r.byte()? != 0 {
                    return Err(Bad);
                }
                let mut first_pack = 0;
                for _ in 0..n {
                    let f = folder(r, first_pack)?;
                    first_pack += f.packed.len();
                    s.folders.push(f);
                }
                if r.byte()? != 0x0c {
                    return Err(Bad);
                }
                for f in &mut s.folders {
                    for x in &mut f.sizes {
                        *x = r.number()?;
                    }
                }
                loop {
                    match r.byte()? {
                        0 => break,
                        0x0a => {
                            for (f, c) in s.folders.iter_mut().zip(r.crcs(n)?) {
                                f.crc = c;
                            }
                        }
                        _ => return Err(Bad),
                    }
                }
            }
            0x08 => {
                substreams(r, &mut s)?;
                subs = true;
            }
            _ => return Err(Bad),
        }
    }
    if !subs {
        s.counts = vec![1; s.folders.len()];
        s.sizes = s.folders.iter().map(Folder::size).collect();
        s.crcs = s.folders.iter().map(|f| f.crc).collect();
    }
    Ok(s)
}

fn folder(r: &mut Reader, first_pack: usize) -> Res<Folder> {
    let n = r.count()?;
    if n == 0 || n > 64 {
        return Err(Bad);
    }
    let mut coders = Vec::with_capacity(n);
    for _ in 0..n {
        let flags = r.byte()?;
        // Alternative methods (bit 7) were never used.
        if flags & 0x80 != 0 {
            return Err(Bad);
        }
        let id = r.bytes(u64::from(flags & 0x0f))?.to_vec();
        let (ins, outs) = if flags & 0x10 != 0 { (r.count()?, r.count()?) } else { (1, 1) };
        let props = if flags & 0x20 != 0 {
            let n = r.number()?;
            r.bytes(n)?.to_vec()
        } else {
            Vec::new()
        };
        if ins > 64 || outs > 64 {
            return Err(Bad);
        }
        coders.push(Coder { id, props, ins, outs });
    }
    let ins: usize = coders.iter().map(|c| c.ins).sum();
    let outs: usize = coders.iter().map(|c| c.outs).sum();
    let binds = (0..outs.checked_sub(1).ok_or(Bad)?)
        .map(|_| Ok((r.count()?, r.count()?)))
        .collect::<Res<Vec<_>>>()?;
    let n_packed = ins.checked_sub(binds.len()).ok_or(Bad)?;
    let packed = if n_packed == 1 {
        vec![(0..ins).find(|i| !binds.iter().any(|b| b.0 == *i)).ok_or(Bad)?]
    } else {
        (0..n_packed).map(|_| r.count()).collect::<Res<Vec<_>>>()?
    };
    Ok(Folder {
        coders,
        binds,
        packed,
        sizes: vec![0; outs],
        crc: None,
        first_pack,
    })
}

fn substreams(r: &mut Reader, s: &mut Streams) -> Res<()> {
    let n = s.folders.len();
    s.counts = vec![1; n];
    let mut t = r.byte()?;
    if t == 0x0d {
        for c in &mut s.counts {
            *c = r.count()?;
        }
        t = r.byte()?;
    }
    let have_sizes = t == 0x09;
    for (f, &c) in s.folders.iter().zip(&s.counts) {
        if c == 0 {
            continue;
        }
        let mut sum = 0u64;
        if have_sizes {
            for _ in 1..c {
                let x = r.number()?;
                sum = sum.checked_add(x).ok_or(Bad)?;
                s.sizes.push(x);
            }
        } else if c > 1 {
            return Err(Bad);
        }
        s.sizes.push(f.size().checked_sub(sum).ok_or(Bad)?);
    }
    if have_sizes {
        t = r.byte()?;
    }
    // CRCs: given here for every file, except a folder's only file when the folder has one.
    let known = |f: &Folder, c: usize| c == 1 && f.crc.is_some();
    let unknown: usize = s.folders.iter().zip(&s.counts).filter(|(f, c)| !known(f, **c)).map(|(_, c)| *c).sum();
    let mut given = Vec::new();
    loop {
        match t {
            0 => break,
            0x0a => given = r.crcs(unknown)?,
            _ => return Err(Bad),
        }
        t = r.byte()?;
    }
    let mut given = given.into_iter();
    for (f, &c) in s.folders.iter().zip(&s.counts) {
        if known(f, c) {
            s.crcs.push(f.crc);
        } else {
            s.crcs.extend((0..c).map(|_| given.next().flatten()));
        }
    }
    Ok(())
}

fn header_body(r: &mut Reader) -> Res<Archive> {
    let mut id = r.byte()?;
    if id == 0x02 {
        // Archive properties: none that matter.
        loop {
            if r.byte()? == 0 {
                break;
            }
            let n = r.number()?;
            r.bytes(n)?;
        }
        id = r.byte()?;
    }
    if id == 0x03 {
        streams(r)?;
        id = r.byte()?;
    }
    let mut s = Streams::default();
    if id == 0x04 {
        s = streams(r)?;
        id = r.byte()?;
    }
    let mut entries = Vec::new();
    if id == 0x05 {
        entries = files(r, &s)?;
        id = r.byte()?;
    }
    if id != 0 {
        return Err(Bad);
    }
    Ok(Archive {
        packs: s.packs,
        folders: s.folders,
        entries,
    })
}

fn files(r: &mut Reader, s: &Streams) -> Res<Vec<Entry>> {
    let n = r.count()?;
    let mut empty_stream = vec![false; n];
    let (mut empty_file, mut anti) = (Vec::new(), Vec::new());
    let mut names = Vec::new();
    let mut mtimes = vec![None; n];
    loop {
        let t = r.byte()?;
        if t == 0 {
            break;
        }
        let size = r.number()?;
        let mut p = Reader { b: r.bytes(size)?, at: 0 };
        let empties = empty_stream.iter().filter(|e| **e).count();
        match t {
            0x0e => empty_stream = p.bits(n)?,
            0x0f => empty_file = p.bits(empties)?,
            0x10 => anti = p.bits(empties)?,
            0x11 => {
                if p.byte()? != 0 {
                    return Err(Bad);
                }
                let units: Vec<u16> = p.b[1..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
                names = units.split(|&u| u == 0).take(n).map(String::from_utf16_lossy).collect();
            }
            0x14 => {
                let defined = p.defined(n)?;
                if p.byte()? != 0 {
                    return Err(Bad);
                }
                for (m, d) in mtimes.iter_mut().zip(defined) {
                    if d {
                        *m = time::filetime(p.u64()?);
                    }
                }
            }
            // Other times, attributes, padding.
            _ => {}
        }
    }
    if names.len() != n {
        return Err(Bad);
    }
    // Files with data take the folders' files in order.
    let mut folders = s.counts.iter().enumerate().flat_map(|(f, &c)| std::iter::repeat_n(f, c));
    let (mut data, mut empty) = (0usize, 0usize);
    let mut entries = Vec::with_capacity(n);
    for (i, name) in names.into_iter().enumerate() {
        let mut e = Entry {
            name,
            size: 0,
            crc: None,
            is_dir: false,
            is_anti: false,
            mtime: mtimes[i],
            folder: None,
        };
        if empty_stream[i] {
            e.is_dir = !empty_file.get(empty).copied().unwrap_or(false);
            e.is_anti = anti.get(empty).copied().unwrap_or(false);
            empty += 1;
        } else {
            e.folder = Some(folders.next().ok_or(Bad)?);
            e.size = *s.sizes.get(data).ok_or(Bad)?;
            e.crc = s.crcs.get(data).copied().flatten();
            data += 1;
        }
        entries.push(e);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEVEN_ZIP: &str = r"C:\Program Files\7-Zip\7z.exe";

    #[test]
    fn reads_what_sevenz_rust2_reads() {
        if !std::path::Path::new(SEVEN_ZIP).exists() {
            eprintln!("7-Zip not installed: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("brar-7z-header-{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("sub/deeper")).unwrap();
        std::fs::create_dir_all(src.join("empty dir")).unwrap();
        let mut x = 5u64;
        let noise: Vec<u8> = (0..300_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        std::fs::write(src.join("a.txt"), "B-Rar reads 7z headers. ".repeat(4000)).unwrap();
        std::fs::write(src.join("sub/b.bin"), &noise).unwrap();
        std::fs::write(src.join("sub/deeper/c.txt"), "c").unwrap();
        std::fs::write(src.join("empty.txt"), "").unwrap();
        std::fs::write(src.join("ünï 名.txt"), "unicode").unwrap();
        // A program, for the BCJ filter.
        std::fs::copy(std::env::current_exe().unwrap(), src.join("prog.exe")).unwrap();

        let cases: [(&str, &[&str], bool); 14] = [
            ("default", &[], true),
            ("nonsolid", &["-ms=off"], true),
            ("plainheader", &["-mhc=off"], true),
            ("lzma", &["-m0=lzma"], true),
            ("stored", &["-mx0"], true),
            ("ppmd", &["-m0=ppmd"], true),
            ("password", &["-pabc"], true),
            ("encheader", &["-pabc", "-mhe=on"], false),
            ("bzip2", &["-m0=bzip2"], true),
            ("deflate", &["-m0=deflate"], true),
            ("deflate64", &["-m0=deflate64"], true),
            (
                "bcj2",
                &["-m0=BCJ2", "-m1=LZMA", "-m2=LZMA", "-m3=LZMA", "-mb0:1", "-mb0s1:2", "-mb0s2:3"],
                true,
            ),
            ("delta", &["-mf=Delta:4"], true),
            ("arm64", &["-mf=ARM64"], true),
        ];
        for (name, args, readable) in cases {
            let archive = dir.join(format!("{name}.7z"));
            let status = std::process::Command::new(SEVEN_ZIP)
                .arg("a")
                .arg(&archive)
                .args(args)
                .arg("-bso0")
                .arg("*")
                .current_dir(&src)
                .status()
                .unwrap();
            assert!(status.success(), "{name}");
            let map = std::fs::read(&archive).unwrap();
            let password = args.contains(&"-pabc").then_some("abc");
            let src = Source::Map(&map);
            if !readable {
                // An encrypted header: no password, a wrong one.
                assert!(matches!(Archive::read(src, &Keys::new(None)), Err(Error::PasswordRequired)), "{name}");
                assert!(matches!(Archive::read(src, &Keys::new(Some("abd"))), Err(Error::WrongPassword)), "{name}");
            }
            let keys = Keys::new(password);
            let ours = Archive::read(src, &keys).unwrap_or_else(|e| panic!("{name}: not read: {e}"));
            let pw = if args.contains(&"-pabc") {
                sevenz_rust2::Password::from("abc")
            } else {
                sevenz_rust2::Password::empty()
            };
            let theirs = sevenz_rust2::Archive::read(&mut std::io::Cursor::new(&map[..]), &pw).unwrap();
            assert_eq!(ours.entries.len(), theirs.files.len(), "{name}");
            assert_eq!(ours.folders.len(), theirs.blocks.len(), "{name}");
            for (i, (o, t)) in ours.entries.iter().zip(&theirs.files).enumerate() {
                assert_eq!(o.name, t.name, "{name}");
                assert_eq!(o.size, t.size, "{name}: {}", o.name);
                assert_eq!(o.is_dir, t.is_directory, "{name}: {}", o.name);
                assert_eq!(o.is_anti, t.is_anti_item, "{name}: {}", o.name);
                assert_eq!(o.crc, t.has_crc.then_some(t.crc as u32), "{name}: {}", o.name);
                assert_eq!(o.folder, theirs.stream_map.file_block_index[i], "{name}: {}", o.name);
                let mtime = t.has_last_modified_date.then(|| time::filetime(t.last_modified_date.into())).flatten();
                assert_eq!(o.mtime, mtime, "{name}: {}", o.name);
            }
            // Every block decodes, giving every file its CRC.
            for (f, folder) in ours.folders.iter().enumerate() {
                assert_eq!(folder.size(), theirs.blocks[f].get_unpack_size(), "{name}");
                let mut data = Vec::new();
                folder_reader(src, &ours.packs, folder, &keys).unwrap().read_to_end(&mut data).unwrap();
                let mut at = 0;
                for e in ours.entries.iter().filter(|e| e.folder == Some(f)) {
                    assert_eq!(Some(crc(&data[at..at + e.size as usize])), e.crc, "{name}: {}", e.name);
                    at += e.size as usize;
                }
                assert_eq!(at, data.len(), "{name}");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
