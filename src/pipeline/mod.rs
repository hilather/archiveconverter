//! Orchestrates nested 7z conversion with one-at-a-time nested processing.

pub mod context;
pub mod plan;

pub use context::PipelineOptions;
pub use plan::{build_plan, ActionKind, ConversionPlan, PlanOptions};

use crate::archive::{ArchiveBackend, PackOptions};
use crate::convert::{ConvertContext, ConverterRegistry};
use crate::error::{Error, Result};
use crate::util::pathnorm::{is_safe_member_path, normalize_member_path};
use crate::util::temp::{remove_dir_all_quiet, JobTemp};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Run conversion (or dry-run). Returns the plan that was executed/printed.
pub fn run(backend: &dyn ArchiveBackend, opts: &PipelineOptions) -> Result<ConversionPlan> {
    if !opts.input.is_file() {
        return Err(Error::Other(format!(
            "input not found: {}",
            opts.input.display()
        )));
    }

    let entries = backend.list(&opts.input)?;
    let plan = build_plan(
        opts.input.clone(),
        opts.output.clone(),
        &entries,
        PlanOptions {
            outer_filter: &opts.exclude_outer,
            rename: &opts.rename,
            convert_nested_7z: true,
        },
    )?;

    if opts.dry_run {
        println!("{plan}");
        return Ok(plan);
    }

    execute(backend, opts, &plan)?;
    Ok(plan)
}

fn execute(backend: &dyn ArchiveBackend, opts: &PipelineOptions, plan: &ConversionPlan) -> Result<()> {
    let start = Instant::now();
    let job = JobTemp::create(opts.temp_dir.as_deref(), opts.keep_temp)?;
    let staging = job.child("outer-staging");
    fs::create_dir_all(&staging)?;

    // Ensure nested concurrency is at least 1; v1 processes serially regardless.
    let _ = opts.nested_concurrency.max(1);
    let registry = ConverterRegistry::with_builtins();

    let mut nested_index = 0usize;
    for entry in &plan.entries {
        match entry.action {
            ActionKind::Skip => {
                tracing::info!(path = %entry.source_path, "skipping outer member");
            }
            ActionKind::Passthrough => {
                tracing::info!(
                    path = %entry.source_path,
                    dest = %entry.dest_path,
                    "passthrough outer member"
                );
                if !is_safe_member_path(&entry.dest_path) {
                    return Err(Error::Other(format!(
                        "unsafe destination path: {}",
                        entry.dest_path
                    )));
                }
                let dest = staging.join(&entry.dest_path);
                backend.extract_member(&opts.input, &entry.source_path, &dest)?;
            }
            ActionKind::ConvertNested => {
                tracing::info!(
                    path = %entry.source_path,
                    dest = %entry.dest_path,
                    index = nested_index,
                    "converting nested 7z (one at a time)"
                );
                convert_one_nested(
                    backend,
                    &registry,
                    opts,
                    job.path(),
                    nested_index,
                    &entry.source_path,
                    &entry.dest_path,
                    &staging,
                )?;
                nested_index += 1;
            }
        }
    }

    // Pack staging into final non-solid outer archive.
    if let Some(parent) = opts.output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    // Write to temp then move into place (rename when same FS; else copy).
    let out_tmp = job.child("final-out.7z");
    let mut pack = opts.pack.clone();
    pack.non_solid = true;
    let t = Instant::now();
    backend.pack_dir(&staging, &out_tmp, &pack)?;
    tracing::debug!(
        stage = "pack_outer",
        ms = t.elapsed().as_millis() as u64,
        "packed outer archive"
    );

    if opts.verify {
        tracing::info!("verifying output archive");
        backend.test(&out_tmp)?;
        // Spot-check: list should not be empty if plan had keep entries
        let listed = backend.list(&out_tmp)?;
        let files = listed.iter().filter(|e| !e.is_dir).count();
        let expected = plan.passthrough_count() + plan.nested_count();
        if files != expected {
            return Err(Error::Other(format!(
                "verify failed: expected {expected} files in output, found {files}"
            )));
        }
    }

    persist_file(&out_tmp, &opts.output)?;

    tracing::info!(
        output = %opts.output.display(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "conversion complete"
    );
    Ok(())
}

/// Move `src` to `dest`, falling back to copy+remove across filesystems.
fn persist_file(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            fs::copy(src, dest)?;
            let _ = fs::remove_file(src);
            Ok(())
        }
        Err(e) => Err(Error::Io(e)),
    }
}

fn convert_one_nested(
    backend: &dyn ArchiveBackend,
    registry: &ConverterRegistry,
    opts: &PipelineOptions,
    job_root: &Path,
    index: usize,
    source_member: &str,
    dest_member: &str,
    staging: &Path,
) -> Result<()> {
    let nested_root = job_root.join(format!("nested-{index:04}"));
    if nested_root.exists() {
        remove_dir_all_quiet(&nested_root);
    }
    fs::create_dir_all(&nested_root)?;

    let inner_in = nested_root.join("inner-in.7z");
    backend.extract_member(&opts.input, source_member, &inner_in)?;

    let conv = registry
        .get("7z-solid-to-nonsolid")
        .ok_or_else(|| Error::Other("missing 7z converter".into()))?;

    let mut ctx = ConvertContext::new(nested_root.join("convert"));
    fs::create_dir_all(&ctx.temp_dir)?;
    ctx.exclude = opts.exclude_inner.clone();
    ctx.pack = PackOptions {
        non_solid: true,
        threads: opts.pack.threads,
        level: opts.pack.level,
    };
    // Verify only the final outer archive. Per-nested `7z t` on million-file
    // archives would dominate wall time without improving outer integrity much.
    ctx.verify = false;

    let t = Instant::now();
    let output = conv.convert(backend, &inner_in, &ctx)?;
    tracing::debug!(
        stage = "nested_convert",
        index,
        ms = t.elapsed().as_millis() as u64,
        "nested solid→non-solid done"
    );

    if !is_safe_member_path(dest_member) {
        return Err(Error::Other(format!(
            "unsafe destination path: {dest_member}"
        )));
    }
    let dest = staging.join(dest_member);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // Prefer rename within the same temp volume; copy only if needed.
    persist_file(&output.path, &dest)?;

    // Free disk before next nested archive.
    remove_dir_all_quiet(&nested_root);
    Ok(())
}

/// Convert a single (non-nested) 7z archive to non-solid — used by tests and simple mode.
pub fn convert_single(
    backend: &dyn ArchiveBackend,
    input: &Path,
    output: &Path,
    exclude_inner: &crate::filter::MemberFilter,
    pack: &PackOptions,
    verify: bool,
    temp_parent: Option<&Path>,
    keep_temp: bool,
) -> Result<()> {
    let job = JobTemp::create(temp_parent, keep_temp)?;
    let registry = ConverterRegistry::with_builtins();
    let conv = registry
        .get("7z-solid-to-nonsolid")
        .ok_or_else(|| Error::Other("missing converter".into()))?;

    let mut ctx = ConvertContext::new(job.child("convert"));
    fs::create_dir_all(&ctx.temp_dir)?;
    ctx.exclude = exclude_inner.clone();
    ctx.pack = pack.clone();
    ctx.pack.non_solid = true;
    ctx.verify = verify;

    let out = conv.convert(backend, input, &ctx)?;
    persist_file(&out.path, output)?;
    Ok(())
}

/// Utility for tests: normalized file list from archive.
pub fn list_file_paths(backend: &dyn ArchiveBackend, archive: &Path) -> Result<Vec<String>> {
    let mut v: Vec<String> = backend
        .list(archive)?
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| normalize_member_path(&e.path))
        .collect();
    v.sort();
    Ok(v)
}

pub fn staging_path_for(dest_member: &str) -> PathBuf {
    PathBuf::from(normalize_member_path(dest_member))
}
