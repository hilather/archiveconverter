//! Temporary workspace helpers for conversion jobs.

use crate::error::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Job-scoped temporary directory that can live under a user-supplied parent.
pub struct JobTemp {
    inner: Option<TempDir>,
    keep: bool,
    path: PathBuf,
}

impl JobTemp {
    pub fn create(parent: Option<&Path>, keep: bool) -> Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("archiveconverter-");
        let inner = match parent {
            Some(p) => {
                fs::create_dir_all(p)?;
                builder.tempdir_in(p).map_err(Error::Io)?
            }
            None => builder.tempdir().map_err(Error::Io)?,
        };
        let path = inner.path().to_path_buf();
        Ok(Self {
            inner: Some(inner),
            keep,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Create a subdirectory for one nested conversion and return its path.
    pub fn nested_dir(&self, index: usize) -> Result<PathBuf> {
        let dir = self.child(&format!("nested-{index:04}"));
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

impl Drop for JobTemp {
    fn drop(&mut self) {
        if self.keep {
            if let Some(dir) = self.inner.take() {
                let path = dir.keep();
                tracing::info!(path = %path.display(), "keeping temp directory");
            }
        } else {
            // TempDir drops and removes automatically.
            self.inner.take();
        }
    }
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

pub fn remove_dir_all_quiet(path: &Path) {
    if path.exists() {
        if let Err(e) = fs::remove_dir_all(path) {
            tracing::warn!(path = %path.display(), error = %e, "failed to remove temp dir");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_nested_dirs() {
        let job = JobTemp::create(None, false).unwrap();
        let n = job.nested_dir(0).unwrap();
        assert!(n.is_dir());
        assert!(n.starts_with(job.path()));
    }
}
