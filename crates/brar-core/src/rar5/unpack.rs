//! RAR5 / RAR7 decompression (the "v50" LZ + Huffman algorithm), decoding one non-solid file
//! straight into a buffer of its final size.
//!
//! Because the whole file lives in one buffer, matches copy directly from earlier output
//! (no ring window, no wrap checks), and filters run in place once decoding is done: nothing
//! reads the unfiltered bytes after that.
//!
//! Speed comes from: a 64-bit bit buffer kept in a register and refilled 8 bytes at a time
//! without branches, Huffman lookup tables that resolve most codes in one read and are rebuilt
//! with range fills (this data has a new table every ~20 KB), and 8/16-byte match copies.

use std::ptr;

/// Alphabet sizes: main (literals, filter, repeats, lengths), distances (RAR5 / RAR7),
/// low distance bits, repeat lengths, and the bit-length code.
const NC: usize = 306;
const DCB: usize = 64;
const DCX: usize = 80;
const LDC: usize = 16;
const RC: usize = 44;
const BC: usize = 20;
const MAX_TABLE: usize = NC + DCX + LDC + RC;

/// Longest single match: length slot 43 with 9 extra bits (4097), plus up to 3 for long
/// distances.
const MAX_MATCH: usize = 4100;
/// Extra bytes the output buffer needs past the file size: one maximal match started at the
/// last position, plus the overshoot of 16-byte chunked copies.
pub(crate) const OUTPUT_SLACK: usize = MAX_MATCH + 32;
/// Zero bytes the input must be followed by: refills load 8 bytes at a time, and on corrupt
/// data a step may read a little past the end before the bounds check stops it.
pub(crate) const INPUT_PADDING: usize = 64;
const MAX_FILTER_BLOCK: usize = 0x40_0000;

#[derive(Debug)]
pub(crate) struct Corrupt;

type Res<T> = std::result::Result<T, Corrupt>;

/// MSB-first bit reader. The next bits sit left-aligned in `buf`; `cnt` of them are valid.
struct Bits<'a> {
    data: &'a [u8],
    /// Next byte to load into `buf`.
    next: usize,
    buf: u64,
    cnt: u32,
    /// `next` may not pass this (keeps every 8-byte load inside the padded input).
    max_next: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], packed_len: usize) -> Self {
        let mut b = Self {
            data,
            next: 0,
            buf: 0,
            cnt: 0,
            max_next: packed_len + 32,
        };
        b.refill();
        b
    }

    /// Tops `buf` up to 56..=63 valid bits with one unaligned load, no branches (the
    /// libdeflate technique). Bits below `cnt` that were already there are the same bytes
    /// loaded again, so OR-ing them in is harmless.
    #[inline(always)]
    fn refill(&mut self) {
        let next = self.next.min(self.max_next);
        // SAFETY: next <= max_next = packed_len + 32, and the input has packed_len +
        // INPUT_PADDING (64) bytes, so next + 8 is in bounds.
        let w = unsafe { (self.data.as_ptr().add(next) as *const u64).read_unaligned() };
        self.buf |= u64::from_be(w) >> self.cnt;
        self.next = next + ((63 - self.cnt) >> 3) as usize;
        self.cnt |= 56;
    }

    /// The next `n` (1..=32) bits; needs `cnt >= n`.
    #[inline(always)]
    fn peek(&self, n: u32) -> u32 {
        (self.buf >> (64 - n)) as u32
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) {
        self.buf <<= n;
        self.cnt -= n;
    }

    #[inline(always)]
    fn take(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.consume(n);
        v
    }

    /// Bit position in the input.
    #[inline(always)]
    fn pos(&self) -> usize {
        self.next * 8 - self.cnt as usize
    }

    fn align_to_byte(&mut self) {
        let skip = self.cnt & 7;
        self.consume(skip);
    }
}

/// Canonical Huffman decoder for code lengths up to 15 bits: a direct lookup table for codes
/// up to `quick_bits` long, and the length-limit search for the rest.
struct Huffman {
    /// Left-aligned 16-bit upper limit of codes for each length.
    limit: [u32; 16],
    /// Index into `symbols` of the first code of each length.
    first: [u32; 16],
    count: [u32; 16],
    /// Symbols sorted by code length, then value.
    symbols: [u16; NC],
    size: usize,
    quick_bits: u32,
    quick_limit: u32,
    /// `symbol << 5 | code length` for every `quick_bits`-bit prefix below `quick_limit`.
    quick: Box<[u16]>,
}

impl Huffman {
    fn new(quick_bits: u32) -> Self {
        Self {
            limit: [0; 16],
            first: [0; 16],
            count: [0; 16],
            symbols: [0; NC],
            size: 0,
            quick_bits,
            quick_limit: 0,
            quick: vec![0; 1 << quick_bits].into_boxed_slice(),
        }
    }

    fn build(&mut self, lengths: &[u8]) {
        let size = lengths.len();
        self.size = size;
        let mut count = [0u32; 16];
        for &l in lengths {
            count[usize::from(l & 15)] += 1;
        }
        count[0] = 0;
        self.count = count;
        self.symbols[..size].fill(0);
        self.limit[0] = 0;
        self.first[0] = 0;
        // Arithmetic wraps like the reference decoder's 32-bit unsigned math, so even
        // over-subscribed (corrupt) tables decode the same way.
        let mut upper = 0u32;
        let mut canonical = true;
        for i in 1..16 {
            upper = upper.wrapping_add(count[i]);
            self.limit[i] = upper.wrapping_shl(16 - i as u32);
            canonical &= upper <= 1 << i && self.limit[i] >= self.limit[i - 1];
            upper = upper.wrapping_mul(2);
            self.first[i] = self.first[i - 1].wrapping_add(count[i - 1]);
        }
        let mut next = self.first;
        for (symbol, &l) in lengths.iter().enumerate() {
            let l = usize::from(l & 15);
            if l != 0 {
                if let Some(slot) = self.symbols.get_mut(next[l] as usize) {
                    *slot = symbol as u16;
                }
                next[l] = next[l].wrapping_add(1);
            }
        }
        let q = self.quick_bits;
        self.quick_limit = self.limit[q as usize];
        if canonical {
            // Codes of each length are consecutive, so each symbol owns one contiguous run of
            // 2^(q - length) prefixes: fill runs instead of searching per prefix.
            let mut at = 0usize;
            for (len, &n) in count.iter().enumerate().take(q as usize + 1).skip(1) {
                let span = 1usize << (q as usize - len);
                let first = self.first[len] as usize;
                for k in 0..n as usize {
                    let entry = (self.symbols[first + k] << 5) | len as u16;
                    self.quick[at..at + span].fill(entry);
                    at += span;
                }
            }
        } else {
            self.build_quick_slow();
        }
    }

    /// Per-prefix construction, exactly mirroring the reference decoder (corrupt tables).
    #[cold]
    fn build_quick_slow(&mut self) {
        let q = self.quick_bits;
        let mut len = 1usize;
        for code in 0..(1u32 << q) {
            let field = code << (16 - q);
            while len < 16 && field >= self.limit[len] {
                len += 1;
            }
            let dist = field.wrapping_sub(self.limit[len - 1]) >> (16 - len as u32);
            let symbol = if len < 16 {
                let p = self.first[len].wrapping_add(dist) as usize;
                if p < self.size { self.symbols[p] } else { 0 }
            } else {
                0
            };
            self.quick[code as usize] = (symbol << 5) | len as u16;
        }
    }

    /// Needs at least 15 valid bits.
    #[inline(always)]
    fn decode(&self, bits: &mut Bits) -> usize {
        let field = bits.peek(16) & 0xfffe;
        if field < self.quick_limit {
            // SAFETY: `field` < 2^16, so the index is < 2^quick_bits, the table's length.
            let e = unsafe { *self.quick.get_unchecked((field >> (16 - self.quick_bits)) as usize) };
            bits.consume(u32::from(e & 31));
            return usize::from(e >> 5);
        }
        self.decode_long(bits, field)
    }

    #[cold]
    fn decode_long(&self, bits: &mut Bits, field: u32) -> usize {
        let mut len = 15;
        for i in self.quick_bits as usize + 1..15 {
            if field < self.limit[i] {
                len = i;
                break;
            }
        }
        bits.consume(len as u32);
        let dist = field.wrapping_sub(self.limit[len - 1]) >> (16 - len as u32);
        let p = self.first[len].wrapping_add(dist) as usize;
        usize::from(self.symbols[if p < self.size { p } else { 0 }])
    }
}

struct Tables {
    main: Huffman,
    dist: Huffman,
    low: Huffman,
    rep: Huffman,
    bit_lengths: Huffman,
    lengths: [u8; MAX_TABLE],
}

struct Block {
    /// Bit position just past the block's last bit.
    end: usize,
    last: bool,
    has_tables: bool,
}

struct Filter {
    start: usize,
    len: usize,
    kind: u32,
    channels: usize,
}

/// Reusable decoder (its tables are a few KB, so keep one per thread).
pub(crate) struct Unpacker {
    t: Tables,
    filters: Vec<Filter>,
    scratch: Vec<u8>,
}

impl Default for Unpacker {
    fn default() -> Self {
        Self {
            t: Tables {
                main: Huffman::new(12),
                dist: Huffman::new(10),
                low: Huffman::new(7),
                rep: Huffman::new(9),
                bit_lengths: Huffman::new(7),
                lengths: [0; MAX_TABLE],
            },
            filters: Vec::new(),
            scratch: Vec::new(),
        }
    }
}

impl Unpacker {
    /// Decodes one file. `input` is the packed data followed by [`INPUT_PADDING`] zero bytes
    /// (`packed_len` excludes them); `out` is `size` + [`OUTPUT_SLACK`] bytes. On success,
    /// `out[..size]` holds the file, filters applied.
    pub(crate) fn unpack(&mut self, input: &[u8], packed_len: usize, out: &mut [u8], size: usize, extra_dist: bool) -> Res<()> {
        assert!(input.len() >= packed_len + INPUT_PADDING && out.len() >= size + OUTPUT_SLACK);
        let mut bits = Bits::new(input, packed_len);
        // Each decode step reads well under 16 bytes; past this the input is corrupt.
        let limit = (packed_len + 16) * 8;
        self.filters.clear();
        let dist_codes = if extra_dist { DCX } else { DCB };

        let mut block = read_block_header(&mut bits, limit)?;
        if !block.has_tables {
            return Err(Corrupt); // the first block must carry tables
        }
        read_tables(&mut bits, &mut self.t, dist_codes, limit)?;

        let base = out.as_mut_ptr();
        let mut pos = 0usize;
        let mut old = [0usize; 4];
        let mut last_len = 0usize;

        'blocks: loop {
            {
                // A shared borrow for the whole block, so the tables stay in registers.
                let t = &self.t;
                let filters = &mut self.filters;
                // One bound per step: the block end, or the input limit if a corrupt block
                // claims to run past the data.
                let end = block.end.min(limit);
                while bits.pos() < end {
                    if pos >= size {
                        break 'blocks; // anything past the file size is discarded anyway
                    }
                    // Literals (most symbols) need at most 15 bits, so refill only when below
                    // 32: that keeps the input load off most literals' dependency chain.
                    if bits.cnt < 32 {
                        bits.refill();
                    }
                    let slot = t.main.decode(&mut bits);
                    if slot < 256 {
                        // SAFETY: pos < size <= out.len().
                        unsafe { *base.add(pos) = slot as u8 };
                        pos += 1;
                        continue;
                    }
                    // Everything else reads more fields; 56+ bits covers a length and the
                    // distance slot (read_distance refills for the long distance bits).
                    bits.refill();
                    if slot >= 262 {
                        let mut len = slot_to_length(&mut bits, slot - 262);
                        let dist = read_distance(&mut bits, t);
                        if dist > 0x100 {
                            len += 1;
                            if dist > 0x2000 {
                                len += 1;
                                if dist > 0x40000 {
                                    len += 1;
                                }
                            }
                        }
                        old = [dist, old[0], old[1], old[2]];
                        last_len = len;
                        // SAFETY: pos < size and len <= MAX_MATCH, so the copy stays inside
                        // OUTPUT_SLACK.
                        unsafe { copy_match(base, pos, dist, len) };
                        pos += len;
                        continue;
                    }
                    if slot == 256 {
                        filters.push(read_filter(&mut bits, pos));
                        continue;
                    }
                    if slot == 257 {
                        if last_len != 0 {
                            // SAFETY: as above; last_len came from slot_to_length.
                            unsafe { copy_match(base, pos, old[0], last_len) };
                            pos += last_len;
                        }
                        continue;
                    }
                    // 258..=261: reuse one of the last four distances.
                    let n = slot - 258;
                    let dist = old[n];
                    for i in (1..=n).rev() {
                        old[i] = old[i - 1];
                    }
                    old[0] = dist;
                    let len_slot = t.rep.decode(&mut bits);
                    let len = slot_to_length(&mut bits, len_slot);
                    last_len = len;
                    // SAFETY: as above.
                    unsafe { copy_match(base, pos, dist, len) };
                    pos += len;
                }
            }
            if bits.pos() < block.end {
                return Err(Corrupt); // stopped at the input limit, not the block end
            }
            if block.last {
                break;
            }
            block = read_block_header(&mut bits, limit)?;
            if block.has_tables {
                read_tables(&mut bits, &mut self.t, dist_codes, limit)?;
            }
        }
        if pos < size {
            return Err(Corrupt);
        }
        let filters = std::mem::take(&mut self.filters);
        let result = filters.iter().try_for_each(|f| apply_filter(&mut out[..size], f, &mut self.scratch));
        self.filters = filters;
        result
    }
}

fn read_block_header(bits: &mut Bits, limit: usize) -> Res<Block> {
    bits.align_to_byte();
    bits.refill();
    if bits.pos() > limit {
        return Err(Corrupt);
    }
    let flags = bits.take(8);
    let byte_count = ((flags >> 3) & 3) + 1;
    if byte_count == 4 {
        return Err(Corrupt);
    }
    let bit_size = (flags & 7) + 1;
    let saved = bits.take(8);
    let mut block_size = 0u32;
    for i in 0..byte_count {
        block_size += bits.take(8) << (i * 8);
    }
    let check = (0x5A ^ flags ^ block_size ^ (block_size >> 8) ^ (block_size >> 16)) & 0xff;
    if check != saved {
        return Err(Corrupt);
    }
    // The block ends `bit_size` bits into its last byte.
    let start = bits.pos() >> 3;
    let end = ((start + block_size as usize) * 8 + bit_size as usize).saturating_sub(8);
    Ok(Block {
        end,
        last: flags & 0x40 != 0,
        has_tables: flags & 0x80 != 0,
    })
}

fn read_tables(bits: &mut Bits, t: &mut Tables, dist_codes: usize, limit: usize) -> Res<()> {
    // Bit lengths of the bit-length code: 4 bits each, 15 escapes a run of zeros.
    let mut bl = [0u8; BC];
    let mut i = 0;
    while i < BC {
        bits.refill();
        let len = bits.take(4) as u8;
        if len == 15 {
            let zeros = bits.take(4) as usize;
            if zeros == 0 {
                bl[i] = 15;
                i += 1;
            } else {
                let end = (i + zeros + 2).min(BC);
                bl[i..end].fill(0);
                i = end;
            }
        } else {
            bl[i] = len;
            i += 1;
        }
    }
    t.bit_lengths.build(&bl);

    let total = NC + dist_codes + LDC + RC;
    let mut i = 0;
    while i < total {
        bits.refill();
        if bits.pos() > limit {
            return Err(Corrupt);
        }
        let n = t.bit_lengths.decode(bits);
        if n < 16 {
            t.lengths[i] = n as u8;
            i += 1;
        } else if n < 18 {
            let count = if n == 16 { bits.take(3) + 3 } else { bits.take(7) + 11 } as usize;
            if i == 0 {
                return Err(Corrupt); // "repeat previous" with nothing before it
            }
            let prev = t.lengths[i - 1];
            let end = (i + count).min(total);
            t.lengths[i..end].fill(prev);
            i = end;
        } else {
            let count = if n == 18 { bits.take(3) + 3 } else { bits.take(7) + 11 } as usize;
            let end = (i + count).min(total);
            t.lengths[i..end].fill(0);
            i = end;
        }
    }
    let lengths = t.lengths;
    t.main.build(&lengths[..NC]);
    t.dist.build(&lengths[NC..NC + dist_codes]);
    t.low.build(&lengths[NC + dist_codes..NC + dist_codes + LDC]);
    t.rep.build(&lengths[NC + dist_codes + LDC..total]);
    Ok(())
}

/// Needs up to 9 valid bits.
#[inline(always)]
fn slot_to_length(bits: &mut Bits, slot: usize) -> usize {
    if slot < 8 {
        return 2 + slot;
    }
    let lbits = (slot / 4 - 1) as u32;
    let len = 2 + ((4 | (slot & 3)) << lbits);
    len + bits.take(lbits) as usize
}

/// Refills as needed: up to 15 + 34 + 15 bits.
#[inline(always)]
fn read_distance(bits: &mut Bits, t: &Tables) -> usize {
    bits.refill();
    let slot = t.dist.decode(bits);
    if slot < 4 {
        return 1 + slot;
    }
    let dbits = (slot / 2 - 1) as u32;
    let mut dist = 1 + ((2 | (slot & 1)) << dbits);
    if dbits >= 4 {
        if dbits > 4 {
            // The high (dbits - 4) bits come raw, the low 4 from their own Huffman table.
            bits.refill();
            let high_bits = dbits - 4;
            let high = if high_bits > 32 {
                let hi = bits.take(high_bits - 32) as u64;
                bits.refill();
                (hi << 32) | u64::from(bits.take(32))
            } else {
                u64::from(bits.take(high_bits))
            };
            dist += (high as usize) << 4;
            bits.refill();
        }
        dist += t.low.decode(bits);
    } else {
        dist += bits.take(dbits) as usize;
    }
    dist
}

/// Copies `len` bytes from `dist` bytes back. Distances reaching before the file start fill
/// zeros (the reference decoder's rule for a non-solid file).
///
/// # Safety
/// `base` must be valid for `pos + len + 16` bytes.
#[inline(always)]
unsafe fn copy_match(base: *mut u8, pos: usize, dist: usize, len: usize) {
    unsafe {
        let dst = base.add(pos);
        if dist > pos || dist == 0 {
            if dist != 0 {
                ptr::write_bytes(dst, 0, len);
            }
            return;
        }
        let src = dst.sub(dist);
        if dist >= 16 {
            // Chunks never overlap (dist >= 16); overshooting up to 15 bytes past `len` is
            // fine, those bytes get written again later.
            let mut i = 0;
            while i < len {
                ptr::copy_nonoverlapping(src.add(i), dst.add(i), 16);
                i += 16;
            }
        } else if dist >= 8 {
            let mut i = 0;
            while i < len {
                ptr::copy_nonoverlapping(src.add(i), dst.add(i), 8);
                i += 8;
            }
        } else {
            for i in 0..len {
                *dst.add(i) = *src.add(i);
            }
        }
    }
}

fn read_filter(bits: &mut Bits, pos: usize) -> Filter {
    let start = read_filter_number(bits) as usize;
    let mut len = read_filter_number(bits) as usize;
    if len > MAX_FILTER_BLOCK {
        len = 0;
    }
    bits.refill();
    let kind = bits.take(3);
    let mut channels = 0;
    if kind == 0 {
        channels = bits.take(5) as usize + 1;
    }
    // The start is relative to the current output position.
    Filter {
        start: pos + start,
        len,
        kind,
        channels,
    }
}

fn read_filter_number(bits: &mut Bits) -> u32 {
    bits.refill();
    let bytes = bits.take(2) + 1;
    let mut v = 0u32;
    for i in 0..bytes {
        v = v.wrapping_add(bits.take(8) << (i * 8));
    }
    v
}

const FILTER_DELTA: u32 = 0;
const FILTER_E8: u32 = 1;
const FILTER_E8E9: u32 = 2;
const FILTER_ARM: u32 = 3;

fn apply_filter(out: &mut [u8], f: &Filter, scratch: &mut Vec<u8>) -> Res<()> {
    if f.len == 0 {
        return Ok(());
    }
    let end = f.start.checked_add(f.len).ok_or(Corrupt)?;
    let data = out.get_mut(f.start..end).ok_or(Corrupt)?;
    // The x86/ARM filters see the block's offset in the file, truncated to 32 bits.
    let file_offset = f.start as u32;
    match f.kind {
        FILTER_E8 | FILTER_E8E9 => {
            const FILE_SIZE: u32 = 0x100_0000;
            let second = if f.kind == FILTER_E8E9 { 0xe9 } else { 0xe8 };
            let mut cur = 0usize;
            while cur + 4 < data.len() {
                let b = data[cur];
                cur += 1;
                if b == 0xe8 || b == second {
                    let offset = (cur as u32).wrapping_add(file_offset) % FILE_SIZE;
                    let addr = u32::from_le_bytes(data[cur..cur + 4].try_into().unwrap());
                    if addr & 0x8000_0000 != 0 {
                        if addr.wrapping_add(offset) & 0x8000_0000 == 0 {
                            data[cur..cur + 4].copy_from_slice(&addr.wrapping_add(FILE_SIZE).to_le_bytes());
                        }
                    } else if addr.wrapping_sub(FILE_SIZE) & 0x8000_0000 != 0 {
                        data[cur..cur + 4].copy_from_slice(&addr.wrapping_sub(offset).to_le_bytes());
                    }
                    cur += 4;
                }
            }
        }
        FILTER_ARM => {
            let mut cur = 0usize;
            while cur + 3 < data.len() {
                if data[cur + 3] == 0xeb {
                    let mut off = u32::from(data[cur]) | u32::from(data[cur + 1]) << 8 | u32::from(data[cur + 2]) << 16;
                    off = off.wrapping_sub(file_offset.wrapping_add(cur as u32) / 4);
                    data[cur] = off as u8;
                    data[cur + 1] = (off >> 8) as u8;
                    data[cur + 2] = (off >> 16) as u8;
                }
                cur += 4;
            }
        }
        FILTER_DELTA => {
            // Bytes of each channel are stored together; interleave them back while undoing
            // the byte-wise differences.
            let channels = f.channels.max(1);
            scratch.clear();
            scratch.resize(data.len(), 0);
            let mut src = 0;
            for ch in 0..channels {
                let mut prev = 0u8;
                let mut dst = ch;
                while dst < data.len() {
                    prev = prev.wrapping_sub(data[src]);
                    scratch[dst] = prev;
                    src += 1;
                    dst += channels;
                }
            }
            data.copy_from_slice(scratch);
        }
        _ => return Err(Corrupt),
    }
    Ok(())
}
