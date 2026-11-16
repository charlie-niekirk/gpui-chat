//! System notifications for highlight hits (PR 5.3): what they say, how clicking one finds its
//! message again, and how often they may appear. Plain data and functions, tested without a window;
//! [`crate::attention`] decides *when* and [`crate::platform::notify`] hands them to the system.
//!
//! The operating system is only asked (once, by GPUI, the first time something is posted) for the
//! right to show notifications when the first one is about to appear, never at startup.

use std::time::{Duration, Instant};

use gpui_chat_core::{ChannelId, HitReason, Message, MessageId};

use crate::chat::display::author_name;

/// The shortest time between two notifications. Hits inside the gap are held and come as one
/// summary at its end, so a busy chat full of matches cannot flood the notification centre.
pub const MIN_GAP: Duration = Duration::from_secs(5);

/// Longest notification body, in characters.
const BODY_CHARS: usize = 160;

/// What to put in a notification for `message`, hit in `channel`. Without `previews` it only says
/// there is a new highlight in the channel: no name and no text reaches a locked screen.
pub fn describe(
    message: &Message,
    channel: &ChannelId,
    reason: HitReason,
    previews: bool,
) -> (String, String) {
    if !previews {
        return (format!("New highlight in #{channel}"), String::new());
    }
    let author = message.author.as_ref().map_or("Someone", author_name);
    let title = match reason {
        HitReason::Mention => format!("{author} mentioned you in #{channel}"),
        HitReason::Rule(_) | HitReason::User(_) => format!("{author} in #{channel}"),
    };
    (title, body(&message.plain_text()))
}

/// `text` on one line, cut to [`BODY_CHARS`].
fn body(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= BODY_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(BODY_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// What clicking a notification should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The message that raised it, in its channel.
    Message(ChannelId, MessageId),
    /// The Mentions view (the summary notification).
    Mentions,
}

const MESSAGE_TAG: &str = "hit";
const SUMMARY_TAG: &str = "summary";

/// The system notification's identity for a message; clicking it brings [`Target::Message`] back.
pub fn message_tag(channel: &ChannelId, id: &MessageId) -> String {
    format!("{MESSAGE_TAG}:{channel}:{id}")
}

/// The identity of the summary notification (a newer one replaces the older).
pub fn summary_tag() -> String {
    SUMMARY_TAG.to_owned()
}

/// What a tag from [`message_tag`] / [`summary_tag`] means; `None` for anything else.
pub fn parse_tag(tag: &str) -> Option<Target> {
    if tag == SUMMARY_TAG {
        return Some(Target::Mentions);
    }
    let mut parts = tag.splitn(3, ':');
    match (parts.next()?, parts.next()?, parts.next()?) {
        (MESSAGE_TAG, channel, id) if !channel.is_empty() && !id.is_empty() => Some(
            Target::Message(ChannelId::new(channel), MessageId::from(id)),
        ),
        _ => None,
    }
}

/// `3 more highlights in #a, #b` for the hits held back.
pub fn summary_text(count: usize, channels: &[ChannelId]) -> (String, String) {
    let title = format!(
        "{count} more {}",
        if count == 1 {
            "highlight"
        } else {
            "highlights"
        }
    );
    let shown: Vec<String> = channels.iter().take(3).map(|c| format!("#{c}")).collect();
    let more = channels.len().saturating_sub(shown.len());
    let mut body = format!("in {}", shown.join(", "));
    if more > 0 {
        body.push_str(&format!(" and {more} more"));
    }
    (title, body)
}

/// Rate limit for notifications: at most one per [`MIN_GAP`], the rest counted and summarised.
#[derive(Debug, Default)]
pub struct Coalescer {
    last: Option<Instant>,
    /// The hits held back, oldest first.
    held: Vec<(ChannelId, MessageId)>,
}

/// What to do with a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Post it now.
    Send,
    /// Counted for the summary; see [`Coalescer::next_summary`].
    Held,
}

impl Coalescer {
    /// A notification-worthy hit, message `id` in `channel`, at `now`.
    pub fn offer(&mut self, now: Instant, channel: &ChannelId, id: &MessageId) -> Verdict {
        if self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= MIN_GAP)
            && self.held.is_empty()
        {
            self.last = Some(now);
            return Verdict::Send;
        }
        self.held.push((channel.clone(), id.clone()));
        Verdict::Held
    }

    /// A held message was removed by a moderator: it is not in the summary any more. Whether it
    /// was held.
    pub fn forget(&mut self, channel: &ChannelId, id: &MessageId) -> bool {
        let before = self.held.len();
        self.held.retain(|(c, m)| !(c == channel && m == id));
        self.held.len() != before
    }

    /// Nothing is held (the settings switched notifications off).
    pub fn clear(&mut self) {
        self.held.clear();
    }

    /// When the summary of the held hits is due, if there are any.
    pub fn summary_due(&self) -> Option<Instant> {
        (!self.held.is_empty()).then(|| self.last.map_or_else(Instant::now, |last| last + MIN_GAP))
    }

    /// The held hits as one summary, once its time has come: how many and in which channels.
    pub fn next_summary(&mut self, now: Instant) -> Option<(usize, Vec<ChannelId>)> {
        if self.summary_due().is_none_or(|due| now < due) {
            return None;
        }
        self.last = Some(now);
        let held = std::mem::take(&mut self.held);
        let mut channels: Vec<ChannelId> = Vec::new();
        for (channel, _) in &held {
            if !channels.contains(channel) {
                channels.push(channel.clone());
            }
        }
        Some((held.len(), channels))
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{Author, Fragment, Timestamp};

    use super::*;

    fn message(author: &str, text: &str) -> Message {
        Message::chat(
            "m1",
            Timestamp(0),
            Author::new(author.to_lowercase(), author),
            vec![Fragment::Text(text.into())],
        )
    }

    #[test]
    fn a_notification_names_who_where_and_what() {
        let channel = ChannelId::new("forsen");
        let (title, body) = describe(
            &message("Alice", "hey @viewer\nlook   at this"),
            &channel,
            HitReason::Mention,
            true,
        );
        assert_eq!(title, "Alice mentioned you in #forsen");
        assert_eq!(body, "hey @viewer look at this");
        let (title, _) = describe(&message("Bob", "hi"), &channel, HitReason::Rule(0), true);
        assert_eq!(title, "Bob in #forsen");
        // Without previews nothing of the message or its author is in it.
        let secret = message("Alice", "my password is hunter2");
        let (title, body) = describe(&secret, &channel, HitReason::Mention, false);
        assert_eq!(title, "New highlight in #forsen");
        assert_eq!(body, "");
    }

    #[test]
    fn a_long_body_is_cut_on_a_character_boundary() {
        let text = "é".repeat(500);
        let body = body(&text);
        assert_eq!(body.chars().count(), BODY_CHARS);
        assert!(body.ends_with('…'));
    }

    #[test]
    fn a_tag_leads_back_to_its_message() {
        let channel = ChannelId::new("forsen");
        let id = MessageId::from("0c1d-2e3f");
        let tag = message_tag(&channel, &id);
        assert_eq!(parse_tag(&tag), Some(Target::Message(channel, id)));
        assert_eq!(parse_tag(&summary_tag()), Some(Target::Mentions));
        for bad in ["", "hit", "hit:", "hit:a:", "hit::b", "other:a:b"] {
            assert_eq!(parse_tag(bad), None, "{bad:?}");
        }
        // An id may hold the separator.
        assert_eq!(
            parse_tag("hit:a:b:c"),
            Some(Target::Message(ChannelId::new("a"), MessageId::from("b:c")))
        );
    }

    fn id(n: u32) -> MessageId {
        MessageId::from(format!("m{n}"))
    }

    #[test]
    fn the_first_hit_is_sent_and_a_burst_is_held_for_one_summary() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        let (a, b) = (ChannelId::new("a"), ChannelId::new("b"));
        assert_eq!(c.offer(t0, &a, &id(0)), Verdict::Send);
        assert_eq!(c.summary_due(), None);
        let secs = Duration::from_secs;
        assert_eq!(c.offer(t0 + secs(1), &a, &id(1)), Verdict::Held);
        assert_eq!(c.offer(t0 + secs(2), &b, &id(2)), Verdict::Held);
        assert_eq!(c.offer(t0 + secs(3), &a, &id(3)), Verdict::Held);
        assert_eq!(c.summary_due(), Some(t0 + MIN_GAP));
        // Too early: nothing.
        assert_eq!(c.next_summary(t0 + secs(4)), None);
        let (count, channels) = c.next_summary(t0 + MIN_GAP).unwrap();
        assert_eq!((count, channels), (3, vec![a.clone(), b]));
        assert_eq!(c.next_summary(t0 + MIN_GAP), None, "said once");
        // The summary counts as a notification: a hit right after is held again.
        assert_eq!(c.offer(t0 + MIN_GAP + secs(1), &a, &id(4)), Verdict::Held);
    }

    #[test]
    fn a_removed_message_leaves_the_held_summary_and_an_empty_one_is_not_sent() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        let (a, b) = (ChannelId::new("a"), ChannelId::new("b"));
        c.offer(t0, &a, &id(0));
        c.offer(t0, &a, &id(1));
        c.offer(t0, &b, &id(2));
        assert!(c.forget(&a, &id(1)));
        assert!(!c.forget(&a, &id(1)), "gone already");
        assert!(!c.forget(&b, &id(1)), "another channel's id is not it");
        assert!(c.forget(&b, &id(2)));
        assert_eq!(c.next_summary(t0 + MIN_GAP), None, "nothing left to say");
        assert_eq!(c.summary_due(), None);
        // Only the channels that still have a held hit are named.
        c.offer(t0 + MIN_GAP, &a, &id(3));
        c.offer(t0 + MIN_GAP, &b, &id(4));
        c.offer(t0 + MIN_GAP, &b, &id(5));
        c.forget(&a, &id(3));
        assert_eq!(c.next_summary(t0 + MIN_GAP * 2), Some((2, vec![b])));
        c.offer(t0 + MIN_GAP * 3, &a, &id(6));
        c.offer(t0 + MIN_GAP * 3, &a, &id(7));
        c.clear();
        assert_eq!(c.next_summary(t0 + MIN_GAP * 9), None);
    }

    #[test]
    fn after_a_quiet_spell_the_next_hit_is_sent_at_once() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        let a = ChannelId::new("a");
        assert_eq!(c.offer(t0, &a, &id(0)), Verdict::Send);
        assert_eq!(c.offer(t0 + MIN_GAP, &a, &id(1)), Verdict::Send);
    }

    #[test]
    fn a_summary_says_how_many_and_where() {
        let a = ChannelId::new("a");
        assert_eq!(
            summary_text(1, std::slice::from_ref(&a)),
            ("1 more highlight".to_owned(), "in #a".to_owned())
        );
        let many: Vec<_> = ["a", "b", "c", "d", "e"].map(ChannelId::new).to_vec();
        assert_eq!(
            summary_text(9, &many),
            (
                "9 more highlights".to_owned(),
                "in #a, #b, #c and 2 more".to_owned()
            )
        );
    }
}
