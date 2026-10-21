//! RAR5 encryption: AES-256 in CBC mode, keys from PBKDF2-HMAC-SHA256.
//!
//! One key derivation gives three values, by running the PBKDF2 loop on past the key's
//! iteration count: 16 more iterations give the key that turns CRCs into MACs, 16 more the
//! value the password check is folded from. Decryption is random access (each plaintext block
//! needs only its own and the previous ciphertext block), so any range of a file's data can be
//! read on its own, in parallel.

use aes::{
    Aes256, Block,
    cipher::{BlockCipherDecrypt, KeyInit},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The most PBKDF2 iterations (as a power of 2) unRAR accepts.
pub(crate) const MAX_LG2_COUNT: u8 = 24;

pub(crate) struct Keys {
    aes: Aes256,
    /// Turns CRC32s into MACs (archives made with "tweaked" checksums).
    hash_key: [u8; 32],
    /// Compared with the check value stored in the archive.
    pub check: [u8; 8],
}

impl Keys {
    /// Derives the keys for `password` with this salt and 2^`lg2_count` iterations.
    pub(crate) fn derive(password: &str, salt: &[u8; 16], lg2_count: u8) -> Self {
        let mac = Hmac::<Sha256>::new_from_slice(password.as_bytes()).expect("HMAC takes any key length");
        let hmac = |data: &[u8]| -> [u8; 32] {
            let mut m = mac.clone();
            m.update(data);
            m.finalize().into_bytes().into()
        };
        let mut first = [0u8; 20];
        first[..16].copy_from_slice(salt);
        first[16..].copy_from_slice(&1u32.to_be_bytes());
        let mut u = hmac(&first);
        let mut acc = u;
        let mut values = [[0u8; 32]; 3];
        for (value, rounds) in values.iter_mut().zip([(1u32 << lg2_count) - 1, 16, 16]) {
            for _ in 0..rounds {
                u = hmac(&u);
                acc.iter_mut().zip(&u).for_each(|(a, b)| *a ^= b);
            }
            *value = acc;
        }
        let mut check = [0u8; 8];
        for (i, b) in values[2].iter().enumerate() {
            check[i % 8] ^= b;
        }
        Self {
            aes: Aes256::new_from_slice(&values[0]).expect("a 32-byte key"),
            hash_key: values[1],
            check,
        }
    }

    /// What an archive with MAC checksums stores for data whose CRC32 is `crc`.
    pub(crate) fn mac(&self, crc: u32) -> u32 {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.hash_key).expect("HMAC takes any key length");
        m.update(&crc.to_le_bytes());
        let digest = m.finalize().into_bytes();
        digest.iter().enumerate().fold(0u32, |v, (i, &b)| v ^ u32::from(b) << ((i & 3) * 8))
    }

    /// What an archive with MAC checksums stores for data whose BLAKE2sp digest is `digest`.
    pub(crate) fn mac_digest(&self, digest: &[u8; 32]) -> [u8; 32] {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.hash_key).expect("HMAC takes any key length");
        m.update(digest);
        m.finalize().into_bytes().into()
    }

    /// Decrypts `data` (whole 16-byte blocks of ciphertext) in place; `prev` is the ciphertext
    /// block before it, or the IV at the start.
    pub(crate) fn decrypt(&self, mut prev: [u8; 16], data: &mut [u8]) {
        // Eight blocks at a time, which AES-NI pipelines; each plaintext block is XORed with
        // the ciphertext before it, which is still in `data` while going backwards.
        let mut tmp = [Block::default(); 8];
        for batch in data.chunks_mut(128) {
            let n = batch.len() / 16;
            for (t, c) in tmp.iter_mut().zip(batch.as_chunks::<16>().0) {
                t.copy_from_slice(c);
            }
            self.aes.decrypt_blocks(&mut tmp[..n]);
            let last: [u8; 16] = batch[(n - 1) * 16..n * 16].try_into().unwrap();
            for j in (0..n).rev() {
                let before: [u8; 16] = if j == 0 { prev } else { batch[(j - 1) * 16..j * 16].try_into().unwrap() };
                for (k, o) in batch[j * 16..(j + 1) * 16].iter_mut().enumerate() {
                    *o = tmp[j][k] ^ before[k];
                }
            }
            prev = last;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CBC decryption against the cbc crate's, over a range starting mid-stream.
    #[test]
    fn decrypts_any_range() {
        use aes::cipher::{BlockModeEncrypt, KeyIvInit};
        let keys = Keys::derive("secret", &[7; 16], 4);
        let iv = [3u8; 16];
        let plain: Vec<u8> = (0..16 * 37).map(|i| (i * 31 % 251) as u8).collect();
        let mut ct = plain.clone();
        let key = {
            // Re-derive the raw AES key the same way to encrypt with the cbc crate.
            let mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
            let mut first = [7u8; 20];
            first[16..].copy_from_slice(&1u32.to_be_bytes());
            let mut m = mac.clone();
            m.update(&first);
            let mut u: [u8; 32] = m.finalize().into_bytes().into();
            let mut acc = u;
            for _ in 0..15 {
                let mut m = mac.clone();
                m.update(&u);
                u = m.finalize().into_bytes().into();
                acc.iter_mut().zip(&u).for_each(|(a, b)| *a ^= b);
            }
            acc
        };
        cbc::Encryptor::<Aes256>::new_from_slices(&key, &iv)
            .unwrap()
            .encrypt_padded::<aes::cipher::block_padding::NoPadding>(&mut ct, plain.len())
            .unwrap();
        let mut all = ct.clone();
        keys.decrypt(iv, &mut all);
        assert_eq!(all, plain);
        let mut part = ct[16 * 9..16 * 30].to_vec();
        keys.decrypt(ct[16 * 8..16 * 9].try_into().unwrap(), &mut part);
        assert_eq!(part, plain[16 * 9..16 * 30]);
    }
}
