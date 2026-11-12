//! Error types. All are `Clone` because one failed load is reported to every waiter.

use std::sync::Arc;

/// Why a download failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// HTTP 404 or 410. Negatively cached (see `ImageCacheConfig::not_found_ttl`).
    #[error("not found (HTTP {0})")]
    NotFound(u16),
    /// Any other non-success status.
    #[error("HTTP status {0}")]
    Status(u16),
    /// No complete response within the request timeout.
    #[error("timed out")]
    Timeout,
    /// Connection, TLS or protocol failure.
    #[error("network error: {0}")]
    Network(Arc<str>),
    /// The body was larger than `ImageCacheConfig::max_download_bytes`.
    #[error("response larger than {limit} bytes")]
    TooLarge { limit: u64 },
}

impl FetchError {
    /// Worth retrying: timeouts, network failures, 408, 429 and 5xx.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Timeout | Self::Network(_) => true,
            Self::Status(s) => *s == 408 || *s == 429 || *s >= 500,
            Self::NotFound(_) | Self::TooLarge { .. } => false,
        }
    }
}

/// Why bytes could not be turned into frames.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("empty input")]
    Empty,
    #[error("unrecognised image format")]
    UnknownFormat,
    /// Recognised but not decoded here (AVIF, JPEG, ...).
    #[error("unsupported image format: {0}")]
    Unsupported(Arc<str>),
    /// Canvas exceeds `DecodeLimits`.
    #[error("image too large: {width}x{height}")]
    TooLarge { width: u32, height: u32 },
    /// Truncated or invalid data.
    #[error("malformed image: {0}")]
    Malformed(Arc<str>),
}

/// The result of a failed image load, as seen by every waiter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// The load task was dropped before finishing (runtime shut down, or it panicked).
    #[error("image load cancelled")]
    Cancelled,
}

/// Failure to construct an [`crate::ImageCache`].
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("could not build the HTTP client: {0}")]
    Http(String),
}
