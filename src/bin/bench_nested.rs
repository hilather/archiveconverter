//! Nested 7z conversion benchmarks.
//!
//! Scale ladder (per nested archive unless noted):
//!
//! | scale  | files  | size   | nested outers | typical use        |
//! |--------|--------|--------|---------------|--------------------|
//! | tiny   | 200    | 1 MiB  | 1,2           | seconds, CI-ish    |
//! | small  | 2_000  | 4 MiB  | 1,2,4         | ~tens of seconds   |
//! | quick  | 10_000 | 16 MiB | 1,2,4         | few minutes        |
//! | full   | 1e6    | 300 MiB| 1,2,4,10      | hours              |
//!
//! Usage:
//!   cargo run --release --bin bench_nested -- generate --scale tiny
//!   cargo run --release --bin bench_nested -- baseline-manual --scale tiny --threads 1,2,4
//!   cargo run --release --bin bench_nested -- run --scale tiny --threads 1,2,4
//!   cargo run --release --bin bench_nested -- all --scale small
//!   # aliases: --quick == --scale quick
//!
//! Manual baselines (`baseline-manual`) time a one-nested-at-a-time 7z CLI script
//! with the **same** `-mmt=N` as the tool's `--threads N`, and store them under
//! `benchdata/<scale>/results/manual_baseline.json`. Subsequent `run` prints
//! tool vs manual side-by-side without re-running the slow manual path.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ScaleName {
    /// ~200 files / 1 MiB — whole matrix in a few seconds
    Tiny,
    /// ~2k files / 4 MiB — good default for iteration
    Small,
    /// ~10k files / 16 MiB — still minutes, not hours
    Quick,
    /// ~1M files / 300 MiB — heavy production-shaped load
    Full,
}

#[derive(Debug, Parser)]
#[command(name = "bench_nested", about = "Nested 7z conversion benchmarks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Root directory for fixtures and results (default: ./benchdata)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Create solid nested benchmark archives
    Generate(GenArgs),
    /// Run conversion benchmarks (requires generate first)
    Run(RunArgs),
    /// Generate then run (single --scale applies to both)
    All(AllArgs),
    /// Time manual one-at-a-time 7z CLI and store baselines for side-by-side compare
    BaselineManual(BaselineArgs),
    /// Print built-in scale definitions
    Scales,
}

#[derive(Debug, Clone, clap::Args)]
struct AllArgs {
    /// Fixture scale: tiny | small | quick | full
    #[arg(long, value_enum, default_value_t = ScaleName::Small)]
    scale: ScaleName,

    /// Alias for `--scale quick`
    #[arg(long, conflicts_with = "scale")]
    quick: bool,

    /// Rebuild fixtures even if they already exist
    #[arg(long)]
    force: bool,

    /// Compression level when packing fixtures
    #[arg(long, default_value_t = 1)]
    pack_level: u32,

    /// Thread counts to measure
    #[arg(long, value_delimiter = ',', default_value = "1,2,4")]
    threads: Vec<u32>,

    /// Conversion compression level
    #[arg(long, default_value_t = 1)]
    level: u32,

    /// Nested counts override (default: scale preset)
    #[arg(long, value_delimiter = ',')]
    nested_counts: Option<Vec<usize>>,

    #[arg(long, default_value_t = 0)]
    warmup: u32,

    #[arg(long, default_value_t = 1)]
    repeats: u32,

    /// Also refresh manual 7z baselines before the tool run
    #[arg(long, default_value_t = false)]
    refresh_manual_baseline: bool,
}

#[derive(Debug, Clone, clap::Args)]
struct GenArgs {
    /// Fixture scale: tiny | small | quick | full
    #[arg(long, value_enum, default_value_t = ScaleName::Small)]
    scale: ScaleName,

    /// Alias for `--scale quick`
    #[arg(long, conflicts_with = "scale")]
    quick: bool,

    /// Uncompressed payload target per nested archive (MiB)
    #[arg(long)]
    size_mib: Option<u64>,

    /// Text files per nested archive
    #[arg(long)]
    files: Option<u64>,

    /// Nested archive counts to materialize as outer-N.7z
    #[arg(long, value_delimiter = ',')]
    nested_counts: Option<Vec<usize>>,

    /// Compression level when packing fixtures (lower = faster gen)
    #[arg(long, default_value_t = 1)]
    pack_level: u32,

    /// Rebuild fixtures even if they already exist
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Clone, clap::Args)]
struct RunArgs {
    /// Fixture scale (must match generate)
    #[arg(long, value_enum, default_value_t = ScaleName::Small)]
    scale: ScaleName,

    /// Alias for `--scale quick`
    #[arg(long, conflicts_with = "scale")]
    quick: bool,

    /// Thread counts to measure
    #[arg(long, value_delimiter = ',', default_value = "1,2,4")]
    threads: Vec<u32>,

    /// Nested counts to benchmark (outers must exist)
    #[arg(long, value_delimiter = ',')]
    nested_counts: Option<Vec<usize>>,

    /// Conversion compression level
    #[arg(long, default_value_t = 1)]
    level: u32,

    /// Warmup runs per cell (not recorded)
    #[arg(long, default_value_t = 0)]
    warmup: u32,

    /// Timed runs per cell (median reported)
    #[arg(long, default_value_t = 1)]
    repeats: u32,

    /// Skip loading/printing stored manual 7z baselines
    #[arg(long, default_value_t = false)]
    no_manual_compare: bool,
}

#[derive(Debug, Clone, clap::Args)]
struct BaselineArgs {
    /// Fixture scale (must match generate)
    #[arg(long, value_enum, default_value_t = ScaleName::Small)]
    scale: ScaleName,

    /// Alias for `--scale quick`
    #[arg(long, conflicts_with = "scale")]
    quick: bool,

    /// Thread counts for manual 7z `-mmt=N` (must match tool `--threads` cells)
    #[arg(long, value_delimiter = ',', default_value = "1,2,4")]
    threads: Vec<u32>,

    /// Nested counts to baseline (outers must exist)
    #[arg(long, value_delimiter = ',')]
    nested_counts: Option<Vec<usize>>,

    /// Compression level for pack (`-mx=N`), same as tool `--level`
    #[arg(long, default_value_t = 1)]
    level: u32,

    /// Timed runs per cell (median stored)
    #[arg(long, default_value_t = 1)]
    repeats: u32,

    /// Replace entire baseline file instead of merging missing/updated cells
    #[arg(long, default_value_t = false)]
    replace: bool,
}

#[derive(Debug, Clone)]
struct Scale {
    name: &'static str,
    size_mib: u64,
    files: u64,
    nested_counts: Vec<usize>,
}

impl Scale {
    fn from_name(name: ScaleName) -> Self {
        match name {
            ScaleName::Tiny => Self {
                name: "tiny",
                size_mib: 1,
                files: 200,
                nested_counts: vec![1, 2],
            },
            ScaleName::Small => Self {
                name: "small",
                size_mib: 4,
                files: 2_000,
                nested_counts: vec![1, 2, 4],
            },
            ScaleName::Quick => Self {
                name: "quick",
                size_mib: 16,
                files: 10_000,
                nested_counts: vec![1, 2, 4],
            },
            ScaleName::Full => Self {
                name: "full",
                size_mib: 300,
                files: 1_000_000,
                nested_counts: vec![1, 2, 4, 10],
            },
        }
    }

    fn apply_overrides(&mut self, gen: &GenArgs) {
        if let Some(s) = gen.size_mib {
            self.size_mib = s;
        }
        if let Some(f) = gen.files {
            self.files = f;
        }
        if let Some(ref c) = gen.nested_counts {
            self.nested_counts = c.clone();
        }
    }
}

fn resolve_scale_name(scale: ScaleName, quick_flag: bool) -> ScaleName {
    if quick_flag {
        ScaleName::Quick
    } else {
        scale
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let data = cli
        .data_dir
        .unwrap_or_else(|| PathBuf::from("benchdata"));

    match cli.cmd {
        Cmd::Scales => print_scales(),
        Cmd::Generate(g) => generate(&data, &g)?,
        Cmd::Run(r) => run_bench(&data, &r)?,
        Cmd::BaselineManual(b) => run_baseline_manual(&data, &b)?,
        Cmd::All(a) => {
            let scale = resolve_scale_name(a.scale, a.quick);
            let gen = GenArgs {
                scale,
                quick: false,
                size_mib: None,
                files: None,
                nested_counts: a.nested_counts.clone(),
                pack_level: a.pack_level,
                force: a.force,
            };
            let run = RunArgs {
                scale,
                quick: false,
                threads: a.threads.clone(),
                nested_counts: a.nested_counts.clone(),
                level: a.level,
                warmup: a.warmup,
                repeats: a.repeats,
                no_manual_compare: false,
            };
            generate(&data, &gen)?;
            if a.refresh_manual_baseline {
                let b = BaselineArgs {
                    scale,
                    quick: false,
                    threads: a.threads,
                    nested_counts: a.nested_counts,
                    level: a.level,
                    repeats: a.repeats,
                    replace: false,
                };
                run_baseline_manual(&data, &b)?;
            }
            run_bench(&data, &run)?;
        }
    }
    Ok(())
}

fn print_scales() {
    println!("Available scales:\n");
    println!(
        "{:<8} {:>10} {:>8} {:>16}  {}",
        "name", "files", "MiB", "nested outers", "intent"
    );
    for (name, intent) in [
        (ScaleName::Tiny, "seconds; smoke / CI"),
        (ScaleName::Small, "default iteration"),
        (ScaleName::Quick, "minutes; denser file count"),
        (ScaleName::Full, "hours; production-shaped"),
    ] {
        let s = Scale::from_name(name);
        println!(
            "{:<8} {:>10} {:>8} {:>16}  {}",
            s.name,
            s.files,
            s.size_mib,
            format!("{:?}", s.nested_counts),
            intent
        );
    }
    println!("\nExamples:");
    println!("  bench_nested generate --scale tiny");
    println!("  bench_nested baseline-manual --scale tiny --threads 1,2,4");
    println!("  bench_nested all --scale small --threads 1,2,4");
    println!("  bench_nested run --scale full --threads 1,2,3,4");
    println!();
    println!("Manual baselines: time one-nested-at-a-time 7z with matching -mmt=N,");
    println!("store under benchdata/<scale>/results/manual_baseline.json, then");
    println!("`run` shows tool vs manual side-by-side at the same thread count.");
}

fn scale_from(gen: &GenArgs) -> Scale {
    let mut s = Scale::from_name(resolve_scale_name(gen.scale, gen.quick));
    s.apply_overrides(gen);
    s
}

fn scale_dir(data: &Path, scale_name: &str) -> PathBuf {
    data.join(scale_name)
}

fn find_7z() -> Result<PathBuf> {
    for name in ["7zz", "7z", "7za"] {
        if let Ok(p) = which::which(name) {
            return Ok(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let p = PathBuf::from(home).join(".local/bin/7zz");
    if p.is_file() {
        return Ok(p);
    }
    bail!("7z/7zz not found on PATH");
}

fn archiveconverter_bin() -> Result<PathBuf> {
    // Prefer freshly built release binary next to this exe, else cargo-run style path
    let exe = std::env::current_exe()?;
    if let Some(dir) = exe.parent() {
        let candidate = dir.join("archiveconverter");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // Fallback: target/release or target/debug from CARGO_MANIFEST_DIR
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for profile in ["release", "debug"] {
        let p = manifest.join("target").join(profile).join("archiveconverter");
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("archiveconverter binary not found; build with cargo build --release");
}

fn generate(data: &Path, gen: &GenArgs) -> Result<()> {
    let scale = scale_from(gen);
    let root = scale_dir(data, scale.name);
    fs::create_dir_all(&root)?;

    println!("=== Fixture generation ({}) ===", scale.name);
    println!(
        "target: {} MiB uncompressed, {} files per nested, outers: {:?}",
        scale.size_mib, scale.files, scale.nested_counts
    );
    println!("root: {}", root.display());

    let sevenz = find_7z()?;
    println!("7z: {}", sevenz.display());

    let inner = root.join("inner-solid.7z");
    if inner.is_file() && !gen.force {
        println!("reusing existing {}", inner.display());
    } else {
        if gen.force && inner.exists() {
            fs::remove_file(&inner)?;
        }
        let tree = root.join("inner-tree");
        if tree.exists() {
            fs::remove_dir_all(&tree)?;
        }
        fs::create_dir_all(&tree)?;

        let t0 = Instant::now();
        write_file_tree(&tree, scale.files, scale.size_mib)?;
        println!(
            "wrote file tree in {:.1}s",
            t0.elapsed().as_secs_f64()
        );

        let t1 = Instant::now();
        pack_solid(&sevenz, &tree, &inner, gen.pack_level)?;
        println!(
            "packed solid inner in {:.1}s → {} ({} bytes)",
            t1.elapsed().as_secs_f64(),
            inner.display(),
            fs::metadata(&inner)?.len()
        );

        // Free the unpacked tree after packing
        println!("removing unpacked tree to free disk...");
        fs::remove_dir_all(&tree)?;
    }

    let inner_bytes = fs::metadata(&inner)?.len();
    for n in &scale.nested_counts {
        let outer = root.join(format!("outer-{n}.7z"));
        if outer.is_file() && !gen.force {
            println!("reusing {}", outer.display());
            continue;
        }
        if gen.force && outer.exists() {
            fs::remove_file(&outer)?;
        }
        let stage = root.join(format!("outer-{n}-stage"));
        if stage.exists() {
            fs::remove_dir_all(&stage)?;
        }
        fs::create_dir_all(&stage)?;
        for i in 0..*n {
            let name = format!("nested-{i:02}.7z");
            fs::copy(&inner, stage.join(&name))?;
        }
        fs::write(
            stage.join("README.txt"),
            format!("bench outer with {n} nested solid archives\n"),
        )?;

        let t0 = Instant::now();
        pack_solid(&sevenz, &stage, &outer, gen.pack_level)?;
        println!(
            "packed outer-{n} in {:.1}s → {} bytes ({}×{} + readme)",
            t0.elapsed().as_secs_f64(),
            fs::metadata(&outer)?.len(),
            n,
            inner_bytes
        );
        fs::remove_dir_all(&stage)?;
    }

    // Marker for scale metadata
    fs::write(
        root.join("SCALE.txt"),
        format!(
            "name={}\nsize_mib={}\nfiles={}\nnested_counts={:?}\ninner_bytes={}\n",
            scale.name, scale.size_mib, scale.files, scale.nested_counts, inner_bytes
        ),
    )?;

    println!("generate complete.");
    Ok(())
}

/// Create `files` small text files totaling approximately `size_mib` MiB.
fn write_file_tree(tree: &Path, files: u64, size_mib: u64) -> Result<()> {
    let target_bytes = size_mib * 1024 * 1024;
    let per_file = (target_bytes / files.max(1)).max(32) as usize;

    // Shard into enough dirs that no folder has huge entries, without creating
    // 1000 empty dirs for tiny fixtures.
    let dirs = ((files / 50).clamp(1, 1000)) as u64;
    for d in 0..dirs {
        fs::create_dir_all(tree.join(format!("d{d:04}")))?;
    }

    println!("creating {files} files (~{per_file} bytes each, ~{size_mib} MiB, {dirs} dirs)...");

    (0..files).into_par_iter().try_for_each(|i| -> Result<()> {
        let dir = i % dirs;
        let path = tree
            .join(format!("d{dir:04}"))
            .join(format!("f{i:07}.txt"));
        // Deterministic compressible text with a unique prefix
        let mut body = String::with_capacity(per_file);
        body.push_str(&format!("id={i:07}\n"));
        let pad = "lorem ipsum dolor sit amet benchmark line\n";
        while body.len() + pad.len() <= per_file {
            body.push_str(pad);
        }
        while body.len() < per_file {
            body.push('x');
        }
        let f = File::create(&path)
            .with_context(|| format!("create {}", path.display()))?;
        let mut w = BufWriter::new(f);
        w.write_all(body.as_bytes())?;
        w.flush()?;
        Ok(())
    })?;

    Ok(())
}

fn pack_solid(sevenz: &Path, src_dir: &Path, dest: &Path, level: u32) -> Result<()> {
    let dest = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        std::env::current_dir()?.join(dest)
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        fs::remove_file(&dest)?;
    }
    let mx = format!("-mx={level}");
    let dest_s = dest.to_string_lossy().into_owned();
    let status = Command::new(sevenz)
        .args([
            "a",
            "-t7z",
            mx.as_str(),
            "-ms=on",
            "-mmt=on",
            "-y",
            dest_s.as_str(),
            ".",
        ])
        .current_dir(src_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("spawn 7z pack ({})", sevenz.display()))?;
    if !status.success() {
        bail!("7z pack failed for {} (status {status})", dest.display());
    }
    if !dest.is_file() {
        bail!("7z pack reported success but {} missing", dest.display());
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct BenchRow {
    scale: String,
    nested: usize,
    threads: u32,
    seconds: f64,
    input_bytes: u64,
    output_bytes: u64,
}

/// Stored manual 7z CLI baseline (one nested archive at a time, matching `-mmt=N`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManualBaselineFile {
    version: u32,
    scale: String,
    /// How the baseline was measured.
    method: String,
    notes: String,
    updated_unix: u64,
    entries: Vec<ManualBaselineEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManualBaselineEntry {
    nested: usize,
    threads: u32,
    level: u32,
    seconds: f64,
    output_bytes: u64,
    repeats: u32,
}

fn baseline_path(out_dir: &Path) -> PathBuf {
    out_dir.join("manual_baseline.json")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_manual_baseline(path: &Path) -> Result<Option<ManualBaselineFile>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let file: ManualBaselineFile = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(file))
}

fn save_manual_baseline(path: &Path, file: &ManualBaselineFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(file)?;
    fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn lookup_manual(
    base: Option<&ManualBaselineFile>,
    nested: usize,
    threads: u32,
    level: u32,
) -> Option<&ManualBaselineEntry> {
    base?.entries
        .iter()
        .find(|e| e.nested == nested && e.threads == threads && e.level == level)
}

fn merge_baseline_entries(
    existing: Option<ManualBaselineFile>,
    scale: &str,
    new_entries: Vec<ManualBaselineEntry>,
    replace: bool,
) -> ManualBaselineFile {
    let mut file = if replace {
        ManualBaselineFile {
            version: 1,
            scale: scale.into(),
            method: "manual-7z-one-at-a-time".into(),
            notes: "Outer members extracted one-by-one; each nested solid→non-solid with -ms=off -mmt=N; outer pack -ms=off -mmt=N. Thread N matches tool --threads N.".into(),
            updated_unix: now_unix(),
            entries: Vec::new(),
        }
    } else if let Some(mut e) = existing {
        e.updated_unix = now_unix();
        e
    } else {
        ManualBaselineFile {
            version: 1,
            scale: scale.into(),
            method: "manual-7z-one-at-a-time".into(),
            notes: "Outer members extracted one-by-one; each nested solid→non-solid with -ms=off -mmt=N; outer pack -ms=off -mmt=N. Thread N matches tool --threads N.".into(),
            updated_unix: now_unix(),
            entries: Vec::new(),
        }
    };

    for entry in new_entries {
        if let Some(slot) = file
            .entries
            .iter_mut()
            .find(|e| e.nested == entry.nested && e.threads == entry.threads && e.level == entry.level)
        {
            *slot = entry;
        } else {
            file.entries.push(entry);
        }
    }
    file.entries
        .sort_by(|a, b| (a.nested, a.threads, a.level).cmp(&(b.nested, b.threads, b.level)));
    file
}

fn run_baseline_manual(data: &Path, args: &BaselineArgs) -> Result<()> {
    let scale = Scale::from_name(resolve_scale_name(args.scale, args.quick));
    let scale_name = scale.name;
    let root = scale_dir(data, scale_name);
    if !root.is_dir() {
        bail!(
            "missing fixtures at {} — run: bench_nested generate --scale {}",
            root.display(),
            scale_name
        );
    }

    let nested_counts = args
        .nested_counts
        .clone()
        .unwrap_or_else(|| scale.nested_counts.clone());

    let sevenz = find_7z()?;
    let out_dir = root.join("results");
    fs::create_dir_all(&out_dir)?;
    let work_root = root.join("tmp").join("manual-baseline");
    fs::create_dir_all(&work_root)?;

    let path = baseline_path(&out_dir);
    let existing = if args.replace {
        None
    } else {
        load_manual_baseline(&path)?
    };

    println!("=== Manual 7z baseline ({scale_name}) ===");
    println!("7z: {}", sevenz.display());
    println!("threads (-mmt=N): {:?}", args.threads);
    println!("nested: {nested_counts:?}");
    println!("level: {}", args.level);
    println!("store: {}", path.display());
    println!("method: one nested at a time (serial outer members)");

    let mut new_entries = Vec::new();

    for &n in &nested_counts {
        let input = root.join(format!("outer-{n}.7z"));
        if !input.is_file() {
            bail!("missing {} — re-run generate", input.display());
        }

        for &threads in &args.threads {
            let mut times = Vec::new();
            let mut output_bytes = 0u64;
            for r in 0..args.repeats.max(1) {
                let output = out_dir.join(format!("manual-n{n}-t{threads}-r{r}.7z"));
                if output.exists() {
                    fs::remove_file(&output)?;
                }
                let work = work_root.join(format!("n{n}-t{threads}-r{r}"));
                if work.exists() {
                    fs::remove_dir_all(&work)?;
                }
                println!(
                    "manual nested={n} threads={threads} (-mmt={threads}) rep={r} ..."
                );
                let elapsed = convert_manual_7z(
                    &sevenz,
                    &input,
                    &output,
                    &work,
                    args.level,
                    threads,
                )?;
                output_bytes = fs::metadata(&output)?.len();
                println!(
                    "  → {:.2}s  out={} bytes",
                    elapsed.as_secs_f64(),
                    output_bytes
                );
                times.push(elapsed.as_secs_f64());
                // Drop work + intermediate outputs; keep last archive for spot checks.
                let _ = fs::remove_dir_all(&work);
                if r + 1 < args.repeats {
                    let _ = fs::remove_file(&output);
                }
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = times[times.len() / 2];
            new_entries.push(ManualBaselineEntry {
                nested: n,
                threads,
                level: args.level,
                seconds: median,
                output_bytes,
                repeats: args.repeats.max(1),
            });
        }
    }

    let file = merge_baseline_entries(existing, scale_name, new_entries, args.replace);
    save_manual_baseline(&path, &file)?;
    println!("wrote {}", path.display());
    println!();
    println!("Stored cells:");
    for e in &file.entries {
        println!(
            "  nested={:<3} threads={:<3} level={}  {:.2}s",
            e.nested, e.threads, e.level, e.seconds
        );
    }
    Ok(())
}

fn abs_path(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

/// Manual one-at-a-time 7z CLI conversion with matching `-mmt={threads}`.
///
/// Mirrors the fair "script" baseline: list outer → for each member extract only
/// that member → if nested `.7z` extract/pack non-solid with `-mmt=N` → free temps
/// → pack non-solid outer with `-mmt=N`. No concurrent nests.
fn convert_manual_7z(
    sevenz: &Path,
    outer: &Path,
    output: &Path,
    work: &Path,
    level: u32,
    threads: u32,
) -> Result<Duration> {
    let t0 = Instant::now();
    let outer = abs_path(outer)?;
    let output = abs_path(output)?;
    let work = abs_path(work)?;

    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    let staging = work.join("staging");
    let one = work.join("one-member");
    fs::create_dir_all(&staging)?;

    let members = list_7z_files(sevenz, &outer)?;
    for name in members {
        if one.exists() {
            fs::remove_dir_all(&one)?;
        }
        fs::create_dir_all(&one)?;

        let one_s = one.to_string_lossy().into_owned();
        let outer_s = outer.to_string_lossy().into_owned();
        run_7z(
            sevenz,
            &["x", "-y", &format!("-o{one_s}"), &outer_s, &name],
            None,
        )?;

        let extracted = find_file_under(&one, &name).with_context(|| {
            format!("extracted outer member {name} not found under {}", one.display())
        })?;

        if name.ends_with(".7z") {
            let dest_name = name.rsplit('/').next().unwrap_or(&name).to_string();
            let inner_work = work.join("inner-active");
            if inner_work.exists() {
                fs::remove_dir_all(&inner_work)?;
            }
            let tree = inner_work.join("tree");
            fs::create_dir_all(&tree)?;
            let tree_s = tree.to_string_lossy().into_owned();
            let extracted_s = extracted.to_string_lossy().into_owned();
            run_7z(
                sevenz,
                &["x", "-y", &format!("-o{tree_s}"), &extracted_s],
                None,
            )?;
            let out_inner = staging.join(&dest_name);
            if out_inner.exists() {
                fs::remove_file(&out_inner)?;
            }
            let out_inner_s = out_inner.to_string_lossy().into_owned();
            let mx = format!("-mx={level}");
            let mmt = format!("-mmt={threads}");
            run_7z(
                sevenz,
                &[
                    "a",
                    "-t7z",
                    mx.as_str(),
                    "-ms=off",
                    mmt.as_str(),
                    "-y",
                    out_inner_s.as_str(),
                    ".",
                ],
                Some(&tree),
            )?;
            fs::remove_dir_all(&inner_work)?;
        } else {
            let base = name.rsplit('/').next().unwrap_or(&name);
            fs::copy(&extracted, staging.join(base))?;
        }

        fs::remove_dir_all(&one)?;
    }

    if output.exists() {
        fs::remove_file(&output)?;
    }
    let output_s = output.to_string_lossy().into_owned();
    let mx = format!("-mx={level}");
    let mmt = format!("-mmt={threads}");
    run_7z(
        sevenz,
        &[
            "a",
            "-t7z",
            mx.as_str(),
            "-ms=off",
            mmt.as_str(),
            "-y",
            output_s.as_str(),
            ".",
        ],
        Some(&staging),
    )?;

    Ok(t0.elapsed())
}

fn run_7z(sevenz: &Path, args: &[&str], cwd: Option<&Path>) -> Result<()> {
    let mut cmd = Command::new(sevenz);
    cmd.args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let out = cmd
        .output()
        .with_context(|| format!("spawn 7z ({})", sevenz.display()))?;
    // 7z exit 0 = ok, 1 = warning (often ok)
    let code = out.status.code().unwrap_or(2);
    if code > 1 {
        bail!(
            "7z {:?} failed (code {code}): {}{}",
            args,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
    }
    Ok(())
}

/// List non-directory file paths inside a 7z via `7z l -slt`.
///
/// 7zz technical listing: archive header block has `Type = 7z` and no
/// `Attributes`; members have `Size` / `Attributes` (and sometimes `Folder`).
fn list_7z_files(sevenz: &Path, archive: &Path) -> Result<Vec<String>> {
    let out = Command::new(sevenz)
        .args(["l", "-slt", archive.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("list {}", archive.display()))?;
    if !out.status.success() && out.status.code().unwrap_or(2) > 1 {
        bail!(
            "7z list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut paths = Vec::new();
    let mut current_path: Option<String> = None;
    let mut is_dir = false;
    let mut is_archive_header = false;
    let mut is_member = false; // saw Size or Attributes

    let flush = |paths: &mut Vec<String>,
                 current_path: &mut Option<String>,
                 is_dir: bool,
                 is_archive_header: bool,
                 is_member: bool| {
        if let Some(p) = current_path.take() {
            if !is_archive_header && is_member && !is_dir && !p.is_empty() {
                paths.push(p);
            }
        }
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Path = ") {
            flush(
                &mut paths,
                &mut current_path,
                is_dir,
                is_archive_header,
                is_member,
            );
            current_path = Some(rest.to_string());
            is_dir = false;
            is_archive_header = false;
            is_member = false;
        } else if line.starts_with("Type = ") {
            let t = line.trim_start_matches("Type = ").trim();
            if t.eq_ignore_ascii_case("7z") || t.eq_ignore_ascii_case("zip") {
                is_archive_header = true;
            }
        } else if line.starts_with("Folder = ") {
            let v = line.trim_start_matches("Folder = ").trim();
            is_dir = v == "+" || v.eq_ignore_ascii_case("true");
            is_member = true;
        } else if line.starts_with("Attributes = ") {
            let v = line.trim_start_matches("Attributes = ");
            // e.g. "A -rw-rw-r--" or "D_ ..."
            is_dir = v.contains('D');
            is_member = true;
        } else if line.starts_with("Size = ") {
            is_member = true;
        } else if line.is_empty() {
            flush(
                &mut paths,
                &mut current_path,
                is_dir,
                is_archive_header,
                is_member,
            );
            is_dir = false;
            is_archive_header = false;
            is_member = false;
        }
    }
    flush(
        &mut paths,
        &mut current_path,
        is_dir,
        is_archive_header,
        is_member,
    );
    if paths.is_empty() {
        bail!("no file members listed in {}", archive.display());
    }
    Ok(paths)
}

fn find_file_under(root: &Path, member: &str) -> Option<PathBuf> {
    let want = member.replace('\\', "/");
    let direct = root.join(&want);
    if direct.is_file() {
        return Some(direct);
    }
    // Basename fallback (7z may strip path components depending on flags)
    let base = want.rsplit('/').next().unwrap_or(&want);
    let walk = walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok());
    for entry in walk {
        if entry.file_type().is_file() {
            let p = entry.path();
            if p.file_name().and_then(|s| s.to_str()) == Some(base) {
                return Some(p.to_path_buf());
            }
        }
    }
    None
}

fn run_bench(data: &Path, run: &RunArgs) -> Result<()> {
    let scale = Scale::from_name(resolve_scale_name(run.scale, run.quick));
    let scale_name = scale.name;
    let root = scale_dir(data, scale_name);
    if !root.is_dir() {
        bail!(
            "missing fixtures at {} — run: bench_nested generate --scale {}",
            root.display(),
            scale_name
        );
    }

    let nested_counts = run
        .nested_counts
        .clone()
        .unwrap_or_else(|| scale.nested_counts.clone());

    let ac = archiveconverter_bin()?;
    let out_dir = root.join("results");
    fs::create_dir_all(&out_dir)?;
    let temp_parent = root.join("tmp");
    fs::create_dir_all(&temp_parent)?;

    let manual = if run.no_manual_compare {
        None
    } else {
        load_manual_baseline(&baseline_path(&out_dir))?
    };

    println!("=== Benchmark run ({scale_name}) ===");
    println!("archiveconverter: {}", ac.display());
    println!("threads: {:?}", run.threads);
    println!("nested: {nested_counts:?}");
    println!("level: {}", run.level);
    match &manual {
        Some(m) => println!(
            "manual baseline: {} ({} cells)",
            baseline_path(&out_dir).display(),
            m.entries.len()
        ),
        None if !run.no_manual_compare => {
            println!(
                "manual baseline: none — run `bench_nested baseline-manual --scale {scale_name}` for side-by-side"
            );
        }
        None => {}
    }

    let mut rows = Vec::new();

    for &n in &nested_counts {
        let input = root.join(format!("outer-{n}.7z"));
        if !input.is_file() {
            bail!("missing {} — re-run generate", input.display());
        }
        let input_bytes = fs::metadata(&input)?.len();

        for &threads in &run.threads {
            for w in 0..run.warmup {
                println!("warmup nested={n} threads={threads} #{w}");
                let output = out_dir.join(format!("warmup-n{n}-t{threads}.7z"));
                let _ = run_convert(&ac, &input, &output, threads, run.level, &temp_parent)?;
                let _ = fs::remove_file(&output);
            }

            let mut times = Vec::new();
            let mut output_bytes = 0u64;
            for r in 0..run.repeats.max(1) {
                let output = out_dir.join(format!("out-n{n}-t{threads}-r{r}.7z"));
                if output.exists() {
                    fs::remove_file(&output)?;
                }
                println!("run nested={n} threads={threads} rep={r} ...");
                let elapsed = run_convert(&ac, &input, &output, threads, run.level, &temp_parent)?;
                output_bytes = fs::metadata(&output)?.len();
                println!(
                    "  → {:.2}s  out={} bytes",
                    elapsed.as_secs_f64(),
                    output_bytes
                );
                times.push(elapsed.as_secs_f64());
                // Keep last artifact; remove intermediates if multi-repeat
                if r + 1 < run.repeats {
                    fs::remove_file(&output)?;
                }
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = times[times.len() / 2];
            rows.push(BenchRow {
                scale: scale_name.into(),
                nested: n,
                threads,
                seconds: median,
                input_bytes,
                output_bytes,
            });
        }
    }

    let manual_ref = manual.as_ref();
    print_report(&rows, manual_ref, run.level);
    write_csv(&out_dir.join("results.csv"), &rows, manual_ref, run.level)?;
    write_markdown(&out_dir.join("results.md"), &rows, manual_ref, run.level)?;
    println!(
        "wrote {} and {}",
        out_dir.join("results.csv").display(),
        out_dir.join("results.md").display()
    );
    Ok(())
}

fn run_convert(
    ac: &Path,
    input: &Path,
    output: &Path,
    threads: u32,
    level: u32,
    temp_parent: &Path,
) -> Result<Duration> {
    let t0 = Instant::now();
    let status = Command::new(ac)
        .args([
            "convert",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--threads",
            &threads.to_string(),
            "--level",
            &level.to_string(),
            "--temp-dir",
            temp_parent.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .context("spawn archiveconverter")?;
    if !status.success() {
        bail!("convert failed (nested bench) status={status}");
    }
    Ok(t0.elapsed())
}

fn print_report(rows: &[BenchRow], manual: Option<&ManualBaselineFile>, level: u32) {
    println!();
    println!("==================== RESULTS ====================");
    let has_manual = manual.is_some();
    if has_manual {
        println!(
            "{:<8} {:>7} {:>8} {:>10} {:>10} {:>10} {:>10} {:>12}",
            "scale", "nested", "threads", "tool_s", "manual_s", "ratio", "vs_t1", "input_MiB"
        );
    } else {
        println!(
            "{:<8} {:>7} {:>8} {:>10} {:>12} {:>12} {:>8}",
            "scale", "nested", "threads", "seconds", "input_MiB", "output_MiB", "speedup"
        );
    }

    for row in rows {
        let base_t1 = rows
            .iter()
            .find(|b| b.nested == row.nested && b.threads == 1 && b.scale == row.scale)
            .map(|b| b.seconds)
            .unwrap_or(row.seconds);
        let speedup = base_t1 / row.seconds;

        if has_manual {
            let m = lookup_manual(manual, row.nested, row.threads, level);
            let (manual_s, ratio) = match m {
                Some(e) => (format!("{:.2}", e.seconds), format!("{:.2}x", row.seconds / e.seconds)),
                None => ("—".into(), "—".into()),
            };
            println!(
                "{:<8} {:>7} {:>8} {:>10.2} {:>10} {:>10} {:>9.2}x {:>12.1}",
                row.scale,
                row.nested,
                row.threads,
                row.seconds,
                manual_s,
                ratio,
                speedup,
                row.input_bytes as f64 / (1024.0 * 1024.0),
            );
        } else {
            println!(
                "{:<8} {:>7} {:>8} {:>10.2} {:>12.1} {:>12.1} {:>7.2}x",
                row.scale,
                row.nested,
                row.threads,
                row.seconds,
                row.input_bytes as f64 / (1024.0 * 1024.0),
                row.output_bytes as f64 / (1024.0 * 1024.0),
                speedup
            );
        }
    }
    println!("=================================================");
    println!();
    println!("Notes:");
    println!("- vs_t1 / speedup is tool threads=1 for the same nested count.");
    if has_manual {
        println!("- manual_s is stored baseline (one nest at a time, -mmt=N matching threads).");
        println!("- ratio = tool_s / manual_s  (<1 means tool faster). Same thread count both sides.");
        println!("- Tool may convert multiple nests concurrently (size budget); manual is always serial.");
        println!("- Single nest: tool forces pack threads=1 even if --threads N (MT often slower).");
    } else {
        println!("- Size-aware nested concurrency: --threads sets nest workers + pack -mmt (when nests≥2).");
        println!("- Single nest: pack threads forced to 1; multi-nest can convert in parallel under budget.");
    }
    println!("- Solid decompress is largely sequential; gains come from non-solid recompress + nest concurrency.");
    println!("- Many tiny files reduce MT pack efficiency; multi-nest workers still help overall wall time.");
    println!("- Published tables: docs/bench/RESULTS.md");
}

fn write_csv(
    path: &Path,
    rows: &[BenchRow],
    manual: Option<&ManualBaselineFile>,
    level: u32,
) -> Result<()> {
    let mut f = BufWriter::new(File::create(path)?);
    writeln!(
        f,
        "scale,nested,threads,tool_seconds,manual_seconds,ratio_tool_over_manual,input_bytes,output_bytes,speedup_vs_t1"
    )?;
    for row in rows {
        let base = rows
            .iter()
            .find(|b| b.nested == row.nested && b.threads == 1 && b.scale == row.scale)
            .map(|b| b.seconds)
            .unwrap_or(row.seconds);
        let speedup = base / row.seconds;
        let (manual_s, ratio) = match lookup_manual(manual, row.nested, row.threads, level) {
            Some(e) => (format!("{:.4}", e.seconds), format!("{:.4}", row.seconds / e.seconds)),
            None => (String::new(), String::new()),
        };
        writeln!(
            f,
            "{},{},{},{:.4},{},{},{},{},{:.4}",
            row.scale,
            row.nested,
            row.threads,
            row.seconds,
            manual_s,
            ratio,
            row.input_bytes,
            row.output_bytes,
            speedup
        )?;
    }
    Ok(())
}

fn write_markdown(
    path: &Path,
    rows: &[BenchRow],
    manual: Option<&ManualBaselineFile>,
    level: u32,
) -> Result<()> {
    let mut f = BufWriter::new(File::create(path)?);
    writeln!(f, "# Nested conversion benchmark results")?;
    writeln!(f)?;
    if manual.is_some() {
        writeln!(
            f,
            "| scale | nested | threads | tool s | manual s | ratio tool/manual | speedup vs t1 | input MiB |"
        )?;
        writeln!(
            f,
            "|-------|-------:|--------:|-------:|---------:|------------------:|--------------:|----------:|"
        )?;
    } else {
        writeln!(
            f,
            "| scale | nested | threads | seconds | input MiB | output MiB | speedup vs 1 thread |"
        )?;
        writeln!(
            f,
            "|-------|-------:|--------:|--------:|----------:|-----------:|--------------------:|"
        )?;
    }
    for row in rows {
        let base = rows
            .iter()
            .find(|b| b.nested == row.nested && b.threads == 1 && b.scale == row.scale)
            .map(|b| b.seconds)
            .unwrap_or(row.seconds);
        let speedup = base / row.seconds;
        if manual.is_some() {
            let (manual_s, ratio) = match lookup_manual(manual, row.nested, row.threads, level) {
                Some(e) => (format!("{:.2}", e.seconds), format!("{:.2}x", row.seconds / e.seconds)),
                None => ("—".into(), "—".into()),
            };
            writeln!(
                f,
                "| {} | {} | {} | {:.2} | {} | {} | {:.2}x | {:.1} |",
                row.scale,
                row.nested,
                row.threads,
                row.seconds,
                manual_s,
                ratio,
                speedup,
                row.input_bytes as f64 / (1024.0 * 1024.0),
            )?;
        } else {
            writeln!(
                f,
                "| {} | {} | {} | {:.2} | {:.1} | {:.1} | {:.2}x |",
                row.scale,
                row.nested,
                row.threads,
                row.seconds,
                row.input_bytes as f64 / (1024.0 * 1024.0),
                row.output_bytes as f64 / (1024.0 * 1024.0),
                speedup
            )?;
        }
    }
    writeln!(f)?;
    writeln!(f, "## Interpretation")?;
    writeln!(f)?;
    writeln!(
        f,
        "- These runs pin pack threads via `--threads N` (tool) / `-mmt=N` (manual baseline)."
    )?;
    writeln!(
        f,
        "- Manual baseline: **one nested at a time**. Tool may convert several nests concurrently under the size budget."
    )?;
    writeln!(
        f,
        "- Single nested archive: tool forces pack threads=1 (MT often slower on dense tiny-file nests)."
    )?;
    writeln!(
        f,
        "- `ratio tool/manual` uses the **same thread count** on both sides when a baseline cell exists."
    )?;
    Ok(())
}
