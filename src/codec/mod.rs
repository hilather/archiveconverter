//! Pluggable LZMA2 codecs for Phase 3 (pure Rust vs liblzma).
//!
//! Codecs produce **raw LZMA2** streams (method id `0x21`) suitable for embedding
//! in non-solid 7z pack streams, plus the 1-byte 7z LZMA2 properties byte.

mod dir_writer;
mod liblzma_codec;
mod pure_rust;
mod sevenz_header;
mod store_writer;
mod tar_writer;
mod writer;

pub use dir_writer::{count_dir_files, default_dir_from_input, DirOuterWriter};
pub use liblzma_codec::LibLzmaCodec;
pub use pure_rust::PureRustCodec;
pub use sevenz_header::{system_time_to_filetime, FileMeta, ATTR_FILE};
pub use store_writer::{NonsolidStoreWriter, OuterFormat, SyncedOuterWriter};
pub use tar_writer::{count_tar_files, TarOuterWriter};
pub use writer::{write_nonsolid_lzma2, NonsolidLzma2Writer, PackedEntry};

use crate::error::Result;

/// Which LZMA2 implementation to use for native parallel encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodecKind {
    /// `lzma-rust2` (same family as sevenz-rust2).
    #[default]
    PureRust,
    /// System **liblzma** via raw encoder (usually faster).
    LibLzma,
}

impl CodecKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CodecKind::PureRust => "pure-rust",
            CodecKind::LibLzma => "liblzma",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pure-rust" | "rust" | "pure" => Some(Self::PureRust),
            "liblzma" | "lzma" | "xz" | "system" => Some(Self::LibLzma),
            _ => None,
        }
    }
}

/// Result of compressing one buffer to raw LZMA2.
#[derive(Debug, Clone)]
pub struct Lzma2Compressed {
    /// Raw LZMA2 payload (no xz container).
    pub data: Vec<u8>,
    /// 7z LZMA2 properties byte (dict size encoding).
    pub props: u8,
    /// CRC32 of **uncompressed** data.
    pub crc32: u32,
    /// Uncompressed size.
    pub uncompressed_size: u64,
}

/// Pluggable LZMA2 codec.
pub trait Lzma2Codec: Send + Sync {
    fn name(&self) -> &'static str;

    /// Compress `input` at 7z-style level 0–9.
    fn compress(&self, input: &[u8], level: u32) -> Result<Lzma2Compressed>;
}

/// Build a codec by kind.
pub fn open_codec(kind: CodecKind) -> Box<dyn Lzma2Codec> {
    match kind {
        CodecKind::PureRust => Box::new(PureRustCodec),
        CodecKind::LibLzma => Box::new(LibLzmaCodec),
    }
}

/// Encode dict_size as the single LZMA2 property byte used by 7-Zip.
pub fn lzma2_dict_prop(dict_size: u32) -> u8 {
    let dict_size = dict_size.clamp(4096, 0xFFFF_FFF0);
    let lead = dict_size.leading_zeros();
    let second_bit = (dict_size >> (30u32.wrapping_sub(lead))).wrapping_sub(2);
    (19u32.wrapping_sub(lead) * 2 + second_bit) as u8
}

/// Dict size for 7z levels roughly matching presets (powers of two).
pub fn dict_size_for_level(level: u32) -> u32 {
    let level = level.min(9);
    // Match common 7z/lzma presets roughly
    match level {
        0 => 64 * 1024,
        1 => 1 << 16,
        2 => 1 << 18,
        3 => 1 << 20,
        4 => 1 << 20,
        5 => 1 << 22,
        6 => 1 << 23,
        7 => 1 << 24,
        8 => 1 << 25,
        _ => 1 << 26,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_rust_roundtrip_prop() {
        let c = PureRustCodec;
        let msg = b"hello phase3 codec ".repeat(100);
        let out = c.compress(&msg, 1).unwrap();
        assert!(!out.data.is_empty());
        assert_eq!(out.uncompressed_size, msg.len() as u64);
        assert_ne!(out.crc32, 0);
    }

    #[test]
    fn liblzma_roundtrip_prop() {
        let c = LibLzmaCodec;
        let msg = b"hello liblzma codec ".repeat(100);
        let out = c.compress(&msg, 1).unwrap();
        assert!(!out.data.is_empty());
        assert_eq!(out.uncompressed_size, msg.len() as u64);
    }
}
