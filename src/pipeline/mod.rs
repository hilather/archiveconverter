//! Orchestrates nested 7z conversion with size-aware concurrency and solid single-pass.

pub mod context;
pub mod plan;

pub use context::PipelineOptions;
pub use plan::{build_plan, ActionKind, ConversionPlan, PlanOptions};

use crate::archive::{ArchiveBackend, PackOptions};
use crate::convert::{ConvertContext, ConverterRegistry};
use crate::error::{Error, Result};
use crate::util::pathnorm::{is_safe_member_path, normalize_member_path};
use crate::util::size_parse::{can_admit_nested, resolve_nested_workers};
use crate::util::temp::{remove_dir_all_quiet, JobTemp};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
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

    let max_workers = resolve_nested_workers(opts.nested_concurrency, opts.pack.threads);
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
    if use_solid_pass {
        let t = Instant::now();
        tracing::info!(
            members = needed.len(),
            solid,
            max_workers,
            size_budget,
            "bulk extract of needed outer members (single-pass / pre-stage for concurrency)"
        );
        backend.extract_members(&opts.input, &needed, &outer_pass)?;
        log_stage(opts, "outer_bulk_extract", t.elapsed());
    }

    // Passthrough files first (cheap).
    for entry in &plan.entries {
        if entry.action != ActionKind::Passthrough {
            continue;
        }
        if !is_safe_member_path(&entry.dest_path) {
            return Err(Error::Other(format!(
                "unsafe destination path: {}",
                entry.dest_path
            )));
        }
        let dest = staging.join(&entry.dest_path);
        if use_solid_pass {
            let src = find_extracted(&outer_pass, &entry.source_path)?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dest)?;
            let _ = fs::remove_file(&src);
        } else {
            backend.extract_member(&opts.input, &entry.source_path, &dest)?;
        }
        tracing::info!(path = %entry.source_path, dest = %entry.dest_path, "passthrough outer member");
    }

    let nested: Vec<_> = plan
        .entries
        .iter()
        .filter(|e| e.action == ActionKind::ConvertNested)
        .collect();

    if !nested.is_empty() {
        convert_nested_size_aware(
            backend,
            &registry,
            opts,
            job.path(),
            &staging,
            use_solid_pass,
            &outer_pass,
            &nested,
            max_workers,
            size_budget,
        )?;
    }

    if use_solid_pass {
        remove_dir_all_quiet(&outer_pass);
    }

    if let Some(parent) = opts.output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let out_tmp = job.child("final-out.7z");
    let mut pack = opts.pack.clone();
    pack.non_solid = true;
    // Outer usually has few members → multi-thread pack is fine if user didn't pin.
    let t = Instant::now();
    backend.pack_dir(&staging, &out_tmp, &pack)?;
    log_stage(opts, "pack_outer", t.elapsed());

    if opts.verify {
        tracing::info!("verifying output archive");
        backend.test(&out_tmp)?;
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
        solid_single_pass = use_solid_pass,
        nested_workers = max_workers,
        nested_size_budget = size_budget,
        "conversion complete"
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
fn convert_nested_size_aware(
    backend: &dyn ArchiveBackend,
    registry: &ConverterRegistry,
    opts: &PipelineOptions,
    job_root: &Path,
    staging: &Path,
    use_solid_pass: bool,
    outer_pass: &Path,
    nested: &[&plan::PlannedEntry],
    max_workers: usize,
    size_budget: u64,
) -> Result<()> {
    // Materialize sources, then sort by packed size ascending.
    let mut jobs: Vec<NestedJob> = Vec::with_capacity(nested.len());
    for (index, entry) in nested.iter().enumerate() {
        let path = if use_solid_pass {
            find_extracted(outer_pass, &entry.source_path)?
        } else {
            let p = job_root.join(format!("pre-{index:04}.7z"));
            backend.extract_member(&opts.input, &entry.source_path, &p)?;
            p
        };
        // Prefer listing size; fall back to on-disk size after extract.
        let size = if entry.size > 0 {
            entry.size
        } else {
            fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
        };
        jobs.push(NestedJob {
            index,
            source_path: entry.source_path.clone(),
            dest_path: entry.dest_path.clone(),
            size,
            path,
        });
    }
    jobs.sort_by(|a, b| a.size.cmp(&b.size).then_with(|| a.index.cmp(&b.index)));

    let max_workers = max_workers.max(1).min(jobs.len().max(1));
    tracing::info!(
        nested = jobs.len(),
        max_workers,
        size_budget,
        smallest = jobs.first().map(|j| j.size).unwrap_or(0),
        largest = jobs.last().map(|j| j.size).unwrap_or(0),
        "nested convert schedule (smallest-first, size-aware)"
    );

    if max_workers == 1 {
        return convert_nested_serial_ordered(backend, registry, opts, job_root, staging, jobs);
    }

    convert_nested_parallel_budget(
        backend,
        registry,
        opts,
        job_root,
        staging,
        jobs,
        max_workers,
        size_budget,
    )
}

struct NestedJob {
    index: usize,
    source_path: String,
    dest_path: String,
    size: u64,
    path: PathBuf,
}

fn convert_nested_serial_ordered(
    backend: &dyn ArchiveBackend,
    registry: &ConverterRegistry,
    opts: &PipelineOptions,
    job_root: &Path,
    staging: &Path,
    jobs: Vec<NestedJob>,
) -> Result<()> {
    for job in jobs {
        tracing::info!(
            path = %job.source_path,
            dest = %job.dest_path,
            index = job.index,
            size = job.size,
            "converting nested 7z"
        );
        convert_one_nested_from_file(
            backend,
            registry,
            opts,
            job_root,
            job.index,
            &job.path,
            &job.dest_path,
            staging,
        )?;
        let _ = fs::remove_file(&job.path);
    }
    Ok(())
}

fn convert_nested_parallel_budget(
    backend: &dyn ArchiveBackend,
    registry: &ConverterRegistry,
    opts: &PipelineOptions,
    job_root: &Path,
    staging: &Path,
    jobs: Vec<NestedJob>,
    max_workers: usize,
    size_budget: u64,
) -> Result<()> {
    let total = jobs.len();
    let mut pending: VecDeque<NestedJob> = jobs.into();
    let opts = opts.clone();
    let job_root = job_root.to_path_buf();
    let staging = staging.to_path_buf();

    let (done_tx, done_rx) = mpsc::channel::<(u64, String, std::result::Result<(), String>)>();
    let mut running_count = 0usize;
    let mut running_sum = 0u64;
    let mut finished = 0usize;
    let mut errors: Vec<String> = Vec::new();

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
                let staging = &staging;
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
                        staging,
                    )
                    .map_err(|e| e.to_string());
                    let _ = fs::remove_file(&job.path);
                    let _ = done_tx.send((size, src_name, result));
                });
            }
            Ok(())
        };

        if let Err(e) = try_spawn(&mut pending, &mut running_count, &mut running_sum) {
            errors.push(e.to_string());
        }

        while finished < total && errors.is_empty() {
            if running_count == 0 {
                // Pending left but nothing admitted — force one (should be rare).
                if pending.is_empty() {
                    break;
                }
                if let Err(e) = try_spawn(&mut pending, &mut running_count, &mut running_sum) {
                    errors.push(e.to_string());
                    break;
                }
                if running_count == 0 {
                    errors.push("nested scheduler stalled with pending work".into());
                    break;
                }
            }

            match done_rx.recv() {
                Ok((size, src_name, result)) => {
                    running_sum = running_sum.saturating_sub(size);
                    running_count = running_count.saturating_sub(1);
                    finished += 1;
                    if let Err(e) = result {
                        errors.push(format!("{src_name}: {e}"));
                    } else if let Err(e) =
                        try_spawn(&mut pending, &mut running_count, &mut running_sum)
                    {
                        errors.push(e.to_string());
                    }
                }
                Err(_) => {
                    errors.push("nested worker channel closed early".into());
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
                if let Err(e) = result {
                    errors.push(format!("{src_name}: {e}"));
                }
            } else {
                break;
            }
        }
        drop(done_tx);
    });

    if !errors.is_empty() {
        return Err(Error::Other(format!(
            "size-aware nested convert failed: {}",
            errors.join("; ")
        )));
    }
    if finished != total {
        return Err(Error::Other(format!(
            "size-aware nested convert incomplete: finished {finished}/{total}"
        )));
    }
    Ok(())
}

fn find_extracted(root: &Path, member: &str) -> Result<PathBuf> {
    let norm = normalize_member_path(member);
    let direct = root.join(&norm);
    if direct.is_file() {
        return Ok(direct);
    }
    let base = norm.rsplit('/').next().unwrap_or(&norm);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file()
                && p.file_name().and_then(|s| s.to_str()) == Some(base)
            {
                return Ok(p);
            }
        }
    }
    Err(Error::EntryNotFound(member.to_string()))
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

fn convert_one_nested_from_file(
    backend: &dyn ArchiveBackend,
    registry: &ConverterRegistry,
    opts: &PipelineOptions,
    job_root: &Path,
    index: usize,
    inner_in: &Path,
    dest_member: &str,
    staging: &Path,
) -> Result<()> {
    let nested_root = job_root.join(format!("nested-{index:04}"));
    if nested_root.exists() {
        remove_dir_all_quiet(&nested_root);
    }
    fs::create_dir_all(&nested_root)?;

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

    if !is_safe_member_path(dest_member) {
        return Err(Error::Other(format!(
            "unsafe destination path: {dest_member}"
        )));
    }
    let dest = staging.join(dest_member);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    persist_file(&output.path, &dest)?;
    remove_dir_all_quiet(&nested_root);
    Ok(())
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
