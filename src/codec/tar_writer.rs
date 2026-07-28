//! Uncompressed tar outer-archive writer.
//!
//! Nested converted `.7z` blobs and passthrough members are appended as
//! regular tar file entries (no gzip/xz). Concurrent producers must use
//! [`crate::codec::SyncedOuterWriter`].

use crate::error::{Error, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use tar::{Builder, Header};

/// Streaming uncompressed tar builder for the outer container.
pub struct TarOuterWriter {
    builder: Builder<File>,
    count: usize,
}

impl TarOuterWriter {
    /// Create (or replace) the tar at `path`.
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let file = File::create(path)?;
        Ok(Self {
            builder: Builder::new(file),
            count: 0,
        })
    }

    fn normalize_name(name: &str) -> String {
        name.trim_start_matches('/')
            .trim_start_matches("./")
            .replace('\\', "/")
    }

    /// Append a file from disk under `name` (relative member path).
    pub fn push_path(&mut self, name: String, src: &Path) -> Result<()> {
        let name = Self::normalize_name(&name);
        if name.is_empty() {
            return Err(Error::Other("empty tar member name".into()));
        }
        self.builder
            .append_path_with_name(src, &name)
            .map_err(|e| {
                Error::Other(format!(
                    "tar append {} as {name}: {e}",
                    src.display()
                ))
            })?;
        self.count += 1;
        Ok(())
    }

    /// Append an in-memory buffer as a regular file member.
    pub fn push_bytes(&mut self, name: String, data: &[u8]) -> Result<()> {
        let name = Self::normalize_name(&name);
        if name.is_empty() {
            return Err(Error::Other("empty tar member name".into()));
        }
        let mut header = Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        self.builder
            .append_data(&mut header, &name, data)
            .map_err(|e| Error::Other(format!("tar append bytes as {name}: {e}")))?;
        self.count += 1;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Finalize the tar (writes end-of-archive blocks).
    pub fn finish(mut self) -> Result<()> {
        if self.count == 0 {
            return Err(Error::Other("cannot write empty tar archive".into()));
        }
        self.builder
            .finish()
            .map_err(|e| Error::Other(format!("tar finish: {e}")))?;
        // Ensure data hits disk; Builder::finish flushes the archive trailer.
        let mut file = self.builder.into_inner().map_err(|e| {
            Error::Other(format!("tar into_inner: {e}"))
        })?;
        file.flush()?;
        Ok(())
    }
}

/// Count regular file entries in an uncompressed tar (for `--verify`).
pub fn count_tar_files(path: &Path) -> Result<usize> {
    let file = File::open(path).map_err(|e| {
        Error::Other(format!("open tar for verify {}: {e}", path.display()))
    })?;
    let mut archive = tar::Archive::new(file);
    let mut n = 0usize;
    for entry in archive
        .entries()
        .map_err(|e| Error::Other(format!("tar entries: {e}")))?
    {
        let entry = entry.map_err(|e| Error::Other(format!("tar entry: {e}")))?;
        if entry.header().entry_type().is_file() {
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;

    #[test]
    fn tar_writer_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.7z");
        let b = dir.path().join("readme.txt");
        fs::write(&a, b"nested-payload").unwrap();
        fs::write(&b, b"hello").unwrap();

        let out = dir.path().join("outer.tar");
        let mut w = TarOuterWriter::create(&out).unwrap();
        w.push_path("nested/a.7z".into(), &a).unwrap();
        w.push_path("readme.txt".into(), &b).unwrap();
        w.finish().unwrap();

        assert_eq!(count_tar_files(&out).unwrap(), 2);

        let file = File::open(&out).unwrap();
        let mut archive = tar::Archive::new(file);
        let mut found = Vec::new();
        for e in archive.entries().unwrap() {
            let mut e = e.unwrap();
            if e.header().entry_type().is_file() {
                let path = e.path().unwrap().to_string_lossy().into_owned();
                let mut buf = Vec::new();
                e.read_to_end(&mut buf).unwrap();
                found.push((path, buf));
            }
        }
        assert!(found.iter().any(|(p, d)| p.ends_with("a.7z") && d == b"nested-payload"));
        assert!(found.iter().any(|(p, d)| p == "readme.txt" && d == b"hello"));
    }

    #[test]
    fn empty_finish_errors() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("empty.tar");
        let w = TarOuterWriter::create(&out).unwrap();
        assert!(w.finish().is_err());
    }
}
