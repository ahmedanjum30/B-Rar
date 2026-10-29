//! Pieces of one Deflate stream, decoded without the output before them, for decoding the
//! stream on several cores.
//!
//! A piece starts at a block boundary found by trial ([`find_block`]): Deflate doesn't mark
//! where blocks start, but a dynamic block's header (three Huffman code descriptions, which
//! must form complete codes) or a stored block's (a length and its complement) is very
//! unlikely to appear by chance. The piece is decoded into 16-bit values ([`decode_piece`]):
//! bytes, and markers for bytes a match copied from the 32 KB before the piece, unknown at the
//! time. Once the previous piece is complete, [`resolve`] replaces the markers.

use super::inflate::{Bad, Bits, Bytes, END, INVALID, LITERAL, Output, Res, Tables, fast};

/// How far back a match can reach.
pub(crate) const WINDOW: usize = 32768;
/// Values from this up are markers: `MARKER + i` is byte `i` of the window before the piece.
pub(crate) const MARKER: u16 = 0x8000;

/// One decoded piece: a head of bytes and markers, then (once 32 KB of output have passed
/// without a marker, so no later match can reach before the piece) plain bytes.
pub(crate) struct Piece {
    /// Bytes (< 256) and markers.
    pub head: Vec<u16>,
    /// The output after the head: `tail[tail_from..]` (before that, a copy of the head's last
    /// 32 KB that the decoder copied from).
    pub tail: Vec<u8>,
    pub tail_from: usize,
    /// Where it started, and where its last block ended (bit positions in the stream).
    pub start: usize,
    pub end: usize,
    /// It ended with the stream's final block.
    pub last: bool,
}

impl Piece {
    pub(crate) fn tail(&self) -> &[u8] {
        &self.tail[self.tail_from..]
    }
}

/// Decodes from `start` (a block boundary) until a block ends at or after bit `stop`, or the
/// final block ends. `input` is the stream (`len` bytes) followed by the padding
/// [`super::inflate::inflate`] needs. At most `max_out` bytes (a damaged stream can't make it
/// run away).
pub(crate) fn decode_piece(t: &mut Tables, input: &[u8], len: usize, start: usize, stop: usize, max_out: usize) -> Res<Piece> {
    let mut bits = Bits::new(input, len, start);
    let mut head: Vec<u16> = Vec::with_capacity(1 << 20);
    // Where the last marker in `head` ends (0: the window before the piece is all markers).
    let mut marked = 0usize;
    // Plain bytes once `tail_from > 0`.
    let mut tail: Vec<u8> = Vec::new();
    let mut tail_from = 0usize;
    loop {
        bits.refill();
        let last = bits.take(1) == 1;
        let kind = bits.take(2);
        if tail_from == 0 && head.len() - marked >= WINDOW {
            // No marker in the last 32 KB: nothing later can reach before the piece. Plain bytes
            // from here, after a copy of those 32 KB to copy from.
            tail.reserve(8 << 20);
            tail.extend(head[head.len() - WINDOW..].iter().map(|&v| v as u8));
            tail_from = WINDOW;
        }
        match kind {
            0 => {
                bits.align_to_byte();
                bits.refill();
                let n = bits.take(16) as usize;
                if bits.take(16) as usize != !n & 0xffff {
                    return Err(Bad);
                }
                let at = bits.pos() / 8;
                if at + n > len {
                    return Err(Bad);
                }
                if tail_from > 0 {
                    tail.extend_from_slice(&input[at..at + n]);
                } else {
                    head.extend(input[at..at + n].iter().map(|&b| u16::from(b)));
                }
                bits = Bits::new(input, len, (at + n) * 8);
            }
            1 | 2 => {
                if kind == 1 {
                    t.fixed()?;
                } else {
                    t.dynamic(&mut bits, false)?;
                }
                bits = if tail_from > 0 {
                    block8(t, bits, len, &mut tail, max_out + tail_from)?
                } else {
                    block16(t, bits, len, &mut head, &mut marked, max_out)?
                };
            }
            _ => return Err(Bad),
        }
        if bits.overrun(len) || head.len() + tail.len() - tail_from > max_out {
            return Err(Bad);
        }
        if last || bits.pos() >= stop {
            return Ok(Piece {
                head,
                tail,
                tail_from,
                start,
                end: bits.pos(),
                last,
            });
        }
    }
}

/// 16-bit values for [`fast`] and [`block16`]: copies from before the piece leave markers, and
/// `marked` follows where the last marker written ends (conservatively: a copy that may have
/// carried one counts as a marker up to its end, which only delays the switch to plain bytes).
struct Values {
    base: *mut u16,
    marked: usize,
}

impl Output for Values {
    type Elem = u16;

    #[inline(always)]
    fn base(&self) -> *mut u16 {
        self.base
    }

    #[inline(always)]
    unsafe fn literal(at: *mut u16, byte: u8) {
        // SAFETY: the caller's.
        unsafe { *at = u16::from(byte) };
    }

    #[inline(always)]
    unsafe fn copy(&mut self, at: *mut u16, back: usize, n: usize) -> bool {
        // SAFETY: at is within the buffer.
        let pos = unsafe { at.offset_from(self.base) } as usize;
        if back > pos + WINDOW {
            return false;
        }
        // SAFETY: the caller's room for pos + n (and 3 values past it); sources at or after
        // pos - back >= 0 when back <= pos.
        unsafe {
            let base = self.base;
            let dst = base.add(pos);
            if back > pos {
                // Partly (or wholly) from before the piece: markers for that part.
                for j in 0..n {
                    let src = pos + j;
                    *dst.add(j) = if src < back {
                        MARKER + (WINDOW + src - back) as u16
                    } else {
                        *base.add(src - back)
                    };
                }
                self.marked = pos + n;
            } else if back >= 4 {
                // Four values (8 bytes) at a time, noting whether any was a marker: only the
                // match's own values count (the last word may run past it, into values that
                // aren't part of it, or not written yet).
                let src = dst.sub(back);
                let mut any = 0u64;
                let mut j = 0;
                while j < n {
                    let w = (src.add(j) as *const u64).read_unaligned();
                    (dst.add(j) as *mut u64).write_unaligned(w);
                    let lanes = (n - j).min(4);
                    any |= if lanes == 4 { w } else { w & ((1u64 << (16 * lanes)) - 1) };
                    j += 4;
                }
                if any & 0x8000_8000_8000_8000 != 0 {
                    self.marked = pos + n;
                }
            } else {
                let src = dst.sub(back);
                let mut any = 0u16;
                for j in 0..n {
                    let v = *src.add(j);
                    *dst.add(j) = v;
                    any |= v;
                }
                if any >= MARKER {
                    self.marked = pos + n;
                }
            }
        }
        true
    }
}

/// One Huffman-coded block into 16-bit values (see [`Values`]).
fn block16<'a>(t: &Tables, mut bits: Bits<'a>, len: usize, out: &mut Vec<u16>, marked: &mut usize, max_out: usize) -> Res<Bits<'a>> {
    let (lit, dist) = (&t.litlen, &t.dist);
    // Room for one round (a match, or three literals) plus the copies' overshoot (in values).
    const ROUND: usize = 258 + 3 + 32;
    let mut pos = out.len();
    let mut values = Values {
        base: out.as_mut_ptr(),
        marked: *marked,
    };
    let result = loop {
        if out.capacity() - pos < 1 << 16 {
            // SAFETY: values below pos were written.
            unsafe { out.set_len(pos) };
            out.reserve(out.capacity().max(1 << 20));
        }
        values.base = out.as_mut_ptr();
        // The unchecked loop while a whole match and the copies' overshoot fit (in the
        // capacity and under max_out); single checked rounds only near those ends.
        let fast_end = (out.capacity() - ROUND).min(max_out.saturating_sub(258));
        // SAFETY: fast_end + 258 and the overshoot are within the capacity.
        if let Some(r) = unsafe { fast(t, &mut bits, &mut values, &mut pos, fast_end) } {
            break r.map(|()| bits);
        }
        if out.capacity() - pos < ROUND {
            // The fast loop ran up to the capacity's end: more room first.
            continue;
        }
        bits.refill();
        let (mut e, mut v) = lit.decode(&mut bits);
        // SAFETY (stores and copies): ROUND values after pos.
        if e & LITERAL != 0 {
            unsafe { Values::literal(values.base.add(pos), (e >> 16) as u8) };
            pos += 1;
            (e, v) = lit.decode(&mut bits);
            if e & LITERAL != 0 {
                unsafe { Values::literal(values.base.add(pos), (e >> 16) as u8) };
                pos += 1;
                (e, v) = lit.decode(&mut bits);
                if e & LITERAL != 0 {
                    unsafe { Values::literal(values.base.add(pos), (e >> 16) as u8) };
                    pos += 1;
                    continue;
                }
            }
        }
        if e & (END | INVALID) != 0 {
            break if e & INVALID != 0 { Err(Bad) } else { Ok(bits) };
        }
        bits.refill();
        let n = v;
        let (d, back) = dist.decode(&mut bits);
        if d & INVALID != 0 || pos + n > max_out || !unsafe { values.copy(values.base.add(pos), back, n) } {
            break Err(Bad);
        }
        pos += n;
        if bits.overrun(len) {
            break Err(Bad);
        }
    };
    *marked = values.marked;
    // SAFETY: values below pos were written.
    unsafe { out.set_len(pos) };
    result
}
/// One Huffman-coded block into plain bytes, copying only from earlier in `out`.
fn block8<'a>(t: &Tables, mut bits: Bits<'a>, len: usize, out: &mut Vec<u8>, max_out: usize) -> Res<Bits<'a>> {
    let (lit, dist) = (&t.litlen, &t.dist);
    // Room for one round (a match, or three literals) plus the copies' overshoot.
    const ROUND: usize = 258 + 3 + super::inflate::OUTPUT_SLACK;
    let mut pos = out.len();
    let result = loop {
        if out.capacity() - pos < 1 << 16 {
            // SAFETY: bytes below pos were written.
            unsafe { out.set_len(pos) };
            out.reserve(out.capacity().max(1 << 20));
        }
        let base = out.as_mut_ptr();
        // The unchecked loop while a whole match and the copies' overshoot fit (in the
        // capacity and under max_out); single checked rounds only near those ends.
        let fast_end = (out.capacity() - ROUND).min(max_out.saturating_sub(258));
        // SAFETY: fast_end + 258 + OUTPUT_SLACK <= capacity.
        if let Some(r) = unsafe { fast(t, &mut bits, &mut Bytes(base), &mut pos, fast_end) } {
            break r.map(|()| bits);
        }
        if out.capacity() - pos < ROUND {
            // The fast loop ran up to the capacity's end (it stops up to a match past
            // fast_end): more room first.
            continue;
        }
        bits.refill();
        let (mut e, mut v) = lit.decode(&mut bits);
        // SAFETY (stores and copies): the reserve above left ROUND bytes after pos.
        if e & LITERAL != 0 {
            unsafe { *base.add(pos) = (e >> 16) as u8 };
            pos += 1;
            (e, v) = lit.decode(&mut bits);
            if e & LITERAL != 0 {
                unsafe { *base.add(pos) = (e >> 16) as u8 };
                pos += 1;
                (e, v) = lit.decode(&mut bits);
                if e & LITERAL != 0 {
                    unsafe { *base.add(pos) = (e >> 16) as u8 };
                    pos += 1;
                    continue;
                }
            }
        }
        if e & (END | INVALID) != 0 {
            break if e & INVALID != 0 { Err(Bad) } else { Ok(bits) };
        }
        bits.refill();
        let n = v;
        let (d, back) = dist.decode(&mut bits);
        if d & INVALID != 0 {
            break Err(Bad);
        }
        if back > pos || pos + n > max_out {
            break Err(Bad);
        }
        unsafe { super::inflate::copy_match(base, pos, back, n) };
        pos += n;
        if bits.overrun(len) {
            break Err(Bad);
        }
    };
    // SAFETY: bytes below pos were written.
    unsafe { out.set_len(pos) };
    result
}

/// Replaces the markers in `head` with bytes of `window` (the output before the piece, up to
/// the last [`WINDOW`] bytes of it), appending the result to `dst`.
pub(crate) fn resolve(head: &[u16], window: &[u8], dst: &mut Vec<u8>) -> Res<()> {
    if window.len() >= WINDOW && head.len() >= 1 << 16 {
        // A table from every 16-bit value to its byte (bytes to themselves, markers to the
        // window's byte): one lookup per value, no branch. Values 256..MARKER don't occur.
        let mut lut = vec![0u8; 1 << 16];
        for (i, l) in lut[..256].iter_mut().enumerate() {
            *l = i as u8;
        }
        lut[usize::from(MARKER)..].copy_from_slice(&window[window.len() - WINDOW..]);
        let start = dst.len();
        dst.resize(start + head.len(), 0);
        for (d, &v) in dst[start..].iter_mut().zip(head) {
            *d = lut[usize::from(v)];
        }
        return Ok(());
    }
    let missing = WINDOW - window.len().min(WINDOW);
    let window = &window[window.len().saturating_sub(WINDOW)..];
    dst.reserve(head.len());
    for &v in head {
        if v < MARKER {
            dst.push(v as u8);
        } else {
            let i = usize::from(v - MARKER);
            if i < missing {
                return Err(Bad); // before the start of the stream
            }
            dst.push(window[i - missing]);
        }
    }
    Ok(())
}
/// The first bit position in `from..to` where a dynamic or stored block plausibly starts (see
/// the module comment). Fixed-Huffman blocks can't be told from noise and aren't found.
pub(crate) fn find_block(t: &mut Tables, input: &[u8], len: usize, from: usize, to: usize) -> Option<usize> {
    let to = to.min(len * 8);
    let mut b = from;
    while b < to {
        // The three header bits and what follows, from a 64-bit window at `b`.
        let byte = b / 8;
        // SAFETY: byte < len and the input has INPUT_PADDING (16) bytes after len.
        let w = u64::from_le(unsafe { (input.as_ptr().add(byte) as *const u64).read_unaligned() }) >> (b % 8);
        match (w >> 1) & 3 {
            2 => {
                // Dynamic: HLIT <= 29, HDIST <= 29 first (cheap), then the whole header.
                if (w >> 3) & 31 <= 29 && (w >> 8) & 31 <= 29 && dynamic_header_at(t, input, len, b) && confirm(t, input, len, b) {
                    return Some(b);
                }
            }
            0 => {
                // Stored: zero padding to the byte boundary, then LEN and its complement. 32
                // bits match by chance once in 65,536 positions, a few times per block of
                // compressed data, so the block after this one must have a valid header too.
                let pad = (8 - (b + 3) % 8) % 8;
                let rest = w >> 3;
                if rest & ((1 << pad) - 1) == 0 && pad + 32 <= 61 {
                    let n = (rest >> pad) & 0xffff;
                    let nn = (rest >> (pad + 16)) & 0xffff;
                    let after = (b + 3 + pad + 32) / 8 * 8 + n as usize * 8;
                    // A final block must end the stream: starting a bit early, on the 1 that ends
                    // the previous block's code, would read a later stored block as the last one.
                    // The next block: a dynamic one (decoded to confirm it), or another stored one
                    // (incompressible data comes as runs of them).
                    let fits = if w & 1 == 1 {
                        after == len * 8
                    } else {
                        after < len * 8
                            && (stored_header_at(input, len, after) || (dynamic_header_at(t, input, len, after) && confirm(t, input, len, after)))
                    };
                    if n == !nn & 0xffff && n > 0 && fits {
                        return Some(b);
                    }
                }
            }
            _ => {}
        }
        b += 1;
    }
    None
}

/// A header that passes [`dynamic_header_at`] still turns up by chance about once every few
/// blocks of real data, so a candidate is only taken after its whole block decodes cleanly (no
/// invalid code, nothing further back than [`WINDOW`]) and ends where another block plausibly
/// starts, or the stream ends.
fn confirm(t: &mut Tables, input: &[u8], len: usize, b: usize) -> bool {
    let Ok(piece) = decode_piece(t, input, len, b, b + 1, 4 << 20) else {
        return false;
    };
    if piece.last {
        // Only if it really is the end of the stream (see the stored case in find_block).
        return piece.end.div_ceil(8) == len;
    }
    let e = piece.end;
    if e + 3 > len * 8 {
        return false;
    }
    // SAFETY: e / 8 < len and the input has INPUT_PADDING (16) bytes after len.
    let w = u64::from_le(unsafe { (input.as_ptr().add(e / 8) as *const u64).read_unaligned() }) >> (e % 8);
    match (w >> 1) & 3 {
        1 => true, // fixed Huffman: nothing to check in its header
        2 => dynamic_header_at(t, input, len, e),
        0 => {
            let pad = (8 - (e + 3) % 8) % 8;
            let rest = w >> 3;
            pad + 32 <= 61 && (rest >> pad) & 0xffff == !(rest >> (pad + 16)) & 0xffff
        }
        _ => false,
    }
}

/// Do decoding from bit `a` and from bit `b` mean the same thing? Yes if they're equal, or if
/// both read as stored-block headers padded to the same byte: a stored header is 3 bits and
/// zero padding, so starting a few (zero) bits early reads it the same, and [`find_block`]
/// returns the earliest such position.
pub(crate) fn same_start(input: &[u8], a: usize, b: usize) -> bool {
    let stored_byte = |p: usize| {
        // SAFETY: callers pass positions inside the stream; the input is padded after it.
        let w = u64::from_le(unsafe { (input.as_ptr().add(p / 8) as *const u64).read_unaligned() }) >> (p % 8);
        let pad = (8 - (p + 3) % 8) % 8;
        // The byte the data starts at, and whether it's the final block.
        ((w >> 1) & 3 == 0 && (w >> 3) & ((1 << pad) - 1) == 0).then_some(((p + 3).div_ceil(8), w & 1))
    };
    a == b || matches!((stored_byte(a), stored_byte(b)), (Some(x), Some(y)) if x == y)
}

/// Is there a stored block's header at byte-aligned bit `b`: LEN and its complement, LEN > 0,
/// its data inside the stream?
fn stored_header_at(input: &[u8], len: usize, b: usize) -> bool {
    if !b.is_multiple_of(8) || b / 8 + 5 > len {
        return false;
    }
    let p = b / 8;
    let (flags, n, nn) = (
        input[p],
        u16::from_le_bytes([input[p + 1], input[p + 2]]),
        u16::from_le_bytes([input[p + 3], input[p + 4]]),
    );
    // Block headers start on the bit after the previous block; a stored one that directly
    // follows another starts on a byte boundary: 3 header bits and 5 zero padding bits.
    flags & 0b1111_1110 == 0 && n == !nn && n > 0 && p + 5 + usize::from(n) <= len
}

/// Is there a (strictly) valid dynamic block header at bit `b`?
fn dynamic_header_at(t: &mut Tables, input: &[u8], len: usize, b: usize) -> bool {
    let mut bits = Bits::new(input, len, b);
    bits.refill();
    if bits.take(3) >> 1 != 2 {
        return false;
    }
    t.dynamic(&mut bits, true).is_ok()
}

#[cfg(test)]
mod tests {
    use super::super::inflate::{INPUT_PADDING, OUTPUT_SLACK, inflate_recording};
    use super::*;
    use std::io::Write;

    /// ~8 MB with long-range repeats, compressed at `level` with a few sync flushes (empty
    /// stored blocks) on the way: many blocks. Returns the stream, its length, the true output
    /// and the block boundaries.
    #[allow(clippy::type_complexity)]
    fn sample(level: u32) -> (Vec<u8>, usize, Vec<u8>, Vec<(usize, usize)>) {
        let mut x = 7u64;
        let mut data = Vec::new();
        while data.len() < 8 << 20 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            match x % 4 {
                0 => data.extend((0..300).map(|i| (x >> (i % 8)) as u8)), // noise
                1 => data.extend_from_slice(b"the quick brown fox jumps over the lazy dog, "),
                _ if data.is_empty() => {}
                _ => {
                    // a repeat from up to 30 KB back
                    let back = 1 + (x as usize >> 8) % 30_000.min(data.len());
                    let start = data.len() - back;
                    let n = 20 + (x as usize >> 24) % 200;
                    for i in 0..n {
                        data.push(data[start + i % back]);
                    }
                }
            }
        }
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
        for (i, part) in data.chunks(1 << 20).enumerate() {
            e.write_all(part).unwrap();
            if i % 3 == 1 {
                e.flush().unwrap();
            }
        }
        let mut packed = e.finish().unwrap();
        let len = packed.len();
        packed.resize(len + INPUT_PADDING, 0);
        let mut out = vec![0u8; data.len() + OUTPUT_SLACK];
        let mut starts = Vec::new();
        inflate_recording(&mut Tables::default(), &packed, len, &mut out, data.len(), Some(&mut starts)).unwrap();
        assert!(out[..data.len()] == data[..]);
        (packed, len, data, starts)
    }

    #[test]
    fn pieces_resolve_to_the_true_output() {
        // (zlib's level 1 writes too few blocks in 2 MB to cut it into pieces.)
        for level in [6, 9] {
            let (packed, len, data, starts) = sample(level);
            assert!(starts.len() > 4, "level {level}: only {} blocks", starts.len());
            let mut t = Tables::default();
            for k in [1, starts.len() / 2, starts.len() - 1] {
                let (bit, at) = starts[k];
                // Up to the block two later (or the end).
                let stop = starts.get(k + 2).map_or(usize::MAX, |s| s.0);
                let piece = decode_piece(&mut t, &packed, len, bit, stop, data.len()).unwrap();
                assert_eq!(piece.start, bit);
                let mut got = Vec::new();
                resolve(&piece.head, &data[..at], &mut got).unwrap();
                got.extend_from_slice(piece.tail());
                assert!(got[..] == data[at..at + got.len()], "level {level}, block {k}");
                if let Some(next) = starts.get(k + 2) {
                    assert_eq!(piece.end, next.0, "level {level}, block {k}: ends at a block boundary");
                }
            }
        }
    }

    /// Decoding speed by mode on the biggest Deflate entry of $BRAR_BENCH_ZIP, one thread,
    /// cycles per output byte: the whole stream as one piece (plain bytes after its first 32 KB),
    /// and 64 MB from a block in the middle (16-bit values with markers throughout), against
    /// the plain decoder. Run with
    /// `cargo test --release -p brar-core --lib piece_modes -- --ignored --nocapture`.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn piece_modes() {
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
        let path = std::env::var("BRAR_BENCH_ZIP").expect("set BRAR_BENCH_ZIP");
        let file = std::fs::File::open(&path).unwrap();
        let mut archive = ::zip::ZipArchive::new(&file).unwrap();
        let (mut start, mut packed_len, mut size) = (0, 0, 0);
        for i in 0..archive.len() {
            let f = archive.by_index_raw(i).unwrap();
            if f.compression() == ::zip::CompressionMethod::Deflated && f.size() > size {
                (start, packed_len, size) = (f.data_start().unwrap(), f.compressed_size() as usize, f.size());
            }
        }
        let mut packed = vec![0u8; packed_len + INPUT_PADDING];
        crate::whole::read_at(&file, start, &mut packed[..packed_len]).unwrap();
        let mut t = Tables::default();
        let mut out = vec![0u8; size as usize + OUTPUT_SLACK];
        let c0 = cycles();
        let mut starts = Vec::new();
        inflate_recording(&mut t, &packed, packed_len, &mut out, size as usize, Some(&mut starts)).unwrap();
        let plain = cycles() - c0;
        let c0 = cycles();
        let whole = decode_piece(&mut t, &packed, packed_len, 0, usize::MAX, 1 << 40).unwrap();
        let one_piece = cycles() - c0;
        let mid = starts[starts.len() / 2].0;
        let c0 = cycles();
        let p = decode_piece(&mut t, &packed, packed_len, mid, mid + (16 << 20) * 8, 1 << 40).unwrap();
        let marked = cycles() - c0;
        let n = p.head.len() + p.tail().len();
        println!(
            "plain {:.2} cycles/B; as one piece {:.2} (head {} KB); from mid-stream {:.2} cycles/B over {} MB (head {} MB)",
            plain as f64 / size as f64,
            one_piece as f64 / size as f64,
            whole.head.len() >> 10,
            marked as f64 / n as f64,
            n >> 20,
            p.head.len() >> 20
        );
    }

    /// The block finder on real streams: the biggest Deflate entry of each ZIP in
    /// $BRAR_BENCH_ZIP (`;`-separated), from 200 random positions. Every position found must be
    /// a true block start (and the first one after the search start, unless a fixed-code or
    /// empty stored block came first). Run with
    /// `cargo test --release -p brar-core --lib finder_real -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn finder_real() {
        let paths = std::env::var("BRAR_BENCH_ZIP").expect("set BRAR_BENCH_ZIP");
        for path in paths.split(';').filter(|p| !p.is_empty()) {
            let file = std::fs::File::open(path).unwrap();
            let mut archive = ::zip::ZipArchive::new(&file).unwrap();
            let (mut start, mut packed_len, mut size) = (0, 0, 0);
            for i in 0..archive.len() {
                let f = archive.by_index_raw(i).unwrap();
                if f.compression() == ::zip::CompressionMethod::Deflated && f.size() > size {
                    (start, packed_len, size) = (f.data_start().unwrap(), f.compressed_size() as usize, f.size());
                }
            }
            let mut packed = vec![0u8; packed_len + INPUT_PADDING];
            crate::whole::read_at(&file, start, &mut packed[..packed_len]).unwrap();
            let mut out = vec![0u8; size as usize + OUTPUT_SLACK];
            let mut starts = Vec::new();
            let mut t = Tables::default();
            inflate_recording(&mut t, &packed, packed_len, &mut out, size as usize, Some(&mut starts)).unwrap();
            let truth: Vec<usize> = starts.iter().map(|s| s.0).collect();
            let kind = |bit: usize| (packed[bit / 8] as u32 | (packed[bit / 8 + 1] as u32) << 8) >> (bit % 8) >> 1 & 3;
            let fixed = truth.iter().filter(|&&b| kind(b) == 1).count();
            let (mut first, mut later, mut wrong, mut none) = (0, 0, 0, 0);
            let mut x = 12345u64;
            let t0 = std::time::Instant::now();
            for _ in 0..200 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let from = (x as usize) % (packed_len * 8);
                match find_block(&mut t, &packed, packed_len, from, from + (8 << 20) * 8) {
                    Some(b) => {
                        let i = truth.partition_point(|&s| s < from);
                        if truth.get(i).is_some_and(|&s| same_start(&packed, s, b)) {
                            first += 1;
                        } else if truth.iter().any(|&s| s > from && same_start(&packed, s, b)) {
                            later += 1; // a true start (or one that reads the same), not the first
                        } else {
                            wrong += 1;
                        }
                    }
                    None => none += 1,
                }
            }
            let per = t0.elapsed().as_secs_f64() / 200.0;
            let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy();
            println!(
                "{name:<12} {} MB: {} blocks ({fixed} fixed-code), avg {} KB packed per block; searches: {first} first, {later} later, {wrong} WRONG, {none} none; {:.2} ms per search",
                size >> 20,
                truth.len(),
                (packed_len / truth.len().max(1)) >> 10,
                per * 1e3
            );
            assert_eq!(wrong, 0);
        }
    }

    #[test]
    fn finds_true_block_starts() {
        let (packed, len, _, starts) = sample(6);
        let mut t = Tables::default();
        let truth: std::collections::HashSet<usize> = starts.iter().map(|s| s.0).collect();
        // A block's type: its bits 1-2 (1 = fixed Huffman, which the finder can't recognize).
        let kind = |bit: usize| (packed[bit / 8] as u32 | (packed[bit / 8 + 1] as u32) << 8) >> (bit % 8) >> 1 & 3;
        let (mut next, mut later, mut wrong) = (0, 0, 0);
        for w in starts.windows(2).skip(1) {
            // From just after one boundary: the next true one, or (past a fixed-code block or
            // an empty stored block, which aren't searched for) a later one or none; never a
            // position that isn't a boundary.
            let skippable = kind(w[1].0) == 1 || (kind(w[1].0) == 0);
            match find_block(&mut t, &packed, len, w[0].0 + 1, len * 8) {
                Some(b) if b == w[1].0 => next += 1,
                Some(b) if truth.contains(&b) && skippable => later += 1,
                None if skippable => later += 1,
                _ => wrong += 1,
            }
        }
        assert!(next > 0 && wrong == 0, "{next} next, {later} later, {wrong} wrong");
    }
}
