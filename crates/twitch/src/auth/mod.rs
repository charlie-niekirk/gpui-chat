//! Twitch login: the OAuth device flow, the token store seam and the one owner of the tokens.
//!
//! * [`OAuthClient`] talks to `id.twitch.tv` (device code, token, refresh, validate, revoke). Its
//!   base URL is injectable, so tests run against a local mock server.
//! * [`TokenStore`] is where the tokens live between runs: the app's is the system keychain (through
//!   `keyring`), tests and Linux CI use [`MemoryTokenStore`]. One [`StoredToken`] is written as one
//!   record, so the access and the refresh token always change together.
//! * [`Auth`] owns the tokens at runtime. It hands out access tokens, refreshes them (exactly one
//!   refresh at a time: Twitch refresh tokens are one-use), validates them at start and hourly
//!   ([`Auth::run_validation`]), and publishes an [`AuthState`] the UI watches.
//!
//! The client is a **public** client (no secret): the device flow and refreshes send only the
//! client id. [`client_id`] is [`DEFAULT_CLIENT_ID`] unless [`CLIENT_ID_ENV`] overrides it.
//!
//! Tokens never reach a log: [`Secret`] prints as `[redacted]`, and so does every type holding one.

mod manager;
mod oauth;
mod secret;
mod store;
mod token;

#[cfg(test)]
mod tests;

pub use manager::{Account, Auth, AuthState, DevicePolling, SESSION_EXPIRED_NOTICE, TokenLease};
pub use oauth::{
    AuthError, DeviceCode, ID_BASE_URL, OAuthClient, PollOutcome, TokenResponse, Validation,
};
pub(crate) use oauth::{describe, user_agent};
pub use secret::Secret;
pub use store::{MemoryTokenStore, StoreError, TokenStore, decode_token, encode_token};
pub use token::{StoredToken, missing_scopes};

/// The Client ID of the app registered at dev.twitch.tv (Client Type: Public, redirect
/// `http://localhost`). Public clients have no secret, so this is not one.
pub const DEFAULT_CLIENT_ID: &str = "s70un5nph0b9z6dvxgo1teeofij58h";

/// Environment variable that overrides [`DEFAULT_CLIENT_ID`] (for a fork with its own app).
pub const CLIENT_ID_ENV: &str = "GPUI_CHAT_TWITCH_CLIENT_ID";

/// Every scope the app asks for, in one place. A stored login granted fewer of them (the list grew
/// in a newer build) still works for what it covers, but the UI asks the user to log in again.
///
/// * `chat:read`, `chat:edit`: authenticated IRC (PR 3.2).
/// * `user:read:chat`, `user:write:chat`: Helix chat, Send Chat Message (PR 3.2).
/// * `user:read:emotes`: the user's own Twitch emotes (PR 3.3).
/// * `user:read:follows`: the followed-live sidebar (PR 3.5).
pub const SCOPES: &[&str] = &[
    "chat:read",
    "chat:edit",
    "user:read:chat",
    "user:write:chat",
    "user:read:emotes",
    "user:read:follows",
];

/// The client id to use: `$GPUI_CHAT_TWITCH_CLIENT_ID` when set and not blank, else
/// [`DEFAULT_CLIENT_ID`].
pub fn client_id() -> String {
    client_id_from(|name| std::env::var(name).ok())
}

/// [`client_id`] with the environment injected (tests).
pub fn client_id_from(get: impl Fn(&str) -> Option<String>) -> String {
    get(CLIENT_ID_ENV)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned())
}

/// Seconds since the Unix epoch (0 if the clock is before it).
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
