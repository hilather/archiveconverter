//! Command-line interface.

use crate::archive::PackOptions;
use crate::error::{Error, Result};
use crate::filter::{MemberFilter, NameTransformer};
use crate::pipeline::PipelineOptions;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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

    /// Thread count for 7z (-mmt); omit for -mmt=on
    #[arg(long)]
    pub threads: Option<u32>,

    /// Nested conversion concurrency (v1: only 1 is used for disk safety)
    #[arg(long, default_value_t = 1)]
    pub nested_concurrency: usize,
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
        opts.nested_concurrency = self.nested_concurrency.max(1);
        opts.pack = PackOptions {
            non_solid: true,
            threads: self.threads,
            level: self.level,
        };
        Ok(opts)
    }
}
