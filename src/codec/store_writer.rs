//! Non-solid 7z writer that **stores** (Copy method) whole files as pack streams.
//!
//! Used for the outer archive: nested converted `.7z` blobs and passthrough
//! members are appended without recompression. Headers match sevenz-rust2 layout
//! for ratarmount / ratarmount-rs / py7zr compatibility.
//!
//! Concurrent producers must use [`SyncedOuterWriter`].

use super::sevenz_header::{
    write_raw_header, write_start_header, HeaderFile, SIG_HEADER_SIZE,
};
use crate::error::{Error, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

/// Streaming non-solid 7z writer using the **Copy** (store) method per file.
pub struct NonsolidStoreWriter {
    file: File,
    files: Vec<HeaderFile>,
}

impl NonsolidStoreWriter {
    /// Create output path and write a placeholder start header.
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let mut file = File::create(path)?;
        file.write_all(&[0u8; SIG_HEADER_SIZE as usize])?;
        Ok(Self {
            file,
            files: Vec::new(),
        })
    }

    /// Append raw file bytes as a stored pack stream (no recompression).
    pub fn push_path(&mut self, name: String, src: &Path) -> Result<()> {
        let meta = std::fs::metadata(src).map_err(|e| {
            Error::Other(format!("stat {} for outer append: {e}", src.display()))
        })?;
        if meta.len() == 0 {
            // Empty file: no pack stream; marked empty for FilesInfo.
            self.files.push(HeaderFile {
                name,
                pack_size: 0,
                pack_crc: 0,
                unpack_size: 0,
                content_crc: 0,
                method_id: vec![0x00],
                method_props: vec![],
                empty: true,
            });
            return Ok(());
        }

        let mut input = File::open(src).map_err(|e| {
            Error::Other(format!("open {} for outer append: {e}", src.display()))
        })?;
        let mut hasher = crc32fast::Hasher::new();
        let mut buf = [0u8; 256 * 1024];
        let mut size = 0u64;
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            self.file.write_all(&buf[..n])?;
            size += n as u64;
        }
        let crc = hasher.finalize();
        self.files.push(HeaderFile {
            name,
            pack_size: size,
            pack_crc: crc,
            unpack_size: size,
            content_crc: crc,
            method_id: vec![0x00], // Copy
            method_props: vec![],
            empty: false,
        });
        Ok(())
    }

    /// Append an in-memory buffer as a stored member.
    pub fn push_bytes(&mut self, name: String, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            self.files.push(HeaderFile {
                name,
                pack_size: 0,
                pack_crc: 0,
                unpack_size: 0,
                content_crc: 0,
                method_id: vec![0x00],
                method_props: vec![],
                empty: true,
            });
            return Ok(());
        }
        let crc = crc32fast::hash(data);
        self.file.write_all(data)?;
        self.files.push(HeaderFile {
            name,
            pack_size: data.len() as u64,
            pack_crc: crc,
            unpack_size: data.len() as u64,
            content_crc: crc,
            method_id: vec![0x00],
            method_props: vec![],
            empty: false,
        });
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Write end header and fix start signature. Consumes the writer.
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

/// Thread-safe outer archive builder: all appends go through one mutex.
pub struct SyncedOuterWriter {
    inner: Mutex<NonsolidStoreWriter>,
    path: std::path::PathBuf,
}

impl SyncedOuterWriter {
    pub fn create(path: &Path) -> Result<Self> {
        Ok(Self {
            inner: Mutex::new(NonsolidStoreWriter::create(path)?),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn push_path(&self, name: String, src: &Path) -> Result<()> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| Error::Other("outer writer mutex poisoned".into()))?;
        tracing::debug!(
            member = %name,
            src = %src.display(),
            outer = %self.path.display(),
            "appending stored member to outer 7z"
        );
        g.push_path(name, src)
    }

    pub fn push_bytes(&self, name: String, data: &[u8]) -> Result<()> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| Error::Other("outer writer mutex poisoned".into()))?;
        g.push_bytes(name, data)
    }

    pub fn len(&self) -> Result<usize> {
        let g = self
            .inner
            .lock()
            .map_err(|_| Error::Other("outer writer mutex poisoned".into()))?;
        Ok(g.len())
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    pub fn finish(self) -> Result<()> {
        let writer = self
            .inner
            .into_inner()
            .map_err(|_| Error::Other("outer writer mutex poisoned".into()))?;
        writer.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::native::NativeSevenZ;
    use crate::archive::ArchiveBackend;
    use std::fs;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn store_writer_roundtrip_list_extract() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.txt");
        fs::write(&a, b"nested-like-payload-aaaa").unwrap();
        fs::write(&b, b"passthrough hello").unwrap();

        let out = dir.path().join("outer.7z");
        let mut w = NonsolidStoreWriter::create(&out).unwrap();
        w.push_path("nested/a.7z".into(), &a).unwrap();
        w.push_path("readme.txt".into(), &b).unwrap();
        w.finish().unwrap();

        let native = NativeSevenZ::new();
        native.test(&out).unwrap();
        assert!(!native.is_solid(&out).unwrap());
        let names: Vec<_> = native
            .list(&out)
            .unwrap()
            .into_iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path)
            .collect();
        assert!(names.iter().any(|n| n.ends_with("a.7z")), "{names:?}");
        assert!(names.iter().any(|n| n == "readme.txt"), "{names:?}");

        let dest = dir.path().join("out-a.bin");
        native.extract_member(&out, "nested/a.7z", &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"nested-like-payload-aaaa");
    }

    #[test]
    fn store_writer_empty_file_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("e.7z");
        let mut w = NonsolidStoreWriter::create(&out).unwrap();
        w.push_bytes("empty.dat".into(), b"").unwrap();
        w.push_bytes("sub/x.txt".into(), b"hi").unwrap();
        w.finish().unwrap();
        let native = NativeSevenZ::new();
        native.test(&out).unwrap();
        let list = native.list(&out).unwrap();
        let names: Vec<_> = list.into_iter().filter(|e| !e.is_dir).map(|e| e.path).collect();
        assert!(names.iter().any(|n| n.ends_with("empty.dat")), "{names:?}");
        assert!(names.iter().any(|n| n.contains("x.txt")), "{names:?}");
    }

    #[test]
    fn synced_writer_serializes_concurrent_appends() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("mt.7z");
        let outer = Arc::new(SyncedOuterWriter::create(&out).unwrap());

        let mut handles = Vec::new();
        for i in 0..8 {
            let outer = Arc::clone(&outer);
            let path = dir.path().join(format!("f{i}.dat"));
            fs::write(&path, format!("payload-{i}-{}", "x".repeat(100))).unwrap();
            handles.push(thread::spawn(move || {
                outer
                    .push_path(format!("m{i:02}.dat"), &path)
                    .expect("push");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let outer = match Arc::try_unwrap(outer) {
            Ok(w) => w,
            Err(_) => panic!("no other Arc refs expected"),
        };
        assert_eq!(outer.len().unwrap(), 8);
        outer.finish().unwrap();

        let native = NativeSevenZ::new();
        native.test(&out).unwrap();
        assert_eq!(
            native
                .list(&out)
                .unwrap()
                .into_iter()
                .filter(|e| !e.is_dir)
                .count(),
            8
        );
    }

    #[test]
    fn empty_finish_errors() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("empty.7z");
        let w = NonsolidStoreWriter::create(&out).unwrap();
        assert!(w.finish().is_err());
    }
}
