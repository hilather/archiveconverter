//! Build a conversion plan for dry-run and execution.

use crate::archive::{ArchiveFormat, EntryMeta};
use crate::codec::FileMeta;
use crate::error::Result;
use crate::filter::{MemberFilter, NameTransformer};
use crate::util::pathnorm::{is_safe_member_path, normalize_member_path};
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Skip,
    Passthrough,
    ConvertNested,
}

#[derive(Debug, Clone)]
pub struct PlannedEntry {
    pub source_path: String,
    pub dest_path: String,
    pub size: u64,
    pub action: ActionKind,
    pub reason: String,
    /// Source outer-member file info (mtime/attrs) when known from listing.
    pub meta: FileMeta,
}

/// Counts filled after a live convert (not dry-run). Plan-time skips stay on
/// [`ConversionPlan::skip_count`]; these are members dropped while executing.
#[derive(Debug, Clone, Default)]
pub struct RuntimeStats {
    pub nested_converted: usize,
    pub nested_skipped: usize,
    pub passthrough_written: usize,
    pub passthrough_skipped: usize,
}

impl RuntimeStats {
    pub fn runtime_skipped(&self) -> usize {
        self.nested_skipped + self.passthrough_skipped
    }
}

#[derive(Debug, Clone)]
pub struct ConversionPlan {
    pub input: PathBuf,
    pub output: PathBuf,
    pub entries: Vec<PlannedEntry>,
    /// Populated by [`crate::pipeline::run`] after a real convert.
    pub runtime: RuntimeStats,
}

impl ConversionPlan {
    pub fn nested_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.action == ActionKind::ConvertNested)
            .count()
    }

    pub fn passthrough_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.action == ActionKind::Passthrough)
            .count()
    }

    pub fn skip_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.action == ActionKind::Skip)
            .count()
    }
}

impl fmt::Display for ConversionPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Plan: {} -> {}", self.input.display(), self.output.display())?;
        writeln!(
            f,
            "  nested converts: {}  passthrough: {}  skipped: {}",
            self.nested_count(),
            self.passthrough_count(),
            self.skip_count()
        )?;
        for e in &self.entries {
            let action = match e.action {
                ActionKind::Skip => "SKIP",
                ActionKind::Passthrough => "COPY",
                ActionKind::ConvertNested => "CONVERT",
            };
            if e.source_path == e.dest_path {
                writeln!(
                    f,
                    "  [{action}] {} ({} bytes) — {}",
                    e.source_path, e.size, e.reason
                )?;
            } else {
                writeln!(
                    f,
                    "  [{action}] {} -> {} ({} bytes) — {}",
                    e.source_path, e.dest_path, e.size, e.reason
                )?;
            }
        }
        Ok(())
    }
}

pub struct PlanOptions<'a> {
    pub outer_filter: &'a MemberFilter,
    pub rename: &'a NameTransformer,
    /// When true, treat nested 7z as ConvertNested; otherwise passthrough.
    pub convert_nested_7z: bool,
}

/// Build a plan from outer archive listing.
pub fn build_plan(
    input: PathBuf,
    output: PathBuf,
    entries: &[EntryMeta],
    opts: PlanOptions<'_>,
) -> Result<ConversionPlan> {
    let file_entries: Vec<&EntryMeta> = entries.iter().filter(|e| !e.is_dir).collect();

    // Pre-compute renames for non-skipped members that we keep.
    let mut planned = Vec::new();
    let mut seen_src = HashSet::new();
    for e in &file_entries {
        let src = normalize_member_path(&e.path);
        if src.is_empty() {
            planned.push(PlannedEntry {
                source_path: e.path.clone(),
                dest_path: String::new(),
                size: e.size,
                action: ActionKind::Skip,
                reason: "empty member path".into(),
                meta: e.meta.clone(),
            });
            continue;
        }
        if !is_safe_member_path(&src) {
            planned.push(PlannedEntry {
                source_path: src,
                dest_path: String::new(),
                size: e.size,
                action: ActionKind::Skip,
                reason: "unsafe member path".into(),
                meta: e.meta.clone(),
            });
            continue;
        }
        if !seen_src.insert(src.clone()) {
            planned.push(PlannedEntry {
                source_path: src,
                dest_path: String::new(),
                size: e.size,
                action: ActionKind::Skip,
                reason: "duplicate member path in listing".into(),
                meta: e.meta.clone(),
            });
            continue;
        }
        if !opts.outer_filter.should_keep(&src) {
            planned.push(PlannedEntry {
                source_path: src,
                dest_path: String::new(),
                size: e.size,
                action: ActionKind::Skip,
                reason: "excluded by outer filter".into(),
                meta: e.meta.clone(),
            });
            continue;
        }

        let is_nested_7z = e.format_hint == ArchiveFormat::SevenZ && opts.convert_nested_7z;
        let action = if is_nested_7z {
            ActionKind::ConvertNested
        } else {
            ActionKind::Passthrough
        };
        let reason = if is_nested_7z {
            "nested 7z → non-solid convert".into()
        } else {
            "repack into non-solid outer".into()
        };

        planned.push(PlannedEntry {
            source_path: src,
            dest_path: String::new(), // filled after rename
            size: e.size,
            action,
            reason,
            meta: e.meta.clone(),
        });
    }

    // Apply renames only to non-skipped; detect collisions.
    let keep_names: Vec<String> = planned
        .iter()
        .filter(|p| p.action != ActionKind::Skip)
        .map(|p| p.source_path.clone())
        .collect();
    let rename_map = opts
        .rename
        .transform_all(keep_names.iter().map(|s| s.as_str()))?;

    for p in planned.iter_mut() {
        if p.action == ActionKind::Skip {
            continue;
        }
        let dest = rename_map
            .get(&p.source_path)
            .cloned()
            .unwrap_or_else(|| p.source_path.clone());
        if dest != p.source_path {
            p.reason = format!("{}; renamed", p.reason);
        }
        if !is_safe_member_path(&dest) {
            p.action = ActionKind::Skip;
            p.reason = format!("unsafe destination path after rename: {dest}");
            p.dest_path = String::new();
            continue;
        }
        p.dest_path = dest;
    }

    Ok(ConversionPlan {
        input,
        output,
        entries: planned,
        runtime: RuntimeStats::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{MemberFilter, NameTransformer};

    fn meta(path: &str, format: ArchiveFormat) -> EntryMeta {
        EntryMeta {
            path: path.into(),
            meta: Default::default(),
            size: 100,
            is_dir: false,
            format_hint: format,
        }
    }

    #[test]
    fn plans_nested_and_skip() {
        let entries = vec![
            meta("a_old.7z", ArchiveFormat::SevenZ),
            meta("readme.txt", ArchiveFormat::Unknown),
            meta("skip.7z", ArchiveFormat::SevenZ),
        ];
        let outer = MemberFilter::with_excludes([r"^skip\.7z$"]).unwrap();
        let rename = NameTransformer::from_pairs([r"_old\.7z$=.7z"]).unwrap();
        let plan = build_plan(
            "in.7z".into(),
            "out.7z".into(),
            &entries,
            PlanOptions {
                outer_filter: &outer,
                rename: &rename,
                convert_nested_7z: true,
            },
        )
        .unwrap();

        assert_eq!(plan.nested_count(), 1);
        assert_eq!(plan.passthrough_count(), 1);
        assert_eq!(plan.skip_count(), 1);
        let a = plan
            .entries
            .iter()
            .find(|e| e.source_path == "a_old.7z")
            .unwrap();
        assert_eq!(a.dest_path, "a.7z");
        assert_eq!(a.action, ActionKind::ConvertNested);
    }

    #[test]
    fn skips_unsafe_and_duplicate_members() {
        let entries = vec![
            meta("../evil.7z", ArchiveFormat::SevenZ),
            meta("ok.7z", ArchiveFormat::SevenZ),
            meta("ok.7z", ArchiveFormat::SevenZ),
            meta("", ArchiveFormat::Unknown),
        ];
        let outer = MemberFilter::new();
        let rename = NameTransformer::new();
        let plan = build_plan(
            "in.7z".into(),
            "out.7z".into(),
            &entries,
            PlanOptions {
                outer_filter: &outer,
                rename: &rename,
                convert_nested_7z: true,
            },
        )
        .unwrap();
        assert_eq!(plan.nested_count(), 1);
        assert_eq!(plan.skip_count(), 3);
        assert!(plan
            .entries
            .iter()
            .any(|e| e.reason.contains("unsafe")));
        assert!(plan
            .entries
            .iter()
            .any(|e| e.reason.contains("duplicate")));
    }

    #[test]
    fn skips_unsafe_rename_destination() {
        let entries = vec![
            meta("ok.7z", ArchiveFormat::SevenZ),
            meta("readme.txt", ArchiveFormat::Unknown),
        ];
        let outer = MemberFilter::new();
        let rename = NameTransformer::from_pairs([r"readme\.txt$=../evil"]).unwrap();
        let plan = build_plan(
            "in.7z".into(),
            "out.7z".into(),
            &entries,
            PlanOptions {
                outer_filter: &outer,
                rename: &rename,
                convert_nested_7z: true,
            },
        )
        .unwrap();
        assert_eq!(plan.nested_count(), 1);
        assert_eq!(plan.passthrough_count(), 0);
        assert_eq!(plan.skip_count(), 1);
        assert!(plan
            .entries
            .iter()
            .any(|e| e.source_path == "readme.txt" && e.action == ActionKind::Skip));
    }
}
