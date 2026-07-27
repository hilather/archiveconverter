//! Minimal non-solid multi-file 7z writer for precompressed LZMA2 streams.
//!
//! Supports streaming: append each compressed pack as it finishes, then write the
//! header at the end. Format subset verified with official 7zz and sevenz-rust2.

use super::Lzma2Compressed;
use crate::error::{Error, Result};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

const SIG: &[u8] = b"7z\xBC\xAF\x27\x1C";
const SIG_HEADER_SIZE: u64 = 32;

const K_END: u8 = 0x00;
const K_HEADER: u8 = 0x01;
const K_MAIN_STREAMS_INFO: u8 = 0x04;
const K_FILES_INFO: u8 = 0x05;
const K_PACK_INFO: u8 = 0x06;
const K_UNPACK_INFO: u8 = 0x07;
const K_SUB_STREAMS_INFO: u8 = 0x08;
const K_SIZE: u8 = 0x09;
const K_CRC: u8 = 0x0A;
const K_FOLDER: u8 = 0x0B;
const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
const K_NAME: u8 = 0x11;

/// One file ready to pack (already LZMA2-compressed).
pub struct PackedEntry {
    pub name: String,
    pub compressed: Lzma2Compressed,
}

/// Streaming non-solid 7z writer: packs are appended immediately; header last.
pub struct NonsolidLzma2Writer {
    file: File,
    /// Metadata only (names, props, sizes, CRCs) — not full uncompressed data.
    entries: Vec<EntryMeta>,
    pack_sizes: Vec<u64>,
    pack_crcs: Vec<u32>,
}

struct EntryMeta {
    name: String,
    props: u8,
    crc32: u32,
    uncompressed_size: u64,
}

impl NonsolidLzma2Writer {
    /// Create output path and write a placeholder start header.
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut file = File::create(path)?;
        file.write_all(&[0u8; SIG_HEADER_SIZE as usize])?;
        Ok(Self {
            file,
            entries: Vec::new(),
            pack_sizes: Vec::new(),
            pack_crcs: Vec::new(),
        })
    }

    /// Append one precompressed pack stream and record header metadata.
    ///
    /// Uncompressed data is not retained — only the compressed payload is written
    /// to disk, then dropped after this call returns.
    pub fn push_packed(&mut self, name: String, compressed: Lzma2Compressed) -> Result<()> {
        let pack_crc = crc32fast::hash(&compressed.data);
        self.pack_sizes.push(compressed.data.len() as u64);
        self.pack_crcs.push(pack_crc);
        self.file.write_all(&compressed.data)?;
        self.entries.push(EntryMeta {
            name,
            props: compressed.props,
            crc32: compressed.crc32,
            uncompressed_size: compressed.uncompressed_size,
        });
        Ok(())
    }

    pub fn push_entry(&mut self, entry: PackedEntry) -> Result<()> {
        self.push_packed(entry.name, entry.compressed)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write the end header and fix the start signature. Consumes the writer.
    pub fn finish(mut self) -> Result<()> {
        if self.entries.is_empty() {
            return Err(Error::Other("cannot write empty 7z archive".into()));
        }

        let mut header = Vec::with_capacity(64 * 1024 + self.entries.len() * 64);
        write_header(
            &mut header,
            &self.entries,
            &self.pack_sizes,
            &self.pack_crcs,
        )?;

        let header_pos = self.file.stream_position()?;
        self.file.write_all(&header)?;
        let header_crc = crc32fast::hash(&header);

        let next_header_offset = header_pos - SIG_HEADER_SIZE;
        let next_header_size = header.len() as u64;

        let mut sig = [0u8; SIG_HEADER_SIZE as usize];
        {
            let mut w = &mut sig[..];
            w.write_all(SIG)?;
            w.write_all(&[0, 4])?; // version
            w.write_all(&0u32.to_le_bytes())?; // placeholder CRC of start header
            w.write_all(&next_header_offset.to_le_bytes())?;
            w.write_all(&next_header_size.to_le_bytes())?;
            w.write_all(&header_crc.to_le_bytes())?;
        }
        let start_crc = crc32fast::hash(&sig[12..]);
        sig[8..12].copy_from_slice(&start_crc.to_le_bytes());

        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&sig)?;
        self.file.flush()?;
        Ok(())
    }
}

/// Write a non-solid multi-file 7z archive from precompressed LZMA2 streams (batch).
pub fn write_nonsolid_lzma2(path: &Path, entries: &[PackedEntry]) -> Result<()> {
    if entries.is_empty() {
        return Err(Error::Other("cannot write empty 7z archive".into()));
    }
    let mut w = NonsolidLzma2Writer::create(path)?;
    for e in entries {
        w.push_packed(
            e.name.clone(),
            Lzma2Compressed {
                data: e.compressed.data.clone(),
                props: e.compressed.props,
                crc32: e.compressed.crc32,
                uncompressed_size: e.compressed.uncompressed_size,
            },
        )?;
    }
    w.finish()
}

fn write_header(
    h: &mut Vec<u8>,
    entries: &[EntryMeta],
    pack_sizes: &[u64],
    pack_crcs: &[u32],
) -> Result<()> {
    h.push(K_HEADER);
    h.push(K_MAIN_STREAMS_INFO);

    // Pack info: pos=0 (pack data starts immediately after signature header)
    h.push(K_PACK_INFO);
    write_u64(h, 0)?; // packPos
    write_u64(h, entries.len() as u64)?; // numPackStreams
    h.push(K_SIZE);
    for s in pack_sizes {
        write_u64(h, *s)?;
    }
    h.push(K_CRC);
    h.push(1); // all defined
    for c in pack_crcs {
        h.extend_from_slice(&c.to_le_bytes());
    }
    h.push(K_END);

    // Unpack info: one folder per file (non-solid)
    h.push(K_UNPACK_INFO);
    h.push(K_FOLDER);
    write_u64(h, entries.len() as u64)?;
    h.push(0); // external = 0
    for e in entries {
        write_folder_lzma2(h, e.props)?;
    }
    h.push(K_CODERS_UNPACK_SIZE);
    for e in entries {
        write_u64(h, e.uncompressed_size)?;
    }
    h.push(K_END); // end unpack info

    // Substreams: CRCs for each file
    h.push(K_SUB_STREAMS_INFO);
    h.push(K_CRC);
    h.push(1); // all defined
    for e in entries {
        h.extend_from_slice(&e.crc32.to_le_bytes());
    }
    h.push(K_END); // end substreams
    h.push(K_END); // end main streams info

    // Files info
    h.push(K_FILES_INFO);
    write_u64(h, entries.len() as u64)?;
    h.push(K_NAME);
    let mut names = Vec::new();
    names.push(0); // external
    for e in entries {
        for c in e.name.encode_utf16() {
            names.extend_from_slice(&c.to_le_bytes());
        }
        names.extend_from_slice(&0u16.to_le_bytes());
    }
    write_u64(h, names.len() as u64)?;
    h.extend_from_slice(&names);
    h.push(K_END); // end files info
    h.push(K_END); // end header
    Ok(())
}

/// One LZMA2 coder folder (non-solid single stream).
fn write_folder_lzma2(h: &mut Vec<u8>, props: u8) -> Result<()> {
    write_u64(h, 1)?;
    let id = [0x21u8]; // LZMA2
    let props_bytes = [props];
    let flags = (id.len() as u8) & 0x0F | 0x20; // props exist
    h.push(flags);
    h.extend_from_slice(&id);
    write_u64(h, props_bytes.len() as u64)?;
    h.extend_from_slice(&props_bytes);
    Ok(())
}

/// 7z UINT64 encoding (same algorithm as sevenz-rust2).
fn write_u64(h: &mut Vec<u8>, mut value: u64) -> Result<()> {
    let mut first: u64 = 0;
    let mut mask: u64 = 0x80;
    let mut i = 0u32;
    while i < 8 {
        if value < (1u64 << (7 * (i + 1))) {
            first |= value >> (8 * i);
            break;
        }
        first |= mask;
        mask >>= 1;
        i += 1;
    }
    h.push((first & 0xFF) as u8);
    while i > 0 {
        h.push((value & 0xFF) as u8);
        value >>= 8;
        i -= 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::native::NativeSevenZ;
    use crate::archive::ArchiveBackend;
    use crate::codec::{open_codec, CodecKind};
    use std::fs;

    #[test]
    fn custom_writer_readable_by_native_and_lists_files() {
        let dir = tempfile::tempdir().unwrap();
        let codec = open_codec(CodecKind::PureRust);
        let a = codec.compress(b"alpha data content", 1).unwrap();
        let b = codec.compress(b"beta data content!!", 1).unwrap();
        let entries = vec![
            PackedEntry {
                name: "a.txt".into(),
                compressed: a,
            },
            PackedEntry {
                name: "b.txt".into(),
                compressed: b,
            },
        ];
        let out = dir.path().join("out.7z");
        write_nonsolid_lzma2(&out, &entries).unwrap();

        let native = NativeSevenZ::new();
        match native.list(&out) {
            Ok(list) => {
                let names: Vec<_> = list.iter().map(|e| e.path.as_str()).collect();
                assert!(names.iter().any(|n| n.ends_with("a.txt")), "{names:?}");
                assert!(names.iter().any(|n| n.ends_with("b.txt")), "{names:?}");
                assert!(!native.is_solid(&out).unwrap());
            }
            Err(e) => {
                if let Ok(bin) = crate::archive::sevenz::find_7z_binary() {
                    let st = std::process::Command::new(bin)
                        .args(["t", out.to_str().unwrap()])
                        .status()
                        .unwrap();
                    assert!(st.success(), "7z t failed after native list err: {e}");
                } else {
                    panic!("native list failed: {e}");
                }
            }
        }
        let _ = fs::metadata(&out).unwrap();
    }

    #[test]
    fn streaming_writer_appends_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let codec = open_codec(CodecKind::LibLzma);
        let out = dir.path().join("stream.7z");
        let mut w = NonsolidLzma2Writer::create(&out).unwrap();
        for i in 0..20 {
            let data = format!("payload-{i}-{}", "x".repeat(50));
            let c = codec.compress(data.as_bytes(), 1).unwrap();
            w.push_packed(format!("f{i:02}.txt"), c).unwrap();
        }
        w.finish().unwrap();

        let native = NativeSevenZ::new();
        native.test(&out).unwrap();
        assert_eq!(
            native
                .list(&out)
                .unwrap()
                .into_iter()
                .filter(|e| !e.is_dir)
                .count(),
            20
        );
    }
}
