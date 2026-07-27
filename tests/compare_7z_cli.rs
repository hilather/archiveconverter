//! Correctness + timing comparison: `archiveconverter` vs a manual 7z CLI pipeline.
//!
//! The manual path mirrors what a user would do by hand:
//!   1. `7z x` outer → staging
//!   2. for each nested `.7z`: extract → (optional excludes) → `7z a -ms=off`
//!   3. `7z a -ms=off` new outer from staged members
//!
//! We assert both paths produce the same member set and content hashes, and we
//! print wall-clock timings for benchmark comparison.

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

/// Manual 7z CLI conversion matching our tool's semantics for this fixture:
/// - exclude outer `skip_me.7z`
/// - rename `alpha_old.7z` → `alpha.7z`
/// - inside nested: drop `*.tmp` and `__MACOSX/**`
/// - all archives non-solid (`-ms=off`)
fn convert_manual_7z(
    outer: &Path,
    output: &Path,
    work: &Path,
    level: u32,
    threads: u32,
) -> Duration {
    let t0 = Instant::now();
    let bin = sevenz();
    let _ = bin;

    if work.exists() {
        fs::remove_dir_all(work).unwrap();
    }
    let outer_x = work.join("outer-x");
    let staging = work.join("staging");
    fs::create_dir_all(&outer_x).unwrap();
    fs::create_dir_all(&staging).unwrap();

    // 1. Extract outer
    run_7z(
        &[
            "x",
            "-y",
            &format!("-o{}", outer_x.display()),
            outer.to_str().unwrap(),
        ],
        None,
    );

    // 2. Process members
    for entry in fs::read_dir(&outer_x).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if name == "skip_me.7z" {
            continue;
        }
        if name.ends_with(".7z") {
            let dest_name = if name == "alpha_old.7z" {
                "alpha.7z".to_string()
            } else {
                name.clone()
            };
            let inner_work = work.join(format!("inner-{}", dest_name));
            let tree = inner_work.join("tree");
            fs::create_dir_all(&tree).unwrap();
            run_7z(
                &[
                    "x",
                    "-y",
                    &format!("-o{}", tree.display()),
                    path.to_str().unwrap(),
                ],
                None,
            );
            // Exclude like our regex filters: *.tmp and __MACOSX/**
            remove_matching(&tree, &|p: &Path| {
                let s = p.to_string_lossy().replace('\\', "/");
                s.ends_with(".tmp") || s.contains("/__MACOSX/") || s.contains("__MACOSX/")
            });
            let out_inner = staging.join(&dest_name);
            // Pack non-solid from tree contents
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
            let _ = fs::remove_dir_all(&inner_work);
        } else {
            // passthrough non-archive members
            fs::copy(&path, staging.join(&name)).unwrap();
        }
    }

    // 3. Pack non-solid outer
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
