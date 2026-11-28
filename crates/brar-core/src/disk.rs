//! Where an archive is read from. On a hard drive, reading several places at once (one per
//! core, which is what makes B-Rar fast on an SSD) keeps the head jumping between them: a
//! drive that reads 150 MB/s in order then manages a few MB/s. So on one, the archive is read
//! in order, one piece at a time ([`Reader`]), while decoding and writing still use every core.

use std::{fs::File, io, path::Path, sync::Mutex};

use crate::whole::read_at;

/// Reads are split into pieces of this size: between pieces a cancel or pause takes effect,
/// and on a hard drive another thread may take its turn (big enough that the drive spends
/// its time reading, not seeking: ~0.1 s of reading per ~0.01 s seek).
pub(crate) const PIECE: usize = 16 << 20;

/// Whether `path` is on a drive that pays for seeking (a hard drive), as Windows reports it.
/// `BRAR_DISK=hdd` or `ssd` overrides the answer (for testing). Unknown means no.
pub(crate) fn seeks_slowly(path: &Path) -> bool {
    match std::env::var("BRAR_DISK").as_deref() {
        Ok("hdd") => return true,
        Ok("ssd") => return false,
        _ => {}
    }
    #[cfg(windows)]
    {
        seek_penalty(path).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// Asks the volume `path` is on whether it incurs a seek penalty (IOCTL_STORAGE_QUERY_PROPERTY,
/// StorageDeviceSeekPenaltyProperty). `None` when the volume can't say (a network drive, a
/// volume over several disks).
#[cfg(windows)]
fn seek_penalty(path: &Path) -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumePathNameW, OPEN_EXISTING},
            System::{
                IO::DeviceIoControl,
                Ioctl::{
                    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY,
                    StorageDeviceSeekPenaltyProperty,
                },
            },
        },
        core::PCWSTR,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut volume = [0u16; 261];
    // SAFETY: a NUL-terminated path and a buffer of the length passed.
    unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut volume) }.ok()?;
    // "E:\" -> "\\.\E:", the volume as a device.
    let root = String::from_utf16_lossy(&volume[..volume.iter().position(|&c| c == 0)?]);
    let letter = root.strip_suffix('\\').filter(|r| r.len() == 2 && r.ends_with(':'))?;
    let device: Vec<u16> = format!(r"\\.\{letter}").encode_utf16().chain(Some(0)).collect();
    // SAFETY: opening the volume for queries only (no read or write access asked for).
    let handle = unsafe {
        CreateFileW(
            PCWSTR(device.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut answer = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned = 0u32;
    // SAFETY: input and output buffers of the sizes passed, on a handle we own.
    let asked = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const _),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(&mut answer as *mut _ as *mut _),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            Some(&mut returned),
            None,
        )
    };
    // SAFETY: closing the handle opened above.
    let _ = unsafe { CloseHandle(handle) };
    asked.ok()?;
    (returned as usize >= size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>()).then_some(answer.IncursSeekPenalty)
}

/// Positioned reads of one archive file, in [`PIECE`]s: `running` is asked before each (it
/// blocks while paused and fails after a cancel), and on a hard drive (`in_order`) one
/// thread reads at a time.
pub(crate) struct Reader<'a> {
    file: &'a File,
    in_order: Option<Mutex<()>>,
    running: &'a (dyn Fn() -> bool + Sync),
}

impl<'a> Reader<'a> {
    pub(crate) fn new(file: &'a File, in_order: bool, running: &'a (dyn Fn() -> bool + Sync)) -> Self {
        Self {
            file,
            in_order: in_order.then(|| Mutex::new(())),
            running,
        }
    }

    /// Whether reads take turns (a hard drive).
    pub(crate) fn in_order(&self) -> bool {
        self.in_order.is_some()
    }

    pub(crate) fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut at = offset;
        for piece in buf.chunks_mut(PIECE) {
            if !(self.running)() {
                return Err(io::Error::other("cancelled"));
            }
            let _turn = self.in_order.as_ref().map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
            read_at(self.file, at, piece)?;
            at += piece.len() as u64;
        }
        Ok(())
    }
}

/// A [`Reader`] as a buffered `Read + Seek` (for the zip crate: its central directory, and
/// the entries B-Rar's own decoders don't take, which it streams). A refill reads 64 KB after
/// a jump, doubling up to [`PIECE`] while reading straight on, so the directory's many small
/// reads stay small and a streamed entry is read in big pieces. Clones start empty.
pub(crate) struct Buffered<'a> {
    reader: &'a Reader<'a>,
    len: u64,
    pos: u64,
    buf: Vec<u8>,
    /// Where `buf` starts in the file.
    buf_at: u64,
    /// How much the next refill reads.
    size: usize,
}

const FIRST_REFILL: usize = 64 << 10;

impl<'a> Buffered<'a> {
    pub(crate) fn new(reader: &'a Reader<'a>, len: u64) -> Self {
        Self {
            reader,
            len,
            pos: 0,
            buf: Vec::new(),
            buf_at: 0,
            size: FIRST_REFILL,
        }
    }
}

impl Clone for Buffered<'_> {
    fn clone(&self) -> Self {
        Self {
            reader: self.reader,
            len: self.len,
            pos: self.pos,
            buf: Vec::new(),
            buf_at: 0,
            size: FIRST_REFILL,
        }
    }
}

impl io::Read for Buffered<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let end = self.buf_at + self.buf.len() as u64;
        if self.pos < self.buf_at || self.pos >= end {
            // Reading straight on from the last refill: read more at once.
            self.size = if self.pos == end && !self.buf.is_empty() {
                (self.size * 2).min(PIECE)
            } else {
                FIRST_REFILL
            };
            let n = (self.len - self.pos).min(self.size as u64) as usize;
            self.buf.resize(n, 0);
            self.reader.read_at(self.pos, &mut self.buf)?;
            self.buf_at = self.pos;
        }
        let from = (self.pos - self.buf_at) as usize;
        let n = out.len().min(self.buf.len() - from);
        out[..n].copy_from_slice(&self.buf[from..from + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl io::Seek for Buffered<'_> {
    fn seek(&mut self, to: io::SeekFrom) -> io::Result<u64> {
        let pos = match to {
            io::SeekFrom::Start(p) => Some(p),
            io::SeekFrom::End(d) => self.len.checked_add_signed(d),
            io::SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use super::*;

    fn file_of(data: &[u8]) -> (File, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("brar-disk-{}-{}", std::process::id(), data.len()));
        File::create(&path).unwrap().write_all(data).unwrap();
        (File::open(&path).unwrap(), path)
    }

    #[test]
    fn a_cancelled_reader_stops_before_reading() {
        let (file, path) = file_of(&[7u8; 1000]);
        let stopped = || false;
        let reader = Reader::new(&file, true, &stopped);
        let mut buf = [0u8; 10];
        assert!(reader.read_at(0, &mut buf).is_err());
        assert_eq!(buf, [0u8; 10]);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn buffered_reads_match_the_file_through_jumps() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let (file, path) = file_of(&data);
        let running = || true;
        let reader = Reader::new(&file, true, &running);
        let mut b = Buffered::new(&reader, data.len() as u64);
        // Straight through, in odd sizes: the refills grow.
        let mut all = Vec::new();
        let mut piece = [0u8; 9_999];
        loop {
            let n = b.read(&mut piece).unwrap();
            if n == 0 {
                break;
            }
            all.extend_from_slice(&piece[..n]);
        }
        assert_eq!(all, data);
        assert!(b.size > FIRST_REFILL);
        // A jump back starts small again, and reads the right bytes.
        b.seek(SeekFrom::Start(123_456)).unwrap();
        let mut four = [0u8; 4];
        b.read_exact(&mut four).unwrap();
        assert_eq!(four, data[123_456..123_460]);
        assert_eq!(b.size, FIRST_REFILL);
        let mut clone = b.clone();
        clone.seek(SeekFrom::End(-3)).unwrap();
        let mut rest = Vec::new();
        clone.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, data[data.len() - 3..]);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    /// What Windows says about this machine's drives: `cargo test -p brar-core
    /// drives -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn drives() {
        for letter in 'C'..='Z' {
            let root = format!("{letter}:\\");
            if Path::new(&root).exists() {
                #[cfg(windows)]
                println!("{root} seek penalty (None: Windows can't say): {:?}", seek_penalty(Path::new(&root)));
                #[cfg(not(windows))]
                println!("{root} seeks slowly (a hard drive): {}", seeks_slowly(Path::new(&root)));
            }
        }
    }
}
