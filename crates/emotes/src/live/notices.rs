//! The quiet system line for live emote changes: "7TV: added PogU, KEKW · removed Foo".
//!
//! Changes are collected per channel and provider and written out together once the first of them
//! is [`WINDOW`] old, so an editor adding twenty emotes makes one line, not twenty. Time is
//! passed in, so the tests need no clock.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_chat_core::{ChannelId, EmoteProvider};

use crate::set::EmoteChange;

/// How long after a change the line waits for more.
pub const WINDOW: Duration = Duration::from_millis(1500);
/// Names listed per kind before "and N more".
const MAX_NAMES: usize = 5;

#[derive(Default)]
struct Pending {
    since: Option<Instant>,
    added: Vec<String>,
    removed: Vec<String>,
    renamed: Vec<(String, String)>,
}

/// Collects applied changes until they are due.
#[derive(Default)]
pub struct Notices {
    pending: HashMap<(ChannelId, EmoteProvider), Pending>,
}

impl Notices {
    /// Notes `changes` (as [`crate::EmoteSet::with_changes`] reports them) at `now`.
    pub fn push(
        &mut self,
        channel: &ChannelId,
        provider: EmoteProvider,
        changes: &[EmoteChange],
        now: Instant,
    ) {
        if changes.is_empty() {
            return;
        }
        let entry = self.pending.entry((channel.clone(), provider)).or_default();
        entry.since.get_or_insert(now);
        for change in changes {
            match change {
                EmoteChange::Added(e) => entry.added.push(e.emote.name.to_string()),
                EmoteChange::Removed { id, name } => entry
                    .removed
                    .push(name.as_deref().unwrap_or(id.as_str()).to_owned()),
                EmoteChange::Renamed { id, from, to } => entry.renamed.push((
                    from.as_deref().unwrap_or(id.as_str()).to_owned(),
                    to.to_string(),
                )),
            }
        }
    }

    /// When [`Self::take_due`] next has something, if anything is waiting.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending
            .values()
            .filter_map(|p| p.since)
            .min()
            .map(|since| since + WINDOW)
    }

    /// The lines that are due at `now`, oldest group first.
    pub fn take_due(&mut self, now: Instant) -> Vec<(ChannelId, String)> {
        let mut due: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.since.is_some_and(|since| now >= since + WINDOW))
            .map(|(key, p)| (key.clone(), p.since))
            .collect();
        due.sort_by_key(|(_, since)| *since);
        due.into_iter()
            .filter_map(|((channel, provider), _)| {
                let pending = self.pending.remove(&(channel.clone(), provider))?;
                Some((channel, line(provider, &pending)))
            })
            .collect()
    }

    /// Forgets everything pending for `channel` (it was closed).
    pub fn forget(&mut self, channel: &ChannelId) {
        self.pending.retain(|(c, _), _| c != channel);
    }
}

fn list(names: &[String]) -> String {
    let shown = names.iter().take(MAX_NAMES).cloned().collect::<Vec<_>>();
    let mut text = shown.join(", ");
    if names.len() > MAX_NAMES {
        text.push_str(&format!(" and {} more", names.len() - MAX_NAMES));
    }
    text
}

fn line(provider: EmoteProvider, pending: &Pending) -> String {
    let mut parts = Vec::new();
    if !pending.added.is_empty() {
        parts.push(format!("added {}", list(&pending.added)));
    }
    if !pending.removed.is_empty() {
        parts.push(format!("removed {}", list(&pending.removed)));
    }
    if !pending.renamed.is_empty() {
        let pairs: Vec<String> = pending
            .renamed
            .iter()
            .map(|(from, to)| format!("{from} → {to}"))
            .collect();
        parts.push(format!("renamed {}", list(&pairs)));
    }
    format!("{}: {}", provider.name(), parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_chat_core::{Emote, ImageUrls};

    use super::*;
    use crate::set::SetEmote;

    fn added(name: &str) -> EmoteChange {
        EmoteChange::Added(SetEmote {
            emote: Arc::new(Emote {
                id: name.into(),
                name: name.into(),
                provider: EmoteProvider::SevenTv,
                urls: ImageUrls::new(),
                animated: false,
                zero_width: false,
                width: None,
                height: None,
            }),
            original_name: None,
            owner: None,
        })
    }

    fn removed(name: &str) -> EmoteChange {
        EmoteChange::Removed {
            id: name.into(),
            name: Some(name.into()),
        }
    }

    #[test]
    fn changes_within_the_window_make_one_line() {
        let mut notices = Notices::default();
        let c = ChannelId::new("forsen");
        let t = Instant::now();
        notices.push(&c, EmoteProvider::SevenTv, &[added("PogU")], t);
        notices.push(
            &c,
            EmoteProvider::SevenTv,
            &[added("KEKW"), removed("Old")],
            t + Duration::from_millis(400),
        );
        assert_eq!(notices.next_due(), Some(t + WINDOW));
        assert!(notices.take_due(t + Duration::from_millis(1000)).is_empty());
        let due = notices.take_due(t + WINDOW);
        assert_eq!(due, [(c, "7TV: added PogU, KEKW · removed Old".to_owned())]);
        assert_eq!(notices.next_due(), None);
    }

    #[test]
    fn providers_and_channels_have_lines_of_their_own() {
        let mut notices = Notices::default();
        let (a, b) = (ChannelId::new("a"), ChannelId::new("b"));
        let t = Instant::now();
        notices.push(&a, EmoteProvider::SevenTv, &[added("X")], t);
        notices.push(
            &a,
            EmoteProvider::Bttv,
            &[removed("Y")],
            t + Duration::from_millis(10),
        );
        notices.push(
            &b,
            EmoteProvider::Bttv,
            &[added("Z")],
            t + Duration::from_millis(20),
        );
        let lines: Vec<_> = notices
            .take_due(t + WINDOW + Duration::from_secs(1))
            .into_iter()
            .map(|(c, l)| format!("{c}: {l}"))
            .collect();
        assert_eq!(
            lines,
            ["a: 7TV: added X", "a: BTTV: removed Y", "b: BTTV: added Z"]
        );
    }

    #[test]
    fn long_lists_are_cut_and_renames_show_both_names() {
        let mut notices = Notices::default();
        let c = ChannelId::new("c");
        let t = Instant::now();
        let many: Vec<_> = (0..8).map(|i| added(&format!("E{i}"))).collect();
        notices.push(&c, EmoteProvider::SevenTv, &many, t);
        notices.push(
            &c,
            EmoteProvider::SevenTv,
            &[EmoteChange::Renamed {
                id: "i".into(),
                from: Some("Old".into()),
                to: "New".into(),
            }],
            t,
        );
        let (_, line) = notices.take_due(t + WINDOW).remove(0);
        assert_eq!(
            line,
            "7TV: added E0, E1, E2, E3, E4 and 3 more · renamed Old → New"
        );
    }

    #[test]
    fn a_closed_channel_has_nothing_pending() {
        let mut notices = Notices::default();
        let c = ChannelId::new("c");
        notices.push(&c, EmoteProvider::Bttv, &[added("X")], Instant::now());
        notices.forget(&c);
        assert_eq!(notices.next_due(), None);
    }
}
