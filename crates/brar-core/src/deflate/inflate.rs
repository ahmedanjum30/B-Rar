//! Single-stream Deflate decoding into a buffer of known size.

/// The input couldn't be decoded (damaged, or not Deflate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bad;

pub(crate) type Res<T> = Result<T, Bad>;

/// Zero bytes the caller appends to the input: the bit reader loads 8 bytes at a time.
pub(crate) const INPUT_PADDING: usize = 16;
/// Spare bytes the caller leaves after the output: copies may write up to this far past.
pub(crate) const OUTPUT_SLACK: usize = 64;

/// Least-significant-bit-first reader over `data` (which ends in [`INPUT_PADDING`] zeros).
/// `Copy`, so the hot loop works on a local copy the compiler keeps in registers.
#[derive(Clone, Copy)]
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    /// Next byte to load into `buf`.
    next: usize,
    buf: u64,
    /// Valid bits in `buf` (the low ones).
    cnt: u32,
    /// `next` may not pass this (keeps every 8-byte load inside the padded input).
    max_next: usize,
}

impl<'a> Bits<'a> {
    /// A reader at bit `bit` of `data`, whose first `len` bytes are the stream.
    pub(crate) fn new(data: &'a [u8], len: usize, bit: usize) -> Self {
        debug_assert!(data.len() >= len + INPUT_PADDING);
        let mut b = Self {
            data,
            next: bit / 8,
            buf: 0,
            cnt: 0,
            max_next: len + 8,
        };
        b.refill();
        b.consume((bit % 8) as u32);
        b
    }

    /// Tops `buf` up to 56..=63 valid bits with one unaligned load, no branches.
    #[inline(always)]
    pub(super) fn refill(&mut self) {
        let next = self.next.min(self.max_next);
        // SAFETY: next <= max_next = len + 8, and the input has len + INPUT_PADDING (16) bytes,
        // so next + 8 is in bounds.
        let w = unsafe { (self.data.as_ptr().add(next) as *const u64).read_unaligned() };
        self.buf |= u64::from_le(w) << self.cnt;
        self.next = next + ((63 - self.cnt) >> 3) as usize;
        self.cnt |= 56;
    }

    #[inline(always)]
    pub(super) fn peek(&self, n: u32) -> u32 {
        (self.buf & ((1u64 << n) - 1)) as u32
    }

    #[inline(always)]
    pub(super) fn consume(&mut self, n: u32) {
        self.buf >>= n;
        self.cnt -= n;
    }

    #[inline(always)]
    pub(super) fn take(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.consume(n);
        v
    }

    /// Bit position in the input.
    pub(crate) fn pos(&self) -> usize {
        self.next * 8 - self.cnt as usize
    }

    /// Past the end of the stream by more than the bits a symbol could need: the input is
    /// damaged (the padding reads as zeros, which would otherwise decode forever).
    pub(super) fn overrun(&self, len: usize) -> bool {
        self.pos() > (len + 8) * 8
    }

    pub(super) fn align_to_byte(&mut self) {
        let skip = self.cnt & 7;
        self.consume(skip);
    }
}

// Table entries (u32): bits 0..8 = bits to consume; bits 8..12 = extra bits after the code;
// bit 12 = literal; bit 13 = end of block; bit 14 = subtable pointer; bit 15 = invalid code;
// bits 16.. = the literal byte, the length or distance base, or the subtable's first index.
pub(super) const LITERAL: u32 = 1 << 12;
pub(super) const END: u32 = 1 << 13;
const SUBTABLE: u32 = 1 << 14;
pub(super) const INVALID: u32 = 1 << 15;

const LITLEN_BITS: u32 = 11;
const DIST_BITS: u32 = 8;
/// Main table plus room for the subtables of a worst-case code.
const LITLEN_SIZE: usize = (1 << LITLEN_BITS) + 2048;
const DIST_SIZE: usize = (1 << DIST_BITS) + 1024;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13,
];
/// Order the code-length code's lengths are stored in.
const PRECODE_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// A Huffman code's decoding table: entries for `bits`-bit lookups, then subtables.
pub(crate) struct Table {
    entries: Vec<u32>,
    bits: u32,
}

impl Table {
    fn new(size: usize, bits: u32) -> Self {
        Self {
            entries: vec![INVALID; size],
            bits,
        }
    }

    /// Builds the table for code lengths `lens` (0 = unused); `value(symbol)` gives the entry
    /// bits above the code length. Over-subscribed codes are rejected; incomplete ones are
    /// allowed (Deflate permits a lone distance code), their gaps decode as invalid.
    fn build(&mut self, lens: &[u8], value: impl Fn(usize) -> u32) -> Res<bool> {
        let mut count = [0u32; 16];
        for &l in lens {
            count[usize::from(l)] += 1;
        }
        count[0] = 0;
        let mut left = 1i32;
        for c in &count[1..] {
            left = (left << 1) - *c as i32;
            if left < 0 {
                return Err(Bad); // over-subscribed
            }
        }
        // First canonical code of each length.
        let mut next = [0u32; 16];
        let mut code = 0u32;
        for l in 1..16 {
            code = (code + count[l - 1]) << 1;
            next[l] = code;
        }
        let bits = self.bits;
        // A complete code covers every entry; only an incomplete one leaves gaps to mark.
        let complete = left == 0;
        if !complete {
            self.entries[..1 << bits].fill(INVALID);
        }
        // Long codes, by their first `bits` bits: the longest one sets the subtable size.
        // (Fixed arrays, not heap allocations: tables are built for every block.)
        let mut sub_bits = [0u8; 1 << LITLEN_BITS];
        let mut assigned = [0u32; 288];
        for (sym, &l) in lens.iter().enumerate() {
            if l == 0 {
                continue;
            }
            let l = u32::from(l);
            let c = next[l as usize];
            next[l as usize] += 1;
            let rev = c.reverse_bits() >> (32 - l);
            assigned[sym] = rev;
            if l > bits {
                let p = (rev & ((1 << bits) - 1)) as usize;
                sub_bits[p] = sub_bits[p].max((l - bits) as u8);
            }
        }
        // Subtables after the main table.
        let mut sub_start = [0u32; 1 << LITLEN_BITS];
        let mut free = 1u32 << bits;
        for (p, &sb) in sub_bits[..1 << bits].iter().enumerate() {
            if sb > 0 {
                sub_start[p] = free;
                let size = 1u32 << sb;
                if (free + size) as usize > self.entries.len() {
                    return Err(Bad);
                }
                if !complete {
                    self.entries[free as usize..(free + size) as usize].fill(INVALID);
                }
                self.entries[p] = SUBTABLE | (free << 16) | (u32::from(sb) << 8) | bits;
                free += size;
            }
        }
        for (sym, &l) in lens.iter().enumerate() {
            if l == 0 {
                continue;
            }
            let l = u32::from(l);
            let rev = assigned[sym];
            let v = value(sym);
            if l <= bits {
                let e = v | l;
                let mut i = rev as usize;
                while i < 1 << bits {
                    self.entries[i] = e;
                    i += 1 << l;
                }
            } else {
                let p = (rev & ((1 << bits) - 1)) as usize;
                let sb = u32::from(sub_bits[p]);
                let e = v | (l - bits);
                let mut i = (rev >> bits) as usize;
                while i < 1 << sb {
                    self.entries[sub_start[p] as usize + i] = e;
                    i += 1 << (l - bits);
                }
            }
        }
        Ok(complete)
    }

    /// The entry for the next code, its code bits consumed (extra bits not yet).
    #[inline(always)]
    pub(super) fn decode(&self, bits: &mut Bits) -> u32 {
        // SAFETY: the index is below 2^self.bits, the main table's size.
        let mut e = unsafe { *self.entries.get_unchecked(bits.peek(self.bits) as usize) };
        if e & SUBTABLE != 0 {
            bits.consume(e & 0xff);
            let i = (e >> 16) + bits.peek((e >> 8) & 0xf);
            // SAFETY: subtables were sized for every index of their bit count.
            e = unsafe { *self.entries.get_unchecked(i as usize) };
        }
        bits.consume(e & 0xff);
        e
    }
}

fn litlen_value(sym: usize) -> u32 {
    match sym {
        0..=255 => LITERAL | (sym as u32) << 16,
        256 => END,
        257..=285 => u32::from(LENGTH_BASE[sym - 257]) << 16 | u32::from(LENGTH_EXTRA[sym - 257]) << 8,
        _ => INVALID,
    }
}

fn dist_value(sym: usize) -> u32 {
    match sym {
        0..=29 => u32::from(DIST_BASE[sym]) << 16 | u32::from(DIST_EXTRA[sym]) << 8,
        _ => INVALID,
    }
}

/// The two tables of a block, and the code-length code's (built for every dynamic block).
pub(crate) struct Tables {
    pub(super) litlen: Table,
    pub(super) dist: Table,
    pre: Table,
}

impl Default for Tables {
    fn default() -> Self {
        Self {
            litlen: Table::new(LITLEN_SIZE, LITLEN_BITS),
            dist: Table::new(DIST_SIZE, DIST_BITS),
            pre: Table::new(1 << 7, 7),
        }
    }
}

impl Tables {
    pub(super) fn fixed(&mut self) -> Res<()> {
        let mut lens = [0u8; 288];
        lens[..144].fill(8);
        lens[144..256].fill(9);
        lens[256..280].fill(7);
        lens[280..].fill(8);
        self.litlen.build(&lens, litlen_value)?;
        self.dist.build(&[5; 32], dist_value)?;
        Ok(())
    }

    /// A dynamic block's header: the code-length code, then both codes' lengths. `strict`
    /// (for finding block starts by trial) also requires complete codes, as encoders write.
    pub(super) fn dynamic(&mut self, bits: &mut Bits, strict: bool) -> Res<()> {
        bits.refill();
        let nlit = bits.take(5) as usize + 257;
        let ndist = bits.take(5) as usize + 1;
        let nclen = bits.take(4) as usize + 4;
        if nlit > 286 || ndist > 30 {
            return Err(Bad);
        }
        let mut clens = [0u8; 19];
        for &i in &PRECODE_ORDER[..nclen] {
            bits.refill();
            clens[i] = bits.take(3) as u8;
        }
        let pre = &mut self.pre;
        if !pre.build(&clens, |s| (s as u32) << 16)? && strict {
            return Err(Bad);
        }
        let mut lens = [0u8; 286 + 30];
        let n = nlit + ndist;
        let mut i = 0;
        while i < n {
            bits.refill();
            let e = pre.decode(bits);
            if e & INVALID != 0 {
                return Err(Bad);
            }
            match e >> 16 {
                l @ 0..=15 => {
                    lens[i] = l as u8;
                    i += 1;
                }
                16 => {
                    if i == 0 {
                        return Err(Bad);
                    }
                    let r = 3 + bits.take(2) as usize;
                    if i + r > n {
                        return Err(Bad);
                    }
                    let prev = lens[i - 1];
                    lens[i..i + r].fill(prev);
                    i += r;
                }
                s => {
                    let r = if s == 17 { 3 + bits.take(3) } else { 11 + bits.take(7) } as usize;
                    if i + r > n {
                        return Err(Bad);
                    }
                    i += r; // zeros already
                }
            }
        }
        if lens[256] == 0 {
            return Err(Bad); // no end-of-block code
        }
        let lit_complete = self.litlen.build(&lens[..nlit], litlen_value)?;
        let dist_complete = self.dist.build(&lens[nlit..n], dist_value)?;
        // A lone distance code (length 1) is the one incomplete code encoders write.
        let lone_dist = lens[nlit..n].iter().filter(|&&l| l != 0).count() <= 1;
        if strict && (!lit_complete || !(dist_complete || lone_dist)) {
            return Err(Bad);
        }
        Ok(())
    }
}

/// Copies `len` bytes from `dist` back to `out[pos..]`; may write up to 7 bytes past
/// (`dist` >= 8) or do a pattern copy (`dist` < 8). Needs dist <= pos and slack after.
#[inline(always)]
pub(super) unsafe fn copy_match(out: *mut u8, pos: usize, dist: usize, len: usize) {
    // SAFETY: the caller guarantees 1 <= dist <= pos and len + OUTPUT_SLACK bytes of room.
    unsafe {
        let dst = out.add(pos);
        let src = dst.sub(dist);
        if dist >= 8 {
            let mut i = 0;
            while i < len {
                std::ptr::copy_nonoverlapping(src.add(i), dst.add(i), 8);
                i += 8;
            }
        } else if dist == 1 {
            std::ptr::write_bytes(dst, *src, len);
        } else {
            // A short repeating pattern: byte by byte until it spans a whole number of periods
            // that's at least 8 bytes, then 8 at a time from that far back.
            let step = dist * 8usize.div_ceil(dist);
            let head = step.min(len);
            for i in 0..head {
                *dst.add(i) = *src.add(i);
            }
            let mut i = head;
            while i < len {
                std::ptr::copy_nonoverlapping(dst.add(i - step), dst.add(i), 8);
                i += 8;
            }
        }
    }
}

/// (The parallel decoder decodes pieces with its own block functions; this whole-stream
/// decoder is the reference its tests compare against.)
///
/// Decodes a whole Deflate stream: `input` is the stream (`len` bytes) followed by
/// [`INPUT_PADDING`] zeros; the output must be exactly `size` bytes, written to `out`, which
/// has [`OUTPUT_SLACK`] more. Returns the bit position after the final block.
#[cfg(test)]
pub(crate) fn inflate(t: &mut Tables, input: &[u8], len: usize, out: &mut [u8], size: usize) -> Res<usize> {
    inflate_recording(t, input, len, out, size, None)
}

/// [`inflate`], also listing where each block starts: (bit position, output position).
#[cfg(test)]
pub(crate) fn inflate_recording(
    t: &mut Tables,
    input: &[u8],
    len: usize,
    out: &mut [u8],
    size: usize,
    mut starts: Option<&mut Vec<(usize, usize)>>,
) -> Res<usize> {
    assert!(input.len() >= len + INPUT_PADDING && out.len() >= size + OUTPUT_SLACK);
    let mut bits = Bits::new(input, len, 0);
    let mut pos = 0;
    loop {
        if let Some(s) = starts.as_deref_mut() {
            s.push((bits.pos(), pos));
        }
        bits.refill();
        let last = bits.take(1) == 1;
        match bits.take(2) {
            0 => {
                // Stored: LEN, its complement, then the bytes as they are.
                bits.align_to_byte();
                bits.refill();
                let n = bits.take(16) as usize;
                if bits.take(16) as usize != !n & 0xffff {
                    return Err(Bad);
                }
                // The reader holds up to 7 whole bytes ahead; continue from its byte position.
                let at = bits.pos() / 8;
                if at + n > len || pos + n > size {
                    return Err(Bad);
                }
                out[pos..pos + n].copy_from_slice(&input[at..at + n]);
                pos += n;
                bits = Bits::new(input, len, (at + n) * 8);
            }
            1 => {
                t.fixed()?;
                (bits, pos) = block(t, bits, len, out.as_mut_ptr(), pos, size)?;
            }
            2 => {
                t.dynamic(&mut bits, false)?;
                (bits, pos) = block(t, bits, len, out.as_mut_ptr(), pos, size)?;
            }
            _ => return Err(Bad),
        }
        if bits.overrun(len) {
            return Err(Bad);
        }
        if last {
            break;
        }
    }
    if pos != size {
        return Err(Bad);
    }
    Ok(bits.pos())
}

/// One Huffman-coded block, up to its end-of-block code. The reader and position come in and
/// go out by value, so they stay in registers.
#[cfg(test)]
#[inline(never)]
fn block<'a>(t: &Tables, mut bits: Bits<'a>, len: usize, out: *mut u8, mut pos: usize, size: usize) -> Res<(Bits<'a>, usize)> {
    let (lit, dist) = (&t.litlen, &t.dist);
    // Away from the output's end: a match (258) and three literals always fit.
    let fast_end = size.saturating_sub(258 + 3);
    loop {
        // 56+ bits: three literal codes (3 x 15), or a length code with its extra bits and a
        // distance code with its extra bits (15 + 5 + 15 + 13 = 48).
        bits.refill();
        let mut e = lit.decode(&mut bits);
        if e & LITERAL != 0 && pos < fast_end {
            // SAFETY (stores): pos + 3 <= fast_end + 3 <= size.
            unsafe { *out.add(pos) = (e >> 16) as u8 };
            pos += 1;
            e = lit.decode(&mut bits);
            if e & LITERAL != 0 {
                unsafe { *out.add(pos) = (e >> 16) as u8 };
                pos += 1;
                e = lit.decode(&mut bits);
                if e & LITERAL != 0 {
                    unsafe { *out.add(pos) = (e >> 16) as u8 };
                    pos += 1;
                    continue;
                }
            }
        }
        if e & LITERAL != 0 {
            if pos >= size {
                return Err(Bad);
            }
            // SAFETY: pos < size.
            unsafe { *out.add(pos) = (e >> 16) as u8 };
            pos += 1;
            continue;
        }
        if e & (END | INVALID) != 0 {
            if e & INVALID != 0 {
                return Err(Bad);
            }
            return Ok((bits, pos));
        }
        // A match: length (base + extra), then the distance.
        bits.refill();
        let n = (e >> 16) as usize + bits.take((e >> 8) & 0xf) as usize;
        let d = dist.decode(&mut bits);
        if d & INVALID != 0 {
            return Err(Bad);
        }
        let back = (d >> 16) as usize + bits.take((d >> 8) & 0xf) as usize;
        if back > pos || pos + n > size {
            return Err(Bad);
        }
        // SAFETY: back <= pos, pos + n <= size, and out has OUTPUT_SLACK after size.
        unsafe { copy_match(out, pos, back, n) };
        pos += n;
        if bits.overrun(len) {
            return Err(Bad);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn deflate(data: &[u8], level: u32) -> Vec<u8> {
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn roundtrip(data: &[u8], level: u32) {
        let mut packed = deflate(data, level);
        let len = packed.len();
        packed.resize(len + INPUT_PADDING, 0);
        let mut out = vec![0u8; data.len() + OUTPUT_SLACK];
        let mut t = Tables::default();
        inflate(&mut t, &packed, len, &mut out, data.len()).unwrap();
        assert!(out[..data.len()] == *data, "level {level}, {} bytes", data.len());
    }

    #[test]
    fn decodes_what_zlib_encodes() {
        let mut x = 1u64;
        let random: Vec<u8> = (0..300_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let text: Vec<u8> = (0..400_000u32)
            .map(|i| b"the quick brown fox jumps over the lazy dog "[(i % 44) as usize] ^ (i / 997 % 3) as u8)
            .collect();
        let runs: Vec<u8> = (0..100_000u32).map(|i| (i / 1000) as u8).collect();
        for level in [0, 1, 6, 9] {
            roundtrip(&random, level);
            roundtrip(&text, level);
            roundtrip(&runs, level);
            roundtrip(b"", level);
            roundtrip(b"a", level);
        }
    }

    /// B-Rar's decoder against libdeflate on every Deflate entry of each ZIP in
    /// $BRAR_BENCH_ZIP (separated by `;`): outputs compared byte for byte, then CPU cycles per
    /// output byte, best of 3. Run with
    /// `cargo test --release -p brar-core --lib inflate_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn inflate_bench() {
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
        let paths = std::env::var("BRAR_BENCH_ZIP").expect("set BRAR_BENCH_ZIP");
        for path in paths.split(';').filter(|p| !p.is_empty()) {
            let file = std::fs::File::open(path).unwrap();
            let mut archive = ::zip::ZipArchive::new(&file).unwrap();
            let mut entries = Vec::new();
            for i in 0..archive.len() {
                let f = archive.by_index_raw(i).unwrap();
                if f.compression() == ::zip::CompressionMethod::Deflated && f.size() > 0 {
                    let mut input = vec![0u8; f.compressed_size() as usize + INPUT_PADDING];
                    crate::whole::read_at(&file, f.data_start().unwrap(), &mut input[..f.compressed_size() as usize]).unwrap();
                    entries.push((input, f.compressed_size() as usize, f.size() as usize, f.crc32()));
                }
            }
            let bytes: usize = entries.iter().map(|e| e.2).sum();
            let mut ours: Vec<Vec<u8>> = entries.iter().map(|e| vec![0u8; e.2 + OUTPUT_SLACK]).collect();
            let mut theirs: Vec<Vec<u8>> = entries.iter().map(|e| vec![0u8; e.2]).collect();
            let mut t = Tables::default();
            let mut d = libdeflater::Decompressor::new();
            let (mut ours_c, mut theirs_c) = (u64::MAX, u64::MAX);
            for _ in 0..3 {
                let c0 = cycles();
                for (i, (input, len, size, _)) in entries.iter().enumerate() {
                    inflate(&mut t, input, *len, &mut ours[i], *size).unwrap();
                }
                ours_c = ours_c.min(cycles() - c0);
                let c0 = cycles();
                for (i, (input, len, _, _)) in entries.iter().enumerate() {
                    d.deflate_decompress(&input[..*len], &mut theirs[i]).unwrap();
                }
                theirs_c = theirs_c.min(cycles() - c0);
            }
            for (i, (_, _, size, crc)) in entries.iter().enumerate() {
                assert!(ours[i][..*size] == theirs[i][..], "entry {i} differs");
                assert_eq!(crc32fast::hash(&theirs[i]), *crc);
            }
            let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy();
            println!(
                "{name:<14} {} entries, {} MB: B-Rar {:.2} cycles/B, libdeflate {:.2} cycles/B",
                entries.len(),
                bytes >> 20,
                ours_c as f64 / bytes as f64,
                theirs_c as f64 / bytes as f64
            );
        }
    }

    #[test]
    fn rejects_damage() {
        let data: Vec<u8> = (0..50_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let packed = deflate(&data, 6);
        let mut t = Tables::default();
        for cut in [1, packed.len() / 2, packed.len() - 1] {
            let mut p = packed[..cut].to_vec();
            let len = p.len();
            p.resize(len + INPUT_PADDING, 0);
            let mut out = vec![0u8; data.len() + OUTPUT_SLACK];
            assert!(inflate(&mut t, &p, len, &mut out, data.len()).is_err());
        }
    }
}
