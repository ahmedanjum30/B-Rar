//! The branch filters of xz and 7z (BCJ: x86, ARM, ARM Thumb, ARM64, PowerPC, SPARC, IA-64,
//! RISC-V) and the Delta filter, decoding. Compressors do better on machine code whose
//! branch targets are absolute rather than relative, so the encoder made them absolute; these
//! turn them back. Each follows xz's reference code (`simple/*.c`, `delta/delta_decoder.c`)
//! step for step.

/// Whether `b` is the top byte of an x86 operand the encoder would have converted.
fn top_byte(b: u8) -> bool {
    b == 0 || b == 0xff
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    X86,
    Arm,
    ArmThumb,
    Arm64,
    PowerPc,
    Sparc,
    Ia64,
    RiscV,
    /// Delta, with its distance (1..=256).
    Delta(usize),
}

impl Kind {
    /// The filter of a 7z method ID (Delta's distance from its properties).
    pub(crate) fn of_7z(id: &[u8], props: &[u8]) -> Option<Self> {
        Some(match id {
            [3, 3, 1, 3] => Kind::X86,
            [3, 3, 5, 1] => Kind::Arm,
            [3, 3, 7, 1] => Kind::ArmThumb,
            [0xa] => Kind::Arm64,
            [3, 3, 2, 5] => Kind::PowerPc,
            [3, 3, 8, 5] => Kind::Sparc,
            [3, 3, 4, 1] => Kind::Ia64,
            [0xb] => Kind::RiscV,
            [3] => Kind::Delta(usize::from(*props.first().unwrap_or(&0)) + 1),
            _ => return None,
        })
    }
}

/// A filter's state between pieces of output.
pub(crate) struct Filter {
    kind: Kind,
    /// Output position of the next byte to filter.
    pos: u32,
    // x86's.
    prev_mask: u32,
    prev_pos: u32,
    // Delta's.
    history: [u8; 256],
    delta_pos: u8,
    /// Bytes not filtered yet: an instruction may run on into the next piece.
    carry: Vec<u8>,
    buf: Vec<u8>,
}

impl Filter {
    pub(crate) fn new(kind: Kind) -> Self {
        Self {
            kind,
            pos: 0,
            prev_mask: 0,
            prev_pos: 0u32.wrapping_sub(5),
            history: [0; 256],
            delta_pos: 0,
            carry: Vec::new(),
            buf: Vec::new(),
        }
    }

    /// Filters the next piece of the stream, handing what's final to `out` (a few bytes may
    /// wait for the next piece, or [`Filter::finish`]).
    pub(crate) fn write<E>(&mut self, data: &[u8], out: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        buf.extend_from_slice(&self.carry);
        buf.extend_from_slice(data);
        let done = self.code(&mut buf);
        self.carry.clear();
        self.carry.extend_from_slice(&buf[done..]);
        let r = out(&buf[..done]);
        self.buf = buf;
        r
    }

    /// The stream's last bytes, which no instruction can complete: as they are.
    pub(crate) fn finish<E>(&mut self, out: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        let carry = std::mem::take(&mut self.carry);
        if carry.is_empty() { Ok(()) } else { out(&carry) }
    }

    /// Filters `buf` in place up to where an instruction could still run past its end;
    /// returns how far that is.
    fn code(&mut self, buf: &mut [u8]) -> usize {
        let now = self.pos;
        let done = match self.kind {
            Kind::X86 => return self.x86(buf),
            Kind::Arm => arm(now, buf),
            Kind::ArmThumb => arm_thumb(now, buf),
            Kind::Arm64 => arm64(now, buf),
            Kind::PowerPc => powerpc(now, buf),
            Kind::Sparc => sparc(now, buf),
            Kind::Ia64 => ia64(now, buf),
            Kind::RiscV => riscv(now, buf),
            Kind::Delta(distance) => {
                for b in buf.iter_mut() {
                    *b = b.wrapping_add(self.history[(distance + usize::from(self.delta_pos)) & 0xff]);
                    self.history[usize::from(self.delta_pos)] = *b;
                    self.delta_pos = self.delta_pos.wrapping_sub(1);
                }
                buf.len()
            }
        };
        self.pos = now.wrapping_add(done as u32);
        done
    }

    fn x86(&mut self, buf: &mut [u8]) -> usize {
        const ALLOWED: [bool; 8] = [true, true, true, false, true, false, false, false];
        const BIT: [u32; 8] = [0, 1, 2, 2, 3, 3, 3, 3];
        if buf.len() < 5 {
            return 0;
        }
        let now = self.pos;
        let (mut prev_mask, mut prev_pos) = (self.prev_mask, self.prev_pos);
        if now.wrapping_sub(prev_pos) > 5 {
            prev_pos = now.wrapping_sub(5);
        }
        let limit = buf.len() - 5;
        let mut i = 0;
        while i <= limit {
            // Skip to the next E8 or E9, 8 bytes at a time.
            while i + 8 <= limit {
                let x = u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
                // Zero bytes where x & 0xFE was E8.
                let y = (x & 0xfefe_fefe_fefe_fefe) ^ 0xe8e8_e8e8_e8e8_e8e8;
                let found = y.wrapping_sub(0x0101_0101_0101_0101) & !y & 0x8080_8080_8080_8080;
                if found != 0 {
                    i += (found.trailing_zeros() / 8) as usize;
                    break;
                }
                i += 8;
            }
            let b = buf[i];
            if b != 0xe8 && b != 0xe9 {
                i += 1;
                continue;
            }
            let here = now.wrapping_add(i as u32);
            let offset = here.wrapping_sub(prev_pos);
            prev_pos = here;
            if offset > 5 {
                prev_mask = 0;
            } else {
                for _ in 0..offset {
                    prev_mask &= 0x77;
                    prev_mask <<= 1;
                }
            }
            let b = buf[i + 4];
            if top_byte(b) && ALLOWED[((prev_mask >> 1) & 7) as usize] && (prev_mask >> 1) < 0x10 {
                let mut src = u32::from_le_bytes([buf[i + 1], buf[i + 2], buf[i + 3], b]);
                let mut dest;
                loop {
                    dest = src.wrapping_sub(here.wrapping_add(5));
                    if prev_mask == 0 {
                        break;
                    }
                    let n = BIT[(prev_mask >> 1) as usize];
                    if !top_byte((dest >> (24 - n * 8)) as u8) {
                        break;
                    }
                    src = dest ^ ((1u32 << (32 - n * 8)) - 1);
                }
                let d = dest.to_le_bytes();
                buf[i + 1] = d[0];
                buf[i + 2] = d[1];
                buf[i + 3] = d[2];
                buf[i + 4] = (!(((dest >> 24) & 1).wrapping_sub(1))) as u8;
                i += 5;
                prev_mask = 0;
            } else {
                i += 1;
                prev_mask |= 1;
                if top_byte(b) {
                    prev_mask |= 0x10;
                }
            }
        }
        self.prev_mask = prev_mask;
        self.prev_pos = prev_pos;
        self.pos = now.wrapping_add(i as u32);
        i
    }
}

/// Filters read from `inner` (for 7z blocks decoded as a chain of readers).
pub(crate) struct FilterReader<R> {
    inner: R,
    f: Filter,
    /// `out[at..]` is ready to be read.
    out: Vec<u8>,
    at: usize,
    eof: bool,
}

impl<R: std::io::Read> FilterReader<R> {
    pub(crate) fn new(inner: R, kind: Kind) -> Self {
        Self {
            inner,
            f: Filter::new(kind),
            out: Vec::new(),
            at: 0,
            eof: false,
        }
    }
}

impl<R: std::io::Read> std::io::Read for FilterReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.at == self.out.len() {
            if self.eof {
                return Ok(0);
            }
            let mut piece = vec![0; 64 << 10];
            let n = self.inner.read(&mut piece)?;
            self.out.clear();
            self.at = 0;
            let out = &mut self.out;
            let mut keep = |d: &[u8]| -> Result<(), ()> {
                out.extend_from_slice(d);
                Ok(())
            };
            if n == 0 {
                self.eof = true;
                let _ = self.f.finish(&mut keep);
            } else {
                let _ = self.f.write(&piece[..n], &mut keep);
            }
        }
        let n = buf.len().min(self.out.len() - self.at);
        buf[..n].copy_from_slice(&self.out[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

fn read32le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

fn read32be(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

fn arm(now: u32, buf: &mut [u8]) -> usize {
    let size = buf.len() & !3;
    let mut i = 0;
    while i < size {
        if buf[i + 3] == 0xeb {
            let src = (u32::from(buf[i + 2]) << 16 | u32::from(buf[i + 1]) << 8 | u32::from(buf[i])) << 2;
            let dest = src.wrapping_sub(now.wrapping_add(i as u32 + 8)) >> 2;
            buf[i + 2] = (dest >> 16) as u8;
            buf[i + 1] = (dest >> 8) as u8;
            buf[i] = dest as u8;
        }
        i += 4;
    }
    i
}

fn arm_thumb(now: u32, buf: &mut [u8]) -> usize {
    if buf.len() < 4 {
        return 0;
    }
    let size = buf.len() - 4;
    let mut i = 0;
    while i <= size {
        if buf[i + 1] & 0xf8 == 0xf0 && buf[i + 3] & 0xf8 == 0xf8 {
            let src = ((u32::from(buf[i + 1]) & 7) << 19 | u32::from(buf[i]) << 11 | (u32::from(buf[i + 3]) & 7) << 8 | u32::from(buf[i + 2])) << 1;
            let dest = src.wrapping_sub(now.wrapping_add(i as u32 + 4)) >> 1;
            buf[i + 1] = 0xf0 | ((dest >> 19) & 7) as u8;
            buf[i] = (dest >> 11) as u8;
            buf[i + 3] = 0xf8 | ((dest >> 8) & 7) as u8;
            buf[i + 2] = dest as u8;
            i += 2;
        }
        i += 2;
    }
    i
}

fn arm64(now: u32, buf: &mut [u8]) -> usize {
    let size = buf.len() & !3;
    let mut i = 0;
    while i < size {
        let pc = now.wrapping_add(i as u32);
        let mut instr = read32le(&buf[i..]);
        if instr >> 26 == 0x25 {
            // BL.
            let src = instr;
            instr = 0x9400_0000 | (src.wrapping_add(0u32.wrapping_sub(pc >> 2)) & 0x03ff_ffff);
            buf[i..i + 4].copy_from_slice(&instr.to_le_bytes());
        } else if instr & 0x9f00_0000 == 0x9000_0000 {
            // ADRP, within +/-512 MiB.
            let src = ((instr >> 29) & 3) | ((instr >> 3) & 0x001f_fffc);
            if src.wrapping_add(0x0002_0000) & 0x001c_0000 == 0 {
                instr &= 0x9000_001f;
                let dest = src.wrapping_add(0u32.wrapping_sub(pc >> 12));
                instr |= (dest & 3) << 29;
                instr |= (dest & 0x0003_fffc) << 3;
                instr |= 0u32.wrapping_sub(dest & 0x0002_0000) & 0x00e0_0000;
                buf[i..i + 4].copy_from_slice(&instr.to_le_bytes());
            }
        }
        i += 4;
    }
    i
}

fn powerpc(now: u32, buf: &mut [u8]) -> usize {
    let size = buf.len() & !3;
    let mut i = 0;
    while i < size {
        // Branch: 6 bits 18 (opcode), 24 offset, absolute bit 0, link bit 1.
        if buf[i] >> 2 == 0x12 && buf[i + 3] & 3 == 1 {
            let src = (u32::from(buf[i]) & 3) << 24 | u32::from(buf[i + 1]) << 16 | u32::from(buf[i + 2]) << 8 | (u32::from(buf[i + 3]) & !3);
            let dest = src.wrapping_sub(now.wrapping_add(i as u32));
            buf[i] = 0x48 | ((dest >> 24) & 3) as u8;
            buf[i + 1] = (dest >> 16) as u8;
            buf[i + 2] = (dest >> 8) as u8;
            buf[i + 3] = (buf[i + 3] & 3) | dest as u8;
        }
        i += 4;
    }
    i
}

fn sparc(now: u32, buf: &mut [u8]) -> usize {
    let size = buf.len() & !3;
    let mut i = 0;
    while i < size {
        if (buf[i] == 0x40 && buf[i + 1] & 0xc0 == 0) || (buf[i] == 0x7f && buf[i + 1] & 0xc0 == 0xc0) {
            let src = read32be(&buf[i..]) << 2;
            let dest = src.wrapping_sub(now.wrapping_add(i as u32)) >> 2;
            let dest = ((0u32.wrapping_sub((dest >> 22) & 1) << 22) & 0x3fff_ffff) | (dest & 0x3f_ffff) | 0x4000_0000;
            buf[i..i + 4].copy_from_slice(&dest.to_be_bytes());
        }
        i += 4;
    }
    i
}

fn ia64(now: u32, buf: &mut [u8]) -> usize {
    const BRANCH_TABLE: [u32; 32] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 6, 6, 0, 0, 7, 7, 4, 4, 0, 0, 4, 4, 0, 0,
    ];
    let size = buf.len() & !15;
    let mut i = 0;
    while i < size {
        let mask = BRANCH_TABLE[usize::from(buf[i] & 0x1f)];
        let mut bit_pos = 5u32;
        for slot in 0..3 {
            if (mask >> slot) & 1 != 0 {
                let byte_pos = (bit_pos >> 3) as usize;
                let bit_res = bit_pos & 7;
                let mut instruction = 0u64;
                for j in 0..6 {
                    instruction |= u64::from(buf[i + j + byte_pos]) << (8 * j);
                }
                let mut norm = instruction >> bit_res;
                if (norm >> 37) & 0xf == 5 && (norm >> 9) & 7 == 0 {
                    let mut src = ((norm >> 13) & 0xfffff) as u32;
                    src |= (((norm >> 36) & 1) as u32) << 20;
                    src <<= 4;
                    let dest = src.wrapping_sub(now.wrapping_add(i as u32)) >> 4;
                    norm &= !(0x8f_ffffu64 << 13);
                    norm |= u64::from(dest & 0xfffff) << 13;
                    norm |= u64::from(dest & 0x10_0000) << (36 - 20);
                    instruction &= (1u64 << bit_res) - 1;
                    instruction |= norm << bit_res;
                    for j in 0..6 {
                        buf[i + j + byte_pos] = (instruction >> (8 * j)) as u8;
                    }
                }
            }
            bit_pos += 41;
        }
        i += 16;
    }
    i
}

fn riscv(now: u32, buf: &mut [u8]) -> usize {
    // Is `inst2` not the instruction that uses AUIPC `auipc`'s register?
    let not_auipc_pair = |auipc: u32, inst2: u32| ((auipc << 8) ^ inst2.wrapping_sub(3)) & 0xf8003 != 0;
    // Is it not one of the encoder's special AUIPCs (rd = x2, with the packed inst2's opcode
    // bits, for an inst2 whose rs1 isn't x0 or x2)?
    let not_special_auipc = |auipc: u32, rs1: u32| (auipc.wrapping_sub(0x3117) << 18) >= (rs1 & 0x1d);
    if buf.len() < 8 {
        return 0;
    }
    let size = buf.len() - 8;
    let mut i = 0;
    while i <= size {
        let mut inst = u32::from(buf[i]);
        if inst == 0xef {
            // JAL.
            let b1 = u32::from(buf[i + 1]);
            if b1 & 0x0d != 0 {
                i += 2;
                continue;
            }
            let b2 = u32::from(buf[i + 2]);
            let b3 = u32::from(buf[i + 3]);
            let pc = now.wrapping_add(i as u32);
            let addr = (((b1 & 0xf0) << 13) | (b2 << 9) | (b3 << 1)).wrapping_sub(pc);
            buf[i + 1] = ((b1 & 0x0f) | ((addr >> 8) & 0xf0)) as u8;
            buf[i + 2] = (((addr >> 16) & 0x0f) | ((addr >> 7) & 0x10) | ((addr << 4) & 0xe0)) as u8;
            buf[i + 3] = (((addr >> 4) & 0x7f) | ((addr >> 13) & 0x80)) as u8;
            i += 4;
        } else if inst & 0x7f == 0x17 {
            // AUIPC.
            inst |= u32::from(buf[i + 1]) << 8 | u32::from(buf[i + 2]) << 16 | u32::from(buf[i + 3]) << 24;
            let inst2;
            if inst & 0xe80 != 0 {
                let i2 = read32le(&buf[i + 4..]);
                if not_auipc_pair(inst, i2) {
                    i += 6;
                    continue;
                }
                let addr = (inst & 0xffff_f000).wrapping_add(i2 >> 20);
                inst = 0x17 | (2 << 7) | (i2 << 12);
                inst2 = addr;
            } else {
                let rs1 = inst >> 27;
                if not_special_auipc(inst, rs1) {
                    i += 4;
                    continue;
                }
                let addr = read32be(&buf[i + 4..]).wrapping_sub(now.wrapping_add(i as u32));
                inst2 = (inst >> 12) | (addr << 20);
                inst = 0x17 | (rs1 << 7) | (addr.wrapping_add(0x800) & 0xffff_f000);
            }
            buf[i..i + 4].copy_from_slice(&inst.to_le_bytes());
            buf[i + 4..i + 8].copy_from_slice(&inst2.to_le_bytes());
            i += 8;
        } else {
            i += 2;
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    /// x86-like bytes: plenty of E8/E9 with small relative targets, as in real code.
    fn code_like(n: usize) -> Vec<u8> {
        let mut x = 99u64;
        let mut v = Vec::with_capacity(n);
        while v.len() < n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            match x % 9 {
                0 => {
                    v.push(0xe8);
                    v.extend_from_slice(&((x >> 20) as i32 % 100_000).to_le_bytes());
                }
                1 => v.push(0xe9),
                2 => v.extend_from_slice(&[0, 0, 0xff]),
                _ => v.push((x >> 8) as u8),
            }
        }
        v.truncate(n);
        v
    }

    fn encode(data: &[u8], kind: Kind) -> Vec<u8> {
        use liblzma::stream::{Action, Filters, LzmaOptions, Status, Stream};
        let mut filters = Filters::new();
        match kind {
            Kind::X86 => filters.x86(),
            Kind::Arm => filters.arm(),
            Kind::ArmThumb => filters.arm_thumb(),
            Kind::Arm64 => filters.arm64(),
            Kind::PowerPc => filters.powerpc(),
            Kind::Sparc => filters.sparc(),
            Kind::Ia64 => filters.ia64(),
            Kind::RiscV => filters.riscv(),
            Kind::Delta(d) => filters.delta_properties(&[(d - 1) as u8]).unwrap(),
        };
        filters.lzma2(&LzmaOptions::new_preset(0).unwrap());
        let mut s = Stream::new_raw_encoder(&filters).unwrap();
        let mut out = Vec::new();
        let mut input = data;
        loop {
            let before = s.total_in();
            out.reserve(1 << 16);
            let status = s.process_vec(input, &mut out, Action::Finish).unwrap();
            input = &input[(s.total_in() - before) as usize..];
            if status == Status::StreamEnd {
                return out;
            }
        }
    }

    #[test]
    fn undoes_liblzma_filters() {
        let kinds = [
            Kind::X86,
            Kind::Arm,
            Kind::ArmThumb,
            Kind::Arm64,
            Kind::PowerPc,
            Kind::Sparc,
            Kind::Ia64,
            Kind::RiscV,
            Kind::Delta(1),
            Kind::Delta(4),
            Kind::Delta(256),
        ];
        for kind in kinds {
            for n in [0, 4, 5, 100, 1_000_000] {
                let data = code_like(n);
                let packed = encode(&data, kind);
                let mut lz = Vec::new();
                crate::lzma::decode_lzma2(&packed, 1 << 20, &mut |b: &[u8]| -> Result<(), ()> {
                    lz.extend_from_slice(b);
                    Ok(())
                })
                .unwrap();
                // The encoder converted something (or the test checks nothing).
                assert!(n < 100_000 || lz != data, "{kind:?}: nothing filtered");
                // Pieces of awkward sizes: instructions straddle them.
                for piece in [1, 3, 7, 4096, 1 << 20] {
                    let mut f = Filter::new(kind);
                    let mut got = Vec::new();
                    let mut out = |b: &[u8]| -> Result<(), ()> {
                        got.extend_from_slice(b);
                        Ok(())
                    };
                    for p in lz.chunks(piece) {
                        f.write(p, &mut out).unwrap();
                    }
                    f.finish(&mut out).unwrap();
                    assert!(got == data, "{kind:?}: {n} bytes in pieces of {piece}");
                }
                let mut got = Vec::new();
                std::io::Read::read_to_end(&mut FilterReader::new(&lz[..], kind), &mut got).unwrap();
                assert!(got == data, "{kind:?}: {n} bytes, reader");
            }
        }
    }
}
