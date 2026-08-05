//! Non-solid 7z writer that **stores** (Copy method) whole files as pack streams.
//!
//! Used for the outer archive: nested converted `.7z` blobs and passthrough
//! members are appended without recompression. Headers match sevenz-rust2 layout
//! for ratarmount / ratarmount-rs / py7zr compatibility.
//!
//! Concurrent producers must use [`SyncedOuterWriter`], which also supports an
//! uncompressed **tar** outer via [`OuterFormat::Tar`].

use super::sevenz_header::{
    write_raw_header, write_start_header, FileMeta, HeaderFile, SIG_HEADER_SIZE,
};
use super::dir_writer::DirOuterWriter;
use super::tar_writer::TarOuterWriter;
use crate::error::{Error, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Outer container format for the converted nested archive bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OuterFormat {
    /// Non-solid 7z with Copy (store) method per member. Default.
    #[default]
    SevenZ,
    /// Uncompressed tar (no gzip/xz). Nested members remain compressed 7z blobs.
    Tar,
    /// No re-wrap: write first-layer members into a directory.
    Dir,
}

impl OuterFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            OuterFormat::SevenZ => "7z",
            OuterFormat::Tar => "tar",
            OuterFormat::Dir => "dir",
        }
    }

    /// Whether the output path is a directory tree rather than a single file.
    pub fn is_directory(self) -> bool {
        matches!(self, OuterFormat::Dir)
    }

    /// File extension without dot for archive formats; for dir mode, a temp suffix.
    pub fn extension(self) -> &'static str {
        match self {
            OuterFormat::SevenZ => "7z",
            OuterFormat::Tar => "tar",
            OuterFormat::Dir => "dir",
        }
    }

    /// Parse CLI / config string (`7z`, `tar`, `dir`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "7z" | "sevenz" | "7zip" => Some(Self::SevenZ),
            "tar" | "ustar" => Some(Self::Tar),
            "dir" | "directory" | "folder" | "files" => Some(Self::Dir),
            _ => None,
        }
    }

    /// Infer from output path when no explicit flag is set.
    ///
    /// `.tar` → tar; trailing `/` → dir; else 7z.
    pub fn from_output_path(path: &Path) -> Self {
        let s = path.to_string_lossy();
        if s.ends_with('/') || s.ends_with('\\') {
            return Self::Dir;
        }
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("tar") => Self::Tar,
            _ => Self::SevenZ,
        }
    }

    /// Explicit flag wins; otherwise use the output path (when present).
    pub fn resolve(explicit: Option<Self>, output: Option<&Path>) -> Self {
        if let Some(f) = explicit {
            return f;
        }
        match output {
            Some(p) => Self::from_output_path(p),
            None => Self::SevenZ,
        }
    }
}

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
    ///
    /// Uses filesystem metadata of `src` for times/attributes.
    pub fn push_path(&mut self, name: String, src: &Path) -> Result<()> {
        self.push_path_with_meta(name, src, None)
    }

    /// Append raw file bytes, preferring `file_meta` (source archive listing)
    /// and filling any missing fields from the filesystem metadata of `src`.
    pub fn push_path_with_meta(
        &mut self,
        name: String,
        src: &Path,
        file_meta: Option<FileMeta>,
    ) -> Result<()> {
        let fs_len = std::fs::metadata(src)
            .map_err(|e| {
                Error::Other(format!("stat {} for outer append: {e}", src.display()))
            })?
            .len();
        let mut meta = file_meta.unwrap_or_default();
        meta.merge_missing(&FileMeta::from_fs_path(src));

        if fs_len == 0 {
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
                meta,
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
            meta,
        });
        Ok(())
    }

    /// Append an in-memory buffer as a stored member (no times unless provided).
    pub fn push_bytes(&mut self, name: String, data: &[u8]) -> Result<()> {
        self.push_bytes_with_meta(name, data, FileMeta::default())
    }

    /// Append an in-memory buffer with explicit file metadata.
    pub fn push_bytes_with_meta(
        &mut self,
        name: String,
        data: &[u8],
        meta: FileMeta,
    ) -> Result<()> {
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
                meta,
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
            meta,
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

enum OuterInner {
    SevenZ(NonsolidStoreWriter),
    Tar(TarOuterWriter),
    Dir(DirOuterWriter),
}

/// Thread-safe outer archive builder (7z store, uncompressed tar, or directory).
pub struct SyncedOuterWriter {
    inner: Mutex<OuterInner>,
    path: PathBuf,
    format: OuterFormat,
}

impl SyncedOuterWriter {
    pub fn create(path: &Path, format: OuterFormat) -> Result<Self> {
        let inner = match format {
            OuterFormat::SevenZ => OuterInner::SevenZ(NonsolidStoreWriter::create(path)?),
            OuterFormat::Tar => OuterInner::Tar(TarOuterWriter::create(path)?),
            OuterFormat::Dir => OuterInner::Dir(DirOuterWriter::create(path)?),
        };
        Ok(Self {
            inner: Mutex::new(inner),
            path: path.to_path_buf(),
            format,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format(&self) -> OuterFormat {
        self.format
    }

    pub fn push_path(&self, name: String, src: &Path) -> Result<()> {
        self.push_path_with_meta(name, src, None)
    }

    /// Append a member, preserving `file_meta` (source outer listing) when provided.
    pub fn push_path_with_meta(
        &self,
        name: String,
        src: &Path,
        file_meta: Option<FileMeta>,
    ) -> Result<()> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| Error::Other("outer writer mutex poisoned".into()))?;
        tracing::debug!(
            member = %name,
            src = %src.display(),
            outer = %self.path.display(),
            format = self.format.as_str(),
            has_mtime = file_meta.as_ref().and_then(|m| m.mtime).is_some(),
            "appending member to outer"
        );
        match &mut *g {
            OuterInner::SevenZ(w) => w.push_path_with_meta(name, src, file_meta),
            // Tar/dir use FS metadata of `src` (extract/convert temps).
            OuterInner::Tar(w) => w.push_path(name, src),
            OuterInner::Dir(w) => w.push_path(name, src),
        }
    }

    pub fn push_bytes(&self, name: String, data: &[u8]) -> Result<()> {
        self.push_bytes_with_meta(name, data, FileMeta::default())
    }

    pub fn push_bytes_with_meta(
        &self,
        name: String,
        data: &[u8],
        meta: FileMeta,
    ) -> Result<()> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| Error::Other("outer writer mutex poisoned".into()))?;
        match &mut *g {
            OuterInner::SevenZ(w) => w.push_bytes_with_meta(name, data, meta),
            OuterInner::Tar(w) => w.push_bytes(name, data),
            OuterInner::Dir(w) => w.push_bytes(name, data),
        }
    }

    pub fn len(&self) -> Result<usize> {
        let g = self
            .inner
            .lock()
            .map_err(|_| Error::Other("outer writer mutex poisoned".into()))?;
        Ok(match &*g {
            OuterInner::SevenZ(w) => w.len(),
            OuterInner::Tar(w) => w.len(),
            OuterInner::Dir(w) => w.len(),
        })
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    pub fn finish(self) -> Result<()> {
        let writer = self
            .inner
            .into_inner()
            .map_err(|_| Error::Other("outer writer mutex poisoned".into()))?;
        match writer {
            OuterInner::SevenZ(w) => w.finish(),
            OuterInner::Tar(w) => w.finish(),
            OuterInner::Dir(w) => w.finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::native::NativeSevenZ;
    use crate::archive::ArchiveBackend;
    use crate::codec::tar_writer::count_tar_files;
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
        let outer = Arc::new(SyncedOuterWriter::create(&out, OuterFormat::SevenZ).unwrap());

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
    fn synced_tar_concurrent_appends() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("mt.tar");
        let outer = Arc::new(SyncedOuterWriter::create(&out, OuterFormat::Tar).unwrap());

        let mut handles = Vec::new();
        for i in 0..6 {
            let outer = Arc::clone(&outer);
            let path = dir.path().join(format!("t{i}.dat"));
            fs::write(&path, format!("tar-payload-{i}")).unwrap();
            handles.push(thread::spawn(move || {
                outer
                    .push_path(format!("m{i:02}.dat"), &path)
                    .expect("push");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let outer = Arc::try_unwrap(outer).ok().expect("unique");
        assert_eq!(outer.len().unwrap(), 6);
        outer.finish().unwrap();
        assert_eq!(count_tar_files(&out).unwrap(), 6);
    }

    #[test]
    fn empty_finish_errors() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("empty.7z");
        let w = NonsolidStoreWriter::create(&out).unwrap();
        assert!(w.finish().is_err());
    }

    #[test]
    fn outer_format_resolve_from_extension() {
        assert_eq!(
            OuterFormat::resolve(None, Some(Path::new("out.tar"))),
            OuterFormat::Tar
        );
        assert_eq!(
            OuterFormat::resolve(None, Some(Path::new("out.7z"))),
            OuterFormat::SevenZ
        );
        assert_eq!(
            OuterFormat::resolve(None, Some(Path::new("out_dir/"))),
            OuterFormat::Dir
        );
        assert_eq!(
            OuterFormat::resolve(Some(OuterFormat::Tar), Some(Path::new("out.7z"))),
            OuterFormat::Tar
        );
        assert_eq!(
            OuterFormat::resolve(Some(OuterFormat::SevenZ), Some(Path::new("out.tar"))),
            OuterFormat::SevenZ
        );
        assert_eq!(
            OuterFormat::resolve(Some(OuterFormat::Dir), None),
            OuterFormat::Dir
        );
    }
}
