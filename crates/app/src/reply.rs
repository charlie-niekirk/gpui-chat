//! What replying to a message means for the composer (PR 3.4). Pure, so it is unit-tested; the
//! views over it are [`crate::composer::Composer`] (the reply strip) and the chat view (the reply
//! action on a row, the thread popover).
//!
//! Replying puts the box in *reply mode*: the next message is sent with
//! `reply_parent_message_id` (Helix *Send Chat Message*) and, once Twitch accepts it, echoed with
//! the same reply reference an IRC copy would carry, so its context line shows at once. The box is
//! prefilled with `@name ` like Twitch's own chat does (Twitch does not add the mention itself);
//! the user may delete it. Cancelling leaves what the user typed alone, except for that untouched
//! prefill.

use gpui_chat_core::{Message, MessageKind, ReplyRef};

/// Whether `message` can be replied to: something a user wrote in chat. Notices, system lines and
/// sub / raid announcements have nothing to thread under; a deleted message is gone.
pub fn can_reply_to(message: &Message) -> bool {
    matches!(message.kind, MessageKind::Chat | MessageKind::Action)
        && message.author.is_some()
        && !message.flags.deleted
}

/// The message the box is replying to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTarget {
    /// What the reply will carry (and what its echo shows).
    pub reference: ReplyRef,
    prefill: String,
}

impl ReplyTarget {
    /// `None` when `message` cannot be replied to.
    pub fn new(message: &Message) -> Option<Self> {
        if !can_reply_to(message) {
            return None;
        }
        let reference = ReplyRef::to(message)?;
        let prefill = format!("@{} ", mention_name(&reference));
        Some(Self { reference, prefill })
    }

    /// The `@name ` the box starts with.
    #[cfg(test)]
    pub fn prefill(&self) -> &str {
        &self.prefill
    }

    /// The reply strip's text, `Replying to Name: what they said` (the view cuts it to the width).
    pub fn label(&self) -> String {
        let body = single_line(&self.reference.parent_body);
        if body.is_empty() {
            format!("Replying to {}", self.reference.parent_name())
        } else {
            format!("Replying to {}: {body}", self.reference.parent_name())
        }
    }

    /// What starting this reply puts in a box holding `typed` while `previous` was the reply
    /// target: the `@name ` prefill when the box is empty or still holds only the previous
    /// target's untouched prefill, `None` to leave what the user typed alone.
    pub fn text_to_start(&self, typed: &str, previous: Option<&Self>) -> Option<&str> {
        let untouched = previous.is_some_and(|previous| previous.clears_on_cancel(typed));
        (typed.is_empty() || untouched).then_some(self.prefill.as_str())
    }

    /// Whether cancelling should also empty the box: it still holds exactly the prefill, which the
    /// user never made their own.
    pub fn clears_on_cancel(&self, typed: &str) -> bool {
        typed == self.prefill
    }
}

/// The name `@`-mentioning the parent's author uses: the login, so a localised display name still
/// mentions the right user, except when the display name is only the login in another
/// capitalisation (`Alice` for `alice`), which reads better.
pub fn mention_name(reference: &ReplyRef) -> &str {
    let display = reference.parent_display_name.trim();
    if !display.is_empty() && display.eq_ignore_ascii_case(&reference.parent_login) {
        display
    } else if reference.parent_login.is_empty() {
        reference.parent_name()
    } else {
        &reference.parent_login
    }
}

/// The reply mode after a send failed: the target the failed send had, unless the user already
/// chose another message to reply to meanwhile.
pub fn restored(current: Option<ReplyTarget>, failed: Option<ReplyTarget>) -> Option<ReplyTarget> {
    current.or(failed)
}

/// `text` with every run of whitespace and control characters as one space, trimmed; the U+E0000
/// that some clients append to dodge duplicate detection is dropped.
pub fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split(|c: char| c.is_whitespace() || c.is_control() || c == '\u{E0000}') {
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{Author, Fragment, Timestamp};

    use super::*;

    fn chat(id: &str, text: &str) -> Message {
        Message::chat(
            id,
            Timestamp(0),
            Author::new("alice", "Alice"),
            vec![Fragment::Text(text.into())],
        )
    }

    #[test]
    fn only_what_a_user_wrote_can_be_replied_to() {
        assert!(can_reply_to(&chat("1", "hi")));
        let mut action = chat("2", "waves");
        action.kind = MessageKind::Action;
        assert!(can_reply_to(&action));
        for kind in [MessageKind::Notice, MessageKind::System] {
            assert!(!can_reply_to(&Message::plain("3", Timestamp(0), kind, "x")));
        }
        let mut notice = chat("4", "resub");
        notice.kind = MessageKind::UserNotice;
        assert!(!can_reply_to(&notice));
        assert!(!can_reply_to(&chat("5", "gone").with_deleted()));
        assert_eq!(ReplyTarget::new(&notice), None);
    }

    #[test]
    fn the_target_carries_the_reference_the_echo_needs() {
        let target = ReplyTarget::new(&chat("m1", "what is the game")).unwrap();
        assert_eq!(target.reference.parent_id.as_str(), "m1");
        assert_eq!(target.reference.parent_login, "alice");
        assert_eq!(target.reference.parent_body, "what is the game");
        assert_eq!(target.prefill(), "@Alice ");
        assert_eq!(target.label(), "Replying to Alice: what is the game");
    }

    #[test]
    fn replying_to_a_reply_joins_its_thread() {
        let mut parent = chat("m2", "it is chess");
        parent.reply = Some(ReplyRef {
            parent_id: "m1".into(),
            parent_login: "bob".into(),
            parent_display_name: "Bob".into(),
            parent_body: "what is the game".into(),
            thread_parent_id: Some("m0".into()),
        });
        let target = ReplyTarget::new(&parent).unwrap();
        assert_eq!(
            target.reference.parent_id.as_str(),
            "m2",
            "the clicked message is the parent"
        );
        assert_eq!(target.reference.root_id().as_str(), "m0");
    }

    #[test]
    fn the_label_is_one_line_and_copes_with_an_empty_body() {
        let target = ReplyTarget::new(&chat("m1", "first\n  second\tthird")).unwrap();
        assert_eq!(target.label(), "Replying to Alice: first second third");
        let target = ReplyTarget::new(&chat("m1", "")).unwrap();
        assert_eq!(target.label(), "Replying to Alice");
    }

    #[test]
    fn the_mention_is_the_login_unless_the_display_name_only_capitalises_it() {
        let named = |login: &str, display: &str| ReplyRef {
            parent_id: "p".into(),
            parent_login: login.into(),
            parent_display_name: display.into(),
            parent_body: String::new(),
            thread_parent_id: None,
        };
        assert_eq!(mention_name(&named("alice", "Alice")), "Alice");
        assert_eq!(mention_name(&named("alice", "alice")), "alice");
        // A localised display name is not something chat can mention.
        assert_eq!(mention_name(&named("hello_kr", "안녕")), "hello_kr");
        assert_eq!(mention_name(&named("alice", "")), "alice");
        assert_eq!(mention_name(&named("", "Alice")), "Alice");
        let mut localised = chat("m1", "hi");
        localised.author = Some(Author::new("hello_kr", "안녕"));
        assert_eq!(
            ReplyTarget::new(&localised).unwrap().prefill(),
            "@hello_kr "
        );
        // The strip still shows the name as chat does.
        assert_eq!(
            ReplyTarget::new(&localised).unwrap().label(),
            "Replying to 안녕: hi"
        );
    }

    #[test]
    fn starting_a_reply_replaces_an_untouched_prefill_but_keeps_typed_text() {
        let first = ReplyTarget::new(&chat("m1", "a")).unwrap();
        let mut other = chat("m2", "b");
        other.author = Some(Author::new("bob", "Bob"));
        let second = ReplyTarget::new(&other).unwrap();
        // An empty box starts with the prefill.
        assert_eq!(second.text_to_start("", None), Some("@Bob "));
        // Another reply replaces the previous one's untouched `@name `...
        assert_eq!(second.text_to_start("@Alice ", Some(&first)), Some("@Bob "));
        // ...but never what the user typed, with or without a previous reply.
        assert_eq!(second.text_to_start("@Alice well", Some(&first)), None);
        assert_eq!(second.text_to_start("something", None), None);
        // A prefill-looking text that was not a reply's is the user's.
        assert_eq!(second.text_to_start("@Alice ", None), None);
    }

    #[test]
    fn a_failed_send_restores_its_reply_unless_another_was_chosen() {
        let failed = ReplyTarget::new(&chat("m1", "a")).unwrap();
        let mut other = chat("m2", "b");
        other.author = Some(Author::new("bob", "Bob"));
        let newer = ReplyTarget::new(&other).unwrap();
        assert_eq!(restored(None, Some(failed.clone())), Some(failed.clone()));
        assert_eq!(
            restored(Some(newer.clone()), Some(failed.clone())),
            Some(newer)
        );
        assert_eq!(restored(None, None), None);
        // A send that was not a reply restores nothing.
        assert_eq!(restored(None, None), None);
    }

    #[test]
    fn cancelling_clears_only_an_untouched_prefill() {
        let target = ReplyTarget::new(&chat("m1", "hi")).unwrap();
        assert!(target.clears_on_cancel("@Alice "));
        assert!(!target.clears_on_cancel("@Alice well"));
        assert!(!target.clears_on_cancel(""));
        assert!(!target.clears_on_cancel("something else"));
    }
}
