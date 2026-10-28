//! ZIP entries the zip crate leaves to B-Rar: LZMA (method 14, decoded with B-Rar's LZMA
//! decoder) and XZ (95, liblzma), plain or encrypted (ZipCrypto, or WinZip AES), read
//! straight from the archive's bytes.

use std::io::{self, Read};

use aes::{
    Aes128, Aes192, Aes256,
    cipher::{BlockCipherEncrypt, KeyInit},
};
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::{Error, Result, lzma};

pub(crate) const LZMA: u16 = 14;
pub(crate) const XZ: u16 = 95;

/// How an entry is encrypted.
pub(crate) enum Crypt {
    None,
    /// ZipCrypto, with the byte its header's last byte must be (the password check).
    ZipCrypto(u8),
    /// WinZip AES, with its strength (1, 2, 3: 128, 192, 256 bits).
    Aes(u8),
}

/// The method and encryption of an entry, from its central directory record: (method, crypt).
/// WinZip AES entries say 99 and give the real method in their extra field (0x9901).
pub(crate) fn method(method: u16, encrypted: bool, extra: &[u8], check: u8) -> (u16, Crypt) {
    let mut at = 0;
    while at + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[at], extra[at + 1]]);
        let len = usize::from(u16::from_le_bytes([extra[at + 2], extra[at + 3]]));
        let body = &extra[(at + 4).min(extra.len())..(at + 4 + len).min(extra.len())];
        if id == 0x9901 && body.len() >= 7 {
            return (u16::from_le_bytes([body[5], body[6]]), Crypt::Aes(body[4]));
        }
        at += 4 + len;
    }
    (method, if encrypted { Crypt::ZipCrypto(check) } else { Crypt::None })
}

fn bad_password() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "wrong password")
}

/// The entry's data, decrypted and decompressed: `raw` is its bytes in the archive.
pub(crate) fn reader<'a>(raw: Box<dyn Read + 'a>, method: u16, crypt: &Crypt, password: Option<&[u8]>, size: u64) -> Result<Box<dyn Read + 'a>> {
    let mut data: Box<dyn Read + 'a> = match crypt {
        Crypt::None => raw,
        Crypt::ZipCrypto(check) => Box::new(ZipCrypto::new(raw, password.ok_or(Error::PasswordRequired)?, *check).map_err(|_| Error::WrongPassword)?),
        Crypt::Aes(strength) => Box::new(
            WinZipAes::new(raw, password.ok_or(Error::PasswordRequired)?, *strength).map_err(|e| match e.kind() {
                io::ErrorKind::PermissionDenied => Error::WrongPassword,
                _ => Error::RawIo(io::Error::new(io::ErrorKind::InvalidData, e)),
            })?,
        ),
    };
    Ok(match method {
        LZMA => {
            // The LZMA SDK's version (2 bytes), the properties' size, the properties.
            let mut head = [0u8; 4];
            data.read_exact(&mut head)?;
            let mut props = vec![0u8; usize::from(u16::from_le_bytes([head[2], head[3]]))];
            data.read_exact(&mut props)?;
            Box::new(lzma::LzmaReader::new(data, &props, size)?)
        }
        XZ => Box::new(liblzma::read::XzDecoder::new_multi_decoder(data)),
        _ => return Err(Error::Unsupported("ZIP compression method")),
    })
}

/// The CRC-32 table ZipCrypto's keys use.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// PKWARE's traditional encryption (APPNOTE 6.1).
struct ZipCrypto<R> {
    inner: R,
    keys: [u32; 3],
}

impl<R: Read> ZipCrypto<R> {
    fn new(mut inner: R, password: &[u8], check: u8) -> io::Result<Self> {
        let mut keys = [0x1234_5678, 0x2345_6789, 0x3456_7890];
        for &b in password {
            update(&mut keys, b);
        }
        let mut header = [0u8; 12];
        inner.read_exact(&mut header)?;
        for b in &mut header {
            *b ^= stream_byte(&keys);
            update(&mut keys, *b);
        }
        if header[11] != check {
            return Err(bad_password());
        }
        Ok(Self { inner, keys })
    }
}

fn update(keys: &mut [u32; 3], b: u8) {
    let crc = |c: u32, b: u8| (c >> 8) ^ CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize];
    keys[0] = crc(keys[0], b);
    keys[1] = keys[1].wrapping_add(keys[0] & 0xff).wrapping_mul(134_775_813).wrapping_add(1);
    keys[2] = crc(keys[2], (keys[1] >> 24) as u8);
}

fn stream_byte(keys: &[u32; 3]) -> u8 {
    let t = (keys[2] | 2) & 0xffff;
    ((t * (t ^ 1)) >> 8) as u8
}

impl<R: Read> Read for ZipCrypto<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        for b in &mut buf[..n] {
            *b ^= stream_byte(&self.keys);
            update(&mut self.keys, *b);
        }
        Ok(n)
    }
}

/// WinZip AES's keys for `password` and `salt`: PBKDF2-HMAC-SHA1, 1000 rounds, giving the AES
/// key, the HMAC key (`key_len` bytes each) and the 2-byte password check.
pub(crate) fn winzip_keys(password: &[u8], salt: &[u8], key_len: usize) -> Vec<u8> {
    let mut derived = vec![0u8; 2 * key_len + 2];
    let prf = Hmac::<Sha1>::new_from_slice(password).expect("HMAC takes any key length");
    for (i, block) in derived.chunks_mut(20).enumerate() {
        let mut m = prf.clone();
        m.update(salt);
        m.update(&(i as u32 + 1).to_be_bytes());
        let mut u: [u8; 20] = m.finalize().into_bytes().into();
        let mut acc = u;
        for _ in 1..1000 {
            let mut m = prf.clone();
            m.update(&u);
            u = m.finalize().into_bytes().into();
            acc.iter_mut().zip(&u).for_each(|(a, b)| *a ^= b);
        }
        block.copy_from_slice(&acc[..block.len()]);
    }
    derived
}

enum AesKey {
    K128(Aes128),
    K192(Aes192),
    K256(Aes256),
}

/// WinZip's AES encryption (AE-1 and AE-2): AES in CTR mode (a little-endian counter from 1),
/// keys from PBKDF2-HMAC-SHA1, and an HMAC-SHA1 of the ciphertext after it, checked at the end.
struct WinZipAes<R> {
    inner: R,
    key: AesKey,
    mac: Hmac<Sha1>,
    counter: u64,
    /// Keystream bytes not used yet.
    stream: [u8; 16],
    used: usize,
    /// The last 10 bytes read (the authentication code, at the end), and how many are held.
    tail: [u8; 10],
    held: usize,
    done: bool,
}

impl<R: Read> WinZipAes<R> {
    fn new(mut inner: R, password: &[u8], strength: u8) -> io::Result<Self> {
        let key_len = match strength {
            1 => 16,
            2 => 24,
            3 => 32,
            _ => return Err(io::ErrorKind::InvalidData.into()),
        };
        // Salt: half the key length.
        let mut salt = vec![0u8; key_len / 2];
        inner.read_exact(&mut salt)?;
        let mut verifier = [0u8; 2];
        inner.read_exact(&mut verifier)?;
        let derived = winzip_keys(password, &salt, key_len);
        if derived[2 * key_len..] != verifier {
            return Err(bad_password());
        }
        let k = &derived[..key_len];
        let key = match key_len {
            16 => AesKey::K128(Aes128::new_from_slice(k).unwrap()),
            24 => AesKey::K192(Aes192::new_from_slice(k).unwrap()),
            _ => AesKey::K256(Aes256::new_from_slice(k).unwrap()),
        };
        let mac = Hmac::<Sha1>::new_from_slice(&derived[key_len..2 * key_len]).unwrap();
        Ok(Self {
            inner,
            key,
            mac,
            counter: 0,
            stream: [0; 16],
            used: 16,
            tail: [0; 10],
            held: 0,
            done: false,
        })
    }

    fn next_stream(&mut self) {
        self.counter += 1;
        let mut block = [0u8; 16];
        block[..8].copy_from_slice(&self.counter.to_le_bytes());
        let b = (&mut block).into();
        match &self.key {
            AesKey::K128(k) => k.encrypt_block(b),
            AesKey::K192(k) => k.encrypt_block(b),
            AesKey::K256(k) => k.encrypt_block(b),
        }
        self.stream = block;
        self.used = 0;
    }
}

impl<R: Read> Read for WinZipAes<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        // Read ahead of what's handed out by the 10 bytes of the code at the end.
        let mut tmp = vec![0u8; buf.len().min(1 << 16) + 10];
        tmp[..self.held].copy_from_slice(&self.tail[..self.held]);
        let mut have = self.held;
        while have < 11 {
            match self.inner.read(&mut tmp[have..])? {
                0 => break,
                n => have += n,
            }
        }
        if have <= 10 {
            // The end: what's held is the authentication code.
            self.done = true;
            let code = self.mac.clone().finalize().into_bytes();
            if have != 10 || code[..10] != tmp[..10] {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "damaged (authentication code)"));
            }
            return Ok(0);
        }
        let n = have - 10;
        self.tail.copy_from_slice(&tmp[n..have]);
        self.held = 10;
        self.mac.update(&tmp[..n]);
        for (i, c) in tmp[..n].iter().enumerate() {
            if self.used == 16 {
                self.next_stream();
            }
            buf[i] = c ^ self.stream[self.used];
            self.used += 1;
        }
        Ok(n)
    }
}
