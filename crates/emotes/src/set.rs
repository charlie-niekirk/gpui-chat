use std::collections::HashMap;
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
        }
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
}
