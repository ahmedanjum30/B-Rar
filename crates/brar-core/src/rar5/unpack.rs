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
/// Lookup bits of the main table (12 measured best: bigger tables cost more to rebuild every
/// ~20 KB than the longer codes they catch).
const MAIN_QUICK: u32 = 12;

#[derive(Debug)]
pub(crate) struct Corrupt;

type Res<T> = std::result::Result<T, Corrupt>;

/// MSB-first bit reader. The next bits sit left-aligned in `buf`; `cnt` of them are valid.
/// `Copy`, so the hot loop can work on a local copy the compiler keeps in registers: a reader
/// passed by `&mut` to any out-of-line function would be kept in memory instead, putting a
/// store and a reload on every symbol's dependency chain.
#[derive(Clone, Copy)]
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

    /// Consumes the code length held in the low 6 bits of a table entry. The shift uses the
    /// whole entry as its count: x86 masks shift counts to 6 bits, so no masking instruction
    /// lands on the decode's dependency chain.
    #[inline(always)]
    fn consume_entry(&mut self, e: u16) {
        self.buf = self.buf.wrapping_shl(u32::from(e));
        self.cnt -= u32::from(e & 63);
    }

    #[inline(always)]
    fn take(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.consume(n);
        v
    }

    /// The next `n` (0..=32) bits, consumed; 0 reads nothing.
    #[inline(always)]
    fn take_upto(&mut self, n: u32) -> u32 {
        // (buf >> 1) >> (63 - n) is buf >> (64 - n) without the undefined shift by 64.
        let v = ((self.buf >> 1) >> (63 - n)) as u32;
        self.buf = self.buf.wrapping_shl(n);
        self.cnt -= n;
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
    /// `symbol << 6 | code length` for every `quick_bits`-bit prefix; 0 (length 0) for the
    /// prefixes of longer codes, which [`Huffman::decode`] hands to the slow path. The length
    /// sits in the low 6 bits so the entry itself can be the shift count (see `consume_entry`).
    /// Unused by the main table, which fills `main` instead.
    quick: Box<[u16]>,
    /// The main table's entries ([`main_entry`]); empty for the other tables.
    main: Box<[u32]>,
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
            main: Box::new([]),
        }
    }

    /// The main table: 32-bit entries carrying what the hot loop needs ([`main_entry`]).
    fn new_main() -> Self {
        let mut h = Self::new(MAIN_QUICK);
        h.quick = Box::new([]);
        h.main = vec![0; 1 << MAIN_QUICK].into_boxed_slice();
        h
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
                if self.main.is_empty() {
                    for k in 0..n as usize {
                        let entry = (self.symbols[first + k] << 6) | len as u16;
                        self.quick[at..at + span].fill(entry);
                        at += span;
                    }
                } else {
                    for k in 0..n as usize {
                        let entry = main_entry(usize::from(self.symbols[first + k]), len as u32);
                        self.main[at..at + span].fill(entry);
                        at += span;
                    }
                }
            }
            // Prefixes of codes longer than quick_bits.
            if self.main.is_empty() {
                self.quick[at..].fill(0);
            } else {
                self.main[at..].fill(0);
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
            // At or past quick_limit the reference decoder takes its slow path: mark it.
            let hit = field < self.quick_limit;
            if self.main.is_empty() {
                self.quick[code as usize] = if hit { (symbol << 6) | len as u16 } else { 0 };
            } else {
                self.main[code as usize] = if hit { main_entry(usize::from(symbol), len as u32) } else { 0 };
            }
        }
    }

    /// The main table's next symbol as an entry ([`main_entry`]), bits consumed. Needs at
    /// least 15 valid bits.
    #[inline(always)]
    fn decode_main(&self, table: *const u32, bits: &mut Bits) -> u32 {
        // SAFETY: `table` is this table's `main` array, 2^MAIN_QUICK entries; the index is
        // below that.
        let e = unsafe { *table.add((bits.buf >> (64 - MAIN_QUICK)) as usize) };
        if e & 0xff != 0 {
            bits.buf = bits.buf.wrapping_shl(e); // the shift masks the count to the low 6 bits
            bits.cnt -= e & 0xff;
            return e;
        }
        let (len, symbol) = self.decode_long(bits.peek(16) & 0xfffe);
        bits.consume(len);
        main_entry(symbol, len)
    }

    /// Needs at least 15 valid bits.
    #[inline(always)]
    fn decode(&self, bits: &mut Bits) -> usize {
        // SAFETY: the index is < 2^quick_bits, the table's length.
        let e = unsafe { *self.quick.get_unchecked((bits.buf >> (64 - self.quick_bits)) as usize) };
        self.finish(bits, e)
    }

    /// Consumes a quick-table hit, or decodes a longer code (length 0 marks one).
    #[inline(always)]
    fn finish(&self, bits: &mut Bits, e: u16) -> usize {
        if e & 63 != 0 {
            bits.consume_entry(e);
            return usize::from(e >> 6);
        }
        let (len, symbol) = self.decode_long(bits.peek(16) & 0xfffe);
        bits.consume(len);
        symbol
    }

    /// A code longer than the quick table: its length and symbol, from the next 16 bits.
    /// Takes the bits by value (not the reader), so the reader isn't forced into memory.
    #[cold]
    fn decode_long(&self, field: u32) -> (u32, usize) {
        let mut len = 15;
        for i in self.quick_bits as usize + 1..15 {
            if field < self.limit[i] {
                len = i;
                break;
            }
        }
        let dist = field.wrapping_sub(self.limit[len - 1]) >> (16 - len as u32);
        let p = self.first[len].wrapping_add(dist) as usize;
        (len as u32, usize::from(self.symbols[if p < self.size { p } else { 0 }]))
    }
}

/// Main-table entry flags: bits 0..8 hold the code length (0: a code longer than the quick
/// table, decoded by the slow path), bits 16.. the payload.
const E_LITERAL: u32 = 1 << 8;
const E_MATCH: u32 = 1 << 9;

/// What the hot loop needs about a main-alphabet symbol, in one word: a literal's byte, a
/// length slot's base length and extra-bit count, or the symbol (256..=261) itself.
fn main_entry(symbol: usize, code_len: u32) -> u32 {
    match symbol {
        0..=255 => code_len | E_LITERAL | (symbol as u32) << 16,
        262.. => {
            let slot = (symbol - 262) as u32;
            let (base, extra) = if slot < 8 {
                (2 + slot, 0)
            } else {
                (2 + ((4 | (slot & 3)) << (slot / 4 - 1)), slot / 4 - 1)
            };
            code_len | E_MATCH | extra << 10 | base << 16
        }
        _ => code_len | (symbol as u32) << 16,
    }
}

/// Distance slot -> (base distance, extra bits). Slots 0..=3 are the distances 1..=4.
const fn dist_slots() -> ([u64; DCX], [u8; DCX]) {
    let (mut base, mut extra) = ([0u64; DCX], [0u8; DCX]);
    let mut slot = 0;
    while slot < DCX {
        if slot < 4 {
            base[slot] = 1 + slot as u64;
        } else {
            let bits = (slot / 2 - 1) as u32;
            base[slot] = 1 + ((2 | (slot as u64 & 1)) << bits);
            extra[slot] = bits as u8;
        }
        slot += 1;
    }
    (base, extra)
}
const DIST_SLOTS: ([u64; DCX], [u8; DCX]) = dist_slots();

/// What the hot loop carries from block to block: the last four distances and the last length.
#[derive(Clone, Copy, Default)]
struct Recent {
    d0: usize,
    d1: usize,
    d2: usize,
    d3: usize,
    len: usize,
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
                main: Huffman::new_main(),
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
        let mut recent = Recent::default();

        loop {
            let end = block.end.min(limit);
            let done;
            (bits, pos, recent, done) = decode_block(&self.t, &mut self.filters, bits, base, pos, size, end, recent);
            if done {
                break; // reached the file size: anything after it is discarded anyway
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

/// Decodes one block's symbols, from bit position `bits.pos()` up to `end`, into
/// `out[pos..]`. Everything it works on comes in and goes out by value, so the compiler keeps
/// it in registers (a reader passed by `&mut` to out-of-line code would live in memory).
/// Returns `done` once the output reached `size`.
///
/// Away from the block end and the file end (the "fast" region) it decodes two literals per
/// refill check: 32+ valid bits cover two 15-bit codes.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn decode_block<'a>(
    t: &Tables,
    filters: &mut Vec<Filter>,
    mut bits: Bits<'a>,
    out: *mut u8,
    mut pos: usize,
    size: usize,
    end: usize,
    mut r: Recent,
) -> (Bits<'a>, usize, Recent, bool) {
    let main = t.main.main.as_ptr();
    let fast_end = end.saturating_sub(32);
    let fast_size = size.saturating_sub(2);
    loop {
        let e;
        if bits.pos() < fast_end && pos < fast_size {
            if bits.cnt < 32 {
                bits.refill();
            }
            let e1 = t.main.decode_main(main, &mut bits);
            if e1 & E_LITERAL != 0 {
                // SAFETY: pos + 2 <= size <= out.len() for both stores.
                unsafe { *out.add(pos) = (e1 >> 16) as u8 };
                let e2 = t.main.decode_main(main, &mut bits);
                if e2 & E_LITERAL != 0 {
                    unsafe { *out.add(pos + 1) = (e2 >> 16) as u8 };
                    pos += 2;
                    continue;
                }
                pos += 1;
                e = e2;
            } else {
                e = e1;
            }
        } else {
            if bits.pos() >= end {
                return (bits, pos, r, false);
            }
            if pos >= size {
                return (bits, pos, r, true);
            }
            if bits.cnt < 32 {
                bits.refill();
            }
            let e1 = t.main.decode_main(main, &mut bits);
            if e1 & E_LITERAL != 0 {
                // SAFETY: pos < size <= out.len().
                unsafe { *out.add(pos) = (e1 >> 16) as u8 };
                pos += 1;
                continue;
            }
            e = e1;
        }
        // Not a literal. 56+ bits after this refill cover a length's extra bits (9) and the
        // distance code (15); read_dist refills for the rest.
        bits.refill();
        if e & E_MATCH != 0 {
            let mut len = (e >> 16) as usize + bits.take_upto((e >> 10) & 15) as usize;
            // 56+ bits again: the whole distance of any RAR5 file fits (code 15 + 26 raw
            // bits + low code 15), so read_dist only refills for RAR7's longer distances.
            bits.refill();
            let dist = read_dist(&mut bits, t);
            // Longer distances add to the length.
            len += usize::from(dist > 0x100) + usize::from(dist > 0x2000) + usize::from(dist > 0x40000);
            r = Recent {
                d0: dist,
                d1: r.d0,
                d2: r.d1,
                d3: r.d2,
                len,
            };
            // SAFETY: pos < size and len <= MAX_MATCH, so the copy stays inside OUTPUT_SLACK.
            unsafe { copy_match(out, pos, dist, len) };
            pos += len;
            continue;
        }
        match e >> 16 {
            256 => {
                // Through a copy: passing `bits` itself by `&mut` would keep it in memory.
                let mut b = bits;
                filters.push(read_filter(&mut b, pos));
                bits = b;
            }
            257 => {
                if r.len != 0 {
                    // SAFETY: as above; r.len came from a decoded length.
                    unsafe { copy_match(out, pos, r.d0, r.len) };
                    pos += r.len;
                }
            }
            n => {
                // 258..=261: reuse one of the last four distances, moving it to the front.
                let dist = match n {
                    258 => r.d0,
                    259 => {
                        let d = r.d1;
                        r.d1 = r.d0;
                        d
                    }
                    260 => {
                        let d = r.d2;
                        (r.d2, r.d1) = (r.d1, r.d0);
                        d
                    }
                    _ => {
                        let d = r.d3;
                        (r.d3, r.d2, r.d1) = (r.d2, r.d1, r.d0);
                        d
                    }
                };
                r.d0 = dist;
                let len_slot = t.rep.decode(&mut bits);
                let len = slot_to_length(&mut bits, len_slot);
                r.len = len;
                // SAFETY: as above.
                unsafe { copy_match(out, pos, dist, len) };
                pos += len;
            }
        }
    }
}

/// A distance: its slot from the distance table, then extra bits (the high ones raw, the low
/// four from their own table). Needs 56 valid bits; refills only past 26 raw bits (RAR7).
#[inline(always)]
fn read_dist(bits: &mut Bits, t: &Tables) -> usize {
    let slot = t.dist.decode(bits);
    let (base, extra) = (DIST_SLOTS.0[slot], u32::from(DIST_SLOTS.1[slot]));
    if extra < 4 {
        return (base + u64::from(bits.take_upto(extra))) as usize;
    }
    let high_bits = extra - 4;
    let high = if high_bits <= 26 {
        u64::from(bits.take_upto(high_bits))
    } else {
        // RAR7 only: up to 36 raw bits.
        bits.refill();
        if high_bits > 32 {
            let hi = u64::from(bits.take(high_bits - 32));
            bits.refill();
            (hi << 32) | u64::from(bits.take(32))
        } else {
            u64::from(bits.take(high_bits))
        }
    };
    if high_bits > 26 {
        bits.refill();
    }
    (base + (high << 4) + t.low.decode(bits) as u64) as usize
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
