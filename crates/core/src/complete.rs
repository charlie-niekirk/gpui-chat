//! Autocomplete for the input box (PR 3.3): what the word under the caret asks for, how candidates
//! are ranked, what accepting one does to the text, and the lists the candidates come from.
//!
//! Pure and I/O free, so it is tested on any OS. The app's `completion` module is the view over it.
//!
//! * [`find_trigger`] reads the *token* ending at the caret (the run of non-whitespace characters
//!   before it) and says what it asks for: `@name` is a [user](TriggerKind::User), `/name` as the
//!   first word is a [command](TriggerKind::Command), `:name` (two characters or more, so `:)` and
//!   `:D` stay smileys) is an [emote](TriggerKind::Emote), and any other word is an emote when the
//!   user pressed Tab (`explicit`).
//! * [`rank`] orders candidates for a query: case-sensitive prefix, case-insensitive prefix, the
//!   start of a camelCase / digit / underscore "hump" (`pog` finds `monkaPog`), any substring,
//!   then a subsequence (`mkw` finds `monkaW`). Within one tier a recently used candidate wins,
//!   then the shorter name, then alphabetical.
//! * [`apply`] turns the accepted candidate into the edit: the token is replaced, a space follows.
//! * [`recent_chatters`] lists who spoke in a channel lately, newest first, for `@` completion;
//!   [`Recents`] is the small most-recently-used list of emote names the picker persists.

use std::ops::Range;

use crate::message::{Message, MessageKind};

/// What a token asks to be completed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// `:name`, or a plain word after Tab.
    Emote,
    /// `@name`.
    User,
    /// `/name`, as the first word of the line.
    Command,
}

/// The token under the caret and what it asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    /// Byte range of the token in the text, sigil (`@`, `/`, `:`) included; it ends at the caret.
    pub range: Range<usize>,
    /// What was typed after the sigil (the whole word for a Tab on a plain word).
    pub query: String,
}

/// Fewest characters after `:` before it asks for emotes (`:D` and `:)` are smileys).
pub const MIN_COLON_QUERY: usize = 2;

/// What the token ending at byte offset `caret` of `text` asks for, or `None`.
///
/// `explicit` is a Tab: it also completes a plain word as an emote. Without it, `@` and `/` open at
/// once (the empty query lists everyone / every command) and `:` opens after
/// [`MIN_COLON_QUERY`] characters. Typing in the middle of a word does not open anything unless
/// `explicit`: the text after the caret is left alone either way, only the part before it is
/// replaced.
pub fn find_trigger(text: &str, caret: usize, explicit: bool) -> Option<Trigger> {
    if caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let before = &text[..caret];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let token = &before[start..];
    if token.is_empty() {
        return None;
    }
    let mid_word = text[caret..]
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace());
    if mid_word && !explicit {
        return None;
    }
    let range = start..caret;
    let trigger = |kind, query: &str| Trigger {
        kind,
        range: range.clone(),
        query: query.to_owned(),
    };
    if let Some(rest) = token.strip_prefix('@') {
        return Some(trigger(TriggerKind::User, rest));
    }
    if let Some(rest) = token.strip_prefix('/') {
        // Only the first word is a command; `//text` is a literal slash.
        let first_word = text[..start].trim().is_empty();
        return (first_word && !rest.starts_with('/')).then(|| trigger(TriggerKind::Command, rest));
    }
    if let Some(rest) = token.strip_prefix(':')
        && (explicit || rest.chars().count() >= MIN_COLON_QUERY)
        && rest.chars().next().is_some_and(char::is_alphanumeric)
    {
        return Some(trigger(TriggerKind::Emote, rest));
    }
    explicit.then(|| trigger(TriggerKind::Emote, token))
}

/// The text `trigger`'s token becomes when `candidate` (the emote name, the user's name, the
/// command's name, without any sigil) is accepted, and the replaced byte range.
///
/// An emote is the bare name, a user gets `@`, a command `/`; a space follows unless one already
/// does. `rest` is the text after the caret.
pub fn apply(trigger: &Trigger, candidate: &str, rest: &str) -> Edit {
    let sigil = match trigger.kind {
        TriggerKind::Emote => "",
        TriggerKind::User => "@",
        TriggerKind::Command => "/",
    };
    let space = if rest.starts_with(char::is_whitespace) {
        ""
    } else {
        " "
    };
    Edit {
        range: trigger.range.clone(),
        text: format!("{sigil}{candidate}{space}"),
    }
}

/// The text to put in place of `selection` (a caret when empty) of `text` to insert `word`, the way
/// the emote picker does: a space before it unless the text before already ends in whitespace (or
/// is empty), a space after unless whitespace follows.
pub fn word_insertion(text: &str, selection: Range<usize>, word: &str) -> String {
    let before = text.get(..selection.start).unwrap_or("");
    let after = text.get(selection.end..).unwrap_or("");
    let lead = if before.is_empty() || before.ends_with(char::is_whitespace) {
        ""
    } else {
        " "
    };
    let trail = if after.starts_with(char::is_whitespace) {
        ""
    } else {
        " "
    };
    format!("{lead}{word}{trail}")
}

/// Replace `range` of the text with `text`; the caret goes after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

impl Edit {
    /// `text` with the edit applied, and the caret's byte offset in the result.
    pub fn applied_to(&self, text: &str) -> (String, usize) {
        let mut out = String::with_capacity(text.len() + self.text.len());
        out.push_str(&text[..self.range.start]);
        out.push_str(&self.text);
        let caret = out.len();
        out.push_str(&text[self.range.end..]);
        (out, caret)
    }
}

/// How well a candidate matches a query; lower is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Starts with the query, same case.
    Prefix,
    /// Starts with the query, ignoring case.
    PrefixIgnoreCase,
    /// The query starts a word inside the name: after a lower→upper change (`monkaPog`), a
    /// digit/letter change, `_`, `-` or a space.
    Hump,
    /// The query is somewhere inside the name, ignoring case.
    Substring,
    /// The query's characters appear in the name in order, ignoring case.
    Subsequence,
}

/// How `candidate` matches `query`, or `None` if it does not. An empty query matches everything
/// as [`Tier::Prefix`].
pub fn match_tier(query: &str, candidate: &str) -> Option<Tier> {
    if query.is_empty() || candidate.starts_with(query) {
        return Some(Tier::Prefix);
    }
    let query_lower: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let chars: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    // Lowercasing can change the length (`İ`); positions below are only trusted when it did not.
    let aligned = lower.len() == chars.len();
    if starts_with_chars(&lower, &query_lower) {
        return Some(Tier::PrefixIgnoreCase);
    }
    if aligned {
        let at_hump = (1..chars.len()).any(|i| {
            is_hump(chars[i - 1], chars[i]) && starts_with_chars(&lower[i..], &query_lower)
        });
        if at_hump {
            return Some(Tier::Hump);
        }
    }
    if lower
        .windows(query_lower.len().max(1))
        .any(|window| window == query_lower.as_slice())
    {
        return Some(Tier::Substring);
    }
    // One-character queries would match nearly everything as a subsequence.
    if query_lower.len() >= 2 && is_subsequence(&query_lower, &lower) {
        return Some(Tier::Subsequence);
    }
    None
}

fn starts_with_chars(haystack: &[char], needle: &[char]) -> bool {
    haystack.len() >= needle.len() && &haystack[..needle.len()] == needle
}

/// Whether a word starts at `current`, given the character before it.
fn is_hump(previous: char, current: char) -> bool {
    if matches!(previous, '_' | '-' | ' ' | '.') {
        return current.is_alphanumeric();
    }
    (previous.is_lowercase() && current.is_uppercase())
        || (previous.is_alphabetic() && current.is_ascii_digit())
        || (previous.is_ascii_digit() && current.is_alphabetic())
}

fn is_subsequence(needle: &[char], haystack: &[char]) -> bool {
    let mut wanted = needle.iter();
    let mut next = wanted.next();
    for c in haystack {
        if Some(c) == next {
            next = wanted.next();
        }
    }
    next.is_none()
}

/// The indices of `items` that match `query`, best first, at most `limit`.
///
/// Order: [`Tier`], then recently used (`recency` gives the rank, 0 = newest, for an item that was;
/// it also orders users by who spoke last), then the shorter name, then case-insensitive
/// alphabetical, then the original position, so the result is deterministic.
pub fn rank<T>(
    query: &str,
    items: &[T],
    name: impl Fn(&T) -> &str,
    recency: impl Fn(&T) -> Option<usize>,
    limit: usize,
) -> Vec<usize> {
    struct Hit {
        index: usize,
        tier: Tier,
        recency: usize,
        len: usize,
        lower: String,
    }
    let mut hits: Vec<Hit> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let candidate = name(item);
            let tier = match_tier(query, candidate)?;
            Some(Hit {
                index,
                tier,
                recency: recency(item).unwrap_or(usize::MAX),
                len: candidate.chars().count(),
                lower: candidate.to_lowercase(),
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        (a.tier, a.recency, a.len, &a.lower, a.index)
            .cmp(&(b.tier, b.recency, b.len, &b.lower, b.index))
    });
    hits.truncate(limit);
    hits.into_iter().map(|hit| hit.index).collect()
}

/// A slash command the input box understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandInfo {
    /// Without the slash.
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
}

/// The commands [`crate::parse_input`] accepts (a test keeps the two in step).
pub const COMMANDS: &[CommandInfo] = &[CommandInfo {
    name: "me",
    usage: "/me <message>",
    summary: "Send a message as an action",
}];

/// Someone who spoke in a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chatter {
    pub login: String,
    pub display_name: String,
}

/// Who wrote the given messages (oldest first, as [`crate::MessageStore::iter`] yields them), newest
/// speaker first, each once, at most `limit`. Only chat lines count: notices and system lines have
/// no one to mention, and a sub or raid announcement is the system speaking.
pub fn recent_chatters<'a>(
    messages: impl DoubleEndedIterator<Item = &'a Message>,
    limit: usize,
) -> Vec<Chatter> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for message in messages.rev() {
        if out.len() >= limit {
            break;
        }
        let Some(author) = &message.author else {
            continue;
        };
        if !matches!(message.kind, MessageKind::Chat | MessageKind::Action)
            || author.login.is_empty()
            || !seen.insert(author.login.as_str())
        {
            continue;
        }
        let display_name = if author.display_name.is_empty() {
            author.login.clone()
        } else {
            author.display_name.clone()
        };
        out.push(Chatter {
            login: author.login.clone(),
            display_name,
        });
    }
    out
}

/// How many emote names [`Recents`] keeps.
pub const RECENT_EMOTES: usize = 40;

/// The emotes used last, newest first, by the name that is typed. Names rather than emotes: a name
/// is what survives a restart, and the picker shows the ones the current channel can still use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recents {
    names: Vec<String>,
}

impl Recents {
    /// A list read from storage: blank and repeated names dropped, cut to [`RECENT_EMOTES`].
    pub fn from_names(names: impl IntoIterator<Item = String>) -> Self {
        let mut recents = Self::default();
        for name in names.into_iter().collect::<Vec<_>>().into_iter().rev() {
            recents.push(&name);
        }
        recents
    }

    /// `name` was just used: it moves to the front.
    pub fn push(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        self.names.retain(|known| known != name);
        self.names.insert(0, name.to_owned());
        self.names.truncate(RECENT_EMOTES);
    }

    /// Newest first.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// 0 for the newest.
    pub fn rank_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|known| known == name)
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::compose::{Input, InputError, parse_input};
    use crate::message::{Author, Message, Timestamp};

    fn trigger(text: &str, explicit: bool) -> Option<Trigger> {
        find_trigger(text, text.len(), explicit)
    }

    fn kind_and_query(text: &str, explicit: bool) -> Option<(TriggerKind, String)> {
        trigger(text, explicit).map(|t| (t.kind, t.query))
    }

    #[test]
    fn the_sigil_decides_what_is_asked_for() {
        assert_eq!(
            kind_and_query("hello @fo", false),
            Some((TriggerKind::User, "fo".into()))
        );
        assert_eq!(
            kind_and_query("hello :kap", false),
            Some((TriggerKind::Emote, "kap".into()))
        );
        assert_eq!(
            kind_and_query("/m", false),
            Some((TriggerKind::Command, "m".into()))
        );
        // The bare sigils open the full lists, except the colon.
        assert_eq!(
            kind_and_query("@", false),
            Some((TriggerKind::User, String::new()))
        );
        assert_eq!(
            kind_and_query("/", false),
            Some((TriggerKind::Command, String::new()))
        );
        assert_eq!(kind_and_query(":", false), None);
    }

    #[test]
    fn smileys_and_ordinary_words_do_not_open_anything_by_themselves() {
        for text in [
            "hi :)", "hi :D", "hi :(", "hello", "hello ", "", "a :-)", "hi ::",
        ] {
            assert_eq!(trigger(text, false), None, "{text:?}");
        }
        // A colon in the middle of a word is not a sigil.
        assert_eq!(trigger("a:bc", false), None);
    }

    #[test]
    fn tab_completes_a_plain_word_as_an_emote() {
        assert_eq!(
            kind_and_query("hello Kap", true),
            Some((TriggerKind::Emote, "Kap".into()))
        );
        // Nothing to complete after a space.
        assert_eq!(trigger("hello ", true), None);
        // Tab keeps the kind a sigil asks for, and does not need the colon's two characters.
        assert_eq!(
            kind_and_query("hi @fo", true),
            Some((TriggerKind::User, "fo".into()))
        );
        assert_eq!(
            kind_and_query("hi :k", true),
            Some((TriggerKind::Emote, "k".into()))
        );
    }

    #[test]
    fn a_command_is_only_the_first_word() {
        assert_eq!(trigger("hello /me", false), None);
        assert_eq!(trigger("hello /me", true), None);
        assert!(trigger("  /me", false).is_some());
        // `//text` is a literal slash, not a command.
        assert_eq!(trigger("//me", false), None);
    }

    #[test]
    fn the_caret_in_the_middle_of_a_word() {
        // "Kap|pa": no popup while typing, Tab completes the part before the caret only.
        assert_eq!(find_trigger("Kappa", 3, false), None);
        let t = find_trigger("Kappa", 3, true).unwrap();
        assert_eq!((t.range.clone(), t.query.as_str()), (0..3, "Kap"));
        // Before a space it is the end of a word.
        let t = find_trigger("@fo bar", 3, false).unwrap();
        assert_eq!((t.range, t.query.as_str()), (0..3, "fo"));
        // Never splits a character.
        assert_eq!(find_trigger("é", 1, true), None);
        assert_eq!(find_trigger("ab", 9, true), None);
    }

    #[test]
    fn non_ascii_words() {
        let t = trigger("hi :éa", false).unwrap();
        assert_eq!(t.query, "éa");
        assert_eq!(t.range, 3..7);
    }

    #[test]
    fn accepting_replaces_the_token_and_adds_a_space() {
        let text = "hi :kap";
        let t = trigger(text, false).unwrap();
        let edit = apply(&t, "Kappa", "");
        assert_eq!(edit.applied_to(text), ("hi Kappa ".to_owned(), 9));

        let text = "yo @fo";
        let t = trigger(text, false).unwrap();
        assert_eq!(
            apply(&t, "Forsen", "").applied_to(text),
            ("yo @Forsen ".to_owned(), 11)
        );

        let t = trigger("/m", false).unwrap();
        assert_eq!(apply(&t, "me", "").applied_to("/m"), ("/me ".to_owned(), 4));
    }

    #[test]
    fn the_picker_inserts_a_word_with_the_spacing_it_needs() {
        let insert = |text: &str, at: Range<usize>| {
            let edit = Edit {
                range: at.clone(),
                text: word_insertion(text, at, "Kappa"),
            };
            edit.applied_to(text).0
        };
        assert_eq!(insert("", 0..0), "Kappa ");
        assert_eq!(insert("hi", 2..2), "hi Kappa ");
        assert_eq!(insert("hi ", 3..3), "hi Kappa ");
        // Before a word: a space after, none needed before the start.
        assert_eq!(insert("hi", 0..0), "Kappa hi");
        // Between two words with a space either side: no doubled spaces.
        assert_eq!(insert("a  b", 2..2), "a Kappa b");
        // A selection is replaced.
        assert_eq!(insert("say hello", 4..9), "say Kappa ");
        // Out-of-range input does not panic.
        assert_eq!(word_insertion("é", 1..1, "x"), "x ");
    }

    #[test]
    fn accepting_in_the_middle_keeps_the_rest() {
        // `@fo|o bar` is not a trigger, but Tab on `:ka|` before a space is: no second space.
        let text = "hi :ka there";
        let t = find_trigger(text, 6, false).unwrap();
        let (out, caret) = apply(&t, "Kappa", &text[6..]).applied_to(text);
        assert_eq!(out, "hi Kappa there");
        assert_eq!(caret, "hi Kappa".len());
        // Mid-word (explicit): a space is added so the rest of the word is not glued on.
        let text = "Kapxyz";
        let t = find_trigger(text, 3, true).unwrap();
        assert_eq!(
            apply(&t, "Kappa", &text[3..]).applied_to(text),
            ("Kappa xyz".to_owned(), 6)
        );
    }

    fn names<'a>(items: &'a [&'a str], order: &[usize]) -> Vec<&'a str> {
        order.iter().map(|&i| items[i]).collect()
    }

    fn ranked(query: &str, items: &[&str]) -> Vec<String> {
        names(items, &rank(query, items, |s| s, |_| None, 50))
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn tiers_run_from_exact_prefix_to_subsequence() {
        let items = [
            "xxkappaxx",  // substring
            "monkaPog",   // hump of "pog"
            "Pogchamp",   // case-insensitive prefix of "pog"
            "pog",        // exact prefix
            "PepegaOgre", // subsequence p-o-g
            "unrelated",
        ];
        assert_eq!(
            ranked("pog", &items),
            ["pog", "Pogchamp", "monkaPog", "PepegaOgre"]
        );
        assert_eq!(ranked("kappa", &items), ["xxkappaxx"]);
        assert_eq!(ranked("zzz", &items), Vec::<String>::new());
    }

    #[test]
    fn match_tier_details() {
        assert_eq!(match_tier("Kap", "Kappa"), Some(Tier::Prefix));
        assert_eq!(match_tier("kap", "Kappa"), Some(Tier::PrefixIgnoreCase));
        assert_eq!(match_tier("pog", "monkaPog"), Some(Tier::Hump));
        assert_eq!(match_tier("w", "monkaW"), Some(Tier::Hump));
        assert_eq!(match_tier("2", "Kappa2"), Some(Tier::Hump));
        assert_eq!(match_tier("fish", "gold_fish"), Some(Tier::Hump));
        assert_eq!(match_tier("ppa", "Kappa"), Some(Tier::Substring));
        assert_eq!(match_tier("mkw", "monkaW"), Some(Tier::Subsequence));
        // One character never matches as a mere subsequence.
        assert_eq!(match_tier("z", "Kappa"), None);
        assert_eq!(match_tier("a", "Kappa"), Some(Tier::Substring));
        assert_eq!(match_tier("", "anything"), Some(Tier::Prefix));
        // A character that changes length when lowercased does not break it (`İ` becomes two).
        assert!(match_tier("stan", "İstanbul").is_some());
    }

    #[test]
    fn recent_use_breaks_ties_within_a_tier_only() {
        let items = ["KappaPride", "Kappa", "KappaHD", "xxkap"];
        let recent = |name: &&str| (*name == "KappaHD").then_some(0);
        let order = rank("Kap", &items, |s| s, recent, 10);
        // Recently used first among the prefix matches, then shorter; the substring match last
        // however recent it is.
        assert_eq!(
            names(&items, &order),
            ["KappaHD", "Kappa", "KappaPride", "xxkap"]
        );
        let recent = |name: &&str| (*name == "xxkap").then_some(0);
        let order = rank("Kap", &items, |s| s, recent, 10);
        assert_eq!(names(&items, &order)[3], "xxkap");
    }

    #[test]
    fn the_empty_query_lists_everything_by_recency_then_name() {
        let items = ["b", "a", "c"];
        let recent = |name: &&str| (*name == "c").then_some(0);
        let order = rank("", &items, |s| s, recent, 10);
        assert_eq!(names(&items, &order), ["c", "a", "b"]);
        assert_eq!(rank("", &items, |s| s, |_| None, 2).len(), 2);
    }

    #[test]
    fn ranking_is_deterministic_for_equal_names() {
        let items = ["Kappa", "Kappa", "kappa"];
        let a = rank("kap", &items, |s| s, |_| None, 10);
        let b = rank("kap", &items, |s| s, |_| None, 10);
        assert_eq!(a, b);
        // The case-sensitive prefix match comes first: "kappa" for the query "kap".
        assert_eq!(a[0], 2);
    }

    fn message(login: &str, display: &str, kind: MessageKind) -> Message {
        let mut message = Message::chat(
            "id",
            Timestamp::from_unix_millis(0),
            Author::new(login, display),
            vec![],
        );
        message.kind = kind;
        message
    }

    #[test]
    fn recent_chatters_are_newest_first_and_unique() {
        let messages = [
            message("alice", "Alice", MessageKind::Chat),
            message("bob", "bob", MessageKind::Chat),
            message("system", "", MessageKind::System),
            message("alice", "Alice", MessageKind::Action),
            message("carol", "", MessageKind::Chat),
            message("dave", "Dave", MessageKind::UserNotice),
            message("", "", MessageKind::Chat),
        ];
        let chatters = recent_chatters(messages.iter(), 10);
        let who: Vec<_> = chatters.iter().map(|c| c.login.as_str()).collect();
        assert_eq!(who, ["carol", "alice", "bob"]);
        // An empty display name falls back to the login.
        assert_eq!(chatters[0].display_name, "carol");
        assert_eq!(recent_chatters(messages.iter(), 2).len(), 2);
        assert!(recent_chatters(std::iter::empty::<&Message>(), 5).is_empty());
        // Shared messages work too.
        let shared: Vec<Arc<Message>> = messages.iter().cloned().map(Arc::new).collect();
        assert_eq!(recent_chatters(shared.iter().map(|m| &**m), 10).len(), 3);
    }

    #[test]
    fn users_are_ranked_by_who_spoke_last() {
        let chatters = ["zed", "zoe", "bob"];
        let recency = |name: &&str| chatters.iter().position(|c| c == name);
        let order = rank("z", &chatters, |s| s, recency, 10);
        assert_eq!(names(&chatters, &order), ["zed", "zoe"]);
        let order = rank("", &chatters, |s| s, recency, 10);
        assert_eq!(names(&chatters, &order), ["zed", "zoe", "bob"]);
    }

    #[test]
    fn every_listed_command_is_one_the_parser_accepts() {
        for command in COMMANDS {
            let line = format!("/{} text", command.name);
            assert!(
                !matches!(
                    parse_input(&line),
                    Input::Invalid(InputError::UnknownCommand(_))
                ),
                "/{} is listed but not parsed",
                command.name
            );
            assert!(command.usage.starts_with(&format!("/{}", command.name)));
        }
    }

    #[test]
    fn recents_keep_the_newest_first_without_repeats() {
        let mut recents = Recents::default();
        for name in ["a", "b", "c", "a", " ", ""] {
            recents.push(name);
        }
        assert_eq!(recents.names(), ["a", "c", "b"]);
        assert_eq!(recents.rank_of("c"), Some(1));
        assert_eq!(recents.rank_of("z"), None);
        for i in 0..100 {
            recents.push(&format!("e{i}"));
        }
        assert_eq!(recents.names().len(), RECENT_EMOTES);
        assert_eq!(recents.names()[0], "e99");
    }

    #[test]
    fn recents_read_from_storage_keep_their_order() {
        let recents = Recents::from_names(["x", "y", "x", "", "z"].map(String::from));
        assert_eq!(recents.names(), ["x", "y", "z"]);
        let many = (0..100).map(|i| format!("n{i}"));
        let recents = Recents::from_names(many);
        assert_eq!(recents.names().len(), RECENT_EMOTES);
        assert_eq!(recents.names()[0], "n0");
    }
}
