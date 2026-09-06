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
    /// Cooperative stop via [`CancelFlag`](crate::CancelFlag) (pause button).
    /// Never retried automatically — resuming is the user's choice.
    #[error("download cancelled")]
    Cancelled,
    /// Media (HLS/DASH) shape we do not support yet (encrypted streams,
    /// live manifests, unknown segment schemes). Never retried.
    #[error("unsupported media: {0}")]
    Unsupported(String),
    /// Anything else (config, state file, ...).
    #[error("{0}")]
    Other(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, CcdmError>;

impl CcdmError {
    /// Whether retrying later might succeed (network hiccups, 5xx, ...),
    /// as opposed to permanent problems (bad URL, no range support).
    /// Mirrors XDM's transient-vs-fatal failure split.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Http(_) | Self::Io(_) | Self::Other(_) => true,
            Self::InvalidUrl(_)
            | Self::RangeNotSupported
            | Self::Cancelled
            | Self::Unsupported(_) => false,
        }
    }
}

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
