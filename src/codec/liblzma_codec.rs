//! liblzma raw LZMA2 encoder (system library via `lzma-sys`).

use super::{dict_size_for_level, lzma2_dict_prop, Lzma2Codec, Lzma2Compressed};
use crate::error::{Error, Result};
use std::ptr;

pub struct LibLzmaCodec;

impl Lzma2Codec for LibLzmaCodec {
    fn name(&self) -> &'static str {
        "liblzma"
    }

    fn compress(&self, input: &[u8], level: u32) -> Result<Lzma2Compressed> {
        let level = level.min(9);
        let dict = dict_size_for_level(level);
        let props = lzma2_dict_prop(dict);
        let data = raw_lzma2_encode(input, level, dict)?;
        Ok(Lzma2Compressed {
            data,
            props,
            crc32: crc32fast::hash(input),
            uncompressed_size: input.len() as u64,
        })
    }
}

fn raw_lzma2_encode(input: &[u8], level: u32, dict_size: u32) -> Result<Vec<u8>> {
    unsafe {
        let mut opts: lzma_sys::lzma_options_lzma = std::mem::zeroed();
        if lzma_sys::lzma_lzma_preset(&mut opts, level) != 0 {
            return Err(Error::Other("liblzma: lzma_lzma_preset failed".into()));
        }
        opts.dict_size = dict_size;

        let filters = [
            lzma_sys::lzma_filter {
                id: lzma_sys::LZMA_FILTER_LZMA2,
                options: &mut opts as *mut _ as *mut _,
            },
            lzma_sys::lzma_filter {
                id: lzma_sys::LZMA_VLI_UNKNOWN,
                options: ptr::null_mut(),
            },
        ];

        // Bound output buffer (worst case ~input + overhead)
        let bound = input.len() + input.len() / 3 + 128;
        let mut out = vec![0u8; bound];

        let mut strm: lzma_sys::lzma_stream = std::mem::zeroed();
        let ret = lzma_sys::lzma_raw_encoder(&mut strm, filters.as_ptr());
        if ret != lzma_sys::LZMA_OK {
            return Err(Error::Other(format!(
                "liblzma: lzma_raw_encoder failed ({ret})"
            )));
        }

        strm.next_in = input.as_ptr();
        strm.avail_in = input.len();
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len();

        let out_pos = loop {
            let ret = lzma_sys::lzma_code(&mut strm, lzma_sys::LZMA_FINISH);
            if ret == lzma_sys::LZMA_STREAM_END {
                break out.len() - strm.avail_out;
            }
            if ret != lzma_sys::LZMA_OK {
                lzma_sys::lzma_end(&mut strm);
                return Err(Error::Other(format!(
                    "liblzma: lzma_code failed ({ret})"
                )));
            }
            if strm.avail_out == 0 {
                // grow buffer
                let used = out.len();
                out.resize(out.len() * 2, 0);
                strm.next_out = out.as_mut_ptr().add(used);
                strm.avail_out = out.len() - used;
            }
        };
        lzma_sys::lzma_end(&mut strm);
        out.truncate(out_pos);
        Ok(out)
    }
}
