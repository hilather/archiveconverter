//! Directory outer "archive": write first-layer members as plain files.
//!
//! Nested converted `.7z` blobs and passthrough files land under a root
//! directory (no re-wrap in 7z/tar). Concurrent producers use
//! [`crate::codec::SyncedOuterWriter`].

use crate::error::{Error, Result};
use crate::util::pathnorm::{is_safe_member_path, normalize_member_path};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Writes outer members directly into a directory tree.
pub struct DirOuterWriter {
    root: PathBuf,
    count: usize,
}

impl DirOuterWriter {
    /// Create the root directory (parents as needed). Existing dir is reused;
    /// individual member paths are overwritten on push.
    pub fn create(path: &Path) -> Result<Self> {
        fs::create_dir_all(path).map_err(|e| {
            Error::Other(format!("create outer dir {}: {e}", path.display()))
        })?;
        Ok(Self {
            root: path.to_path_buf(),
            count: 0,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dest_path(&self, name: &str) -> Result<PathBuf> {
        let name = normalize_member_path(name);
        if !is_safe_member_path(&name) {
            return Err(Error::Other(format!(
                "unsafe outer member path for dir output: {name}"
            )));
        }
        Ok(self.root.join(Path::new(&name)))
    }

    /// Place `src` at `name` under the root (prefer rename, fall back to copy).
    pub fn push_path(&mut self, name: String, src: &Path) -> Result<()> {
        let dest = self.dest_path(&name)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            if dest.is_dir() {
                fs::remove_dir_all(&dest)?;
            } else {
                fs::remove_file(&dest)?;
            }
        }
        match fs::rename(src, &dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                fs::copy(src, &dest)?;
                let _ = fs::remove_file(src);
            }
            Err(e) => {
                // rename can also fail if src and dest are on same fs but other reasons;
                // try copy as fallback for any non-success rename of a file.
                if src.is_file() {
                    fs::copy(src, &dest).map_err(|e2| {
                        Error::Other(format!(
                            "dir outer write {} → {}: rename={e}, copy={e2}",
                            src.display(),
                            dest.display()
                        ))
                    })?;
                    let _ = fs::remove_file(src);
                } else {
                    return Err(Error::Other(format!(
                        "dir outer write {} → {}: {e}",
                        src.display(),
                        dest.display()
                    )));
                }
            }
        }
        self.count += 1;
        Ok(())
    }

    /// Write in-memory bytes as a regular file under `name`.
    pub fn push_bytes(&mut self, name: String, data: &[u8]) -> Result<()> {
        let dest = self.dest_path(&name)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&dest).map_err(|e| {
            Error::Other(format!("create {}: {e}", dest.display()))
        })?;
        f.write_all(data)?;
        f.flush()?;
        self.count += 1;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn finish(self) -> Result<()> {
        if self.count == 0 {
            return Err(Error::Other(
                "cannot finish empty directory outer (no members written)".into(),
            ));
        }
        Ok(())
    }
}

/// Count regular files under `root` (for `--verify`).
pub fn count_dir_files(root: &Path) -> Result<usize> {
    if !root.is_dir() {
        return Err(Error::Other(format!(
            "verify dir outer: not a directory: {}",
            root.display()
        )));
    }
    let mut n = 0usize;
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            n += 1;
        }
    }
    Ok(n)
}

/// Default output directory for dir mode: same location as the input archive,
/// name = archive file stem (e.g. `path/game.7z` → `path/game`).
pub fn default_dir_from_input(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("output");
    match input.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(stem),
        _ => PathBuf::from(stem),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn dir_writer_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("out");
        let a = tmp.path().join("a.7z");
        fs::write(&a, b"nested").unwrap();

        let mut w = DirOuterWriter::create(&root).unwrap();
        w.push_path("nested/a.7z".into(), &a).unwrap();
        w.push_bytes("readme.txt".into(), b"hi").unwrap();
        w.finish().unwrap();

        assert_eq!(count_dir_files(&root).unwrap(), 2);
        assert_eq!(fs::read(root.join("nested/a.7z")).unwrap(), b"nested");
        assert_eq!(fs::read_to_string(root.join("readme.txt")).unwrap(), "hi");
    }

    #[test]
    fn default_dir_matches_archive_stem() {
        assert_eq!(
            default_dir_from_input(Path::new("/data/game.7z")),
            PathBuf::from("/data/game")
        );
        assert_eq!(
            default_dir_from_input(Path::new("outer.7z")),
            PathBuf::from("outer")
        );
        assert_eq!(
            default_dir_from_input(Path::new("foo/bar.tar.7z")),
            PathBuf::from("foo/bar.tar")
        );
    }

    #[test]
    fn rejects_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = DirOuterWriter::create(tmp.path()).unwrap();
        let f = tmp.path().join("x");
        fs::write(&f, b"x").unwrap();
        assert!(w.push_path("../evil".into(), &f).is_err());
    }
}
