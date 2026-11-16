//! What Helix says about users, for the user card (PR 5.4): a per-session cache.
//!
//! [`UserDirectory`] is one entity per app. A card asks for a login ([`UserDirectory::lookup`]);
//! the request runs on the backend runtime (never on the UI thread), the answer is kept for the
//! rest of the session and the entity notifies whoever draws it. Asking again for a login that is
//! loading or known does nothing, so a card may ask on every render. Helix needs a token, so while
//! logged out nothing is asked and the card says "Log in to see account details".
//!
//! * [`Profile::Missing`] (no such account) and [`Profile::Found`] are kept; a failure
//!   ([`Profile::Failed`]) stays until the card's Retry ([`UserDirectory::retry`]) or until the
//!   card is opened on that user again.
//! * Follow ages (`Get Channel Followers`) are cached per channel and user. They need a scope the
//!   app does not ask for at login and a moderator's token, so [`Follow::Unavailable`] is the usual
//!   answer: the card then leaves the line out.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_chat_twitch::{HelixClient, UserProfile, UserQuery};
use gpui_kit::{App, AppContext as _, Context, Entity, Global};
use tokio::sync::oneshot;

use crate::{account, runtime};

/// Where a lookup stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Profile {
    Loading,
    Found(Arc<UserProfile>),
    /// Twitch has no account with that login.
    Missing,
    /// The request failed; the sentence says how.
    Failed(String),
}

/// Where a follow lookup stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    Loading,
    /// When the user followed the channel (Unix seconds), or `None` when they do not.
    Since(Option<i64>),
    /// The login may not read follows (no scope, not a moderator) or Twitch refused.
    Unavailable,
}

pub struct UserDirectory {
    profiles: HashMap<String, Profile>,
    follows: HashMap<(String, String), Follow>,
}

struct Handle(Entity<UserDirectory>);

impl Global for Handle {}

/// Creates the directory (once, at startup).
pub fn init(cx: &mut App) {
    let directory = cx.new(|_| UserDirectory {
        profiles: HashMap::new(),
        follows: HashMap::new(),
    });
    cx.set_global(Handle(directory));
}

/// The app's directory, or `None` before [`init`] (tests that do not build one).
pub fn current(cx: &App) -> Option<Entity<UserDirectory>> {
    cx.try_global::<Handle>().map(|handle| handle.0.clone())
}

/// The Helix client of the logged-in user: `None` while logged out, restoring, or in an offline run.
pub fn helix(cx: &App) -> Option<HelixClient> {
    let hub = account::current(cx)?;
    let hub = hub.read(cx);
    hub.state().account()?;
    Some(hub.helix().clone())
}

/// Whether account details can be looked up at all (someone is logged in).
pub fn available(cx: &App) -> bool {
    helix(cx).is_some()
}

fn key(login: &str) -> String {
    login.to_ascii_lowercase()
}

impl UserDirectory {
    pub fn profile(&self, login: &str) -> Option<&Profile> {
        self.profiles.get(&key(login))
    }

    pub fn follow(&self, broadcaster_id: &str, user_id: &str) -> Option<Follow> {
        self.follows
            .get(&(broadcaster_id.to_owned(), user_id.to_owned()))
            .copied()
    }

    /// Looks `login` up unless it is known or already being asked for (or nobody is logged in, or
    /// it cannot be a login).
    pub fn lookup(&mut self, login: &str, cx: &mut Context<Self>) {
        if !gpui_chat_core::is_login(login) || self.profiles.contains_key(&key(login)) {
            return;
        }
        let Some(helix) = helix(cx) else {
            return;
        };
        self.profiles.insert(key(login), Profile::Loading);
        let owned = login.to_owned();
        let (done, outcome) = oneshot::channel();
        drop(runtime::spawn_backend(cx, async move {
            let result = helix.get_user(UserQuery::Login(&owned)).await;
            // The card may be gone by now: nobody to tell.
            let _ = done.send(result);
        }));
        let login = key(login);
        cx.spawn(async move |this, cx| {
            // The runtime is stopping: leave the entry; the app is going away.
            let Ok(result) = outcome.await else {
                return;
            };
            this.update(cx, |this, cx| {
                let profile = match result {
                    Ok(Some(profile)) => Profile::Found(Arc::new(profile)),
                    Ok(None) => Profile::Missing,
                    Err(error) => {
                        tracing::info!(%error, %login, "could not look the user up");
                        Profile::Failed(format!("Could not load account details ({error})."))
                    }
                };
                this.profiles.insert(login, profile);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Forgets a failed lookup of `login` and asks again.
    pub fn retry(&mut self, login: &str, cx: &mut Context<Self>) {
        if matches!(self.profiles.get(&key(login)), Some(Profile::Failed(_))) {
            self.profiles.remove(&key(login));
            self.lookup(login, cx);
        }
    }

    /// Looks up when `user_id` followed the channel `broadcaster_id` (unless known or being asked).
    /// The caller only asks for a moderator of that channel: Twitch refuses anyone else.
    pub fn lookup_follow(&mut self, broadcaster_id: &str, user_id: &str, cx: &mut Context<Self>) {
        let pair = (broadcaster_id.to_owned(), user_id.to_owned());
        if self.follows.contains_key(&pair) {
            return;
        }
        let Some(helix) = helix(cx) else {
            return;
        };
        self.follows.insert(pair.clone(), Follow::Loading);
        let (done, outcome) = oneshot::channel();
        let ids = pair.clone();
        drop(runtime::spawn_backend(cx, async move {
            let result = helix.get_follow_age(&ids.0, &ids.1).await;
            let _ = done.send(result);
        }));
        cx.spawn(async move |this, cx| {
            let Ok(result) = outcome.await else {
                return;
            };
            this.update(cx, |this, cx| {
                let follow = match result {
                    Ok(since) => Follow::Since(since),
                    Err(error) => {
                        // Usually a login without `moderator:read:followers`: not worth more than
                        // a line in the log, the card leaves the follow age out.
                        tracing::debug!(%error, "follow age unavailable");
                        Follow::Unavailable
                    }
                };
                this.follows.insert(pair, follow);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Puts a follow answer in the cache (tests).
    #[cfg(test)]
    pub fn seed_follow(&mut self, broadcaster_id: &str, user_id: &str, follow: Follow) {
        self.follows
            .insert((broadcaster_id.to_owned(), user_id.to_owned()), follow);
    }

    /// Puts a lookup state in the cache as if Helix had answered (tests).
    #[cfg(test)]
    pub fn seed_state(&mut self, login: &str, profile: Profile) {
        self.profiles.insert(key(login), profile);
    }
}
