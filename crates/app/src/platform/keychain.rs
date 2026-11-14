//! The Twitch login in the system credential store (macOS Keychain), through `keyring`.
//!
//! One generic-password item holds the whole login as one JSON record
//! (`gpui_chat_twitch::auth::encode_token`: access token, refresh token, expiry, scopes, login,
//! user id), so the two tokens are always replaced together.
//!
//! * service: [`gpui_chat_core::APP_ID`], or `$GPUI_CHAT_KEYCHAIN_SERVICE` when set (test runs that
//!   must not touch the real login);
//! * account: `twitch`.

use gpui_chat_core::APP_ID;
use gpui_chat_twitch::auth::{StoreError, StoredToken, TokenStore, decode_token, encode_token};

/// Overrides the keychain service name (the item's "Where" in Keychain Access).
pub const SERVICE_ENV: &str = "GPUI_CHAT_KEYCHAIN_SERVICE";
/// The keychain account name of the item.
pub const ACCOUNT: &str = "twitch";

/// The login in the keychain item `service` / [`ACCOUNT`].
pub struct KeychainTokenStore {
    service: String,
}

impl KeychainTokenStore {
    /// The store for this app: [`APP_ID`] unless `$GPUI_CHAT_KEYCHAIN_SERVICE` names another.
    pub fn from_env() -> Self {
        let service = std::env::var(SERVICE_ENV)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| APP_ID.to_owned());
        Self { service }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    fn entry(&self) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(&self.service, ACCOUNT).map_err(map_error)
    }
}

/// `keyring` errors as store errors. Never formats a payload: some variants carry the stored
/// bytes.
fn map_error(error: keyring::Error) -> StoreError {
    match error {
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => {
            StoreError::Corrupt("the keychain item is not a login record".to_owned())
        }
        keyring::Error::NoStorageAccess(platform) => {
            StoreError::Unavailable(format!("no access to the keychain: {platform}"))
        }
        keyring::Error::PlatformFailure(platform) => {
            StoreError::Unavailable(format!("keychain error: {platform}"))
        }
        keyring::Error::NoDefaultStore => {
            StoreError::Unavailable("no system credential store".to_owned())
        }
        other => StoreError::Unavailable(other.to_string()),
    }
}

impl TokenStore for KeychainTokenStore {
    fn load(&self) -> Result<Option<StoredToken>, StoreError> {
        match self.entry()?.get_password() {
            Ok(value) => decode_token(&value).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(map_error(error)),
        }
    }

    fn save(&self, token: &StoredToken) -> Result<(), StoreError> {
        self.entry()?
            .set_password(&encode_token(token))
            .map_err(map_error)
    }

    fn clear(&self) -> Result<(), StoreError> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(map_error(error)),
        }
    }
}
