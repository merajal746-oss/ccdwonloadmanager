//! Error type for the core engine.

use thiserror::Error;

/// All errors produced by `ccdm-core`.
#[derive(Debug, Error)]
pub enum CcdmError {
    /// HTTP/client failure (message only, so the type stays small).
    #[error("http error: {0}")]
    Http(String),
    /// Filesystem failure.
    #[error("io error: {0}")]
    Io(String),
    /// URL could not be parsed.
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    /// Server ignored our `Range` request; resume/segmenting is impossible.
    #[error("server does not support range requests")]
    RangeNotSupported,
    /// Anything else (config, state file, ...).
    #[error("{0}")]
    Other(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, CcdmError>;

impl From<reqwest::Error> for CcdmError {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err.to_string())
    }
}

impl From<std::io::Error> for CcdmError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<serde_json::Error> for CcdmError {
    fn from(err: serde_json::Error) -> Self {
        Self::Other(err.to_string())
    }
}

impl From<url::ParseError> for CcdmError {
    fn from(err: url::ParseError) -> Self {
        Self::InvalidUrl(err.to_string())
    }
}
