//! Convert a 7z archive to non-solid form, applying member exclusions.

use super::{ConvertContext, ConvertOutput, Converter};
use crate::archive::detect::format_from_path;
use crate::archive::{ArchiveBackend, ArchiveFormat, EntryMeta};
use crate::error::{Error, Result};
use crate::util::pathnorm::{is_safe_member_path, normalize_member_path};
use crate::util::temp::remove_dir_all_quiet;
use std::fs;
use std::path::Path;
use std::time::Instant;

/// Rebuild a 7z archive with `-ms=off`, optionally excluding members by filter.
pub struct SevenZSolidToNonSolid;

impl Converter for SevenZSolidToNonSolid {
    fn id(&self) -> &'static str {
        "7z-solid-to-nonsolid"
    }

    fn description(&self) -> &'static str {
        "Rebuild 7z as non-solid (-ms=off), with optional member exclusion"
    }

    fn matches(&self, entry: &EntryMeta) -> bool {
        !entry.is_dir && entry.format_hint == ArchiveFormat::SevenZ
    }

    fn convert(
        &self,
        backend: &dyn ArchiveBackend,
        input: &Path,
        ctx: &ConvertContext,
    ) -> Result<ConvertOutput> {
        if !input.is_file() {
            return Err(Error::Other(format!(
                "input archive not found: {}",
                input.display()
            )));
        }

        let total_start = Instant::now();
        let work = ctx.temp_dir.join("work");
        let tree = work.join("tree");
        let out = work.join("out.7z");
        if work.exists() {
            remove_dir_all_quiet(&work);
        }
        fs::create_dir_all(&tree)?;

        let filter_active = !ctx.exclude.is_empty();

        // Listing 1M members via `7z l -slt` is ~5s and ~175MB of text. Skip when
        // there is nothing to filter; extract+pack still yields a correct non-solid archive.
        let entries = if filter_active {
            let t = Instant::now();
            let entries = backend.list(input)?;
            tracing::debug!(
                stage = "list",
                ms = t.elapsed().as_millis() as u64,
                members = entries.iter().filter(|e| !e.is_dir).count(),
                "listed archive for filtering"
            );
            Some(entries)
        } else {
            None
        };

        if let Some(ref entries) = entries {
            let kept = entries
                .iter()
                .filter(|e| !e.is_dir)
                .filter(|e| is_safe_member_path(&e.path))
                .filter(|e| ctx.exclude.should_keep(&e.path))
                .count();
            tracing::info!(
                input = %input.display(),
                total = entries.iter().filter(|e| !e.is_dir).count(),
                kept,
                "converting 7z to non-solid"
            );
        } else {
            tracing::info!(
                input = %input.display(),
                "converting 7z to non-solid (no member filters; skipping pre-list)"
            );
        }

        // Extract whole archive then delete excluded paths (solid streams require sequential decode).
        let t = Instant::now();
        backend.extract_all(input, &tree)?;
        tracing::debug!(
            stage = "extract",
            ms = t.elapsed().as_millis() as u64,
            "extracted archive to tree"
        );

        // Remove excluded / unsafe files from tree (only when filters are active).
        if let Some(ref entries) = entries {
            let t = Instant::now();
            let mut removed = 0u64;
            for e in entries.iter().filter(|e| !e.is_dir) {
                let rel = normalize_member_path(&e.path);
                let full = tree.join(&rel);
                let keep_it = is_safe_member_path(&rel) && ctx.exclude.should_keep(&rel);
                if !keep_it && full.exists() {
                    tracing::debug!(path = %rel, "removing excluded member");
                    let _ = fs::remove_file(&full);
                    removed += 1;
                }
            }
            if removed > 0 {
                prune_empty_dirs(&tree)?;
            }
            tracing::debug!(
                stage = "filter_tree",
                ms = t.elapsed().as_millis() as u64,
                removed,
                "applied member filters to tree"
            );
        }

        let mut pack_opts = ctx.pack.clone();
        pack_opts.non_solid = true;
        let t = Instant::now();
        backend.pack_dir(&tree, &out, &pack_opts)?;
        tracing::debug!(
            stage = "pack",
            ms = t.elapsed().as_millis() as u64,
            "packed non-solid archive"
        );

        // Drop the unpacked tree as soon as packing finishes so peak disk falls earlier
        // (especially important before the caller copies the result and cleans the job dir).
        let t = Instant::now();
        remove_dir_all_quiet(&tree);
        tracing::debug!(
            stage = "cleanup_tree",
            ms = t.elapsed().as_millis() as u64,
            "removed extract tree"
        );

        if ctx.verify {
            let t = Instant::now();
            backend.test(&out)?;
            tracing::debug!(
                stage = "verify",
                ms = t.elapsed().as_millis() as u64,
                "verified converted archive"
            );
        }

        tracing::debug!(
            stage = "convert_total",
            ms = total_start.elapsed().as_millis() as u64,
            "solid→non-solid convert finished"
        );

        Ok(ConvertOutput { path: out })
    }
}

fn prune_empty_dirs(root: &Path) -> Result<()> {
    let mut dirs: Vec<_> = walkdir::WalkDir::new(root)
        .contents_first(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.into_path())
        .collect();
    // deepest first already via contents_first
    for d in dirs.drain(..) {
        if d == root {
            continue;
        }
        if fs::read_dir(&d)?.next().is_none() {
            let _ = fs::remove_dir(&d);
        }
    }
    Ok(())
}

/// Helper used by pipeline for non-entry conversions (path-based).
pub fn convert_sevenz_file(
    backend: &dyn ArchiveBackend,
    input: &Path,
    ctx: &ConvertContext,
) -> Result<ConvertOutput> {
    let entry = EntryMeta {
        path: input
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archive.7z")
            .to_string(),
        size: fs::metadata(input).map(|m| m.len()).unwrap_or(0),
        is_dir: false,
        format_hint: format_from_path(
            input
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("x.7z"),
        ),
    };
    let conv = SevenZSolidToNonSolid;
    if !conv.matches(&entry) {
        return Err(Error::Other("not a 7z archive".into()));
    }
    conv.convert(backend, input, ctx)
}
