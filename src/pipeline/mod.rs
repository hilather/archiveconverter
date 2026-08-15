//! Orchestrates nested 7z conversion with size-aware concurrency and solid single-pass.

pub mod context;
pub mod plan;

pub use context::PipelineOptions;
pub use plan::{build_plan, ActionKind, ConversionPlan, PlanOptions, RuntimeStats};

use crate::archive::native::NativeSevenZ;
use crate::archive::{ArchiveBackend, EntryMeta, PackOptions};
use crate::codec::{
    count_dir_files, count_tar_files, FileMeta, OuterFormat, SyncedOuterWriter,
};
use crate::convert::{ConvertContext, ConverterRegistry};
use crate::error::{Error, Result};
use crate::util::pathnorm::{is_safe_member_path, normalize_member_path};
use crate::util::size_parse::{can_admit_nested, resolve_nested_workers};
use crate::util::temp::{remove_dir_all_quiet, JobTemp};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Instant;

/// Fill missing per-member times/attrs from a native 7z header parse.
///
/// CLI `list`/`extract -so` often lack FILETIME; the pure-Rust reader still
/// surfaces exact header metadata for outer store preservation.
fn enrich_entry_meta(archive: &Path, entries: &mut [EntryMeta]) {
    let native = NativeSevenZ::new();
    let Ok(listed) = native.list(archive) else {
        return;
    };
    let by_path: HashMap<String, FileMeta> = listed
        .into_iter()
        .map(|e| (normalize_member_path(&e.path), e.meta))
        .collect();
    for e in entries.iter_mut() {
        let key = normalize_member_path(&e.path);
        if let Some(m) = by_path.get(&key) {
            // Prefer exact header fields from the native parse over incomplete
            // CLI listings (e.g. BA attr "A" → 0x20 without Unix mode bits).
            e.meta.mtime = m.mtime.or(e.meta.mtime);
            e.meta.ctime = m.ctime.or(e.meta.ctime);
            e.meta.atime = m.atime.or(e.meta.atime);
            e.meta.windows_attributes =
                m.windows_attributes.or(e.meta.windows_attributes);
        }
    }
}

/// Run conversion (or dry-run). Returns the plan that was executed/printed.
pub fn run(backend: &dyn ArchiveBackend, opts: &PipelineOptions) -> Result<ConversionPlan> {
    if !opts.input.is_file() {
        return Err(Error::Other(format!(
            "input not found: {}",
            opts.input.display()
        )));
    }

    let mut entries = backend.list(&opts.input)?;
    enrich_entry_meta(&opts.input, &mut entries);
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

    let runtime = execute(backend, opts, &plan)?;
    let mut plan = plan;
    plan.runtime = runtime;
    Ok(plan)
}

fn execute(
    backend: &dyn ArchiveBackend,
    opts: &PipelineOptions,
    plan: &ConversionPlan,
) -> Result<RuntimeStats> {
    let start = Instant::now();
    let job = JobTemp::create(opts.temp_dir.as_deref(), opts.keep_temp)?;

    let nest_n = plan.nested_count();
    // One nest: never parallelize across nests, and force pack/encode threads=1.
    // Full benches show multi-thread LZMA often *slower* on dense tiny-file nests.
    let max_workers = if nest_n <= 1 {
        1
    } else {
        resolve_nested_workers(opts.nested_concurrency, opts.pack.threads)
    };
    let size_budget = opts.nested_size_budget;
    let registry = ConverterRegistry::with_builtins();

    let needed: Vec<&str> = plan
        .entries
        .iter()
        .filter(|e| e.action != ActionKind::Skip)
        .map(|e| e.source_path.as_str())
        .collect();

    // When workers > 1, bulk-extract nested sources first so workers do not
    // re-decode a solid outer in parallel.
    let want_bulk = (opts.solid_single_pass && needed.len() > 1)
        || (max_workers > 1 && plan.nested_count() > 1);
    let solid = backend.is_solid(&opts.input).unwrap_or(false);
    let use_solid_pass = want_bulk && (solid || max_workers > 1);

    let outer_pass = job.child("outer-solid-pass");
    let mut use_solid_pass = use_solid_pass;
    if use_solid_pass {
        let t = Instant::now();
        tracing::info!(
            members = needed.len(),
            solid,
            max_workers,
            size_budget,
            "bulk extract of needed outer members (single-pass / pre-stage for concurrency)"
        );
        match backend.extract_members(&opts.input, &needed, &outer_pass) {
            Ok(()) => log_stage(opts, "outer_bulk_extract", t.elapsed()),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "bulk extract failed; falling back to per-member extract so remaining members can still be written"
                );
                eprintln!(
                    "warning: bulk extract failed ({e}); falling back to per-member extract"
                );
                use_solid_pass = false;
                remove_dir_all_quiet(&outer_pass);
            }
        }
    }

    // Streaming outer: append finished members as they complete (mutex-serialized).
    if let Some(parent) = opts.output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let outer_format = opts.outer_format;
    let out_tmp = if outer_format.is_directory() {
        job.child("final-out-dir")
    } else {
        job.child(&format!("final-out.{}", outer_format.extension()))
    };
    let outer = Arc::new(SyncedOuterWriter::create(&out_tmp, outer_format)?);
    tracing::info!(
        path = %out_tmp.display(),
        format = outer_format.as_str(),
        "outer: append writer (7z/tar store, or directory — no recompress wrap)"
    );

    // Passthrough files first (cheap) — stream into outer under the same mutex.
    // Unexpected types / extract failures are skipped so the rest of the archive
    // can still be written (same policy as corrupt nested converts).
    let pass_tmp = job.child("passthrough-tmp");
    fs::create_dir_all(&pass_tmp)?;
    let mut passthrough_written = 0usize;
    let mut passthrough_skipped = 0usize;
    for (pass_i, entry) in plan.entries.iter().enumerate() {
        if entry.action != ActionKind::Passthrough {
            continue;
        }
        match stage_passthrough(
            backend,
            opts,
            entry,
            pass_i,
            &pass_tmp,
            use_solid_pass,
            &outer_pass,
            &outer,
        ) {
            Ok(()) => passthrough_written += 1,
            Err(e) => {
                log_member_skip("passthrough member", &entry.source_path, &e.to_string());
                passthrough_skipped += 1;
            }
        }
    }
    remove_dir_all_quiet(&pass_tmp);

    let nested: Vec<_> = plan
        .entries
        .iter()
        .filter(|e| e.action == ActionKind::ConvertNested)
        .collect();

    let mut nested_converted = 0usize;
    let mut nested_skipped = 0usize;
    if !nested.is_empty() {
        // For a single nested archive, pin compress threads to 1 even if --threads N.
        let mut nested_opts = opts.clone();
        if nested.len() <= 1 {
            if opts.pack.threads.map(|t| t != 1).unwrap_or(true) {
                tracing::info!(
                    "single nested archive: forcing --threads 1 for nest convert (MT often slower)"
                );
            }
            nested_opts.pack.threads = Some(1);
        }
        let stats = convert_nested_size_aware(
            backend,
            &registry,
            &nested_opts,
            job.path(),
            Arc::clone(&outer),
            use_solid_pass,
            &outer_pass,
            &nested,
            max_workers,
            size_budget,
        )?;
        nested_converted = stats.converted;
        nested_skipped = stats.skipped;
        if nested_skipped > 0 {
            tracing::warn!(
                converted = nested_converted,
                skipped = nested_skipped,
                "some nested archives were skipped (see earlier errors); they are not in the output"
            );
        }
    }

    if use_solid_pass {
        remove_dir_all_quiet(&outer_pass);
    }

    // Drop all Arc clones except ours so we can finish the writer.
    let outer = Arc::try_unwrap(outer).map_err(|_| {
        Error::Other("outer writer still shared; internal bug (producers not joined)".into())
    })?;

    if outer.is_empty()? {
        return Err(Error::Other(format!(
            "nothing to write: all {} nested archive(s) failed and there are no usable passthrough members",
            nested.len()
        )));
    }

    let t = Instant::now();
    let member_count = outer.len()?;
    outer.finish()?;
    log_stage(opts, "pack_outer_append_finish", t.elapsed());
    tracing::info!(
        members = member_count,
        format = outer_format.as_str(),
        ms = t.elapsed().as_millis() as u64,
        "outer finalize complete"
    );

    if opts.verify {
        tracing::info!(format = outer_format.as_str(), "verifying output");
        let expected = passthrough_written + nested_converted;
        let files = match outer_format {
            OuterFormat::SevenZ => {
                backend.test(&out_tmp)?;
                let listed = backend.list(&out_tmp)?;
                listed.iter().filter(|e| !e.is_dir).count()
            }
            OuterFormat::Tar => count_tar_files(&out_tmp)?,
            OuterFormat::Dir => count_dir_files(&out_tmp)?,
        };
        if files != expected {
            return Err(Error::Other(format!(
                "verify failed: expected {expected} files in output (passthrough + converted nested; {nested_skipped} skipped), found {files}"
            )));
        }
    }

    if outer_format.is_directory() {
        persist_dir(&out_tmp, &opts.output)?;
    } else {
        persist_file(&out_tmp, &opts.output)?;
    }

    tracing::info!(
        output = %opts.output.display(),
        outer_format = outer_format.as_str(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        solid_single_pass = use_solid_pass,
        nested_workers = max_workers,
        nested_size_budget = size_budget,
        nested_converted,
        nested_skipped,
        passthrough_written,
        passthrough_skipped,
        "conversion complete"
    );
    Ok(RuntimeStats {
        nested_converted,
        nested_skipped,
        passthrough_written,
        passthrough_skipped,
    })
}

/// Outcome of nested converts: successful members + soft-failed (corrupt/unreadable).
struct NestedRunStats {
    converted: usize,
    skipped: usize,
}

fn log_nested_skip(source_path: &str, err: &str) {
    log_member_skip("nested archive", source_path, err);
}

fn log_member_skip(kind: &str, source_path: &str, err: &str) {
    tracing::error!(
        path = %source_path,
        error = %err,
        kind,
        "skipping member; it will NOT appear in the output archive"
    );
    // Always surface on stderr so operators see it even at default log levels.
    eprintln!("warning: skipping {kind} '{source_path}': {err} (not included in output)");
}

fn stage_passthrough(
    backend: &dyn ArchiveBackend,
    opts: &PipelineOptions,
    entry: &plan::PlannedEntry,
    index: usize,
    pass_tmp: &Path,
    use_solid_pass: bool,
    outer_pass: &Path,
    outer: &SyncedOuterWriter,
) -> Result<()> {
    if !is_safe_member_path(&entry.dest_path) {
        return Err(Error::Other(format!(
            "unsafe destination path: {}",
            entry.dest_path
        )));
    }
    // Unique temp name (index prefix avoids a/b vs a__b collisions).
    let tmp = pass_tmp.join(format!("{index:04}_{}", entry.dest_path.replace('/', "__")));
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    if use_solid_pass {
        let src = find_extracted(outer_pass, &entry.source_path)?;
        if !src.is_file() {
            return Err(Error::Other(format!(
                "extracted member is not a regular file: {}",
                src.display()
            )));
        }
        fs::copy(&src, &tmp)?;
        let _ = fs::remove_file(&src);
    } else {
        backend.extract_member(&opts.input, &entry.source_path, &tmp)?;
    }
    if !tmp.is_file() {
        let _ = fs::remove_file(&tmp);
        return Err(Error::Other(
            "extracted passthrough is not a regular file".into(),
        ));
    }
    let mut meta = entry.meta.clone();
    meta.merge_missing(&FileMeta::from_fs_path(&tmp));
    outer.push_path_with_meta(entry.dest_path.clone(), &tmp, Some(meta))?;
    let _ = fs::remove_file(&tmp);
    tracing::info!(
        path = %entry.source_path,
        dest = %entry.dest_path,
        "passthrough outer member (appended)"
    );
    Ok(())
}

fn log_stage(opts: &PipelineOptions, stage: &str, d: std::time::Duration) {
    if opts.profile {
        tracing::info!(stage, ms = d.as_millis() as u64, "profile");
    } else {
        tracing::debug!(stage, ms = d.as_millis() as u64, "stage done");
    }
}

/// Nested convert jobs ordered **smallest packed size first**, admitted while
/// `running_count < max_workers` and `running_sum + size <= size_budget`
/// (a lone nest may exceed the budget).
///
/// Corrupt / unreadable nested archives are **skipped** (logged, not fatal).
fn convert_nested_size_aware(
    backend: &dyn ArchiveBackend,
    registry: &ConverterRegistry,
    opts: &PipelineOptions,
    job_root: &Path,
    outer: Arc<SyncedOuterWriter>,
    use_solid_pass: bool,
    outer_pass: &Path,
    nested: &[&plan::PlannedEntry],
    max_workers: usize,
    size_budget: u64,
) -> Result<NestedRunStats> {
    // Materialize sources, then sort by packed size ascending.
    let mut jobs: Vec<NestedJob> = Vec::with_capacity(nested.len());
    let mut stage_skipped = 0usize;
    for (index, entry) in nested.iter().enumerate() {
        let path = if use_solid_pass {
            match find_extracted(outer_pass, &entry.source_path) {
                Ok(p) => p,
                Err(e) => {
                    log_nested_skip(&entry.source_path, &e.to_string());
                    stage_skipped += 1;
                    continue;
                }
            }
        } else {
            let p = job_root.join(format!("pre-{index:04}.7z"));
            if let Err(e) = backend.extract_member(&opts.input, &entry.source_path, &p) {
                log_nested_skip(&entry.source_path, &e.to_string());
                stage_skipped += 1;
                continue;
            }
            p
        };
        // Prefer listing size; fall back to on-disk size after extract.
        let size = if entry.size > 0 {
            entry.size
        } else {
            fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
        };
        // Capture outer-member meta before convert rewrites the nest file on disk.
        let mut meta = entry.meta.clone();
        meta.merge_missing(&FileMeta::from_fs_path(&path));
        jobs.push(NestedJob {
            index,
            source_path: entry.source_path.clone(),
            dest_path: entry.dest_path.clone(),
            size,
            path,
            meta,
        });
    }
    jobs.sort_by(|a, b| a.size.cmp(&b.size).then_with(|| a.index.cmp(&b.index)));

    if jobs.is_empty() {
        return Ok(NestedRunStats {
            converted: 0,
            skipped: stage_skipped,
        });
    }

    let max_workers = max_workers.max(1).min(jobs.len());
    tracing::info!(
        nested = jobs.len(),
        stage_skipped,
        max_workers,
        size_budget,
        smallest = jobs.first().map(|j| j.size).unwrap_or(0),
        largest = jobs.last().map(|j| j.size).unwrap_or(0),
        "nested convert schedule (smallest-first, size-aware)"
    );

    let mut stats = if max_workers == 1 {
        convert_nested_serial_ordered(backend, registry, opts, job_root, outer, jobs)?
    } else {
        convert_nested_parallel_budget(
            backend,
            registry,
            opts,
            job_root,
            outer,
            jobs,
            max_workers,
            size_budget,
        )?
    };
    stats.skipped += stage_skipped;
    Ok(stats)
}

struct NestedJob {
    index: usize,
    source_path: String,
    dest_path: String,
    size: u64,
    path: PathBuf,
    /// Outer-archive member metadata for the nested `.7z` (not inner file meta).
    meta: FileMeta,
}

fn convert_nested_serial_ordered(
    backend: &dyn ArchiveBackend,
    registry: &ConverterRegistry,
    opts: &PipelineOptions,
    job_root: &Path,
    outer: Arc<SyncedOuterWriter>,
    jobs: Vec<NestedJob>,
) -> Result<NestedRunStats> {
    let mut converted = 0usize;
    let mut skipped = 0usize;
    for job in jobs {
        tracing::info!(
            path = %job.source_path,
            dest = %job.dest_path,
            index = job.index,
            size = job.size,
            "converting nested 7z"
        );
        match convert_one_nested_from_file(
            backend,
            registry,
            opts,
            job_root,
            job.index,
            &job.path,
            &job.dest_path,
            &outer,
            &job.meta,
        ) {
            Ok(()) => converted += 1,
            Err(e) => {
                log_nested_skip(&job.source_path, &e.to_string());
                skipped += 1;
            }
        }
        let _ = fs::remove_file(&job.path);
    }
    Ok(NestedRunStats {
        converted,
        skipped,
    })
}

fn convert_nested_parallel_budget(
    backend: &dyn ArchiveBackend,
    registry: &ConverterRegistry,
    opts: &PipelineOptions,
    job_root: &Path,
    outer: Arc<SyncedOuterWriter>,
    jobs: Vec<NestedJob>,
    max_workers: usize,
    size_budget: u64,
) -> Result<NestedRunStats> {
    let total = jobs.len();
    let mut pending: VecDeque<NestedJob> = jobs.into();
    let opts = opts.clone();
    let job_root = job_root.to_path_buf();

    let (done_tx, done_rx) = mpsc::channel::<(u64, String, std::result::Result<(), String>)>();
    let mut running_count = 0usize;
    let mut running_sum = 0u64;
    let mut finished = 0usize;
    let mut converted = 0usize;
    let mut skipped = 0usize;
    let mut fatal: Option<String> = None;

    thread::scope(|scope| {
        // Local admit+spawn helper closes over scope / channels.
        let try_spawn = |pending: &mut VecDeque<NestedJob>,
                         running_count: &mut usize,
                         running_sum: &mut u64|
         -> Result<()> {
            while can_admit_nested(
                *running_sum,
                *running_count,
                pending.front().map(|j| j.size).unwrap_or(0),
                size_budget,
                max_workers,
            ) {
                let Some(job) = pending.pop_front() else {
                    break;
                };
                // Re-check after pop (front size used above).
                if !can_admit_nested(
                    *running_sum,
                    *running_count,
                    job.size,
                    size_budget,
                    max_workers,
                ) {
                    // Should not happen if front-only; put back just in case.
                    pending.push_front(job);
                    break;
                }

                tracing::info!(
                    path = %job.source_path,
                    dest = %job.dest_path,
                    index = job.index,
                    size = job.size,
                    running = *running_count + 1,
                    running_sum = *running_sum + job.size,
                    max_workers,
                    size_budget,
                    "converting nested 7z (size-aware worker)"
                );

                *running_sum += job.size;
                *running_count += 1;

                let done_tx = done_tx.clone();
                let opts = &opts;
                let job_root = &job_root;
                let outer = Arc::clone(&outer);
                let registry = registry;
                let backend = backend;
                let size = job.size;
                let src_name = job.source_path.clone();
                scope.spawn(move || {
                    let result = convert_one_nested_from_file(
                        backend,
                        registry,
                        opts,
                        job_root,
                        job.index,
                        &job.path,
                        &job.dest_path,
                        &outer,
                        &job.meta,
                    )
                    .map_err(|e| e.to_string());
                    let _ = fs::remove_file(&job.path);
                    let _ = done_tx.send((size, src_name, result));
                });
            }
            Ok(())
        };

        if let Err(e) = try_spawn(&mut pending, &mut running_count, &mut running_sum) {
            fatal = Some(e.to_string());
        }

        while finished < total && fatal.is_none() {
            if running_count == 0 {
                // Pending left but nothing admitted — force one (should be rare).
                if pending.is_empty() {
                    break;
                }
                if let Err(e) = try_spawn(&mut pending, &mut running_count, &mut running_sum) {
                    fatal = Some(e.to_string());
                    break;
                }
                if running_count == 0 {
                    fatal = Some("nested scheduler stalled with pending work".into());
                    break;
                }
            }

            match done_rx.recv() {
                Ok((size, src_name, result)) => {
                    running_sum = running_sum.saturating_sub(size);
                    running_count = running_count.saturating_sub(1);
                    finished += 1;
                    match result {
                        Ok(()) => converted += 1,
                        Err(e) => {
                            log_nested_skip(&src_name, &e);
                            skipped += 1;
                        }
                    }
                    if let Err(e) = try_spawn(&mut pending, &mut running_count, &mut running_sum)
                    {
                        fatal = Some(e.to_string());
                    }
                }
                Err(_) => {
                    fatal = Some("nested worker channel closed early".into());
                    break;
                }
            }
        }

        // Drain any still-running workers so scope can exit cleanly.
        while running_count > 0 {
            if let Ok((size, src_name, result)) = done_rx.recv() {
                running_sum = running_sum.saturating_sub(size);
                running_count = running_count.saturating_sub(1);
                finished += 1;
                match result {
                    Ok(()) => converted += 1,
                    Err(e) => {
                        log_nested_skip(&src_name, &e);
                        skipped += 1;
                    }
                }
            } else {
                break;
            }
        }
        drop(done_tx);
    });

    if let Some(e) = fatal {
        return Err(Error::Other(format!(
            "size-aware nested convert failed: {e}"
        )));
    }
    if finished != total {
        return Err(Error::Other(format!(
            "size-aware nested convert incomplete: finished {finished}/{total}"
        )));
    }
    Ok(NestedRunStats {
        converted,
        skipped,
    })
}

fn find_extracted(root: &Path, member: &str) -> Result<PathBuf> {
    let norm = normalize_member_path(member);
    if !is_safe_member_path(&norm) {
        return Err(Error::Other(format!("unsafe extracted member path: {norm}")));
    }
    let direct = root.join(&norm);
    if direct.is_file() {
        return Ok(direct);
    }
    let base = norm.rsplit('/').next().unwrap_or(&norm);
    let mut exact: Option<PathBuf> = None;
    let mut basename_hits: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::debug!(path = %dir.display(), error = %e, "skip unreadable extract dir");
                continue;
            }
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if !p.is_file() {
                continue;
            }
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            if normalize_member_path(&rel) == norm {
                exact = Some(p);
                break;
            }
            if p.file_name().and_then(|s| s.to_str()) == Some(base) {
                basename_hits.push(p);
            }
        }
        if exact.is_some() {
            break;
        }
    }
    if let Some(p) = exact {
        return Ok(p);
    }
    match basename_hits.len() {
        1 => Ok(basename_hits.remove(0)),
        0 => Err(Error::EntryNotFound(member.to_string())),
        n => Err(Error::Other(format!(
            "ambiguous extracted member '{member}': {n} files named '{base}'"
        ))),
    }
}

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

/// Move a finished outer directory into place (replace existing dest).
fn persist_dir(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    if dest.exists() {
        if dest.is_dir() {
            fs::remove_dir_all(dest).map_err(|e| {
                Error::Other(format!("remove existing output dir {}: {e}", dest.display()))
            })?;
        } else {
            fs::remove_file(dest)?;
        }
    }
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            copy_dir_recursive(src, dest)?;
            remove_dir_all_quiet(src);
            Ok(())
        }
        Err(e) => Err(Error::Other(format!(
            "move outer dir {} → {}: {e}",
            src.display(),
            dest.display()
        ))),
    }
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.map_err(|e| {
            Error::Other(format!("walk {} for persist: {e}", src.display()))
        })?;
        let rel = entry.path().strip_prefix(src).map_err(|e| {
            Error::Other(format!("strip prefix: {e}"))
        })?;
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        } else {
            tracing::warn!(
                path = %entry.path().display(),
                "skipping non-regular file while persisting directory outer"
            );
        }
    }
    Ok(())
}

fn convert_one_nested_from_file(
    backend: &dyn ArchiveBackend,
    registry: &ConverterRegistry,
    opts: &PipelineOptions,
    job_root: &Path,
    index: usize,
    inner_in: &Path,
    dest_member: &str,
    outer: &SyncedOuterWriter,
    outer_member_meta: &FileMeta,
) -> Result<()> {
    let nested_root = job_root.join(format!("nested-{index:04}"));
    if nested_root.exists() {
        remove_dir_all_quiet(&nested_root);
    }
    fs::create_dir_all(&nested_root)?;

    let result = (|| -> Result<()> {
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
        ctx.verify = false;
        ctx.passthrough_nonsolid = opts.passthrough_nonsolid;
        ctx.prefer_streaming = opts.prefer_streaming;

        let t = Instant::now();
        let output = conv.convert(backend, inner_in, &ctx)?;
        log_stage(opts, "nested_convert", t.elapsed());
        if !output.path.is_file() {
            return Err(Error::Other(format!(
                "nested convert produced no regular file: {}",
                output.path.display()
            )));
        }

        if !is_safe_member_path(dest_member) {
            return Err(Error::Other(format!(
                "unsafe destination path: {dest_member}"
            )));
        }
        // Preserve the outer member's source times/attrs (not the convert-temp file times).
        outer.push_path_with_meta(
            dest_member.to_string(),
            &output.path,
            Some(outer_member_meta.clone()),
        )?;
        Ok(())
    })();

    // Always scrub per-nested temp, including on convert failure (corrupt archive).
    remove_dir_all_quiet(&nested_root);
    result
}

/// Convert a single (non-nested) 7z archive to non-solid.
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
    convert_single_ex(
        backend,
        input,
        output,
        exclude_inner,
        pack,
        verify,
        temp_parent,
        keep_temp,
        true,
    )
}

/// Like [`convert_single`] with passthrough control.
pub fn convert_single_ex(
    backend: &dyn ArchiveBackend,
    input: &Path,
    output: &Path,
    exclude_inner: &crate::filter::MemberFilter,
    pack: &PackOptions,
    verify: bool,
    temp_parent: Option<&Path>,
    keep_temp: bool,
    passthrough_nonsolid: bool,
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
    ctx.passthrough_nonsolid = passthrough_nonsolid;
    // Streaming is a native-backend optimization; safe to enable always (CLI returns false).
    ctx.prefer_streaming = true;

    let out = conv.convert(backend, input, &ctx)?;
    persist_file(&out.path, output)?;
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ArchiveFormat;
    use crate::codec::OuterFormat;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    fn file_entry(path: &str) -> EntryMeta {
        EntryMeta {
            path: path.into(),
            size: 8,
            is_dir: false,
            format_hint: ArchiveFormat::Unknown,
            meta: Default::default(),
        }
    }

    /// In-memory backend for pipeline skip / bulk-fallback regressions.
    struct ScriptedBackend {
        entries: Vec<EntryMeta>,
        blobs: HashMap<String, Vec<u8>>,
        fail_extract: HashSet<String>,
        fail_bulk: bool,
        omit_from_bulk: HashSet<String>,
        bulk_calls: Mutex<usize>,
        member_calls: Mutex<Vec<String>>,
    }

    impl ScriptedBackend {
        fn new(pairs: &[(&str, &[u8])]) -> Self {
            let mut blobs = HashMap::new();
            let mut entries = Vec::new();
            for (path, data) in pairs {
                blobs.insert((*path).to_string(), data.to_vec());
                entries.push(file_entry(path));
            }
            Self {
                entries,
                blobs,
                fail_extract: HashSet::new(),
                fail_bulk: false,
                omit_from_bulk: HashSet::new(),
                bulk_calls: Mutex::new(0),
                member_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl ArchiveBackend for ScriptedBackend {
        fn format(&self) -> ArchiveFormat {
            ArchiveFormat::SevenZ
        }

        fn list(&self, _archive: &Path) -> Result<Vec<EntryMeta>> {
            Ok(self.entries.clone())
        }

        fn is_solid(&self, _archive: &Path) -> Result<bool> {
            Ok(true)
        }

        fn extract_member(&self, _archive: &Path, member: &str, dest_file: &Path) -> Result<()> {
            self.member_calls
                .lock()
                .unwrap()
                .push(member.to_string());
            if self.fail_extract.contains(member) {
                return Err(Error::Other(format!("scripted extract fail: {member}")));
            }
            let data = self
                .blobs
                .get(member)
                .ok_or_else(|| Error::EntryNotFound(member.to_string()))?;
            if let Some(p) = dest_file.parent() {
                fs::create_dir_all(p)?;
            }
            fs::write(dest_file, data)?;
            Ok(())
        }

        fn extract_members(&self, archive: &Path, members: &[&str], dest_dir: &Path) -> Result<()> {
            *self.bulk_calls.lock().unwrap() += 1;
            if self.fail_bulk {
                return Err(Error::Other("scripted bulk extract failed".into()));
            }
            fs::create_dir_all(dest_dir)?;
            for m in members {
                if self.omit_from_bulk.contains(*m) {
                    continue;
                }
                let dest = dest_dir.join(m);
                self.extract_member(archive, m, &dest)?;
            }
            Ok(())
        }

        fn extract_all(&self, _archive: &Path, _dest_dir: &Path) -> Result<()> {
            Err(Error::Other("extract_all unused in scripted tests".into()))
        }

        fn pack_dir(&self, _src: &Path, _dest: &Path, _opts: &PackOptions) -> Result<()> {
            Err(Error::Other("pack_dir unused in scripted tests".into()))
        }

        fn test(&self, _archive: &Path) -> Result<()> {
            Ok(())
        }
    }

    fn run_passthrough_job(backend: &ScriptedBackend, out: &Path) -> ConversionPlan {
        let input = out.parent().unwrap().join("dummy-input.7z");
        fs::write(&input, b"not-a-real-archive").unwrap();
        let mut opts = PipelineOptions::new(input, out.to_path_buf());
        opts.outer_format = OuterFormat::Dir;
        opts.verify = false;
        opts.solid_single_pass = true;
        opts.nested_concurrency = 1;
        opts.temp_dir = Some(out.parent().unwrap().join("tmp"));
        run(backend, &opts).expect("pipeline should succeed")
    }

    #[test]
    fn find_extracted_prefers_exact_path() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("nested/foo.7z");
        let b = dir.path().join("other/foo.7z");
        fs::create_dir_all(a.parent().unwrap()).unwrap();
        fs::create_dir_all(b.parent().unwrap()).unwrap();
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let got = find_extracted(dir.path(), "nested/foo.7z").unwrap();
        assert_eq!(got, a);
    }

    #[test]
    fn find_extracted_unique_basename_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let only = dir.path().join("other/bar.7z");
        fs::create_dir_all(only.parent().unwrap()).unwrap();
        fs::write(&only, b"x").unwrap();
        let got = find_extracted(dir.path(), "wanted/bar.7z").unwrap();
        assert_eq!(got, only);
    }

    #[test]
    fn find_extracted_ambiguous_basename_errors() {
        let dir = tempfile::tempdir().unwrap();
        for rel in ["a/foo.7z", "b/foo.7z"] {
            let p = dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"x").unwrap();
        }
        let err = find_extracted(dir.path(), "missing/foo.7z").unwrap_err();
        assert!(
            err.to_string().contains("ambiguous"),
            "{}",
            err
        );
    }

    #[test]
    fn find_extracted_rejects_unsafe_member() {
        let dir = tempfile::tempdir().unwrap();
        let err = find_extracted(dir.path(), "../evil").unwrap_err();
        assert!(err.to_string().contains("unsafe"), "{err}");
    }

    #[test]
    fn passthrough_extract_failure_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = ScriptedBackend::new(&[
            ("keep.txt", b"keep"),
            ("drop.txt", b"drop"),
            ("also.txt", b"also"),
        ]);
        backend.fail_extract.insert("drop.txt".into());
        backend.fail_bulk = true;
        let out = dir.path().join("out");
        let plan = run_passthrough_job(&backend, &out);
        assert_eq!(plan.runtime.passthrough_written, 2);
        assert_eq!(plan.runtime.passthrough_skipped, 1);
        assert!(out.join("keep.txt").is_file());
        assert!(out.join("also.txt").is_file());
        assert!(!out.join("drop.txt").exists());
        assert_eq!(*backend.bulk_calls.lock().unwrap(), 1);
        assert!(
            backend
                .member_calls
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "drop.txt")
        );
    }

    #[test]
    fn bulk_extract_failure_falls_back_to_per_member() {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = ScriptedBackend::new(&[("a.txt", b"aaa"), ("b.txt", b"bbb")]);
        backend.fail_bulk = true;
        let out = dir.path().join("out");
        let plan = run_passthrough_job(&backend, &out);
        assert_eq!(plan.runtime.passthrough_written, 2);
        assert_eq!(plan.runtime.passthrough_skipped, 0);
        assert_eq!(*backend.bulk_calls.lock().unwrap(), 1);
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"aaa");
        assert_eq!(fs::read(out.join("b.txt")).unwrap(), b"bbb");
    }

    #[test]
    fn missing_bulk_member_is_skipped_others_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = ScriptedBackend::new(&[("a.txt", b"aaa"), ("ghost.txt", b"g")]);
        backend.omit_from_bulk.insert("ghost.txt".into());
        let out = dir.path().join("out");
        let plan = run_passthrough_job(&backend, &out);
        assert_eq!(plan.runtime.passthrough_written, 1);
        assert_eq!(plan.runtime.passthrough_skipped, 1);
        assert!(out.join("a.txt").is_file());
        assert!(!out.join("ghost.txt").exists());
    }

    #[test]
    fn all_passthrough_failures_error_with_nothing_to_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = ScriptedBackend::new(&[("a.txt", b"a"), ("b.txt", b"b")]);
        backend.fail_extract.insert("a.txt".into());
        backend.fail_extract.insert("b.txt".into());
        backend.fail_bulk = true;
        let input = dir.path().join("in.7z");
        fs::write(&input, b"x").unwrap();
        let out = dir.path().join("out");
        let mut opts = PipelineOptions::new(input, out);
        opts.outer_format = OuterFormat::Dir;
        opts.temp_dir = Some(dir.path().join("tmp"));
        let err = run(&backend, &opts).unwrap_err();
        assert!(
            err.to_string().contains("nothing to write"),
            "{err}"
        );
    }
}
