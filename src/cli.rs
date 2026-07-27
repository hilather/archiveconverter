//! Command-line interface.

use crate::archive::native::{NativeOptions, NativePipeline, DEFAULT_LARGE_FILE_THRESHOLD};
use crate::archive::{BackendKind, PackOptions};
use crate::codec::CodecKind;
use crate::error::{Error, Result};
use crate::filter::{MemberFilter, NameTransformer};
use crate::pipeline::PipelineOptions;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

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

    /// Output .7z path
    #[arg(short = 'o', long)]
    pub output: PathBuf,

    /// Regex to exclude members inside nested archives (repeatable)
    #[arg(long = "exclude-inner")]
    pub exclude_inner: Vec<String>,

    /// Regex to exclude members of the outer archive (repeatable)
    #[arg(long = "exclude-outer")]
    pub exclude_outer: Vec<String>,

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
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,
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
    pub fn to_pipeline_options(&self) -> Result<PipelineOptions> {
        let mut exclude_inner = MemberFilter::with_excludes(&self.exclude_inner)?;
        let mut exclude_outer = MemberFilter::with_excludes(&self.exclude_outer)?;
        if self.basename_match {
            exclude_inner = exclude_inner.basename_only(true);
            exclude_outer = exclude_outer.basename_only(true);
        }
        let rename = NameTransformer::from_pairs(&self.rename)?;
        if self.level > 9 {
            return Err(Error::Other("--level must be 0-9".into()));
        }
        let mut opts = PipelineOptions::new(self.input.clone(), self.output.clone());
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
