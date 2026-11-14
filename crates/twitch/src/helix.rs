//! Helix (`api.twitch.tv/helix`) plumbing: authenticated GETs with one retry after a refresh, and
//! Twitch's rate-limit headers respected.
//!
//! Every request carries `Client-Id` and `Authorization: Bearer <user token>` from [`Auth`]. A 401
//! refreshes the token through [`Auth::refresh_after_unauthorized`] (so a burst of 401s makes one
//! refresh) and retries once. `Ratelimit-Remaining` / `Ratelimit-Reset` are remembered: while the
//! bucket is empty, requests wait for the reset (up to [`HelixClient::max_wait`]); a 429 is retried
//! once after that wait.
//!
//! Endpoint-specific parsing lives with the caller; this returns status and body. The base URL is
//! injectable, so tests run against a local mock server.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap};
use thiserror::Error;

use crate::auth::{Auth, AuthError, TokenLease, describe, unix_now, user_agent};

/// The default Helix base.
pub const HELIX_BASE_URL: &str = "https://api.twitch.tv/helix";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest wait for an empty rate-limit bucket before a request goes out anyway.
const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(60);

/// Why a Helix request failed. Non-2xx statuses are not errors: they come back as a
/// [`HelixResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HelixError {
    /// No token (logged out), or the login ended while refreshing.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// No complete response.
    #[error("network error: {0}")]
    Network(String),
}

/// A completed Helix exchange (any status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelixResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The last rate-limit headers Twitch sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimit {
    pub limit: Option<u32>,
    pub remaining: Option<u32>,
    /// When the bucket refills, in Unix seconds.
    pub reset_at: Option<u64>,
}

impl RateLimit {
    /// Reads `Ratelimit-Limit`, `Ratelimit-Remaining` and `Ratelimit-Reset`; `None` when the
    /// response has none of them.
    pub fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let number = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
        };
        let limit = RateLimit {
            limit: number("ratelimit-limit").and_then(|v| u32::try_from(v).ok()),
            remaining: number("ratelimit-remaining").and_then(|v| u32::try_from(v).ok()),
            reset_at: number("ratelimit-reset"),
        };
        (limit != RateLimit::default()).then_some(limit)
    }

    /// How long to wait at `now` (Unix seconds) before the next request: until the reset while the
    /// bucket is empty, else nothing.
    pub fn wait(&self, now: u64) -> Duration {
        match (self.remaining, self.reset_at) {
            (Some(0), Some(reset)) if reset > now => Duration::from_secs(reset - now),
            // Empty with no reset time: a short pause rather than hammering.
            (Some(0), None) => Duration::from_secs(1),
            _ => Duration::ZERO,
        }
    }
}

struct Inner {
    http: reqwest::Client,
    base: String,
    auth: Auth,
    limit: Mutex<RateLimit>,
    max_wait: Duration,
}

/// Authenticated Helix GETs. Cheap to clone; clones share the rate-limit state.
#[derive(Clone)]
pub struct HelixClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for HelixClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelixClient")
            .field("base", &self.inner.base)
            .field("rate_limit", &self.rate_limit())
            .finish_non_exhaustive()
    }
}

impl HelixClient {
    /// A client for `api.twitch.tv/helix` acting as `auth`'s user.
    pub fn new(auth: Auth) -> Result<Self, HelixError> {
        Self::with_base(auth, HELIX_BASE_URL)
    }

    /// A client talking to `base` instead of the real Helix (tests).
    pub fn with_base(auth: Auth, base: impl Into<String>) -> Result<Self, HelixError> {
        let http = reqwest::Client::builder()
            .user_agent(user_agent())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| HelixError::Network(describe(error)))?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                base: base.into().trim_end_matches('/').to_owned(),
                auth,
                limit: Mutex::new(RateLimit::default()),
                max_wait: DEFAULT_MAX_WAIT,
            }),
        })
    }

    /// The base URL; [`HelixClient::get`] takes paths below it.
    pub fn base(&self) -> &str {
        &self.inner.base
    }

    pub fn auth(&self) -> &Auth {
        &self.inner.auth
    }

    /// The last rate-limit headers seen.
    pub fn rate_limit(&self) -> RateLimit {
        *self
            .inner
            .limit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Longest wait for the rate-limit reset.
    pub fn max_wait(&self) -> Duration {
        self.inner.max_wait
    }

    /// The full URL of `path` (`chat/badges/global`, `chat/badges?broadcaster_id=1`).
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.inner.base, path.trim_start_matches('/'))
    }

    /// GETs `path` (relative to the base) as the logged-in user.
    ///
    /// A 401 refreshes the token and retries once; a 429 waits for the rate-limit reset and
    /// retries once. Whatever the retry returns is the result.
    pub async fn get(&self, path: &str) -> Result<HelixResponse, HelixError> {
        let auth = &self.inner.auth;
        let mut lease = auth.access_token().await?;
        let mut response = self.send(path, &lease).await?;
        if response.status == 401 {
            lease = auth.refresh_after_unauthorized(lease.generation).await?;
            response = self.send(path, &lease).await?;
        }
        if response.status == 429 {
            // `send` waits for the reset the 429 announced before going out again.
            let lease = auth.access_token().await?;
            response = self.send(path, &lease).await?;
        }
        Ok(response)
    }

    async fn send(&self, path: &str, lease: &TokenLease) -> Result<HelixResponse, HelixError> {
        let wait = self.rate_limit().wait(unix_now()).min(self.inner.max_wait);
        if !wait.is_zero() {
            tracing::debug!(?wait, "Helix rate limit exhausted; waiting for the reset");
            tokio::time::sleep(wait).await;
        }
        let response = self
            .inner
            .http
            .get(self.url(path))
            .header("Client-Id", self.inner.auth.client_id())
            .header(AUTHORIZATION, format!("Bearer {}", lease.token.expose()))
            .send()
            .await
            .map_err(|error| HelixError::Network(describe(error)))?;
        let status = response.status().as_u16();
        if let Some(limit) = RateLimit::from_headers(response.headers()) {
            *self
                .inner
                .limit
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = limit;
        } else if status == 429 {
            // A 429 without headers: assume an empty bucket for a moment.
            *self
                .inner
                .limit
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = RateLimit {
                remaining: Some(0),
                ..RateLimit::default()
            };
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| HelixError::Network(describe(error)))?
            .to_vec();
        Ok(HelixResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn rate_limit_headers() {
        let mut headers = HeaderMap::new();
        assert_eq!(RateLimit::from_headers(&headers), None);
        headers.insert("Ratelimit-Limit", HeaderValue::from_static("800"));
        headers.insert("Ratelimit-Remaining", HeaderValue::from_static("0"));
        headers.insert("Ratelimit-Reset", HeaderValue::from_static("1000"));
        let limit = RateLimit::from_headers(&headers).unwrap();
        assert_eq!(
            limit,
            RateLimit {
                limit: Some(800),
                remaining: Some(0),
                reset_at: Some(1000)
            }
        );
        assert_eq!(limit.wait(990), Duration::from_secs(10));
        assert_eq!(limit.wait(1000), Duration::ZERO);
        assert_eq!(limit.wait(2000), Duration::ZERO);
        let some_left = RateLimit {
            remaining: Some(5),
            ..limit
        };
        assert_eq!(some_left.wait(990), Duration::ZERO);
        let no_reset = RateLimit {
            reset_at: None,
            ..limit
        };
        assert_eq!(no_reset.wait(990), Duration::from_secs(1));
    }
}
