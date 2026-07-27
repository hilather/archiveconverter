//! Archive converter library: nested solid 7z → non-solid with filters.

pub mod archive;
pub mod cli;
pub mod codec;
pub mod convert;
pub mod error;
pub mod filter;
pub mod pipeline;
pub mod util;

pub use error::{Error, Result};
