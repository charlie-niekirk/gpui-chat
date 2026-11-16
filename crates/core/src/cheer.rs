//! Cheermotes (PR 5.1): a channel's set (prefix, tiers, one image per tier) and the pass that
//! gives the `Cheer` fragments of a message their images. Data and text work only; the Helix
//! request that fills a set is in `gpui-chat-twitch`, the drawing in the app.

use std::collections::HashMap;
use std::sync::Arc;

use crate::emote::Emote;
use crate::message::Fragment;
use crate::tokenizer::{for_each_word, joins_word, split_cheer};

/// One tier of a cheermote: used for amounts from `min_bits` up to the next tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheerTier {
    pub min_bits: u32,
    /// The tier's image, as an emote named by the cheermote's prefix.
    pub image: Arc<Emote>,
}

/// A cheermote: `Cheer`, `Kappa`, or a channel's own prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cheermote {
    pub prefix: String,
    pub tiers: Vec<CheerTier>,
}

/// Every cheermote usable in one channel, found by prefix (case-insensitive).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheermoteSet {
    by_prefix: HashMap<String, Cheermote>,
}

impl CheermoteSet {
    /// Builds a set. Tiers are sorted by their minimum, a cheermote without tiers is dropped, and
    /// a prefix given twice keeps the first.
    pub fn new(cheermotes: impl IntoIterator<Item = Cheermote>) -> Self {
        let mut by_prefix = HashMap::new();
        for mut cheermote in cheermotes {
            if cheermote.tiers.is_empty() || cheermote.prefix.is_empty() {
                continue;
            }
            cheermote.tiers.sort_by_key(|tier| tier.min_bits);
            by_prefix
                .entry(cheermote.prefix.to_ascii_lowercase())
                .or_insert(cheermote);
        }
        Self { by_prefix }
    }

    pub fn is_empty(&self) -> bool {
        self.by_prefix.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_prefix.len()
    }

    /// The tier that draws `bits` of the cheermote called `prefix`: the largest whose minimum is
    /// not above the amount.
    pub fn tier(&self, prefix: &str, bits: u32) -> Option<&CheerTier> {
        let cheermote = self.by_prefix.get(&prefix.to_ascii_lowercase())?;
        let n = cheermote
            .tiers
            .partition_point(|tier| tier.min_bits <= bits);
        cheermote.tiers.get(n.checked_sub(1)?)
    }

    fn is_prefix(&self, word: &str) -> bool {
        self.by_prefix.contains_key(&word.to_ascii_lowercase())
    }

    /// `fragments` with the cheermotes of this set drawn: a `Cheer` fragment gets the image of its
    /// tier, and a word that is a prefix of this set and an amount (`Cheer100`, also a channel's
    /// own prefix the tokenizer does not know) in a text run becomes one. `None` when nothing
    /// changed. Call it for messages that carry bits only (without bits `Cheer100` is just text),
    /// with the message's `bits` as `max_bits`: Twitch counts the cheermotes of a message from the
    /// left and a word only is one while the running total stays within the amount actually
    /// cheered (cheermotes the tokenizer already found count towards it, too), so `Cheer100`
    /// typed after the bits ran out stays text.
    pub fn resolve(&self, fragments: &[Fragment], max_bits: u32) -> Option<Vec<Fragment>> {
        if self.is_empty() {
            return None;
        }
        let mut used: u32 = 0;
        let mut out: Vec<Fragment> = Vec::with_capacity(fragments.len());
        let mut changed = false;
        let push_text = |out: &mut Vec<Fragment>, text: &str| {
            if text.is_empty() {
                return;
            }
            match out.last_mut() {
                Some(Fragment::Text(last)) => last.push_str(text),
                _ => out.push(Fragment::Text(text.to_owned())),
            }
        };
        for (i, fragment) in fragments.iter().enumerate() {
            match fragment {
                Fragment::Cheer {
                    prefix,
                    bits,
                    image: None,
                } => match self.tier(prefix, *bits) {
                    Some(tier) if used.saturating_add(*bits) <= max_bits => {
                        used += bits;
                        changed = true;
                        out.push(Fragment::Cheer {
                            prefix: prefix.clone(),
                            bits: *bits,
                            image: Some(tier.image.clone()),
                        });
                    }
                    _ => {
                        used = used.saturating_add(*bits);
                        out.push(fragment.clone());
                    }
                },
                Fragment::Cheer { bits, .. } => {
                    used = used.saturating_add(*bits);
                    out.push(fragment.clone());
                }
                Fragment::Text(text) => {
                    let mut pos = 0;
                    for_each_word(text, |range| {
                        let word = &text[range.clone()];
                        // A word glued to an emoji, mention or link is not a whole word.
                        let glued = (range.start == 0
                            && i > 0
                            && fragments.get(i - 1).is_some_and(joins_word))
                            || (range.end == text.len()
                                && fragments.get(i + 1).is_some_and(joins_word));
                        let found = (!glued)
                            .then(|| split_cheer(word, |p| self.is_prefix(p)))
                            .flatten()
                            .filter(|&(_, bits)| used.saturating_add(bits) <= max_bits)
                            .and_then(|(prefix, bits)| {
                                self.tier(prefix, bits).map(|tier| (prefix, bits, tier))
                            });
                        push_text(&mut out, &text[pos..range.start]);
                        pos = range.end;
                        match found {
                            Some((prefix, bits, tier)) => {
                                changed = true;
                                used += bits;
                                out.push(Fragment::Cheer {
                                    prefix: prefix.to_owned(),
                                    bits,
                                    image: Some(tier.image.clone()),
                                });
                            }
                            None => push_text(&mut out, word),
                        }
                    });
                    push_text(&mut out, &text[pos..]);
                }
                other => out.push(other.clone()),
            }
        }
        changed.then_some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote::EmoteProvider;
    use crate::ids::EmoteId;

    fn tier(min_bits: u32, name: &str) -> CheerTier {
        let mut emote = Emote::twitch(EmoteId::from(format!("{name}{min_bits}")), name);
        emote.provider = EmoteProvider::Twitch;
        CheerTier {
            min_bits,
            image: Arc::new(emote),
        }
    }

    fn set() -> CheermoteSet {
        CheermoteSet::new([
            Cheermote {
                prefix: "Cheer".into(),
                // Deliberately out of order.
                tiers: vec![tier(100, "Cheer"), tier(1, "Cheer"), tier(1000, "Cheer")],
            },
            Cheermote {
                prefix: "streamerCheer".into(),
                tiers: vec![tier(1, "streamerCheer")],
            },
            Cheermote {
                prefix: "empty".into(),
                tiers: Vec::new(),
            },
        ])
    }

    fn text(s: &str) -> Fragment {
        Fragment::Text(s.into())
    }

    #[test]
    fn a_tier_is_the_largest_minimum_not_above_the_amount() {
        let set = set();
        assert_eq!(set.len(), 2, "a cheermote without tiers is dropped");
        let min = |bits| set.tier("Cheer", bits).map(|t| t.min_bits);
        assert_eq!(min(1), Some(1));
        assert_eq!(min(99), Some(1));
        assert_eq!(min(100), Some(100));
        assert_eq!(min(999_999), Some(1000));
        assert_eq!(min(0), None);
        assert_eq!(
            set.tier("cHeEr", 5).map(|t| t.min_bits),
            Some(1),
            "prefixes match in any case"
        );
        assert!(set.tier("nope", 5).is_none());
    }

    #[test]
    fn words_in_text_become_cheermotes_including_a_channels_own_prefix() {
        let resolved = set()
            .resolve(&[text("gg streamerCheer50 cheer1000 Cheer0 hello")], 1050)
            .expect("changed");
        let cheers: Vec<_> = resolved
            .iter()
            .filter_map(|f| match f {
                Fragment::Cheer {
                    prefix,
                    bits,
                    image,
                } => Some((prefix.as_str(), *bits, image.as_ref().unwrap().id.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            cheers,
            [
                ("streamerCheer", 50, "streamerCheer1"),
                ("cheer", 1000, "Cheer1000")
            ]
        );
        // The text between is kept exactly, `Cheer0` (no amount) stays text.
        let plain: String = resolved
            .iter()
            .map(|f| {
                let mut s = String::new();
                f.write_plain_text(&mut s);
                s
            })
            .collect();
        assert_eq!(plain, "gg streamerCheer50 cheer1000 Cheer0 hello");
    }

    #[test]
    fn a_cheer_the_tokenizer_found_gets_its_image() {
        let fragments = [Fragment::Cheer {
            prefix: "Cheer".into(),
            bits: 150,
            image: None,
        }];
        let resolved = set().resolve(&fragments, 150).unwrap();
        assert!(matches!(
            &resolved[0],
            Fragment::Cheer { image: Some(i), .. } if i.id.as_str() == "Cheer100"
        ));
        // Nothing to do: no change reported.
        assert!(set().resolve(&resolved, 150).is_none());
        assert!(set().resolve(&[text("hello")], 150).is_none());
        assert!(CheermoteSet::default().resolve(&fragments, 150).is_none());
    }

    #[test]
    fn only_as_many_words_are_cheermotes_as_the_bits_paid_for() {
        let resolved = |text_: &str, bits| {
            set()
                .resolve(&[text(text_)], bits)
                .map(|f| {
                    f.iter()
                        .filter_map(|f| match f {
                            Fragment::Cheer { prefix, bits, .. } => Some(format!("{prefix}{bits}")),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        // 100 bits paid: the first word uses them all, the second is text.
        assert_eq!(resolved("Cheer100 Cheer100", 100), ["Cheer100"]);
        assert_eq!(resolved("Cheer100 Cheer100", 200), ["Cheer100", "Cheer100"]);
        // A word that would go over the total is text, a later smaller one still fits.
        assert_eq!(resolved("Cheer150 Cheer50", 100), ["Cheer50"]);
        // No bits, no cheermotes.
        assert!(resolved("Cheer1", 0).is_empty());
        // Cheers the tokenizer already found count towards the total.
        let fragments = [
            Fragment::Cheer {
                prefix: "Cheer".into(),
                bits: 100,
                image: None,
            },
            text(" Cheer100"),
        ];
        let out = set().resolve(&fragments, 100).unwrap();
        assert_eq!(
            out.iter()
                .filter(|f| matches!(f, Fragment::Cheer { image: Some(_), .. }))
                .count(),
            1,
            "the second word is text"
        );
    }

    #[test]
    fn a_word_glued_to_an_emoji_is_not_a_cheer() {
        let fragments = [
            text("Cheer100"),
            Fragment::Emoji {
                text: "👍".into()
            },
        ];
        assert!(set().resolve(&fragments, 100).is_none());
    }
}
