//! Command-line interface.

use crate::archive::native::{NativeOptions, NativePipeline, DEFAULT_LARGE_FILE_THRESHOLD};
use crate::archive::{BackendKind, PackOptions};
use crate::codec::{CodecKind, OuterFormat};
use crate::error::{Error, Result};
use crate::filter::{MemberFilter, NameTransformer};
use crate::pipeline::PipelineOptions;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Outer container for converted nested archives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliOuterFormat {
    /// Non-solid 7z (Copy/store method). Default when output is not `.tar` / dir.
    #[value(name = "7z", alias = "sevenz")]
    SevenZ,
    /// Uncompressed tar (nested members stay as compressed `.7z` files inside).
    Tar,
    /// No re-wrap: write first-layer members into a directory.
    #[value(name = "dir", alias = "directory", alias = "folder")]
    Dir,
}

impl From<CliOuterFormat> for OuterFormat {
    fn from(v: CliOuterFormat) -> Self {
        match v {
            CliOuterFormat::SevenZ => OuterFormat::SevenZ,
            CliOuterFormat::Tar => OuterFormat::Tar,
            CliOuterFormat::Dir => OuterFormat::Dir,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliBackend {
    /// Official 7zz/7z CLI
    Cli,
    /// Pure-Rust sevenz-rust2 (streaming solid→non-solid)
    Native,
}

impl From<CliBackend> for BackendKind {
    fn from(v: CliBackend) -> Self {
        match v {
            CliBackend::Cli => BackendKind::Cli,
            CliBackend::Native => BackendKind::Native,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "archiveconverter",
    version,
    about = "Convert nested solid 7z archives to non-solid form with filters and renames"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Increase logging verbosity (-v, -vv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Convert an outer 7z (with nested 7z members) to non-solid form
    Convert(ConvertArgs),
    /// Convert a single 7z archive to non-solid (no nested handling)
    ConvertSingle(ConvertSingleArgs),
    /// Show detected 7z backend
    Backend,
    /// List registered converters
    ListConverters,
}

#[derive(Debug, clap::Args)]
pub struct ConvertArgs {
    /// Input outer .7z archive
    pub input: PathBuf,

    /// Output path (file for 7z/tar, directory for `--outer-format dir`).
    /// For dir mode, defaults to `<input-dir>/<archive-stem>/` (matches the archive name).
    /// Required for 7z/tar unless you only dry-run.
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// Outer container: `7z` (default), `tar` (uncompressed), or `dir` (no re-wrap).
    /// If omitted: `.tar` path → tar; path ending in `/` → dir; else 7z.
    /// With `dir` and no `-o`, the directory is named after the input archive.
    #[arg(long = "outer-format", value_enum)]
    pub outer_format: Option<CliOuterFormat>,

    /// Regex to exclude members inside nested archives (repeatable).
    /// Appended after rsync `--filter-inner` / `--filter-from-inner` rules.
    #[arg(long = "exclude-inner")]
    pub exclude_inner: Vec<String>,

    /// Regex to exclude members of the outer archive (repeatable).
    /// Appended after rsync `--filter-outer` / `--filter-from-outer` rules.
    #[arg(long = "exclude-outer")]
    pub exclude_outer: Vec<String>,

    /// Regex include for nested members (repeatable; first-match with excludes).
    /// An include-only list does not drop other files (rsync default).
    #[arg(long = "include-inner")]
    pub include_inner: Vec<String>,

    /// Regex include for outer members (repeatable; first-match with excludes).
    #[arg(long = "include-outer")]
    pub include_outer: Vec<String>,

    /// Rsync filter rule for nested members (`+ pat`, `exclude pat`, or bare
    /// exclude). A leading `-` must use `--filter-inner='- *.tmp'` so clap
    /// does not treat it as a flag.
    #[arg(long = "filter-inner")]
    pub filter_inner: Vec<String>,

    /// Rsync filter rule for outer members (`+ pat`, `exclude pat`, or bare
    /// exclude). Use `--filter-outer='- pat'` if the rule starts with `-`.
    #[arg(long = "filter-outer")]
    pub filter_outer: Vec<String>,

    /// Rsync filter file for nested members (`--filter-from`).
    #[arg(long = "filter-from-inner")]
    pub filter_from_inner: Vec<PathBuf>,

    /// Rsync filter file for outer members.
    #[arg(long = "filter-from-outer")]
    pub filter_from_outer: Vec<PathBuf>,

    /// Rsync `--include-from` file for nested members (one include pattern per line).
    #[arg(long = "include-from-inner")]
    pub include_from_inner: Vec<PathBuf>,

    /// Rsync `--include-from` file for outer members.
    #[arg(long = "include-from-outer")]
    pub include_from_outer: Vec<PathBuf>,

    /// Rsync `--exclude-from` file for nested members (one exclude pattern per line).
    #[arg(long = "exclude-from-inner")]
    pub exclude_from_inner: Vec<PathBuf>,

    /// Rsync `--exclude-from` file for outer members.
    #[arg(long = "exclude-from-outer")]
    pub exclude_from_outer: Vec<PathBuf>,

    /// Rename rule PATTERN=REPLACEMENT for outer member names (repeatable)
    #[arg(long = "rename")]
    pub rename: Vec<String>,

    /// Match exclude patterns against basename only
    #[arg(long)]
    pub basename_match: bool,

    /// Temporary directory parent
    #[arg(long)]
    pub temp_dir: Option<PathBuf>,

    /// Keep temporary files for debugging
    #[arg(long)]
    pub keep_temp: bool,

    /// Verify output with 7z test and entry counts
    #[arg(long)]
    pub verify: bool,

    /// Print the plan without writing output
    #[arg(long)]
    pub dry_run: bool,

    /// Compression level 0-9
    #[arg(long, default_value_t = 5)]
    pub level: u32,

    /// Thread count for 7z / nested worker auto-count; omit for auto
    #[arg(long)]
    pub threads: Option<u32>,

    /// Max concurrent nested conversions (`0` = auto from --threads / CPUs)
    #[arg(long, default_value_t = 0)]
    pub nested_concurrency: usize,

    /// Max total packed size of nested archives converting at once
    /// (default `500M`). `0` = no size cap. Example: `500M`, `1G`, `524288000`.
    #[arg(long, default_value = "500M")]
    pub nested_size_budget: String,

    /// Disable solid-order single-pass outer extract (for A/B comparison).
    #[arg(long = "no-solid-single-pass")]
    pub no_solid_single_pass: bool,

    /// Always recompress nested archives even if already non-solid
    #[arg(long = "no-passthrough-nonsolid")]
    pub no_passthrough_nonsolid: bool,

    /// Disable extract/convert overlap prefetch
    #[arg(long = "no-pipeline-overlap")]
    pub no_pipeline_overlap: bool,

    /// Log stage timings at info level
    #[arg(long)]
    pub profile: bool,

    /// 7z engine: `cli` (default) or `native` (pure Rust / streaming)
    #[arg(long, value_enum, default_value_t = CliBackend::Cli)]
    pub backend: CliBackend,

    /// Native only: pipeline mode: `parallel` (default), `ahead:N`, or `sequential`
    #[arg(long, default_value = "parallel")]
    pub native_pipeline: String,

    /// Native only: size (bytes) above which a file uses multi-threaded LZMA2
    #[arg(long, default_value_t = DEFAULT_LARGE_FILE_THRESHOLD)]
    pub native_large_threshold: u64,

    /// Native only: LZMA2 codec `liblzma` (default) or `pure-rust`
    #[arg(long, default_value = "liblzma")]
    pub native_codec: String,
}

#[derive(Debug, clap::Args)]
pub struct ConvertSingleArgs {
    pub input: PathBuf,
    #[arg(short = 'o', long)]
    pub output: PathBuf,
    /// Regex exclude (repeatable; appended after rsync filter rules).
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,
    /// Regex include (repeatable; first-match with `--exclude`).
    #[arg(long = "include")]
    pub include: Vec<String>,
    /// Rsync filter rule (`+ pat`, `exclude pat`, or bare exclude).
    /// A leading `-` must use `--filter='- *.tmp'`.
    #[arg(long = "filter")]
    pub filter: Vec<String>,
    /// Rsync filter file.
    #[arg(long = "filter-from")]
    pub filter_from: Vec<PathBuf>,
    /// Rsync include-from file (one include pattern per line).
    #[arg(long = "include-from")]
    pub include_from: Vec<PathBuf>,
    /// Rsync exclude-from file (one exclude pattern per line).
    #[arg(long = "exclude-from")]
    pub exclude_from: Vec<PathBuf>,
    #[arg(long)]
    pub temp_dir: Option<PathBuf>,
    #[arg(long)]
    pub keep_temp: bool,
    #[arg(long)]
    pub verify: bool,
    #[arg(long, default_value_t = 5)]
    pub level: u32,
    #[arg(long)]
    pub threads: Option<u32>,
    /// 7z engine: `cli` or `native`
    #[arg(long, value_enum, default_value_t = CliBackend::Cli)]
    pub backend: CliBackend,
    /// Native pipeline: `parallel` | `sequential` | `ahead` | `ahead:N`
    #[arg(long, default_value = "parallel")]
    pub native_pipeline: String,
    /// Native only: MT LZMA2 threshold in bytes
    #[arg(long, default_value_t = DEFAULT_LARGE_FILE_THRESHOLD)]
    pub native_large_threshold: u64,
    /// Native LZMA2 codec: `liblzma` | `pure-rust`
    #[arg(long, default_value = "liblzma")]
    pub native_codec: String,
}

/// Build NativeOptions from shared CLI knobs.
pub fn native_options_from(
    pipeline: &str,
    large_threshold: u64,
    encode_threads: Option<u32>,
    codec: &str,
) -> Result<NativeOptions> {
    let mut o = NativeOptions::default();
    o.pipeline = parse_pipeline(pipeline)?;
    o.large_file_threshold = large_threshold;
    o.encode_threads = encode_threads;
    o.codec = CodecKind::parse(codec).ok_or_else(|| {
        Error::Other(format!(
            "unknown --native-codec '{codec}' (use liblzma or pure-rust)"
        ))
    })?;
    Ok(o)
}

fn parse_pipeline(s: &str) -> Result<NativePipeline> {
    let s = s.trim().to_ascii_lowercase();
    if s == "parallel" || s == "parallel-codec" || s == "p3" {
        return Ok(NativePipeline::ParallelCodec);
    }
    if s == "sequential" || s == "seq" || s == "0" {
        return Ok(NativePipeline::Sequential);
    }
    if s == "ahead" || s == "pipeline" {
        return Ok(NativePipeline::DecodeAhead { depth: 2 });
    }
    if let Some(rest) = s.strip_prefix("ahead:") {
        let depth: usize = rest.parse().map_err(|_| {
            Error::Other(format!("invalid --native-pipeline depth in '{s}'"))
        })?;
        return Ok(NativePipeline::DecodeAhead {
            depth: depth.max(1),
        });
    }
    // numeric only = ahead depth
    if let Ok(depth) = s.parse::<usize>() {
        return Ok(if depth == 0 {
            NativePipeline::Sequential
        } else {
            NativePipeline::DecodeAhead { depth }
        });
    }
    Err(Error::Other(format!(
        "unknown --native-pipeline '{s}' (parallel|sequential|ahead|ahead:N)"
    )))
}

impl ConvertArgs {
    /// Resolve outer format + output path (dir defaults to input archive stem).
    pub fn resolve_output(&self) -> Result<(OuterFormat, std::path::PathBuf)> {
        let format = OuterFormat::resolve(
            self.outer_format.map(Into::into),
            self.output.as_deref(),
        );
        let output = match &self.output {
            Some(p) => p.clone(),
            None if format.is_directory() => {
                crate::codec::default_dir_from_input(&self.input)
            }
            None if self.dry_run => {
                // Dry-run never writes; placeholder path is fine.
                std::path::PathBuf::from("out.7z")
            }
            None => {
                return Err(Error::Other(
                    "-o/--output is required for outer formats 7z and tar \
                     (for directory output use --outer-format dir; default dir name \
                     matches the input archive stem)"
                        .into(),
                ));
            }
        };
        Ok((format, output))
    }

    pub fn to_pipeline_options(&self) -> Result<PipelineOptions> {
        let exclude_inner = MemberFilter::from_cli(
            &self.filter_from_inner,
            &self.filter_inner,
            &self.include_from_inner,
            &self.exclude_from_inner,
            &self.include_inner,
            &self.exclude_inner,
            self.basename_match,
        )?;
        let exclude_outer = MemberFilter::from_cli(
            &self.filter_from_outer,
            &self.filter_outer,
            &self.include_from_outer,
            &self.exclude_from_outer,
            &self.include_outer,
            &self.exclude_outer,
            self.basename_match,
        )?;
        let rename = NameTransformer::from_pairs(&self.rename)?;
        if self.level > 9 {
            return Err(Error::Other("--level must be 0-9".into()));
        }
        let (outer_format, output) = self.resolve_output()?;
        let mut opts = PipelineOptions::new(self.input.clone(), output);
        opts.exclude_inner = exclude_inner;
        opts.exclude_outer = exclude_outer;
        opts.rename = rename;
        opts.temp_dir = self.temp_dir.clone();
        opts.keep_temp = self.keep_temp;
        opts.verify = self.verify;
        opts.dry_run = self.dry_run;
        opts.nested_concurrency = self.nested_concurrency;
        opts.nested_size_budget = crate::util::parse_byte_size(&self.nested_size_budget)?;
        opts.solid_single_pass = !self.no_solid_single_pass;
        opts.passthrough_nonsolid = !self.no_passthrough_nonsolid;
        opts.pipeline_overlap = !self.no_pipeline_overlap;
        opts.profile = self.profile;
        // Native backend: prefer streaming solid→non-solid (no full tree).
        opts.prefer_streaming = matches!(self.backend, CliBackend::Native);
        opts.outer_format = outer_format;
        opts.pack = PackOptions {
            non_solid: true,
            threads: self.threads, // None → auto policy inside converter
            level: self.level,
        };
        Ok(opts)
    }

    pub fn backend_kind(&self) -> BackendKind {
        self.backend.into()
    }

    pub fn native_options(&self) -> Result<NativeOptions> {
        native_options_from(
            &self.native_pipeline,
            self.native_large_threshold,
            self.threads,
            &self.native_codec,
        )
    }
}

impl ConvertSingleArgs {
    pub fn member_filter(&self) -> Result<MemberFilter> {
        MemberFilter::from_cli(
            &self.filter_from,
            &self.filter,
            &self.include_from,
            &self.exclude_from,
            &self.include,
            &self.exclude,
            false,
        )
    }

    pub fn backend_kind(&self) -> BackendKind {
        self.backend.into()
    }

    pub fn native_options(&self) -> Result<NativeOptions> {
        native_options_from(
            &self.native_pipeline,
            self.native_large_threshold,
            self.threads,
            &self.native_codec,
        )
    }
}
