//! [`Auth`]: the one owner of the login at runtime.
//!
//! # One refresh at a time
//!
//! Twitch refresh tokens are one-use: two refreshes racing with the same refresh token leave one of
//! them holding a token Twitch already rotated away, which ends the login. So the tokens sit behind
//! one async mutex together with a *generation* that bumps whenever they change:
//!
//! * a caller that needs a refresh (the token is about to expire, or a request got a 401) passes the
//!   generation of the token it saw;
//! * the refresh runs under the mutex, in a task of its own (so a caller giving up half way cannot
//!   abort it after Twitch rotated the tokens but before they were stored);
//! * whoever gets the mutex next and finds the generation moved on simply takes the new token.
//!
//! However many requests fail at once, one refresh request goes out and everyone gets its result.
//!
//! # Atomic persistence
//!
//! A refreshed pair is written to the [`TokenStore`] as one record *before* the mutex is released,
//! so nothing uses a new access token whose refresh token is not stored yet.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, watch};

use super::oauth::{AuthError, DeviceCode, OAuthClient, PollOutcome, TokenResponse, Validation};
use super::store::{StoreError, TokenStore};
use super::token::{StoredToken, missing_scopes};
use super::{SCOPES, Secret, unix_now};

/// Refresh this long before the access token expires.
const REFRESH_MARGIN_SECS: u64 = 5 * 60;

/// The notice when Twitch ended the login (the refresh was rejected).
pub const SESSION_EXPIRED_NOTICE: &str = "Your Twitch login expired or was revoked. Log in again.";

/// The logged-in account as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub login: String,
    pub user_id: String,
    /// Scopes the app asks for that this login was not granted (the scope list grew since).
    pub missing_scopes: Vec<String>,
}

impl Account {
    /// The login works but predates scopes the app now needs: ask the user to log in again.
    pub fn needs_relogin(&self) -> bool {
        !self.missing_scopes.is_empty()
    }
}

/// What the UI shows about the login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// The stored login has not been read yet.
    Restoring,
    /// `notice` says why, when the user did not log out themselves.
    LoggedOut {
        notice: Option<String>,
    },
    LoggedIn(Account),
}

impl AuthState {
    pub fn account(&self) -> Option<&Account> {
        match self {
            Self::LoggedIn(account) => Some(account),
            _ => None,
        }
    }
}

/// An access token and the generation it belongs to (pass it back to
/// [`Auth::refresh_after_unauthorized`] when Twitch rejects the token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenLease {
    pub token: Secret,
    pub user_id: String,
    /// The login of the account the token belongs to.
    pub login: String,
    pub generation: u64,
}

/// How the device-flow poller turns Twitch's intervals into waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevicePolling {
    /// The length of one of Twitch's "seconds" (`interval`, `expires_in`). One second, except in
    /// tests.
    pub unit: Duration,
    /// Units added to the interval on `slow_down` (RFC 8628: 5).
    pub slow_down_step: u64,
}

impl Default for DevicePolling {
    fn default() -> Self {
        Self {
            unit: Duration::from_secs(1),
            slow_down_step: 5,
        }
    }
}

#[derive(Default)]
struct Slot {
    token: Option<StoredToken>,
    generation: u64,
}

impl Slot {
    fn lease(&self) -> Option<TokenLease> {
        self.token.as_ref().map(|token| TokenLease {
            token: token.access_token.clone(),
            user_id: token.user_id.clone(),
            login: token.login.clone(),
            generation: self.generation,
        })
    }

    fn replace(&mut self, token: Option<StoredToken>) {
        self.token = token;
        self.generation += 1;
    }
}

struct Inner {
    oauth: OAuthClient,
    store: Arc<dyn TokenStore>,
    scopes: Vec<&'static str>,
    slot: Mutex<Slot>,
    state: watch::Sender<AuthState>,
    refresh_margin_secs: u64,
    refreshes: AtomicUsize,
}

/// The login: device flow, tokens, refresh, validation, logout. Cheap to clone; clones share
/// everything. Needs a tokio runtime (it spawns the refresh and calls the store on the blocking
/// pool).
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("oauth", &self.inner.oauth)
            .field("state", &*self.inner.state.borrow())
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// An owner asking for [`SCOPES`], starting in [`AuthState::Restoring`]; call
    /// [`Auth::restore`] next.
    pub fn new(oauth: OAuthClient, store: Arc<dyn TokenStore>) -> Self {
        Self::with_scopes(oauth, store, SCOPES)
    }

    /// [`Auth::new`] with another scope list (tests).
    pub fn with_scopes(
        oauth: OAuthClient,
        store: Arc<dyn TokenStore>,
        scopes: &[&'static str],
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                oauth,
                store,
                scopes: scopes.to_vec(),
                slot: Mutex::new(Slot::default()),
                state: watch::channel(AuthState::Restoring).0,
                refresh_margin_secs: REFRESH_MARGIN_SECS,
                refreshes: AtomicUsize::new(0),
            }),
        }
    }

    pub fn client_id(&self) -> &str {
        self.inner.oauth.client_id()
    }

    /// The current state.
    pub fn state(&self) -> AuthState {
        self.inner.state.borrow().clone()
    }

    /// A receiver that sees every state change.
    pub fn subscribe(&self) -> watch::Receiver<AuthState> {
        self.inner.state.subscribe()
    }

    pub fn is_logged_in(&self) -> bool {
        matches!(*self.inner.state.borrow(), AuthState::LoggedIn(_))
    }

    /// How many refresh requests were sent so far (diagnostics and tests).
    pub fn refresh_count(&self) -> usize {
        self.inner.refreshes.load(Ordering::SeqCst)
    }

    /// Reads the stored login and validates it (Twitch asks for a validation at start).
    ///
    /// A stored login shows as logged in at once; a network failure during the validation keeps it
    /// (offline start), a rejected token is refreshed, and a rejected refresh logs out with a
    /// notice.
    ///
    /// If the login changed while the store was being read (a device login finished first, or a
    /// logout), what was read is stale and is ignored.
    pub async fn restore(&self) -> AuthState {
        let start = self.inner.slot.lock().await.generation;
        let loaded = self.store_call(|store| store.load()).await;
        let mut slot = self.inner.slot.lock().await;
        if slot.generation != start {
            tracing::debug!("the login changed while it was being restored; keeping the newer one");
            drop(slot);
            return self.state();
        }
        match loaded {
            Ok(None) => {
                drop(slot);
                self.publish(AuthState::LoggedOut { notice: None });
            }
            Ok(Some(token)) => {
                let account = self.account_of(&token);
                slot.replace(Some(token));
                drop(slot);
                self.publish(AuthState::LoggedIn(account));
                if let Err(error) = self.validate_now().await {
                    tracing::warn!(%error, "validating the stored Twitch login failed");
                }
            }
            Err(error @ StoreError::Corrupt(_)) => {
                drop(slot);
                tracing::warn!(%error, "discarding the stored Twitch login");
                if let Err(error) = self.store_call(|store| store.clear()).await {
                    tracing::warn!(%error, "could not remove the unreadable Twitch login");
                }
                self.publish(AuthState::LoggedOut {
                    notice: Some("The saved Twitch login could not be read. Log in again.".into()),
                });
            }
            Err(error) => {
                drop(slot);
                tracing::warn!(%error, "cannot read the stored Twitch login");
                self.publish(AuthState::LoggedOut {
                    notice: Some(format!("Could not read the saved Twitch login: {error}")),
                });
            }
        }
        self.state()
    }

    /// An access token for a request, refreshed first when it is about to expire.
    pub async fn access_token(&self) -> Result<TokenLease, AuthError> {
        let (generation, expired) = {
            let slot = self.inner.slot.lock().await;
            let Some(token) = &slot.token else {
                return Err(AuthError::NotLoggedIn);
            };
            let now = unix_now();
            if !token.expires_within(now, self.inner.refresh_margin_secs) {
                return slot.lease().ok_or(AuthError::NotLoggedIn);
            }
            (slot.generation, token.expires_within(now, 0))
        };
        match self.refresh(generation).await {
            // Still valid for a few minutes: a failed refresh is retried by the next request.
            Err(error) if error.is_transient() && !expired => {
                tracing::warn!(%error, "early token refresh failed; using the current token");
                self.inner
                    .slot
                    .lock()
                    .await
                    .lease()
                    .ok_or(AuthError::NotLoggedIn)
            }
            result => result,
        }
    }

    /// Twitch rejected the token of `generation` (a 401): refresh it, unless someone already did,
    /// and return the token to retry with. Concurrent callers share one refresh.
    pub async fn refresh_after_unauthorized(
        &self,
        generation: u64,
    ) -> Result<TokenLease, AuthError> {
        self.refresh(generation).await
    }

    async fn refresh(&self, seen_generation: u64) -> Result<TokenLease, AuthError> {
        let auth = self.clone();
        // A task of its own: if the caller is dropped mid-refresh, the rotated tokens still get
        // stored.
        tokio::spawn(async move { auth.refresh_locked(seen_generation).await })
            .await
            .map_err(|error| AuthError::Interrupted(error.to_string()))?
    }

    async fn refresh_locked(&self, seen_generation: u64) -> Result<TokenLease, AuthError> {
        let mut slot = self.inner.slot.lock().await;
        let Some(current) = slot.token.clone() else {
            return Err(if slot.generation == seen_generation {
                AuthError::NotLoggedIn
            } else {
                AuthError::SessionEnded
            });
        };
        if slot.generation != seen_generation {
            // Somebody refreshed (or logged in again) while we waited for the lock.
            return slot.lease().ok_or(AuthError::NotLoggedIn);
        }
        self.inner.refreshes.fetch_add(1, Ordering::SeqCst);
        match self.inner.oauth.refresh(&current.refresh_token).await {
            Ok(response) => {
                let token = current.refreshed(response, unix_now());
                // Both tokens are stored before anyone can use the new access token.
                let saved = token.clone();
                if let Err(error) = self.store_call(move |store| store.save(&saved)).await {
                    tracing::error!(%error, "could not store the refreshed Twitch login");
                }
                slot.replace(Some(token));
                let account = slot.token.as_ref().map(|token| self.account_of(token));
                if let Some(account) = account {
                    self.publish(AuthState::LoggedIn(account));
                }
                slot.lease().ok_or(AuthError::NotLoggedIn)
            }
            Err(AuthError::RefreshRejected(message)) => {
                tracing::warn!(%message, "Twitch rejected the refresh token; logging out");
                slot.replace(None);
                self.forget_stored().await;
                self.publish(AuthState::LoggedOut {
                    notice: Some(SESSION_EXPIRED_NOTICE.to_owned()),
                });
                Err(AuthError::SessionEnded)
            }
            Err(error) => Err(error),
        }
    }

    /// Validates the current token: updates the account on success, refreshes on a 401, and logs
    /// out (with a notice) if the refresh is rejected or the new token is refused too.
    pub async fn validate_now(&self) -> Result<(), AuthError> {
        let lease = self
            .inner
            .slot
            .lock()
            .await
            .lease()
            .ok_or(AuthError::NotLoggedIn)?;
        match self.inner.oauth.validate(&lease.token).await {
            Ok(validation) => {
                self.apply_validation(lease.generation, validation).await;
                Ok(())
            }
            Err(AuthError::InvalidToken) => {
                let lease = self.refresh(lease.generation).await?;
                match self.inner.oauth.validate(&lease.token).await {
                    Ok(validation) => {
                        self.apply_validation(lease.generation, validation).await;
                        Ok(())
                    }
                    Err(AuthError::InvalidToken) => {
                        self.end_session(lease.generation).await;
                        Err(AuthError::SessionEnded)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Validates every `every` (Twitch requires hourly) while logged in; runs until dropped.
    pub async fn run_validation(self, every: Duration) {
        loop {
            tokio::time::sleep(every).await;
            if !self.is_logged_in() {
                continue;
            }
            match self.validate_now().await {
                Ok(()) | Err(AuthError::NotLoggedIn) => {}
                Err(error) => tracing::warn!(%error, "periodic Twitch token validation failed"),
            }
        }
    }

    async fn apply_validation(&self, generation: u64, validation: Validation) {
        let mut slot = self.inner.slot.lock().await;
        if slot.generation != generation {
            return;
        }
        let Some(token) = slot.token.as_mut() else {
            return;
        };
        if validation.expires_in > 0 {
            token.expires_at = unix_now().saturating_add(validation.expires_in);
        }
        let mut changed = false;
        if !validation.login.is_empty() && token.login != validation.login {
            token.login = validation.login;
            changed = true;
        }
        if !validation.user_id.is_empty() && token.user_id != validation.user_id {
            token.user_id = validation.user_id;
            changed = true;
        }
        if token.scopes != validation.scopes {
            token.scopes = validation.scopes;
            changed = true;
        }
        let token = token.clone();
        if changed {
            let saved = token.clone();
            if let Err(error) = self.store_call(move |store| store.save(&saved)).await {
                tracing::warn!(%error, "could not store the updated Twitch login");
            }
        }
        self.publish(AuthState::LoggedIn(self.account_of(&token)));
    }

    /// Ends the login of `generation` after Twitch refused it: forget it and say why.
    async fn end_session(&self, generation: u64) {
        let mut slot = self.inner.slot.lock().await;
        if slot.generation != generation {
            return;
        }
        slot.replace(None);
        self.forget_stored().await;
        self.publish(AuthState::LoggedOut {
            notice: Some(SESSION_EXPIRED_NOTICE.to_owned()),
        });
    }

    /// Starts a device login: show the result's code and link to the user, then call
    /// [`Auth::complete_device_login`].
    pub async fn request_device_code(&self) -> Result<DeviceCode, AuthError> {
        self.inner
            .oauth
            .request_device_code(&self.inner.scopes)
            .await
    }

    /// Polls until the user approves `code` (honouring `interval` and `slow_down`), then stores the
    /// login and returns the account. Fails with [`AuthError::DeviceCodeExpired`] or
    /// [`AuthError::AccessDenied`].
    ///
    /// Drop the future to cancel. Cancelling only stops the *polling*: once Twitch handed out a
    /// token, validating and installing it runs in a task of its own, so the keychain, the
    /// in-memory login and the published state never disagree (and a token is never dropped
    /// without being revoked).
    pub async fn complete_device_login(
        &self,
        code: &DeviceCode,
        polling: DevicePolling,
    ) -> Result<Account, AuthError> {
        let oauth = &self.inner.oauth;
        let deadline = tokio::time::Instant::now() + polling.unit * saturate(code.expires_in);
        let mut interval = code.interval.max(1);
        let response = loop {
            tokio::time::sleep(polling.unit * saturate(interval)).await;
            if tokio::time::Instant::now() >= deadline {
                return Err(AuthError::DeviceCodeExpired);
            }
            match oauth.poll_device_code(code, &self.inner.scopes).await {
                Ok(PollOutcome::Token(response)) => break response,
                Ok(PollOutcome::Pending) => {}
                Ok(PollOutcome::SlowDown) => interval += polling.slow_down_step,
                Ok(PollOutcome::Expired) => return Err(AuthError::DeviceCodeExpired),
                Ok(PollOutcome::Denied) => return Err(AuthError::AccessDenied),
                Err(error) if error.is_transient() => {
                    tracing::warn!(%error, "device login poll failed; trying again");
                }
                Err(error) => return Err(error),
            }
        };
        let auth = self.clone();
        tokio::spawn(async move { auth.adopt_device_token(response).await })
            .await
            .map_err(|error| AuthError::Interrupted(error.to_string()))?
    }

    /// Validates a token the device flow just produced (whose is it?) and installs it. A token that
    /// cannot be validated is revoked rather than dropped.
    async fn adopt_device_token(&self, response: TokenResponse) -> Result<Account, AuthError> {
        let validation = match self.inner.oauth.validate(&response.access_token).await {
            Ok(validation) => validation,
            Err(error) => {
                self.revoke_in_background(response.access_token);
                return Err(error);
            }
        };
        let mut token =
            StoredToken::from_response(response, validation.login, validation.user_id, unix_now());
        if token.scopes.is_empty() {
            token.scopes = validation.scopes;
        }
        Ok(self.install(token).await)
    }

    /// Makes `token` the login: stored, then in use. A previous, different login is revoked.
    async fn install(&self, token: StoredToken) -> Account {
        let account = self.account_of(&token);
        let previous = {
            let mut slot = self.inner.slot.lock().await;
            let saved = token.clone();
            if let Err(error) = self.store_call(move |store| store.save(&saved)).await {
                // Logged in for this run; the next start will not find the login.
                tracing::error!(%error, "could not store the Twitch login");
            }
            let previous = slot.token.take();
            slot.replace(Some(token));
            previous
        };
        self.publish(AuthState::LoggedIn(account.clone()));
        if let Some(previous) = previous {
            self.revoke_in_background(previous.access_token);
        }
        account
    }

    /// Logs out: forgets the stored login and revokes the token (best effort).
    pub async fn logout(&self) {
        let previous = {
            let mut slot = self.inner.slot.lock().await;
            let previous = slot.token.take();
            slot.replace(None);
            previous
        };
        self.forget_stored().await;
        self.publish(AuthState::LoggedOut { notice: None });
        if let Some(previous) = previous
            && let Err(error) = self.inner.oauth.revoke(&previous.access_token).await
        {
            tracing::warn!(%error, "revoking the Twitch token failed");
        }
    }

    fn revoke_in_background(&self, token: Secret) {
        let oauth = self.inner.oauth.clone();
        drop(tokio::spawn(async move {
            if let Err(error) = oauth.revoke(&token).await {
                tracing::warn!(%error, "revoking the previous Twitch token failed");
            }
        }));
    }

    async fn forget_stored(&self) {
        if let Err(error) = self.store_call(|store| store.clear()).await {
            tracing::warn!(%error, "could not remove the stored Twitch login");
        }
    }

    fn account_of(&self, token: &StoredToken) -> Account {
        Account {
            login: token.login.clone(),
            user_id: token.user_id.clone(),
            missing_scopes: missing_scopes(&token.scopes, &self.inner.scopes)
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }
    }

    fn publish(&self, state: AuthState) {
        self.inner.state.send_if_modified(|current| {
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        });
    }

    /// Runs a (blocking) store call on the blocking pool.
    async fn store_call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn TokenStore) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let store = self.inner.store.clone();
        tokio::task::spawn_blocking(move || f(&*store))
            .await
            .map_err(|error| StoreError::Unavailable(error.to_string()))?
    }
}

fn saturate(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
