//! Helpers for the "whole file in memory" fast paths (ZIP and RAR5 via B-Rar's
//! own decoder): positioned reads and a cap on how many bytes those paths hold at once.

use std::{
    fs::File,
    io,
    sync::{Condvar, Mutex},
};

/// Most bytes the fast paths hold in memory at once (compressed + decoded, across threads).
/// Larger entries take the streaming paths instead.
pub(crate) const BUDGET: u64 = 2 << 30;

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
