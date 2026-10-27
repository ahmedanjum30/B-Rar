//! B-Rar's own LZMA and LZMA2 decoder (7z's main methods).
//!
//! LZMA is a range coder over adaptive bit probabilities: every literal, match length and
//! distance is decoded bit by bit, each bit choosing between two sub-ranges sized by its
//! probability, which then moves towards what was seen. LZMA2 wraps LZMA in chunks (up to 2 MB
//! out each) that can reset the state, the probabilities or the dictionary; after a dictionary
//! reset nothing before it is needed, which is what lets a stream be split across threads.
//!
//! The output goes into a flat window: the bytes decoded so far, of which at least the last
//! dictionary's worth is kept. When the window fills up, the last dictionary's worth moves to
//! its front, so a match never wraps around.
//!
//! Speed: the bits of a literal (most of what poorly compressible data is made of) are as hard
//! to predict as coin tosses, so they're decided without branches; on x86-64 the 8-bit trees of
//! literals (plain, and after a match) are in assembly, which decides each bit from one compare.
//! LZMA2 streams that 7-Zip wrote on several threads restart their
//! dictionary every few dictionary sizes; [`decode_lzma2_mt`] decodes those parts in parallel.

use std::sync::{Condvar, Mutex};

use crate::{
    deflate::inflate::{Bad, Res},
    whole::{BUDGET, Budget, Lease, installed_memory},
};

const NUM_STATES: usize = 12;
const POS_STATES_MAX: usize = 1 << 4;
const LEN_TO_POS_STATES: usize = 4;
const END_POS_MODEL: u32 = 14;
const FULL_DISTANCES: usize = 1 << (END_POS_MODEL / 2);
const ALIGN_BITS: u32 = 4;
const MATCH_MIN: usize = 2;
/// The longest match.
const MATCH_MAX: usize = MATCH_MIN + 8 + 8 + 256 - 1;

/// How far one symbol can read past the input check at its start (it uses at most one byte
/// per bit): `decode` wants this much readable input after the run it decodes.
const PAD: usize = 64;

/// The state after a literal.
const AFTER_LITERAL: [u8; NUM_STATES] = [0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 4, 5];
/// Probabilities are 11-bit: 1024 is "50/50".
const PROB_INIT: u16 = 1024;
const MOVE_BITS: u32 = 5;
const TOP: u32 = 1 << 24;

/// LZMA's properties: literal context bits, literal position bits, position bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Props {
    pub lc: u32,
    pub lp: u32,
    pub pb: u32,
}

impl Props {
    /// From the properties byte `(pb * 5 + lp) * 9 + lc`.
    pub(crate) fn from_byte(b: u8) -> Res<Self> {
        if b >= 9 * 5 * 5 {
            return Err(Bad);
        }
        let b = u32::from(b);
        Ok(Self {
            lc: b % 9,
            lp: b / 9 % 5,
            pb: b / 45,
        })
    }
}

#[derive(Clone)]
struct LenProbs {
    choice: u16,
    choice2: u16,
    low: [[u16; 8]; POS_STATES_MAX],
    mid: [[u16; 8]; POS_STATES_MAX],
    high: [u16; 256],
}

impl LenProbs {
    fn new() -> Self {
        Self {
            choice: PROB_INIT,
            choice2: PROB_INIT,
            low: [[PROB_INIT; 8]; POS_STATES_MAX],
            mid: [[PROB_INIT; 8]; POS_STATES_MAX],
            high: [PROB_INIT; 256],
        }
    }
}

/// Everything LZMA adapts: the probabilities, the state and the last four distances.
#[derive(Clone)]
struct Model {
    is_match: [u16; NUM_STATES * POS_STATES_MAX],
    is_rep: [u16; NUM_STATES],
    is_rep_g0: [u16; NUM_STATES],
    is_rep_g1: [u16; NUM_STATES],
    is_rep_g2: [u16; NUM_STATES],
    is_rep0_long: [u16; NUM_STATES * POS_STATES_MAX],
    pos_slot: [[u16; 64]; LEN_TO_POS_STATES],
    spec_pos: [u16; FULL_DISTANCES - END_POS_MODEL as usize],
    align: [u16; 1 << ALIGN_BITS],
    len: LenProbs,
    rep_len: LenProbs,
    /// 0x300 per literal context.
    literal: Vec<u16>,
    state: usize,
    reps: [u32; 4],
}

impl Model {
    fn new(props: Props) -> Self {
        Self {
            is_match: [PROB_INIT; NUM_STATES * POS_STATES_MAX],
            is_rep: [PROB_INIT; NUM_STATES],
            is_rep_g0: [PROB_INIT; NUM_STATES],
            is_rep_g1: [PROB_INIT; NUM_STATES],
            is_rep_g2: [PROB_INIT; NUM_STATES],
            is_rep0_long: [PROB_INIT; NUM_STATES * POS_STATES_MAX],
            pos_slot: [[PROB_INIT; 64]; LEN_TO_POS_STATES],
            spec_pos: [PROB_INIT; FULL_DISTANCES - END_POS_MODEL as usize],
            align: [PROB_INIT; 1 << ALIGN_BITS],
            len: LenProbs::new(),
            rep_len: LenProbs::new(),
            literal: vec![PROB_INIT; 0x300 << (props.lc + props.lp)],
            state: 0,
            reps: [0; 4],
        }
    }
}

/// The decoded output so far: `buf[..pos]`, the window matches copy from. `base` counts the
/// bytes that slid out of the front (always a multiple of 4096).
pub(crate) struct Window {
    buf: Vec<u8>,
    pos: usize,
    /// Matches can't reach before this (where the last dictionary reset was, if it's still in
    /// the window).
    floor: usize,
    /// The output position of the last dictionary reset: LZMA's position contexts count from
    /// there.
    origin: u64,
    /// Bytes kept when sliding: the dictionary size.
    keep: usize,
    /// `buf[..emitted]` has been handed out already.
    emitted: usize,
    base: u64,
}

impl Window {
    /// A window for dictionary size `dict`, sliding in steps of about `step` bytes.
    pub(crate) fn new(dict: usize, step: usize) -> Self {
        let keep = dict.next_multiple_of(4096);
        Self {
            // + 4096: sliding keeps a multiple of 4096 more than `keep`.
            buf: vec![0; keep + 4096 + step.max(1 << 20) + MATCH_MAX + 64],
            pos: 0,
            floor: 0,
            origin: 0,
            keep,
            emitted: 0,
            base: 0,
        }
    }

    /// Bytes decoded in all.
    pub(crate) fn total(&self) -> u64 {
        self.base + self.pos as u64
    }

    /// Hands what's new to `out`, then makes room for at least `need` more bytes (sliding the
    /// last dictionary's worth to the front if needed).
    fn flush<E>(&mut self, need: usize, out: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        if self.emitted < self.pos {
            out(&self.buf[self.emitted..self.pos])?;
            self.emitted = self.pos;
        }
        if self.buf.len() - self.pos < need + MATCH_MAX + 64 {
            let from = (self.pos.saturating_sub(self.keep)) & !4095;
            self.buf.copy_within(from..self.pos, 0);
            self.pos -= from;
            self.emitted = self.pos;
            self.floor = self.floor.saturating_sub(from);
            self.base += from as u64;
        }
        Ok(())
    }

    /// A dictionary reset: nothing after it may refer back, and positions count from here.
    fn reset(&mut self) {
        self.floor = self.pos;
        self.origin = self.total();
    }
}

/// The decoder: the model, and the properties it was built for.
pub(crate) struct Decoder {
    props: Props,
    model: Model,
}

/// A failed decode: damaged data, or the output callback's own error.
#[derive(Debug)]
pub(crate) enum Failure<E> {
    Bad,
    Stop(E),
}

impl<E> From<Bad> for Failure<E> {
    fn from(_: Bad) -> Self {
        Failure::Bad
    }
}

impl Decoder {
    pub(crate) fn new(props: Props) -> Self {
        Self {
            props,
            model: Model::new(props),
        }
    }

    fn reset_state(&mut self, props: Props) {
        if props != self.props || self.model.literal.len() != 0x300 << (props.lc + props.lp) {
            self.model = Model::new(props);
        } else {
            let literal = std::mem::take(&mut self.model.literal);
            self.model = Model {
                literal,
                ..Model::new(Props { lc: 0, lp: 0, pb: 0 })
            };
            self.model.literal.fill(PROB_INIT);
        }
        self.props = props;
    }

    /// Decodes from `input` with coder `rc` until `w.pos` reaches `end` or the coder passes
    /// `limit` (`input` has [`PAD`] bytes after it, so a symbol that starts at or before it
    /// can't read past the end); returns whether it met the end marker. With `exact` (LZMA2
    /// chunks) a match may not run past `end`; without (LZMA) it may, and the caller cuts the
    /// output.
    ///
    /// The loop does plain literals, most of what poorly compressible data is made of, itself;
    /// the other symbols are in [`Self::other`].
    #[inline(never)]
    fn decode(&mut self, rc: &mut Rc, input: &[u8], limit: usize, w: &mut Window, end: usize, exact: bool) -> Res<bool> {
        assert!(input.len() >= limit + PAD && w.buf.len() >= end + MATCH_MAX + 64);
        // A copy, so it stays in registers.
        let mut c = *rc;
        let Props { lc, lp, pb } = self.props;
        let f = Frame {
            buf: w.buf.as_mut_ptr(),
            floor: w.floor,
            end,
            exact,
            lc,
            // The bits of (position << 8 | previous byte) that pick the literal context.
            lit_mask: (0x100 << lp) - (0x100 >> lc),
        };
        let pb_mask = (1usize << pb) - 1;
        // Position contexts count from the last dictionary reset.
        let base_low = w.base.wrapping_sub(w.origin) as usize;
        let m = &mut self.model;
        let mut pos = w.pos;
        let mut state = m.state;
        let mut done = false;
        while pos < end && c.at <= limit {
            let total = base_low.wrapping_add(pos);
            let pos_state = total & pb_mask;
            let literal = c.bit(input, &mut m.is_match[(state << 4) + pos_state]) == 0;
            if literal && state < 7 {
                let probs = f.literal_probs(m, total, pos);
                #[cfg(target_arch = "x86_64")]
                let s = c.literal_asm(input, probs);
                #[cfg(not(target_arch = "x86_64"))]
                let s = c.tree(input, probs, 8);
                // SAFETY: pos < end.
                unsafe { *f.buf.add(pos) = s as u8 };
                pos += 1;
                state = usize::from(AFTER_LITERAL[state]);
                continue;
            }
            (c, pos, state, done) = Self::other(m, input, &f, c, pos, state, pos_state, total, literal)?;
            if done {
                break;
            }
        }
        *rc = c;
        m.state = state;
        w.pos = pos;
        Ok(done)
    }

    /// A symbol other than a plain literal: a literal after a match (`literal`), a match or a
    /// repeated match. Returns the coder, position and state after it, and whether it was the
    /// end marker.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn other(
        m: &mut Model,
        input: &[u8],
        f: &Frame,
        mut rc: Rc,
        mut pos: usize,
        mut state: usize,
        pos_state: usize,
        total: usize,
        literal: bool,
    ) -> Res<(Rc, usize, usize, bool)> {
        let buf = f.buf;
        if literal {
            // Decoded against the byte at the last distance: while the bits agree with its bits,
            // they have their own probabilities.
            let back = m.reps[0] as usize + 1;
            if back > pos - f.floor {
                return Err(Bad);
            }
            // SAFETY: back <= pos - floor.
            #[cfg_attr(target_arch = "x86_64", allow(unused_mut))]
            let mut match_byte = usize::from(unsafe { *buf.add(pos - back) });
            let probs = f.literal_probs(m, total, pos);
            #[cfg(target_arch = "x86_64")]
            let s = rc.matched_asm(input, probs, match_byte as u8);
            #[cfg(not(target_arch = "x86_64"))]
            let s = {
                let mut s = 1usize;
                let mut offs = 0x100usize;
                while s < 0x100 {
                    match_byte <<= 1;
                    let match_bit = match_byte & offs;
                    let b = rc.bit_free(input, &mut probs[offs + match_bit + s]);
                    s = (s << 1) | b;
                    // Once a bit differs from the match byte's, the rest are plain.
                    offs &= if b == 0 { !match_bit } else { match_bit };
                }
                s
            };
            // SAFETY: pos < end.
            unsafe { *buf.add(pos) = s as u8 };
            return Ok((rc, pos + 1, usize::from(AFTER_LITERAL[state]), false));
        }
        let len;
        if rc.bit(input, &mut m.is_rep[state]) == 0 {
            // A new distance.
            len = MATCH_MIN + rc.length(input, &mut m.len, pos_state);
            state = if state < 7 { 7 } else { 10 };
            let len_state = (len - MATCH_MIN).min(LEN_TO_POS_STATES - 1);
            let slot = rc.tree(input, &mut m.pos_slot[len_state], 6) as u32;
            let dist = if slot < 4 {
                slot
            } else {
                let direct = (slot >> 1) - 1;
                let dist = 2 | (slot & 1);
                if slot < END_POS_MODEL {
                    let dist = dist << direct;
                    // This slot's probabilities: from `dist - slot` on.
                    dist + rc.reverse_tree(input, &mut m.spec_pos[(dist - slot) as usize..], direct) as u32
                } else {
                    // The middle bits come without probabilities, the low 4 with.
                    let dist = (dist << (direct - ALIGN_BITS)) | rc.direct(input, direct - ALIGN_BITS);
                    let dist = (dist << ALIGN_BITS) + rc.reverse_tree(input, &mut m.align[1..], ALIGN_BITS) as u32;
                    if dist == u32::MAX {
                        // The end marker.
                        return Ok((rc, pos, state, true));
                    }
                    dist
                }
            };
            m.reps = [dist, m.reps[0], m.reps[1], m.reps[2]];
        } else {
            if rc.bit(input, &mut m.is_rep_g0[state]) == 0 {
                if rc.bit(input, &mut m.is_rep0_long[(state << 4) + pos_state]) == 0 {
                    // One byte from the last distance.
                    let back = m.reps[0] as usize + 1;
                    if back > pos - f.floor {
                        return Err(Bad);
                    }
                    // SAFETY: pos < end, back <= pos - floor.
                    unsafe { *buf.add(pos) = *buf.add(pos - back) };
                    return Ok((rc, pos + 1, if state < 7 { 9 } else { 11 }, false));
                }
            } else {
                let reps = &mut m.reps;
                let dist;
                if rc.bit(input, &mut m.is_rep_g1[state]) == 0 {
                    dist = reps[1];
                } else {
                    if rc.bit(input, &mut m.is_rep_g2[state]) == 0 {
                        dist = reps[2];
                    } else {
                        dist = reps[3];
                        reps[3] = reps[2];
                    }
                    reps[2] = reps[1];
                }
                reps[1] = reps[0];
                reps[0] = dist;
            }
            len = MATCH_MIN + rc.length(input, &mut m.rep_len, pos_state);
            state = if state < 7 { 8 } else { 11 };
        }
        let back = m.reps[0] as usize + 1;
        if back > pos - f.floor || (f.exact && len > f.end - pos) {
            return Err(Bad);
        }
        // SAFETY: back <= pos - floor, and the window has MATCH_MAX bytes after `end`.
        unsafe {
            let dst = buf.add(pos);
            let src = dst.sub(back);
            if back >= 8 {
                let mut i = 0;
                while i < len {
                    (dst.add(i) as *mut u64).write_unaligned((src.add(i) as *const u64).read_unaligned());
                    i += 8;
                }
            } else {
                for i in 0..len {
                    *dst.add(i) = *src.add(i);
                }
            }
        }
        pos += len;
        Ok((rc, pos, state, false))
    }
}

/// What one decode run works on, besides the model and the coder.
struct Frame {
    buf: *mut u8,
    floor: usize,
    end: usize,
    exact: bool,
    lc: u32,
    lit_mask: usize,
}

impl Frame {
    /// The probabilities of the literal at `pos` (`total` from the dictionary reset).
    #[inline(always)]
    fn literal_probs<'m>(&self, m: &'m mut Model, total: usize, pos: usize) -> &'m mut [u16] {
        // SAFETY: pos - 1 is in the window when it's past the floor.
        let prev = if pos > self.floor { unsafe { *self.buf.add(pos - 1) } } else { 0 };
        let at = 3 * ((((total << 8) + usize::from(prev)) & self.lit_mask) << self.lc);
        &mut m.literal[at..at + 0x300]
    }
}

/// One bit of an 8-bit literal tree, for `Rc::literal_asm`.
#[cfg(target_arch = "x86_64")]
macro_rules! lit_bit {
    () => {
        concat!(
            "cmp {range:e}, 0x1000000
",
            "jae 3f
",
            "shl {range:e}, 8
",
            "shl {code:e}, 8
",
            "movzx {t:e}, byte ptr [{inp}]
",
            "or {code:e}, {t:e}
",
            "inc {inp}
",
            "3:
",
            "movzx {c0:e}, word ptr [{probs} + {sym}*4]
",
            "movzx {c1:e}, word ptr [{probs} + {sym}*4 + 2]
",
            "mov {b:e}, {range:e}
",
            "shr {b:e}, 11
",
            "imul {b:e}, {v:e}
",
            "mov {t:e}, {range:e}
",
            "sub {t:e}, {b:e}
",
            "mov {range:e}, {b:e}
",
            "lea {x}, [{v} - 2017]
",
            "mov {c2:e}, {code:e}
",
            // The bit: CF set for a 0.
            "sub {c2:e}, {b:e}
",
            "cmovae {range:e}, {t:e}
",
            "cmovae {code:e}, {c2:e}
",
            "cmovae {x}, {v}
",
            "cmovae {c0:e}, {c1:e}
",
            "lea {t}, [{sym}*2 + 1]
",
            "sbb {t}, 0
",
            "sar {x}, 5
",
            "sub {v}, {x}
",
            "mov word ptr [{probs} + {sym}*2], {v:x}
",
            "mov {sym}, {t}
",
            "mov {v:e}, {c0:e}
",
        )
    };
}

/// The last bit of a bit tree, for `Rc::tree_asm`: `lit_bit!` without loading the children.
#[cfg(target_arch = "x86_64")]
macro_rules! last_bit {
    () => {
        concat!(
            "cmp {range:e}, 0x1000000
",
            "jae 3f
",
            "shl {range:e}, 8
",
            "shl {code:e}, 8
",
            "movzx {t:e}, byte ptr [{inp}]
",
            "or {code:e}, {t:e}
",
            "inc {inp}
",
            "3:
",
            "mov {b:e}, {range:e}
",
            "shr {b:e}, 11
",
            "imul {b:e}, {v:e}
",
            "mov {t:e}, {range:e}
",
            "sub {t:e}, {b:e}
",
            "mov {range:e}, {b:e}
",
            "lea {x}, [{v} - 2017]
",
            "mov {c2:e}, {code:e}
",
            // The bit: CF set for a 0.
            "sub {c2:e}, {b:e}
",
            "cmovae {range:e}, {t:e}
",
            "cmovae {code:e}, {c2:e}
",
            "cmovae {x}, {v}
",
            "lea {t}, [{sym}*2 + 1]
",
            "sbb {t}, 0
",
            "sar {x}, 5
",
            "sub {v}, {x}
",
            "mov word ptr [{probs} + {sym}*2], {v:x}
",
            "mov {sym}, {t}
",
        )
    };
}

/// The range decoder: `range` and `code`, and where its next input byte is.
#[derive(Clone, Copy)]
struct Rc {
    range: u32,
    code: u32,
    at: usize,
}

impl Rc {
    /// A coder starting on `input` (its first 5 bytes: a 0 and the first code).
    fn new(input: &[u8]) -> Res<Self> {
        match input {
            [0, a, b, c, d, ..] => Ok(Self {
                range: u32::MAX,
                code: u32::from_be_bytes([*a, *b, *c, *d]),
                at: 5,
            }),
            _ => Err(Bad),
        }
    }

    #[inline(always)]
    fn normalize(&mut self, input: &[u8]) {
        if self.range < TOP {
            self.range <<= 8;
            // SAFETY: each symbol starts with `at` <= the limit (checked), and reads less than
            // PAD bytes.
            self.code = (self.code << 8) | u32::from(unsafe { *input.get_unchecked(self.at) });
            self.at += 1;
        }
    }

    /// A bit with probability `p`, with a branch: for bits that are mostly the same.
    #[inline(always)]
    fn bit(&mut self, input: &[u8], p: &mut u16) -> usize {
        self.normalize(input);
        let bound = (self.range >> 11) * u32::from(*p);
        if self.code < bound {
            self.range = bound;
            *p += (2048 - *p) >> MOVE_BITS;
            0
        } else {
            self.range -= bound;
            self.code -= bound;
            *p -= *p >> MOVE_BITS;
            1
        }
    }

    /// Decides a bit with probability `v` without a branch, for bits that are hard to predict
    /// (in trees: literals, lengths, distance slots): whether it's a 0, and the probability's
    /// new value.
    #[inline(always)]
    fn decide(&mut self, input: &[u8], v: u16) -> (bool, u16) {
        self.normalize(input);
        let v = u32::from(v);
        let bound = (self.range >> 11) * v;
        let zero = self.code < bound;
        self.range = std::hint::select_unpredictable(zero, bound, self.range.wrapping_sub(bound));
        self.code = self.code.wrapping_sub(std::hint::select_unpredictable(zero, 0, bound));
        // v - (v - x) >> 5 (rounding down): v + (2048 - v) >> 5 for a 0, v - v >> 5 for a 1.
        let x = std::hint::select_unpredictable(zero, 2017, 0);
        (zero, (v as i32 - ((v as i32 - x) >> MOVE_BITS)) as u16)
    }

    #[inline(always)]
    fn bit_free(&mut self, input: &[u8], p: &mut u16) -> usize {
        let zero;
        (zero, *p) = self.decide(input, *p);
        usize::from(!zero)
    }

    /// A bit tree `bits` deep, from node 1 of `probs`. While a bit is being decided, both
    /// children's probabilities load, so the next bit doesn't wait for memory.
    #[inline(always)]
    fn tree(&mut self, input: &[u8], probs: &mut [u16], bits: u32) -> usize {
        #[cfg(target_arch = "x86_64")]
        if true {
            return self.tree_asm(input, probs, bits);
        }
        let mut s = 1usize;
        let mut v = probs[1];
        for _ in 1..bits {
            let pair = &probs[2 * s..2 * s + 2];
            // SAFETY: both are references. Volatile, or LLVM picks the address first and loads
            // after deciding.
            let (c0, c1) = unsafe { (std::ptr::read_volatile(&pair[0]), std::ptr::read_volatile(&pair[1])) };
            let zero;
            (zero, probs[s]) = self.decide(input, v);
            s = (s << 1) | usize::from(!zero);
            v = std::hint::select_unpredictable(zero, c0, c1);
        }
        let zero;
        (zero, probs[s]) = self.decide(input, v);
        ((s << 1) | usize::from(!zero)) - (1 << bits)
    }

    /// An 8-bit literal tree in assembly: one compare per bit, its flags driving every choice.
    /// `probs` needs 0x200 entries (it reads the children of the last nodes too).
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    fn literal_asm(&mut self, input: &[u8], probs: &mut [u16]) -> usize {
        assert!(probs.len() >= 0x200);
        let mut range = self.range;
        let mut code = self.code;
        // SAFETY: the symbol reads less than PAD bytes from `at`.
        let mut inp = unsafe { input.as_ptr().add(self.at) };
        let mut sym = 1usize;
        let v = u64::from(probs[1]);
        // SAFETY: nodes 1..256 and their children (to 511) are in `probs`.
        unsafe {
            std::arch::asm!(
                lit_bit!(),
                lit_bit!(),
                lit_bit!(),
                lit_bit!(),
                lit_bit!(),
                lit_bit!(),
                lit_bit!(),
                lit_bit!(),
                range = inout(reg) range,
                code = inout(reg) code,
                inp = inout(reg) inp,
                sym = inout(reg) sym,
                v = inout(reg) v => _,
                probs = in(reg) probs.as_mut_ptr(),
                t = out(reg) _,
                b = out(reg) _,
                c0 = out(reg) _,
                c1 = out(reg) _,
                c2 = out(reg) _,
                x = out(reg) _,
                options(nostack),
            );
        }
        self.range = range;
        self.code = code;
        // SAFETY: same allocation.
        self.at = unsafe { inp.offset_from(input.as_ptr()) } as usize;
        sym - 0x100
    }

    /// A bit tree `bits` deep (`probs` has its `1 << bits` nodes) in assembly: as
    /// [`Rc::literal_asm`], in a loop, the last level without loading children (they'd be past
    /// `probs`).
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    fn tree_asm(&mut self, input: &[u8], probs: &mut [u16], bits: u32) -> usize {
        assert!(bits >= 1 && probs.len() >= 1 << bits);
        let mut range = self.range;
        let mut code = self.code;
        // SAFETY: the symbol reads less than PAD bytes from `at`.
        let mut inp = unsafe { input.as_ptr().add(self.at) };
        let mut sym = 1usize;
        let v = u64::from(probs[1]);
        // SAFETY: the nodes (below 1 << bits) and the children of all but the last level are
        // in `probs`.
        unsafe {
            std::arch::asm!(
                "test {n}, {n}",
                "jz 4f",
                "2:",
                lit_bit!(),
                "dec {n}",
                "jnz 2b",
                "4:",
                last_bit!(),
                range = inout(reg) range,
                code = inout(reg) code,
                inp = inout(reg) inp,
                sym = inout(reg) sym,
                v = inout(reg) v => _,
                n = inout(reg) u64::from(bits - 1) => _,
                probs = in(reg) probs.as_mut_ptr(),
                t = out(reg) _,
                b = out(reg) _,
                c0 = out(reg) _,
                c1 = out(reg) _,
                c2 = out(reg) _,
                x = out(reg) _,
                options(nostack),
            );
        }
        self.range = range;
        self.code = code;
        // SAFETY: same allocation.
        self.at = unsafe { inp.offset_from(input.as_ptr()) } as usize;
        sym - (1 << bits)
    }

    /// A literal after a match, in assembly: decoded against `match_byte` (the byte at the last
    /// distance), whose bits pick their own probabilities until one differs (as LzmaDec does
    /// it). `probs` is the literal's 0x300.
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    fn matched_asm(&mut self, input: &[u8], probs: &mut [u16], match_byte: u8) -> usize {
        assert!(probs.len() >= 0x300);
        let mut range = self.range;
        let mut code = self.code;
        // SAFETY: the symbol reads less than PAD bytes from `at`.
        let mut inp = unsafe { input.as_ptr().add(self.at) };
        let mut sym = 1usize;
        // SAFETY: offs + bit + sym < 0x300.
        unsafe {
            std::arch::asm!(
                "2:",
                "cmp {range:e}, 0x1000000",
                "jae 3f",
                "shl {range:e}, 8",
                "shl {code:e}, 8",
                "movzx {t:e}, byte ptr [{inp}]",
                "or {code:e}, {t:e}",
                "inc {inp}",
                "3:",
                "add {mb:e}, {mb:e}",
                "mov {bit:e}, {offs:e}",
                "and {offs:e}, {mb:e}",
                "lea {i}, [{offs} + {bit}]",
                "add {i}, {sym}",
                "movzx {v:e}, word ptr [{probs} + {i}*2]",
                "mov {b:e}, {range:e}",
                "shr {b:e}, 11",
                "imul {b:e}, {v:e}",
                "mov {t:e}, {range:e}",
                "sub {t:e}, {b:e}",
                "mov {range:e}, {b:e}",
                "lea {x}, [{v} - 2017]",
                // After a 0 bit the rest are plain: offs ^ bit.
                "xor {bit:e}, {offs:e}",
                "mov {c2:e}, {code:e}",
                // The bit: CF set for a 0.
                "sub {c2:e}, {b:e}",
                "cmovae {range:e}, {t:e}",
                "cmovae {code:e}, {c2:e}",
                "cmovae {x}, {v}",
                "cmovb {offs:e}, {bit:e}",
                "lea {t}, [{sym}*2 + 1]",
                "sbb {t}, 0",
                "sar {x}, 5",
                "sub {v}, {x}",
                "mov word ptr [{probs} + {i}*2], {v:x}",
                "mov {sym}, {t}",
                "cmp {sym}, 256",
                "jb 2b",
                range = inout(reg) range,
                code = inout(reg) code,
                inp = inout(reg) inp,
                sym = inout(reg) sym,
                mb = inout(reg) u64::from(match_byte) => _,
                offs = inout(reg) 0x100u64 => _,
                probs = in(reg) probs.as_mut_ptr(),
                bit = out(reg) _,
                i = out(reg) _,
                v = out(reg) _,
                t = out(reg) _,
                b = out(reg) _,
                c2 = out(reg) _,
                x = out(reg) _,
                options(nostack),
            );
        }
        self.range = range;
        self.code = code;
        // SAFETY: same allocation.
        self.at = unsafe { inp.offset_from(input.as_ptr()) } as usize;
        sym - 0x100
    }

    /// A bit tree read low bit first, its nodes from `probs[0]` (node 1) on.
    #[inline(always)]
    fn reverse_tree(&mut self, input: &[u8], probs: &mut [u16], bits: u32) -> usize {
        let mut s = 1usize;
        let mut r = 0usize;
        for i in 0..bits {
            let b = self.bit_free(input, &mut probs[s - 1]);
            s = (s << 1) | b;
            r |= b << i;
        }
        r
    }

    /// `n` bits without probabilities (each 50/50).
    #[inline(always)]
    fn direct(&mut self, input: &[u8], n: u32) -> u32 {
        let mut r = 0;
        for _ in 0..n {
            self.normalize(input);
            self.range >>= 1;
            // 0 or all ones: whether the bit was a 1.
            let t = (self.code.wrapping_sub(self.range) >> 31).wrapping_sub(1);
            self.code = self.code.wrapping_sub(self.range & t);
            r = (r << 1) | (t & 1);
        }
        r
    }

    /// A match length less MATCH_MIN.
    #[inline(always)]
    fn length(&mut self, input: &[u8], l: &mut LenProbs, pos_state: usize) -> usize {
        if self.bit(input, &mut l.choice) == 0 {
            self.tree(input, &mut l.low[pos_state], 3)
        } else if self.bit(input, &mut l.choice2) == 0 {
            8 + self.tree(input, &mut l.mid[pos_state], 3)
        } else {
            16 + self.tree(input, &mut l.high, 8)
        }
    }
}

/// Decodes an LZMA2 stream with dictionary size `dict`, handing the output to `out` in order;
/// returns the bytes decoded. The stream ends at its end marker or at the end of `input`.
pub(crate) fn decode_lzma2<E>(input: &[u8], dict: usize, out: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<u64, Failure<E>> {
    run(input, None, &mut Window::new(dict, 8 << 20), out)
}

/// [`decode_lzma2`] into window `w`, starting with properties `props` (those in force where a
/// segment starts, if it starts with an uncompressed chunk).
fn run<E>(input: &[u8], props: Option<u8>, w: &mut Window, out: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<u64, Failure<E>> {
    let mut s = Lzma2::new(props)?;
    let mut at = 0usize;
    while at < input.len() {
        match s.chunk(&input[at..], w, out)? {
            Some(n) => at += n,
            None => break,
        }
    }
    w.flush(0, out).map_err(Failure::Stop)?;
    Ok(w.total())
}

/// LZMA2's state between chunks.
struct Lzma2 {
    decoder: Option<Decoder>,
    props: Option<Props>,
    padded: Vec<u8>,
}

impl Lzma2 {
    fn new(props: Option<u8>) -> Res<Self> {
        Ok(Self {
            decoder: None,
            props: props.map(Props::from_byte).transpose()?,
            padded: Vec::new(),
        })
    }

    /// The length of the chunk at the start of `input`, from its header (`None` if the header
    /// isn't all there).
    fn chunk_len(input: &[u8]) -> Option<usize> {
        let control = *input.first()?;
        Some(match control {
            0 => 1,
            1 | 2 => 3 + usize::from(u16::from_be_bytes([*input.get(1)?, *input.get(2)?])) + 1,
            0x80.. => 5 + usize::from(control >= 0xc0) + usize::from(u16::from_be_bytes([*input.get(3)?, *input.get(4)?])) + 1,
            _ => 1,
        })
    }

    /// Decodes the chunk at the start of `input` into `w`, first handing `out` what's in the
    /// window and making room; returns the chunk's length, or `None` at the end marker.
    fn chunk<E>(&mut self, input: &[u8], w: &mut Window, out: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<Option<usize>, Failure<E>> {
        let control = input[0];
        let mut at = 1;
        if control == 0 {
            return Ok(None);
        }
        if control == 1 || control == 2 {
            // Uncompressed; 1 resets the dictionary.
            let size = usize::from(u16::from_be_bytes([*input.get(1).ok_or(Bad)?, *input.get(2).ok_or(Bad)?])) + 1;
            let data = input.get(3..3 + size).ok_or(Bad)?;
            w.flush(size, out).map_err(Failure::Stop)?;
            if control == 1 {
                w.reset();
            }
            w.buf[w.pos..w.pos + size].copy_from_slice(data);
            w.pos += size;
            return Ok(Some(3 + size));
        }
        if control < 0x80 {
            return Err(Failure::Bad);
        }
        let header = input.get(1..5).ok_or(Bad)?;
        let unpacked = (usize::from(control & 0x1f) << 16) + usize::from(u16::from_be_bytes([header[0], header[1]])) + 1;
        let packed = usize::from(u16::from_be_bytes([header[2], header[3]])) + 1;
        at += 4;
        let reset = (control >> 5) & 3;
        if reset >= 2 {
            self.props = Some(Props::from_byte(*input.get(at).ok_or(Bad)?)?);
            at += 1;
        }
        let p = self.props.ok_or(Bad)?;
        if p.lc + p.lp > 4 {
            return Err(Failure::Bad);
        }
        w.flush(unpacked, out).map_err(Failure::Stop)?;
        if reset == 3 {
            w.reset();
        }
        let d = self.decoder.get_or_insert_with(|| Decoder::new(p));
        if reset >= 1 {
            d.reset_state(p);
        }
        // The decoder reads up to PAD bytes ahead: from the input when it has them, else from
        // a padded copy (at the end).
        let chunk = match input.get(at..at + packed + PAD) {
            Some(chunk) => chunk,
            None => {
                self.padded.clear();
                self.padded.extend_from_slice(input.get(at..at + packed).ok_or(Bad)?);
                self.padded.resize(packed + PAD, 0);
                &self.padded[..]
            }
        };
        let end = w.pos + unpacked;
        let mut rc = Rc::new(chunk)?;
        let marker = d.decode(&mut rc, chunk, packed, w, end, true)?;
        rc.normalize(chunk);
        if marker || rc.at > packed || w.pos != end {
            return Err(Failure::Bad);
        }
        Ok(Some(at + packed))
    }
}

/// A damaged stream, as an I/O error.
fn bad_data() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "damaged LZMA data")
}

/// Hands out `w.buf[*sent..w.pos]`; whether there was any.
fn serve(w: &Window, sent: &mut usize, buf: &mut [u8]) -> Option<usize> {
    let n = (w.pos - *sent).min(buf.len());
    if n == 0 {
        return None;
    }
    buf[..n].copy_from_slice(&w.buf[*sent..*sent + n]);
    *sent += n;
    Some(n)
}

/// An LZMA2 stream as a reader (for 7z blocks whose packed data isn't all in memory): decodes
/// a chunk at a time from `inner`.
pub(crate) struct Lzma2Reader<R> {
    inner: R,
    s: Lzma2,
    w: Window,
    chunk: Vec<u8>,
    /// `w.buf[..sent]` has been read.
    sent: usize,
    done: bool,
}

impl<R: std::io::Read> Lzma2Reader<R> {
    pub(crate) fn new(inner: R, dict: usize) -> Self {
        Self {
            inner,
            s: Lzma2::new(None).expect("no properties"),
            w: Window::new(dict, 8 << 20),
            chunk: Vec::new(),
            sent: 0,
            done: false,
        }
    }

    /// Reads and decodes the next chunk; `false` at the end.
    fn next(&mut self) -> std::io::Result<bool> {
        self.chunk.clear();
        self.chunk.resize(6, 0);
        // The control byte (no more input: the end), then the rest of the header.
        if self.inner.read(&mut self.chunk[..1])? == 0 {
            return Ok(false);
        }
        let head = match self.chunk[0] {
            0 => return Ok(false),
            1 | 2 => 3,
            0xc0.. => 6,
            0x80.. => 5,
            _ => return Err(bad_data()),
        };
        self.inner.read_exact(&mut self.chunk[1..head])?;
        let len = Lzma2::chunk_len(&self.chunk[..head]).ok_or_else(bad_data)?;
        self.chunk.resize(len, 0);
        self.inner.read_exact(&mut self.chunk[head..])?;
        // Everything decoded has been read, so the window can slide without handing it out.
        self.w.emitted = self.w.pos;
        let before = self.w.total();
        self.s
            .chunk(&self.chunk, &mut self.w, &mut |_| Ok::<(), ()>(()))
            .map_err(|_| bad_data())?;
        self.sent = self.w.pos - (self.w.total() - before) as usize;
        Ok(true)
    }
}

impl<R: std::io::Read> std::io::Read for Lzma2Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if let Some(n) = serve(&self.w, &mut self.sent, buf) {
                return Ok(n);
            }
            if self.done || buf.is_empty() {
                return Ok(0);
            }
            self.done = !self.next()?;
        }
    }
}

/// An LZMA (LZMA1) stream of `size` bytes as a reader, like [`decode_lzma`].
pub(crate) struct LzmaReader<R> {
    inner: R,
    d: Decoder,
    w: Window,
    rc: Option<Rc>,
    /// Input: `input[..valid]` read from `inner` (then zeros, for the decoder's look-ahead).
    input: Vec<u8>,
    valid: usize,
    eof: bool,
    size: u64,
    sent: usize,
    done: bool,
}

impl<R: std::io::Read> LzmaReader<R> {
    pub(crate) fn new(inner: R, props: &[u8], size: u64) -> std::io::Result<Self> {
        let &[p, d0, d1, d2, d3] = props else { return Err(bad_data()) };
        let dict = u64::from(u32::from_le_bytes([d0, d1, d2, d3])).clamp(4096, size.max(4096));
        Ok(Self {
            inner,
            d: Decoder::new(Props::from_byte(p).map_err(|_| bad_data())?),
            w: Window::new(usize::try_from(dict).map_err(|_| bad_data())?, Self::STEP),
            rc: None,
            input: vec![0; Self::INPUT + PAD],
            valid: 0,
            eof: false,
            size,
            sent: 0,
            done: false,
        })
    }

    const STEP: usize = 4 << 20;
    const INPUT: usize = 1 << 20;

    /// Moves what's left of the input to the front and reads more.
    fn refill(&mut self, from: usize) -> std::io::Result<()> {
        self.input.copy_within(from..self.valid, 0);
        self.valid -= from;
        while !self.eof && self.valid < Self::INPUT {
            match self.inner.read(&mut self.input[self.valid..Self::INPUT])? {
                0 => self.eof = true,
                n => self.valid += n,
            }
        }
        self.input[self.valid..].fill(0);
        Ok(())
    }

    /// Decodes up to a step more; `false` at the end.
    fn next(&mut self) -> std::io::Result<bool> {
        if self.w.total() >= self.size {
            return Ok(false);
        }
        let mut rc = match self.rc {
            Some(rc) => rc,
            None => {
                self.refill(0)?;
                Rc::new(&self.input[..self.valid]).map_err(|_| bad_data())?
            }
        };
        let step = usize::try_from(self.size - self.w.total()).unwrap_or(usize::MAX).min(Self::STEP);
        self.w.emitted = self.w.pos;
        self.w.flush(step, &mut |_| Ok::<(), ()>(())).map_err(|_| bad_data())?;
        self.sent = self.w.pos;
        let end = self.w.pos + step;
        while self.w.pos < end {
            // While more input can come, the coder stops PAD bytes before what's read.
            let limit = if self.eof { self.valid } else { self.valid.saturating_sub(PAD) };
            if rc.at > limit {
                if self.eof {
                    return Err(bad_data());
                }
                self.refill(rc.at)?;
                rc.at = 0;
                continue;
            }
            if self
                .d
                .decode(&mut rc, &self.input, limit, &mut self.w, end, false)
                .map_err(|_| bad_data())?
            {
                // The end marker.
                self.size = self.size.min(self.w.total());
                break;
            }
        }
        self.rc = Some(rc);
        Ok(true)
    }
}

impl<R: std::io::Read> std::io::Read for LzmaReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // Never past `size` (a match can run past it).
            let left = self.size.saturating_sub(self.w.total() - (self.w.pos - self.sent) as u64);
            let cap = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            if let Some(n) = serve(&self.w, &mut self.sent, &mut buf[..cap]) {
                return Ok(n);
            }
            if self.done || cap == 0 {
                return Ok(0);
            }
            self.done = !self.next()?;
        }
    }
}

/// An independent part of an LZMA2 stream: from one dictionary reset to the next.
struct Segment {
    /// Its chunks in the stream.
    packed: std::ops::Range<usize>,
    unpacked: u64,
    /// The properties byte in force where it starts: an uncompressed chunk can reset the
    /// dictionary and leave them as they were.
    props: Option<u8>,
}

/// Splits an LZMA2 stream at its dictionary resets, from the chunk headers alone. `None` if
/// they don't add up (decoding it in one go then reports the damage).
fn segments(input: &[u8]) -> Option<Vec<Segment>> {
    segments_read(
        &|at, buf: &mut [u8]| {
            let s = input.get(at as usize..at as usize + buf.len()).ok_or(std::io::ErrorKind::UnexpectedEof)?;
            buf.copy_from_slice(s);
            Ok(())
        },
        input.len() as u64,
    )
}

/// [`segments`] of a stream of `len` bytes that `read` reads (at an offset), header by header.
fn segments_read(read: &dyn Fn(u64, &mut [u8]) -> std::io::Result<()>, len: u64) -> Option<Vec<Segment>> {
    let mut parts: Vec<Segment> = Vec::new();
    let (mut at, mut props) = (0u64, None);
    let mut h = [0u8; 6];
    while at < len {
        // A chunk header: up to 6 bytes.
        let n = (len - at).min(6) as usize;
        h[n..].fill(0);
        read(at, &mut h[..n]).ok()?;
        let control = h[0];
        if control == 0 {
            break;
        }
        let here = usize::try_from(at).ok()?;
        if control == 1 || control >= 0xe0 {
            if let Some(last) = parts.last_mut() {
                last.packed.end = here;
            }
            parts.push(Segment {
                packed: here..here,
                unpacked: 0,
                props,
            });
        }
        let (unpacked, size) = if control < 0x80 {
            if control > 2 {
                return None;
            }
            let n = usize::from(u16::from_be_bytes([h[1], h[2]])) + 1;
            (n, 3 + n)
        } else {
            if control >= 0xc0 {
                props = Some(h[5]);
            }
            let n = (usize::from(control & 0x1f) << 16) + usize::from(u16::from_be_bytes([h[1], h[2]])) + 1;
            (n, 5 + usize::from(control >= 0xc0) + usize::from(u16::from_be_bytes([h[3], h[4]])) + 1)
        };
        // The stream must start with a dictionary reset.
        parts.last_mut()?.unpacked += unpacked as u64;
        at += size as u64;
    }
    parts.last_mut()?.packed.end = usize::try_from(at.min(len)).ok()?;
    Some(parts)
}

/// Decodes one segment (`input`: its chunks) into memory.
fn decode_segment(input: &[u8], s: &Segment) -> Result<Vec<u8>, Bad> {
    let size = usize::try_from(s.unpacked).map_err(|_| Bad)?;
    // A window that holds the whole segment never slides.
    let mut w = Window::new(size, 0);
    match run(input, s.props, &mut w, &mut |_| Ok::<(), ()>(())) {
        Ok(n) if n == s.unpacked => {}
        _ => return Err(Bad),
    }
    let mut data = w.buf;
    data.truncate(size);
    Ok(data)
}

/// Most bytes [`decode_lzma2_mt`] holds in memory at once: an eighth of the PC's memory, at
/// least 512 MB and at most 4 GB.
fn budget() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| installed_memory().map_or(BUDGET, |m| (m / 8).clamp(512 << 20, 4 << 30)))
}

/// Whether `parts` are worth decoding in parallel (and fit in memory).
fn splits(parts: &[Segment], threads: usize) -> bool {
    threads > 1 && parts.len() > 1 && parts.iter().all(|s| s.unpacked <= budget() / 4)
}

/// A decoded segment and the memory it holds.
type Decoded<'a> = Result<(Vec<u8>, Lease<'a>), Bad>;

/// Decodes an LZMA2 stream like [`decode_lzma2`], on up to `threads` threads: the parts between
/// dictionary resets (which 7-Zip writes when compressing on several threads) are independent,
/// so workers decode them into memory at once (at most [`budget`] bytes of them), and this
/// thread hands them to `out` in order. Streams with a part bigger than a quarter of that
/// decode on one thread.
pub(crate) fn decode_lzma2_mt<E>(input: &[u8], dict: usize, threads: usize, out: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<u64, Failure<E>> {
    match segments(input) {
        Some(parts) if splits(&parts, threads) => in_parts(&parts, threads, &|s| Ok(std::borrow::Cow::Borrowed(&input[s.packed.clone()])), out),
        _ => decode_lzma2(input, dict, out),
    }
}

/// [`decode_lzma2_mt`] for a stream of `len` bytes that `read` reads (at an offset), not
/// mapped (a split archive's): each worker reads its part. `None` if the stream doesn't split,
/// for the caller to decode it as a reader.
pub(crate) fn decode_lzma2_mt_read<E>(
    read: &(dyn Fn(u64, &mut [u8]) -> std::io::Result<()> + Sync),
    len: u64,
    threads: usize,
    out: &mut dyn FnMut(&[u8]) -> Result<(), E>,
) -> Option<Result<u64, Failure<E>>> {
    let parts = segments_read(read, len).filter(|p| splits(p, threads))?;
    Some(in_parts(
        &parts,
        threads,
        &|s| {
            let mut data = vec![0u8; s.packed.len()];
            read(s.packed.start as u64, &mut data).map_err(|_| Bad)?;
            Ok(std::borrow::Cow::Owned(data))
        },
        out,
    ))
}

/// Decodes `parts` on up to `threads` workers, each getting a part's chunks from `load`, and
/// hands them to `out` in order.
#[allow(clippy::type_complexity)]
fn in_parts<'a, E>(
    parts: &[Segment],
    threads: usize,
    load: &(dyn Fn(&Segment) -> Result<std::borrow::Cow<'a, [u8]>, Bad> + Sync),
    out: &mut dyn FnMut(&[u8]) -> Result<(), E>,
) -> Result<u64, Failure<E>> {
    let budget = Budget::new(budget());
    let next = Mutex::new(0usize);
    // Decoded parts by index, and whether the output stopped.
    let slots = Mutex::new((parts.iter().map(|_| None).collect::<Vec<Option<Decoded>>>(), false));
    let ready = Condvar::new();
    std::thread::scope(|scope| {
        for _ in 0..threads.min(parts.len()) {
            scope.spawn(|| {
                loop {
                    let (k, lease) = {
                        let mut n = next.lock().unwrap();
                        if *n >= parts.len() || slots.lock().unwrap().1 {
                            return;
                        }
                        let k = *n;
                        *n += 1;
                        // Taken in order, under the lock: the parts holding memory all come
                        // before this one, so they go out (freeing it) without waiting for it.
                        (k, budget.take(parts[k].unpacked))
                    };
                    let r = load(&parts[k])
                        .and_then(|input| decode_segment(&input, &parts[k]))
                        .map(|data| (data, lease));
                    let mut s = slots.lock().unwrap();
                    if s.1 {
                        return;
                    }
                    s.0[k] = Some(r);
                    ready.notify_all();
                }
            });
        }
        let mut total = 0u64;
        let mut result = Ok(());
        for k in 0..parts.len() {
            let r = {
                let mut s = slots.lock().unwrap();
                loop {
                    if let Some(r) = s.0[k].take() {
                        break r;
                    }
                    s = ready.wait(s).unwrap();
                }
            };
            result = match r {
                Ok((data, _lease)) => {
                    total += data.len() as u64;
                    out(&data).map_err(Failure::Stop)
                }
                Err(Bad) => Err(Failure::Bad),
            };
            if result.is_err() {
                break;
            }
        }
        // Stop the workers and free the memory held, so none waits for it.
        let mut s = slots.lock().unwrap();
        s.1 = true;
        s.0.iter_mut().for_each(|r| *r = None);
        drop(s);
        result.map(|()| total)
    })
}

/// Decodes a 7z LZMA (LZMA1) stream of `size` bytes (7z's have no end marker, usually) with the
/// coder's 5 properties bytes (`lc`/`lp`/`pb`, then the dictionary size), handing the output to
/// `out` in order; returns the bytes decoded.
pub(crate) fn decode_lzma<E>(input: &[u8], props: &[u8], size: u64, out: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<u64, Failure<E>> {
    const STEP: usize = 8 << 20;
    let &[p, d0, d1, d2, d3] = props else { return Err(Failure::Bad) };
    let mut d = Decoder::new(Props::from_byte(p)?);
    let dict = u64::from(u32::from_le_bytes([d0, d1, d2, d3])).clamp(4096, size.max(4096));
    let mut w = Window::new(usize::try_from(dict).map_err(|_| Bad)?, STEP);
    // The output, cut at `size` (a match can run past it).
    let mut sent = 0u64;
    let mut out = |b: &[u8]| -> Result<(), E> {
        let n = b.len().min(usize::try_from(size - sent).unwrap_or(usize::MAX));
        sent += n as u64;
        if n > 0 { out(&b[..n]) } else { Ok(()) }
    };
    let mut rc = Rc::new(input)?;
    // The coder runs on `input` while PAD bytes follow, then on a padded copy of the rest.
    let split = input.len().saturating_sub(PAD);
    let mut tail = Vec::new();
    while w.total() < size {
        let step = usize::try_from(size - w.total()).unwrap_or(usize::MAX).min(STEP);
        w.flush(step, &mut out).map_err(Failure::Stop)?;
        let end = w.pos + step;
        if tail.is_empty() && rc.at <= split {
            if d.decode(&mut rc, input, split, &mut w, end, false)? {
                break;
            }
            if w.pos >= end {
                continue;
            }
        }
        if tail.is_empty() {
            tail.extend_from_slice(&input[split..]);
            tail.resize(input.len() - split + PAD, 0);
            rc.at -= split;
        }
        let limit = input.len() - split;
        if d.decode(&mut rc, &tail, limit, &mut w, end, false)? {
            break;
        }
        if w.pos < end {
            return Err(Failure::Bad);
        }
    }
    w.flush(0, &mut out).map_err(Failure::Stop)?;
    Ok(sent)
}

/// LZMA2's dictionary size from its 7z (or xz) properties byte.
pub(crate) fn lzma2_dict(prop: u8) -> Res<usize> {
    match prop {
        40 => Ok(u32::MAX as usize),
        0..40 => Ok(((2 | (u64::from(prop) & 1)) << (prop / 2 + 11)) as usize),
        _ => Err(Bad),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lzma2_encode(data: &[u8], preset: u32) -> Vec<u8> {
        use liblzma::stream::{Action, Filters, LzmaOptions, Status, Stream};
        let opts = LzmaOptions::new_preset(preset).unwrap();
        let mut filters = Filters::new();
        filters.lzma2(&opts);
        let mut s = Stream::new_raw_encoder(&filters).unwrap();
        let mut out = Vec::with_capacity(data.len() + 1024);
        let mut input = data;
        loop {
            let before = s.total_in();
            out.reserve(64 * 1024);
            let status = s.process_vec(input, &mut out, Action::Finish).unwrap();
            input = &input[(s.total_in() - before) as usize..];
            if status == Status::StreamEnd {
                return out;
            }
        }
    }

    fn sample(n: usize) -> Vec<u8> {
        let mut x = 7u64;
        let words = [b"archive ".as_slice(), b"extract ", b"faster ", b"B-Rar ", b"lzma ", b"window "];
        let mut v = Vec::with_capacity(n);
        while v.len() < n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if x.is_multiple_of(7) {
                v.push(x as u8);
            } else {
                v.extend_from_slice(words[(x % 6) as usize]);
            }
        }
        v.truncate(n);
        v
    }

    #[test]
    fn decodes_what_liblzma_encodes() {
        for (n, preset) in [(0, 6), (1, 6), (100_000, 0), (3_000_000, 6), (5_000_000, 9)] {
            let data = sample(n);
            let packed = lzma2_encode(&data, preset);
            let mut got = Vec::new();
            let total = decode_lzma2(&packed, 8 << 20, &mut |b: &[u8]| -> Result<(), ()> {
                got.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            assert_eq!(total as usize, n);
            assert!(got == data, "{n} bytes, preset {preset}");
        }
    }

    #[test]
    fn small_window_slides() {
        // A window smaller than the data (dictionary 1 MB) makes it slide many times.
        let data = sample(20_000_000);
        let packed = lzma2_encode(&data, 1);
        let mut got = Vec::new();
        decode_lzma2(&packed, 1 << 20, &mut |b: &[u8]| -> Result<(), ()> {
            got.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        assert!(got == data);
    }

    #[test]
    fn splits_at_dictionary_resets() {
        // Streams joined without the end markers between them: one stream, each part
        // starting with a dictionary reset, as 7-Zip writes when compressing on several threads.
        let parts = [sample(3_000_000), sample(1), sample(2_000_000)];
        let mut packed = Vec::new();
        let mut data = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            let mut s = lzma2_encode(p, [6, 0, 9][i]);
            assert_eq!(s.pop(), Some(0));
            packed.extend_from_slice(&s);
            data.extend_from_slice(p);
        }
        packed.push(0);
        assert_eq!(segments(&packed).unwrap().len(), 3);
        for threads in [1, 2, 8] {
            let mut got = Vec::new();
            let total = decode_lzma2_mt(&packed, 8 << 20, threads, &mut |b: &[u8]| -> Result<(), ()> {
                got.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            assert_eq!(total as usize, data.len());
            assert!(got == data, "{threads} threads");
            // The same, read (as from a split archive's parts) rather than mapped.
            let read = |at: u64, buf: &mut [u8]| {
                buf.copy_from_slice(&packed[at as usize..at as usize + buf.len()]);
                Ok(())
            };
            let mut got = Vec::new();
            let total = decode_lzma2_mt_read(&read, packed.len() as u64, threads, &mut |b: &[u8]| -> Result<(), ()> {
                got.extend_from_slice(b);
                Ok(())
            });
            match total {
                Some(total) => assert!(total.unwrap() as usize == data.len() && got == data, "{threads} threads, read"),
                None => assert_eq!(threads, 1),
            }
        }
        // Damage in the last part fails the whole stream.
        let at = segments(&packed).unwrap()[2].packed.start + 1000;
        packed[at] ^= 0x55;
        let mut got = Vec::new();
        let r = decode_lzma2_mt(&packed, 8 << 20, 4, &mut |b: &[u8]| -> Result<(), ()> {
            got.extend_from_slice(b);
            Ok(())
        });
        assert!(r.is_err() || got != data);
    }

    #[test]
    fn decodes_lzma1() {
        use liblzma::stream::{Action, Filters, LzmaOptions, Status, Stream};
        // 20 MB with preset 0's 256 KB dictionary: the window slides, a whole step at a time.
        for (n, preset) in [(0, 6), (1, 0), (300_000, 6), (2_000_000, 1), (20_000_000, 0)] {
            let data = sample(n);
            let opts = LzmaOptions::new_preset(preset).unwrap();
            let mut filters = Filters::new();
            filters.lzma1(&opts);
            let mut s = Stream::new_raw_encoder(&filters).unwrap();
            let mut packed = Vec::new();
            let mut input = &data[..];
            loop {
                let before = s.total_in();
                packed.reserve(1 << 16);
                let status = s.process_vec(input, &mut packed, Action::Finish).unwrap();
                input = &input[(s.total_in() - before) as usize..];
                if status == Status::StreamEnd {
                    break;
                }
            }
            // The presets' lc=3, lp=0, pb=2, and their dictionary size.
            let dict: u32 = if preset == 0 {
                256 << 10
            } else if preset == 1 {
                1 << 20
            } else {
                8 << 20
            };
            let mut props = vec![(2 * 5) * 9 + 3];
            props.extend_from_slice(&dict.to_le_bytes());
            let mut got = Vec::new();
            let total = decode_lzma(&packed, &props, n as u64, &mut |b: &[u8]| -> Result<(), ()> {
                got.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            assert_eq!(total as usize, n);
            assert!(got == data, "{n} bytes, preset {preset}");
        }
    }

    /// Reads in pieces of `n` bytes at most, as a file or a pipe might hand them out.
    struct Trickle<'a>(&'a [u8], usize);

    impl std::io::Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.1).min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn readers_match() {
        use std::io::Read;
        let data = sample(9_000_000);
        let packed = lzma2_encode(&data, 1);
        for piece in [1, 7, 4096] {
            let mut got = Vec::new();
            Lzma2Reader::new(Trickle(&packed, piece), 1 << 20).read_to_end(&mut got).unwrap();
            assert!(got == data, "LZMA2, pieces of {piece}");
        }
        use liblzma::stream::{Action, Filters, LzmaOptions, Status, Stream};
        let mut filters = Filters::new();
        filters.lzma1(&LzmaOptions::new_preset(0).unwrap());
        let mut s = Stream::new_raw_encoder(&filters).unwrap();
        let mut lz = Vec::new();
        let mut input = &data[..];
        loop {
            let before = s.total_in();
            lz.reserve(1 << 16);
            let status = s.process_vec(input, &mut lz, Action::Finish).unwrap();
            input = &input[(s.total_in() - before) as usize..];
            if status == Status::StreamEnd {
                break;
            }
        }
        let mut props = vec![(2 * 5) * 9 + 3];
        props.extend_from_slice(&(256u32 << 10).to_le_bytes());
        for piece in [7, 4096, 1 << 20] {
            let mut got = Vec::new();
            LzmaReader::new(Trickle(&lz, piece), &props, data.len() as u64)
                .unwrap()
                .read_to_end(&mut got)
                .unwrap();
            assert!(got == data, "LZMA, pieces of {piece}");
        }
    }

    #[test]
    fn rejects_damage() {
        let data = sample(500_000);
        let mut packed = lzma2_encode(&data, 6);
        for cut in [1, packed.len() / 2] {
            let r = decode_lzma2(&packed[..cut], 8 << 20, &mut |_: &[u8]| -> Result<(), ()> { Ok(()) });
            assert!(r.is_err(), "cut at {cut}");
        }
        let mid = packed.len() / 2;
        packed[mid] ^= 0x55;
        let mut got = Vec::new();
        let r = decode_lzma2(&packed, 8 << 20, &mut |b: &[u8]| -> Result<(), ()> {
            got.extend_from_slice(b);
            Ok(())
        });
        assert!(r.is_err() || got != data);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// One core's speed (or `BRAR_LZMA_THREADS`' for LZMA2 in parts) on the LZMA and LZMA2
    /// blocks of a real 7z archive, against liblzma:
    /// `BRAR_LZMA_BENCH=archive.7z cargo test --release -p brar-core --lib lzma_bench -- --ignored --nocapture`.
    /// `BRAR_LZMA_MB` decodes only the first so many MB of each block.
    #[test]
    #[ignore]
    fn lzma_bench() {
        let path = std::env::var("BRAR_LZMA_BENCH").expect("BRAR_LZMA_BENCH=archive.7z");
        let file = std::fs::File::open(&path).unwrap();
        let map = unsafe { memmap2::Mmap::map(&file) }.unwrap();
        let archive = sevenz_rust2::Archive::read(&mut std::io::Cursor::new(&map[..]), &sevenz_rust2::Password::empty()).unwrap();
        let limit = std::env::var("BRAR_LZMA_MB").ok().and_then(|v| v.parse::<u64>().ok()).map(|mb| mb << 20);
        let threads = std::env::var("BRAR_LZMA_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
        // (packed, properties, LZMA2?, bytes to decode, CRC to check)
        let mut streams = Vec::new();
        for (b, block) in archive.blocks.iter().enumerate() {
            let Some(coder) = block.coders.iter().find(|c| matches!(c.encoder_method_id(), [0x21] | [3, 1, 1])) else {
                continue;
            };
            let first = archive.stream_map.block_first_pack_stream_index()[b];
            let at = (32 + archive.pack_pos() + archive.stream_map.pack_stream_offsets()[first]) as usize;
            let packed = &map[at..at + archive.pack_sizes()[first] as usize];
            let size = block.get_unpack_size().min(limit.unwrap_or(u64::MAX));
            let crc = (block.coders.len() == 1 && block.has_crc && limit.is_none()).then_some(block.crc as u32);
            streams.push((packed, coder.properties(), coder.encoder_method_id() == [0x21], size, crc));
        }
        let unpacked: u64 = streams.iter().map(|s| s.3).sum();
        println!("{} blocks, {} MB", streams.len(), unpacked >> 20);
        for round in 0..3 {
            let t = Instant::now();
            for &(packed, props, lzma2, size, crc) in &streams {
                let mut h = crc32fast::Hasher::new();
                let mut done = 0u64;
                let mut out = |b: &[u8]| {
                    if round == 0 {
                        h.update(b);
                    }
                    done += b.len() as u64;
                    if done >= size { Err(()) } else { Ok(()) }
                };
                let r = if lzma2 {
                    decode_lzma2_mt::<()>(packed, lzma2_dict(props[0]).unwrap(), threads, &mut out)
                } else {
                    decode_lzma::<()>(packed, props, size, &mut out)
                };
                assert!(matches!(r, Ok(_) | Err(Failure::Stop(()))), "B-Rar failed");
                assert!(done >= size);
                if let (0, Some(crc)) = (round, crc) {
                    assert_eq!(h.finalize(), crc, "CRC");
                }
            }
            let ours = t.elapsed().as_secs_f64();
            let t = Instant::now();
            for &(packed, props, lzma2, size, _) in &streams {
                use liblzma::stream::{Action, Filters, LzmaOptions, Stream};
                let mut opts = LzmaOptions::new_preset(6).unwrap();
                let mut f = Filters::new();
                if lzma2 {
                    opts.dict_size(lzma2_dict(props[0]).unwrap().min(u32::MAX as usize) as u32);
                    f.lzma2(&opts);
                } else {
                    let p = props[0];
                    opts.literal_context_bits(u32::from(p % 9))
                        .literal_position_bits(u32::from(p / 9 % 5))
                        .position_bits(u32::from(p / 45))
                        .dict_size(u32::from_le_bytes(props[1..5].try_into().unwrap()));
                    f.lzma1(&opts);
                }
                let mut s = Stream::new_raw_decoder(&f).unwrap();
                let mut out = vec![0u8; 8 << 20];
                let mut input = packed;
                while s.total_out() < size {
                    let (i, o) = (s.total_in(), s.total_out());
                    s.process(input, &mut out, Action::Run).unwrap();
                    input = &input[(s.total_in() - i) as usize..];
                    assert!(s.total_out() > o || s.total_in() > i, "liblzma stuck");
                }
            }
            let theirs = t.elapsed().as_secs_f64();
            let mb = unpacked as f64 / 1048576.0;
            println!(
                "round {round}: B-Rar {ours:.2} s ({:.0} MB/s), liblzma {theirs:.2} s ({:.0} MB/s)",
                mb / ours,
                mb / theirs
            );
        }
    }
}
