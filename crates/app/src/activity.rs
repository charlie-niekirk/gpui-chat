//! What the user has not seen yet (PR 5.3): unread messages and mentions per channel, and the log
//! the Mentions view shows. Plain data, tested without a window; [`crate::attention`] feeds it.
//!
//! A channel is **being watched** when a pane shows it and the window is the active one. Messages
//! of a watched channel are seen by definition and count for nothing; messages of any other
//! channel count as unread, and a highlight hit among them as an unseen mention. Showing a channel
//! in the *focused* pane while the window is active (switching to it, focusing its pane, coming
//! back to the window) clears both. A channel in another, unfocused pane is watched too, but its
//! backlog (what came while it was hidden) stays until its pane is focused: the dots are cleared by
//! the user's attention, not by a pane merely being on screen.
//!
//! Never counted, whatever the channel: recent-messages history, the user's own messages and
//! messages that are already removed. A message removed later leaves the Mentions view and no
//! longer counts as an unseen mention.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use gpui_chat_core::{ChannelId, HighlightColor, Message, MessageFlags, MessageId, MessageKind};

/// Hits the Mentions view remembers.
pub const MENTION_LOG: usize = 500;

/// One highlight hit in the Mentions view.
#[derive(Debug, Clone)]
pub struct MentionEntry {
    pub channel: ChannelId,
    pub message: Arc<Message>,
    /// The message's text, read once when the hit is logged (the panel draws it every frame).
    pub text: Arc<str>,
    pub color: HighlightColor,
    /// The user has not looked at it yet (it arrived while the channel was not watched).
    pub unseen: bool,
}

/// Whether `message` can count as new activity: a line somebody wrote (chat, `/me`, the text of an
/// event) that is live, not the user's own echo and not removed.
pub fn is_countable(message: &Message) -> bool {
    matches!(
        message.kind,
        MessageKind::Chat | MessageKind::Action | MessageKind::UserNotice
    ) && counts(&message.flags)
}

fn counts(flags: &MessageFlags) -> bool {
    !flags.history && !flags.echo && !flags.deleted
}

#[derive(Debug, Default)]
pub struct Activity {
    unread: HashMap<ChannelId, u32>,
    /// Oldest first.
    mentions: VecDeque<MentionEntry>,
    /// Unseen entries of `mentions` per channel, kept in step so the sidebar asks in O(1).
    unseen: HashMap<ChannelId, usize>,
    /// Bumped whenever the log changes (an entry added, dropped or marked seen), so the Mentions
    /// panel rebuilds its rows only then.
    generation: u64,
    /// How many hits were ever logged (never goes down), so a view can tell how many are new
    /// since it last looked even while others leave the log.
    added: u64,
    /// Channels shown by a pane.
    visible: HashSet<ChannelId>,
    /// The channel of the focused pane.
    focused: Option<ChannelId>,
    window_active: bool,
}

impl Activity {
    /// Tells it what is on screen. Returns whether anything the sidebar or the Mentions view shows
    /// changed (the focused channel's activity is cleared).
    pub fn set_view(
        &mut self,
        visible: impl IntoIterator<Item = ChannelId>,
        focused: Option<ChannelId>,
        window_active: bool,
    ) -> bool {
        self.visible = visible.into_iter().collect();
        self.focused = focused;
        self.window_active = window_active;
        match self.focused.clone().filter(|_| window_active) {
            Some(channel) => self.mark_seen(&channel),
            None => false,
        }
    }

    /// Whether the user is looking at `channel` right now.
    pub fn is_watched(&self, channel: &ChannelId) -> bool {
        self.window_active && self.visible.contains(channel)
    }

    /// Clears the unread count and the unseen mentions of `channel`; whether there was any.
    pub fn mark_seen(&mut self, channel: &ChannelId) -> bool {
        let mut changed = self.unread.remove(channel).is_some();
        if self.unseen.remove(channel).is_some() {
            for entry in self.mentions.iter_mut().filter(|e| &e.channel == channel) {
                entry.unseen = false;
            }
            self.generation += 1;
            changed = true;
        }
        changed
    }

    /// A countable message arrived in `channel`. Returns whether the unread state changed (it is
    /// shown as a dot, so only the first message of a run does).
    pub fn note_message(&mut self, channel: &ChannelId) -> bool {
        if self.is_watched(channel) {
            return false;
        }
        let count = self.unread.entry(channel.clone()).or_insert(0);
        *count = count.saturating_add(1);
        *count == 1
    }

    /// A highlight hit arrived in `channel`: it goes in the Mentions log (a watched channel's is
    /// already seen). Returns whether an unseen mention was added.
    pub fn note_hit(
        &mut self,
        channel: &ChannelId,
        message: Arc<Message>,
        color: HighlightColor,
    ) -> bool {
        let unseen = !self.is_watched(channel);
        if self.mentions.len() >= MENTION_LOG
            && let Some(old) = self.mentions.pop_front()
        {
            self.uncount(&old);
        }
        if unseen {
            *self.unseen.entry(channel.clone()).or_insert(0) += 1;
        }
        self.generation += 1;
        self.added += 1;
        self.mentions.push_back(MentionEntry {
            channel: channel.clone(),
            text: message.plain_text().into(),
            message,
            color,
            unseen,
        });
        unseen
    }

    /// Unread messages in `channel`.
    pub fn unread(&self, channel: &ChannelId) -> u32 {
        self.unread.get(channel).copied().unwrap_or(0)
    }

    /// Mentions of `channel` the user has not looked at.
    pub fn unseen_mentions(&self, channel: &ChannelId) -> usize {
        self.unseen.get(channel).copied().unwrap_or(0)
    }

    /// Hits logged so far, ever.
    pub fn added(&self) -> u64 {
        self.added
    }

    /// Changes whenever the Mentions log does; see [`Activity::mentions`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The Mentions view's rows, newest first.
    pub fn mentions(&self) -> impl Iterator<Item = &MentionEntry> {
        self.mentions.iter().rev()
    }

    /// How many hits are logged.
    #[cfg(test)]
    pub fn mention_count(&self) -> usize {
        self.mentions.len()
    }

    /// Hits nobody has looked at, across all channels.
    #[cfg(test)]
    pub fn total_unseen_mentions(&self) -> usize {
        self.mentions.iter().filter(|e| e.unseen).count()
    }

    /// A moderator deleted the message: it leaves the log. The ids that were in it.
    pub fn message_removed(&mut self, channel: &ChannelId, id: &MessageId) -> Vec<MessageId> {
        self.retain(|e| !(&e.channel == channel && e.message.id == *id))
    }

    /// `login` was timed out or banned in `channel` (or, with `None`, the whole chat was cleared):
    /// their hits leave the log. The ids that were in it.
    pub fn user_removed(&mut self, channel: &ChannelId, login: Option<&str>) -> Vec<MessageId> {
        self.retain(|e| {
            &e.channel != channel
                || login.is_some_and(|login| {
                    e.message
                        .author
                        .as_ref()
                        .is_none_or(|a| !a.login.eq_ignore_ascii_case(login))
                })
        })
    }

    /// The user ignored someone (PR 5.7): their hits leave the log, and stop counting as unseen
    /// mentions. Whether anything left.
    pub fn drop_ignored(&mut self, ignores: &gpui_chat_core::Ignores) -> bool {
        if ignores.is_empty() {
            return false;
        }
        !self.retain(|e| !ignores.is_ignored(&e.message)).is_empty()
    }

    /// The channel was closed: nothing of it is left to show.
    pub fn forget(&mut self, channel: &ChannelId) -> bool {
        let unread = self.unread.remove(channel).is_some();
        let log = !self.retain(|e| &e.channel != channel).is_empty();
        self.unseen.remove(channel);
        unread || log
    }

    /// Keeps the entries `keep` says yes to; the ids of the ones dropped.
    fn retain(&mut self, keep: impl Fn(&MentionEntry) -> bool) -> Vec<MessageId> {
        let mut dropped = Vec::new();
        let mut kept = VecDeque::with_capacity(self.mentions.len());
        for entry in std::mem::take(&mut self.mentions) {
            if keep(&entry) {
                kept.push_back(entry);
            } else {
                self.uncount(&entry);
                dropped.push(entry.message.id.clone());
            }
        }
        self.mentions = kept;
        if !dropped.is_empty() {
            self.generation += 1;
        }
        dropped
    }

    /// An entry leaves the log: it no longer counts as unseen.
    fn uncount(&mut self, entry: &MentionEntry) {
        if entry.unseen
            && let Some(count) = self.unseen.get_mut(&entry.channel)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.unseen.remove(&entry.channel);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{Author, Fragment, Timestamp};

    use super::*;

    fn channel(login: &str) -> ChannelId {
        ChannelId::new(login)
    }

    fn message(id: &str, login: &str) -> Arc<Message> {
        Arc::new(Message::chat(
            id,
            Timestamp(0),
            Author::new(login, login),
            vec![Fragment::Text("hi".into())],
        ))
    }

    fn watching(visible: &[&str], focused: Option<&str>, active: bool) -> Activity {
        let mut a = Activity::default();
        a.set_view(
            visible.iter().map(|c| channel(c)),
            focused.map(channel),
            active,
        );
        a
    }

    #[test]
    fn only_live_lines_somebody_wrote_and_that_stand_count() {
        let chat = message("1", "alice");
        assert!(is_countable(&chat));
        let mut history = (*chat).clone();
        history.flags.history = true;
        let mut echo = (*chat).clone();
        echo.flags.echo = true;
        let removed = chat.with_deleted();
        let system = Message::plain("s", Timestamp(0), MessageKind::System, "x");
        let notice = Message::plain("n", Timestamp(0), MessageKind::Notice, "x");
        for not in [&history, &echo, &removed, &system, &notice] {
            assert!(!is_countable(not), "{not:?}");
        }
    }

    #[test]
    fn a_channel_that_is_not_watched_collects_unread_and_the_dot_changes_once() {
        let mut a = watching(&["a"], Some("a"), true);
        assert!(
            !a.note_message(&channel("a")),
            "the focused channel is seen"
        );
        assert!(a.note_message(&channel("b")));
        assert!(
            !a.note_message(&channel("b")),
            "already unread: no change to show"
        );
        assert_eq!((a.unread(&channel("a")), a.unread(&channel("b"))), (0, 2));
    }

    #[test]
    fn nothing_is_watched_while_the_window_is_not_active() {
        let mut a = watching(&["a"], Some("a"), false);
        assert!(a.note_message(&channel("a")));
        assert!(a.note_hit(&channel("a"), message("1", "x"), HighlightColor::Accent));
        assert_eq!(a.unseen_mentions(&channel("a")), 1);
        // Coming back clears the focused channel.
        assert!(a.set_view([channel("a")], Some(channel("a")), true));
        assert_eq!(
            (a.unread(&channel("a")), a.unseen_mentions(&channel("a"))),
            (0, 0)
        );
        assert_eq!(a.mention_count(), 1, "the hit stays in the log, seen");
    }

    #[test]
    fn a_hit_in_a_watched_channel_is_logged_as_seen() {
        let mut a = watching(&["a"], Some("a"), true);
        assert!(!a.note_hit(&channel("a"), message("1", "x"), HighlightColor::Red));
        assert_eq!(a.mention_count(), 1);
        assert_eq!(a.total_unseen_mentions(), 0);
        assert!(a.note_hit(&channel("b"), message("2", "x"), HighlightColor::Red));
        assert_eq!(a.total_unseen_mentions(), 1);
    }

    #[test]
    fn focusing_the_channel_clears_it_but_merely_showing_it_does_not() {
        let mut a = watching(&["a", "b"], Some("a"), true);
        a.set_view([channel("a")], Some(channel("a")), true);
        a.note_message(&channel("b"));
        a.note_hit(&channel("b"), message("1", "x"), HighlightColor::Accent);
        // `b` is now shown in a second pane, unfocused: messages stop counting, the backlog stays.
        assert!(!a.set_view([channel("a"), channel("b")], Some(channel("a")), true));
        assert!(!a.note_message(&channel("b")), "visible: seen on arrival");
        assert_eq!(
            (a.unread(&channel("b")), a.unseen_mentions(&channel("b"))),
            (1, 1)
        );
        // Focusing its pane clears it.
        assert!(a.set_view([channel("a"), channel("b")], Some(channel("b")), true));
        assert_eq!(
            (a.unread(&channel("b")), a.unseen_mentions(&channel("b"))),
            (0, 0)
        );
        assert!(!a.set_view([channel("a"), channel("b")], Some(channel("b")), true));
    }

    #[test]
    fn the_log_is_newest_first_and_bounded() {
        let mut a = Activity::default();
        for n in 0..MENTION_LOG + 3 {
            a.note_hit(
                &channel("a"),
                message(&n.to_string(), "x"),
                HighlightColor::Accent,
            );
        }
        assert_eq!(a.mention_count(), MENTION_LOG);
        let ids: Vec<_> = a
            .mentions()
            .map(|e| e.message.id.as_str().to_owned())
            .collect();
        assert_eq!(ids[0], (MENTION_LOG + 2).to_string());
        assert_eq!(ids.last().unwrap(), "3", "the oldest ones were dropped");
    }

    #[test]
    fn a_removed_message_leaves_the_log_and_stops_counting_as_a_mention() {
        let mut a = Activity::default();
        a.note_hit(&channel("a"), message("1", "troll"), HighlightColor::Accent);
        a.note_hit(
            &channel("a"),
            message("2", "friend"),
            HighlightColor::Accent,
        );
        a.note_hit(&channel("b"), message("3", "troll"), HighlightColor::Accent);
        assert_eq!(a.message_removed(&channel("a"), &"2".into()), ["2".into()]);
        assert!(a.message_removed(&channel("a"), &"2".into()).is_empty());
        assert_eq!(a.unseen_mentions(&channel("a")), 1);
        // A timeout removes that user's hits in that channel only.
        assert_eq!(a.user_removed(&channel("a"), Some("TROLL")), ["1".into()]);
        assert_eq!(a.unseen_mentions(&channel("a")), 0);
        assert_eq!(a.unseen_mentions(&channel("b")), 1);
        // A clear removes the channel's.
        assert_eq!(a.user_removed(&channel("b"), None), ["3".into()]);
        assert_eq!(a.mention_count(), 0);
        assert_eq!(a.total_unseen_mentions(), 0);
    }

    #[test]
    fn the_counters_follow_the_log_through_eviction_seeing_and_the_generation() {
        let mut a = Activity::default();
        let g0 = a.generation();
        for n in 0..MENTION_LOG + 10 {
            a.note_hit(
                &channel("a"),
                message(&n.to_string(), "x"),
                HighlightColor::Accent,
            );
        }
        assert_eq!(
            a.unseen_mentions(&channel("a")),
            MENTION_LOG,
            "evicted ones stop counting"
        );
        assert_eq!(a.unseen_mentions(&channel("a")), a.total_unseen_mentions());
        let g1 = a.generation();
        assert!(g1 > g0);
        assert!(a.mark_seen(&channel("a")));
        assert_eq!(a.unseen_mentions(&channel("a")), 0);
        assert!(a.generation() > g1, "the rows' dots changed");
        let g2 = a.generation();
        assert!(!a.mark_seen(&channel("a")));
        assert_eq!(a.generation(), g2, "nothing changed, nothing to rebuild");
        // The text is read once.
        assert_eq!(&*a.mentions().next().unwrap().text, "hi");
    }

    #[test]
    fn the_added_count_only_goes_up() {
        let mut a = Activity::default();
        for n in 0..MENTION_LOG + 5 {
            a.note_hit(
                &channel("a"),
                message(&n.to_string(), "x"),
                HighlightColor::Accent,
            );
        }
        assert_eq!(
            a.added(),
            (MENTION_LOG + 5) as u64,
            "eviction does not lower it"
        );
        a.message_removed(&channel("a"), &"100".into());
        a.forget(&channel("a"));
        assert_eq!(a.added(), (MENTION_LOG + 5) as u64);
    }

    #[test]
    fn closing_a_channel_forgets_it() {
        let mut a = Activity::default();
        a.note_message(&channel("a"));
        a.note_hit(&channel("a"), message("1", "x"), HighlightColor::Accent);
        a.note_hit(&channel("b"), message("2", "x"), HighlightColor::Accent);
        assert!(a.forget(&channel("a")));
        assert_eq!((a.unread(&channel("a")), a.mention_count()), (0, 1));
        assert!(!a.forget(&channel("a")));
    }
}
