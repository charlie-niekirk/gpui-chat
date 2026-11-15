//! Every emote one channel's input box can offer (PR 3.3): the user's own Twitch emotes and the
//! loaded third-party sets, de-duplicated by the name that is typed and indexed by provider for
//! the picker's tabs.
//!
//! A name that two sets share is typed once and means the first one in precedence order, the same
//! order chat resolves words in: Twitch's own emotes (the IRC tags win over any word lookup), then
//! channel 7TV → BTTV → FFZ, then global 7TV → BTTV → FFZ (see [`crate::resolver`]).

use std::collections::HashSet;
use std::sync::Arc;

use gpui_chat_core::{Emote, EmoteProvider};

use crate::set::EmoteSet;

const PROVIDERS: [EmoteProvider; 4] = [
    EmoteProvider::Twitch,
    EmoteProvider::SevenTv,
    EmoteProvider::Bttv,
    EmoteProvider::Ffz,
];

fn index(provider: EmoteProvider) -> usize {
    match provider {
        EmoteProvider::Twitch => 0,
        EmoteProvider::SevenTv => 1,
        EmoteProvider::Bttv => 2,
        EmoteProvider::Ffz => 3,
    }
}

/// The emotes available in one channel, for completion and the picker.
#[derive(Debug, Clone, Default)]
pub struct EmoteCatalog {
    /// Each name once, in precedence order.
    all: Vec<Arc<Emote>>,
    /// Per provider, each name once within the provider, the channel's before the global ones.
    by_provider: [Vec<Arc<Emote>>; 4],
}

impl EmoteCatalog {
    /// `twitch` are the user's own Twitch emotes; `sets` the third-party sets in the precedence
    /// order of [`crate::EmoteResolver::sets`].
    pub fn build(twitch: &[Arc<Emote>], sets: &[Arc<EmoteSet>]) -> Self {
        let mut catalog = Self::default();
        let mut taken: HashSet<Arc<str>> = HashSet::new();
        let mut per_provider: [HashSet<Arc<str>>; 4] = Default::default();
        let sources = std::iter::once(twitch.iter().collect::<Vec<_>>())
            .chain(sets.iter().map(|set| set.iter().collect::<Vec<_>>()));
        for emotes in sources {
            for emote in emotes {
                let slot = index(emote.provider);
                if per_provider[slot].insert(emote.name.clone()) {
                    catalog.by_provider[slot].push(emote.clone());
                }
                if taken.insert(emote.name.clone()) {
                    catalog.all.push(emote.clone());
                }
            }
        }
        catalog
    }

    /// Every emote, each name once.
    pub fn all(&self) -> &[Arc<Emote>] {
        &self.all
    }

    /// `provider`'s emotes (channel before global), each name once.
    pub fn provider(&self, provider: EmoteProvider) -> &[Arc<Emote>] {
        &self.by_provider[index(provider)]
    }

    /// The providers that have at least one emote, in tab order.
    pub fn providers(&self) -> impl Iterator<Item = EmoteProvider> + '_ {
        PROVIDERS
            .into_iter()
            .filter(|provider| !self.provider(*provider).is_empty())
    }

    /// The emote typed as `name` (case-sensitive), by precedence.
    pub fn find(&self, name: &str) -> Option<&Arc<Emote>> {
        self.all.iter().find(|emote| &*emote.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::ImageUrls;

    use super::*;
    use crate::set::{EmoteScope, SetEmote};

    fn emote(provider: EmoteProvider, name: &str) -> Arc<Emote> {
        Arc::new(Emote {
            id: format!("{}-{name}", provider.name()).into(),
            name: name.into(),
            provider,
            urls: ImageUrls::new(),
            animated: false,
            zero_width: false,
            width: None,
            height: None,
        })
    }

    fn set(provider: EmoteProvider, scope: EmoteScope, names: &[&str]) -> Arc<EmoteSet> {
        Arc::new(EmoteSet::new(
            provider,
            scope,
            names
                .iter()
                .map(|name| SetEmote {
                    emote: emote(provider, name),
                    original_name: None,
                    owner: None,
                })
                .collect(),
        ))
    }

    fn names(emotes: &[Arc<Emote>]) -> Vec<String> {
        emotes
            .iter()
            .map(|e| format!("{}:{}", e.provider.name(), e.name))
            .collect()
    }

    #[test]
    fn a_name_belongs_to_the_first_provider_in_precedence_order() {
        let twitch = [
            emote(EmoteProvider::Twitch, "Kappa"),
            emote(EmoteProvider::Twitch, "LUL"),
        ];
        let sets = [
            set(
                EmoteProvider::SevenTv,
                EmoteScope::Channel,
                &["Kappa", "OMEGALUL"],
            ),
            set(
                EmoteProvider::Bttv,
                EmoteScope::Channel,
                &["OMEGALUL", "monkaW"],
            ),
            set(
                EmoteProvider::SevenTv,
                EmoteScope::Global,
                &["monkaW", "EZ"],
            ),
        ];
        let catalog = EmoteCatalog::build(&twitch, &sets);
        assert_eq!(
            names(catalog.all()),
            [
                "Twitch:Kappa",
                "Twitch:LUL",
                "7TV:OMEGALUL",
                "BTTV:monkaW",
                "7TV:EZ"
            ]
        );
        // The twitch emote wins the name, as it does in chat.
        assert_eq!(
            catalog.find("Kappa").unwrap().provider,
            EmoteProvider::Twitch
        );
        assert_eq!(catalog.find("kappa"), None);
    }

    #[test]
    fn provider_tabs_list_their_own_emotes_channel_first() {
        let sets = [
            set(
                EmoteProvider::SevenTv,
                EmoteScope::Channel,
                &["Chan", "Both"],
            ),
            set(EmoteProvider::Bttv, EmoteScope::Channel, &["Both"]),
            set(
                EmoteProvider::SevenTv,
                EmoteScope::Global,
                &["Glob", "Both"],
            ),
        ];
        let catalog = EmoteCatalog::build(&[], &sets);
        // `Both` is in the 7TV and the BTTV tab (each lists what its provider has) ...
        assert_eq!(
            names(catalog.provider(EmoteProvider::SevenTv)),
            ["7TV:Chan", "7TV:Both", "7TV:Glob"]
        );
        assert_eq!(names(catalog.provider(EmoteProvider::Bttv)), ["BTTV:Both"]);
        // ... but is offered for completion once, as the first provider's.
        assert_eq!(names(catalog.all()), ["7TV:Chan", "7TV:Both", "7TV:Glob"],);
        assert!(catalog.provider(EmoteProvider::Ffz).is_empty());
        let providers: Vec<_> = catalog.providers().collect();
        assert_eq!(providers, [EmoteProvider::SevenTv, EmoteProvider::Bttv]);
    }

    #[test]
    fn nothing_loaded_is_empty() {
        let catalog = EmoteCatalog::build(&[], &[]);
        assert!(catalog.is_empty());
        assert_eq!(catalog.providers().count(), 0);
    }
}
