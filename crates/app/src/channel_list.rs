//! The user's open channels: their order and which one is selected.
//!
//! Plain data with no GPUI in it, so every rule (no duplicates, what becomes selected after a
//! removal, reordering) is unit-tested. The shell owns one, renders it as the sidebar and writes
//! it to `state.toml` through [`ChannelList::to_saved`].

use gpui_chat_core::ChannelId;

use crate::login::normalise_login;
use crate::state::SavedChannels;

/// How far a channel moves in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelList {
    channels: Vec<ChannelId>,
    selected: Option<ChannelId>,
}

impl ChannelList {
    /// The saved list. Every entry goes through [`normalise_login`], so a hand-edited `Forsen` or
    /// `#forsen` still restores as `forsen`; entries that are still not a login, and duplicates,
    /// are dropped. The saved selection is used only if that channel is in the list; otherwise
    /// the first channel is selected.
    pub fn from_saved(saved: &SavedChannels) -> Self {
        let mut list = Self::default();
        for login in &saved.open {
            if let Ok(channel) = normalise_login(login) {
                list.append(channel);
            }
        }
        list.selected = saved
            .selected
            .as_deref()
            .and_then(|login| normalise_login(login).ok())
            .filter(|selected| list.contains(selected))
            .or_else(|| list.channels.first().cloned());
        list
    }

    pub fn to_saved(&self) -> SavedChannels {
        SavedChannels {
            open: self
                .channels
                .iter()
                .map(|channel| channel.login().to_owned())
                .collect(),
            selected: self
                .selected
                .as_ref()
                .map(|channel| channel.login().to_owned()),
        }
    }

    pub fn channels(&self) -> &[ChannelId] {
        &self.channels
    }

    pub fn selected(&self) -> Option<&ChannelId> {
        self.selected.as_ref()
    }

    pub fn contains(&self, channel: &ChannelId) -> bool {
        self.channels.contains(channel)
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// Appends `channel` without touching the selection; `false` if it is already open.
    pub fn append(&mut self, channel: ChannelId) -> bool {
        if self.contains(&channel) {
            return false;
        }
        self.channels.push(channel);
        true
    }

    /// Selects the first channel, if there is one.
    pub fn select_first(&mut self) {
        self.selected = self.channels.first().cloned();
    }

    /// Appends `channel` at the end and selects it. A channel that is already open is only
    /// selected (not moved). Returns whether it was newly added.
    pub fn add(&mut self, channel: ChannelId) -> bool {
        let added = self.append(channel.clone());
        self.selected = Some(channel);
        added
    }

    /// Selects `channel` if it is open.
    pub fn select(&mut self, channel: &ChannelId) -> bool {
        if self.contains(channel) {
            self.selected = Some(channel.clone());
            true
        } else {
            false
        }
    }

    /// Removes `channel`. If it was selected, the channel that takes its place (the next one,
    /// else the previous one) is selected.
    pub fn remove(&mut self, channel: &ChannelId) -> bool {
        let Some(index) = self.channels.iter().position(|c| c == channel) else {
            return false;
        };
        self.channels.remove(index);
        if self.selected.as_ref() == Some(channel) {
            self.selected = self
                .channels
                .get(index)
                .or_else(|| index.checked_sub(1).and_then(|i| self.channels.get(i)))
                .cloned();
        }
        true
    }

    /// Moves `channel` one place; `false` if it is not open or already at that end.
    pub fn move_channel(&mut self, channel: &ChannelId, direction: Move) -> bool {
        let Some(index) = self.channels.iter().position(|c| c == channel) else {
            return false;
        };
        let target = match direction {
            Move::Up => index.checked_sub(1),
            Move::Down => (index + 1 < self.channels.len()).then_some(index + 1),
        };
        let Some(target) = target else { return false };
        self.channels.swap(index, target);
        true
    }

    /// The channel `steps` places after the selection, wrapping; `None` when empty.
    pub fn neighbour(&self, steps: isize) -> Option<&ChannelId> {
        let len = self.channels.len() as isize;
        if len == 0 {
            return None;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|s| self.channels.iter().position(|c| c == s))
            .map_or(0, |i| i as isize);
        self.channels
            .get((current + steps).rem_euclid(len) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(name: &str) -> ChannelId {
        ChannelId::new(name)
    }

    fn list(names: &[&str]) -> ChannelList {
        let mut list = ChannelList::default();
        for name in names {
            list.add(ch(name));
        }
        list
    }

    fn logins(list: &ChannelList) -> Vec<&str> {
        list.channels().iter().map(ChannelId::login).collect()
    }

    #[test]
    fn add_appends_selects_and_ignores_duplicates() {
        let mut list = ChannelList::default();
        assert!(list.add(ch("alpha")));
        assert!(list.add(ch("beta")));
        assert_eq!(list.selected(), Some(&ch("beta")));
        assert!(!list.add(ch("alpha")), "already open");
        assert_eq!(logins(&list), ["alpha", "beta"]);
        assert_eq!(list.selected(), Some(&ch("alpha")), "re-adding selects it");
    }

    #[test]
    fn removing_the_selection_selects_the_next_then_the_previous() {
        let mut list = list(&["a", "b", "c"]);
        list.select(&ch("b"));
        assert!(list.remove(&ch("b")));
        assert_eq!(list.selected(), Some(&ch("c")));
        assert!(list.remove(&ch("c")));
        assert_eq!(list.selected(), Some(&ch("a")));
        assert!(list.remove(&ch("a")));
        assert_eq!(list.selected(), None);
        assert!(!list.remove(&ch("a")));
    }

    #[test]
    fn removing_another_channel_keeps_the_selection() {
        let mut list = list(&["a", "b", "c"]);
        list.select(&ch("c"));
        list.remove(&ch("a"));
        assert_eq!(list.selected(), Some(&ch("c")));
    }

    #[test]
    fn select_requires_an_open_channel() {
        let mut list = list(&["a", "b"]);
        assert!(!list.select(&ch("zzz")));
        assert_eq!(list.selected(), Some(&ch("b")));
    }

    #[test]
    fn moving_swaps_with_the_neighbour_and_stops_at_the_ends() {
        let mut list = list(&["a", "b", "c"]);
        assert!(list.move_channel(&ch("c"), Move::Up));
        assert_eq!(logins(&list), ["a", "c", "b"]);
        assert!(list.move_channel(&ch("a"), Move::Down));
        assert_eq!(logins(&list), ["c", "a", "b"]);
        assert!(!list.move_channel(&ch("c"), Move::Up));
        assert!(!list.move_channel(&ch("b"), Move::Down));
        assert!(!list.move_channel(&ch("nope"), Move::Up));
        assert_eq!(list.selected(), Some(&ch("c")), "moving does not select");
    }

    #[test]
    fn neighbour_wraps() {
        let mut list = list(&["a", "b", "c"]);
        list.select(&ch("c"));
        assert_eq!(list.neighbour(1), Some(&ch("a")));
        assert_eq!(list.neighbour(-1), Some(&ch("b")));
        list.select(&ch("a"));
        assert_eq!(list.neighbour(-1), Some(&ch("c")));
        assert_eq!(ChannelList::default().neighbour(1), None);
    }

    #[test]
    fn saved_roundtrip_keeps_order_and_selection() {
        let mut original = list(&["a_b", "ccc", "ddd"]);
        original.select(&ch("ccc"));
        assert_eq!(ChannelList::from_saved(&original.to_saved()), original);
    }

    #[test]
    fn restoring_drops_junk_and_duplicates_and_fixes_the_selection() {
        let saved = SavedChannels {
            open: vec![
                "forsen".into(),
                "Not Valid".into(),
                "xqc".into(),
                "forsen".into(),
                String::new(),
                "#Hash_Tag".into(),
                "FORSEN".into(),
                "https://www.twitch.tv/directory".into(),
            ],
            selected: Some("gone".into()),
        };
        let list = ChannelList::from_saved(&saved);
        assert_eq!(logins(&list), ["forsen", "xqc", "hash_tag"]);
        assert_eq!(list.selected(), Some(&ch("forsen")));

        let list = ChannelList::from_saved(&SavedChannels {
            selected: Some("XQC".into()),
            ..saved
        });
        assert_eq!(
            list.selected(),
            Some(&ch("xqc")),
            "selection is normalised too"
        );
        assert_eq!(
            ChannelList::from_saved(&SavedChannels::default()),
            ChannelList::default()
        );
    }
}
