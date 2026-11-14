//! The persisted login: both tokens and who they belong to, written as one record.

use serde::{Deserialize, Serialize};

use super::Secret;
use super::oauth::TokenResponse;

/// Everything a login needs to survive a restart. The token store writes it as **one** record, so
/// the access token and the (one-use) refresh token can never be persisted apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: Secret,
    pub refresh_token: Secret,
    /// When the access token expires, in Unix seconds; 0 when unknown.
    #[serde(default)]
    pub expires_at: u64,
    /// The scopes Twitch granted.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Lowercase login of the account.
    pub login: String,
    /// Twitch user id of the account.
    pub user_id: String,
}

impl StoredToken {
    /// The record for a fresh token (device flow or refresh), for the account `login` / `user_id`.
    pub fn from_response(
        response: TokenResponse,
        login: impl Into<String>,
        user_id: impl Into<String>,
        now: u64,
    ) -> Self {
        Self {
            expires_at: expiry(now, response.expires_in),
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            scopes: response.scopes,
            login: login.into(),
            user_id: user_id.into(),
        }
    }

    /// This login with the tokens of a refresh. Twitch may omit the scopes of a refresh; the old
    /// ones then stay.
    pub fn refreshed(&self, response: TokenResponse, now: u64) -> Self {
        Self {
            expires_at: expiry(now, response.expires_in),
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            scopes: if response.scopes.is_empty() {
                self.scopes.clone()
            } else {
                response.scopes
            },
            login: self.login.clone(),
            user_id: self.user_id.clone(),
        }
    }

    /// `true` when the access token expires within `margin_secs` of `now` (or already has). An
    /// unknown expiry never counts as expiring: validation finds out instead.
    pub fn expires_within(&self, now: u64, margin_secs: u64) -> bool {
        self.expires_at != 0 && self.expires_at <= now.saturating_add(margin_secs)
    }
}

/// `expires_in` seconds after `now`; 0 (unknown) when Twitch did not say.
fn expiry(now: u64, expires_in: u64) -> u64 {
    if expires_in == 0 {
        0
    } else {
        now.saturating_add(expires_in)
    }
}

/// The scopes of `wanted` that `granted` lacks, in `wanted`'s order. Not empty means the login
/// predates a scope the app now needs, so the user should log in again.
pub fn missing_scopes<'a>(granted: &[String], wanted: &[&'a str]) -> Vec<&'a str> {
    wanted
        .iter()
        .copied()
        .filter(|scope| !granted.iter().any(|have| have == scope))
        .collect()
}
