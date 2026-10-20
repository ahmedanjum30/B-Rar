use std::{fs::File, io::Read, path::Path};

use crate::{Error, IoContext, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    SevenZ,
    Rar,
    /// A tar archive or a single compressed file, possibly wrapped in a codec.
    Stream(Codec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Uncompressed tar.
    None,
    Gzip,
    Xz,
    Zstd,
    Bzip2,
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Format::Zip => "zip",
            Format::SevenZ => "7z",
            Format::Rar => "rar",
            Format::Stream(Codec::None) => "tar",
            Format::Stream(Codec::Gzip) => "gzip",
            Format::Stream(Codec::Xz) => "xz",
            Format::Stream(Codec::Zstd) => "zstd",
            Format::Stream(Codec::Bzip2) => "bzip2",
        })
    }
}

/// Identifies an archive by its magic bytes, never by its extension.
pub fn detect(path: &Path) -> Result<Format> {
    let mut head = [0u8; 512];
    let mut file = File::open(path).at(path)?;
    let mut len = 0;
    while len < head.len() {
        match file.read(&mut head[len..]).at(path)? {
            0 => break,
            n => len += n,
        }
    }
    detect_bytes(&head[..len]).ok_or(Error::UnknownFormat)
}

pub(crate) fn detect_bytes(head: &[u8]) -> Option<Format> {
    let format = match head {
        [b'P', b'K', 3, 4, ..] | [b'P', b'K', 5, 6, ..] | [b'P', b'K', 7, 8, ..] => Format::Zip,
        [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, ..] => Format::SevenZ,
        [b'R', b'a', b'r', b'!', 0x1A, 0x07, ..] => Format::Rar,
        [0x1F, 0x8B, ..] => Format::Stream(Codec::Gzip),
        [0xFD, b'7', b'z', b'X', b'Z', 0, ..] => Format::Stream(Codec::Xz),
        [0x28, 0xB5, 0x2F, 0xFD, ..] => Format::Stream(Codec::Zstd),
        [b'B', b'Z', b'h', ..] => Format::Stream(Codec::Bzip2),
        _ if is_tar(head) => Format::Stream(Codec::None),
        _ => return None,
    };
    Some(format)
}

pub(crate) fn is_tar(head: &[u8]) -> bool {
    head.get(257..262) == Some(b"ustar")
}
