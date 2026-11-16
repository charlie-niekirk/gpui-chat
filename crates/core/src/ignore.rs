//! Ignored users and phrases (PR 5.7): messages the user does not want to see.
//!
//! [`IgnoreConfig`] is the `[ignores]` section of `settings.toml`; [`Ignores`] is that configuration
//! compiled once, plus the users on the Twitch block list ([`Ignores::set_blocked`]) and who the
//! local user is, and answers [`Ignores::is_ignored`] for one message. The matching follows the
//! conventions of [`crate::highlight`] (and shares its code): phrases are plain text compared
//! ignoring case, whole words unless the rule says otherwise; regular expressions run on the
//! `regex` crate (linear time, no backtracking), are compiled with a size limit, are
//! case-insensitive unless they say `(?-i)` and one that matches the empty text is refused.
//!
//! What can be ignored:
//!
//! * a chat message or `/me` line is ignored when its author is an ignored user (by login, or by
//!   Twitch id for the block list) or its text matches an ignored phrase or regular expression;
//! * an event card (a sub, a raid, …) only when its *user* is ignored: phrases are for what people
//!   type, and an announcement must not vanish because it says `gg`;
//! * lines nobody wrote (notices, system lines) never;
//! * the local user's own messages never, whatever the rules say.

use std::collections::HashSet;

use regex::{RegexSet, RegexSetBuilder};
use serde::{Deserialize, Serialize};

use crate::highlight::{
    MAX_PATTERN_CHARS, MAX_RULES, MAX_USERS, Matcher, ProblemKind, REGEX_NEST_LIMIT,
    REGEX_SET_SIZE_LIMIT, RuleKind, RuleProblem, check_regex, login_problem, lowercase,
    normalise_user,
};
use crate::message::{Message, MessageKind};

/// One ignored phrase or regular expression (`[[ignores.rules]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IgnoreRule {
    pub kind: RuleKind,
    /// The text to look for, or the regular expression.
    pub pattern: String,
    /// Phrases only: match only whole words (`ann` is not found in `channel`).
    pub whole_word: bool,
}

impl Default for IgnoreRule {
    fn default() -> Self {
        Self {
            kind: RuleKind::Phrase,
            pattern: String::new(),
            whole_word: true,
        }
    }
}

/// `[ignores]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IgnoreConfig {
    /// Ignored users' logins (an `@` in front and capitals are ignored).
    pub users: Vec<String>,
    /// Ignored phrases and regular expressions.
    pub rules: Vec<IgnoreRule>,
    /// Show an ignored message as a one-line "1 ignored message" instead of hiding it.
    pub show_collapsed: bool,
    /// Treat the users on your Twitch block list as ignored (needs *Enable block list sync…*).
    pub sync_block_list: bool,
}

impl Default for IgnoreConfig {
    fn default() -> Self {
        Self {
            users: Vec::new(),
            rules: Vec::new(),
            show_collapsed: false,
            sync_block_list: true,
        }
    }
}

impl IgnoreConfig {
    /// Every problem of the configuration, in file order; empty when all of it can be used.
    pub fn problems(&self) -> Vec<RuleProblem> {
        Ignores::compile(self).1
    }

    /// The first problem, for a check that stops at one.
    pub fn first_problem(&self) -> Option<RuleProblem> {
        self.problems().into_iter().next()
    }

    /// Whether `login` is one of the ignored users.
    pub fn ignores_user(&self, login: &str) -> bool {
        let login = normalise_user(login);
        self.users.iter().any(|user| normalise_user(user) == login)
    }
}

/// Why `rule` cannot be used (`` `pattern`: reason ``), or `None`.
pub fn ignore_rule_problem(rule: &IgnoreRule) -> Option<String> {
    let built = match rule.kind {
        RuleKind::Phrase => Matcher::phrase(&rule.pattern, rule.whole_word).map(drop),
        RuleKind::Regex => check_regex(&rule.pattern),
    };
    built
        .err()
        .map(|reason| format!("`{}`: {reason}", rule.pattern))
}

/// Why `login` cannot be an ignored user, or `None`.
pub fn ignore_user_problem(login: &str) -> Option<String> {
    login_problem(&normalise_user(login))
}

/// An [`IgnoreConfig`] compiled for matching.
#[derive(Debug, Clone, Default)]
pub struct Ignores {
    /// Lowercase logins from the configuration.
    users: HashSet<String>,
    /// Lowercase logins and Twitch ids of the Twitch block list.
    blocked_logins: HashSet<String>,
    blocked_ids: HashSet<String>,
    phrases: Vec<Matcher>,
    regexes: Option<RegexSet>,
    /// The local user's lowercase login: never ignored.
    own: Option<String>,
    show_collapsed: bool,
}

impl Ignores {
    /// Compiles `config`: the entries that work, and a problem for each that does not (which are
    /// left out, so one bad regular expression does not switch the others off).
    pub fn compile(config: &IgnoreConfig) -> (Self, Vec<RuleProblem>) {
        let mut problems = Vec::new();
        let mut phrases = Vec::new();
        let mut patterns: Vec<&str> = Vec::new();
        let mut users = HashSet::new();
        if config.rules.len() > MAX_RULES {
            problems.push(RuleProblem {
                kind: ProblemKind::Lists,
                reason: format!("more than {MAX_RULES} rules"),
            });
        }
        if config.users.len() > MAX_USERS {
            problems.push(RuleProblem {
                kind: ProblemKind::Lists,
                reason: format!("more than {MAX_USERS} ignored users"),
            });
        }
        for (index, rule) in config.rules.iter().enumerate().take(MAX_RULES) {
            if let Some(problem) = ignore_rule_problem(rule) {
                problems.push(RuleProblem {
                    kind: ProblemKind::Rule(index),
                    reason: problem,
                });
                continue;
            }
            match rule.kind {
                RuleKind::Phrase => {
                    if let Ok(matcher) = Matcher::phrase(&rule.pattern, rule.whole_word) {
                        phrases.push(matcher);
                    }
                }
                RuleKind::Regex => patterns.push(&rule.pattern),
            }
        }
        let regexes = if patterns.is_empty() {
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
                    None
                }
            }
        };
        for (index, login) in config.users.iter().enumerate().take(MAX_USERS) {
            let login = normalise_user(login);
            match login_problem(&login) {
                None => {
                    users.insert(login);
                }
                Some(reason) => problems.push(RuleProblem {
                    kind: ProblemKind::User(index),
                    reason,
                }),
            }
        }
        let engine = Self {
            users,
            phrases,
            regexes,
            show_collapsed: config.show_collapsed,
            ..Self::default()
        };
        (engine, problems)
    }

    /// Says who the local user is (their messages are never ignored).
    pub fn set_own(&mut self, login: Option<&str>) {
        self.own = login
            .map(str::to_lowercase)
            .filter(|login| !login.is_empty());
    }

    /// Replaces the Twitch block list: `(id, login)` of each blocked user.
    pub fn set_blocked(&mut self, blocked: impl IntoIterator<Item = (String, String)>) {
        self.blocked_logins.clear();
        self.blocked_ids.clear();
        for (id, login) in blocked {
            self.blocked_ids.insert(id);
            self.blocked_logins.insert(login.to_lowercase());
        }
    }

    /// Show ignored messages as one collapsed line instead of hiding them.
    pub fn show_collapsed(&self) -> bool {
        self.show_collapsed
    }

    /// Whether nothing is ignored at all (so a caller can skip reading messages).
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
            && self.blocked_ids.is_empty()
            && self.blocked_logins.is_empty()
            && self.phrases.is_empty()
            && self.regexes.is_none()
    }

    /// Whether `login` is an ignored user, from the configuration or the block list.
    pub fn ignores_login(&self, login: &str) -> bool {
        let login = lowercase(login);
        self.users.contains(&*login) || self.blocked_logins.contains(&*login)
    }

    /// Whether `login` is on the Twitch block list (as opposed to ignored here).
    pub fn is_blocked(&self, login: &str) -> bool {
        self.blocked_logins.contains(&*lowercase(login))
    }

    /// Whether the local user wrote `message`.
    pub fn is_own(&self, message: &Message) -> bool {
        match (&self.own, &message.author) {
            (Some(own), Some(author)) => *own == *lowercase(&author.login),
            _ => false,
        }
    }

    /// Whether `message` is one the user does not want to see (see the module docs).
    pub fn is_ignored(&self, message: &Message) -> bool {
        if self.is_empty() {
            return false;
        }
        let event = match message.kind {
            MessageKind::Chat | MessageKind::Action => false,
            MessageKind::UserNotice => true,
            MessageKind::Notice | MessageKind::System => return false,
        };
        let Some(author) = &message.author else {
            return false;
        };
        if self.is_own(message) {
            return false;
        }
        if self.ignores_login(&author.login)
            || author
                .user_id
                .as_deref()
                .is_some_and(|id| self.blocked_ids.contains(id))
        {
            return true;
        }
        if event || (self.phrases.is_empty() && self.regexes.is_none()) {
            return false;
        }
        let text = message.plain_text();
        self.phrases.iter().any(|phrase| phrase.is_match(&text))
            || self.regexes.as_ref().is_some_and(|set| set.is_match(&text))
    }
}

/// `MAX_PATTERN_CHARS`, for the settings page's help text.
pub const MAX_IGNORE_PATTERN_CHARS: usize = MAX_PATTERN_CHARS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Author, Fragment, Timestamp};

    fn chat(login: &str, text: &str) -> Message {
        Message::chat(
            "id",
            Timestamp(0),
            Author::new(login, login),
            vec![Fragment::Text(text.into())],
        )
    }

    fn phrase(pattern: &str) -> IgnoreRule {
        IgnoreRule {
            pattern: pattern.into(),
            ..IgnoreRule::default()
        }
    }

    fn regex(pattern: &str) -> IgnoreRule {
        IgnoreRule {
            kind: RuleKind::Regex,
            pattern: pattern.into(),
            ..IgnoreRule::default()
        }
    }

    fn engine(config: &IgnoreConfig) -> Ignores {
        let (engine, problems) = Ignores::compile(config);
        assert!(problems.is_empty(), "{problems:?}");
        engine
    }

    #[test]
    fn nothing_is_ignored_by_default() {
        let engine = engine(&IgnoreConfig::default());
        assert!(engine.is_empty());
        assert!(!engine.is_ignored(&chat("anyone", "anything")));
    }

    #[test]
    fn users_are_ignored_by_login_ignoring_case_and_at() {
        let engine = engine(&IgnoreConfig {
            users: vec!["@Spammer".into(), " other ".into()],
            ..IgnoreConfig::default()
        });
        assert!(engine.is_ignored(&chat("spammer", "hi")));
        assert!(engine.is_ignored(&chat("SPAMMER", "hi")));
        assert!(engine.is_ignored(&chat("other", "hi")));
        assert!(!engine.is_ignored(&chat("friend", "hi")));
        let mut action = chat("spammer", "waves");
        action.kind = MessageKind::Action;
        assert!(engine.is_ignored(&action));
    }

    #[test]
    fn phrases_match_whole_words_unless_told_otherwise() {
        let engine = engine(&IgnoreConfig {
            rules: vec![
                phrase("buy followers"),
                IgnoreRule {
                    whole_word: false,
                    ..phrase("spoiler")
                },
            ],
            ..IgnoreConfig::default()
        });
        assert!(engine.is_ignored(&chat("a", "cheap BUY FOLLOWERS here")));
        assert!(!engine.is_ignored(&chat("a", "buy followersX")));
        assert!(engine.is_ignored(&chat("a", "nospoilers")));
        assert!(!engine.is_ignored(&chat("a", "hello")));
    }

    #[test]
    fn regexes_run_ignoring_case_and_one_hit_is_enough() {
        let engine = engine(&IgnoreConfig {
            rules: vec![regex(r"^!clip\b"), regex(r"\d{4,}")],
            ..IgnoreConfig::default()
        });
        assert!(engine.is_ignored(&chat("a", "!CLIP this")));
        assert!(engine.is_ignored(&chat("a", "call 123456")));
        assert!(!engine.is_ignored(&chat("a", "clip !clip")));
    }

    #[test]
    fn emotes_count_by_their_name_like_everywhere_else() {
        use crate::emote::Emote;
        use std::sync::Arc;
        let emote = Emote::twitch("1", "Kappa");
        let message = Message::chat(
            "id",
            Timestamp(0),
            Author::new("a", "a"),
            vec![Fragment::Emote {
                emote: Arc::new(emote),
                zero_width_overlays: Vec::new(),
            }],
        );
        let engine = engine(&IgnoreConfig {
            rules: vec![phrase("kappa")],
            ..IgnoreConfig::default()
        });
        assert!(engine.is_ignored(&message));
    }

    #[test]
    fn own_messages_are_never_ignored() {
        let mut engine = engine(&IgnoreConfig {
            users: vec!["me".into()],
            rules: vec![phrase("hello")],
            ..IgnoreConfig::default()
        });
        engine.set_own(Some("Me"));
        assert!(!engine.is_ignored(&chat("me", "hello")));
        assert!(engine.is_ignored(&chat("you", "hello")));
        engine.set_own(None);
        assert!(engine.is_ignored(&chat("me", "x")));
    }

    #[test]
    fn events_follow_their_user_but_not_phrases_and_notices_are_never_ignored() {
        let engine = engine(&IgnoreConfig {
            users: vec!["gifter".into()],
            rules: vec![phrase("gg")],
            ..IgnoreConfig::default()
        });
        let mut event = chat("raider", "gg");
        event.kind = MessageKind::UserNotice;
        assert!(
            !engine.is_ignored(&event),
            "an announcement saying gg stays"
        );
        let mut event = chat("gifter", "");
        event.kind = MessageKind::UserNotice;
        assert!(engine.is_ignored(&event));
        let mut notice = chat("gifter", "gg");
        notice.kind = MessageKind::Notice;
        notice.author = None;
        assert!(!engine.is_ignored(&notice));
        let mut system = chat("gifter", "gg");
        system.kind = MessageKind::System;
        assert!(!engine.is_ignored(&system));
    }

    #[test]
    fn the_block_list_ignores_by_id_or_login() {
        let mut engine = Ignores::default();
        engine.set_blocked([("42".to_owned(), "Blocked".to_owned())]);
        assert!(!engine.is_empty());
        assert!(engine.is_ignored(&chat("blocked", "hi")));
        let mut renamed = chat("new_name", "hi");
        renamed.author.as_mut().unwrap().user_id = Some("42".into());
        assert!(
            engine.is_ignored(&renamed),
            "the account is blocked, not the name"
        );
        assert!(engine.is_blocked("BLOCKED"));
        engine.set_blocked(Vec::new());
        assert!(engine.is_empty());
        assert!(!engine.is_ignored(&chat("blocked", "hi")));
    }

    #[test]
    fn bad_entries_are_reported_and_the_rest_still_work() {
        let config = IgnoreConfig {
            users: vec!["fine".into(), "not a login".into()],
            rules: vec![phrase("ok"), regex("("), regex(""), regex("a*")],
            ..IgnoreConfig::default()
        };
        let (engine, problems) = Ignores::compile(&config);
        let shown: Vec<String> = problems.iter().map(ToString::to_string).collect();
        assert_eq!(shown.len(), 4, "{shown:?}");
        assert_eq!(shown[0], "rule 2: `(`: unclosed group");
        assert!(shown[3].starts_with("user 2: "), "{shown:?}");
        assert!(engine.is_ignored(&chat("a", "ok")));
        assert!(engine.is_ignored(&chat("fine", "x")));
        assert!(config.first_problem().is_some());
        assert!(IgnoreConfig::default().first_problem().is_none());
    }

    #[test]
    fn limits_follow_the_highlight_rules() {
        let long = "a".repeat(MAX_IGNORE_PATTERN_CHARS + 1);
        assert!(
            ignore_rule_problem(&phrase(&long))
                .unwrap()
                .contains("longer than 500")
        );
        let too_many = IgnoreConfig {
            rules: (0..=MAX_RULES).map(|i| phrase(&format!("p{i}"))).collect(),
            ..IgnoreConfig::default()
        };
        assert!(
            too_many
                .problems()
                .iter()
                .any(|p| p.to_string().contains("more than"))
        );
        assert!(ignore_user_problem("with space").is_some());
        assert!(ignore_user_problem("@Fine_1").is_none());
    }

    #[test]
    fn ignoring_a_user_is_found_in_the_config() {
        let config = IgnoreConfig {
            users: vec!["Alice".into()],
            ..IgnoreConfig::default()
        };
        assert!(config.ignores_user("@alice"));
        assert!(!config.ignores_user("bob"));
    }

    #[test]
    fn a_pathological_regex_is_still_fast() {
        // Nested quantifiers would take forever in a backtracking engine.
        let engine = engine(&IgnoreConfig {
            rules: vec![regex(r"(a+)+$")],
            ..IgnoreConfig::default()
        });
        let text = format!("{}b", "a".repeat(50_000));
        let started = std::time::Instant::now();
        assert!(!engine.is_ignored(&chat("x", &text)));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
