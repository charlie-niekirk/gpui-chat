use std::collections::HashMap;
use std::sync::Arc;

use gpui_chat_core::{Emote, EmoteId, EmoteProvider};

/// Whether a set applies everywhere or to one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmoteScope {
    Global,
    Channel,
}

/// One live change to an emote set (7TV `emote_set.update`, BTTV `emote_*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmoteChange {
    Added(SetEmote),
    /// `name` is the emote's name when known (set by [`EmoteSet::with_changes`] in what it applied).
    Removed {
        id: EmoteId,
        name: Option<Arc<str>>,
    },
    /// `from` is the previous name when known.
    Renamed {
        id: EmoteId,
        from: Option<Arc<str>>,
        to: Arc<str>,
    },
}

/// One emote plus the provider metadata that `core::Emote` has no field for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetEmote {
    /// `emote.name` is what users type. For 7TV channel sets that is the channel's alias
    /// when one is set.
    pub emote: Arc<Emote>,
    /// 7TV only: the emote's own name when the channel renamed it (`emote.name` is then
    /// the alias).
    pub original_name: Option<Arc<str>>,
    /// Uploader / owner display name, when the provider reports one. BTTV channel emotes
    /// (owned by the channel itself) have none.
    pub owner: Option<Arc<str>>,
}

/// All emotes one provider offers for one scope, in the provider's order, with O(1)
/// lookup by name.
///
/// Within a BTTV channel set the channel's own emotes come before its shared emotes.
/// Names are not de-duplicated in [`EmoteSet::emotes`], but [`EmoteSet::get`] returns the
/// first emote of a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmoteSet {
    pub provider: EmoteProvider,
    pub scope: EmoteScope,
    emotes: Vec<SetEmote>,
    /// Name → index of the first emote with that name.
    index: HashMap<Arc<str>, usize>,
    origin: Option<SetOrigin>,
}

/// The provider's own ids behind a channel set: what the live-update sockets subscribe to
/// (7TV: the emote set id and the owning 7TV user's id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetOrigin {
    /// `None`: the owner has an account but no active set.
    pub set_id: Option<Arc<str>>,
    pub owner_id: Option<Arc<str>>,
    /// 7TV: the index of the owner's Twitch connection in their connection list (a
    /// `user.update` names the connection that changed by index).
    pub connection: Option<usize>,
}

impl EmoteSet {
    pub fn new(provider: EmoteProvider, scope: EmoteScope, emotes: Vec<SetEmote>) -> Self {
        let mut index = HashMap::with_capacity(emotes.len());
        for (i, e) in emotes.iter().enumerate() {
            index.entry(e.emote.name.clone()).or_insert(i);
        }
        Self {
            provider,
            scope,
            emotes,
            index,
            origin: None,
        }
    }

    /// This set with the provider's ids recorded (see [`SetOrigin`]).
    #[must_use]
    pub fn with_origin(mut self, origin: SetOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    /// The provider's ids behind this set, when it reported them.
    pub fn origin(&self) -> Option<&SetOrigin> {
        self.origin.as_ref()
    }

    /// A copy with `changes` applied (a live update) and the changes that took effect, names
    /// filled in. Additions go to the end (an emote already there under the same id is replaced, reported only if its name changed),
    /// a rename keeps the emote's position, removals and renames are matched by emote id, and a
    /// change naming an unknown emote is dropped.
    #[must_use]
    pub fn with_changes(&self, changes: &[EmoteChange]) -> (Self, Vec<EmoteChange>) {
        let mut emotes = self.emotes.clone();
        let mut applied = Vec::new();
        for change in changes {
            match change {
                EmoteChange::Added(entry) => {
                    match emotes.iter().position(|e| e.emote.id == entry.emote.id) {
                        Some(i) => {
                            // The emote is known: an update. Only a new name is news.
                            let from = emotes[i].emote.name.clone();
                            let renamed = from != entry.emote.name;
                            emotes[i] = entry.clone();
                            if renamed {
                                applied.push(EmoteChange::Renamed {
                                    id: entry.emote.id.clone(),
                                    from: Some(from),
                                    to: entry.emote.name.clone(),
                                });
                            }
                        }
                        None => {
                            emotes.push(entry.clone());
                            applied.push(change.clone());
                        }
                    }
                }
                EmoteChange::Removed { id, .. } => {
                    if let Some(i) = emotes.iter().position(|e| e.emote.id == *id) {
                        let gone = emotes.remove(i);
                        applied.push(EmoteChange::Removed {
                            id: id.clone(),
                            name: Some(gone.emote.name.clone()),
                        });
                    }
                }
                EmoteChange::Renamed { id, to, .. } => {
                    let Some(i) = emotes.iter().position(|e| e.emote.id == *id) else {
                        continue;
                    };
                    if emotes[i].emote.name == *to {
                        continue;
                    }
                    let from = emotes[i].emote.name.clone();
                    let mut emote = (*emotes[i].emote).clone();
                    emote.name = to.clone();
                    emotes[i].emote = Arc::new(emote);
                    applied.push(EmoteChange::Renamed {
                        id: id.clone(),
                        from: Some(from),
                        to: to.clone(),
                    });
                }
            }
        }
        let mut set = Self::new(self.provider, self.scope, emotes);
        set.origin = self.origin.clone();
        (set, applied)
    }

    /// An empty set (e.g. a channel without an account at the provider).
    pub fn empty(provider: EmoteProvider, scope: EmoteScope) -> Self {
        Self::new(provider, scope, Vec::new())
    }

    /// All entries, in the provider's order.
    pub fn emotes(&self) -> &[SetEmote] {
        &self.emotes
    }

    pub fn len(&self) -> usize {
        self.emotes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.emotes.is_empty()
    }

    /// The emotes, ready for a resolver map.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<Emote>> {
        self.emotes.iter().map(|e| &e.emote)
    }

    /// Whether an emote with this id is in the set.
    pub fn contains_id(&self, id: &str) -> bool {
        self.emotes.iter().any(|e| e.emote.id.as_str() == id)
    }

    /// The first emote typed as `name` (case-sensitive), in O(1).
    pub fn get(&self, name: &str) -> Option<&SetEmote> {
        self.index.get(name).map(|&i| &self.emotes[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_chat_core::ImageUrls;

    fn entry(id: &str, name: &str) -> SetEmote {
        SetEmote {
            emote: Arc::new(Emote {
                id: id.into(),
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
        }
    }

    #[test]
    fn lookup_keeps_the_first_of_duplicate_names() {
        let set = EmoteSet::new(
            EmoteProvider::Bttv,
            EmoteScope::Channel,
            vec![entry("1", "A"), entry("2", "B"), entry("3", "A")],
        );
        assert_eq!(set.len(), 3);
        assert_eq!(set.get("A").unwrap().emote.id.as_str(), "1");
        assert_eq!(set.get("B").unwrap().emote.id.as_str(), "2");
        assert!(set.get("a").is_none());
        assert!(
            EmoteSet::empty(EmoteProvider::Ffz, EmoteScope::Global)
                .get("A")
                .is_none()
        );
    }

    fn changes_set() -> EmoteSet {
        EmoteSet::new(
            EmoteProvider::Bttv,
            EmoteScope::Channel,
            vec![entry("1", "A"), entry("2", "B")],
        )
        .with_origin(SetOrigin {
            set_id: Some("S".into()),
            owner_id: None,
            connection: None,
        })
    }

    #[test]
    fn live_changes_add_remove_and_rename_and_report_what_happened() {
        let set = changes_set();
        let (next, applied) = set.with_changes(&[
            EmoteChange::Added(entry("3", "C")),
            EmoteChange::Removed {
                id: "1".into(),
                name: None,
            },
            EmoteChange::Renamed {
                id: "2".into(),
                from: None,
                to: "Bee".into(),
            },
        ]);
        let names: Vec<_> = next.iter().map(|e| &*e.name).collect();
        assert_eq!(names, ["Bee", "C"]);
        assert!(next.get("B").is_none() && next.get("Bee").is_some());
        // The removal reports the name it had; the rename the old one.
        assert_eq!(
            applied[1],
            EmoteChange::Removed {
                id: "1".into(),
                name: Some("A".into())
            }
        );
        assert_eq!(
            applied[2],
            EmoteChange::Renamed {
                id: "2".into(),
                from: Some("B".into()),
                to: "Bee".into()
            }
        );
        assert_eq!(
            next.origin(),
            set.origin(),
            "the set's ids survive an update"
        );
        // The original is untouched (sets are shared behind an Arc).
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn changes_to_unknown_emotes_and_repeats_are_dropped() {
        let set = changes_set();
        let (next, applied) = set.with_changes(&[
            EmoteChange::Removed {
                id: "nope".into(),
                name: None,
            },
            EmoteChange::Renamed {
                id: "nope".into(),
                from: None,
                to: "X".into(),
            },
            // Already named so: nothing happened.
            EmoteChange::Renamed {
                id: "1".into(),
                from: None,
                to: "A".into(),
            },
            // The same emote again (an update raced a reload): not news.
            EmoteChange::Added(entry("2", "B")),
        ]);
        assert!(applied.is_empty(), "{applied:?}");
        assert_eq!(next, set);
    }

    #[test]
    fn an_added_emote_that_exists_replaces_it_and_reports_only_a_new_name() {
        let (next, applied) =
            changes_set().with_changes(&[EmoteChange::Added(entry("1", "Alpha"))]);
        assert_eq!(next.len(), 2);
        assert_eq!(next.emotes()[0].emote.name.as_ref(), "Alpha");
        assert_eq!(
            applied,
            [EmoteChange::Renamed {
                id: "1".into(),
                from: Some("A".into()),
                to: "Alpha".into()
            }]
        );
    }
}
