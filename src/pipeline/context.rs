//! Runtime options for the conversion pipeline.

use crate::archive::PackOptions;
use crate::codec::OuterFormat;
use crate::filter::{MemberFilter, NameTransformer};
use crate::util::DEFAULT_NESTED_SIZE_BUDGET;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct PipelineOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub exclude_inner: MemberFilter,
    pub exclude_outer: MemberFilter,
    pub rename: NameTransformer,
    pub temp_dir: Option<PathBuf>,
    pub keep_temp: bool,
    pub verify: bool,
    pub dry_run: bool,
    /// Max concurrent nested conversions (`0` = auto from `--threads` / CPU count).
    pub nested_concurrency: usize,
    /// Max sum of **packed** sizes of nested archives in flight together.
    /// Default 500 MiB. `0` = no size cap (workers only). A single nest larger
    /// than the budget still runs alone.
    pub nested_size_budget: u64,
    pub pack: PackOptions,
    /// Solid outers: extract needed members in one 7z pass.
    pub solid_single_pass: bool,
    /// Skip recompress when nested is already non-solid and filters empty.
    pub passthrough_nonsolid: bool,
    /// Prefetch next nested extract while converting current (serial path).
    pub pipeline_overlap: bool,
    /// Emit stage timings at info level.
    pub profile: bool,
    /// Prefer native streaming convert (no full extract tree) when backend supports it.
    pub prefer_streaming: bool,
    /// Outer container: non-solid 7z (default) or uncompressed tar.
    pub outer_format: OuterFormat,
}

impl PipelineOptions {
    pub fn new(input: PathBuf, output: PathBuf) -> Self {
        let outer_format = OuterFormat::from_output_path(&output);
        Self {
            input,
            output,
            exclude_inner: MemberFilter::new(),
            exclude_outer: MemberFilter::new(),
            rename: NameTransformer::new(),
            temp_dir: None,
            keep_temp: false,
            verify: false,
            dry_run: false,
            nested_concurrency: 0, // auto
            nested_size_budget: DEFAULT_NESTED_SIZE_BUDGET,
            pack: PackOptions::default(),
            solid_single_pass: true,
            passthrough_nonsolid: true,
            pipeline_overlap: true,
            profile: false,
            prefer_streaming: false,
            outer_format,
        }
    }
}
