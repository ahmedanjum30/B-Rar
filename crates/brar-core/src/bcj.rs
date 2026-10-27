//! The x86 BCJ filter's decoder (7z method 03030103, which 7-Zip adds for archives with
//! programs in them). Compressors do better on machine code whose CALL (E8) and JMP (E9)
//! targets are absolute rather than relative, so the encoder turned the 4-byte operands after
//! those opcodes absolute; this turns them back. It follows the reference algorithm (xz's
//! `simple/x86.c`, from the LZMA SDK), byte for byte.

/// Whether `b` is the top byte of an operand the encoder would have converted.
fn top_byte(b: u8) -> bool {
    b == 0 || b == 0xff
}

/// The decoder's state between pieces of output.
pub(crate) struct X86 {
    prev_mask: u32,
    prev_pos: u32,
    /// Output position of the next byte to filter.
    pos: u32,
    /// Bytes filtered so far that wait for the next piece, which may convert them.
    carry: Vec<u8>,
    buf: Vec<u8>,
}

impl X86 {
    pub(crate) fn new() -> Self {
        Self {
            prev_mask: 0,
            prev_pos: 0u32.wrapping_sub(5),
            pos: 0,
            carry: Vec::new(),
            buf: Vec::new(),
        }
    }

    /// Filters the next piece of the stream, handing what's final to `out` (up to 4 bytes
    /// wait for the next piece, or [`X86::finish`]).
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

    /// The stream's last bytes, which no operand can follow.
    pub(crate) fn finish<E>(&mut self, out: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        let carry = std::mem::take(&mut self.carry);
        if carry.is_empty() { Ok(()) } else { out(&carry) }
    }

    /// Filters `buf` in place up to where an operand could still run past its end; returns
    /// how far that is.
    fn code(&mut self, buf: &mut [u8]) -> usize {
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

    fn encode(data: &[u8]) -> Vec<u8> {
        use liblzma::stream::{Action, Filters, LzmaOptions, Status, Stream};
        let mut filters = Filters::new();
        filters.x86();
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
    fn undoes_liblzma_x86_filter() {
        for n in [0, 4, 5, 100, 1_000_000] {
            let data = code_like(n);
            let packed = encode(&data);
            // Pieces of awkward sizes: operands straddle them.
            for piece in [1, 3, 7, 4096, 1 << 20] {
                let mut lz = Vec::new();
                crate::lzma::decode_lzma2(&packed, 1 << 20, &mut |b: &[u8]| -> Result<(), ()> {
                    lz.extend_from_slice(b);
                    Ok(())
                })
                .unwrap();
                let mut f = X86::new();
                let mut got = Vec::new();
                let mut out = |b: &[u8]| -> Result<(), ()> {
                    got.extend_from_slice(b);
                    Ok(())
                };
                for p in lz.chunks(piece) {
                    f.write(p, &mut out).unwrap();
                }
                f.finish(&mut out).unwrap();
                assert!(got == data, "{n} bytes in pieces of {piece}");
            }
        }
    }
}
