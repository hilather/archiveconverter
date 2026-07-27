//! Automatic 7z `-mmt` policy based on archive shape.

use crate::archive::{ArchiveBackend, EntryMeta};

/// Defaults tuned from benches: many tiny files → single-thread pack is faster.
pub const HIGH_FILE_COUNT: usize = 1_000;
pub const SMALL_AVG_SIZE: u64 = 64 * 1024; // 64 KiB

/// Resolve pack thread count.
///
/// - If `explicit` is set, always honor it.
/// - Else if member stats look like "many tiny files", return `Some(1)`.
/// - Else return `None` (backend uses `-mmt=on`).
pub fn resolve_pack_threads(
    explicit: Option<u32>,
    file_count: usize,
    total_bytes: u64,
) -> Option<u32> {
    if let Some(t) = explicit {
        return Some(t);
    }
    if file_count == 0 {
        return None;
    }
    let avg = total_bytes / file_count as u64;
    if file_count >= HIGH_FILE_COUNT || avg < SMALL_AVG_SIZE {
        tracing::debug!(
            file_count,
            avg_bytes = avg,
            "auto pack threads → 1 (many/tiny files)"
        );
        Some(1)
    } else {
        tracing::debug!(
            file_count,
            avg_bytes = avg,
            "auto pack threads → mmt=on"
        );
        None
    }
}

/// Stats from a listing for auto policy.
pub fn file_stats(entries: &[EntryMeta]) -> (usize, u64) {
    let mut n = 0usize;
    let mut bytes = 0u64;
    for e in entries {
        if !e.is_dir {
            n += 1;
            bytes = bytes.saturating_add(e.size);
        }
    }
    (n, bytes)
}

/// List archive and resolve threads (used when we need a listing anyway).
pub fn resolve_from_archive(
    backend: &dyn ArchiveBackend,
    archive: &std::path::Path,
    explicit: Option<u32>,
) -> crate::error::Result<Option<u32>> {
    if explicit.is_some() {
        return Ok(explicit);
    }
    let entries = backend.list(archive)?;
    let (n, bytes) = file_stats(&entries);
    Ok(resolve_pack_threads(None, n, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honors_explicit() {
        assert_eq!(resolve_pack_threads(Some(4), 1_000_000, 100), Some(4));
    }

    #[test]
    fn many_tiny_forces_one() {
        assert_eq!(
            resolve_pack_threads(None, 50_000, 50_000 * 300),
            Some(1)
        );
    }

    #[test]
    fn few_large_uses_default_mt() {
        assert_eq!(
            resolve_pack_threads(None, 10, 10 * 10 * 1024 * 1024),
            None
        );
    }
}
