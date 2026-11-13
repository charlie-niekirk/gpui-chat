//! The single line of text a message is drawn as, with a semantic [`Role`] per span.
//!
//! `[timestamp] name: body` for chat, `[timestamp] name body` for `/me`, `[timestamp] text` for
//! notices and system lines. Roles, not colours, are stored: colours are resolved at paint time
//! from the current theme, so a theme switch never invalidates a layout. Control characters
//! (newlines, tabs) become spaces so the text shapes as one line; the U+E0000 tag character some
//! clients append is dropped (the tokenizer already strips it from fragments; author-supplied
//! system text is covered here).

use std::ops::Range;
use std::sync::Arc;

use gpui_chat_core::{Author, Emote, Fragment, Message, MessageKind};

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
    /// emote is a span of its own, listed in [`DisplayText::emotes`].
    Emote,
    Emoji,
    Cheer,
    /// Notices, system lines and the system part of a `USERNOTICE` (muted).
    System,
}

impl Role {
    pub fn is_bold(self) -> bool {
        matches!(self, Self::Name | Self::Mention | Self::SelfMention)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub role: Role,
}

/// The display text and its spans. Spans are contiguous, in order, and cover the whole text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayText {
    pub text: String,
    pub spans: Vec<Span>,
    /// `(span index, emote)` for every [`Role::Emote`] span, in text order.
    pub emotes: Vec<(usize, Arc<Emote>)>,
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
            // Emotes are never merged: each span is one image.
            Some(last) if last.role == role && last.range.end == start && role != Role::Emote => {
                last.range.end = end;
            }
            _ => self.spans.push(Span {
                range: start..end,
                role,
            }),
        }
    }

    /// Appends an emote's name as its own [`Role::Emote`] span.
    fn push_emote(&mut self, emote: &Arc<Emote>) {
        let spans = self.spans.len();
        self.push(&emote.name, Role::Emote);
        if self.spans.len() > spans {
            self.emotes.push((spans, emote.clone()));
        }
    }

    /// Index of the span containing byte `index`.
    pub fn span_at(&self, index: usize) -> Option<usize> {
        let i = self.spans.partition_point(|span| span.range.end <= index);
        (i < self.spans.len() && self.spans[i].range.start <= index).then_some(i)
    }
}

/// `HH:MM` (UTC) of a Unix millisecond timestamp.
pub fn clock(unix_millis: i64) -> String {
    let minutes = unix_millis.div_euclid(60_000).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

fn name(author: &Author) -> &str {
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
                out.push_emote(emote);
                // Stacking overlays on their base is PR 2.4; until then they follow it.
                for overlay in zero_width_overlays {
                    out.push(" ", text_role);
                    out.push_emote(overlay);
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
            Fragment::Link { text, .. } => out.push(text, Role::Link),
            Fragment::Cheer { prefix, bits } => out.push(&format!("{prefix}{bits}"), Role::Cheer),
        }
    }
}

/// The display text of `message`.
pub fn build(message: &Message, options: DisplayOptions) -> DisplayText {
    let mut out = DisplayText::default();
    if options.timestamps {
        out.push(&clock(message.timestamp.unix_millis()), Role::Timestamp);
        out.push(" ", Role::Separator);
    }
    let author = message.author.as_ref();
    match message.kind {
        MessageKind::Chat => {
            if let Some(author) = author {
                out.push(name(author), Role::Name);
                out.push(": ", Role::Separator);
            }
            push_body(&mut out, &message.fragments, Role::Text);
        }
        MessageKind::Action => {
            if let Some(author) = author {
                out.push(name(author), Role::Name);
                out.push(" ", Role::Separator);
            }
            push_body(&mut out, &message.fragments, Role::ActionText);
        }
        MessageKind::UserNotice => {
            let header_end = out.text.len();
            if let Some(system) = &message.system_text {
                out.push(system, Role::System);
            }
            if !message.fragments.is_empty() {
                if out.text.len() > header_end {
                    out.push(" ", Role::Separator);
                }
                if let Some(author) = author {
                    out.push(name(author), Role::Name);
                    out.push(": ", Role::Separator);
                }
                push_body(&mut out, &message.fragments, Role::Text);
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
        let display = build(&message, DisplayOptions { timestamps: true });
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
        let untimed = build(&message, DisplayOptions { timestamps: false });
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
            },
        ]);
        let display = build(&message, DisplayOptions { timestamps: true });
        let mut at = 0;
        for span in &display.spans {
            assert_eq!(span.range.start, at);
            at = span.range.end;
        }
        assert_eq!(at, display.text.len());
        assert!(display.text.ends_with("Kappa Kappa👍🏽Cheer100"));
        // Each emote is its own span, recorded with its emote.
        assert_eq!(display.emotes.len(), 2);
        for (span, emote) in &display.emotes {
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
        let display = build(&message, DisplayOptions::default());
        assert_eq!(
            roles(&display)[2..],
            [("Kappa", Role::Emote), ("LUL", Role::Emote)]
        );
        assert_eq!(
            display.emotes.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[test]
    fn action_notice_and_usernotice_layouts() {
        let mut action = chat(vec![Fragment::Text("waves".into())]);
        action.kind = MessageKind::Action;
        assert_eq!(
            roles(&build(&action, DisplayOptions::default())),
            [
                ("Alice", Role::Name),
                (" ", Role::Separator),
                ("waves", Role::ActionText)
            ]
        );

        let notice = Message::plain("2", Timestamp(0), MessageKind::System, "connected");
        assert_eq!(
            roles(&build(&notice, DisplayOptions::default())),
            [("connected", Role::System)]
        );

        let mut sub = chat(vec![Fragment::Text("hype".into())]);
        sub.kind = MessageKind::UserNotice;
        sub.system_text = Some("Alice subscribed for 3 months!".into());
        assert_eq!(
            build(&sub, DisplayOptions::default()).text,
            "Alice subscribed for 3 months! Alice: hype"
        );
    }

    #[test]
    fn control_characters_become_spaces_and_the_tag_character_is_dropped() {
        let message = chat(vec![Fragment::Text("a\nb\tc\u{E0000}d".into())]);
        let display = build(&message, DisplayOptions::default());
        assert_eq!(display.text, "Alice: a b cd");
        assert!(!display.text.contains('\n'));
    }

    #[test]
    fn empty_display_name_falls_back_to_login() {
        let mut message = chat(vec![]);
        message.author = Some(Author::new("bob", " "));
        assert_eq!(build(&message, DisplayOptions::default()).text, "bob: ");
    }
}
