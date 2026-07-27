//! Pure-Rust LZMA2 via `lzma-rust2`.

use super::{dict_size_for_level, lzma2_dict_prop, Lzma2Codec, Lzma2Compressed};
use crate::error::{Error, Result};
use lzma_rust2::{Lzma2Options, Lzma2Writer};
use std::io::Write;

pub struct PureRustCodec;

impl Lzma2Codec for PureRustCodec {
    fn name(&self) -> &'static str {
        "pure-rust"
    }

    fn compress(&self, input: &[u8], level: u32) -> Result<Lzma2Compressed> {
        let level = level.min(9);
        let mut options = Lzma2Options::with_preset(level);
        let dict = dict_size_for_level(level);
        options.lzma_options.dict_size = dict;
        let props = lzma2_dict_prop(dict);

        let mut data = Vec::with_capacity(input.len() / 2 + 64);
        {
            let mut enc = Lzma2Writer::new(&mut data, options);
            enc.write_all(input)
                .map_err(|e| Error::Other(format!("pure-rust lzma2 write: {e}")))?;
            enc.finish()
                .map_err(|e| Error::Other(format!("pure-rust lzma2 finish: {e}")))?;
        }
        Ok(Lzma2Compressed {
            data,
            props,
            crc32: crc32fast::hash(input),
            uncompressed_size: input.len() as u64,
        })
    }
}
