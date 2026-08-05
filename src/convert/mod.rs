//! Conversion profiles and registry.

pub mod sevenz_nonsolid;
pub mod stub_zip;

use crate::archive::{ArchiveBackend, EntryMeta, PackOptions};
use crate::error::Result;
use crate::filter::MemberFilter;
use std::path::{Path, PathBuf};

/// Shared context for a single archive conversion.
#[derive(Debug, Clone)]
pub struct ConvertContext {
    pub exclude: MemberFilter,
    pub pack: PackOptions,
    pub temp_dir: PathBuf,
    /// When true, run `7z t` on the result.
    pub verify: bool,
    /// When true, copy already-non-solid archives without recompressing (if no filters).
    pub passthrough_nonsolid: bool,
    /// Prefer backend streaming solid→non-solid (no full tree) when available.
    pub prefer_streaming: bool,
}

impl ConvertContext {
    pub fn new(temp_dir: PathBuf) -> Self {
        Self {
            exclude: MemberFilter::new(),
            pack: PackOptions::default(),
            temp_dir,
            verify: false,
            passthrough_nonsolid: true,
            prefer_streaming: false,
        }
    }
}

/// Output of a conversion step.
#[derive(Debug)]
pub struct ConvertOutput {
    pub path: PathBuf,
}

/// A pluggable conversion profile.
pub trait Converter: Send + Sync {
    fn id(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn matches(&self, entry: &EntryMeta) -> bool;
    fn convert(
        &self,
        backend: &dyn ArchiveBackend,
        input: &Path,
        ctx: &ConvertContext,
    ) -> Result<ConvertOutput>;
}

/// Registry of available converters.
#[derive(Default)]
pub struct ConverterRegistry {
    converters: Vec<Box<dyn Converter>>,
}

impl ConverterRegistry {
    pub fn new() -> Self {
        Self {
            converters: Vec::new(),
        }
    }

    /// Register built-in converters for v1.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Box::new(sevenz_nonsolid::SevenZSolidToNonSolid));
        // Extension point proof (not selected for .7z matches).
        r.register(Box::new(stub_zip::ZipPassthroughStub));
        r
    }

    pub fn register(&mut self, c: Box<dyn Converter>) {
        self.converters.push(c);
    }

    pub fn find_for(&self, entry: &EntryMeta) -> Option<&dyn Converter> {
        self.converters
            .iter()
            .find(|c| c.matches(entry))
            .map(|c| c.as_ref())
    }

    pub fn get(&self, id: &str) -> Option<&dyn Converter> {
        self.converters
            .iter()
            .find(|c| c.id() == id)
            .map(|c| c.as_ref())
    }

    pub fn list_ids(&self) -> Vec<&'static str> {
        self.converters.iter().map(|c| c.id()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ArchiveFormat;

    #[test]
    fn builtins_register_sevenz() {
        let r = ConverterRegistry::with_builtins();
        assert!(r.list_ids().contains(&"7z-solid-to-nonsolid"));
        let entry = EntryMeta {
            path: "x.7z".into(),
            size: 1,
            is_dir: false,
            format_hint: ArchiveFormat::SevenZ,
            meta: Default::default(),
        };
        assert_eq!(
            r.find_for(&entry).unwrap().id(),
            "7z-solid-to-nonsolid"
        );
    }
}
