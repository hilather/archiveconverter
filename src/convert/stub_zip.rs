//! Stub converter proving the registry extension path for future ZIP support.

use super::{ConvertContext, ConvertOutput, Converter};
use crate::archive::{ArchiveBackend, ArchiveFormat, EntryMeta};
use crate::error::{Error, Result};
use std::path::Path;

/// Placeholder: matches `.zip` members but conversion is not implemented in v1.
pub struct ZipPassthroughStub;

impl Converter for ZipPassthroughStub {
    fn id(&self) -> &'static str {
        "zip-stub"
    }

    fn description(&self) -> &'static str {
        "Stub for future ZIP conversion (not implemented)"
    }

    fn matches(&self, entry: &EntryMeta) -> bool {
        !entry.is_dir && entry.format_hint == ArchiveFormat::Zip
    }

    fn convert(
        &self,
        _backend: &dyn ArchiveBackend,
        _input: &Path,
        _ctx: &ConvertContext,
    ) -> Result<ConvertOutput> {
        Err(Error::Other(
            "ZIP conversion is not implemented yet (stub registered for extension)".into(),
        ))
    }
}
