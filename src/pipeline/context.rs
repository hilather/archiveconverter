//! Runtime options for the conversion pipeline.

use crate::filter::{MemberFilter, NameTransformer};
use crate::archive::PackOptions;
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
    /// Nested archive conversions run one at a time (disk efficiency).
    pub nested_concurrency: usize,
    pub pack: PackOptions,
}

impl PipelineOptions {
    pub fn new(input: PathBuf, output: PathBuf) -> Self {
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
            nested_concurrency: 1,
            pack: PackOptions::default(),
        }
    }
}
