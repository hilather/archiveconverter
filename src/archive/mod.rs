//! Archive format traits and 7z backends (CLI + native).

pub mod detect;
pub mod native;
pub mod sevenz;

use crate::error::Result;
use std::path::{Path, PathBuf};

/// Which implementation drives 7z operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// Official `7zz`/`7z` CLI (default; battle-tested).
    #[default]
    Cli,
    /// Pure-Rust `sevenz-rust2` (Phase 1 streaming convert).
    Native,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Cli => "cli",
            BackendKind::Native => "native",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cli" | "7z" | "7zz" => Some(Self::Cli),
            "native" | "rust" | "sevenz-rust2" => Some(Self::Native),
            _ => None,
        }
    }
}

/// Construct a backend by kind.
pub fn open_backend(kind: BackendKind) -> Result<Box<dyn ArchiveBackend>> {
    open_backend_with(kind, native::NativeOptions::default())
}

/// Construct a backend, applying `native_opts` when kind is Native.
pub fn open_backend_with(
    kind: BackendKind,
    native_opts: native::NativeOptions,
) -> Result<Box<dyn ArchiveBackend>> {
    match kind {
        BackendKind::Cli => Ok(Box::new(sevenz::SevenZCli::discover()?)),
        BackendKind::Native => Ok(Box::new(native::NativeSevenZ::with_options(native_opts))),
    }
}

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

    /// Whether the archive uses solid compression (single solid block for many files).
    ///
    /// Default: `false` (safe / no single-pass optimization).
    fn is_solid(&self, _archive: &Path) -> Result<bool> {
        Ok(false)
    }

    /// Extract a single member to `dest_file` (parent dirs created).
    fn extract_member(&self, archive: &Path, member: &str, dest_file: &Path) -> Result<()>;

    /// Extract several named members into `dest_dir` in **one** backend invocation.
    ///
    /// For solid 7z this is a single solid-stream decode (O(n)) instead of one
    /// extract_member call per file (which restarts solid decode → O(n²)).
    fn extract_members(&self, archive: &Path, members: &[&str], dest_dir: &Path) -> Result<()> {
        // Default fallback: sequential single-member extracts.
        for m in members {
            let dest = dest_dir.join(m);
            self.extract_member(archive, m, &dest)?;
        }
        Ok(())
    }

    /// Extract all members into `dest_dir`.
    fn extract_all(&self, archive: &Path, dest_dir: &Path) -> Result<()>;

    /// Extract all members, applying 7z-style exclude globs (`-x!pattern`).
    ///
    /// Default falls back to `extract_all` (ignores globs) — backends should override.
    fn extract_all_with_excludes(
        &self,
        archive: &Path,
        dest_dir: &Path,
        exclude_globs: &[String],
    ) -> Result<()> {
        let _ = exclude_globs;
        self.extract_all(archive, dest_dir)
    }

    /// Pack directory contents into a new archive with conversion profile flags.
    fn pack_dir(&self, src_dir: &Path, dest_archive: &Path, opts: &PackOptions) -> Result<()>;

    /// Test archive integrity.
    fn test(&self, archive: &Path) -> Result<()>;

    /// Optional fast path: solid/any → non-solid without unpacking a full tree.
    ///
    /// Returns `Ok(true)` if handled, `Ok(false)` to fall back to extract+pack.
    fn convert_to_nonsolid_streaming(
        &self,
        _input: &Path,
        _output: &Path,
        _filter: &crate::filter::MemberFilter,
        _pack: &PackOptions,
    ) -> Result<bool> {
        Ok(false)
    }
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
