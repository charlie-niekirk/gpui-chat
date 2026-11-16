//! The single line of text a message is drawn as, with a semantic [`Role`] per span.
//!
//! `[timestamp] name: body` for chat, `[timestamp] name body` for `/me`, `[timestamp] text` for
//! notices and system lines. Roles, not colours, are stored: colours are resolved at paint time
//! from the current theme, so a theme switch never invalidates a layout. Control characters
//! (newlines, tabs) become spaces so the text shapes as one line; the U+E0000 tag character some
//! clients append is dropped (the tokenizer already strips it from fragments; author-supplied
//! system text is covered here).

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use gpui_chat_core::{Author, Badge, Emote, Fragment, Message, MessageKind, ReplyRef};
use gpui_chat_media::ImageKey;

/// What a span of the display text is; decides its font weight and colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Timestamp,
    /// The author's name (bold, in their readability-adjusted colour).
    Name,
    /// `": "` after the name and the spaces between header parts.
    Separator,
    Text,
    /// Body text of a `/me` line, drawn in the author's colour.
    ActionText,
    /// `@someone` (bold).
    Mention,
    /// `@viewer`: a mention of the local user (bold, highlight colour).
    SelfMention,
    Link,
    /// An emote's name: drawn as an inline image (its glyphs only if the image failed). Every
    /// emote, overlays included, is a span of its own, listed in [`DisplayText::emotes`].
    Emote,
    /// A badge's title: drawn as an inline image, never as text (a badge without an image is not
    /// in the display text at all). Every badge is a span of its own, listed in
    /// [`DisplayText::badges`].
    Badge,
    Emoji,
    /// The amount of a cheermote (`100` of `Cheer100`; the word itself is an image, or text in
    /// this role when the channel's cheermotes are not known).
    Cheer,
    /// Notices and system lines (muted).
    System,
    /// The line above an event's message (PR 5.1): `Alice subscribed at Tier 1`, `Announcement`,
    /// `First message`. Drawn in the event's accent colour.
    Header,
    /// Where a shared-chat message was sent (`[OtherChan]`), muted.
    Source,
}

impl Role {
    pub fn is_bold(self) -> bool {
        matches!(
            self,
            Self::Name | Self::Mention | Self::SelfMention | Self::Cheer
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub role: Role,
}

/// One [`Role::Emote`] span and the emote it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayEmote {
    /// Index into [`DisplayText::spans`].
    pub span: usize,
    pub emote: Arc<Emote>,
    /// A zero-width emote stacked on the nearest preceding non-overlay entry (its base). The
    /// base, its overlays and the single spaces between them in the text make one stack, drawn
    /// as one box.
    pub overlay: bool,
}

/// A badge that is drawn: its definition and the image it is drawn from. The caller decides
/// which badges get a slot (the ones with an image that has not failed for good); the display
/// text has room for exactly those.
#[derive(Clone, Debug, PartialEq)]
pub struct BadgeSlot {
    pub badge: Arc<Badge>,
    /// The variant to draw.
    pub url: ImageKey,
    /// Logical side of the square box (badges are square).
    pub size: f32,
}

/// One [`Role::Badge`] span and the badge it names.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayBadge {
    /// Index into [`DisplayText::spans`].
    pub span: usize,
    pub slot: BadgeSlot,
}

/// A detected link: where its text is and what it opens. Two links with nothing between them
/// share one [`Role::Link`] span but stay two entries here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayLink {
    pub range: Range<usize>,
    pub url: Arc<str>,
}

/// The display text and its spans. Spans are contiguous, in order, and cover the whole text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisplayText {
    pub text: String,
    pub spans: Vec<Span>,
    /// Every [`Role::Emote`] span, in text order.
    pub emotes: Vec<DisplayEmote>,
    /// Every [`Role::Badge`] span, in text order (before the author's name).
    pub badges: Vec<DisplayBadge>,
    /// Every link, in text order.
    pub links: Vec<DisplayLink>,
    /// Byte offsets of the hard line breaks (each is one space character in `text`, so copying
    /// and hit-testing see a space), in text order.
    pub breaks: Vec<usize>,
}

/// Settings that change what is drawn (and therefore the layout).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DisplayOptions {
    pub timestamps: bool,
}

const IGNORED_CHAR: char = '\u{E0000}';

impl DisplayText {
    /// Appends `s` as `role`, merging with the previous span when the role is the same.
    fn push(&mut self, s: &str, role: Role) {
        let start = self.text.len();
        for c in s.chars() {
            match c {
                IGNORED_CHAR => {}
                c if c.is_control() => self.text.push(' '),
                c => self.text.push(c),
            }
        }
        let end = self.text.len();
        if end == start {
            return;
        }
        match self.spans.last_mut() {
            // Emotes and badges are never merged: each span is one image.
            Some(last)
                if last.role == role
                    && last.range.end == start
                    && !matches!(role, Role::Emote | Role::Badge) =>
            {
                last.range.end = end;
            }
            _ => self.spans.push(Span {
                range: start..end,
                role,
            }),
        }
    }

    /// Appends a hard line break (see [`DisplayText::breaks`]).
    fn push_break(&mut self) {
        self.breaks.push(self.text.len());
        self.push(" ", Role::Separator);
    }

    /// Appends an emote's name as its own [`Role::Emote`] span.
    fn push_emote(&mut self, emote: &Arc<Emote>, overlay: bool) {
        self.push_emote_as(&emote.name, emote, overlay);
    }

    /// Appends `text` as the [`Role::Emote`] span of `emote` (a cheermote drawn from the prefix as
    /// typed, `cheer`, not the set's `Cheer`).
    fn push_emote_as(&mut self, text: &str, emote: &Arc<Emote>, overlay: bool) {
        let span = self.spans.len();
        self.push(text, Role::Emote);
        if self.spans.len() > span {
            self.emotes.push(DisplayEmote {
                span,
                emote: emote.clone(),
                overlay,
            });
        }
    }

    /// Appends each badge's title as its own [`Role::Badge`] span, each followed by a space, so
    /// the name (or whatever follows) is a space away from the last one.
    fn push_badges(&mut self, slots: &[BadgeSlot]) {
        for slot in slots {
            let span = self.spans.len();
            self.push(&slot.badge.title, Role::Badge);
            // A badge without a title has no span to draw into.
            if self.spans.len() > span {
                self.badges.push(DisplayBadge {
                    span,
                    slot: slot.clone(),
                });
                self.push(" ", Role::Separator);
            }
        }
    }

    /// The link whose text contains byte `index`.
    pub fn link_at(&self, index: usize) -> Option<&DisplayLink> {
        let i = self.links.partition_point(|link| link.range.end <= index);
        self.links.get(i).filter(|link| link.range.start <= index)
    }

    /// Index of the span containing byte `index`.
    pub fn span_at(&self, index: usize) -> Option<usize> {
        let i = self.spans.partition_point(|span| span.range.end <= index);
        (i < self.spans.len() && self.spans[i].range.start <= index).then_some(i)
    }
}

/// The text of a reply's context line: `↳ Replying to Name: what they said`. One line: runs of
/// whitespace and control characters become one space, and a parent whose text Twitch did not send
/// is just named.
pub fn reply_text(reply: &ReplyRef) -> String {
    let body = crate::reply::single_line(&reply.parent_body);
    let name = reply.parent_name();
    if body.is_empty() {
        format!("↳ Replying to {name}")
    } else {
        format!("↳ Replying to {name}: {body}")
    }
}

/// `HH:MM` (UTC) of a Unix millisecond timestamp.
pub fn clock(unix_millis: i64) -> String {
    let minutes = unix_millis.div_euclid(60_000).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// The name a message shows for its author: the display name, or the login when that is blank.
pub fn author_name(author: &Author) -> &str {
    if author.display_name.trim().is_empty() {
        &author.login
    } else {
        &author.display_name
    }
}

fn push_body(out: &mut DisplayText, fragments: &[Fragment], text_role: Role) {
    for fragment in fragments {
        match fragment {
            Fragment::Text(text) => out.push(text, text_role),
            Fragment::Emote {
                emote,
                zero_width_overlays,
            } => {
                out.push_emote(emote, false);
                // The overlays follow their base, a space apart: the text (copy, hit-testing)
                // names them all, and the layout draws the whole stack as one box.
                for overlay in zero_width_overlays {
                    out.push(" ", text_role);
                    out.push_emote(overlay, true);
                }
            }
            Fragment::Emoji { text } => out.push(text, Role::Emoji),
            Fragment::Mention { text, is_self, .. } => out.push(
                text,
                if *is_self {
                    Role::SelfMention
                } else {
                    Role::Mention
                },
            ),
            Fragment::Link { text, url } => {
                let start = out.text.len();
                out.push(text, Role::Link);
                let end = out.text.len();
                if end > start {
                    out.links.push(DisplayLink {
                        range: start..end,
                        url: Arc::from(url.as_str()),
                    });
                }
            }
            // With its image (the channel's cheermotes are known) the word is a box and the
            // amount follows it as text; without, the whole `Cheer100` is text.
            Fragment::Cheer {
                prefix,
                bits,
                image: Some(image),
            } => {
                out.push_emote_as(prefix, image, false);
                out.push(&bits.to_string(), Role::Cheer);
            }
            Fragment::Cheer { prefix, bits, .. } => {
                out.push(&format!("{prefix}{bits}"), Role::Cheer)
            }
        }
    }
}

/// The fragments of `message` as drawn: for a reply, without the `@parent ` Twitch puts in front of
/// it (the context line already says who is being replied to, and Twitch's own chat hides it too).
/// Only a single leading mention of the reply's parent (any capitalisation) and one space after it
/// go, and only when something else is left; the stored message is untouched.
pub fn body_fragments(message: &Message) -> Cow<'_, [Fragment]> {
    let fragments = &message.fragments[..];
    let Some(reply) = &message.reply else {
        return Cow::Borrowed(fragments);
    };
    let [Fragment::Mention { login, .. }, rest @ ..] = fragments else {
        return Cow::Borrowed(fragments);
    };
    if !login.eq_ignore_ascii_case(&reply.parent_login) {
        return Cow::Borrowed(fragments);
    }
    let mut out = Vec::with_capacity(rest.len());
    match rest {
        [Fragment::Text(text), tail @ ..] => {
            let text = text.strip_prefix(' ').unwrap_or(text);
            if !text.is_empty() {
                out.push(Fragment::Text(text.to_owned()));
            }
            out.extend_from_slice(tail);
        }
        rest => out.extend_from_slice(rest),
    }
    if out.is_empty() {
        // A reply that is only the mention keeps it.
        return Cow::Borrowed(fragments);
    }
    Cow::Owned(out)
}

/// The display text of `message`; `badges` are drawn before the author's name.
///
/// An event row (PR 5.1, [`super::events`]) has its header first and the message on the line
/// after a hard break; the timestamp is on the header of a `USERNOTICE`, on the message of a chat
/// line that only carries a marker (`First message`). A message sent in another channel of a
/// shared chat session starts with that channel's name.
pub fn build(message: &Message, options: DisplayOptions, badges: &[BadgeSlot]) -> DisplayText {
    let mut out = DisplayText::default();
    let author = message.author.as_ref();
    let header = super::events::header(message);
    let stamp = |out: &mut DisplayText| {
        if options.timestamps {
            out.push(&clock(message.timestamp.unix_millis()), Role::Timestamp);
            out.push(" ", Role::Separator);
        }
    };
    let header_has_stamp = header.as_ref().is_none_or(|header| header.is_notice);
    if header_has_stamp {
        stamp(&mut out);
    }
    // Whether the header is followed by a message line.
    let has_message_line = match message.kind {
        MessageKind::UserNotice => !message.fragments.is_empty(),
        _ => true,
    };
    let source = super::events::source_label(message);
    // The label of a notice from another channel starts its header; a chat line's starts the
    // message line (below).
    if let (Some(label), MessageKind::UserNotice, Some(_)) = (&source, message.kind, &header) {
        out.push(label, Role::Source);
        out.push(" ", Role::Separator);
    }
    if let Some(header) = &header {
        out.push(&header.text, Role::Header);
        if has_message_line {
            out.push_break();
        }
    }
    if !header_has_stamp {
        stamp(&mut out);
    }
    match message.kind {
        MessageKind::Chat | MessageKind::Action => {
            if let Some(label) = &source {
                out.push(label, Role::Source);
                out.push(" ", Role::Separator);
            }
            let chat = message.kind == MessageKind::Chat;
            if let Some(author) = author {
                out.push_badges(badges);
                out.push(author_name(author), Role::Name);
                out.push(if chat { ": " } else { " " }, Role::Separator);
            }
            let role = if chat { Role::Text } else { Role::ActionText };
            push_body(&mut out, &body_fragments(message), role);
        }
        MessageKind::UserNotice => {
            if header.is_none() {
                // A notice we have no line for: system text and the user's message on one line.
                if let Some(system) = &message.system_text {
                    out.push(system, Role::System);
                }
            }
            if !message.fragments.is_empty() {
                if header.is_none() && !out.text.is_empty() && !out.text.ends_with(' ') {
                    out.push(" ", Role::Separator);
                }
                if let Some(author) = author {
                    out.push_badges(badges);
                    out.push(author_name(author), Role::Name);
                    out.push(": ", Role::Separator);
                }
                push_body(&mut out, &body_fragments(message), Role::Text);
            }
        }
        MessageKind::Notice | MessageKind::System => {
            push_body(&mut out, &message.fragments, Role::System);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_chat_core::{Emote, Timestamp};

    use super::*;

    fn roles(display: &DisplayText) -> Vec<(&str, Role)> {
        display
            .spans
            .iter()
            .map(|span| (&display.text[span.range.clone()], span.role))
            .collect()
    }

    fn chat(fragments: Vec<Fragment>) -> Message {
        Message::chat(
            "1",
            Timestamp(3_661_000),
            Author::new("alice", "Alice"),
            fragments,
        )
    }

    fn reply(body: &str) -> ReplyRef {
        ReplyRef {
            parent_id: "p".into(),
            parent_login: "bob".into(),
            parent_display_name: "Bob".into(),
            parent_body: body.into(),
            thread_parent_id: None,
        }
    }

    #[test]
    fn a_reply_line_names_the_parent_and_quotes_it_on_one_line() {
        assert_eq!(
            reply_text(&reply("hello there")),
            "↳ Replying to Bob: hello there"
        );
        assert_eq!(
            reply_text(&reply("two\nlines\u{E0000}  and\ttabs")),
            "↳ Replying to Bob: two lines and tabs"
        );
        assert_eq!(reply_text(&reply("")), "↳ Replying to Bob");
        let mut nameless = reply("x");
        nameless.parent_display_name.clear();
        assert_eq!(reply_text(&nameless), "↳ Replying to bob: x");
    }

    /// A reply to `parent_login` whose text is `text`, tokenized as a real message is.
    fn reply_to(parent_login: &str, text: &str) -> Message {
        let mut message = chat(
            gpui_chat_core::Tokenizer::new()
                .tokenize(text, &[])
                .fragments,
        );
        message.reply = Some(ReplyRef {
            parent_id: "p".into(),
            parent_login: parent_login.into(),
            parent_display_name: parent_login.into(),
            parent_body: "x".into(),
            thread_parent_id: None,
        });
        message
    }

    fn drawn(message: &Message) -> String {
        build(message, DisplayOptions::default(), &[]).text
    }

    #[test]
    fn a_reply_does_not_draw_the_mention_twitch_put_in_front_of_it() {
        assert_eq!(
            drawn(&reply_to("bob", "@bob it is chess")),
            "Alice: it is chess"
        );
        // Capitalisation does not matter (the tag is a lowercase login, the text is as typed).
        assert_eq!(
            drawn(&reply_to("bob", "@BoB it is chess")),
            "Alice: it is chess"
        );
        // Only the first mention and a single space go.
        assert_eq!(drawn(&reply_to("bob", "@bob @bob hi")), "Alice: @bob hi");
        assert_eq!(
            drawn(&reply_to("bob", "@bob  two spaces")),
            "Alice:  two spaces"
        );
        // The stored text is untouched: copy and other text actions can still have all of it.
        let message = reply_to("bob", "@bob it is chess");
        assert_eq!(message.plain_text(), "@bob it is chess");
    }

    #[test]
    fn a_reply_keeps_text_that_does_not_start_with_its_parents_mention() {
        // Another user mentioned first, the parent later, or no mention at all.
        assert_eq!(drawn(&reply_to("bob", "@carol hi")), "Alice: @carol hi");
        assert_eq!(drawn(&reply_to("bob", "hi @bob")), "Alice: hi @bob");
        assert_eq!(
            drawn(&reply_to("bob", "no mention here")),
            "Alice: no mention here"
        );
        // A longer name that merely starts the same.
        assert_eq!(drawn(&reply_to("bob", "@bobby hi")), "Alice: @bobby hi");
        // A reply that is only the mention keeps it (nothing would be left to draw).
        assert_eq!(drawn(&reply_to("bob", "@bob")), "Alice: @bob");
        assert_eq!(drawn(&reply_to("bob", "@bob ")), "Alice: @bob ");
    }

    #[test]
    fn a_message_that_is_not_a_reply_keeps_a_leading_mention() {
        let plain = chat(
            gpui_chat_core::Tokenizer::new()
                .tokenize("@bob hi", &[])
                .fragments,
        );
        assert_eq!(drawn(&plain), "Alice: @bob hi");
    }

    #[test]
    fn the_mention_is_hidden_in_the_recorded_replies() {
        // fixtures/irc/replies.irc: Twitch's replies all start with `@parent `.
        let path = format!(
            "{}/../../fixtures/irc/replies.irc",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(path).unwrap();
        let mut shown = Vec::new();
        for line in text.lines() {
            if let Some(gpui_chat_core::ChatEvent::Message { message, .. }) =
                gpui_chat_twitch::parse_line(line).unwrap()
            {
                shown.push((
                    message.plain_text(),
                    drawn(&message),
                    message.reply.is_some(),
                ));
            }
        }
        // The root has no reply: drawn as is. Each reply loses its `@Parent `.
        assert_eq!(
            shown[0].1,
            "Alice: what is the best opening for a quick game?"
        );
        for (plain, drawn, is_reply) in &shown {
            assert_eq!(plain.starts_with('@'), *is_reply, "{plain}");
            if *is_reply {
                assert!(!drawn.contains('@'), "{drawn}");
            }
        }
        let bodies: Vec<_> = shown.iter().map(|(_, drawn, _)| drawn.as_str()).collect();
        assert!(bodies.contains(&"Bob: the Italian is solid"), "{bodies:?}");
        assert!(bodies.contains(&"Carol: until someone plays the Sicilian"));
        assert!(bodies.contains(&"Gina: agreed"));
    }

    // ---- event rows (PR 5.1) ------------------------------------------------------------------

    /// The messages of `fixtures/irc/events.irc` by the last digits of their id.
    fn event(n: &str) -> Arc<Message> {
        let path = format!(
            "{}/../../fixtures/irc/events.irc",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(path).unwrap();
        let tail = format!("8000-{n:0>12};");
        let line = text
            .lines()
            .find(|line| line.contains(&tail))
            .unwrap_or_else(|| panic!("no event {n}"));
        match gpui_chat_twitch::parse_line(line).unwrap().unwrap() {
            gpui_chat_core::ChatEvent::Message { message, .. } => message,
            other => panic!("{other:?}"),
        }
    }

    fn timed(message: &Message) -> DisplayText {
        build(message, DisplayOptions { timestamps: true }, &[])
    }

    /// The text with each hard break shown as `⏎`.
    fn with_breaks(display: &DisplayText) -> String {
        let mut text = display.text.clone();
        for &at in display.breaks.iter().rev() {
            text.replace_range(at..at + 1, "⏎");
        }
        text
    }

    #[test]
    fn a_sub_with_a_message_is_a_header_line_then_the_message_line() {
        // 12 months, streak shared, the user's message after the break; the stamp is on the header.
        let display = timed(&event("12"));
        assert_eq!(
            with_breaks(&display),
            "22:15 LongSub resubscribed at Tier 1 for 12 months, 12 month streak⏎LongSub: a year Kappa, art at https://example.com/art"
        );
        let header = display
            .spans
            .iter()
            .find(|span| span.role == Role::Header)
            .unwrap();
        assert_eq!(
            &display.text[header.range.clone()],
            "LongSub resubscribed at Tier 1 for 12 months, 12 month streak"
        );
        assert_eq!(display.breaks.len(), 1);
        assert!(
            display
                .link_at(display.text.find("example").unwrap())
                .is_some()
        );
        // Without a message there is no break at all.
        let bare = timed(&event("13"));
        assert!(bare.breaks.is_empty());
        assert_eq!(
            bare.text,
            "22:15 BigSub resubscribed at Tier 3 for 27 months"
        );
    }

    #[test]
    fn announcements_gifts_raids_and_unknown_notices() {
        assert_eq!(
            with_breaks(&timed(&event("42"))),
            "22:15 Announcement⏎ModUser: Giveaway winner will be drawn at the end of stream Kappa and also a very long announcement line that has to wrap onto a second line in a narrow pane"
        );
        assert_eq!(
            timed(&event("20")).text,
            "22:15 Gifter gifted a Tier 1 sub to Lucky (50 gifted in total)"
        );
        assert_eq!(
            timed(&event("30")).text,
            "22:15 1234 raiders from RaidingChannel have joined"
        );
        assert_eq!(
            timed(&event("31")).text,
            "22:15 1 raider from TinyChannel has joined"
        );
        // A msg-id this build does not know: Twitch's own line.
        assert_eq!(
            timed(&event("52")).text,
            "22:15 Payer is paying forward the Gift they got from Gifter to Lucky!"
        );
        assert_eq!(
            with_breaks(&timed(&event("50"))),
            "22:15 Fresh is new here⏎Fresh: @everyone say hi"
        );
    }

    #[test]
    fn a_marked_chat_line_has_its_marker_above_and_the_time_on_the_message() {
        assert_eq!(
            with_breaks(&timed(&event("72"))),
            "First message⏎22:15 Newbie: hi everyone, first time here"
        );
        assert_eq!(
            with_breaks(&timed(&event("60"))),
            "Cheered 100 bits⏎22:15 Cheerer: Cheer100 nice play"
        );
        assert_eq!(
            with_breaks(&timed(&event("71"))),
            "Redeemed a reward⏎22:15 Redeemer: please hydrate Kappa"
        );
        assert_eq!(
            with_breaks(&timed(&event("73"))),
            "First message⏎22:15 Newbie2 waves hello"
        );
        // Without timestamps the marker is simply the first line.
        let untimed = build(&event("70"), DisplayOptions::default(), &[]);
        assert_eq!(
            with_breaks(&untimed),
            "Highlighted message⏎Highlighter: this message cost channel points"
        );
    }

    #[test]
    fn a_message_from_another_shared_channel_starts_with_that_channels_name() {
        assert_eq!(
            timed(&event("82")).text,
            "22:15 [shared chat] Visitor: shared chat says hi"
        );
        let mut named = (*event("82")).clone();
        named.shared.as_mut().unwrap().source_name = Some("OtherChan".into());
        let display = timed(&named);
        assert_eq!(
            display.text,
            "22:15 [OtherChan] Visitor: shared chat says hi"
        );
        assert!(display.spans.iter().any(|s| s.role == Role::Source));
        // Sent in this channel (source = this room): no label.
        assert_eq!(
            timed(&event("83")).text,
            "22:15 Host: welcome everyone, both chats"
        );
    }

    #[test]
    fn a_notice_of_another_shared_channel_is_labelled_on_its_header() {
        // A partner's raid is not ours: the label comes first, the stamp before it.
        assert_eq!(
            timed(&event("100")).text,
            "22:15 [shared chat] 2500 raiders from BigRaider have joined"
        );
        assert_eq!(
            with_breaks(&timed(&event("101"))),
            "22:15 [shared chat] Announcement⏎OtherMod: Both chats: raffle in ten minutes"
        );
        let mut named = (*event("100")).clone();
        named.shared.as_mut().unwrap().source_name = Some("OtherChan".into());
        let display = timed(&named);
        assert!(display.text.starts_with("22:15 [OtherChan] 2500 raiders"));
        assert_eq!(display.spans[2].role, Role::Source);
    }

    #[test]
    fn a_cheermote_with_its_image_is_a_box_and_its_amount() {
        let mut message = (*event("60")).clone();
        let mut image = Emote::twitch("cheer:cheer:100", "Cheer");
        image.width = Some(28);
        let image = Arc::new(image);
        let Fragment::Cheer { image: slot, .. } = &mut message.fragments[0] else {
            panic!("a cheer")
        };
        *slot = Some(image.clone());
        let display = build(&message, DisplayOptions::default(), &[]);
        assert_eq!(
            with_breaks(&display),
            "Cheered 100 bits⏎Cheerer: Cheer100 nice play"
        );
        let cheer = &display.emotes[0];
        assert_eq!(
            &display.text[display.spans[cheer.span].range.clone()],
            "Cheer"
        );
        assert!(Arc::ptr_eq(&cheer.emote, &image));
        let amount = display
            .spans
            .iter()
            .find(|s| s.role == Role::Cheer)
            .unwrap();
        assert_eq!(&display.text[amount.range.clone()], "100");
        assert!(Role::Cheer.is_bold());
        // Typed in another case, shown as typed.
        let big = event("61");
        assert!(
            build(&big, DisplayOptions::default(), &[])
                .text
                .contains("cheer1000 Kappa500 cheer250")
        );
    }

    #[test]
    fn a_notice_with_no_event_keeps_the_old_one_line_layout() {
        // Messages built without the event model (as before PR 5.1) still draw system + message.
        let mut message = chat(vec![Fragment::Text("hype".into())]);
        message.kind = MessageKind::UserNotice;
        message.system_text = Some("Alice subscribed!".into());
        let display = timed(&message);
        assert_eq!(display.text, "01:01 Alice subscribed! Alice: hype");
        assert!(display.breaks.is_empty());
    }

    #[test]
    fn every_event_fixture_builds_a_coherent_display() {
        let path = format!(
            "{}/../../fixtures/irc/events.irc",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(path).unwrap();
        let mut rows = 0;
        for line in text.lines() {
            let Ok(Some(gpui_chat_core::ChatEvent::Message { message, .. })) =
                gpui_chat_twitch::parse_line(line)
            else {
                continue;
            };
            rows += 1;
            let display = timed(&message);
            let mut at = 0;
            for span in &display.spans {
                assert_eq!(span.range.start, at, "{}", display.text);
                at = span.range.end;
            }
            assert_eq!(at, display.text.len());
            for &b in &display.breaks {
                assert_eq!(&display.text[b..b + 1], " ", "a break is one space");
            }
            assert!(display.breaks.len() <= 1, "{}", display.text);
            assert!(!display.text.is_empty());
        }
        assert!(rows > 35, "{rows} messages");
    }

    #[test]
    fn clock_formats_utc_time_of_day() {
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(1_700_000_000_000), "22:13");
        assert_eq!(clock(-60_000), "23:59");
    }

    #[test]
    fn chat_line_has_timestamp_name_and_body_roles() {
        let message = chat(vec![
            Fragment::Text("hi ".into()),
            Fragment::Mention {
                text: "@Bob".into(),
                login: "bob".into(),
                is_self: false,
            },
            Fragment::Text(" see ".into()),
            Fragment::Link {
                text: "x.com".into(),
                url: "https://x.com".into(),
            },
            Fragment::Text(" ".into()),
            Fragment::Mention {
                text: "@me".into(),
                login: "me".into(),
                is_self: true,
            },
        ]);
        let display = build(&message, DisplayOptions { timestamps: true }, &[]);
        assert_eq!(display.text, "01:01 Alice: hi @Bob see x.com @me");
        assert_eq!(
            roles(&display),
            [
                ("01:01", Role::Timestamp),
                (" ", Role::Separator),
                ("Alice", Role::Name),
                (": ", Role::Separator),
                ("hi ", Role::Text),
                ("@Bob", Role::Mention),
                (" see ", Role::Text),
                ("x.com", Role::Link),
                (" ", Role::Text),
                ("@me", Role::SelfMention),
            ]
        );
        // The link keeps where it opens, found by any byte of its text.
        assert_eq!(display.links.len(), 1);
        let link = &display.links[0];
        assert_eq!(&display.text[link.range.clone()], "x.com");
        assert_eq!(&*link.url, "https://x.com");
        assert_eq!(display.link_at(link.range.start), Some(link));
        assert_eq!(display.link_at(link.range.end - 1), Some(link));
        assert_eq!(display.link_at(link.range.end), None);
        assert_eq!(display.link_at(0), None);
        let untimed = build(&message, DisplayOptions { timestamps: false }, &[]);
        assert!(untimed.text.starts_with("Alice: "));
    }

    #[test]
    fn spans_are_contiguous_and_cover_the_text() {
        let emote = Arc::new(Emote::twitch("25", "Kappa"));
        let message = chat(vec![
            Fragment::Emote {
                emote: emote.clone(),
                zero_width_overlays: vec![emote],
            },
            Fragment::Emoji {
                text: "👍🏽".into()
            },
            Fragment::Cheer {
                prefix: "Cheer".into(),
                bits: 100,
                image: None,
            },
        ]);
        let display = build(&message, DisplayOptions { timestamps: true }, &[]);
        let mut at = 0;
        for span in &display.spans {
            assert_eq!(span.range.start, at);
            at = span.range.end;
        }
        assert_eq!(at, display.text.len());
        assert!(display.text.ends_with("Kappa Kappa👍🏽Cheer100"));
        // Each emote is its own span, recorded with its emote.
        assert_eq!(display.emotes.len(), 2);
        assert_eq!(
            display.emotes.iter().map(|e| e.overlay).collect::<Vec<_>>(),
            [false, true]
        );
        for DisplayEmote { span, emote, .. } in &display.emotes {
            assert_eq!(display.spans[*span].role, Role::Emote);
            assert_eq!(
                &display.text[display.spans[*span].range.clone()],
                &*emote.name
            );
        }
        assert_eq!(display.span_at(0), Some(0));
        assert_eq!(display.span_at(display.text.len()), None);
    }

    #[test]
    fn adjacent_emotes_stay_separate_spans() {
        let kappa = Arc::new(Emote::twitch("25", "Kappa"));
        let lul = Arc::new(Emote::twitch("425618", "LUL"));
        let message = chat(vec![
            Fragment::Emote {
                emote: kappa,
                zero_width_overlays: vec![],
            },
            Fragment::Emote {
                emote: lul,
                zero_width_overlays: vec![],
            },
        ]);
        let display = build(&message, DisplayOptions::default(), &[]);
        assert_eq!(
            roles(&display)[2..],
            [("Kappa", Role::Emote), ("LUL", Role::Emote)]
        );
        assert_eq!(
            display.emotes.iter().map(|e| e.span).collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[test]
    fn action_notice_and_usernotice_layouts() {
        let mut action = chat(vec![Fragment::Text("waves".into())]);
        action.kind = MessageKind::Action;
        assert_eq!(
            roles(&build(&action, DisplayOptions::default(), &[])),
            [
                ("Alice", Role::Name),
                (" ", Role::Separator),
                ("waves", Role::ActionText)
            ]
        );

        let notice = Message::plain("2", Timestamp(0), MessageKind::System, "connected");
        assert_eq!(
            roles(&build(&notice, DisplayOptions::default(), &[])),
            [("connected", Role::System)]
        );

        let mut sub = chat(vec![Fragment::Text("hype".into())]);
        sub.kind = MessageKind::UserNotice;
        sub.system_text = Some("Alice subscribed for 3 months!".into());
        assert_eq!(
            build(&sub, DisplayOptions::default(), &[]).text,
            "Alice subscribed for 3 months! Alice: hype"
        );
    }

    #[test]
    fn control_characters_become_spaces_and_the_tag_character_is_dropped() {
        let message = chat(vec![Fragment::Text("a\nb\tc\u{E0000}d".into())]);
        let display = build(&message, DisplayOptions::default(), &[]);
        assert_eq!(display.text, "Alice: a b cd");
        assert!(!display.text.contains('\n'));
    }

    #[test]
    fn empty_display_name_falls_back_to_login() {
        let mut message = chat(vec![]);
        message.author = Some(Author::new("bob", " "));
        assert_eq!(
            build(&message, DisplayOptions::default(), &[]).text,
            "bob: "
        );
    }
    fn slot(title: &str, url: &str) -> BadgeSlot {
        BadgeSlot {
            badge: Arc::new(Badge {
                id: gpui_chat_core::BadgeId::new(title, "1"),
                title: title.to_owned(),
                urls: gpui_chat_core::ImageUrls::new(),
            }),
            url: ImageKey::new(url),
            size: 18.0,
        }
    }

    #[test]
    fn badges_come_before_the_name_each_a_span_of_its_own() {
        let message = chat(vec![Fragment::Text("hi".into())]);
        let slots = [slot("Moderator", "m"), slot("Subscriber", "s")];
        let display = build(&message, DisplayOptions { timestamps: true }, &slots);
        assert_eq!(display.text, "01:01 Moderator Subscriber Alice: hi");
        assert_eq!(
            roles(&display),
            [
                ("01:01", Role::Timestamp),
                (" ", Role::Separator),
                ("Moderator", Role::Badge),
                (" ", Role::Separator),
                ("Subscriber", Role::Badge),
                (" ", Role::Separator),
                ("Alice", Role::Name),
                (": ", Role::Separator),
                ("hi", Role::Text),
            ]
        );
        let spans: Vec<_> = display.badges.iter().map(|b| b.span).collect();
        assert_eq!(spans, [2, 4]);
        assert_eq!(display.badges[1].slot.url.url(), "s");

        // /me and the chat part of a USERNOTICE put them before the name too; notices and system
        // lines have no author and so none.
        let mut action = chat(vec![Fragment::Text("waves".into())]);
        action.kind = MessageKind::Action;
        assert_eq!(
            build(&action, DisplayOptions::default(), &slots[..1]).text,
            "Moderator Alice waves"
        );
        let mut sub = chat(vec![Fragment::Text("hype".into())]);
        sub.kind = MessageKind::UserNotice;
        sub.system_text = Some("Alice subscribed!".into());
        assert_eq!(
            build(&sub, DisplayOptions::default(), &slots[..1]).text,
            "Alice subscribed! Moderator Alice: hype"
        );
        let notice = Message::plain("2", Timestamp(0), MessageKind::System, "x");
        let display = build(&notice, DisplayOptions::default(), &slots);
        assert_eq!(display.text, "x");
        assert!(display.badges.is_empty());
    }

    #[test]
    fn a_badge_without_a_title_has_no_span_and_none_means_no_text() {
        let message = chat(vec![]);
        let display = build(&message, DisplayOptions::default(), &[slot("", "x")]);
        assert_eq!(display.text, "Alice: ");
        assert!(display.badges.is_empty());
        assert_eq!(
            build(&message, DisplayOptions::default(), &[]).text,
            "Alice: "
        );
    }
}
