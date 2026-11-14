//! Twitch chat badges: where their definitions come from ([`BadgeSource`]) and the shared store
//! the chat view looks them up in ([`BadgeRegistry`]).
//!
//! A badge in an IRC message is only a `set/version` pair (`subscriber/12`, `moderator/1`); what
//! it looks like (title, images) is a *definition* published per channel (subscriber tiers and
//! months, bits tiers, custom names) and globally (moderator, VIP, staff, ...).
//!
//! * [`BadgeSource`] is the seam: [`BadgeSource::global`] and [`BadgeSource::channel`] return a
//!   [`BadgeSet`]. [`AnonymousBadgeSource`] needs no login: the global set is a bundled snapshot
//!   ([`snapshot`]) and a channel's comes from `api.ivr.fi` ([`ivr`]). [`HelixBadgeSource`]
//!   ([`helix`]) is the logged-in one: Twitch's own `chat/badges` endpoints, through an
//!   authenticating [`crate::HttpClient`] the app supplies.
//! * [`BadgeRegistry`] holds the loaded sets: a channel's first, then the global ones, looked up
//!   by `(set, version)` without allocating. Each store bumps a version number so a view can tell
//!   that the badges of a channel it already laid out may now resolve.
//!
//! Failures degrade silently: a source that fails is logged as a warning and the badges it would
//! have supplied are simply not shown.

mod anonymous;
pub mod helix;
pub mod ivr;
pub mod snapshot;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use gpui_chat_core::{Badge, BadgeId, ChannelId};
use thiserror::Error;

pub use anonymous::AnonymousBadgeSource;
pub use helix::HelixBadgeSource;

use crate::http::HttpError;

/// Why a badge source failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BadgeError {
    /// The id is not a Twitch user id (digits only).
    #[error("invalid Twitch user id {0:?} (digits expected)")]
    InvalidUserId(String),
    #[error(transparent)]
    Http(#[from] HttpError),
    /// A non-success status other than 404 on a channel lookup.
    #[error("unexpected HTTP status {0}")]
    Status(u16),
    /// The body was not the JSON this source expects.
    #[error("malformed response: {0}")]
    Parse(String),
    /// The bundled snapshot is unreadable (a build problem, not a runtime condition).
    #[error("bundled badge snapshot: {0}")]
    Snapshot(String),
}

/// A boxed future (object-safe trait methods cannot be `async fn`).
pub type BadgeFuture<'a> = Pin<Box<dyn Future<Output = Result<BadgeSet, BadgeError>> + Send + 'a>>;

/// Where badge definitions come from. One call is one attempt; callers retry.
pub trait BadgeSource: Send + Sync + 'static {
    /// The badges every channel shares (moderator, VIP, broadcaster, staff, events, ...).
    fn global(&self) -> BadgeFuture<'_>;

    /// A channel's own badges (subscriber tiers, bits tiers, custom badges), for its room id
    /// (the Twitch user id of the broadcaster). A channel without any is an empty set, not an
    /// error.
    fn channel<'a>(&'a self, room_id: &'a str) -> BadgeFuture<'a>;
}

/// Badge definitions by set and version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BadgeSet {
    sets: HashMap<Arc<str>, HashMap<Arc<str>, Arc<Badge>>>,
    len: usize,
}

impl BadgeSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `badge`, replacing a definition with the same set and version.
    pub fn insert(&mut self, badge: Badge) {
        let versions = self.sets.entry(badge.id.set.clone()).or_default();
        if versions
            .insert(badge.id.version.clone(), Arc::new(badge))
            .is_none()
        {
            self.len += 1;
        }
    }

    /// The definition of `set/version`. O(1), no allocation.
    pub fn get(&self, set: &str, version: &str) -> Option<&Arc<Badge>> {
        self.sets.get(set)?.get(version)
    }

    /// Number of definitions (versions) in all sets.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Every definition, sets in name order and versions in version order, so the output is
    /// deterministic.
    pub fn iter_sorted(&self) -> impl Iterator<Item = &Arc<Badge>> {
        let mut names: Vec<&Arc<str>> = self.sets.keys().collect();
        names.sort();
        names.into_iter().flat_map(move |name| {
            let versions = &self.sets[name];
            let mut ids: Vec<&Arc<str>> = versions.keys().collect();
            ids.sort_by(|a, b| version_order(a, b));
            ids.into_iter().map(move |id| &versions[id])
        })
    }
}

/// Numeric versions in numeric order (`2` before `10`), others after them by text.
fn version_order(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

/// A set and the registry version at which it was stored.
struct Stored {
    set: Arc<BadgeSet>,
    version: u64,
}

#[derive(Default)]
struct Sets {
    global: Option<Stored>,
    channels: HashMap<ChannelId, Stored>,
}

/// The loaded badge sets, shared between the loaders (writers, on the runtime) and the chat view
/// (reader, on the UI thread). Reading is O(1) per badge and takes a read lock only.
#[derive(Default)]
pub struct BadgeRegistry {
    sets: RwLock<Sets>,
    /// Bumped by every store; each stored set remembers the value it was stored at.
    counter: AtomicU64,
}

impl BadgeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the global badges.
    pub fn store_global(&self, set: BadgeSet) {
        let stored = self.stored(set);
        self.sets
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .global = Some(stored);
    }

    /// Replaces `channel`'s badges.
    pub fn store_channel(&self, channel: &ChannelId, set: BadgeSet) {
        let stored = self.stored(set);
        self.sets
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .channels
            .insert(channel.clone(), stored);
    }

    /// Drops `channel`'s badges (the channel was closed).
    pub fn forget_channel(&self, channel: &ChannelId) {
        self.sets
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .channels
            .remove(channel);
    }

    fn stored(&self, set: BadgeSet) -> Stored {
        // Built before the lock is taken: the clone-free move is all that happens under it.
        Stored {
            set: Arc::new(set),
            version: self.counter.fetch_add(1, Ordering::Relaxed) + 1,
        }
    }

    /// The definition of `id` in `channel`: the channel's own badges first, then the global ones.
    pub fn lookup(&self, channel: &ChannelId, id: &BadgeId) -> Option<Arc<Badge>> {
        self.lookup_str(channel, &id.set, &id.version)
    }

    /// [`Self::lookup`] by text.
    pub fn lookup_str(&self, channel: &ChannelId, set: &str, version: &str) -> Option<Arc<Badge>> {
        let sets = self.sets.read().unwrap_or_else(PoisonError::into_inner);
        sets.channels
            .get(channel)
            .and_then(|stored| stored.set.get(set, version))
            .or_else(|| {
                sets.global
                    .as_ref()
                    .and_then(|stored| stored.set.get(set, version))
            })
            .cloned()
    }

    /// A number that changes whenever a lookup in `channel` may give another answer than it did
    /// before: the newer of the channel's and the global set's store. A view remembers it and
    /// lays its messages out again when it moves.
    pub fn version(&self, channel: &ChannelId) -> u64 {
        let sets = self.sets.read().unwrap_or_else(PoisonError::into_inner);
        let global = sets.global.as_ref().map_or(0, |stored| stored.version);
        let channel = sets
            .channels
            .get(channel)
            .map_or(0, |stored| stored.version);
        global.max(channel)
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::ImageUrls;

    use super::*;

    fn badge(set: &str, version: &str, title: &str) -> Badge {
        Badge {
            id: BadgeId::new(set, version),
            title: title.to_owned(),
            urls: ImageUrls::new(),
        }
    }

    fn set(badges: &[(&str, &str, &str)]) -> BadgeSet {
        let mut set = BadgeSet::new();
        for (s, v, t) in badges {
            set.insert(badge(s, v, t));
        }
        set
    }

    #[test]
    fn a_set_is_looked_up_by_set_and_version_and_replaces_duplicates() {
        let mut s = set(&[("moderator", "1", "Moderator"), ("subscriber", "0", "Sub")]);
        assert_eq!(s.len(), 2);
        assert_eq!(s.get("moderator", "1").unwrap().title, "Moderator");
        assert!(s.get("moderator", "2").is_none());
        assert!(s.get("vip", "1").is_none());
        s.insert(badge("moderator", "1", "Mod"));
        assert_eq!(s.len(), 2);
        assert_eq!(s.get("moderator", "1").unwrap().title, "Mod");
        assert!(BadgeSet::new().is_empty());
    }

    #[test]
    fn iteration_is_sorted_with_numeric_versions_in_numeric_order() {
        let s = set(&[
            ("subscriber", "12", "12"),
            ("bits", "100", "100"),
            ("subscriber", "2", "2"),
            ("subscriber", "0", "0"),
            ("bits", "1", "1"),
            ("bits", "x", "x"),
        ]);
        let order: Vec<_> = s
            .iter_sorted()
            .map(|b| format!("{}/{}", b.id.set, b.id.version))
            .collect();
        assert_eq!(
            order,
            [
                "bits/1",
                "bits/100",
                "bits/x",
                "subscriber/0",
                "subscriber/2",
                "subscriber/12"
            ]
        );
    }

    #[test]
    fn channel_badges_win_over_global_ones_and_global_fill_the_gaps() {
        let registry = BadgeRegistry::new();
        let channel = ChannelId::new("a");
        let other = ChannelId::new("b");
        registry.store_global(set(&[
            ("moderator", "1", "Moderator"),
            ("subscriber", "0", "Subscriber"),
        ]));
        registry.store_channel(&channel, set(&[("subscriber", "0", "Pog Squad")]));

        let title =
            |c: &ChannelId, s: &str, v: &str| registry.lookup_str(c, s, v).map(|b| b.title.clone());
        assert_eq!(
            title(&channel, "subscriber", "0").as_deref(),
            Some("Pog Squad")
        );
        assert_eq!(
            title(&channel, "moderator", "1").as_deref(),
            Some("Moderator")
        );
        // Another channel never sees this channel's badges.
        assert_eq!(
            title(&other, "subscriber", "0").as_deref(),
            Some("Subscriber")
        );
        assert_eq!(title(&channel, "vip", "1"), None);
        assert_eq!(
            registry
                .lookup(&channel, &BadgeId::new("moderator", "1"))
                .unwrap()
                .title,
            "Moderator"
        );

        registry.forget_channel(&channel);
        assert_eq!(
            title(&channel, "subscriber", "0").as_deref(),
            Some("Subscriber")
        );
    }

    #[test]
    fn the_version_moves_when_a_lookup_could_change() {
        let registry = BadgeRegistry::new();
        let (a, b) = (ChannelId::new("a"), ChannelId::new("b"));
        assert_eq!(registry.version(&a), 0);
        registry.store_global(set(&[("moderator", "1", "Moderator")]));
        let after_global = registry.version(&a);
        assert!(after_global > 0);
        assert_eq!(registry.version(&b), after_global);

        registry.store_channel(&a, BadgeSet::new());
        let a_after = registry.version(&a);
        assert!(a_after > after_global);
        // `b` is not affected by `a`'s store.
        assert_eq!(registry.version(&b), after_global);

        registry.store_global(set(&[("moderator", "1", "Moderator")]));
        assert!(registry.version(&b) > after_global);
        assert!(registry.version(&a) > a_after);
    }
}
