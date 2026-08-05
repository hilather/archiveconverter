//! Minimal non-solid multi-file 7z writer for precompressed LZMA2 streams.
//!
//! Headers follow sevenz-rust2 / 7-Zip layout (substream CRCs, no folder CRCs,
//! names + optional mtime + win attributes) so ratarmount / ratarmount-rs / py7zr
//! parse and stream members correctly. Callers must supply source file metadata
//! when preservation is required.

use super::sevenz_header::{
    write_raw_header, write_start_header, FileMeta, HeaderFile, SIG_HEADER_SIZE,
};
use super::Lzma2Compressed;
use crate::error::{Error, Result};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

/// One file ready to pack (already LZMA2-compressed).
pub struct PackedEntry {
    pub name: String,
    pub compressed: Lzma2Compressed,
    /// Source member metadata (mtime / attrs); empty = omit from header.
    pub meta: FileMeta,
}

/// Streaming non-solid 7z writer: packs are appended immediately; header last.
pub struct NonsolidLzma2Writer {
    file: File,
    files: Vec<HeaderFile>,
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
            files: Vec::new(),
        })
    }

    /// Append one precompressed pack stream and record header metadata.
    pub fn push_packed(
        &mut self,
        name: String,
        compressed: Lzma2Compressed,
        meta: FileMeta,
    ) -> Result<()> {
        let pack_crc = crc32fast::hash(&compressed.data);
        let pack_size = compressed.data.len() as u64;
        self.file.write_all(&compressed.data)?;
        self.files.push(HeaderFile {
            name,
            pack_size,
            pack_crc,
            unpack_size: compressed.uncompressed_size,
            content_crc: compressed.crc32,
            method_id: vec![0x21], // LZMA2
            method_props: vec![compressed.props],
            empty: compressed.uncompressed_size == 0 && pack_size == 0,
            meta,
        });
        Ok(())
    }

    pub fn push_entry(&mut self, entry: PackedEntry) -> Result<()> {
        self.push_packed(entry.name, entry.compressed, entry.meta)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Write the end header and fix the start signature. Consumes the writer.
    pub fn finish(mut self) -> Result<()> {
        if self.files.is_empty() {
            return Err(Error::Other("cannot write empty 7z archive".into()));
        }

        let mut header = Vec::with_capacity(64 * 1024 + self.files.len() * 64);
        write_raw_header(&mut header, &self.files)?;

        let header_pos = self.file.stream_position()?;
        self.file.write_all(&header)?;
        let header_crc = crc32fast::hash(&header);

        let next_header_offset = header_pos - SIG_HEADER_SIZE;
        let next_header_size = header.len() as u64;
        let sig = write_start_header(next_header_offset, next_header_size, header_crc);

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
            e.meta.clone(),
        )?;
    }
    w.finish()
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
                meta: FileMeta {
                    mtime: Some(100_000_000_000_000),
                    windows_attributes: Some(0x20),
                    ..Default::default()
                },
            },
            PackedEntry {
                name: "b.txt".into(),
                compressed: b,
                meta: FileMeta {
                    mtime: Some(200_000_000_000_000),
                    windows_attributes: Some(0x20),
                    ..Default::default()
                },
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
                let a_meta = list.iter().find(|e| e.path.ends_with("a.txt")).unwrap();
                assert_eq!(a_meta.meta.mtime, Some(100_000_000_000_000));
                let b_meta = list.iter().find(|e| e.path.ends_with("b.txt")).unwrap();
                assert_eq!(b_meta.meta.mtime, Some(200_000_000_000_000));
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
            w.push_packed(
                format!("f{i:02}.txt"),
                c,
                FileMeta {
                    mtime: Some(1_000_000_000_000 + i as u64),
                    ..Default::default()
                },
            )
            .unwrap();
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

    #[test]
    fn nested_paths_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let codec = open_codec(CodecKind::PureRust);
        let out = dir.path().join("paths.7z");
        let mut w = NonsolidLzma2Writer::create(&out).unwrap();
        let c = codec.compress(b"in subdir", 1).unwrap();
        w.push_packed("sub/dir/x.txt".into(), c, FileMeta::default())
            .unwrap();
        w.finish().unwrap();
        let native = NativeSevenZ::new();
        native.test(&out).unwrap();
        let dest = dir.path().join("x.txt");
        native
            .extract_member(&out, "sub/dir/x.txt", &dest)
            .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"in subdir");
    }
}
