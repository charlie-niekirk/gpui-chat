//! The immutable chat message and its fragments.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::emote::{BadgeId, Emote};
use crate::ids::MessageId;
use crate::special::{EventKind, SharedChat};

/// Milliseconds since the Unix epoch (the IRC `tmi-sent-ts` tag, or a clock reading
/// taken by whoever creates the message). Core never reads the clock itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn from_unix_millis(ms: i64) -> Self {
        Self(ms)
    }

    pub fn unix_millis(self) -> i64 {
        self.0
    }
}

/// An sRGB colour (a user's name colour).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses `#RRGGBB` (the `#` is optional). Anything else, including the empty
    /// string Twitch sends for "no colour set", is `None`.
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#').unwrap_or(s);
        if s.len() != 6 || !s.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        Some(Self::new(byte(0)?, byte(2)?, byte(4)?))
    }

    /// `#rrggbb`.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Who sent a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    /// Lowercase login (`forsen`).
    pub login: String,
    /// Display name with the user's own capitalisation (may be non-ASCII).
    pub display_name: String,
    /// Twitch numeric user id, when known.
    pub user_id: Option<String>,
    /// `None` when the user has not picked a colour; the UI chooses a default.
    pub color: Option<Rgb>,
    /// Badges in IRC order (images are resolved at render time).
    pub badges: Vec<BadgeId>,
}

impl Author {
    pub fn new(login: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            login: login.into(),
            display_name: display_name.into(),
            user_id: None,
            color: None,
            badges: Vec::new(),
        }
    }
}

/// What sort of line this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// A normal chat message.
    Chat,
    /// `/me` message: the whole line is rendered in the author's colour.
    Action,
    /// Server `NOTICE` text (no author).
    Notice,
    /// `USERNOTICE` (sub, raid, announcement, ...): `system_text` holds the
    /// system line, `fragments` the optional user-typed message, `author` the user.
    UserNotice,
    /// Locally generated line (connection status, "chat cleared", ...). No author.
    System,
}

/// Per-message flags. Mutations after creation go through [`Message::with_deleted`]
/// (messages themselves are immutable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageFlags {
    /// Mentions the local user (set by the tokenizer's `mentions_self`).
    pub mention: bool,
    /// The author's first message in this channel.
    pub first_message: bool,
    /// Highlighted by a rule or a channel-points highlight.
    pub highlighted: bool,
    /// Removed by a moderator / timeout / ban; kept so the UI can strike it through.
    pub deleted: bool,
    /// Came from recent-messages history rather than live chat.
    pub history: bool,
    /// The app's own stand-in for a message the user just sent, shown until Twitch's copy of it
    /// (same id, with the real emotes, colour and badges) arrives and replaces it.
    pub echo: bool,
}

/// The message this one replies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRef {
    pub parent_id: MessageId,
    pub parent_login: String,
    pub parent_display_name: String,
    pub parent_body: String,
    /// Root of the thread when the parent is itself a reply.
    pub thread_parent_id: Option<MessageId>,
}

impl ReplyRef {
    /// The reference a reply to `parent` carries (what Twitch's own copy of the reply would have
    /// in its `reply-parent-*` / `reply-thread-parent-*` tags). `None` for a line nobody wrote
    /// (notice, system line). Replying to a reply joins its thread, so the root is the parent's
    /// own root; replying to anything else starts one rooted at the parent.
    pub fn to(parent: &Message) -> Option<Self> {
        let author = parent.author.as_ref()?;
        Some(Self {
            parent_id: parent.id.clone(),
            parent_login: author.login.clone(),
            parent_display_name: author.display_name.clone(),
            parent_body: parent.plain_text(),
            thread_parent_id: Some(parent.thread_root().clone()),
        })
    }

    /// The root of the thread this reply belongs to: the thread parent, or the parent itself when
    /// the tags name no other root.
    pub fn root_id(&self) -> &MessageId {
        self.thread_parent_id.as_ref().unwrap_or(&self.parent_id)
    }

    /// The name to show for the parent's author.
    pub fn parent_name(&self) -> &str {
        if self.parent_display_name.trim().is_empty() {
            &self.parent_login
        } else {
            &self.parent_display_name
        }
    }
}

/// One rendered piece of a message body. See [`crate::tokenizer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fragment {
    /// Plain text, whitespace included. Adjacent plain words are merged into one run.
    Text(String),
    /// An emote, with any zero-width emotes stacked on top of it.
    Emote {
        emote: Arc<Emote>,
        zero_width_overlays: Vec<Arc<Emote>>,
    },
    /// One emoji *grapheme*: ZWJ sequences, skin tones, flags and keycaps stay whole.
    Emoji { text: String },
    /// `@login`. `text` is as typed (with the `@`), `login` is lowercase.
    Mention {
        text: String,
        login: String,
        /// Names the local user.
        is_self: bool,
    },
    /// A link. `text` is as typed; `url` always has a scheme (bare domains get `https://`).
    Link { text: String, url: String },
    /// A cheermote such as `Cheer100`: prefix as typed and the bit amount. `image` is the
    /// cheermote's image for that amount (a tier of its set, named by the prefix) once the
    /// channel's cheermotes are known ([`crate::CheermoteSet::resolve`]); without it the text is
    /// drawn.
    Cheer {
        prefix: String,
        bits: u32,
        image: Option<Arc<Emote>>,
    },
}

impl Fragment {
    /// Appends the text this fragment stands for (overlays are appended after a
    /// space; whitespace between an emote and its overlays is not preserved).
    pub fn write_plain_text(&self, out: &mut String) {
        match self {
            Self::Text(t) => out.push_str(t),
            Self::Emote {
                emote,
                zero_width_overlays,
            } => {
                out.push_str(&emote.name);
                for overlay in zero_width_overlays {
                    out.push(' ');
                    out.push_str(&overlay.name);
                }
            }
            Self::Emoji { text } | Self::Mention { text, .. } | Self::Link { text, .. } => {
                out.push_str(text);
            }
            Self::Cheer { prefix, bits, .. } => {
                out.push_str(prefix);
                let _ = write!(out, "{bits}");
            }
        }
    }
}

/// The raw IRC tags of a message, kept so later features (PR 5.1 and others) can read
/// tags core does not model. Cheap to clone (shared), keeps the order it was built
/// with, empty by default. Values are stored as the transport hands them over: the
/// Twitch backend unescapes them (`\s` is already a space) and orders the pairs by key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawTags(Arc<[(String, String)]>);

impl RawTags {
    pub fn new(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self(pairs.into_iter().collect())
    }

    /// The value of the first tag called `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find_map(|(k, v)| (k == key).then_some(v.as_str()))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A chat message. Immutable; share it as `Arc<Message>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub timestamp: Timestamp,
    pub kind: MessageKind,
    /// `None` for [`MessageKind::Notice`] and [`MessageKind::System`].
    pub author: Option<Author>,
    pub fragments: Vec<Fragment>,
    /// The `system-msg` line of a [`MessageKind::UserNotice`].
    pub system_text: Option<String>,
    /// The `msg-id` of a [`MessageKind::UserNotice`] (`sub`, `resub`, `raid`,
    /// `announcement`, ...) or [`MessageKind::Notice`].
    pub notice_id: Option<String>,
    /// Bits cheered with this message (the IRC `bits` tag).
    pub bits: Option<u32>,
    /// What a [`MessageKind::UserNotice`] announced (PR 5.1); `None` for every other kind.
    pub event: Option<EventKind>,
    /// The channel-points custom reward this message redeemed (`custom-reward-id`; Twitch does
    /// not send its name).
    pub reward_id: Option<String>,
    /// Where the message was sent when it comes from a shared chat session.
    pub shared: Option<SharedChat>,
    /// All raw IRC tags as received; empty for synthetic messages.
    pub raw_tags: RawTags,
    pub flags: MessageFlags,
    pub reply: Option<ReplyRef>,
}

impl Message {
    /// A chat message with default flags.
    pub fn chat(
        id: impl Into<MessageId>,
        timestamp: Timestamp,
        author: Author,
        fragments: Vec<Fragment>,
    ) -> Self {
        Self {
            id: id.into(),
            timestamp,
            kind: MessageKind::Chat,
            author: Some(author),
            fragments,
            system_text: None,
            notice_id: None,
            bits: None,
            event: None,
            reward_id: None,
            shared: None,
            raw_tags: RawTags::default(),
            flags: MessageFlags::default(),
            reply: None,
        }
    }

    /// An author-less line of plain text of the given kind
    /// ([`MessageKind::Notice`] or [`MessageKind::System`]).
    pub fn plain(
        id: impl Into<MessageId>,
        timestamp: Timestamp,
        kind: MessageKind,
        text: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            timestamp,
            kind,
            author: None,
            fragments: vec![Fragment::Text(text.into())],
            system_text: None,
            notice_id: None,
            bits: None,
            event: None,
            reward_id: None,
            shared: None,
            raw_tags: RawTags::default(),
            flags: MessageFlags::default(),
            reply: None,
        }
    }

    /// A copy with `flags.deleted` set. Used by the store, since stored messages
    /// are shared and never mutated in place.
    #[must_use]
    pub fn with_deleted(&self) -> Self {
        let mut copy = self.clone();
        copy.flags.deleted = true;
        copy
    }

    /// The id of the thread this message is in: its reply's root, or its own id when it is not a
    /// reply (a message with replies is the root of its thread).
    pub fn thread_root(&self) -> &MessageId {
        self.reply.as_ref().map_or(&self.id, ReplyRef::root_id)
    }

    /// The body as plain text (see [`Fragment::write_plain_text`]).
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for fragment in &self.fragments {
            fragment.write_plain_text(&mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_hex_round_trip() {
        let c = Rgb::from_hex("#1E90FF").unwrap();
        assert_eq!(c, Rgb::new(0x1e, 0x90, 0xff));
        assert_eq!(c.to_hex(), "#1e90ff");
        assert_eq!(Rgb::from_hex("1e90ff"), Some(c));
        assert_eq!(Rgb::from_hex(""), None);
        assert_eq!(Rgb::from_hex("#12345"), None);
        assert_eq!(Rgb::from_hex("#gggggg"), None);
        assert_eq!(Rgb::from_hex("#éééé"), None);
    }

    #[test]
    fn raw_tags_lookup_and_cheap_clone() {
        let tags = RawTags::new([
            ("msg-id".to_owned(), "raid".to_owned()),
            ("bits".to_owned(), "100".to_owned()),
        ]);
        assert_eq!(tags.get("bits"), Some("100"));
        assert_eq!(tags.get("nope"), None);
        assert_eq!(
            tags.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            ["msg-id", "bits"]
        );
        assert!(RawTags::default().is_empty());
        let mut m = Message::plain("1", Timestamp(0), MessageKind::Notice, "x");
        m.raw_tags = tags.clone();
        assert!(
            Arc::ptr_eq(&m.clone().raw_tags.0, &tags.0),
            "clone shares the tags"
        );
        assert_eq!((m.notice_id.as_deref(), m.bits), (None, None));
    }

    fn reply_to(id: &str, parent: &str, root: Option<&str>) -> Message {
        let mut m = Message::chat(
            id,
            Timestamp(0),
            Author::new("replier", "Replier"),
            vec![Fragment::Text("hi".into())],
        );
        m.reply = Some(ReplyRef {
            parent_id: parent.into(),
            parent_login: "p".into(),
            parent_display_name: String::new(),
            parent_body: "body".into(),
            thread_parent_id: root.map(MessageId::from),
        });
        m
    }

    #[test]
    fn a_thread_root_is_the_tagged_root_the_parent_or_the_message_itself() {
        let plain = Message::chat("m1", Timestamp(0), Author::new("a", "A"), vec![]);
        assert_eq!(plain.thread_root().as_str(), "m1");
        assert_eq!(
            reply_to("m2", "m1", Some("m0")).thread_root().as_str(),
            "m0"
        );
        // No thread tag: the parent is the root.
        assert_eq!(reply_to("m2", "m1", None).thread_root().as_str(), "m1");
        let reply = reply_to("m2", "m1", None).reply.unwrap();
        assert_eq!(reply.parent_name(), "p", "falls back to the login");
    }

    #[test]
    fn replying_to_a_message_builds_the_reference_twitch_would_send_back() {
        let mut parent = Message::chat(
            "m1",
            Timestamp(0),
            Author::new("alice", "Alice"),
            vec![Fragment::Text("what is the game".into())],
        );
        let reference = ReplyRef::to(&parent).unwrap();
        assert_eq!(reference.parent_id.as_str(), "m1");
        assert_eq!(reference.parent_name(), "Alice");
        assert_eq!(reference.parent_body, "what is the game");
        assert_eq!(
            reference.root_id().as_str(),
            "m1",
            "a new thread is rooted at the parent"
        );
        // Replying to a reply joins its thread.
        parent.reply = reply_to("m1", "m0", Some("m0")).reply;
        assert_eq!(ReplyRef::to(&parent).unwrap().root_id().as_str(), "m0");
        // Nothing to reply to on a line without an author.
        let notice = Message::plain("n", Timestamp(0), MessageKind::Notice, "x");
        assert_eq!(ReplyRef::to(&notice), None);
    }

    #[test]
    fn with_deleted_leaves_original_untouched() {
        let m = Message::plain("1", Timestamp(0), MessageKind::System, "hi");
        let d = m.with_deleted();
        assert!(!m.flags.deleted);
        assert!(d.flags.deleted);
        assert_eq!(d.plain_text(), "hi");
    }
}
