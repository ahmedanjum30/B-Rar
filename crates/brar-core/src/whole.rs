//! Helpers for the "whole file in memory" fast paths (ZIP and RAR5 via B-Rar's
//! own decoder): positioned reads and a cap on how many bytes those paths hold at once.

use std::{
    fs::File,
    io,
    sync::{Condvar, Mutex},
};

/// Most bytes the fast paths hold in memory at once (compressed + decoded, across threads):
/// 3/16 of the PC's memory, at most 2 GB (768 MB with 4 GB, 1.5 GB with 8 GB, 2 GB from
/// about 11 GB), so a small PC isn't pushed into swapping; 2 GB when the memory can't be read.
/// Larger entries take the streaming paths instead. Not more on big PCs: on a 14 GB ZIP of
/// big, barely compressible entries, 6 and 8 GB were no faster than 2 GB (25-28 s each) and
/// took 8-15 GB of memory. `BRAR_BUDGET_MB` overrides it (for testing).
pub(crate) fn budget() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
        if let Some(mb) = std::env::var("BRAR_BUDGET_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
            return mb << 20;
        }
        installed_memory().map_or(2 << 30, |m| (m / 16 * 3).clamp(256 << 20, 2 << 30))
    })
}

/// Most bytes of decoded small files waiting to be written in the background: 1/32 of the
/// PC's memory, at most 512 MB (128 MB with 4 GB, 512 MB from 16 GB); 512 MB when it can't
/// be read.
pub(crate) fn writes_in_flight() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| installed_memory().map_or(512 << 20, |m| (m / 32).clamp(64 << 20, 512 << 20)))
}

/// The PC's installed memory, if it can tell.
pub(crate) fn installed_memory() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: `status` is a MEMORYSTATUSEX with its length set.
        unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
        Some(status.ullTotalPhys)
    }
    #[cfg(not(windows))]
    None
}

/// Fills `buf` from `file` at `offset`, without a shared file cursor (safe across threads).
pub(crate) fn read_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        #[cfg(windows)]
        let n = file.seek_read(buf, offset)?;
        #[cfg(unix)]
        let n = file.read_at(buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// Caps the bytes held by fast-path jobs at once. A job larger than what's free waits until
/// the others finish; one job always runs, so nothing can deadlock.
pub(crate) struct Budget {
    state: Mutex<(u64, usize)>, // (bytes in use, jobs holding a lease)
    freed: Condvar,
    limit: u64,
}

pub(crate) struct Lease<'a> {
    budget: &'a Budget,
    bytes: u64,
}

impl Budget {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            state: Mutex::new((0, 0)),
            freed: Condvar::new(),
            limit,
        }
    }

    pub(crate) fn take(&self, bytes: u64) -> Lease<'_> {
        let mut state = self.state.lock().unwrap();
        while state.1 > 0 && state.0 + bytes > self.limit {
            state = self.freed.wait(state).unwrap();
        }
        state.0 += bytes;
        state.1 += 1;
        Lease { budget: self, bytes }
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut state = self.budget.state.lock().unwrap();
        state.0 -= self.bytes;
        state.1 -= 1;
        self.budget.freed.notify_all();
    }
}
