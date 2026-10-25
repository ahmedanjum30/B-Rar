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

// Table entries (u32): bits 0..6 = bits to consume, the code and its extra bits together;
// bits 8..12 = the code's own bits (the extra bits sit above them); bit 12 = literal;
// bit 13 = end of block; bit 14 = subtable pointer; bit 15 = invalid code; bits 16.. = the
// literal byte, the length or distance base, or the subtable's first index. A subtable
// pointer consumes the main table's bits and keeps the subtable's index bits in 8..12.
// Bits 6..8 stay zero, so an entry can be a shift count as it is.
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

/// A table's entries: its part of the one allocation [`Tables`] holds.
struct Entries {
    ptr: *mut u32,
    len: usize,
}

impl std::ops::Deref for Entries {
    type Target = [u32];
    fn deref(&self) -> &[u32] {
        // SAFETY: a part of Tables' allocation, which outlives its tables; parts don't overlap.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl std::ops::DerefMut for Entries {
    fn deref_mut(&mut self) -> &mut [u32] {
        // SAFETY: as deref; &mut self makes it the only access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

/// A Huffman code's decoding table: entries for `bits`-bit lookups, then subtables.
pub(crate) struct Table {
    entries: Entries,
    bits: u32,
}

impl Table {
    /// Builds the table for code lengths `lens` (0 = unused); `value(symbol)` gives the entry's
    /// flags and value (bits 12..) and its extra bit count (bits 0..8). Over-subscribed codes
    /// are rejected; incomplete ones are allowed (Deflate permits a lone distance code), their
    /// gaps decode as invalid.
    #[inline(always)]
    fn build(&mut self, lens: &[u8], value: impl Fn(usize) -> u32) -> Res<bool> {
        // The lengths in four quarters (zero-padded), each with its own counters: counting
        // and sorting run as four independent chains (runs of equal lengths would otherwise
        // make each update wait for the previous one's store).
        let mut padded = [0u8; 320];
        padded[..lens.len()].copy_from_slice(lens);
        let q = lens.len().div_ceil(4);
        let mut counts = [[0u16; 16]; 4];
        for j in 0..q {
            for (k, c) in counts.iter_mut().enumerate() {
                c[usize::from(padded[k * q + j] & 15)] += 1;
            }
        }
        let mut count = [0u32; 16];
        for (l, c) in count.iter_mut().enumerate().skip(1) {
            *c = u32::from(counts[0][l] + counts[1][l] + counts[2][l] + counts[3][l]);
        }
        let mut left = 1i32;
        for c in &count[1..] {
            left = (left << 1) - *c as i32;
            if left < 0 {
                return Err(Bad); // over-subscribed
            }
        }
        // A complete code covers every entry; only an incomplete one leaves gaps to mark.
        let complete = left == 0;
        if complete {
            self.build_complete(&padded, q, &counts, &count, value);
            return Ok(true);
        }
        // First canonical code of each length.
        let mut next = [0u32; 16];
        let mut code = 0u32;
        for l in 1..16 {
            code = (code + count[l - 1]) << 1;
            next[l] = code;
        }
        let bits = self.bits;
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
            let entry = |code: u32| compose(v, code);
            if l <= bits {
                let e = entry(l);
                let mut i = rev as usize;
                while i < 1 << bits {
                    self.entries[i] = e;
                    i += 1 << l;
                }
            } else {
                let p = (rev & ((1 << bits) - 1)) as usize;
                let sb = u32::from(sub_bits[p]);
                let e = entry(l - bits);
                let mut i = (rev >> bits) as usize;
                while i < 1 << sb {
                    self.entries[sub_start[p] as usize + i] = e;
                    i += 1 << (l - bits);
                }
            }
        }
        Ok(complete)
    }

    /// [`Table::build`] for a complete code (what encoders write, so nearly every table): codes
    /// in canonical order, shortest first, each written once into a table only as big as its
    /// length needs, the table doubled (copied onto its upper half) at each longer length.
    /// Codes longer than the main table's bits go to subtables, one per shared prefix.
    /// `padded` and `counts`: the lengths in quarters of `q`, and each quarter's counts.
    #[inline(always)]
    fn build_complete(&mut self, padded: &[u8; 320], q: usize, counts: &[[u16; 16]; 4], count: &[u32; 16], value: impl Fn(usize) -> u32) {
        // Symbols sorted by code length (stable: canonical order within a length).
        let mut offset = [0u16; 17];
        for l in 1..16 {
            offset[l + 1] = offset[l] + count[l] as u16;
        }
        // Each quarter's place for its symbols of each length, after the earlier quarters';
        // unused symbols (length 0) go past the end, without a branch.
        let mut place = [[0u16; 16]; 4];
        for l in 1..16 {
            let mut o = offset[l];
            for k in 0..4 {
                place[k][l] = o;
                o += counts[k][l];
            }
        }
        for (k, p) in place.iter_mut().enumerate() {
            p[0] = 288 + 80 * k as u16;
        }
        let mut sorted = [0u16; 288 + 4 * 80];
        for j in 0..q {
            for (k, p) in place.iter_mut().enumerate() {
                let sym = k * q + j;
                let o = &mut p[usize::from(padded[sym] & 15)];
                sorted[usize::from(*o)] = sym as u16;
                *o += 1;
            }
        }
        let entry = |sym: u16, code: u32| compose(value(usize::from(sym)), code);
        let bits = self.bits as usize;

        let t = &mut self.entries;
        let mut syms = sorted.iter();
        // The code being assigned, bit-reversed (as the table indexes it): the next one in
        // canonical order is a reversed increment.
        let mut code = 0usize;
        let mut len = 1;
        while count[len] == 0 {
            len += 1;
        }
        let mut left = count[len];
        let mut end = 1usize << len;
        while len <= bits {
            loop {
                t[code] = entry(*syms.next().unwrap(), len as u32);
                if code == end - 1 {
                    // The last code (all ones): the table is full at this length.
                    while end < 1 << bits {
                        t.copy_within(..end, end);
                        end <<= 1;
                    }
                    return;
                }
                let bit = 1 << (usize::BITS - 1 - (code ^ (end - 1)).leading_zeros());
                code = (code & (bit - 1)) | bit;
                left -= 1;
                if left == 0 {
                    break;
                }
            }
            loop {
                len += 1;
                if len <= bits {
                    t.copy_within(..end, end);
                    end <<= 1;
                }
                left = count[len];
                if left != 0 {
                    break;
                }
            }
        }
        // Longer codes: the main table is complete up to here; subtables follow it.
        let mask = (1 << bits) - 1;
        let mut end = 1usize << bits;
        let (mut prefix, mut start) = (usize::MAX, 0);
        loop {
            if code & mask != prefix {
                // A new prefix: its subtable is as big as its longest code needs.
                prefix = code & mask;
                start = end;
                let mut sub = len - bits;
                let mut used = left;
                while used < 1 << sub {
                    sub += 1;
                    used = (used << 1) + count[bits + sub];
                }
                end = start + (1 << sub);
                t[prefix] = SUBTABLE | (start as u32) << 16 | (sub as u32) << 8 | bits as u32;
            }
            let e = entry(*syms.next().unwrap(), (len - bits) as u32);
            let mut i = start + (code >> bits);
            while i < end {
                t[i] = e;
                i += 1 << (len - bits);
            }
            if code == (1 << len) - 1 {
                return;
            }
            let bit = 1 << (usize::BITS - 1 - (code ^ ((1 << len) - 1)).leading_zeros());
            code = (code & (bit - 1)) | bit;
            left -= 1;
            while left == 0 {
                len += 1;
                left = count[len];
            }
        }
    }

    /// The next code's entry and its value (the literal byte, the length or the distance, extra
    /// bits included), all its bits consumed.
    #[inline(always)]
    pub(super) fn decode(&self, bits: &mut Bits) -> (u32, usize) {
        // SAFETY: the index is below 2^self.bits, the main table's size.
        let mut e = unsafe { *self.entries.get_unchecked(bits.peek(self.bits) as usize) };
        if e & SUBTABLE != 0 {
            bits.consume(e & 0x3f);
            let i = (e >> 16) + bits.peek((e >> 8) & 0xf);
            // SAFETY: subtables were sized for every index of their bit count.
            e = unsafe { *self.entries.get_unchecked(i as usize) };
        }
        let saved = bits.buf;
        bits.consume(e & 0x3f);
        (e, value(e, saved))
    }
}

/// Whether literal codes take most of the code space: [`LITERAL_HEAVY`] of 2^15. Worked out
/// from the other codes' share (`other_lens`: end of block and lengths, ~30 symbols, not
/// 256), the code being complete.
#[inline(always)]
fn literal_heavy(other_lens: &[u8]) -> bool {
    let other: u32 = other_lens.iter().map(|&l| if l == 0 { 0 } else { (1u32 << 15) >> l }).sum();
    (1u32 << 15).saturating_sub(other) >= LITERAL_HEAVY
}

/// The share of code space (of 2^15) from which a block counts as literal-heavy.
const LITERAL_HEAVY: u32 = 26_000;

/// The entry for a symbol whose code has `code` bits in its table: `v` from the value
/// function (flags and value, extra bit count in the low byte).
#[inline(always)]
fn compose(v: u32, code: u32) -> u32 {
    // Arithmetic, not a branch on the kind: v + code (bit count, after the extra bits'), plus
    // code << 8 unless a literal.
    let not_literal = ((v >> 12) & 1) ^ 1;
    v + code * (1 + (not_literal << 8))
}

/// An entry's value (not for literals, whose byte is `e >> 16`): its base plus the extra bits that followed its code in `saved` (the bit
/// buffer before the entry was consumed).
#[inline(always)]
fn value(e: u32, saved: u64) -> usize {
    let extra = (saved & ((1u64 << (e & 0x3f)) - 1)) >> ((e >> 8) & 0xf);
    (e >> 16) as usize + extra as usize
}

/// Each litlen symbol's flags, value and extra bit count, for [`Table::build`] (a lookup:
/// symbols come sorted by code length, so a `match` on them would mispredict).
const LITLEN_VALUES: [u32; 288] = {
    let mut v = [INVALID; 288];
    let mut s = 0;
    while s < 286 {
        v[s] = match s {
            0..=255 => LITERAL | (s as u32) << 16,
            256 => END,
            _ => (LENGTH_BASE[s - 257] as u32) << 16 | LENGTH_EXTRA[s - 257] as u32,
        };
        s += 1;
    }
    v
};

const DIST_VALUES: [u32; 32] = {
    let mut v = [INVALID; 32];
    let mut s = 0;
    while s < 30 {
        v[s] = (DIST_BASE[s] as u32) << 16 | DIST_EXTRA[s] as u32;
        s += 1;
    }
    v
};

/// The code-length code's symbols: the length to write (bits 24..), how many times (bits
/// 16..24, plus the extra bits), PREV_LENGTH for "the previous length" (16).
const PRE_VALUES: [u32; 19] = {
    let mut v = [0; 19];
    let mut s = 0;
    while s < 16 {
        v[s] = (s as u32) << 24 | 1 << 16;
        s += 1;
    }
    v[16] = PREV_LENGTH | 3 << 16 | 2;
    v[17] = 3 << 16 | 3;
    v[18] = 11 << 16 | 7;
    v
};
const PREV_LENGTH: u32 = END;

fn litlen_value(sym: usize) -> u32 {
    LITLEN_VALUES[sym]
}

fn dist_value(sym: usize) -> u32 {
    DIST_VALUES[sym]
}

/// The two tables of a block, and the code-length code's (built for every dynamic block).
/// One allocation for all three, the distance table right after the litlen table: the
/// decoding loop reaches both from one register.
pub(crate) struct Tables {
    pub(super) litlen: Table,
    pub(super) dist: Table,
    pre: Table,
    /// The tables hold the fixed codes: another fixed block needs no build.
    fixed: bool,
    /// Most symbols the litlen code describes are literals (by its code lengths: a code of
    /// L bits stands for a share of about 2^-L), which picks the decoding loop.
    pub(super) literal_heavy: bool,
    store: *mut [u32],
}

/// Where the distance table starts, from the litlen table's start.
pub(super) const DIST_AT: usize = LITLEN_SIZE;
const PRE_SIZE: usize = 1 << 7;

// SAFETY: Tables owns its allocation; nothing else points into it.
unsafe impl Send for Tables {}

impl Default for Tables {
    fn default() -> Self {
        let store = Box::into_raw(vec![INVALID; LITLEN_SIZE + DIST_SIZE + PRE_SIZE].into_boxed_slice());
        let base = store as *mut u32;
        // SAFETY: the parts are within the allocation and don't overlap.
        let part = |at: usize, len: usize| Entries {
            ptr: unsafe { base.add(at) },
            len,
        };
        Self {
            litlen: Table {
                entries: part(0, LITLEN_SIZE),
                bits: LITLEN_BITS,
            },
            dist: Table {
                entries: part(DIST_AT, DIST_SIZE),
                bits: DIST_BITS,
            },
            pre: Table {
                entries: part(DIST_AT + DIST_SIZE, PRE_SIZE),
                bits: 7,
            },
            fixed: false,
            literal_heavy: false,
            store,
        }
    }
}

impl Drop for Tables {
    fn drop(&mut self) {
        // SAFETY: from Box::into_raw in default, dropped once.
        drop(unsafe { Box::from_raw(self.store) });
    }
}

impl Tables {
    pub(super) fn fixed(&mut self) -> Res<()> {
        if self.fixed {
            return Ok(());
        }
        let mut lens = [0u8; 288];
        lens[..144].fill(8);
        lens[144..256].fill(9);
        lens[256..280].fill(7);
        lens[280..].fill(8);
        self.litlen.build(&lens, litlen_value)?;
        self.dist.build(&[5; 32], dist_value)?;
        self.literal_heavy = literal_heavy(&lens[256..]);
        self.fixed = true;
        Ok(())
    }

    /// A dynamic block's header: the code-length code, then both codes' lengths. `strict`
    /// (for finding block starts by trial) also requires complete codes, as encoders write.
    pub(super) fn dynamic(&mut self, bits: &mut Bits, strict: bool) -> Res<()> {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("bmi2") {
            // SAFETY: the CPU has BMI2.
            return unsafe { self.dynamic_bmi2(bits, strict) };
        }
        self.dynamic_any(bits, strict)
    }

    /// [`Tables::dynamic`] compiled for BMI2 (variable shifts and masks in one instruction).
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "bmi2")]
    unsafe fn dynamic_bmi2(&mut self, bits: &mut Bits, strict: bool) -> Res<()> {
        self.dynamic_any(bits, strict)
    }

    #[inline(always)]
    fn dynamic_any(&mut self, bits: &mut Bits, strict: bool) -> Res<()> {
        // Whatever happens below, the tables no longer hold the fixed codes.
        self.fixed = false;
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
        if !pre.build(&clens, |s| PRE_VALUES[s])? && strict {
            return Err(Bad);
        }
        // Every symbol the same way, no branch on which: 16 copies of its length stored (runs
        // over 16 are zeros, which the array already holds past the last store), then the
        // position moved by its count.
        let mut lens = [0u8; 286 + 30 + 16];
        let n = nlit + ndist;
        let (mut i, mut prev) = (0, 0u32);
        // Three symbols per refill (14 bits each at most: a 7-bit code and 7 extra bits).
        'symbols: while i < n {
            bits.refill();
            for _ in 0..3 {
                // SAFETY: masked to the precode table's 7 bits; its codes are 7 bits at most.
                let e = unsafe { *pre.entries.get_unchecked(bits.peek(7) as usize) };
                let saved = bits.buf;
                bits.consume(e & 0x3f);
                if e & INVALID != 0 || (e & PREV_LENGTH != 0 && i == 0) {
                    return Err(Bad);
                }
                let extra = (saved & ((1u64 << (e & 0x3f)) - 1)) >> ((e >> 8) & 0xf);
                let count = ((e >> 16) & 0xff) as usize + extra as usize;
                let len = if e & PREV_LENGTH != 0 { prev } else { e >> 24 };
                // SAFETY: i < n, and lens has 16 bytes past any n.
                // (Two 8-byte stores: a 16-byte splat by multiplication would take a 128-bit
                // multiply.)
                let splat = u64::from(len) * 0x0101_0101_0101_0101;
                unsafe {
                    let at = lens.as_mut_ptr().add(i) as *mut u64;
                    at.write_unaligned(splat);
                    at.add(1).write_unaligned(splat);
                }
                prev = len;
                i += count;
                if i >= n {
                    break 'symbols;
                }
            }
        }
        if i > n {
            return Err(Bad);
        }
        if lens[256] == 0 {
            return Err(Bad); // no end-of-block code
        }
        let lit_complete = self.litlen.build(&lens[..nlit], litlen_value)?;
        self.literal_heavy = literal_heavy(&lens[256..nlit]);
        let dist_complete = self.dist.build(&lens[nlit..n], dist_value)?;
        // A lone distance code (length 1) is the one incomplete code encoders write.
        let lone_dist = lens[nlit..n].iter().filter(|&&l| l != 0).count() <= 1;
        if strict && (!lit_complete || !(dist_complete || lone_dist)) {
            return Err(Bad);
        }
        Ok(())
    }
}

/// Copies `len` (3..=258) bytes from `dist` back to `out[pos..]`; may write up to 48 bytes
/// past the match's end. Needs dist <= pos and [`OUTPUT_SLACK`] bytes of room after.
#[inline(always)]
pub(super) unsafe fn copy_match(out: *mut u8, pos: usize, dist: usize, len: usize) {
    // SAFETY: the caller guarantees 1 <= dist <= pos and len + OUTPUT_SLACK bytes of room.
    unsafe {
        let dst = out.add(pos);
        let src = dst.sub(dist);
        if dist >= 16 {
            // 16 bytes at a time (whole vectors: nothing left for the compiler to vectorize,
            // with the alias checks and registers that would take). Most matches are short:
            // 48 bytes without a loop branch.
            let chunk = |i: usize| (dst.add(i) as *mut u128).write_unaligned((src.add(i) as *const u128).read_unaligned());
            chunk(0);
            chunk(16);
            chunk(32);
            let mut i = 48;
            while i < len {
                chunk(i);
                i += 16;
            }
        } else if dist >= 8 {
            let word = |i: usize| (dst.add(i) as *mut u64).write_unaligned((src.add(i) as *const u64).read_unaligned());
            let mut i = 0;
            while i < len {
                word(i);
                i += 8;
            }
        } else if dist == 1 {
            let v = u64::from(*src) * 0x0101_0101_0101_0101;
            let mut i = 0;
            while i < len {
                (dst.add(i) as *mut u64).write_unaligned(v);
                i += 8;
            }
        } else {
            // A short repeating pattern: byte by byte until it spans a whole number of periods
            // that's at least 8 bytes, then 8 at a time from that far back.
            // dist * ceil(8 / dist), from a table (a division costs more than the copy).
            const STEP: [u8; 8] = [0, 8, 8, 9, 8, 10, 12, 14];
            let step = usize::from(STEP[dist]);
            let head = step.min(len);
            for i in 0..head {
                *dst.add(i) = *src.add(i);
            }
            let mut i = head;
            while i < len {
                (dst.add(i) as *mut u64).write_unaligned((dst.add(i - step) as *const u64).read_unaligned());
                i += 8;
            }
        }
    }
}
/// Decodes a whole Deflate stream: `input` is the stream (`len` bytes) followed by
/// [`INPUT_PADDING`] zeros; the output must be exactly `size` bytes, written to `out`, which
/// has [`OUTPUT_SLACK`] more. Returns the bit position after the final block.
pub(crate) fn inflate(t: &mut Tables, input: &[u8], len: usize, out: &mut [u8], size: usize) -> Res<usize> {
    inflate_recording(t, input, len, out, size, None)
}

/// [`inflate`], also listing where each block starts: (bit position, output position).
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
                (bits, pos) = block(t, bits, out.as_mut_ptr(), pos, size)?;
            }
            2 => {
                t.dynamic(&mut bits, false)?;
                (bits, pos) = block(t, bits, out.as_mut_ptr(), pos, size)?;
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
/// go out by value, so they stay in registers. Every code either writes output or ends the
/// block, so damaged input (or the zero padding past it) stops at `size` at the latest.
#[inline(never)]
fn block<'a>(t: &Tables, mut bits: Bits<'a>, out: *mut u8, mut pos: usize, size: usize) -> Res<(Bits<'a>, usize)> {
    // SAFETY: out has size + OUTPUT_SLACK bytes; fast stops 258 short of size.
    if let Some(r) = unsafe { fast(t, &mut bits, &mut Bytes(out), &mut pos, size.saturating_sub(258)) } {
        return r.map(|()| (bits, pos));
    }
    // Near the end of the output: every store checked.
    let (lit, dist) = (&t.litlen, &t.dist);
    loop {
        bits.refill();
        let (e, v) = lit.decode(&mut bits);
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
            return if e & INVALID != 0 { Err(Bad) } else { Ok((bits, pos)) };
        }
        let (d, back) = dist.decode(&mut bits);
        if d & INVALID != 0 || back > pos || pos + v > size {
            return Err(Bad);
        }
        // SAFETY: back <= pos, pos + v <= size, and out has OUTPUT_SLACK after size.
        unsafe { copy_match(out, pos, back, v) };
        pos += v;
    }
}

/// Decodes a Huffman-coded block's symbols into `out` from `pos` while `pos` is below
/// `fast_end`: Some at the block's end (or damage), None once `pos` reaches `fast_end` or the
/// input nears its end (the caller goes on with checked decoding). Nothing to do (None) if it
/// starts there.
///
/// # Safety
/// `out` must have room for `fast_end + 258 +` [`OUTPUT_SLACK`] bytes; matches copy from
/// anywhere in `out[..pos]`.
/// Where [`fast`] puts what it decodes: through a pointer to the next element (one register
/// for the output position, the buffer's start needed only for the distance check).
pub(super) trait Output {
    type Elem;
    /// The buffer's start.
    fn base(&self) -> *mut Self::Elem;
    /// Stores a literal at `at`.
    ///
    /// # Safety
    /// `at` is in bounds.
    unsafe fn literal(at: *mut Self::Elem, byte: u8);
    /// Copies a match of `n` bytes from `back` before `at`; false if that's out of reach.
    ///
    /// # Safety
    /// `at + n` and the copy's overshoot are in bounds.
    unsafe fn copy(&mut self, at: *mut Self::Elem, back: usize, n: usize) -> bool;
}

/// Plain bytes, copying from earlier in the same buffer.
pub(super) struct Bytes(pub *mut u8);

impl Output for Bytes {
    type Elem = u8;

    #[inline(always)]
    fn base(&self) -> *mut u8 {
        self.0
    }

    #[inline(always)]
    unsafe fn literal(at: *mut u8, byte: u8) {
        // SAFETY: the caller's.
        unsafe { *at = byte };
    }

    #[inline(always)]
    unsafe fn copy(&mut self, at: *mut u8, back: usize, n: usize) -> bool {
        // SAFETY: at is within the buffer.
        if back > unsafe { at.offset_from(self.0) } as usize {
            return false;
        }
        // SAFETY: back is within what's written, and the caller's room.
        unsafe { copy_match(at, 0, back, n) };
        true
    }
}
#[inline(always)]
pub(super) unsafe fn fast<O: Output>(t: &Tables, bits: &mut Bits, out: &mut O, pos_out: &mut usize, fast_end: usize) -> Option<Res<()>> {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("bmi2") {
        // SAFETY: the CPU has BMI2; the rest is the caller's.
        return unsafe {
            if t.literal_heavy {
                fast_bmi2::<O, true>(t, bits, out, pos_out, fast_end)
            } else {
                fast_bmi2::<O, false>(t, bits, out, pos_out, fast_end)
            }
        };
    }
    // SAFETY: the caller's.
    unsafe {
        if t.literal_heavy {
            fast_any::<O, true>(t, bits, out, pos_out, fast_end)
        } else {
            fast_any::<O, false>(t, bits, out, pos_out, fast_end)
        }
    }
}

/// [`fast`] compiled for BMI2: extracting a value's extra bits takes `bzhi` and `shrx`, two
/// instructions instead of seven.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
unsafe fn fast_bmi2<O: Output, const LITERALS: bool>(
    t: &Tables,
    bits: &mut Bits,
    out: &mut O,
    pos_out: &mut usize,
    fast_end: usize,
) -> Option<Res<()>> {
    // SAFETY: the caller's.
    unsafe { fast_any::<O, LITERALS>(t, bits, out, pos_out, fast_end) }
}

#[inline(always)]
/// `LITERALS`: the loop for literal-heavy blocks (four literals per refill; a literal run's
/// next entry goes round the loop again), else the one for match-heavy blocks (entries
/// consumed before they're looked at, a literal run going straight into its match).
unsafe fn fast_any<O: Output, const LITERALS: bool>(
    t: &Tables,
    bits: &mut Bits,
    out: &mut O,
    pos_out: &mut usize,
    fast_end: usize,
) -> Option<Res<()>> {
    let pos = *pos_out;
    let lit = t.litlen.entries.as_ptr();
    let (lit_mask, dist_mask) = ((1u64 << LITLEN_BITS) - 1, (1u64 << DIST_BITS) - 1);
    // SAFETY (all table reads): main-table indexes are masked to the main table's size, and
    // subtable indexes stay inside the subtables `build` sized for them.
    macro_rules! main {
        ($buf:expr) => {
            unsafe { *lit.add(($buf & lit_mask) as usize) }
        };
    }

    // A whole match (258 bytes) always fits before `fast_end` plus 258, so no output checks,
    // and the input has room for a round's refills (two, of up to 7 bytes each), so no clamp.
    // The reader's state lives in locals (registers) until the loop ends. Each round starts
    // with the bit buffer at 56+ bits and the next litlen entry already looked up
    // ("preloaded", overlapping the lookup with the previous round's stores).
    let in_end = bits.max_next.saturating_sub(14);
    if pos < fast_end && bits.next < in_end {
        let data = bits.data.as_ptr();
        // At a fixed distance from the litlen table: no register of its own.
        debug_assert!(std::ptr::eq(t.dist.entries.ptr, unsafe { t.litlen.entries.ptr.add(DIST_AT) }));
        // SAFETY: the distance table is part of the same allocation, at DIST_AT.
        let dist = unsafe { lit.add(DIST_AT) };
        // The input as a pointer (one register, not a base and an index).
        // SAFETY: next and in_end are within the padded input.
        let (mut inp, in_stop) = unsafe { (data.add(bits.next), data.add(in_end)) };
        let (mut buf, mut cnt) = (bits.buf, bits.cnt);
        // The output as a pointer, too.
        let base = out.base();
        // SAFETY: pos and fast_end are within the output (the caller's).
        let (mut dst, dst_end) = unsafe { (base.add(pos), base.add(fast_end)) };
        // `cnt` counts in its low byte only: consuming subtracts a whole entry (its low byte is
        // the bit count), and shifts take the entry as it is (x86 masks the count to 6 bits).
        // One instruction less between a table lookup and the next.
        macro_rules! refill {
            () => {
                // SAFETY: inp < max_next (see in_end), and the input has 8 bytes past that.
                let w = unsafe { (inp as *const u64).read_unaligned() };
                buf |= u64::from_le(w) << (cnt as u8);
                inp = unsafe { inp.add(((63 - cnt as u8) >> 3) as usize) };
                cnt |= 56;
            };
        }
        macro_rules! consume {
            ($e:expr) => {
                buf = buf.wrapping_shr($e);
                cnt = cnt.wrapping_sub($e);
            };
        }
        macro_rules! peek {
            ($n:expr) => {
                (buf & ((1u64 << $n) - 1)) as usize
            };
        }
        // Each round consumes its preloaded entry first, then looks at what it was (as
        // libdeflate does): a literal run that ends in a length code goes straight on to the
        // match, refilling before the distance only if the bits ran low.
        refill!();
        let mut e = main!(buf);
        let result = if LITERALS {
            loop {
                if e & LITERAL != 0 {
                    // Literal entries of the main table take at most 11 bits: four of them,
                    // and the next lookup's 11, fit in 56.
                    consume!(e);
                    // SAFETY (stores): dst < dst_end, with 258 bytes of room after.
                    unsafe { O::literal(dst, (e >> 16) as u8) };
                    e = main!(buf);
                    let mut k = 1;
                    if e & LITERAL != 0 {
                        consume!(e);
                        unsafe { O::literal(dst.add(1), (e >> 16) as u8) };
                        e = main!(buf);
                        k = 2;
                        if e & LITERAL != 0 {
                            consume!(e);
                            unsafe { O::literal(dst.add(2), (e >> 16) as u8) };
                            e = main!(buf);
                            k = 3;
                            if e & LITERAL != 0 {
                                consume!(e);
                                unsafe { O::literal(dst.add(3), (e >> 16) as u8) };
                                e = main!(buf);
                                k = 4;
                            }
                        }
                    }
                    dst = unsafe { dst.add(k) };
                    if dst >= dst_end || inp >= in_stop {
                        break None;
                    }
                    refill!();
                    continue;
                }
                if e & (SUBTABLE | END | INVALID) != 0 {
                    // The rare entries, behind one test.
                    if e & SUBTABLE != 0 {
                        consume!(e);
                        let i = (e >> 16) as usize + peek!((e >> 8) & 0xf);
                        e = unsafe { *lit.add(i) };
                        if e & LITERAL != 0 {
                            consume!(e);
                            unsafe { O::literal(dst, (e >> 16) as u8) };
                            dst = unsafe { dst.add(1) };
                            refill!();
                            e = main!(buf);
                            if dst >= dst_end || inp >= in_stop {
                                break None;
                            }
                            continue;
                        }
                    }
                    if e & (END | INVALID) != 0 {
                        consume!(e);
                        break Some(if e & INVALID != 0 { Err(Bad) } else { Ok(()) });
                    }
                }
                // A match: the length code and its extra bits (20 bits at most), then the
                // distance code and its extra bits (28 at most): 48 of the 56.
                let saved = buf;
                consume!(e);
                let n = value(e, saved);
                let mut d = unsafe { *dist.add((buf & dist_mask) as usize) };
                if d & SUBTABLE != 0 {
                    consume!(d);
                    let i = (d >> 16) as usize + peek!((d >> 8) & 0xf);
                    d = unsafe { *dist.add(i) };
                }
                let saved = buf;
                consume!(d);
                let back = value(d, saved);
                refill!();
                e = main!(buf);
                // SAFETY: dst + 258 + OUTPUT_SLACK is within out.
                if d & INVALID != 0 || !unsafe { out.copy(dst, back, n) } {
                    break Some(Err(Bad));
                }
                dst = unsafe { dst.add(n) };
                if dst >= dst_end || inp >= in_stop {
                    break None;
                }
            }
        } else {
            loop {
                let mut saved = buf;
                consume!(e);
                if e & LITERAL != 0 {
                    // Three more literals at most (11 bits each in the main table): after three,
                    // a length code with its extra bits (16, or a subtable pointer and 4 + 5) still
                    // fits in the 56.
                    // SAFETY (stores): pos < fast_end, with 258 bytes of room after.
                    unsafe { O::literal(dst, (e >> 16) as u8) };
                    dst = unsafe { dst.add(1) };
                    e = main!(buf);
                    saved = buf;
                    consume!(e);
                    if e & LITERAL != 0 {
                        unsafe { O::literal(dst, (e >> 16) as u8) };
                        dst = unsafe { dst.add(1) };
                        e = main!(buf);
                        saved = buf;
                        consume!(e);
                        if e & LITERAL != 0 {
                            unsafe { O::literal(dst, (e >> 16) as u8) };
                            dst = unsafe { dst.add(1) };
                            e = main!(buf);
                            saved = buf;
                            consume!(e);
                            if e & LITERAL != 0 {
                                unsafe { O::literal(dst, (e >> 16) as u8) };
                                dst = unsafe { dst.add(1) };
                                e = main!(buf);
                                refill!();
                                if dst >= dst_end || inp >= in_stop {
                                    break None;
                                }
                                continue;
                            }
                        }
                    }
                }
                if e & (SUBTABLE | END | INVALID) != 0 {
                    // The rare entries, behind one test.
                    if e & SUBTABLE != 0 {
                        // The pointer's bits are consumed; now the subtable's entry.
                        let i = (e >> 16) as usize + peek!((e >> 8) & 0xf);
                        e = unsafe { *lit.add(i) };
                        saved = buf;
                        consume!(e);
                        if e & LITERAL != 0 {
                            unsafe { O::literal(dst, (e >> 16) as u8) };
                            dst = unsafe { dst.add(1) };
                            refill!();
                            e = main!(buf);
                            if dst >= dst_end || inp >= in_stop {
                                break None;
                            }
                            continue;
                        }
                    }
                    if e & (END | INVALID) != 0 {
                        break Some(if e & INVALID != 0 { Err(Bad) } else { Ok(()) });
                    }
                }
                // A match: the length is in the consumed entry and the bits it saved. The distance
                // needs up to 28 bits (15 + 13): refill first if fewer are left.
                let n = value(e, saved);
                if (cnt as u8) < 28 {
                    refill!();
                }
                let mut d = unsafe { *dist.add((buf & dist_mask) as usize) };
                if d & SUBTABLE != 0 {
                    consume!(d);
                    let i = (d >> 16) as usize + peek!((d >> 8) & 0xf);
                    d = unsafe { *dist.add(i) };
                }
                let saved = buf;
                consume!(d);
                let back = value(d, saved);
                refill!();
                e = main!(buf);
                // SAFETY: pos + 258 + OUTPUT_SLACK is within out.
                if d & INVALID != 0 || !unsafe { out.copy(dst, back, n) } {
                    break Some(Err(Bad));
                }
                dst = unsafe { dst.add(n) };
                if dst >= dst_end || inp >= in_stop {
                    break None;
                }
            }
        };
        // SAFETY: inp is within the input.
        let next = unsafe { inp.offset_from(data) } as usize;
        (bits.buf, bits.cnt, bits.next) = (buf, cnt & 0xff, next);
        // SAFETY: dst is within the output.
        *pos_out = unsafe { dst.offset_from(base) } as usize;
        return result;
    }
    None
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

    /// Cost per block, ours and libdeflate's: the same data ($BRAR_BLOCK_DATA, a file)
    /// compressed into ~1 KB blocks and into ~64 KB blocks (sync flushes); the time difference
    /// over the block count difference.
    #[test]
    #[ignore]
    fn block_cost() {
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
        let data = std::fs::read(std::env::var("BRAR_BLOCK_DATA").unwrap()).unwrap();
        let data = &data[..data.len().min(16 << 20)];
        let stream = |every: usize| {
            let mut c = flate2::Compress::new(flate2::Compression::new(6), false);
            let mut out = Vec::with_capacity(data.len());
            for piece in data.chunks(every) {
                out.reserve(piece.len() + 1024);
                c.compress_vec(piece, &mut out, flate2::FlushCompress::Sync).unwrap();
            }
            out.reserve(1024);
            c.compress_vec(&[], &mut out, flate2::FlushCompress::Finish).unwrap();
            out
        };
        let mut t = Tables::default();
        let mut d = libdeflater::Decompressor::new();
        let mut out = vec![0u8; data.len() + OUTPUT_SLACK];
        let mut times = Vec::new();
        for every in [1024, 65536] {
            let mut input = stream(every);
            let len = input.len();
            input.resize(len + INPUT_PADDING, 0);
            let mut starts = Vec::new();
            inflate_recording(&mut t, &input, len, &mut out, data.len(), Some(&mut starts)).unwrap();
            assert!(out[..data.len()] == *data);
            let (mut ours, mut theirs) = (u64::MAX, u64::MAX);
            for _ in 0..5 {
                let c0 = cycles();
                inflate(&mut t, &input, len, &mut out, data.len()).unwrap();
                ours = ours.min(cycles() - c0);
                let c0 = cycles();
                d.deflate_decompress(&input[..len], &mut out[..data.len()]).unwrap();
                theirs = theirs.min(cycles() - c0);
            }
            println!(
                "blocks every {every}: {} blocks, B-Rar {:.2} cycles/B, libdeflate {:.2}",
                starts.len(),
                ours as f64 / data.len() as f64,
                theirs as f64 / data.len() as f64
            );
            times.push((starts.len() as f64, ours as f64, theirs as f64));
        }
        let blocks = times[0].0 - times[1].0;
        println!(
            "per block: B-Rar {:.0} cycles, libdeflate {:.0}",
            (times[0].1 - times[1].1) / blocks,
            (times[0].2 - times[1].2) / blocks
        );
    }

    /// Where the time goes, instruction by instruction: a thread decodes $BRAR_BLOCK_DATA
    /// (64 KB blocks) over and over with $BRAR_PROFILE (`ours` or `libdeflate`) while this one
    /// samples its instruction pointer (suspend, read the context, resume). Prints the image
    /// base and the most frequent addresses, to look up in `objdump -d` of the test binary.
    #[test]
    #[ignore]
    fn profile_loop() {
        #[repr(C, align(16))]
        struct Context([u8; 1232]);
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SuspendThread(thread: isize) -> u32;
            fn ResumeThread(thread: isize) -> u32;
            fn GetThreadContext(thread: isize, context: *mut Context) -> i32;
            fn GetModuleHandleW(name: *const u16) -> isize;
        }
        use std::os::windows::io::AsRawHandle;
        use std::sync::atomic::{AtomicBool, Ordering};
        let data = std::fs::read(std::env::var("BRAR_BLOCK_DATA").unwrap()).unwrap();
        let data = data[..data.len().min(16 << 20)].to_vec();
        let _ = &data;
        let mut c = flate2::Compress::new(flate2::Compression::new(6), false);
        let mut input = Vec::with_capacity(data.len());
        for piece in data.chunks(65536) {
            input.reserve(piece.len() + 1024);
            c.compress_vec(piece, &mut input, flate2::FlushCompress::Sync).unwrap();
        }
        input.reserve(1024);
        c.compress_vec(&[], &mut input, flate2::FlushCompress::Finish).unwrap();
        let len = input.len();
        input.resize(len + INPUT_PADDING, 0);
        let ours = std::env::var("BRAR_PROFILE").as_deref() != Ok("libdeflate");
        // $BRAR_PROFILE_ZIP: every Deflate entry of that ZIP instead.
        let mut streams = vec![(input, len, data.len())];
        if let Ok(path) = std::env::var("BRAR_PROFILE_ZIP") {
            streams.clear();
            let file = std::fs::File::open(path).unwrap();
            let mut archive = ::zip::ZipArchive::new(&file).unwrap();
            for i in 0..archive.len() {
                let f = archive.by_index_raw(i).unwrap();
                if f.compression() == ::zip::CompressionMethod::Deflated && f.size() > 0 {
                    let mut input = vec![0u8; f.compressed_size() as usize + INPUT_PADDING];
                    crate::whole::read_at(&file, f.data_start().unwrap(), &mut input[..f.compressed_size() as usize]).unwrap();
                    streams.push((input, f.compressed_size() as usize, f.size() as usize));
                }
            }
        }
        static STOP: AtomicBool = AtomicBool::new(false);
        let worker = std::thread::spawn(move || {
            let mut t = Tables::default();
            let mut d = libdeflater::Decompressor::new();
            let most = streams.iter().map(|s| s.2).max().unwrap();
            let mut out = vec![0u8; most + OUTPUT_SLACK];
            while !STOP.load(Ordering::Relaxed) {
                for (input, len, size) in &streams {
                    if ours {
                        inflate(&mut t, input, *len, &mut out, *size).unwrap();
                    } else {
                        d.deflate_decompress(&input[..*len], &mut out[..*size]).unwrap();
                    }
                }
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let h = worker.as_raw_handle() as isize;
        let mut hist = std::collections::HashMap::<u64, u32>::new();
        let mut ctx = Context([0; 1232]);
        for _ in 0..40_000 {
            std::thread::sleep(std::time::Duration::from_micros(100));
            ctx.0[0x30..0x34].copy_from_slice(&0x0010_0001u32.to_le_bytes()); // CONTEXT_CONTROL
            // SAFETY: a live thread's handle and an aligned CONTEXT.
            unsafe {
                SuspendThread(h);
                if GetThreadContext(h, &mut ctx) != 0 {
                    let rip = u64::from_le_bytes(ctx.0[0xf8..0x100].try_into().unwrap());
                    *hist.entry(rip).or_default() += 1;
                }
                ResumeThread(h);
            }
        }
        STOP.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        // SAFETY: the running executable's handle.
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as u64;
        let total: u32 = hist.values().sum();
        let mut v: Vec<_> = hist.into_iter().collect();
        v.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        println!("base {base:#x}, {total} samples");
        for (rip, n) in v.iter().take(5000) {
            println!("{:#x} {:5} {:.2}%", rip - base, n, *n as f64 * 100.0 / total as f64);
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
