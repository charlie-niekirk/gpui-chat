use std::sync::Arc;

use gpui_chat_core::{Emote, EmoteProvider};

/// Whether a set applies everywhere or to one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmoteScope {
    Global,
    Channel,
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

/// All emotes one provider offers for one scope, in the provider's order.
///
/// Within a BTTV channel set the channel's own emotes come before its shared emotes.
/// Names are not de-duplicated; the resolver keeps the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmoteSet {
    pub provider: EmoteProvider,
    pub scope: EmoteScope,
    pub emotes: Vec<SetEmote>,
}

impl EmoteSet {
    /// An empty set (e.g. a channel without an account at the provider).
    pub fn empty(provider: EmoteProvider, scope: EmoteScope) -> Self {
        Self {
            provider,
            scope,
            emotes: Vec::new(),
        }
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

    /// First emote typed as `name` (case-sensitive).
    pub fn get(&self, name: &str) -> Option<&SetEmote> {
        self.emotes.iter().find(|e| &*e.emote.name == name)
    }
}
