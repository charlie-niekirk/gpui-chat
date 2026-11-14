//! Where the login is kept between runs.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use thiserror::Error;

use super::StoredToken;

/// Why the token store failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoreError {
    /// The backing store (keychain, secret service) refused or is not there.
    #[error("token store unavailable: {0}")]
    Unavailable(String),
    /// Something is stored but it is not a login this build can read.
    #[error("stored login is unreadable: {0}")]
    Corrupt(String),
}

/// Persistence for the one login. Implementations are blocking (the system keychain is);
/// [`super::Auth`] calls them off the async threads.
///
/// [`TokenStore::save`] must replace the whole record at once: the access and refresh tokens are
/// only ever written together.
pub trait TokenStore: Send + Sync + 'static {
    /// The stored login, `None` when there is none.
    fn load(&self) -> Result<Option<StoredToken>, StoreError>;
    /// Replaces the stored login with `token`.
    fn save(&self, token: &StoredToken) -> Result<(), StoreError>;
    /// Removes the stored login; removing nothing is not an error.
    fn clear(&self) -> Result<(), StoreError>;
}

/// The record as one string, for stores that keep a single secret value (the keychain).
pub fn encode_token(token: &StoredToken) -> String {
    // A struct of strings, numbers and a list cannot fail to serialise.
    serde_json::to_string(token).unwrap_or_default()
}

/// The inverse of [`encode_token`].
pub fn decode_token(value: &str) -> Result<StoredToken, StoreError> {
    serde_json::from_str(value).map_err(|error| StoreError::Corrupt(error.to_string()))
}

/// An in-memory store: tests, Linux CI, and anything that must not touch the system keychain.
#[derive(Default)]
pub struct MemoryTokenStore {
    token: Mutex<Option<StoredToken>>,
    saves: AtomicUsize,
    fail: AtomicBool,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that already holds `token`.
    pub fn with(token: StoredToken) -> Self {
        let store = Self::new();
        *store.slot() = Some(token);
        store
    }

    /// What is stored now.
    pub fn get(&self) -> Option<StoredToken> {
        self.slot().clone()
    }

    /// How many times [`TokenStore::save`] succeeded.
    pub fn saves(&self) -> usize {
        self.saves.load(Ordering::SeqCst)
    }

    /// Makes every following call fail with [`StoreError::Unavailable`] (or work again).
    pub fn set_failing(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<StoredToken>> {
        self.token.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn check(&self) -> Result<(), StoreError> {
        if self.fail.load(Ordering::SeqCst) {
            Err(StoreError::Unavailable(
                "memory store set to fail".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self) -> Result<Option<StoredToken>, StoreError> {
        self.check()?;
        Ok(self.get())
    }

    fn save(&self, token: &StoredToken) -> Result<(), StoreError> {
        self.check()?;
        *self.slot() = Some(token.clone());
        self.saves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn clear(&self) -> Result<(), StoreError> {
        self.check()?;
        *self.slot() = None;
        Ok(())
    }
}
