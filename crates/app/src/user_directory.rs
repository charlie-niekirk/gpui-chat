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
//!   app does not ask for at login and a moderator's token: they are only asked for when the login
//!   was granted `moderator:read:followers` ([`follow_allowed`]), and any refusal is
//!   [`Follow::Unavailable`]: the card then leaves the line out.
//! * Everything is dropped when the account changes (log out, log in as someone else): follow ages
//!   are what that login may see, and a lookup still in flight for the old one is ignored.
//!
//! The state machine is [`Cache`] (no GPUI, no network, unit-tested); [`UserDirectory`] runs the requests.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_chat_twitch::users::FOLLOWERS_SCOPE;
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

/// The cache and its state machine: what is known, loading or failed, per account.
#[derive(Default)]
pub struct Cache {
    profiles: HashMap<String, Profile>,
    follows: HashMap<(String, String), Follow>,
    /// The user id of the login the entries belong to (`None`: logged out).
    account: Option<String>,
    /// Counts clears, so an answer to a request made for an earlier account is recognised.
    epoch: u64,
}

impl Cache {
    pub fn profile(&self, login: &str) -> Option<&Profile> {
        self.profiles.get(&key(login))
    }

    pub fn follow(&self, broadcaster_id: &str, user_id: &str) -> Option<Follow> {
        self.follows
            .get(&(broadcaster_id.to_owned(), user_id.to_owned()))
            .copied()
    }

    /// Tells the cache who is logged in. A different account (or none) drops every entry, loading
    /// ones included; whether anything was dropped.
    pub fn sync_account(&mut self, user_id: Option<&str>) -> bool {
        if self.account.as_deref() == user_id {
            return false;
        }
        self.account = user_id.map(str::to_owned);
        self.epoch += 1;
        let had = !self.profiles.is_empty() || !self.follows.is_empty();
        self.profiles.clear();
        self.follows.clear();
        had
    }

    /// Marks `login` as loading when it is a login nobody asked about yet; the epoch to answer
    /// with when a request should be made.
    pub fn begin_profile(&mut self, login: &str) -> Option<u64> {
        if !gpui_chat_core::is_login(login) || self.profiles.contains_key(&key(login)) {
            return None;
        }
        self.profiles.insert(key(login), Profile::Loading);
        Some(self.epoch)
    }

    /// Records the answer to a request begun at `epoch`; ignored when the account changed since.
    pub fn finish_profile(
        &mut self,
        login: &str,
        epoch: u64,
        result: Result<Option<UserProfile>, String>,
    ) {
        if epoch != self.epoch {
            return;
        }
        let profile = match result {
            Ok(Some(profile)) => Profile::Found(Arc::new(profile)),
            Ok(None) => Profile::Missing,
            Err(why) => Profile::Failed(why),
        };
        self.profiles.insert(key(login), profile);
    }

    /// Forgets a failed lookup and begins it again; `None` when `login` has not failed.
    pub fn retry(&mut self, login: &str) -> Option<u64> {
        if !matches!(self.profiles.get(&key(login)), Some(Profile::Failed(_))) {
            return None;
        }
        self.profiles.remove(&key(login));
        self.begin_profile(login)
    }

    pub fn begin_follow(&mut self, broadcaster_id: &str, user_id: &str) -> Option<u64> {
        let pair = (broadcaster_id.to_owned(), user_id.to_owned());
        if self.follows.contains_key(&pair) {
            return None;
        }
        self.follows.insert(pair, Follow::Loading);
        Some(self.epoch)
    }

    /// `since`: `Err` is any refusal.
    pub fn finish_follow(
        &mut self,
        broadcaster_id: &str,
        user_id: &str,
        epoch: u64,
        since: Result<Option<i64>, ()>,
    ) {
        if epoch != self.epoch {
            return;
        }
        let follow = since.map_or(Follow::Unavailable, Follow::Since);
        self.follows
            .insert((broadcaster_id.to_owned(), user_id.to_owned()), follow);
    }
}

pub struct UserDirectory {
    cache: Cache,
}

struct Handle(Entity<UserDirectory>);

impl Global for Handle {}

/// Creates the directory (once, at startup).
pub fn init(cx: &mut App) {
    let directory = cx.new(|_| UserDirectory {
        cache: Cache::default(),
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

/// The user id of the logged-in account.
fn account_id(cx: &App) -> Option<String> {
    let hub = account::current(cx)?;
    hub.read(cx)
        .state()
        .account()
        .map(|account| account.user_id.clone())
}

/// Whether the login may read follows: it was granted `moderator:read:followers`.
pub fn follow_allowed(cx: &App) -> bool {
    account::current(cx).is_some_and(|hub| {
        hub.read(cx)
            .state()
            .account()
            .is_some_and(|account| account.has_scope(FOLLOWERS_SCOPE))
    })
}

fn key(login: &str) -> String {
    login.to_ascii_lowercase()
}

impl UserDirectory {
    pub fn profile(&self, login: &str) -> Option<&Profile> {
        self.cache.profile(login)
    }

    pub fn follow(&self, broadcaster_id: &str, user_id: &str) -> Option<Follow> {
        self.cache.follow(broadcaster_id, user_id)
    }

    /// Drops what belongs to another account (see [`Cache::sync_account`]); the views that show
    /// it redraw.
    pub fn sync_account(&mut self, cx: &mut Context<Self>) {
        let id = account_id(cx);
        if self.cache.sync_account(id.as_deref()) {
            cx.notify();
        }
    }

    /// Looks `login` up unless it is known or already being asked for (or nobody is logged in, or
    /// it cannot be a login).
    pub fn lookup(&mut self, login: &str, cx: &mut Context<Self>) {
        let Some(helix) = helix(cx) else {
            return;
        };
        self.sync_account(cx);
        let Some(epoch) = self.cache.begin_profile(login) else {
            return;
        };
        self.start_profile(helix, login, epoch, cx);
    }

    fn start_profile(
        &mut self,
        helix: HelixClient,
        login: &str,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        let owned = login.to_owned();
        let (done, outcome) = oneshot::channel();
        drop(runtime::spawn_backend(cx, async move {
            let result = helix.get_user(UserQuery::Login(&owned)).await;
            // The card may be gone by now: nobody to tell.
            let _ = done.send(result);
        }));
        let login = login.to_owned();
        cx.spawn(async move |this, cx| {
            // The runtime is stopping: leave the entry; the app is going away.
            let Ok(result) = outcome.await else {
                return;
            };
            this.update(cx, |this, cx| {
                let result = result.map_err(|error| {
                    tracing::info!(%error, %login, "could not look the user up");
                    format!("Could not load account details ({error}).")
                });
                this.cache.finish_profile(&login, epoch, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Forgets a failed lookup of `login` and asks again.
    pub fn retry(&mut self, login: &str, cx: &mut Context<Self>) {
        let Some(epoch) = self.cache.retry(login) else {
            return;
        };
        match helix(cx) {
            Some(helix) => self.start_profile(helix, login, epoch, cx),
            // Logged out meanwhile: nothing to ask; the entry is gone, the card says so.
            None => {
                self.cache.sync_account(None);
                cx.notify();
            }
        }
    }

    /// Looks up when `user_id` followed the channel `broadcaster_id` (unless known or being asked).
    /// Only for a login that was granted the scope ([`follow_allowed`]) and is a moderator of that
    /// channel: Twitch refuses anyone else.
    pub fn lookup_follow(&mut self, broadcaster_id: &str, user_id: &str, cx: &mut Context<Self>) {
        if !follow_allowed(cx) {
            return;
        }
        let Some(helix) = helix(cx) else {
            return;
        };
        self.sync_account(cx);
        let Some(epoch) = self.cache.begin_follow(broadcaster_id, user_id) else {
            return;
        };
        let ids = (broadcaster_id.to_owned(), user_id.to_owned());
        let (done, outcome) = oneshot::channel();
        let asked = ids.clone();
        drop(runtime::spawn_backend(cx, async move {
            let result = helix.get_follow_age(&asked.0, &asked.1).await;
            let _ = done.send(result);
        }));
        cx.spawn(async move |this, cx| {
            let Ok(result) = outcome.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if let Err(error) = &result {
                    tracing::debug!(%error, "follow age unavailable");
                }
                this.cache
                    .finish_follow(&ids.0, &ids.1, epoch, result.map_err(|_| ()));
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
        self.cache
            .follows
            .insert((broadcaster_id.to_owned(), user_id.to_owned()), follow);
    }

    /// Puts a lookup state in the cache as if Helix had answered (tests).
    #[cfg(test)]
    pub fn seed_state(&mut self, login: &str, profile: Profile) {
        self.cache.profiles.insert(key(login), profile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(login: &str) -> UserProfile {
        UserProfile {
            id: "1".into(),
            login: login.into(),
            display_name: login.into(),
            avatar_url: None,
            created_at: None,
        }
    }

    #[test]
    fn a_login_is_asked_once_and_then_kept() {
        let mut cache = Cache::default();
        cache.sync_account(Some("42"));
        let epoch = cache.begin_profile("Bob").expect("first ask");
        assert_eq!(cache.profile("bob"), Some(&Profile::Loading));
        // Loading: not asked again, whatever the capitalisation.
        assert_eq!(cache.begin_profile("bob"), None);
        cache.finish_profile("bob", epoch, Ok(Some(profile("bob"))));
        assert!(matches!(cache.profile("BOB"), Some(Profile::Found(_))));
        assert_eq!(cache.begin_profile("bob"), None);
        // No such account is an answer too.
        let epoch = cache.begin_profile("ghost").unwrap();
        cache.finish_profile("ghost", epoch, Ok(None));
        assert_eq!(cache.profile("ghost"), Some(&Profile::Missing));
        assert_eq!(cache.begin_profile("ghost"), None);
    }

    #[test]
    fn what_cannot_be_a_login_is_never_asked() {
        let mut cache = Cache::default();
        assert_eq!(cache.begin_profile(""), None);
        assert_eq!(cache.begin_profile("not a login"), None);
        assert_eq!(cache.profile("not a login"), None);
    }

    #[test]
    fn a_failure_stays_until_retried_and_a_retry_asks_again() {
        let mut cache = Cache::default();
        let epoch = cache.begin_profile("bob").unwrap();
        cache.finish_profile("bob", epoch, Err("boom".into()));
        assert_eq!(cache.profile("bob"), Some(&Profile::Failed("boom".into())));
        // Not asked again by itself.
        assert_eq!(cache.begin_profile("bob"), None);
        // Retry only applies to a failure, and starts a new lookup.
        assert_eq!(cache.retry("nobody"), None);
        let epoch = cache.retry("bob").expect("a failed lookup is retried");
        assert_eq!(cache.profile("bob"), Some(&Profile::Loading));
        assert_eq!(cache.retry("bob"), None, "loading is not a failure");
        cache.finish_profile("bob", epoch, Ok(Some(profile("bob"))));
        assert!(matches!(cache.profile("bob"), Some(Profile::Found(_))));
    }

    #[test]
    fn follows_are_asked_once_per_channel_and_user() {
        let mut cache = Cache::default();
        let epoch = cache.begin_follow("1000", "7").unwrap();
        assert_eq!(cache.follow("1000", "7"), Some(Follow::Loading));
        assert_eq!(cache.begin_follow("1000", "7"), None);
        assert!(cache.begin_follow("1000", "8").is_some());
        assert!(cache.begin_follow("2000", "7").is_some());
        cache.finish_follow("1000", "7", epoch, Ok(Some(5)));
        assert_eq!(cache.follow("1000", "7"), Some(Follow::Since(Some(5))));
        // A refusal is "unavailable", and stays so.
        let epoch = cache.begin_follow("3000", "7").unwrap();
        cache.finish_follow("3000", "7", epoch, Err(()));
        assert_eq!(cache.follow("3000", "7"), Some(Follow::Unavailable));
    }

    #[test]
    fn a_different_account_empties_the_cache_and_late_answers_are_dropped() {
        let mut cache = Cache::default();
        assert!(!cache.sync_account(None), "nothing to drop yet");
        cache.sync_account(Some("42"));
        let old = cache.begin_profile("bob").unwrap();
        let old_follow = cache.begin_follow("1000", "7").unwrap();
        assert!(!cache.sync_account(Some("42")), "same account: kept");
        assert_eq!(cache.profile("bob"), Some(&Profile::Loading));
        // Logging out, or in as someone else, forgets everything.
        assert!(cache.sync_account(None));
        assert_eq!(cache.profile("bob"), None);
        assert_eq!(cache.follow("1000", "7"), None);
        // The answer to the old request arrives: it belongs to nobody now.
        cache.finish_profile("bob", old, Ok(Some(profile("bob"))));
        cache.finish_follow("1000", "7", old_follow, Ok(Some(1)));
        assert_eq!(cache.profile("bob"), None);
        assert_eq!(cache.follow("1000", "7"), None);
        // And the new account asks afresh.
        cache.sync_account(Some("43"));
        assert!(cache.begin_profile("bob").is_some());
    }
}
