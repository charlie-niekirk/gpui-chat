//! The Twitch account in the app (PR 3.1): the login state the title bar shows, the device-code
//! sign-in the login sheet drives, and logout.
//!
//! The logic lives in `gpui_chat_twitch::auth::Auth` on the backend runtime; [`AccountHub`] is the
//! UI's mirror of it:
//!
//! * at start it restores the stored login (the system keychain, `platform::keychain`) and then
//!   validates it hourly, as Twitch requires;
//! * every [`AuthState`] change (login, logout, a refresh rejected → logged out with a notice) is
//!   mirrored into the entity, which notifies its observers (the shell);
//! * [`AccountHub::begin_sign_in`] runs the device flow; its progress ([`SignIn`]) is what the login
//!   sheet shows. Dropping or cancelling it aborts the backend task.
//!
//! IRC stays anonymous in this PR; PR 3.2 hands the token to the chat client.

use std::sync::Arc;
use std::time::Duration;

use gpui_chat_twitch::auth::{
    AuthError, AuthState, DeviceCode, DevicePolling, OAuthClient, client_id,
};
use gpui_chat_twitch::{Auth, HelixClient};
use gpui_kit::{App, AppContext as _, Context, Entity, Task};
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::platform::keychain::KeychainTokenStore;
use crate::runtime;

/// Twitch asks apps to validate their tokens at least hourly.
const VALIDATION_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Where a device-code sign-in stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    /// Asking Twitch for a code.
    Requesting,
    /// Waiting for the user to approve `code` at its link.
    Waiting(DeviceCode),
    /// Logged in; the sheet closes.
    Done,
    /// Ended without a login; the sheet says why and offers to try again.
    Failed(String),
}

impl SignIn {
    fn is_finished(&self) -> bool {
        matches!(self, Self::Done | Self::Failed(_))
    }
}

/// What the user reads when a sign-in fails.
fn failure_message(error: &AuthError) -> String {
    match error {
        AuthError::DeviceCodeExpired => "The code expired before it was approved.".to_owned(),
        AuthError::AccessDenied => "The authorization was declined on Twitch.".to_owned(),
        AuthError::Network(_) => format!("Could not reach Twitch ({error})."),
        other => format!("Login failed: {other}"),
    }
}

/// The account as the UI sees it. One per app, owned by the shell.
pub struct AccountHub {
    auth: Auth,
    helix: HelixClient,
    state: AuthState,
    /// The notice of the current logged-out state was dismissed.
    notice_dismissed: bool,
    sign_in: Option<SignIn>,
    /// Numbers sign-in attempts so a stale one cannot overwrite a newer one's progress.
    attempt: u64,
    sign_in_abort: Option<AbortHandle>,
    _sign_in_task: Option<Task<()>>,
    _state_task: Task<()>,
}

impl AccountHub {
    /// Builds the login owner (keychain store, client id from the environment) and the Helix client
    /// on the backend runtime. `None` (logged out for good, with a warning) if an HTTP client cannot
    /// be built.
    pub fn create(cx: &App) -> Option<(Auth, HelixClient)> {
        let built = runtime::enter(cx, || {
            let oauth = OAuthClient::new(client_id())?;
            let store = KeychainTokenStore::from_env();
            tracing::debug!(service = store.service(), "Twitch login keychain item");
            let auth = Auth::new(oauth, Arc::new(store));
            let helix = HelixClient::new(auth.clone()).map_err(|error| match error {
                gpui_chat_twitch::HelixError::Auth(error) => error,
                gpui_chat_twitch::HelixError::Network(message) => AuthError::Network(message),
            })?;
            Ok::<_, AuthError>((auth, helix))
        });
        match built {
            Ok(built) => Some(built),
            Err(error) => {
                tracing::warn!(%error, "Twitch login unavailable");
                None
            }
        }
    }

    /// The hub over `auth`: restores the stored login and starts the hourly validation on the
    /// backend runtime, and mirrors every state change into the entity.
    pub fn start(auth: Auth, helix: HelixClient, cx: &mut App) -> Entity<Self> {
        let mut states = auth.subscribe();
        let restoring = auth.clone();
        // Ends with the runtime on quit.
        drop(runtime::spawn_backend(cx, async move {
            restoring.restore().await;
            restoring.run_validation(VALIDATION_INTERVAL).await;
        }));
        cx.new(|cx: &mut Context<Self>| {
            let state_task = cx.spawn(async move |this, cx| {
                while states.changed().await.is_ok() {
                    let state = states.borrow_and_update().clone();
                    if this.update(cx, |hub, cx| hub.set_state(state, cx)).is_err() {
                        break;
                    }
                }
            });
            Self {
                state: auth.state(),
                auth,
                helix,
                notice_dismissed: false,
                sign_in: None,
                attempt: 0,
                sign_in_abort: None,
                _sign_in_task: None,
                _state_task: state_task,
            }
        })
    }

    pub fn helix(&self) -> &HelixClient {
        &self.helix
    }

    pub fn state(&self) -> &AuthState {
        &self.state
    }

    /// Why the user was logged out, until they dismiss it or the state changes.
    pub fn notice(&self) -> Option<&str> {
        match &self.state {
            AuthState::LoggedOut {
                notice: Some(notice),
            } if !self.notice_dismissed => Some(notice),
            _ => None,
        }
    }

    pub fn dismiss_notice(&mut self, cx: &mut Context<Self>) {
        self.notice_dismissed = true;
        cx.notify();
    }

    fn set_state(&mut self, state: AuthState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            self.notice_dismissed = false;
            cx.notify();
        }
    }

    /// The sign-in in progress (or just finished), for the login sheet.
    pub fn sign_in(&self) -> Option<&SignIn> {
        self.sign_in.as_ref()
    }

    /// Starts a device-code sign-in (cancelling one still running).
    pub fn begin_sign_in(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in(cx);
        self.attempt += 1;
        let attempt = self.attempt;
        let (progress, mut updates) = watch::channel(SignIn::Requesting);
        let auth = self.auth.clone();
        let flow = runtime::spawn_backend(cx, async move {
            let result = async {
                let code = auth.request_device_code().await?;
                progress.send_replace(SignIn::Waiting(code.clone()));
                auth.complete_device_login(&code, DevicePolling::default())
                    .await
            }
            .await;
            progress.send_replace(match result {
                Ok(_) => SignIn::Done,
                Err(error) => {
                    tracing::info!(%error, "Twitch sign-in ended without a login");
                    SignIn::Failed(failure_message(&error))
                }
            });
        });
        self.sign_in_abort = Some(flow.abort_handle());
        self.sign_in = Some(SignIn::Requesting);
        self._sign_in_task = Some(cx.spawn(async move |this, cx| {
            loop {
                // `Err` once the backend task ended (finished or aborted) and its last value was seen.
                let next = match updates.changed().await {
                    Ok(()) => Some(updates.borrow_and_update().clone()),
                    Err(_) => None,
                };
                let more = next.is_some();
                let alive = this.update(cx, |hub, cx| {
                    if hub.attempt != attempt {
                        return false;
                    }
                    match next {
                        Some(progress) => hub.sign_in = Some(progress),
                        // Aborted half way: nothing to show any more.
                        None if !hub.sign_in.as_ref().is_some_and(SignIn::is_finished) => {
                            hub.sign_in = None;
                        }
                        None => {}
                    }
                    cx.notify();
                    true
                });
                if !more || !matches!(alive, Ok(true)) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Stops a running sign-in (the sheet was closed) and forgets its progress.
    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if let Some(abort) = self.sign_in_abort.take() {
            abort.abort();
        }
        self._sign_in_task = None;
        if self.sign_in.take().is_some() {
            cx.notify();
        }
    }

    /// Logs out: the keychain item is removed and the token revoked (in the background).
    pub fn log_out(&mut self, cx: &mut Context<Self>) {
        self.cancel_sign_in(cx);
        let auth = self.auth.clone();
        drop(runtime::spawn_backend(
            cx,
            async move { auth.logout().await },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_messages_say_what_to_do() {
        assert!(failure_message(&AuthError::DeviceCodeExpired).contains("expired"));
        assert!(failure_message(&AuthError::AccessDenied).contains("declined"));
        assert!(failure_message(&AuthError::Network("dns".into())).contains("reach Twitch"));
    }
}
