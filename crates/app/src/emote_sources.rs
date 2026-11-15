//! What one input box can offer as emotes (PR 3.3): the user's own Twitch emotes (Helix, loaded
//! once per user and room) together with the third-party sets the emote service holds, merged into
//! one [`EmoteCatalog`] that is rebuilt only when a set changed.

use std::sync::Arc;

use gpui_chat_core::Emote;
use gpui_chat_emotes::{EmoteCatalog, EmoteSet};
use gpui_chat_twitch::UserEmote;
use gpui_kit::SharedString;

/// The scope `Get User Emotes` needs; a login without it is asked to log in again.
pub use gpui_chat_twitch::user_emotes::SCOPE as OWN_EMOTES_SCOPE;

/// How far loading the user's own emotes got.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    NotAsked,
    Loading,
    Loaded,
    Failed(SharedString),
}

/// Whose emotes in which room were asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub user_id: String,
    pub room_id: String,
}

/// The user's Twitch emotes and the catalog built from them.
pub struct EmoteSources {
    key: Option<Key>,
    status: Status,
    own: Vec<Arc<Emote>>,
    /// Bumped whenever `own` changes, so the catalog knows to rebuild.
    generation: u64,
    catalog: Arc<EmoteCatalog>,
    /// What `catalog` was built from: the sets (held, compared by identity, so a freed set's
    /// address cannot be mistaken for a new one) and `generation`.
    built_from: Option<(Vec<Arc<EmoteSet>>, u64)>,
}

impl Default for EmoteSources {
    fn default() -> Self {
        Self {
            key: None,
            status: Status::NotAsked,
            own: Vec::new(),
            generation: 0,
            catalog: Arc::default(),
            built_from: None,
        }
    }
}

impl EmoteSources {
    /// Whether the emotes of `key` should be requested now: yes for a user or room not asked about
    /// yet; no while loading, once loaded, or after a failure (see [`Self::retry`]). Marks the load
    /// as started.
    pub fn begin(&mut self, key: &Key) -> bool {
        if self.key.as_ref() == Some(key) && self.status != Status::NotAsked {
            return false;
        }
        if self.key.as_ref() != Some(key) && !self.own.is_empty() {
            // Another user (or room): their emotes are not ours.
            self.own.clear();
            self.generation += 1;
        }
        self.key = Some(key.clone());
        self.status = Status::Loading;
        true
    }

    /// The outcome of a load started for `key`. Ignored if the box has moved on to another key.
    pub fn finish(&mut self, key: &Key, outcome: Result<Vec<UserEmote>, String>) {
        if self.key.as_ref() != Some(key) {
            return;
        }
        match outcome {
            Ok(emotes) => {
                // Follower emotes of other channels are listed by Helix but do not work here.
                self.own = emotes
                    .into_iter()
                    .filter(|e| e.usable_in(&key.room_id))
                    .map(|e| Arc::new(e.emote))
                    .collect();
                self.generation += 1;
                self.status = Status::Loaded;
            }
            Err(message) => self.status = Status::Failed(message.into()),
        }
    }

    /// Forget a failure so the next [`Self::begin`] asks again (the picker opening is a good time).
    pub fn retry(&mut self) {
        if matches!(self.status, Status::Failed(_)) {
            self.status = Status::NotAsked;
        }
    }

    /// Logged out, or logged in without the scope: no emotes of the user's own. Drops what was
    /// loaded for someone else.
    pub fn forget(&mut self, why: Option<&str>) {
        if self.key.is_some() || !self.own.is_empty() {
            self.key = None;
            self.own.clear();
            self.generation += 1;
        }
        self.status = match why {
            Some(why) => Status::Failed(why.to_owned().into()),
            None => Status::NotAsked,
        };
    }

    /// What to tell the user about the Twitch tab, if anything: still loading, or why it is empty.
    pub fn note(&self) -> Option<SharedString> {
        match &self.status {
            Status::Loading => Some("Loading your Twitch emotes…".into()),
            Status::Failed(message) => Some(message.clone()),
            Status::NotAsked => self
                .own
                .is_empty()
                .then(|| "Log in to use your Twitch emotes.".into()),
            Status::Loaded => None,
        }
    }

    /// The catalog for the current sets, rebuilt if they (or the user's emotes) changed since the
    /// last call. Cheap otherwise: it compares the sets' identities.
    pub fn catalog(&mut self, sets: &[Arc<EmoteSet>]) -> Arc<EmoteCatalog> {
        let unchanged = self.built_from.as_ref().is_some_and(|(built, generation)| {
            *generation == self.generation
                && built.len() == sets.len()
                && built.iter().zip(sets).all(|(a, b)| Arc::ptr_eq(a, b))
        });
        if !unchanged {
            self.catalog = Arc::new(EmoteCatalog::build(&self.own, sets));
            self.built_from = Some((sets.to_vec(), self.generation));
        }
        self.catalog.clone()
    }

    #[cfg(test)]
    fn own_names(&self) -> Vec<&str> {
        self.own.iter().map(|e| &*e.name).collect()
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{EmoteProvider, ImageUrls};
    use gpui_chat_emotes::{EmoteScope, SetEmote};

    use super::*;

    fn key(user: &str, room: &str) -> Key {
        Key {
            user_id: user.into(),
            room_id: room.into(),
        }
    }

    #[test]
    fn follower_emotes_of_other_channels_are_left_out() {
        let mut sources = EmoteSources::default();
        let k = key("1", "7");
        sources.begin(&k);
        let entry = |name: &str, kind: &str, owner: &str| UserEmote {
            emote: Emote::twitch(name, name),
            owner_id: owner.into(),
            kind: kind.into(),
        };
        sources.finish(
            &k,
            Ok(vec![
                entry("Here", "follower", "7"),
                entry("Elsewhere", "follower", "8"),
                entry("Sub", "subscriptions", "8"),
                entry("Global", "globals", "0"),
            ]),
        );
        assert_eq!(sources.own_names(), ["Here", "Sub", "Global"]);
    }

    #[test]
    fn a_set_that_was_freed_cannot_be_mistaken_for_a_new_one() {
        // The cache holds the sets it was built from, so another set can never take their address.
        let mut sources = EmoteSources::default();
        let first = [set("Old")];
        let before = sources.catalog(&first);
        drop(first);
        let next = [set("New")];
        let after = sources.catalog(&next);
        assert_eq!(&*after.all()[0].name, "New");
        assert!(!Arc::ptr_eq(&before, &after));
    }

    fn user_emotes(names: &[&str]) -> Vec<UserEmote> {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| UserEmote {
                emote: Emote::twitch(i.to_string(), *name),
                owner_id: "0".into(),
                kind: "globals".into(),
            })
            .collect()
    }

    fn set(name: &str) -> Arc<EmoteSet> {
        Arc::new(EmoteSet::new(
            EmoteProvider::Bttv,
            EmoteScope::Global,
            vec![SetEmote {
                emote: Arc::new(Emote {
                    id: name.into(),
                    name: name.into(),
                    provider: EmoteProvider::Bttv,
                    urls: ImageUrls::new(),
                    animated: false,
                    zero_width: false,
                    width: None,
                    height: None,
                }),
                original_name: None,
                owner: None,
            }],
        ))
    }

    #[test]
    fn a_user_and_room_are_asked_about_once() {
        let mut sources = EmoteSources::default();
        let k = key("1", "7");
        assert!(sources.begin(&k));
        assert!(!sources.begin(&k), "already loading");
        sources.finish(&k, Ok(user_emotes(&["Kappa"])));
        assert!(!sources.begin(&k), "already loaded");
        assert_eq!(sources.own_names(), ["Kappa"]);
        assert_eq!(sources.note(), None);
        // Another room asks again.
        assert!(sources.begin(&key("1", "8")));
    }

    #[test]
    fn a_late_answer_for_another_key_is_ignored() {
        let mut sources = EmoteSources::default();
        let old = key("1", "7");
        assert!(sources.begin(&old));
        let new = key("2", "7");
        assert!(sources.begin(&new));
        sources.finish(&old, Ok(user_emotes(&["Theirs"])));
        assert!(sources.own_names().is_empty());
        sources.finish(&new, Ok(user_emotes(&["Mine"])));
        assert_eq!(sources.own_names(), ["Mine"]);
    }

    #[test]
    fn another_user_does_not_keep_the_previous_users_emotes() {
        let mut sources = EmoteSources::default();
        let k = key("1", "7");
        sources.begin(&k);
        sources.finish(&k, Ok(user_emotes(&["Sub"])));
        sources.begin(&key("2", "7"));
        assert!(sources.own_names().is_empty());
    }

    #[test]
    fn a_failure_is_shown_and_asked_again_only_on_request() {
        let mut sources = EmoteSources::default();
        let k = key("1", "7");
        sources.begin(&k);
        sources.finish(&k, Err("Log in again to use your Twitch emotes.".into()));
        assert_eq!(
            sources.note().as_deref(),
            Some("Log in again to use your Twitch emotes.")
        );
        assert!(!sources.begin(&k), "no retry loop");
        sources.retry();
        assert!(sources.begin(&k));
        assert_eq!(
            sources.note().as_deref(),
            Some("Loading your Twitch emotes…")
        );
    }

    #[test]
    fn logging_out_drops_the_emotes() {
        let mut sources = EmoteSources::default();
        let k = key("1", "7");
        sources.begin(&k);
        sources.finish(&k, Ok(user_emotes(&["Sub"])));
        sources.forget(None);
        assert!(sources.own_names().is_empty());
        assert_eq!(
            sources.note().as_deref(),
            Some("Log in to use your Twitch emotes.")
        );
        // The same user logging in again asks again.
        assert!(sources.begin(&k));
    }

    #[test]
    fn the_catalog_is_rebuilt_only_when_something_changed() {
        let mut sources = EmoteSources::default();
        let sets = [set("monkaW")];
        let first = sources.catalog(&sets);
        assert!(Arc::ptr_eq(&first, &sources.catalog(&sets)));
        // The same set reloaded is another Arc: rebuilt.
        let reloaded = [set("monkaW"), set("EZ")];
        let second = sources.catalog(&reloaded);
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.all().len(), 2);
        // The user's emotes arriving rebuild it too, and come first.
        let k = key("1", "7");
        sources.begin(&k);
        sources.finish(&k, Ok(user_emotes(&["Kappa"])));
        let third = sources.catalog(&reloaded);
        assert!(!Arc::ptr_eq(&second, &third));
        assert_eq!(&*third.all()[0].name, "Kappa");
    }
}
