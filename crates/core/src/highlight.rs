//! Highlight rules (PR 5.3): which messages the user wants to notice.
//!
//! [`HighlightConfig`] is the `[highlights]` section of `settings.toml` (the settings crate embeds
//! it as is). [`Highlights`] is that configuration compiled once and then asked about each
//! message ([`Highlights::evaluate`]); the matching is pure code, no clock and no I/O.
//!
//! What counts as a hit, in order of priority for the colour (the first that matches wins, and a
//! message that matches several keeps the strongest signal: it notifies if *any* matching rule says
//! so):
//!
//! 1. the user's own name: the login or the display name, anywhere in the text, as a whole word
//!    (an `@` in front is not part of the word) and ignoring case,
//! 2. the custom phrases and regular expressions, in the order they are written,
//! 3. messages from highlighted users.
//!
//! Messages the user wrote themselves never match. Text is matched as typed: an emote is its name,
//! a link is its text, so `pog` as a whole word does not match the emote `PogChamp` but does match
//! `pog PogChamp`.
//!
//! Matching ignores case with Unicode simple case folding (`É` = `é`, `Σ` = `ς`). Regular
//! expressions run on the `regex` crate, whose search time is linear in the message however the
//! pattern is written (no backtracking), and are compiled with a size limit, so no pattern can
//! stall the chat; a pattern that matches the empty string (so every message) is refused. They are
//! case-insensitive unless they say `(?-i)`.

use std::borrow::Cow;
use std::collections::HashMap;

use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use serde::{Deserialize, Serialize};

use crate::message::{Message, MessageKind};

/// Most phrase and regex rules a configuration may hold.
pub const MAX_RULES: usize = 200;
/// Most highlighted users a configuration may hold.
pub const MAX_USERS: usize = 500;
/// Longest pattern (characters).
pub const MAX_PATTERN_CHARS: usize = 500;
/// Longest Twitch login.
pub(crate) const MAX_LOGIN_CHARS: usize = 25;
/// Memory one compiled phrase or regular expression may take (and its lazy DFA cache).
pub(crate) const REGEX_SIZE_LIMIT: usize = 256 << 10;
/// The budget for all regular expressions together (the one set they are searched with).
pub(crate) const REGEX_SET_SIZE_LIMIT: usize = 4 << 20;
/// Nesting depth a regular expression may have.
pub(crate) const REGEX_NEST_LIMIT: u32 = 50;

/// The colour of a highlighted row: one of the theme's own colours, so every theme (and light and
/// dark) gets a suitable one. The names are what `settings.toml` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HighlightColor {
    /// The theme's `danger` colour.
    Red,
    /// The theme's `success` colour.
    Green,
    /// The theme's `warning` colour.
    Yellow,
    /// The theme's accent. Also what a value this build does not know reads as, so a file from a
    /// newer build still loads.
    #[default]
    #[serde(other)]
    Accent,
}

impl HighlightColor {
    /// Every colour, in the order the settings window lists them.
    pub const ALL: [Self; 4] = [Self::Accent, Self::Red, Self::Green, Self::Yellow];

    /// The name used in `settings.toml`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Accent => "accent",
            Self::Red => "red",
            Self::Green => "green",
            Self::Yellow => "yellow",
        }
    }

    /// The colour called `id` (the names of [`HighlightColor::id`], any case).
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|color| color.id().eq_ignore_ascii_case(id.trim()))
    }

    /// What the settings window calls it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Accent => "Accent",
            Self::Red => "Red",
            Self::Green => "Green",
            Self::Yellow => "Yellow",
        }
    }
}

/// How a rule's pattern is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleKind {
    /// Plain text, ignoring case.
    #[default]
    Phrase,
    /// A regular expression (the `regex` crate's syntax).
    Regex,
}

/// One phrase or regular expression (`[[highlights.rules]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightRule {
    pub kind: RuleKind,
    /// The text to look for, or the regular expression.
    pub pattern: String,
    /// Phrases only: match only whole words (`ann` is not found in `channel`). A phrase that
    /// starts or ends with a character that is not a letter, digit or `_` (`c++`) needs no
    /// boundary there.
    pub whole_word: bool,
    pub color: HighlightColor,
    /// Raise a system notification for a message this rule matches.
    pub notify: bool,
}

impl Default for HighlightRule {
    fn default() -> Self {
        Self {
            kind: RuleKind::Phrase,
            pattern: String::new(),
            whole_word: true,
            color: HighlightColor::Accent,
            notify: false,
        }
    }
}

/// A user whose messages are highlighted (`[[highlights.users]]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightUser {
    /// The login, as `forsen` (an `@` in front and capitals are ignored).
    pub login: String,
    pub color: HighlightColor,
    pub notify: bool,
}

/// `[highlights]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightConfig {
    /// Highlight messages that mention you (your login or display name).
    pub mentions: bool,
    pub mention_color: HighlightColor,
    /// Notify for a mention.
    pub mention_notify: bool,
    /// Master switch for system notifications. Off: highlights still show, nothing is posted.
    pub notifications: bool,
    /// Show who said what in a notification. Off: it only says there is a new highlight in the
    /// channel, so nothing private reaches a locked screen.
    pub notification_previews: bool,
    pub rules: Vec<HighlightRule>,
    pub users: Vec<HighlightUser>,
}

impl Default for HighlightConfig {
    fn default() -> Self {
        Self {
            mentions: true,
            mention_color: HighlightColor::Accent,
            mention_notify: true,
            notifications: true,
            notification_previews: true,
            rules: Vec::new(),
            users: Vec::new(),
        }
    }
}

/// What is wrong with one entry of a configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleProblem {
    pub kind: ProblemKind,
    pub reason: String,
}

/// Which entry a [`RuleProblem`] is about (a zero-based index into its list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    Rule(usize),
    User(usize),
    /// The lists themselves (too many entries).
    Lists,
}

impl std::fmt::Display for RuleProblem {
    /// `rule 3 (`(`): unclosed group` (numbered from 1, as a person counts).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ProblemKind::Rule(i) => write!(f, "rule {}: {}", i + 1, self.reason),
            ProblemKind::User(i) => write!(f, "user {}: {}", i + 1, self.reason),
            ProblemKind::Lists => f.write_str(&self.reason),
        }
    }
}

/// `text` as a login: no `@`, no capitals, no surrounding space.
pub fn normalise_user(text: &str) -> String {
    text.trim().trim_start_matches('@').to_lowercase()
}

pub(crate) fn login_problem(login: &str) -> Option<String> {
    if login.is_empty() {
        return Some("a login is required".to_owned());
    }
    if login.chars().count() > MAX_LOGIN_CHARS
        || !login
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Some(format!(
            "{login:?} is not a Twitch login (letters, digits and _, up to {MAX_LOGIN_CHARS})"
        ));
    }
    None
}

impl HighlightConfig {
    /// Every problem of the configuration, in file order; empty when all of it can be used.
    pub fn problems(&self) -> Vec<RuleProblem> {
        Highlights::compile(self).1
    }

    /// The first problem, for a check that stops at one.
    pub fn first_problem(&self) -> Option<RuleProblem> {
        self.problems().into_iter().next()
    }
}

/// One user entry that compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UserEntry {
    color: HighlightColor,
    notify: bool,
}

/// A phrase, or the own-name pattern, ready to run.
#[derive(Debug, Clone)]
pub(crate) struct Matcher {
    regex: Regex,
    /// Needs a word boundary at each end.
    whole_word: bool,
}

/// Checks the length the user typed (before anything is escaped), so the limit is the one the
/// message names.
fn check_length(pattern: &str) -> Result<(), String> {
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err(format!("longer than {MAX_PATTERN_CHARS} characters"));
    }
    Ok(())
}

fn builder(pattern: &str, size_limit: usize) -> RegexBuilder {
    let mut builder = RegexBuilder::new(pattern);
    builder
        .case_insensitive(true)
        .unicode(true)
        .size_limit(size_limit)
        .dfa_size_limit(size_limit)
        .nest_limit(REGEX_NEST_LIMIT);
    builder
}

/// A regular expression that compiles within the limits and does not match everything.
pub(crate) fn check_regex(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("a regular expression is required".to_owned());
    }
    check_length(pattern)?;
    let regex = builder(pattern, REGEX_SIZE_LIMIT)
        .build()
        .map_err(|error| short_error(&error))?;
    if regex.is_match("") {
        return Err("matches every message (it also matches nothing at all)".to_owned());
    }
    Ok(())
}

impl Matcher {
    pub(crate) fn phrase(phrase: &str, whole_word: bool) -> Result<Self, String> {
        check_length(phrase)?;
        let phrase = phrase.trim();
        if phrase.is_empty() {
            return Err("a phrase is required".to_owned());
        }
        Self::build(&regex::escape(phrase), whole_word)
    }

    fn build(pattern: &str, whole_word: bool) -> Result<Self, String> {
        let regex = builder(pattern, REGEX_SIZE_LIMIT)
            .build()
            .map_err(|error| short_error(&error))?;
        Ok(Self { regex, whole_word })
    }

    pub(crate) fn is_match(&self, text: &str) -> bool {
        if !self.whole_word {
            return self.regex.is_match(text);
        }
        let mut from = 0;
        while from <= text.len() {
            let Some(found) = self.regex.find_at(text, from) else {
                return false;
            };
            if found.is_empty() {
                return false;
            }
            if bounded(text, found.start(), found.end()) {
                return true;
            }
            // Look again from the next character: a hit may start inside this one.
            let step = text[found.start()..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
            from = found.start() + step;
        }
        false
    }
}

/// How one rule looks for its match.
#[derive(Debug, Clone)]
enum RuleMatcher {
    Phrase(Matcher),
    /// Position in [`Highlights::regex_set`].
    Regex(usize),
}

/// Why `rule` cannot be used (`` `pattern`: reason ``), or `None` when it can. The same check
/// [`Highlights::compile`] makes, for one entry on its own (a settings field numbers its own lines).
pub fn rule_problem(rule: &HighlightRule) -> Option<String> {
    let built = match rule.kind {
        RuleKind::Phrase => Matcher::phrase(&rule.pattern, rule.whole_word).map(drop),
        RuleKind::Regex => check_regex(&rule.pattern),
    };
    built
        .err()
        .map(|reason| format!("`{}`: {reason}", rule.pattern))
}

/// Why `user` cannot be used, or `None`.
pub fn user_problem(user: &HighlightUser) -> Option<String> {
    login_problem(&normalise_user(&user.login))
}

/// `text` in lowercase, without copying when it already is (Twitch logins are).
pub(crate) fn lowercase(text: &str) -> Cow<'_, str> {
    if text.chars().any(char::is_uppercase) {
        Cow::Owned(text.to_lowercase())
    } else {
        Cow::Borrowed(text)
    }
}

/// A letter, digit or `_`: what a word is made of.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `text[start..end]` stands as a word of its own: where it starts or ends with a word
/// character, no word character touches it from outside.
fn bounded(text: &str, start: usize, end: usize) -> bool {
    let inside = &text[start..end];
    let before_ok = !inside.chars().next().is_some_and(is_word)
        || !text[..start].chars().next_back().is_some_and(is_word);
    let after_ok = !inside.chars().next_back().is_some_and(is_word)
        || !text[end..].chars().next().is_some_and(is_word);
    before_ok && after_ok
}

/// The line of a regex error that says what is wrong ("unclosed group").
fn short_error(error: &regex::Error) -> String {
    match error {
        regex::Error::Syntax(text) => text
            .lines()
            .rev()
            .find_map(|line| line.trim().strip_prefix("error:"))
            .map_or_else(
                || text.lines().next().unwrap_or("invalid").trim().to_owned(),
                |reason| reason.trim().to_owned(),
            ),
        regex::Error::CompiledTooBig(_) => "too large or complex".to_owned(),
        other => other.to_string(),
    }
}

/// The local user, for the mention rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Own {
    /// Lowercase login.
    pub login: String,
    /// Display name as Twitch writes it; may differ from the login in capitals or script.
    pub display_name: Option<String>,
}

impl Own {
    pub fn new(login: impl Into<String>, display_name: Option<String>) -> Self {
        Self {
            login: login.into().to_lowercase(),
            display_name: display_name.filter(|name| !name.trim().is_empty()),
        }
    }
}

/// Why a message is a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitReason {
    /// It names the user.
    Mention,
    /// The rule at this index of [`HighlightConfig::rules`].
    Rule(usize),
    /// The user entry at this index of [`HighlightConfig::users`].
    User(usize),
}

/// A message a rule matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// The colour of the first matching rule (mention, then rules in order, then users).
    pub color: HighlightColor,
    /// Some matching rule asks for a notification.
    pub notify: bool,
    /// The message names the user (whatever colour won).
    pub mention: bool,
    /// What the colour came from.
    pub reason: HitReason,
}

/// A [`HighlightConfig`] compiled for matching, plus who the local user is.
#[derive(Debug, Clone, Default)]
pub struct Highlights {
    mentions: Option<(HighlightColor, bool)>,
    own: Option<Own>,
    own_matcher: Option<Matcher>,
    /// `(index in the configuration, matcher, colour, notify)`, in file order.
    rules: Vec<(usize, RuleMatcher, HighlightColor, bool)>,
    /// Every regular expression, searched together in one pass over the message.
    regex_set: Option<RegexSet>,
    /// By lowercase login: `(index of the first entry, entry)`; entries for one login are merged
    /// (the first colour, a notification if any asks).
    users: HashMap<String, (usize, UserEntry)>,
}

impl Highlights {
    /// Compiles `config`: the entries that work, and a problem for each that does not (which are
    /// left out, so one bad regular expression does not switch the others off).
    pub fn compile(config: &HighlightConfig) -> (Self, Vec<RuleProblem>) {
        let mut problems = Vec::new();
        let mut rules = Vec::new();
        let mut patterns: Vec<&str> = Vec::new();
        let mut users: HashMap<String, (usize, UserEntry)> = HashMap::new();
        if config.rules.len() > MAX_RULES {
            problems.push(RuleProblem {
                kind: ProblemKind::Lists,
                reason: format!("more than {MAX_RULES} rules"),
            });
        }
        if config.users.len() > MAX_USERS {
            problems.push(RuleProblem {
                kind: ProblemKind::Lists,
                reason: format!("more than {MAX_USERS} highlighted users"),
            });
        }
        for (index, rule) in config.rules.iter().enumerate().take(MAX_RULES) {
            let built = match rule.kind {
                RuleKind::Phrase => {
                    Matcher::phrase(&rule.pattern, rule.whole_word).map(RuleMatcher::Phrase)
                }
                RuleKind::Regex => check_regex(&rule.pattern).map(|()| {
                    patterns.push(&rule.pattern);
                    RuleMatcher::Regex(patterns.len() - 1)
                }),
            };
            match built {
                Ok(matcher) => rules.push((index, matcher, rule.color, rule.notify)),
                Err(reason) => problems.push(RuleProblem {
                    kind: ProblemKind::Rule(index),
                    reason: format!("`{}`: {reason}", rule.pattern),
                }),
            }
        }
        // One pass over the message for all regular expressions, within one memory budget: when
        // they do not fit together none of them is used (and the problem says so).
        let regex_set = if patterns.is_empty() {
            None
        } else {
            match RegexSetBuilder::new(&patterns)
                .case_insensitive(true)
                .unicode(true)
                .size_limit(REGEX_SET_SIZE_LIMIT)
                .dfa_size_limit(REGEX_SET_SIZE_LIMIT)
                .nest_limit(REGEX_NEST_LIMIT)
                .build()
            {
                Ok(set) => Some(set),
                Err(_) => {
                    problems.push(RuleProblem {
                        kind: ProblemKind::Lists,
                        reason: "the regular expressions together are too large or complex"
                            .to_owned(),
                    });
                    rules.retain(|(_, matcher, _, _)| !matches!(matcher, RuleMatcher::Regex(_)));
                    None
                }
            }
        };
        for (index, user) in config.users.iter().enumerate().take(MAX_USERS) {
            let login = normalise_user(&user.login);
            match login_problem(&login) {
                None => {
                    users
                        .entry(login)
                        .and_modify(|(_, entry)| entry.notify |= user.notify)
                        .or_insert((
                            index,
                            UserEntry {
                                color: user.color,
                                notify: user.notify,
                            },
                        ));
                }
                Some(reason) => problems.push(RuleProblem {
                    kind: ProblemKind::User(index),
                    reason,
                }),
            }
        }
        let engine = Self {
            mentions: config
                .mentions
                .then_some((config.mention_color, config.mention_notify)),
            own: None,
            own_matcher: None,
            rules,
            regex_set,
            users,
        };
        (engine, problems)
    }

    /// Says who the local user is (`None`: nobody, mentions never match). Compiles the name
    /// matcher, so call it when the user changes, not for every message; the same user again does
    /// nothing.
    pub fn set_own(&mut self, own: Option<Own>) {
        if self.own == own {
            return;
        }
        self.own_matcher = own.as_ref().and_then(|own| {
            let mut names = vec![regex::escape(&own.login)];
            if let Some(display) = &own.display_name
                && !display.eq_ignore_ascii_case(&own.login)
            {
                names.push(regex::escape(display.trim()));
            }
            if own.login.is_empty() {
                return None;
            }
            Matcher::build(&names.join("|"), true).ok()
        });
        self.own = own;
    }

    /// The user this was told about.
    pub fn own(&self) -> Option<&Own> {
        self.own.as_ref()
    }

    /// Whether any rule could match (so a caller can skip reading the message at all).
    pub fn is_empty(&self) -> bool {
        (self.mentions.is_none() || self.own_matcher.is_none())
            && self.rules.is_empty()
            && self.users.is_empty()
    }

    /// Whether `message` is written by the local user.
    pub fn is_own(&self, message: &Message) -> bool {
        match (&self.own, &message.author) {
            (Some(own), Some(author)) => own.login == *lowercase(&author.login),
            _ => false,
        }
    }

    /// The rule hit by `message`, if any. Only lines somebody wrote (chat, `/me`, the text of an
    /// event) can match, and never the local user's own.
    pub fn evaluate(&self, message: &Message) -> Option<Hit> {
        if !matches!(
            message.kind,
            MessageKind::Chat | MessageKind::Action | MessageKind::UserNotice
        ) || self.is_empty()
        {
            return None;
        }
        let author = message.author.as_ref()?;
        if self.is_own(message) {
            return None;
        }
        let text = message.plain_text();
        let mut hit: Option<Hit> = None;
        let mut mention = false;
        let mut notify = false;
        let mut take = |hit: &mut Option<Hit>, color, wants, reason| {
            notify |= wants;
            hit.get_or_insert(Hit {
                color,
                notify: false,
                mention: false,
                reason,
            });
        };
        if let Some((color, wants)) = self.mentions
            && let Some(matcher) = &self.own_matcher
            && matcher.is_match(&text)
        {
            mention = true;
            take(&mut hit, color, wants, HitReason::Mention);
        }
        // Every regular expression in one pass, then the rules in file order.
        let regexes = self.regex_set.as_ref().map(|set| set.matches(&text));
        for (index, matcher, color, wants) in &self.rules {
            let matched = match matcher {
                RuleMatcher::Phrase(phrase) => phrase.is_match(&text),
                RuleMatcher::Regex(at) => regexes.as_ref().is_some_and(|m| m.matched(*at)),
            };
            if matched {
                take(&mut hit, *color, *wants, HitReason::Rule(*index));
            }
        }
        if let Some((index, entry)) = self.users.get(&*lowercase(&author.login)) {
            take(&mut hit, entry.color, entry.notify, HitReason::User(*index));
        }
        hit.map(|hit| Hit {
            notify,
            mention,
            ..hit
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote::Emote;
    use crate::message::{Author, Fragment, Timestamp};
    use std::sync::Arc;

    fn rule(pattern: &str) -> HighlightRule {
        HighlightRule {
            pattern: pattern.into(),
            ..HighlightRule::default()
        }
    }

    fn regex_rule(pattern: &str) -> HighlightRule {
        HighlightRule {
            kind: RuleKind::Regex,
            ..rule(pattern)
        }
    }

    fn engine(config: HighlightConfig) -> Highlights {
        let (mut engine, problems) = Highlights::compile(&config);
        assert!(problems.is_empty(), "{problems:?}");
        engine.set_own(Some(Own::new("Viewer", Some("Vïewer".into()))));
        engine
    }

    fn only(rules: Vec<HighlightRule>) -> Highlights {
        engine(HighlightConfig {
            mentions: false,
            rules,
            ..HighlightConfig::default()
        })
    }

    fn say(login: &str, text: &str) -> Message {
        Message::chat(
            "m",
            Timestamp(0),
            Author::new(login, login),
            vec![Fragment::Text(text.into())],
        )
    }

    fn hit(engine: &Highlights, text: &str) -> Option<Hit> {
        engine.evaluate(&say("alice", text))
    }

    #[test]
    fn a_name_is_a_mention_as_a_whole_word_in_any_case_with_or_without_the_at_sign() {
        let e = engine(HighlightConfig::default());
        for text in [
            "hi @viewer",
            "VIEWER!",
            "hello, viewer.",
            "@Viewer: look",
            "see viewer's clip",
            "hey Vïewer",
            "hey vÏEWER",
        ] {
            let found = hit(&e, text);
            assert!(found.is_some_and(|h| h.mention), "{text}");
        }
        for text in [
            "viewers",
            "interviewer",
            "viewer_2",
            "2viewer",
            "no mention",
        ] {
            assert_eq!(hit(&e, text), None, "{text}");
        }
    }

    #[test]
    fn a_mention_uses_the_configured_colour_and_notify_and_can_be_switched_off() {
        let e = engine(HighlightConfig {
            mention_color: HighlightColor::Red,
            mention_notify: false,
            ..HighlightConfig::default()
        });
        let found = hit(&e, "yo @viewer").unwrap();
        assert_eq!(
            (found.color, found.notify, found.reason),
            (HighlightColor::Red, false, HitReason::Mention)
        );
        let off = engine(HighlightConfig {
            mentions: false,
            ..HighlightConfig::default()
        });
        assert_eq!(hit(&off, "yo @viewer"), None);
    }

    #[test]
    fn nobody_is_mentioned_while_the_user_is_unknown() {
        let (mut e, _) = Highlights::compile(&HighlightConfig::default());
        assert_eq!(hit(&e, "viewer"), None, "no own name yet");
        e.set_own(Some(Own::new("viewer", None)));
        assert!(hit(&e, "viewer").is_some());
        e.set_own(None);
        assert_eq!(hit(&e, "viewer"), None);
    }

    #[test]
    fn own_messages_and_lines_nobody_wrote_never_match() {
        let e = engine(HighlightConfig::default());
        assert_eq!(
            e.evaluate(&say("viewer", "talking to myself, viewer")),
            None
        );
        assert_eq!(e.evaluate(&say("VIEWER", "viewer")), None);
        let notice = Message::plain("n", Timestamp(0), MessageKind::Notice, "viewer");
        assert_eq!(e.evaluate(&notice), None);
        let system = Message::plain("s", Timestamp(0), MessageKind::System, "viewer");
        assert_eq!(e.evaluate(&system), None);
        let mut action = say("alice", "waves at viewer");
        action.kind = MessageKind::Action;
        assert!(e.evaluate(&action).is_some());
    }

    #[test]
    fn a_phrase_ignores_case_and_by_default_needs_whole_words() {
        let e = only(vec![rule("nice play")]);
        assert!(hit(&e, "That was a NICE play!").is_some());
        assert!(hit(&e, "nice play").is_some());
        assert_eq!(hit(&e, "a nice playlist"), None);
        assert_eq!(hit(&e, "unice play"), None);
        assert_eq!(hit(&e, "nice  play"), None, "spacing is part of the phrase");
    }

    #[test]
    fn a_phrase_can_match_inside_words() {
        let e = only(vec![HighlightRule {
            whole_word: false,
            ..rule("ann")
        }]);
        assert!(hit(&e, "channel").is_some());
        let word = only(vec![rule("ann")]);
        assert_eq!(hit(&word, "channel"), None);
        assert!(hit(&word, "hi ann!").is_some());
    }

    #[test]
    fn a_phrase_ending_in_a_symbol_needs_no_boundary_there() {
        let e = only(vec![rule("c++")]);
        assert!(hit(&e, "I love c++").is_some());
        assert!(hit(&e, "c++, mostly").is_some());
        assert!(hit(&e, "C++").is_some());
        assert_eq!(
            hit(&e, "abc++"),
            None,
            "the word side still needs a boundary"
        );
        let both = only(vec![rule("!hype")]);
        assert!(hit(&both, "yes !hype").is_some());
        assert!(hit(&both, "yes!hype").is_some(), "the symbol side is free");
        assert_eq!(hit(&both, "!hyped"), None);
    }

    #[test]
    fn a_later_occurrence_is_found_after_a_failed_boundary() {
        let e = only(vec![rule("aa")]);
        assert_eq!(hit(&e, "aaa"), None);
        assert!(hit(&e, "aaa aa").is_some());
        let e = only(vec![rule("pog")]);
        assert!(hit(&e, "pogchamp pog").is_some());
        assert!(hit(&e, "xpog pog").is_some());
    }

    #[test]
    fn matching_folds_unicode_case() {
        let e = only(vec![rule("école"), rule("σας")]);
        assert!(hit(&e, "ÉCOLE tomorrow").is_some());
        assert!(hit(&e, "l'École").is_some());
        // Final and medial sigma are one letter for case folding.
        assert!(hit(&e, "ΣΑΣ").is_some());
        assert!(hit(&e, "σας").is_some());
        // Simple case folding only (what the `regex` crate does): ß and ẞ are one letter, but
        // nothing folds ß to "ss", so STRASSE is not straße.
        let sharp = only(vec![rule("straße")]);
        assert!(hit(&sharp, "STRAßE").is_some());
        assert!(hit(&sharp, "STRAẞE").is_some());
        assert_eq!(hit(&sharp, "STRASSE"), None);
        let ascii = only(vec![rule("strasse")]);
        assert_eq!(hit(&ascii, "STRAßE"), None);
        // Turkish dotted İ and dotless ı are letters of their own: a plain i does not match
        // them (and they do not match it), but İ matches İ and ı matches ı.
        let plain = only(vec![rule("istanbul")]);
        assert!(hit(&plain, "ISTANBUL").is_some());
        assert_eq!(hit(&plain, "İSTANBUL"), None);
        assert_eq!(hit(&plain, "ıstanbul"), None);
        let dotted = only(vec![rule("İstanbul")]);
        assert!(hit(&dotted, "İSTANBUL").is_some());
        assert_eq!(hit(&dotted, "ISTANBUL"), None);
        let dotless = only(vec![rule("ısı")]);
        assert!(hit(&dotless, "ISI").is_none(), "I is not ı");
        assert!(hit(&dotless, "ısı").is_some());
        // Letters of other scripts are word characters too.
        let e = only(vec![rule("日本")]);
        assert_eq!(hit(&e, "日本語"), None);
        assert!(hit(&e, "日本 です").is_some());
    }

    #[test]
    fn emotes_and_links_are_matched_by_the_text_they_stand_for() {
        let emote = Arc::new(Emote::twitch("1", "PogChamp"));
        let with_emote = |lead: &str| {
            let mut m = say("alice", lead);
            m.fragments.push(Fragment::Emote {
                emote: emote.clone(),
                zero_width_overlays: Vec::new(),
            });
            m
        };
        let e = only(vec![rule("pog")]);
        assert_eq!(
            e.evaluate(&with_emote("")),
            None,
            "the emote PogChamp is not the word pog"
        );
        assert!(e.evaluate(&with_emote("pog ")).is_some());
        let part = only(vec![HighlightRule {
            whole_word: false,
            ..rule("pog")
        }]);
        assert!(part.evaluate(&with_emote("")).is_some());

        let mut link = say("alice", "see ");
        link.fragments.push(Fragment::Link {
            text: "example.com/pog".into(),
            url: "https://example.com/pog".into(),
        });
        assert!(e.evaluate(&link).is_some(), "the link text counts");
    }

    #[test]
    fn a_mention_fragment_counts_like_the_text_it_shows() {
        let e = engine(HighlightConfig::default());
        let mut m = say("alice", "");
        m.fragments = vec![
            Fragment::Text("hey ".into()),
            Fragment::Mention {
                text: "@viewer".into(),
                login: "viewer".into(),
                is_self: true,
            },
            Fragment::Text("!".into()),
        ];
        assert!(e.evaluate(&m).is_some_and(|h| h.mention));
        m.fragments = vec![Fragment::Mention {
            text: "@viewerfan".into(),
            login: "viewerfan".into(),
            is_self: false,
        }];
        assert_eq!(e.evaluate(&m), None);
    }

    #[test]
    fn a_regex_is_case_insensitive_and_unanchored() {
        let e = only(vec![
            regex_rule(r"\bgg( wp)?\b"),
            regex_rule(r"^!clip\s+\d+$"),
        ]);
        assert!(hit(&e, "GG WP").is_some());
        assert!(hit(&e, "ggwp").is_none());
        assert!(hit(&e, "!clip 12").is_some());
        assert!(hit(&e, "say !clip 12").is_none());
        let exact = only(vec![regex_rule("(?-i)Kappa")]);
        assert!(hit(&exact, "Kappa").is_some());
        assert!(hit(&exact, "kappa").is_none());
    }

    #[test]
    fn a_pathological_regex_is_fast_because_the_engine_never_backtracks() {
        let e = only(vec![regex_rule("(a+)+$"), regex_rule("(x+x+)+y")]);
        let text = format!("{}!", "a".repeat(50_000));
        let started = std::time::Instant::now();
        assert!(hit(&e, &text).is_none());
        assert!(hit(&e, &"x".repeat(50_000)).is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn bad_rules_are_reported_one_by_one_and_do_not_switch_the_others_off() {
        let config = HighlightConfig {
            mentions: false,
            rules: vec![
                regex_rule("("),
                rule("fine"),
                regex_rule(".*"),
                regex_rule("a{1000}{1000}"),
                rule("   "),
                regex_rule(&"x".repeat(MAX_PATTERN_CHARS + 1)),
            ],
            users: vec![
                HighlightUser {
                    login: "not a login".into(),
                    ..HighlightUser::default()
                },
                HighlightUser {
                    login: "".into(),
                    ..HighlightUser::default()
                },
                HighlightUser {
                    login: "@Good_1".into(),
                    ..HighlightUser::default()
                },
            ],
            ..HighlightConfig::default()
        };
        let (mut e, problems) = Highlights::compile(&config);
        let kinds: Vec<_> = problems.iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            [
                ProblemKind::Rule(0),
                ProblemKind::Rule(2),
                ProblemKind::Rule(3),
                ProblemKind::Rule(4),
                ProblemKind::Rule(5),
                ProblemKind::User(0),
                ProblemKind::User(1),
            ]
        );
        assert_eq!(problems[0].to_string(), "rule 1: `(`: unclosed group");
        assert!(problems[1].reason.contains("matches every message"));
        assert!(
            problems[2].reason.contains("too large") || problems[2].reason.contains("repetition"),
            "{}",
            problems[2].reason
        );
        e.set_own(None);
        assert!(e.evaluate(&say("alice", "this is fine")).is_some());
        assert!(e.evaluate(&say("good_1", "anything")).is_some());
        assert_eq!(config.first_problem().unwrap().kind, ProblemKind::Rule(0));
        assert!(HighlightConfig::default().problems().is_empty());
    }

    #[test]
    fn the_length_limit_is_on_what_was_typed_for_phrases_and_regexes_alike() {
        let long = |n: usize| "x".repeat(n);
        let config = |rules| HighlightConfig {
            rules,
            ..HighlightConfig::default()
        };
        // Exactly at the limit is fine; one more is not, and the message names that number.
        assert!(
            config(vec![rule(&long(MAX_PATTERN_CHARS))])
                .problems()
                .is_empty()
        );
        assert!(
            config(vec![regex_rule(&long(MAX_PATTERN_CHARS))])
                .problems()
                .is_empty()
        );
        for rules in [
            vec![rule(&long(MAX_PATTERN_CHARS + 1))],
            vec![regex_rule(&long(MAX_PATTERN_CHARS + 1))],
            // Characters that escaping would double do not get a phrase past the limit either.
            vec![rule(&"+".repeat(MAX_PATTERN_CHARS + 1))],
        ] {
            let problems = config(rules).problems();
            assert_eq!(problems.len(), 1);
            assert!(problems[0].reason.contains("longer than 500 characters"));
        }
        assert!(
            config(vec![rule(&"+".repeat(MAX_PATTERN_CHARS))])
                .problems()
                .is_empty()
        );
    }

    #[test]
    fn all_the_regexes_together_have_a_memory_budget() {
        // Each of these fits on its own; 200 of them together do not fit the shared budget.
        let heavy = r"[a-z]{200}";
        let one = HighlightConfig {
            rules: vec![regex_rule(heavy)],
            ..HighlightConfig::default()
        };
        assert!(one.problems().is_empty(), "{:?}", one.problems());
        let many = HighlightConfig {
            rules: (0..MAX_RULES)
                .map(|i| regex_rule(&format!("{heavy}{i}")))
                .collect(),
            ..HighlightConfig::default()
        };
        let started = std::time::Instant::now();
        let (e, problems) = Highlights::compile(&many);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].kind, ProblemKind::Lists);
        assert!(problems[0].reason.contains("together"));
        assert!(e.rules.is_empty(), "none of them is used");
    }

    #[test]
    fn regexes_are_searched_together_and_keep_their_order_among_phrases() {
        let e = only(vec![
            rule("alpha"),
            HighlightRule {
                color: HighlightColor::Red,
                ..regex_rule(r"\bbeta\b")
            },
            HighlightRule {
                color: HighlightColor::Green,
                notify: true,
                ..regex_rule("gam+a")
            },
        ]);
        let found = hit(&e, "gamma then BETA").unwrap();
        assert_eq!(
            (found.color, found.reason, found.notify),
            (HighlightColor::Red, HitReason::Rule(1), true)
        );
        let found = hit(&e, "alpha gamma").unwrap();
        assert_eq!(found.reason, HitReason::Rule(0));
        assert!(found.notify);
        assert_eq!(hit(&e, "nothing"), None);
    }

    #[test]
    fn duplicate_users_merge_and_a_rule_can_be_checked_alone() {
        let e = engine(HighlightConfig {
            mentions: false,
            users: vec![
                HighlightUser {
                    login: "Bob".into(),
                    color: HighlightColor::Green,
                    notify: false,
                },
                HighlightUser {
                    login: "@bob".into(),
                    color: HighlightColor::Red,
                    notify: true,
                },
            ],
            ..HighlightConfig::default()
        });
        let found = e.evaluate(&say("bob", "x")).unwrap();
        assert_eq!(
            (found.color, found.notify, found.reason),
            (HighlightColor::Green, true, HitReason::User(0))
        );
        assert_eq!(rule_problem(&rule("fine")), None);
        assert_eq!(
            rule_problem(&regex_rule("(")),
            Some("`(`: unclosed group".to_owned())
        );
        assert!(user_problem(&HighlightUser::default()).is_some());
        assert_eq!(
            user_problem(&HighlightUser {
                login: "ok_1".into(),
                ..HighlightUser::default()
            }),
            None
        );
    }

    #[test]
    fn a_regex_that_could_eat_memory_is_refused() {
        let config = HighlightConfig {
            rules: vec![regex_rule(r"\p{L}{200}\p{L}{200}")],
            ..HighlightConfig::default()
        };
        assert_eq!(config.problems().len(), 1);
    }

    #[test]
    fn too_many_rules_are_reported_and_the_excess_ignored() {
        let config = HighlightConfig {
            rules: (0..MAX_RULES + 5)
                .map(|i| rule(&format!("word{i}")))
                .collect(),
            ..HighlightConfig::default()
        };
        let (e, problems) = Highlights::compile(&config);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, ProblemKind::Lists);
        assert_eq!(e.rules.len(), MAX_RULES);
    }

    #[test]
    fn users_match_by_login_ignoring_case_and_the_at_sign() {
        let e = engine(HighlightConfig {
            mentions: false,
            users: vec![HighlightUser {
                login: "@Streamer".into(),
                color: HighlightColor::Green,
                notify: true,
            }],
            ..HighlightConfig::default()
        });
        let found = e.evaluate(&say("streamer", "anything")).unwrap();
        assert_eq!(
            (found.color, found.notify, found.reason),
            (HighlightColor::Green, true, HitReason::User(0))
        );
        assert!(e.evaluate(&say("STREAMER", "x")).is_some());
        assert_eq!(e.evaluate(&say("streamers", "x")), None);
    }

    #[test]
    fn the_first_match_picks_the_colour_and_any_match_can_ask_for_a_notification() {
        let e = engine(HighlightConfig {
            mention_color: HighlightColor::Red,
            mention_notify: false,
            rules: vec![
                HighlightRule {
                    color: HighlightColor::Yellow,
                    notify: false,
                    ..rule("hype")
                },
                HighlightRule {
                    color: HighlightColor::Green,
                    notify: true,
                    ..rule("hype")
                },
            ],
            users: vec![HighlightUser {
                login: "alice".into(),
                color: HighlightColor::Accent,
                notify: false,
            }],
            ..HighlightConfig::default()
        });
        let found = hit(&e, "hype").unwrap();
        assert_eq!(
            (found.color, found.reason, found.notify, found.mention),
            (HighlightColor::Yellow, HitReason::Rule(0), true, false)
        );
        let found = hit(&e, "hype for viewer").unwrap();
        assert_eq!(
            (found.color, found.reason, found.notify, found.mention),
            (HighlightColor::Red, HitReason::Mention, true, true),
            "a mention wins the colour and the later rule still asks to notify"
        );
        let found = hit(&e, "nothing").unwrap();
        assert_eq!(found.reason, HitReason::User(0));
        assert_eq!(e.evaluate(&say("bob", "nothing")), None);
    }

    #[test]
    fn colours_read_and_write_their_names() {
        for color in HighlightColor::ALL {
            assert_eq!(HighlightColor::from_id(color.id()), Some(color));
        }
        assert_eq!(
            HighlightColor::from_id(" Green "),
            Some(HighlightColor::Green)
        );
        assert_eq!(HighlightColor::from_id("blue"), None);
    }

    #[test]
    fn a_login_is_normalised() {
        assert_eq!(normalise_user(" @ForSen "), "forsen");
        assert_eq!(normalise_user("Ünï"), "ünï");
    }

    #[test]
    fn an_empty_engine_matches_nothing_quickly() {
        let (e, _) = Highlights::compile(&HighlightConfig {
            mentions: false,
            ..HighlightConfig::default()
        });
        assert!(e.is_empty());
        assert_eq!(e.evaluate(&say("a", "viewer")), None);
    }
}
