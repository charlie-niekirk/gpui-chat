//! `id.twitch.tv`'s OAuth endpoints, one request per call.
//!
//! * `POST /oauth2/device` → a [`DeviceCode`] (RFC 8628 device authorization);
//! * `POST /oauth2/token` with the device-code grant → [`PollOutcome`] (pending, slow down, a token,
//!   expired, denied), or with `grant_type=refresh_token` → a new token pair;
//! * `GET /oauth2/validate` (`Authorization: OAuth <token>`) → [`Validation`];
//! * `POST /oauth2/revoke`.
//!
//! Twitch reports errors as `{"status": 400, "message": "authorization_pending"}`; the RFC's
//! `{"error": "..."}` is accepted too.

use std::time::Duration;

use gpui_chat_core::APP_NAME;
use serde::Deserialize;
use thiserror::Error;

use super::{Secret, StoreError};

/// The default OAuth base.
pub const ID_BASE_URL: &str = "https://id.twitch.tv";

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Why an auth operation failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    /// No complete response (timeout, DNS, connection refused, ...). Worth retrying later.
    #[error("network error: {0}")]
    Network(String),
    /// An unexpected status, with Twitch's message.
    #[error("Twitch answered {status}: {message}")]
    Status { status: u16, message: String },
    /// The body was not what the endpoint returns.
    #[error("malformed response: {0}")]
    Parse(String),
    /// Twitch refused the refresh token (used, revoked or expired): the login is over.
    #[error("Twitch rejected the refresh token: {0}")]
    RefreshRejected(String),
    /// Validation said the access token is no good.
    #[error("the access token is invalid")]
    InvalidToken,
    /// The device code expired before the user approved it.
    #[error("the login code expired")]
    DeviceCodeExpired,
    /// The user declined the authorization.
    #[error("the authorization was declined")]
    AccessDenied,
    /// Nobody is logged in.
    #[error("not logged in")]
    NotLoggedIn,
    /// The login ended (a refresh was rejected) while this request waited for it.
    #[error("the Twitch session ended; log in again")]
    SessionEnded,
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A background task did not finish (the runtime is shutting down).
    #[error("interrupted: {0}")]
    Interrupted(String),
}

impl AuthError {
    /// `true` for failures that say nothing about the token (network, 5xx): keep the login.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network(_) | Self::Interrupted(_) => true,
            Self::Status { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

/// What the user must do to approve the login: open [`DeviceCode::verification_uri`] and enter
/// [`DeviceCode::user_code`] (Twitch's link already carries the code).
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    /// The secret the client polls with; never shown.
    pub device_code: Secret,
    /// Seconds to wait between polls, as Twitch asked.
    pub interval: u64,
    /// Seconds until the code expires.
    pub expires_in: u64,
}

impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCode")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("device_code", &self.device_code)
            .field("interval", &self.interval)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// A new access / refresh token pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: Secret,
    pub refresh_token: Secret,
    /// Seconds until the access token expires; 0 when Twitch did not say.
    pub expires_in: u64,
    pub scopes: Vec<String>,
}

/// One poll of the device-code grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// The user approved.
    Token(TokenResponse),
    /// Not yet; poll again after the interval.
    Pending,
    /// Polling too fast; add 5 seconds to the interval (RFC 8628 §3.5).
    SlowDown,
    Expired,
    Denied,
}

/// What `oauth2/validate` says about a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
    pub client_id: String,
    pub login: String,
    pub user_id: String,
    pub scopes: Vec<String>,
    /// Seconds until the token expires (0 for tokens that do not).
    pub expires_in: u64,
}

#[derive(Deserialize)]
struct RawDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct RawToken {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<Vec<String>>,
}

impl RawToken {
    fn into_response(self) -> TokenResponse {
        TokenResponse {
            access_token: Secret::new(self.access_token),
            refresh_token: Secret::new(self.refresh_token),
            expires_in: self.expires_in.unwrap_or(0),
            scopes: self.scope.unwrap_or_default(),
        }
    }
}

#[derive(Deserialize)]
struct RawValidation {
    client_id: String,
    // Absent for app access tokens, which this client never has.
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Deserialize, Default)]
struct RawError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// The message of an error body, or the body itself when it is not JSON.
fn error_message(body: &[u8]) -> String {
    let raw: RawError = serde_json::from_slice(body).unwrap_or_default();
    raw.message
        .or(raw.error)
        .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(200).collect())
}

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, AuthError> {
    serde_json::from_slice(body).map_err(|error| AuthError::Parse(error.to_string()))
}

pub(crate) fn network(error: reqwest::Error) -> AuthError {
    AuthError::Network(describe(error))
}

/// A transport error as text. The URL is dropped: none of ours carries a token (tokens go in
/// bodies and headers), but query strings have no business in logs either.
pub(crate) fn describe(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

/// `User-Agent` for Twitch requests: `<APP_NAME>/<version>`.
pub(crate) fn user_agent() -> String {
    format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION"))
}

/// The OAuth endpoints for one client id. Cheap to clone.
#[derive(Clone)]
pub struct OAuthClient {
    http: reqwest::Client,
    base: String,
    client_id: String,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("base", &self.base)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl OAuthClient {
    /// A client for `id.twitch.tv`.
    pub fn new(client_id: impl Into<String>) -> Result<Self, AuthError> {
        Self::with_base(client_id, ID_BASE_URL)
    }

    /// A client talking to `base` instead of `https://id.twitch.tv` (tests).
    pub fn with_base(
        client_id: impl Into<String>,
        base: impl Into<String>,
    ) -> Result<Self, AuthError> {
        let http = reqwest::Client::builder()
            .user_agent(user_agent())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(network)?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            client_id: client_id.into(),
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    fn url(&self, path: &str) -> String {
        format!("{}/oauth2/{path}", self.base)
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<(u16, Vec<u8>), AuthError> {
        let response = self
            .http
            .post(self.url(path))
            .form(form)
            .send()
            .await
            .map_err(network)?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(network)?.to_vec();
        Ok((status, body))
    }

    /// Starts a device login for `scopes`.
    pub async fn request_device_code(&self, scopes: &[&str]) -> Result<DeviceCode, AuthError> {
        let scopes = scopes.join(" ");
        let (status, body) = self
            .post(
                "device",
                &[("client_id", &self.client_id), ("scopes", &scopes)],
            )
            .await?;
        if !(200..300).contains(&status) {
            return Err(AuthError::Status {
                status,
                message: error_message(&body),
            });
        }
        let raw: RawDeviceCode = parse(&body)?;
        Ok(DeviceCode {
            user_code: raw.user_code,
            verification_uri: raw.verification_uri,
            device_code: Secret::new(raw.device_code),
            // RFC 8628: 5 seconds when the server does not say.
            interval: raw.interval.unwrap_or(5),
            expires_in: raw.expires_in.unwrap_or(1800),
        })
    }

    /// Asks once whether the user approved `code`.
    pub async fn poll_device_code(
        &self,
        code: &DeviceCode,
        scopes: &[&str],
    ) -> Result<PollOutcome, AuthError> {
        let scopes = scopes.join(" ");
        let (status, body) = self
            .post(
                "token",
                &[
                    ("client_id", &self.client_id),
                    ("scopes", &scopes),
                    ("device_code", code.device_code.expose()),
                    ("grant_type", DEVICE_GRANT),
                ],
            )
            .await?;
        if (200..300).contains(&status) {
            return Ok(PollOutcome::Token(
                parse::<RawToken>(&body)?.into_response(),
            ));
        }
        let message = error_message(&body);
        let outcome = match message.to_ascii_lowercase().as_str() {
            "authorization_pending" => PollOutcome::Pending,
            "slow_down" => PollOutcome::SlowDown,
            "expired_token" | "invalid device code" => PollOutcome::Expired,
            "access_denied" => PollOutcome::Denied,
            _ => return Err(AuthError::Status { status, message }),
        };
        Ok(outcome)
    }

    /// Trades `refresh_token` (one use) for a new pair.
    pub async fn refresh(&self, refresh_token: &Secret) -> Result<TokenResponse, AuthError> {
        let (status, body) = self
            .post(
                "token",
                &[
                    ("client_id", &self.client_id),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token.expose()),
                ],
            )
            .await?;
        match status {
            200..=299 => Ok(parse::<RawToken>(&body)?.into_response()),
            400 | 401 | 403 => Err(AuthError::RefreshRejected(error_message(&body))),
            status => Err(AuthError::Status {
                status,
                message: error_message(&body),
            }),
        }
    }

    /// Asks Twitch whether `access_token` is still good, and whose it is.
    pub async fn validate(&self, access_token: &Secret) -> Result<Validation, AuthError> {
        let response = self
            .http
            .get(self.url("validate"))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("OAuth {}", access_token.expose()),
            )
            .send()
            .await
            .map_err(network)?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(network)?;
        match status {
            200..=299 => {
                let raw: RawValidation = parse(&body)?;
                Ok(Validation {
                    client_id: raw.client_id,
                    login: raw.login.unwrap_or_default(),
                    user_id: raw.user_id.unwrap_or_default(),
                    scopes: raw.scopes.unwrap_or_default(),
                    expires_in: raw.expires_in.unwrap_or(0),
                })
            }
            401 => Err(AuthError::InvalidToken),
            status => Err(AuthError::Status {
                status,
                message: error_message(&body),
            }),
        }
    }

    /// Revokes `token`. An already invalid token is not an error.
    pub async fn revoke(&self, token: &Secret) -> Result<(), AuthError> {
        let (status, body) = self
            .post(
                "revoke",
                &[("client_id", &self.client_id), ("token", token.expose())],
            )
            .await?;
        match status {
            200..=299 | 400 => Ok(()),
            status => Err(AuthError::Status {
                status,
                message: error_message(&body),
            }),
        }
    }
}
