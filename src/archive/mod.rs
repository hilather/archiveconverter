//! Archive format traits and 7z backend.

pub mod detect;
pub mod sevenz;

use crate::error::Result;
use std::path::{Path, PathBuf};

/// Supported archive formats (extensible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveFormat {
    SevenZ,
    Zip,
    Unknown,
}

impl ArchiveFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveFormat::SevenZ => "7z",
            ArchiveFormat::Zip => "zip",
            ArchiveFormat::Unknown => "unknown",
        }
    }
}

/// Metadata for one archive member.
#[derive(Debug, Clone)]
pub struct EntryMeta {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub format_hint: ArchiveFormat,
}

impl EntryMeta {
    pub fn is_nested_archive(&self) -> bool {
        !self.is_dir
            && matches!(
                self.format_hint,
                ArchiveFormat::SevenZ | ArchiveFormat::Zip
            )
    }
}

/// High-level operations needed by the pipeline (implemented by backends).
pub trait ArchiveBackend: Send + Sync {
    fn format(&self) -> ArchiveFormat;

    fn list(&self, archive: &Path) -> Result<Vec<EntryMeta>>;

    /// Extract a single member to `dest_file` (parent dirs created).
    fn extract_member(&self, archive: &Path, member: &str, dest_file: &Path) -> Result<()>;

    /// Extract all members into `dest_dir`, optionally skipping filtered names.
    fn extract_all(&self, archive: &Path, dest_dir: &Path) -> Result<()>;

    /// Pack directory contents into a new archive with conversion profile flags.
    fn pack_dir(&self, src_dir: &Path, dest_archive: &Path, opts: &PackOptions) -> Result<()>;

    /// Test archive integrity.
    fn test(&self, archive: &Path) -> Result<()>;
}

/// Options for packing / conversion profiles.
#[derive(Debug, Clone)]
pub struct PackOptions {
    /// Non-solid archive (`-ms=off` for 7z).
    pub non_solid: bool,
    /// Multi-threaded compression when supported.
    pub threads: Option<u32>,
    /// Compression level 0-9 (backend-specific).
    pub level: u32,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            non_solid: true,
            threads: None,
            level: 5,
        }
    }
}

/// Locate a usable 7z executable.
pub fn find_7z_binary() -> Result<PathBuf> {
    sevenz::find_7z_binary()
}
