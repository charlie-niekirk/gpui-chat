//! Composing a message (PR 3.2): what the input box does with the text before anything is sent.
//!
//! Pure and I/O free, so it is tested on any OS:
//!
//! * [`parse_input`] turns the typed line into an [`Input`]: nothing to send, a message, a `/me`
//!   action, or the reason it cannot be sent (unknown `/command`, too long).
//! * [`sanitize_line`] is the one place where pasted or typed text becomes a single line: every
//!   kind of line break becomes a space, other control characters go. A multi-line paste is
//!   therefore one message the user still has to confirm with Enter, never several.
//! * [`InputHistory`] is the ↑/↓ recall of what was sent from one input, with the unsent draft
//!   restored when ↓ walks past the newest entry.

use std::borrow::Cow;
use std::fmt;

/// Twitch's limit for one chat message, in characters (code points).
pub const MAX_MESSAGE_CHARS: usize = 500;

/// How many sent lines one input remembers.
pub const HISTORY_CAPACITY: usize = 100;

/// A line that is ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// The text without any command, as shown in chat.
    pub text: String,
    /// `/me`: shown in the author's colour as an action.
    pub action: bool,
}

impl Outgoing {
    /// The text as sent to Twitch: an action goes as `/me <text>`.
    pub fn wire_text(&self) -> String {
        if self.action {
            format!("/me {}", self.text)
        } else {
            self.text.clone()
        }
    }
}

/// Why a typed line is not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    /// `/name` is not a command this version knows (`name` is lowercase, without the slash).
    UnknownCommand(String),
    /// `/me` with nothing after it.
    MissingText { command: &'static str },
    /// More than [`MAX_MESSAGE_CHARS`] characters.
    TooLong { chars: usize },
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand(name) => write!(
                f,
                "Unknown command /{name}. Start with // to send a message that begins with a slash."
            ),
            Self::MissingText { command } => write!(f, "Usage: /{command} <message>"),
            Self::TooLong { chars } => write!(
                f,
                "Message is too long ({chars}/{MAX_MESSAGE_CHARS} characters)."
            ),
        }
    }
}

impl std::error::Error for InputError {}

/// What a typed line means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Blank (or only whitespace): nothing to do.
    Empty,
    Send(Outgoing),
    Invalid(InputError),
}

/// Turns pasted or typed text into one line: each run of line breaks (`\n`, `\r\n`, `\r`, NEL,
/// LS, PS) and tabs becomes one space, other control characters are dropped. Borrowed when the
/// text is already clean. Does not trim.
pub fn sanitize_line(text: &str) -> Cow<'_, str> {
    let dirty = |c: char| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}');
    if !text.chars().any(dirty) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut after_break = false;
    for c in text.chars() {
        match c {
            '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}' => {
                // A run of breaks is one space.
                if !after_break {
                    out.push(' ');
                }
                after_break = true;
                continue;
            }
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
        after_break = false;
    }
    Cow::Owned(out)
}

/// Parses the text of an input box.
///
/// * `/me text` is an action; `/me` alone is a usage error.
/// * `//text` sends `/text` (the escape for a message that starts with a slash).
/// * any other `/command` is rejected: Helix does not run chat commands, and moderation commands
///   arrive with PR 5.2.
/// * more than [`MAX_MESSAGE_CHARS`] characters is rejected (an action counts its `/me `).
///
/// Command names are case-insensitive. The text is [sanitised](sanitize_line) and trimmed first.
pub fn parse_input(raw: &str) -> Input {
    let line = sanitize_line(raw);
    let line = line.trim();
    if line.is_empty() {
        return Input::Empty;
    }
    let outgoing = match line.strip_prefix('/') {
        None => Outgoing {
            text: line.to_owned(),
            action: false,
        },
        // `//text`: a literal leading slash.
        Some(rest) if rest.starts_with('/') => Outgoing {
            text: rest.to_owned(),
            action: false,
        },
        Some(rest) => {
            let (name, argument) = rest
                .split_once(char::is_whitespace)
                .map_or((rest, ""), |(name, argument)| (name, argument.trim_start()));
            match name.to_lowercase().as_str() {
                "me" if argument.is_empty() => {
                    return Input::Invalid(InputError::MissingText { command: "me" });
                }
                "me" => Outgoing {
                    text: argument.to_owned(),
                    action: true,
                },
                other => {
                    return Input::Invalid(InputError::UnknownCommand(other.to_owned()));
                }
            }
        }
    };
    let chars = outgoing.wire_text().chars().count();
    if chars > MAX_MESSAGE_CHARS {
        return Input::Invalid(InputError::TooLong { chars });
    }
    Input::Send(outgoing)
}

/// The lines an input has sent, for ↑/↓ recall.
///
/// [`push`](Self::push) records a sent line (blank lines and a repeat of the newest entry are
/// skipped) and ends any navigation. [`older`](Self::older) walks to older entries, keeping
/// what was in the box when navigation started as the *draft*; [`newer`](Self::newer) walks back and,
/// past the newest entry, returns the draft and leaves navigation.
#[derive(Debug, Clone, Default)]
pub struct InputHistory {
    /// Oldest first.
    entries: Vec<String>,
    /// Index of the entry being shown, `None` while the box holds the user's own text.
    cursor: Option<usize>,
    draft: String,
}

impl InputHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the box currently shows a recalled entry.
    pub fn is_navigating(&self) -> bool {
        self.cursor.is_some()
    }

    /// Records a sent line. Ends navigation and forgets the draft.
    pub fn push(&mut self, line: &str) {
        self.reset();
        let line = line.trim();
        if line.is_empty() || self.entries.last().is_some_and(|last| last == line) {
            return;
        }
        if self.entries.len() >= HISTORY_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(line.to_owned());
    }

    /// ↑: the next older entry. `current` is the box's text, remembered as the draft when this
    /// starts navigation. At the oldest entry it stays there. `None` when there is no history.
    pub fn older(&mut self, current: &str) -> Option<&str> {
        let index = match self.cursor {
            None => {
                let newest = self.entries.len().checked_sub(1)?;
                self.draft = current.to_owned();
                newest
            }
            Some(index) => index.saturating_sub(1),
        };
        self.cursor = Some(index);
        Some(&self.entries[index])
    }

    /// ↓: the next newer entry, or the draft after the newest. `None` when not navigating.
    pub fn newer(&mut self) -> Option<&str> {
        let index = self.cursor?;
        if index + 1 < self.entries.len() {
            self.cursor = Some(index + 1);
            Some(&self.entries[index + 1])
        } else {
            self.cursor = None;
            Some(&self.draft)
        }
    }

    /// Ends navigation (the user edited the box, or sent).
    pub fn reset(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(text: &str) -> Input {
        Input::Send(Outgoing {
            text: text.to_owned(),
            action: false,
        })
    }

    #[test]
    fn blank_is_nothing_to_send() {
        assert_eq!(parse_input(""), Input::Empty);
        assert_eq!(parse_input("   \t "), Input::Empty);
        assert_eq!(parse_input("\n\r\n"), Input::Empty);
    }

    #[test]
    fn a_plain_line_is_trimmed_and_sent() {
        assert_eq!(parse_input("  hello chat  "), send("hello chat"));
        assert_eq!(parse_input("a  b"), send("a  b"));
    }

    #[test]
    fn me_is_an_action_with_the_wire_form_me_text() {
        let Input::Send(out) = parse_input("/me waves") else {
            panic!("expected a message")
        };
        assert!(out.action);
        assert_eq!(out.text, "waves");
        assert_eq!(out.wire_text(), "/me waves");
        // Case and spacing of the command do not matter.
        assert_eq!(parse_input("/ME   waves"), parse_input("/me waves"));
        assert_eq!(parse_input("  /me waves  "), parse_input("/me waves"));
    }

    #[test]
    fn me_alone_says_how_to_use_it() {
        for line in ["/me", "/me   ", "/ME"] {
            assert_eq!(
                parse_input(line),
                Input::Invalid(InputError::MissingText { command: "me" })
            );
        }
        assert_eq!(
            InputError::MissingText { command: "me" }.to_string(),
            "Usage: /me <message>"
        );
    }

    #[test]
    fn other_commands_are_rejected_not_sent() {
        assert_eq!(
            parse_input("/ban someone"),
            Input::Invalid(InputError::UnknownCommand("ban".into()))
        );
        assert_eq!(
            parse_input("/Timeout x 10"),
            Input::Invalid(InputError::UnknownCommand("timeout".into()))
        );
        assert_eq!(
            parse_input("/"),
            Input::Invalid(InputError::UnknownCommand(String::new()))
        );
        assert!(
            InputError::UnknownCommand("ban".into())
                .to_string()
                .starts_with("Unknown command /ban")
        );
    }

    #[test]
    fn a_double_slash_sends_a_leading_slash() {
        assert_eq!(parse_input("//shrug"), send("/shrug"));
        assert_eq!(parse_input("//me hi"), send("/me hi"));
        // A slash inside the text is just text.
        assert_eq!(parse_input("1/2 of chat"), send("1/2 of chat"));
        assert_eq!(parse_input("see /me"), send("see /me"));
    }

    #[test]
    fn the_limit_counts_characters_not_bytes() {
        let limit = "é".repeat(MAX_MESSAGE_CHARS);
        assert_eq!(parse_input(&limit), send(&limit));
        let over = "é".repeat(MAX_MESSAGE_CHARS + 1);
        assert_eq!(
            parse_input(&over),
            Input::Invalid(InputError::TooLong {
                chars: MAX_MESSAGE_CHARS + 1
            })
        );
    }

    #[test]
    fn an_action_counts_its_command_towards_the_limit() {
        let fits = format!("/me {}", "a".repeat(MAX_MESSAGE_CHARS - 4));
        assert!(matches!(parse_input(&fits), Input::Send(_)));
        let over = format!("/me {}", "a".repeat(MAX_MESSAGE_CHARS - 3));
        assert_eq!(
            parse_input(&over),
            Input::Invalid(InputError::TooLong {
                chars: MAX_MESSAGE_CHARS + 1
            })
        );
        assert_eq!(
            InputError::TooLong { chars: 612 }.to_string(),
            "Message is too long (612/500 characters)."
        );
    }

    #[test]
    fn line_breaks_become_one_space_each() {
        assert_eq!(sanitize_line("one\ntwo"), "one two");
        assert_eq!(sanitize_line("one\r\ntwo"), "one two");
        assert_eq!(sanitize_line("one\n\n\r\ntwo"), "one two");
        assert_eq!(
            sanitize_line("one\u{2028}two\u{2029}three\u{85}four"),
            "one two three four"
        );
        assert_eq!(sanitize_line("a\tb"), "a b");
        // Other control characters vanish.
        assert_eq!(sanitize_line("a\u{7}b\u{0}c"), "abc");
        // Already clean text is not copied.
        assert!(matches!(sanitize_line("clean text 🙂"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_multi_line_paste_is_one_message() {
        assert_eq!(
            parse_input("first line\nsecond line\nthird"),
            send("first line second line third")
        );
        // A pasted command line is still a command, once.
        assert!(matches!(
            parse_input("/ban x\nhello"),
            Input::Invalid(InputError::UnknownCommand(_))
        ));
        assert_eq!(parse_input("\n\nhello\n"), send("hello"));
    }

    fn history(lines: &[&str]) -> InputHistory {
        let mut history = InputHistory::new();
        for line in lines {
            history.push(line);
        }
        history
    }

    #[test]
    fn up_walks_back_and_down_returns_to_the_draft() {
        let mut history = history(&["one", "two", "three"]);
        assert_eq!(history.older("draft"), Some("three"));
        assert!(history.is_navigating());
        assert_eq!(history.older("ignored while navigating"), Some("two"));
        assert_eq!(history.older(""), Some("one"));
        // The oldest entry is the end of the line.
        assert_eq!(history.older(""), Some("one"));
        assert_eq!(history.newer(), Some("two"));
        assert_eq!(history.newer(), Some("three"));
        assert_eq!(history.newer(), Some("draft"));
        assert!(!history.is_navigating());
        assert_eq!(history.newer(), None);
    }

    #[test]
    fn up_with_no_history_does_nothing() {
        let mut history = InputHistory::new();
        assert_eq!(history.older("typing"), None);
        assert!(!history.is_navigating());
        assert_eq!(history.newer(), None);
    }

    #[test]
    fn push_skips_blank_and_repeated_lines_and_ends_navigation() {
        let mut history = history(&["a", "a", "  ", "b", " b "]);
        assert_eq!(history.len(), 2);
        history.older("x");
        history.push("c");
        assert!(!history.is_navigating());
        assert_eq!(history.older(""), Some("c"));
        // The draft of the abandoned navigation is gone.
        assert_eq!(history.newer(), Some(""));
        // A line repeating an older (not the newest) entry is kept.
        history.push("a");
        assert_eq!(history.len(), 4);
    }

    #[test]
    fn the_oldest_entries_fall_out_at_capacity() {
        let mut history = InputHistory::new();
        for n in 0..HISTORY_CAPACITY + 5 {
            history.push(&format!("line {n}"));
        }
        assert_eq!(history.len(), HISTORY_CAPACITY);
        for _ in 0..HISTORY_CAPACITY + 10 {
            history.older("");
        }
        assert_eq!(history.older(""), Some("line 5"));
    }

    #[test]
    fn reset_leaves_navigation() {
        let mut history = history(&["a", "b"]);
        history.older("draft");
        history.reset();
        assert!(!history.is_navigating());
        assert_eq!(history.older("new draft"), Some("b"));
        assert_eq!(history.newer(), Some("new draft"));
    }
}
