use thiserror::Error;

use crate::http::HttpError;

/// Why a provider request failed. A 404 for a *channel* lookup is not an error (it is an
/// empty set); everything else is.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EmoteError {
    /// The id is not a Twitch user id (digits only).
    #[error("invalid Twitch user id {0:?} (digits expected)")]
    InvalidUserId(String),
    /// Timeout, connection failure, oversized body...
    #[error(transparent)]
    Http(#[from] HttpError),
    /// The server answered with a non-success status (5xx, or 404 on a global endpoint).
    #[error("unexpected HTTP status {0}")]
    Status(u16),
    /// The body was not the JSON this client expects.
    #[error("malformed response: {0}")]
    Parse(String),
    /// A provider with a fallback source (FFZ) failed on both.
    #[error("primary source failed ({primary}); fallback failed too ({fallback})")]
    BothSourcesFailed {
        primary: Box<EmoteError>,
        fallback: Box<EmoteError>,
    },
}

impl EmoteError {
    pub(crate) fn parse(err: serde_json::Error) -> Self {
        Self::Parse(err.to_string())
    }

    /// `true` for a plain HTTP 404 (the channel has no account at the provider).
    pub(crate) fn is_not_found(&self) -> bool {
        matches!(self, Self::Status(404))
    }
}
