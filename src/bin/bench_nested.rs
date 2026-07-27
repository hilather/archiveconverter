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
//!   cargo run --release --bin bench_nested -- run --scale tiny --threads 1,2,4
//!   cargo run --release --bin bench_nested -- all --scale small
//!   # aliases: --quick == --scale quick

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
                threads: a.threads,
                nested_counts: a.nested_counts,
                level: a.level,
                warmup: a.warmup,
                repeats: a.repeats,
            };
            generate(&data, &gen)?;
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
    println!("  bench_nested all --scale small --threads 1,2,4");
    println!("  bench_nested run --scale full --threads 1,2,3,4");
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
    println!("=== Benchmark run ({scale_name}) ===");
    println!("archiveconverter: {}", ac.display());
    println!("threads: {:?}", run.threads);
    println!("nested: {nested_counts:?}");
    println!("level: {}", run.level);

    let out_dir = root.join("results");
    fs::create_dir_all(&out_dir)?;
    let temp_parent = root.join("tmp");
    fs::create_dir_all(&temp_parent)?;

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

    print_report(&rows);
    write_csv(&out_dir.join("results.csv"), &rows)?;
    write_markdown(&out_dir.join("results.md"), &rows)?;
    println!("wrote {} and {}", out_dir.join("results.csv").display(), out_dir.join("results.md").display());
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

fn print_report(rows: &[BenchRow]) {
    println!();
    println!("==================== RESULTS ====================");
    println!(
        "{:<8} {:>7} {:>8} {:>10} {:>12} {:>12} {:>8}",
        "scale", "nested", "threads", "seconds", "input_MiB", "output_MiB", "speedup"
    );

    // Baseline: per nested count, threads=1
    for row in rows {
        let base = rows
            .iter()
            .find(|b| b.nested == row.nested && b.threads == 1 && b.scale == row.scale)
            .map(|b| b.seconds)
            .unwrap_or(row.seconds);
        let speedup = base / row.seconds;
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
    println!("=================================================");
    println!();
    println!("Notes:");
    println!("- Speedup is vs threads=1 for the same nested count.");
    println!("- Nested archives convert serially (disk-safe); --threads only affects 7z -mmt.");
    println!("- Solid decompress is largely sequential; gains come from non-solid recompress.");
    println!("- Many tiny files reduce MT efficiency vs few large files.");
}

fn write_csv(path: &Path, rows: &[BenchRow]) -> Result<()> {
    let mut f = BufWriter::new(File::create(path)?);
    writeln!(f, "scale,nested,threads,seconds,input_bytes,output_bytes,speedup_vs_t1")?;
    for row in rows {
        let base = rows
            .iter()
            .find(|b| b.nested == row.nested && b.threads == 1 && b.scale == row.scale)
            .map(|b| b.seconds)
            .unwrap_or(row.seconds);
        let speedup = base / row.seconds;
        writeln!(
            f,
            "{},{},{},{:.4},{},{},{:.4}",
            row.scale, row.nested, row.threads, row.seconds, row.input_bytes, row.output_bytes, speedup
        )?;
    }
    Ok(())
}

fn write_markdown(path: &Path, rows: &[BenchRow]) -> Result<()> {
    let mut f = BufWriter::new(File::create(path)?);
    writeln!(f, "# Nested conversion benchmark results")?;
    writeln!(f)?;
    writeln!(
        f,
        "| scale | nested | threads | seconds | input MiB | output MiB | speedup vs 1 thread |"
    )?;
    writeln!(
        f,
        "|-------|-------:|--------:|--------:|----------:|-----------:|--------------------:|"
    )?;
    for row in rows {
        let base = rows
            .iter()
            .find(|b| b.nested == row.nested && b.threads == 1 && b.scale == row.scale)
            .map(|b| b.seconds)
            .unwrap_or(row.seconds);
        let speedup = base / row.seconds;
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
    writeln!(f)?;
    writeln!(f, "## Interpretation")?;
    writeln!(f)?;
    writeln!(
        f,
        "- Default conversion uses `-mmt=on` (all CPUs). These runs pin `-mmt=N` via `--threads`."
    )?;
    writeln!(
        f,
        "- Nested members are still processed **one at a time**; threads only parallelize 7z compression."
    )?;
    writeln!(
        f,
        "- Expect **sub-linear** speedups (often ~1.3–2.5× at 4 threads for this workload), not 4×."
    )?;
    Ok(())
}
