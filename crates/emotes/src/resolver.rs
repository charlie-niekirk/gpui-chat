//! Which emote a word means: the loaded sets, per scope, in precedence order.
//!
//! Precedence (the first hit wins): channel 7TV → channel BTTV → channel FFZ → global 7TV →
//! global BTTV → global FFZ. Twitch's own emotes never come through here: they are resolved from
//! the IRC `emotes` tag ranges before any word is looked up.
//!
//! [`EmoteRegistry`] is the shared, thread-safe store the loaders fill and the chat source reads;
//! [`EmoteResolver`] is an immutable snapshot of one channel's lookup order, a
//! [`WordResolver`] for the core tokenizer.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use gpui_chat_core::{ChannelId, Emote, EmoteProvider, WordResolver};

use crate::loader::ProviderResults;
use crate::set::EmoteSet;

/// Which third-party providers are in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Providers {
    pub seven_tv: bool,
    pub bttv: bool,
    pub ffz: bool,
}

impl Providers {
    /// Every provider.
    pub const ALL: Self = Self {
        seven_tv: true,
        bttv: true,
        ffz: true,
    };

    /// Whether `provider` is on. Twitch's own emotes are always on.
    pub fn is_enabled(self, provider: EmoteProvider) -> bool {
        match provider {
            EmoteProvider::Twitch => true,
            EmoteProvider::SevenTv => self.seven_tv,
            EmoteProvider::Bttv => self.bttv,
            EmoteProvider::Ffz => self.ffz,
        }
    }

    /// `true` if `self` has a provider on that `other` has off.
    pub fn gains_over(self, other: Self) -> bool {
        (self.seven_tv && !other.seven_tv) || (self.bttv && !other.bttv) || (self.ffz && !other.ffz)
    }
}

impl Default for Providers {
    fn default() -> Self {
        Self::ALL
    }
}

/// One scope's sets, in provider precedence order (7TV, BTTV, FFZ).
type Slots = [Option<Arc<EmoteSet>>; 3];

fn slot(provider: EmoteProvider) -> Option<usize> {
    match provider {
        EmoteProvider::SevenTv => Some(0),
        EmoteProvider::Bttv => Some(1),
        EmoteProvider::Ffz => Some(2),
        EmoteProvider::Twitch => None,
    }
}

/// Keeps what loaded; a provider that failed this time leaves its previous set in place, so a
/// reload that hits a provider outage does not wipe the emotes already known.
fn store(slots: &mut Slots, results: &ProviderResults) {
    for set in results.sets() {
        if let Some(i) = slot(set.provider) {
            slots[i] = Some(Arc::new(set.clone()));
        }
    }
}

#[derive(Default)]
struct State {
    enabled: Providers,
    global: Slots,
    channels: HashMap<ChannelId, Slots>,
}

/// The loaded emote sets, shared between the loading tasks and whoever resolves words.
#[derive(Default)]
pub struct EmoteRegistry {
    state: RwLock<State>,
}

impl EmoteRegistry {
    pub fn new(enabled: Providers) -> Self {
        Self {
            state: RwLock::new(State {
                enabled,
                ..State::default()
            }),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn enabled(&self) -> Providers {
        self.read().enabled
    }

    /// Turns providers on or off. Takes effect for the next message; sets stay loaded, so turning
    /// one back on needs no request unless it was never loaded.
    pub fn set_enabled(&self, enabled: Providers) {
        self.write().enabled = enabled;
    }

    /// Records the global sets that loaded (see [`ProviderResults`]).
    pub fn store_global(&self, results: &ProviderResults) {
        store(&mut self.write().global, results);
    }

    /// Records the sets that loaded for `channel`.
    pub fn store_channel(&self, channel: &ChannelId, results: &ProviderResults) {
        store(
            self.write().channels.entry(channel.clone()).or_default(),
            results,
        );
    }

    /// Forgets a channel's sets (the channel was closed).
    pub fn forget_channel(&self, channel: &ChannelId) {
        self.write().channels.remove(channel);
    }

    /// The lookup order for `channel` right now: its own sets, then the global ones, enabled
    /// providers only, empty sets left out.
    pub fn resolver(&self, channel: &ChannelId) -> EmoteResolver {
        let state = self.read();
        let channel_slots = state.channels.get(channel);
        let sets = channel_slots
            .into_iter()
            .flatten()
            .chain(state.global.iter())
            .flatten()
            .filter(|set| state.enabled.is_enabled(set.provider) && !set.is_empty())
            .cloned()
            .collect();
        EmoteResolver { sets }
    }
}

/// A snapshot of the sets one channel resolves words against, in precedence order.
#[derive(Debug, Clone, Default)]
pub struct EmoteResolver {
    sets: Vec<Arc<EmoteSet>>,
}

impl EmoteResolver {
    /// `true` if no set has any emote (nothing can match).
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }
}

impl WordResolver for EmoteResolver {
    fn resolve(&self, word: &str) -> Option<Arc<Emote>> {
        self.sets
            .iter()
            .find_map(|set| set.get(word))
            .map(|entry| entry.emote.clone())
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::ImageUrls;

    use super::*;
    use crate::error::EmoteError;
    use crate::set::{EmoteScope, SetEmote};

    fn set(provider: EmoteProvider, scope: EmoteScope, names: &[&str]) -> EmoteSet {
        let emotes = names
            .iter()
            .map(|name| SetEmote {
                emote: Arc::new(Emote {
                    id: format!("{}-{name}", provider.name()).into(),
                    name: (*name).into(),
                    provider,
                    urls: ImageUrls::new(),
                    animated: false,
                    zero_width: false,
                    width: None,
                    height: None,
                }),
                original_name: None,
                owner: None,
            })
            .collect();
        EmoteSet::new(provider, scope, emotes)
    }

    fn results(
        scope: EmoteScope,
        seven_tv: &[&str],
        bttv: &[&str],
        ffz: &[&str],
    ) -> ProviderResults {
        ProviderResults {
            seven_tv: Ok(set(EmoteProvider::SevenTv, scope, seven_tv)),
            bttv: Ok(set(EmoteProvider::Bttv, scope, bttv)),
            ffz: Ok(set(EmoteProvider::Ffz, scope, ffz)),
        }
    }

    /// The provider and id a word resolves to.
    fn who(resolver: &EmoteResolver, word: &str) -> Option<String> {
        resolver.resolve(word).map(|e| e.id.as_str().to_owned())
    }

    fn registry() -> (EmoteRegistry, ChannelId) {
        let registry = EmoteRegistry::new(Providers::ALL);
        let channel = ChannelId::new("forsen");
        // Every name is in as many sets as its letters say: A everywhere, B in all but the
        // channel 7TV set, and so on, so each step down the order is observable.
        registry.store_channel(
            &channel,
            &results(EmoteScope::Channel, &["A"], &["A", "B"], &["A", "B", "C"]),
        );
        registry.store_global(&results(
            EmoteScope::Global,
            &["A", "B", "C", "D"],
            &["A", "B", "C", "D", "E"],
            &["A", "B", "C", "D", "E", "F"],
        ));
        (registry, channel)
    }

    #[test]
    fn precedence_is_channel_then_global_each_7tv_bttv_ffz() {
        let (registry, channel) = registry();
        let r = registry.resolver(&channel);
        assert_eq!(who(&r, "A").as_deref(), Some("7TV-A"));
        assert_eq!(who(&r, "B").as_deref(), Some("BTTV-B"));
        assert_eq!(who(&r, "C").as_deref(), Some("FFZ-C"));
        // Past every channel set: global 7TV, BTTV, FFZ.
        assert_eq!(who(&r, "D").as_deref(), Some("7TV-D"));
        assert_eq!(who(&r, "E").as_deref(), Some("BTTV-E"));
        assert_eq!(who(&r, "F").as_deref(), Some("FFZ-F"));
        assert_eq!(who(&r, "G"), None);
        // Case-sensitive, whole word.
        assert_eq!(who(&r, "a"), None);
    }

    #[test]
    fn channel_sets_do_not_leak_into_other_channels() {
        let (registry, _) = registry();
        let other = registry.resolver(&ChannelId::new("pajlada"));
        // Only the global sets: A is the global 7TV one.
        assert_eq!(who(&other, "A").as_deref(), Some("7TV-A"));
        assert_eq!(who(&other, "B").as_deref(), Some("7TV-B"));
        let unknown = EmoteRegistry::default().resolver(&ChannelId::new("x"));
        assert!(unknown.is_empty());
    }

    #[test]
    fn a_disabled_provider_is_skipped_and_the_next_one_answers() {
        let (registry, channel) = registry();
        registry.set_enabled(Providers {
            seven_tv: false,
            ..Providers::ALL
        });
        let r = registry.resolver(&channel);
        assert_eq!(who(&r, "A").as_deref(), Some("BTTV-A"));
        assert_eq!(who(&r, "D").as_deref(), Some("BTTV-D"));
        registry.set_enabled(Providers {
            seven_tv: false,
            bttv: false,
            ffz: false,
        });
        assert!(registry.resolver(&channel).is_empty());
        // Back on: nothing had to be loaded again.
        registry.set_enabled(Providers::ALL);
        assert_eq!(
            who(&registry.resolver(&channel), "A").as_deref(),
            Some("7TV-A")
        );
    }

    #[test]
    fn a_failed_provider_keeps_its_previous_set() {
        let (registry, channel) = registry();
        let mut reload = results(EmoteScope::Channel, &["Z"], &[], &[]);
        reload.bttv = Err(EmoteError::Status(500));
        reload.ffz = Err(EmoteError::Status(500));
        registry.store_channel(&channel, &reload);
        let r = registry.resolver(&channel);
        assert_eq!(who(&r, "Z").as_deref(), Some("7TV-Z"), "the new 7TV set");
        assert_eq!(
            who(&r, "B").as_deref(),
            Some("BTTV-B"),
            "kept after the failure"
        );
        assert_eq!(who(&r, "C").as_deref(), Some("FFZ-C"));
    }

    #[test]
    fn forgetting_a_channel_drops_only_its_sets() {
        let (registry, channel) = registry();
        registry.forget_channel(&channel);
        let r = registry.resolver(&channel);
        assert_eq!(who(&r, "B").as_deref(), Some("7TV-B"), "global sets stay");
    }

    #[test]
    fn providers_gain_detection() {
        let off = Providers {
            seven_tv: false,
            bttv: true,
            ffz: false,
        };
        assert!(Providers::ALL.gains_over(off));
        assert!(!off.gains_over(Providers::ALL));
        assert!(!off.gains_over(off));
        assert!(Providers::ALL.is_enabled(EmoteProvider::Twitch));
        assert!(!off.is_enabled(EmoteProvider::SevenTv));
    }
}
