use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("7z backend not found: {0}")]
    BackendMissing(String),

    #[error("7z command failed: {0}")]
    BackendFailed(String),

    #[error("invalid regex '{pattern}': {source}")]
    InvalidRegex {
        pattern: String,
        #[source]
        source: regex::Error,
    },

    #[error("name collision: multiple members map to '{0}'")]
    NameCollision(String),

    #[error("entry not found in archive: {0}")]
    EntryNotFound(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for Error {
    fn from(value: anyhow::Error) -> Self {
        Error::Other(value.to_string())
    }
}
