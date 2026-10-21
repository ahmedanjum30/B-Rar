//! Archives in several parts.
//!
//! - RAR volumes (`name.part1.rar, name.part2.rar, ...` or the old `name.rar, name.r00, ...`):
//!   we start from the first part and unRAR follows the rest.
//! - Split files (`name.7z.001, name.7z.002, ...`, as 7-Zip's "split to volumes" makes, for
//!   any format): the parts are read as one continuous file, see [`Volumes`].
//!
//! Whichever part the user picked, extraction starts from part 1, like WinRAR and 7-Zip.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{Error, IoContext, Result, whole};

/// What to read for an archive the user pointed at.
pub(crate) struct Input {
    /// The file reading starts from: part 1 of a RAR set, or the archive itself.
    pub first: PathBuf,
    /// Name for messages and for a single decompressed file: `name.gz` for `name.gz.001`.
    pub name: PathBuf,
    /// The parts of a split (`.001`) archive, read as one.
    pub split: Option<Volumes>,
}

impl Input {
    /// A reader over the whole archive (all parts of a split one).
    pub(crate) fn reader(&self) -> Result<Box<dyn Read + Send>> {
        Ok(match &self.split {
            Some(v) => Box::new(v.clone()),
            None => Box::new(File::open(&self.first).at(&self.first)?),
        })
    }

    /// The first `n` bytes (fewer if the archive is shorter), for format detection.
    pub(crate) fn head(&self, n: usize) -> Result<Vec<u8>> {
        let mut head = Vec::with_capacity(n);
        self.reader()?.take(n as u64).read_to_end(&mut head).at(&self.first)?;
        Ok(head)
    }
}

pub(crate) fn resolve(path: &Path) -> Result<Input> {
    if let Some(parts) = split_parts(path)? {
        let name = parts[0].with_extension("");
        return Ok(Input {
            first: parts[0].clone(),
            name,
            split: Some(Volumes::open(&parts)?),
        });
    }
    let first = match RarSet::of(path) {
        Some(set) => {
            let first = set.part(1);
            if !first.is_file() {
                return Err(Error::MissingVolume(first));
            }
            first
        }
        None => path.to_path_buf(),
    };
    Ok(Input {
        name: first.clone(),
        first,
        split: None,
    })
}

/// `name.003` -> `[name.001, name.002, name.003, ...]`, as many as exist in sequence.
/// `None` if the name doesn't end in a part number of 3+ digits.
fn split_parts(path: &Path) -> Result<Option<Vec<PathBuf>>> {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return Ok(None);
    };
    if ext.len() < 3 || !ext.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let width = ext.len();
    let part = |n: usize| path.with_extension(format!("{n:0width$}"));
    let first = part(1);
    if !first.is_file() {
        return Err(Error::MissingVolume(first));
    }
    let parts: Vec<PathBuf> = (1..).map(part).take_while(|p| p.is_file()).collect();
    // Any higher-numbered part beyond the run means the next one is missing (otherwise the
    // archive would just look truncated and fail with a confusing format error).
    let stem = path.with_extension("");
    let prefix = stem.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
    let highest = highest_number(&stem, |name| {
        let rest = name.strip_prefix(prefix.as_str())?.strip_prefix('.')?;
        (rest.len() == width).then(|| digits(rest)).flatten()
    });
    if highest > parts.len() {
        return Err(Error::MissingVolume(part(parts.len() + 1)));
    }
    Ok(Some(parts))
}

fn digits(s: &str) -> Option<usize> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// The highest part number among the files next to `sibling`, as `number` reads it from
/// their names.
fn highest_number(sibling: &Path, number: impl Fn(&str) -> Option<usize>) -> usize {
    let dir = sibling.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries.flatten().filter_map(|e| number(e.file_name().to_str()?)).max().unwrap_or(0)
}

/// A RAR volume set, named the new way (`name.part1.rar`, `name.part2.rar`, ...) or the old
/// way (`name.rar`, `name.r00`, `name.r01`, ...).
pub(crate) struct RarSet {
    dir: PathBuf,
    /// `name` in the patterns above, as on disk.
    stem: String,
    /// Digits in new-style part numbers; `None` for the old naming.
    width: Option<usize>,
}

impl RarSet {
    /// The set `path` belongs to, if its name looks like a volume. A plain `name.rar` only
    /// counts (as old-style part 1) when `name.r00` exists.
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        if let Some(stem) = lower.strip_suffix(".rar") {
            if let Some(at) = stem.rfind(".part") {
                let digits = &stem[at + 5..];
                if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Some(Self {
                        dir,
                        stem: name[..at].to_owned(),
                        width: Some(digits.len()),
                    });
                }
            }
            let set = Self {
                dir,
                stem: name[..name.len() - 4].to_owned(),
                width: None,
            };
            return set.part(2).is_file().then_some(set);
        }
        let (stem, ext) = name.rsplit_once('.')?;
        let e = ext.as_bytes();
        (e.len() == 3 && (e[0] | 0x20) == b'r' && e[1].is_ascii_digit() && e[2].is_ascii_digit()).then(|| Self {
            dir,
            stem: stem.to_owned(),
            width: None,
        })
    }

    /// Path of part `n` (1-based).
    pub(crate) fn part(&self, n: usize) -> PathBuf {
        let name = match self.width {
            Some(w) => format!("{}.part{n:0w$}.rar", self.stem),
            None if n == 1 => format!("{}.rar", self.stem),
            None => format!("{}.r{:02}", self.stem, n - 2),
        };
        self.dir.join(name)
    }

    /// The first part that should exist but doesn't, after a failed extraction: a gap below
    /// the highest part on disk, or the part after the last one when that last part's end
    /// record says more volumes follow. `None` when the set looks complete.
    pub(crate) fn missing(&self) -> Option<PathBuf> {
        let prefix = self.stem.to_ascii_lowercase();
        let highest = highest_number(&self.part(1), |name| {
            let lower = name.to_ascii_lowercase();
            let rest = lower.strip_prefix(prefix.as_str())?;
            match self.width {
                Some(_) => digits(rest.strip_prefix(".part")?.strip_suffix(".rar")?),
                // Old naming: .r00 is part 2.
                None => Some(digits(rest.strip_prefix(".r")?)? + 2),
            }
        });
        let highest = highest.max(1);
        if let Some(gap) = (1..=highest).map(|n| self.part(n)).find(|p| !p.is_file()) {
            return Some(gap);
        }
        crate::rar5::more_volumes_follow(&self.part(highest)).then(|| self.part(highest + 1))
    }
}

/// The parts of a split archive read as one file. Cheap to clone (the files are shared);
/// each clone has its own position, and reads use positioned I/O, so clones work in parallel.
#[derive(Clone)]
pub(crate) struct Volumes {
    /// (file, offset of its first byte in the whole, length)
    parts: Arc<Vec<(File, u64, u64)>>,
    len: u64,
    pos: u64,
}

impl Volumes {
    fn open(paths: &[PathBuf]) -> Result<Self> {
        let mut parts = Vec::with_capacity(paths.len());
        let mut start = 0;
        for p in paths {
            let file = File::open(p).at(p)?;
            let len = file.metadata().at(p)?.len();
            parts.push((file, start, len));
            start += len;
        }
        Ok(Self {
            parts: Arc::new(parts),
            len: start,
            pos: 0,
        })
    }

    /// Index of the part holding byte `offset` (< len).
    fn part_at(&self, offset: u64) -> usize {
        self.parts.partition_point(|&(_, start, _)| start <= offset) - 1
    }

    /// Fills `buf` from `offset`, across part boundaries.
    pub(crate) fn read_at(&self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            if offset >= self.len {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let (file, start, len) = &self.parts[self.part_at(offset)];
            let within = offset - start;
            let n = buf.len().min((len - within) as usize);
            whole::read_at(file, within, &mut buf[..n])?;
            buf = &mut buf[n..];
            offset += n as u64;
        }
        Ok(())
    }
}

impl Read for Volumes {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        // Stay within one part per call: one positioned read.
        let (_, start, len) = self.parts[self.part_at(self.pos)];
        let n = buf.len().min((start + len - self.pos) as usize);
        self.read_at(self.pos, &mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Volumes {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = target.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("brar-volumes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn split_parts_read_as_one() {
        let d = temp_dir("split");
        for (n, data) in [(1, &b"hello "[..]), (2, b"split "), (3, b"world")] {
            std::fs::write(d.join(format!("a.bin.{n:03}")), data).unwrap();
        }
        // Picking part 2 starts from part 1 and sees all three.
        let input = resolve(&d.join("a.bin.002")).unwrap();
        assert_eq!(input.first, d.join("a.bin.001"));
        assert_eq!(input.name, d.join("a.bin"));
        let mut all = String::new();
        input.reader().unwrap().read_to_string(&mut all).unwrap();
        assert_eq!(all, "hello split world");
        let v = input.split.unwrap();
        let mut buf = [0u8; 7];
        v.read_at(4, &mut buf).unwrap();
        assert_eq!(&buf, b"o split");
    }

    #[test]
    fn missing_first_part_is_reported() {
        let d = temp_dir("missing");
        std::fs::write(d.join("a.7z.002"), b"x").unwrap();
        assert!(matches!(resolve(&d.join("a.7z.002")), Err(Error::MissingVolume(p)) if p == d.join("a.7z.001")));
    }

    #[test]
    fn gap_in_split_parts_is_reported() {
        let d = temp_dir("gap");
        for n in [1, 2, 4] {
            std::fs::write(d.join(format!("a.zip.{n:03}")), b"x").unwrap();
        }
        assert!(matches!(resolve(&d.join("a.zip.001")), Err(Error::MissingVolume(p)) if p == d.join("a.zip.003")));
    }

    #[test]
    fn rar_part_one_missing_is_reported() {
        let d = temp_dir("rar1");
        std::fs::write(d.join("x.part2.rar"), b"Rar!").unwrap();
        std::fs::write(d.join("x.part3.rar"), b"Rar!").unwrap();
        assert!(matches!(resolve(&d.join("x.part3.rar")), Err(Error::MissingVolume(p)) if p == d.join("x.part1.rar")));
        // A gap in the middle of a set.
        std::fs::write(d.join("x.part1.rar"), b"Rar!").unwrap();
        std::fs::remove_file(d.join("x.part2.rar")).unwrap();
        assert_eq!(RarSet::of(&d.join("x.part1.rar")).unwrap().missing(), Some(d.join("x.part2.rar")));
    }

    #[test]
    fn rar_volumes_start_at_part_one() {
        let d = temp_dir("rar");
        for f in ["x.part01.rar", "x.part02.rar", "old.rar", "old.r00"] {
            std::fs::write(d.join(f), b"Rar!").unwrap();
        }
        assert_eq!(resolve(&d.join("x.part02.rar")).unwrap().first, d.join("x.part01.rar"));
        assert_eq!(resolve(&d.join("old.r00")).unwrap().first, d.join("old.rar"));
        assert_eq!(resolve(&d.join("x.part01.rar")).unwrap().first, d.join("x.part01.rar"));
    }
}
