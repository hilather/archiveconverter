//! Non-solid 7z writer that **stores** (Copy method) whole files as pack streams.
//!
//! Used for the outer archive: nested converted `.7z` blobs and passthrough
//! members are appended without recompression. Pack streams grow by append;
//! the end header is written once at [`NonsolidStoreWriter::finish`].
//!
//! Concurrent producers must use [`SyncedOuterWriter`], which serializes all
//! appends with a mutex so only one thread writes the file at a time.

use crate::error::{Error, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

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

/// Streaming non-solid 7z writer using the **Copy** (store) method per file.
pub struct NonsolidStoreWriter {
    file: File,
    entries: Vec<StoreEntry>,
    pack_sizes: Vec<u64>,
    pack_crcs: Vec<u32>,
}

struct StoreEntry {
    name: String,
    crc32: u32,
    size: u64,
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
            entries: Vec::new(),
            pack_sizes: Vec::new(),
            pack_crcs: Vec::new(),
        })
    }

    /// Append raw file bytes as a stored pack stream (no recompression).
    ///
    /// Streams from disk so large nested archives do not need to fit in RAM.
    /// Caller must ensure exclusive access (or use [`SyncedOuterWriter`]).
    pub fn push_path(&mut self, name: String, src: &Path) -> Result<()> {
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
        self.pack_sizes.push(size);
        self.pack_crcs.push(crc);
        self.entries.push(StoreEntry {
            name,
            crc32: crc,
            size,
        });
        Ok(())
    }

    /// Append an in-memory buffer as a stored member.
    pub fn push_bytes(&mut self, name: String, data: &[u8]) -> Result<()> {
        let crc = crc32fast::hash(data);
        self.file.write_all(data)?;
        self.pack_sizes.push(data.len() as u64);
        self.pack_crcs.push(crc);
        self.entries.push(StoreEntry {
            name,
            crc32: crc,
            size: data.len() as u64,
        });
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write end header and fix start signature. Consumes the writer.
    pub fn finish(mut self) -> Result<()> {
        if self.entries.is_empty() {
            return Err(Error::Other("cannot write empty 7z archive".into()));
        }

        let mut header = Vec::with_capacity(64 * 1024 + self.entries.len() * 64);
        write_store_header(
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
            w.write_all(&[0, 4])?;
            w.write_all(&0u32.to_le_bytes())?;
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

/// Thread-safe outer archive builder: all appends go through one mutex.
///
/// Nested convert workers may finish in any order; each calls
/// [`SyncedOuterWriter::push_path`] so only one writer touches the file.
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

    /// Append a finished member file. Serializes with other pushers.
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

    /// Finalize header. Must be called after all producers are done and no
    /// other handles will call `push_*`.
    pub fn finish(self) -> Result<()> {
        let writer = self
            .inner
            .into_inner()
            .map_err(|_| Error::Other("outer writer mutex poisoned".into()))?;
        writer.finish()
    }
}

fn write_store_header(
    h: &mut Vec<u8>,
    entries: &[StoreEntry],
    pack_sizes: &[u64],
    pack_crcs: &[u32],
) -> Result<()> {
    h.push(K_HEADER);
    h.push(K_MAIN_STREAMS_INFO);

    h.push(K_PACK_INFO);
    write_u64(h, 0)?;
    write_u64(h, entries.len() as u64)?;
    h.push(K_SIZE);
    for s in pack_sizes {
        write_u64(h, *s)?;
    }
    h.push(K_CRC);
    h.push(1);
    for c in pack_crcs {
        h.extend_from_slice(&c.to_le_bytes());
    }
    h.push(K_END);

    h.push(K_UNPACK_INFO);
    h.push(K_FOLDER);
    write_u64(h, entries.len() as u64)?;
    h.push(0);
    for _ in entries {
        write_folder_copy(h)?;
    }
    h.push(K_CODERS_UNPACK_SIZE);
    for e in entries {
        write_u64(h, e.size)?;
    }
    h.push(K_END);

    h.push(K_SUB_STREAMS_INFO);
    h.push(K_CRC);
    h.push(1);
    for e in entries {
        h.extend_from_slice(&e.crc32.to_le_bytes());
    }
    h.push(K_END);
    h.push(K_END);

    h.push(K_FILES_INFO);
    write_u64(h, entries.len() as u64)?;
    h.push(K_NAME);
    let mut names = Vec::new();
    names.push(0);
    for e in entries {
        for c in e.name.encode_utf16() {
            names.extend_from_slice(&c.to_le_bytes());
        }
        names.extend_from_slice(&0u16.to_le_bytes());
    }
    write_u64(h, names.len() as u64)?;
    h.extend_from_slice(&names);
    h.push(K_END);
    h.push(K_END);
    Ok(())
}

/// Single Copy (store) coder folder — method id `0x00`.
fn write_folder_copy(h: &mut Vec<u8>) -> Result<()> {
    write_u64(h, 1)?; // numCoders
    let id = [0x00u8]; // Copy
    let flags = (id.len() as u8) & 0x0F; // no properties
    h.push(flags);
    h.extend_from_slice(&id);
    Ok(())
}

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
