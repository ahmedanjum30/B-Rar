//! B-Rar's own Deflate decoder (RFC 1951), written for speed and for decoding one stream on
//! several cores: a block can be decoded without the output before it, leaving markers for the
//! bytes it would have copied from there (see the parallel decoder).
//!
//! Layout follows the RAR5 decoder's: a 64-bit bit buffer refilled without branches, and
//! Huffman tables that answer a code in one lookup for all but the longest codes.

pub(crate) mod chunk;
pub(crate) mod inflate;
pub(crate) mod parallel;
