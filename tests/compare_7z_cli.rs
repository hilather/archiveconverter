//! Correctness + timing comparison: `archiveconverter` vs a manual 7z CLI pipeline.
//!
//! Both paths use the **same disk model** (one nested archive at a time):
//!   1. List outer members
//!   2. For each kept outer member, in sequence:
//!      - extract **only that** member from the outer (`7z x … member`)
//!      - if nested `.7z`: extract → filter → pack `-ms=off` → drop its temp tree
//!      - if passthrough: copy into outer staging
//!      - delete the extracted outer member temp before the next member
//!   3. Pack staging as non-solid outer
//!
//! We assert both produce the same member set and content hashes, and print
//! wall-clock timings for comparison.

mod common;

use archiveconverter::archive::sevenz::{find_7z_binary, SevenZCli};
use archiveconverter::archive::ArchiveBackend;
use archiveconverter::filter::{MemberFilter, NameTransformer};
use archiveconverter::pipeline::{self, list_file_paths, PipelineOptions};
use common::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn sevenz() -> PathBuf {
    find_7z_binary().expect("7z/7zz required")
}

fn run_7z(args: &[&str], cwd: Option<&Path>) -> () {
    let bin = sevenz();
    let mut cmd = Command::new(&bin);
    cmd.args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let out = cmd.output().expect("spawn 7z");
    assert!(
        out.status.code().unwrap_or(2) <= 1,
        "7z {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Build a solid nested outer suitable for comparison tests (small, fast).
fn make_compare_fixture(root: &Path) -> PathBuf {
    ensure_7z();
    let inners = root.join("inners");
    fs::create_dir_all(&inners).unwrap();

    // Inner A (will be renamed by our tool)
    let a_tree = root.join("tree-a");
    write_file(&a_tree, "data/hello.txt", "hello-a\n");
    write_file(&a_tree, "data/keep.bin", "keep-a");
    write_file(&a_tree, "data/drop.tmp", "tmp-a");
    write_file(&a_tree, "__MACOSX/junk", "junk");
    write_file(&a_tree, "notes/readme.txt", "readme-a");
    pack_solid(&a_tree, &inners.join("alpha_old.7z"));

    // Inner B
    let b_tree = root.join("tree-b");
    write_file(&b_tree, "data/hello.txt", "hello-b\n");
    write_file(&b_tree, "data/keep.bin", "keep-b");
    write_file(&b_tree, "data/drop.tmp", "tmp-b");
    pack_solid(&b_tree, &inners.join("beta.7z"));

    // Outer solid
    let stage = root.join("outer-stage");
    fs::create_dir_all(&stage).unwrap();
    fs::copy(inners.join("alpha_old.7z"), stage.join("alpha_old.7z")).unwrap();
    fs::copy(inners.join("beta.7z"), stage.join("beta.7z")).unwrap();
    fs::copy(inners.join("beta.7z"), stage.join("skip_me.7z")).unwrap();
    write_file(&stage, "readme.txt", "outer readme\n");
    let outer = root.join("outer.7z");
    pack_solid(&stage, &outer);
    outer
}

/// Manual 7z CLI conversion with the **same one-at-a-time disk model** as the tool.
///
/// Semantics for this fixture:
/// - exclude outer `skip_me.7z`
/// - rename `alpha_old.7z` → `alpha.7z`
/// - inside nested: drop `*.tmp` and `__MACOSX/**`
/// - all archives non-solid (`-ms=off`)
///
/// Does **not** extract the whole outer up front (that would stage every nested
/// `.7z` at once and understate peak disk / unfairly favor a bulk script).
fn convert_manual_7z(
    outer: &Path,
    output: &Path,
    work: &Path,
    level: u32,
    threads: u32,
) -> Duration {
    let t0 = Instant::now();
    let backend = backend();

    if work.exists() {
        fs::remove_dir_all(work).unwrap();
    }
    let staging = work.join("staging");
    let one = work.join("one-member");
    fs::create_dir_all(&staging).unwrap();

    // List only (cheap metadata), then process members serially.
    let members: Vec<String> = backend
        .list(outer)
        .unwrap()
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.path)
        .collect();

    for name in members {
        if name == "skip_me.7z" {
            continue;
        }

        // Extract only this outer member (matches tool's extract_member).
        if one.exists() {
            fs::remove_dir_all(&one).unwrap();
        }
        fs::create_dir_all(&one).unwrap();
        run_7z(
            &[
                "x",
                "-y",
                &format!("-o{}", one.display()),
                outer.to_str().unwrap(),
                &name,
            ],
            None,
        );
        let extracted = find_file_under(&one, &name).expect("extracted outer member");

        if name.ends_with(".7z") {
            let dest_name = if name == "alpha_old.7z" {
                "alpha.7z".to_string()
            } else {
                name.rsplit('/').next().unwrap_or(&name).to_string()
            };
            let inner_work = work.join("inner-active");
            if inner_work.exists() {
                fs::remove_dir_all(&inner_work).unwrap();
            }
            let tree = inner_work.join("tree");
            fs::create_dir_all(&tree).unwrap();
            run_7z(
                &[
                    "x",
                    "-y",
                    &format!("-o{}", tree.display()),
                    extracted.to_str().unwrap(),
                ],
                None,
            );
            // Exclude like our regex filters: *.tmp and __MACOSX/**
            remove_matching(&tree, &|p: &Path| {
                let s = p.to_string_lossy().replace('\\', "/");
                s.ends_with(".tmp") || s.contains("/__MACOSX/") || s.contains("__MACOSX/")
            });
            let out_inner = staging.join(&dest_name);
            if out_inner.exists() {
                fs::remove_file(&out_inner).unwrap();
            }
            run_7z(
                &[
                    "a",
                    "-t7z",
                    &format!("-mx={level}"),
                    "-ms=off",
                    &format!("-mmt={threads}"),
                    "-y",
                    out_inner.to_str().unwrap(),
                    ".",
                ],
                Some(&tree),
            );
            // Free nested unpack before next outer member (one-at-a-time).
            fs::remove_dir_all(&inner_work).unwrap();
        } else {
            let base = name.rsplit('/').next().unwrap_or(&name);
            fs::copy(&extracted, staging.join(base)).unwrap();
        }

        // Free this outer member before the next (do not keep all nested .7z on disk).
        fs::remove_dir_all(&one).unwrap();
    }

    // Pack non-solid outer from staging only.
    if output.exists() {
        fs::remove_file(output).unwrap();
    }
    run_7z(
        &[
            "a",
            "-t7z",
            &format!("-mx={level}"),
            "-ms=off",
            &format!("-mmt={threads}"),
            "-y",
            output.to_str().unwrap(),
            ".",
        ],
        Some(&staging),
    );

    t0.elapsed()
}

fn find_file_under(root: &Path, member: &str) -> Option<PathBuf> {
    let want = member.replace('\\', "/");
    let base = want.rsplit('/').next().unwrap_or(&want);
    let direct = root.join(&want);
    if direct.is_file() {
        return Some(direct);
    }
    let mut files = Vec::new();
    walk_files(root, &mut files);
    files.into_iter().find(|p| {
        p.file_name()
            .and_then(|s| s.to_str())
            .map(|s| s == base)
            .unwrap_or(false)
    })
}

fn walk_files(root: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk_files(&p, out);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
}

fn remove_matching(root: &Path, pred: &dyn Fn(&Path) -> bool) {
    let mut files = Vec::new();
    walk_files(root, &mut files);
    for f in files {
        if pred(&f) {
            let _ = fs::remove_file(f);
        }
    }
}

/// Content hash of every regular file under `dir`, keyed by relative path with `/`.
fn tree_hashes(dir: &Path) -> BTreeMap<String, String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut map = BTreeMap::new();
    let mut files = Vec::new();
    walk_files(dir, &mut files);
    for path in files {
        let rel = path
            .strip_prefix(dir)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(&path).unwrap();
        let mut h = DefaultHasher::new();
        bytes.hash(&mut h);
        map.insert(rel, format!("{:016x}", h.finish()));
    }
    map
}

fn extract_all_to(backend: &SevenZCli, archive: &Path, dest: &Path) {
    if dest.exists() {
        fs::remove_dir_all(dest).unwrap();
    }
    fs::create_dir_all(dest).unwrap();
    backend.extract_all(archive, dest).unwrap();
}

/// Fully expand an outer (and nested .7z members one level) for content compare.
fn expand_nested_one_level(backend: &SevenZCli, archive: &Path, dest: &Path) {
    if dest.exists() {
        fs::remove_dir_all(dest).unwrap();
    }
    fs::create_dir_all(dest).unwrap();
    let outer_x = dest.join("_outer");
    backend.extract_all(archive, &outer_x).unwrap();
    for e in fs::read_dir(&outer_x).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if name.ends_with(".7z") && path.is_file() {
            let nest = dest.join(&name);
            fs::create_dir_all(&nest).unwrap();
            backend.extract_all(&path, &nest).unwrap();
        } else if path.is_file() {
            fs::copy(&path, dest.join(&name)).unwrap();
        }
    }
    let _ = fs::remove_dir_all(outer_x);
}

#[test]
fn tool_matches_manual_7z_cli_correctness() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_compare_fixture(root.path());
    let backend = backend();

    let tool_out = root.path().join("tool.7z");
    let mut opts = PipelineOptions::new(outer.clone(), tool_out.clone());
    opts.exclude_outer = MemberFilter::with_excludes([r"^skip_me\.7z$"]).unwrap();
    opts.exclude_inner = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
    opts.rename = NameTransformer::from_pairs([r"_old\.7z$=.7z"]).unwrap();
    opts.pack = default_pack();
    opts.pack.threads = Some(1);
    opts.pack.level = 1;
    opts.temp_dir = Some(root.path().join("tool-tmp"));

    let t_tool = Instant::now();
    pipeline::run(&backend, &opts).expect("tool convert");
    let tool_secs = t_tool.elapsed();

    let cli_out = root.path().join("cli.7z");
    let cli_secs = convert_manual_7z(
        &outer,
        &cli_out,
        &root.path().join("cli-work"),
        1,
        1,
    );

    // Outer member names
    let tool_paths: BTreeSet<_> = list_file_paths(&backend, &tool_out)
        .unwrap()
        .into_iter()
        .collect();
    let cli_paths: BTreeSet<_> = list_file_paths(&backend, &cli_out)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        tool_paths, cli_paths,
        "outer member names differ\ntool={tool_paths:?}\ncli={cli_paths:?}"
    );
    assert!(tool_paths.contains("alpha.7z"));
    assert!(tool_paths.contains("beta.7z"));
    assert!(tool_paths.contains("readme.txt"));
    assert!(!tool_paths.iter().any(|p| p.contains("skip_me")));
    assert!(!tool_paths.iter().any(|p| p.contains("alpha_old")));

    // Deep content equality (expand nested one level)
    let tool_tree = root.path().join("expand-tool");
    let cli_tree = root.path().join("expand-cli");
    expand_nested_one_level(&backend, &tool_out, &tool_tree);
    expand_nested_one_level(&backend, &cli_out, &cli_tree);
    let th = tree_hashes(&tool_tree);
    let ch = tree_hashes(&cli_tree);
    assert_eq!(
        th, ch,
        "expanded content hashes differ\ntool keys={:?}\ncli keys={:?}",
        th.keys().collect::<Vec<_>>(),
        ch.keys().collect::<Vec<_>>()
    );
    // Excludes applied inside nested
    assert!(
        !th.keys().any(|k| k.ends_with(".tmp")),
        "tmp should be gone: {:?}",
        th.keys()
    );
    assert!(
        !th.keys().any(|k| k.contains("__MACOSX")),
        "macosx should be gone: {:?}",
        th.keys()
    );

    println!("\n=== Correctness comparison (tool vs manual 7z CLI) ===");
    println!("outer members: {:?}", tool_paths);
    println!("expanded files: {}", th.len());
    println!(
        "tool wall: {:.3}s   manual 7z wall: {:.3}s   ratio tool/cli: {:.2}x",
        tool_secs.as_secs_f64(),
        cli_secs.as_secs_f64(),
        tool_secs.as_secs_f64() / cli_secs.as_secs_f64().max(1e-9)
    );
    println!("======================================================\n");
}

#[test]
fn benchmark_tool_vs_manual_7z_cli() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    // Slightly larger fixture for a more meaningful timing sample (still CI-friendly).
    let inners = root.path().join("inners");
    fs::create_dir_all(&inners).unwrap();
    for (name, tag) in [("alpha_old.7z", "a"), ("beta.7z", "b"), ("skip_me.7z", "s")] {
        let tree = root.path().join(format!("tree-{tag}"));
        for i in 0..400 {
            write_file(
                &tree,
                &format!("d{}/f{i:04}.txt", i % 20),
                &format!("id={i} tag={tag}\n{}", "line\n".repeat(20)),
            );
        }
        write_file(&tree, "data/drop.tmp", "tmp");
        write_file(&tree, "__MACOSX/x", "junk");
        pack_solid(&tree, &inners.join(name));
    }
    let stage = root.path().join("stage");
    fs::create_dir_all(&stage).unwrap();
    for name in ["alpha_old.7z", "beta.7z", "skip_me.7z"] {
        fs::copy(inners.join(name), stage.join(name)).unwrap();
    }
    write_file(&stage, "readme.txt", "readme\n");
    let outer = root.path().join("outer.7z");
    pack_solid(&stage, &outer);

    let backend = backend();
    let threads = 1u32;
    let level = 1u32;
    let repeats = 2u32;

    let mut tool_times = Vec::new();
    let mut cli_times = Vec::new();

    for r in 0..repeats {
        let tool_out = root.path().join(format!("tool-{r}.7z"));
        let mut opts = PipelineOptions::new(outer.clone(), tool_out);
        opts.exclude_outer = MemberFilter::with_excludes([r"^skip_me\.7z$"]).unwrap();
        opts.exclude_inner = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
        opts.rename = NameTransformer::from_pairs([r"_old\.7z$=.7z"]).unwrap();
        opts.pack.threads = Some(threads);
        opts.pack.level = level;
        opts.pack.non_solid = true;
        opts.temp_dir = Some(root.path().join(format!("tool-tmp-{r}")));

        let t0 = Instant::now();
        pipeline::run(&backend, &opts).unwrap();
        tool_times.push(t0.elapsed().as_secs_f64());

        let cli_out = root.path().join(format!("cli-{r}.7z"));
        let d = convert_manual_7z(
            &outer,
            &cli_out,
            &root.path().join(format!("cli-work-{r}")),
            level,
            threads,
        );
        cli_times.push(d.as_secs_f64());
    }

    tool_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    cli_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let tool_med = tool_times[tool_times.len() / 2];
    let cli_med = cli_times[cli_times.len() / 2];

    println!("\n=== Benchmark: archiveconverter vs manual 7z CLI ===");
    println!("fixture: 3 nested solid 7z × ~400 text files + filters/rename");
    println!("settings: level={level} threads={threads} repeats={repeats}");
    println!(
        "tool times: {:?}  median={:.3}s",
        tool_times, tool_med
    );
    println!(
        "cli  times: {:?}  median={:.3}s",
        cli_times, cli_med
    );
    println!(
        "ratio tool/cli: {:.2}x  ( <1 means tool faster )",
        tool_med / cli_med.max(1e-9)
    );
    println!("===================================================\n");

    // Soft expectation: tool should be within a reasonable factor of hand 7z
    // (same extract+repack work; overhead is orchestration). Fail only if wildly worse.
    assert!(
        tool_med < cli_med * 5.0 + 2.0,
        "tool is unreasonably slower than manual 7z: tool={tool_med:.3}s cli={cli_med:.3}s"
    );

    // Correctness spot-check on last outputs
    let tool_out = root.path().join(format!("tool-{}.7z", repeats - 1));
    let cli_out = root.path().join(format!("cli-{}.7z", repeats - 1));
    let tp: BTreeSet<_> = list_file_paths(&backend, &tool_out)
        .unwrap()
        .into_iter()
        .collect();
    let cp: BTreeSet<_> = list_file_paths(&backend, &cli_out)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(tp, cp);
}

#[test]
fn single_archive_tool_matches_manual_7z() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let solid = make_inner_solid(root.path(), "solid.7z");
    let backend = backend();

    // Tool
    let tool_out = root.path().join("tool.7z");
    let exclude = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
    let t0 = Instant::now();
    pipeline::convert_single(
        &backend,
        &solid,
        &tool_out,
        &exclude,
        &{
            let mut p = default_pack();
            p.threads = Some(1);
            p.level = 1;
            p
        },
        false,
        Some(&root.path().join("tmp")),
        false,
    )
    .unwrap();
    let tool_secs = t0.elapsed();

    // Manual: extract, delete excluded, pack -ms=off
    let t1 = Instant::now();
    let tree = root.path().join("manual-tree");
    backend.extract_all(&solid, &tree).unwrap();
    remove_matching(&tree, &|p| {
        let s = p.to_string_lossy().replace('\\', "/");
        s.ends_with(".tmp") || s.contains("__MACOSX")
    });
    let cli_out = root.path().join("cli.7z");
    run_7z(
        &[
            "a",
            "-t7z",
            "-mx=1",
            "-ms=off",
            "-mmt=1",
            "-y",
            cli_out.to_str().unwrap(),
            ".",
        ],
        Some(&tree),
    );
    let cli_secs = t1.elapsed();

    let tool_x = root.path().join("tx");
    let cli_x = root.path().join("cx");
    extract_all_to(&backend, &tool_out, &tool_x);
    extract_all_to(&backend, &cli_out, &cli_x);
    assert_eq!(tree_hashes(&tool_x), tree_hashes(&cli_x));

    println!(
        "single-archive tool={:.3}s  manual={:.3}s  ratio={:.2}x",
        tool_secs.as_secs_f64(),
        cli_secs.as_secs_f64(),
        tool_secs.as_secs_f64() / cli_secs.as_secs_f64().max(1e-9)
    );
}

/// Already non-solid nested should be copied without recompress when filters empty.
#[test]
fn passthrough_nonsolid_nested() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("t");
    write_file(&tree, "a.txt", "hello");
    let nonsolid = root.path().join("ns.7z");
    pack_nonsolid(&tree, &nonsolid);

    let stage = root.path().join("stage");
    fs::create_dir_all(&stage).unwrap();
    fs::copy(&nonsolid, stage.join("inner.7z")).unwrap();
    write_file(&stage, "readme.txt", "r\n");
    let outer = root.path().join("outer.7z");
    // Outer solid is fine; inner is non-solid.
    pack_solid(&stage, &outer);

    let backend = backend();
    assert!(!backend.is_solid(&nonsolid).unwrap());

    let out = root.path().join("out.7z");
    let mut opts = PipelineOptions::new(outer, out.clone());
    opts.pack.threads = Some(1);
    opts.pack.level = 1;
    opts.passthrough_nonsolid = true;
    opts.temp_dir = Some(root.path().join("tmp"));
    pipeline::run(&backend, &opts).unwrap();

    let inner_out = root.path().join("inner-out.7z");
    backend
        .extract_member(&out, "inner.7z", &inner_out)
        .unwrap();
    assert!(
        !backend.is_solid(&inner_out).unwrap(),
        "passthrough should remain non-solid"
    );
    // Content preserved
    let x = root.path().join("x");
    backend.extract_all(&inner_out, &x).unwrap();
    assert_eq!(fs::read_to_string(x.join("a.txt")).unwrap(), "hello");
}

/// Solid outer, many members: single-pass vs per-member extract on the outer only.
///
/// Nested conversion cost is the same; this isolates outer solid restart waste.
#[test]
fn solid_single_pass_matches_and_not_slower() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    // Several nested solid archives so outer solid restarts hurt without single-pass.
    let inners = root.path().join("inners");
    fs::create_dir_all(&inners).unwrap();
    let n_nested = 8usize;
    let files_each = 2_000u32;
    for i in 0..n_nested {
        let tree = root.path().join(format!("t{i}"));
        for f in 0..files_each {
            write_file(
                &tree,
                &format!("d{}/f{f:04}.txt", f % 20),
                &format!("n={i} f={f}\n{}", "line\n".repeat(8)),
            );
        }
        pack_solid(&tree, &inners.join(format!("nested-{i:02}.7z")));
    }
    let stage = root.path().join("stage");
    fs::create_dir_all(&stage).unwrap();
    for i in 0..n_nested {
        fs::copy(
            inners.join(format!("nested-{i:02}.7z")),
            stage.join(format!("nested-{i:02}.7z")),
        )
        .unwrap();
    }
    write_file(&stage, "readme.txt", "readme\n");
    let outer = root.path().join("outer.7z");
    pack_solid(&stage, &outer);

    let backend = backend();
    assert!(
        backend.is_solid(&outer).unwrap(),
        "fixture outer should be solid"
    );

    let mut base = PipelineOptions::new(outer.clone(), root.path().join("out-pass.7z"));
    base.pack.threads = Some(1);
    base.pack.level = 1;
    base.temp_dir = Some(root.path().join("tmp-pass"));
    base.solid_single_pass = true;

    let t0 = Instant::now();
    pipeline::run(&backend, &base).unwrap();
    let pass_secs = t0.elapsed().as_secs_f64();

    let mut nopass = base.clone();
    nopass.output = root.path().join("out-nopass.7z");
    nopass.temp_dir = Some(root.path().join("tmp-nopass"));
    nopass.solid_single_pass = false;

    let t1 = Instant::now();
    pipeline::run(&backend, &nopass).unwrap();
    let nopass_secs = t1.elapsed().as_secs_f64();

    let p_paths: BTreeSet<_> = list_file_paths(&backend, &base.output)
        .unwrap()
        .into_iter()
        .collect();
    let n_paths: BTreeSet<_> = list_file_paths(&backend, &nopass.output)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(p_paths, n_paths);
    assert_eq!(p_paths.len(), n_nested + 1); // nested + readme

    println!(
        "\n=== solid single-pass A/B (outer solid, {n_nested} nested × {files_each} files) ==="
    );
    println!("single-pass:     {pass_secs:.3}s");
    println!("per-member:      {nopass_secs:.3}s");
    println!(
        "speedup (nopass/pass): {:.2}x  (>1 means single-pass faster)",
        nopass_secs / pass_secs.max(1e-9)
    );
    println!("==============================================================\n");

    // Single-pass should not be meaningfully slower (allow 15% noise).
    assert!(
        pass_secs <= nopass_secs * 1.15 + 0.5,
        "solid single-pass slower than per-member: pass={pass_secs:.3} nopass={nopass_secs:.3}"
    );
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Build one solid nested archive with many small files (shared template for large bench).
fn make_large_inner_solid(root: &Path, files: u64) -> PathBuf {
    let tree = root.join("large-inner-tree");
    if tree.exists() {
        fs::remove_dir_all(&tree).unwrap();
    }
    let dirs = ((files / 100).clamp(1, 500)) as u64;
    for d in 0..dirs {
        fs::create_dir_all(tree.join(format!("d{d:04}"))).unwrap();
    }

    println!("large-bench: writing {files} files across {dirs} dirs...");
    let t0 = Instant::now();
    // Parallel writers (std only — no rayon in integration tests).
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(4)
        .min(16);
    let chunk = files.div_ceil(n_threads);
    std::thread::scope(|scope| {
        for t in 0..n_threads {
            let start = t * chunk;
            let end = ((t + 1) * chunk).min(files);
            if start >= end {
                continue;
            }
            let tree = &tree;
            scope.spawn(move || {
                for i in start..end {
                    let dir = i % dirs;
                    let path = tree
                        .join(format!("d{dir:04}"))
                        .join(format!("f{i:07}.txt"));
                    // ~300–400 bytes: many inodes, pack-bound like production-ish trees
                    let body = format!("id={i:07}\n{}", "lorem line for bench\n".repeat(12));
                    fs::write(path, body).unwrap();
                }
            });
        }
    });
    // A couple of filter targets present in every nested copy.
    write_file(&tree, "data/drop.tmp", "temporary");
    write_file(&tree, "__MACOSX/junk", "junk");
    println!(
        "large-bench: tree written in {:.1}s",
        t0.elapsed().as_secs_f64()
    );

    let solid = root.join("large-inner.7z");
    let t1 = Instant::now();
    pack_solid(&tree, &solid);
    println!(
        "large-bench: solid inner packed in {:.1}s ({} bytes)",
        t1.elapsed().as_secs_f64(),
        fs::metadata(&solid).unwrap().len()
    );
    fs::remove_dir_all(&tree).unwrap();
    solid
}

/// Outer solid archive containing `nested` copies of the same large solid inner + readme.
fn make_large_outer(root: &Path, files_per_nested: u64, nested: usize) -> PathBuf {
    let inner = make_large_inner_solid(root, files_per_nested);
    let stage = root.join("large-outer-stage");
    fs::create_dir_all(&stage).unwrap();
    for i in 0..nested {
        let name = if i == 0 {
            "alpha_old.7z".to_string()
        } else {
            format!("nested-{i:02}.7z")
        };
        fs::copy(&inner, stage.join(&name)).unwrap();
    }
    // One more nested that we exclude (still must list/skip fairly).
    fs::copy(&inner, stage.join("skip_me.7z")).unwrap();
    write_file(&stage, "readme.txt", "large outer readme\n");
    let outer = root.join("large-outer.7z");
    let t0 = Instant::now();
    pack_solid(&stage, &outer);
    println!(
        "large-bench: outer packed in {:.1}s with {} nested + skip_me ({} bytes)",
        t0.elapsed().as_secs_f64(),
        nested,
        fs::metadata(&outer).unwrap().len()
    );
    fs::remove_dir_all(&stage).unwrap();
    outer
}

/// Multi-minute comparison: tool vs manual 7z, **both one nested at a time**.
///
/// Defaults aim for ~2–4 minutes **per** path on a typical workstation (calibrated
/// ~6s / 30k files at -mx=1). Override with env:
///
/// - `LARGE_BENCH_FILES` (default 280000) — files per nested archive  
/// - `LARGE_BENCH_NESTED` (default 3) — nested archives to convert (excludes skip_me)  
/// - `LARGE_BENCH_MIN_SECS` (default 120) — fail if either path finishes too fast  
///
/// Run:
/// ```text
/// cargo test --release --test compare_7z_cli large_tool_vs_manual \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore = "multi-minute load; run: cargo test --release --test compare_7z_cli large_tool_vs_manual -- --ignored --nocapture"]
fn large_tool_vs_manual_one_at_a_time() {
    ensure_7z();

    let files = env_u64("LARGE_BENCH_FILES", 280_000);
    let nested = env_u64("LARGE_BENCH_NESTED", 3) as usize;
    let min_secs = env_f64("LARGE_BENCH_MIN_SECS", 120.0);
    let threads = 1u32;
    let level = 1u32;

    assert!(nested >= 1, "LARGE_BENCH_NESTED must be >= 1");
    assert!(files >= 1_000, "LARGE_BENCH_FILES too small for a large bench");

    // Prefer a stable work root so partial runs can be inspected; still unique.
    let root = tempfile::tempdir().unwrap();
    println!(
        "\n=== LARGE bench (one-at-a-time tool vs manual 7z) ===\n\
         files/nested={files}  nested={nested}  min_secs={min_secs}  level={level} threads={threads}\n\
         work={}\n",
        root.path().display()
    );

    let outer = make_large_outer(root.path(), files, nested);
    let backend = backend();

    // --- tool ---
    let tool_out = root.path().join("tool-large.7z");
    let mut opts = PipelineOptions::new(outer.clone(), tool_out.clone());
    opts.exclude_outer = MemberFilter::with_excludes([r"^skip_me\.7z$"]).unwrap();
    opts.exclude_inner = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
    opts.rename = NameTransformer::from_pairs([r"_old\.7z$=.7z"]).unwrap();
    opts.pack.threads = Some(threads);
    opts.pack.level = level;
    opts.pack.non_solid = true;
    opts.temp_dir = Some(root.path().join("tool-tmp"));

    println!("large-bench: starting TOOL convert...");
    let t_tool = Instant::now();
    pipeline::run(&backend, &opts).expect("tool large convert");
    let tool_secs = t_tool.elapsed().as_secs_f64();
    println!("large-bench: TOOL done in {tool_secs:.1}s");

    // --- manual 7z (same one-at-a-time model) ---
    let cli_out = root.path().join("cli-large.7z");
    println!("large-bench: starting MANUAL 7z convert...");
    let cli_secs = convert_manual_7z(
        &outer,
        &cli_out,
        &root.path().join("cli-work"),
        level,
        threads,
    )
    .as_secs_f64();
    println!("large-bench: MANUAL done in {cli_secs:.1}s");

    // Correctness: outer members only (full expand of 280k×3 is very expensive).
    let tool_paths: BTreeSet<_> = list_file_paths(&backend, &tool_out)
        .unwrap()
        .into_iter()
        .collect();
    let cli_paths: BTreeSet<_> = list_file_paths(&backend, &cli_out)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        tool_paths, cli_paths,
        "outer members differ\ntool={tool_paths:?}\ncli={cli_paths:?}"
    );
    assert!(tool_paths.contains("alpha.7z") || tool_paths.iter().any(|p| p.ends_with(".7z")));
    assert!(tool_paths.contains("readme.txt"));
    assert!(!tool_paths.iter().any(|p| p.contains("skip_me")));
    assert!(!tool_paths.iter().any(|p| p.contains("alpha_old")));

    // Spot-check one nested archive contents (first converted nested).
    let nested_name = tool_paths
        .iter()
        .find(|p| p.ends_with(".7z"))
        .cloned()
        .expect("expected a nested 7z in output");
    let tool_nested = root.path().join("spot-tool.7z");
    let cli_nested = root.path().join("spot-cli.7z");
    backend
        .extract_member(&tool_out, &nested_name, &tool_nested)
        .unwrap();
    backend
        .extract_member(&cli_out, &nested_name, &cli_nested)
        .unwrap();
    let tool_inner_paths: BTreeSet<_> = list_file_paths(&backend, &tool_nested)
        .unwrap()
        .into_iter()
        .collect();
    let cli_inner_paths: BTreeSet<_> = list_file_paths(&backend, &cli_nested)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        tool_inner_paths.len(),
        cli_inner_paths.len(),
        "nested member counts differ"
    );
    assert!(
        !tool_inner_paths.iter().any(|p| p.ends_with(".tmp")),
        "tmp should be excluded"
    );
    assert!(
        !tool_inner_paths.iter().any(|p| p.contains("__MACOSX")),
        "__MACOSX should be excluded"
    );

    let ratio = tool_secs / cli_secs.max(1e-9);
    println!("\n=== LARGE RESULTS (one-at-a-time) ===");
    println!("files/nested={files} nested_converted={nested}");
    println!("tool:   {tool_secs:.1}s");
    println!("manual: {cli_secs:.1}s");
    println!("ratio tool/cli: {ratio:.2}x  (<1 means tool faster)");
    println!("outer members: {tool_paths:?}");
    println!("nested {nested_name} file count: {}", tool_inner_paths.len());
    println!("====================================\n");

    assert!(
        tool_secs >= min_secs,
        "tool finished too quickly ({tool_secs:.1}s < {min_secs}s); raise LARGE_BENCH_FILES"
    );
    assert!(
        cli_secs >= min_secs,
        "manual finished too quickly ({cli_secs:.1}s < {min_secs}s); raise LARGE_BENCH_FILES"
    );
    assert!(
        tool_secs < cli_secs * 5.0 + 30.0,
        "tool unreasonably slower: tool={tool_secs:.1}s manual={cli_secs:.1}s"
    );
}
