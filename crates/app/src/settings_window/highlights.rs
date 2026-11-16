//! The Highlights page of the settings window (PR 5.3): mentions, notifications, and the lists of
//! phrases, regular expressions and users.
//!
//! The lists are one entry per line (the same kind of field as Streamlink's extra arguments). A line
//! is the pattern, optionally preceded by options in braces:
//!
//! ```text
//! nice play
//! {green, notify} !giveaway
//! {anywhere} gg
//! ```
//!
//! Options are a colour (`accent`, `red`, `green`, `yellow`), `notify`, and for phrases `anywhere`
//! (match inside words too). Everything is written to `[[highlights.rules]]` /
//! `[[highlights.users]]` through `config::edit`, which refuses the whole change, naming the rule,
//! when a regular expression does not compile; the field then shows the reason under it.
//! Parsing and printing a line are plain functions with tests.

use std::rc::Rc;

use gpui_chat_core::{
    HighlightColor, HighlightRule, HighlightUser, RuleKind, rule_problem, user_problem,
};
use gpui_chat_settings::{EditValue, SettingEdit};
use gpui_kit::component::Icon;
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::{App, SharedString, WeakEntity};

use super::fields::{self, Reset, Size};
use super::{SettingsView, bool_writer, current, friendly};
use crate::config;

/// Width of the rule fields: patterns are longer than program arguments.
const FIELD: Size = Size {
    width: 220.0,
    rows: 4,
};

/// What the options of a line say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Options {
    color: HighlightColor,
    notify: bool,
    anywhere: bool,
}

fn parse_options(text: &str) -> Result<Options, String> {
    let mut options = Options {
        color: HighlightColor::Accent,
        notify: false,
        anywhere: false,
    };
    for word in text.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        if let Some(color) = HighlightColor::from_id(word) {
            options.color = color;
        } else if word.eq_ignore_ascii_case("notify") {
            options.notify = true;
        } else if word.eq_ignore_ascii_case("anywhere") {
            options.anywhere = true;
        } else {
            return Err(format!(
                "unknown option “{word}” (use accent, red, green, yellow, notify or anywhere)"
            ));
        }
    }
    Ok(options)
}

/// A line as its options and the text after them.
fn split_line(line: &str) -> Result<(Options, &str), String> {
    let line = line.trim();
    let Some(rest) = line.strip_prefix('{') else {
        return Ok((parse_options("")?, line));
    };
    let (inside, text) = rest
        .split_once('}')
        .ok_or_else(|| "the options are missing their closing }".to_owned())?;
    let text = text.trim();
    if text.is_empty() {
        return Err("nothing after the options".to_owned());
    }
    Ok((parse_options(inside)?, text))
}

fn parse_rule(line: &str, kind: RuleKind) -> Result<HighlightRule, String> {
    let (options, pattern) = split_line(line)?;
    if options.anywhere && kind == RuleKind::Regex {
        return Err("anywhere is for phrases; a regular expression is unanchored already".into());
    }
    Ok(HighlightRule {
        kind,
        pattern: pattern.to_owned(),
        whole_word: !options.anywhere,
        color: options.color,
        notify: options.notify,
    })
}

/// A line as a rule the engine can use: parsed, and compiled once to see that it works, so a bad
/// regular expression is reported on its own line of its own field (the file-level check numbers
/// phrases and regular expressions together).
fn checked_rule(line: &str, kind: RuleKind) -> Result<HighlightRule, String> {
    let rule = parse_rule(line, kind)?;
    match rule_problem(&rule) {
        Some(problem) => Err(problem),
        None => Ok(rule),
    }
}

fn checked_user(line: &str) -> Result<HighlightUser, String> {
    let user = parse_user(line)?;
    match user_problem(&user) {
        Some(problem) => Err(problem),
        None => Ok(user),
    }
}

fn parse_user(line: &str) -> Result<HighlightUser, String> {
    let (options, login) = split_line(line)?;
    if options.anywhere {
        return Err("anywhere is for phrases".into());
    }
    Ok(HighlightUser {
        login: login.to_owned(),
        color: options.color,
        notify: options.notify,
    })
}

/// The options of a line, in braces, or nothing when they are all the defaults.
fn options_prefix(color: HighlightColor, notify: bool, anywhere: bool, text: &str) -> String {
    let mut words = Vec::new();
    if color != HighlightColor::Accent {
        words.push(color.id());
    }
    if notify {
        words.push("notify");
    }
    if anywhere {
        words.push("anywhere");
    }
    // A text that starts with `{` would read as options: an empty pair says it is not.
    if words.is_empty() && !text.starts_with('{') {
        return String::new();
    }
    format!("{{{}}} ", words.join(", "))
}

fn format_rule(rule: &HighlightRule) -> String {
    let anywhere = rule.kind == RuleKind::Phrase && !rule.whole_word;
    format!(
        "{}{}",
        options_prefix(rule.color, rule.notify, anywhere, &rule.pattern),
        rule.pattern
    )
}

fn format_user(user: &HighlightUser) -> String {
    format!(
        "{}{}",
        options_prefix(user.color, user.notify, false, &user.login),
        user.login
    )
}

/// Parses every line; the first bad one is reported with its number.
fn parse_all<T>(
    lines: &[String],
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| parse(line).map_err(|reason| format!("Line {}: {reason}", i + 1)))
        .collect()
}

fn rule_row(rule: &HighlightRule) -> Vec<(&'static str, EditValue)> {
    let mut row = vec![
        (
            "kind",
            EditValue::Str(
                match rule.kind {
                    RuleKind::Phrase => "phrase",
                    RuleKind::Regex => "regex",
                }
                .into(),
            ),
        ),
        ("pattern", EditValue::Str(rule.pattern.clone())),
    ];
    if rule.kind == RuleKind::Phrase && !rule.whole_word {
        row.push(("whole_word", EditValue::Bool(false)));
    }
    if rule.color != HighlightColor::Accent {
        row.push(("color", EditValue::Str(rule.color.id().into())));
    }
    if rule.notify {
        row.push(("notify", EditValue::Bool(true)));
    }
    row
}

fn user_row(user: &HighlightUser) -> Vec<(&'static str, EditValue)> {
    let mut row = vec![("login", EditValue::Str(user.login.clone()))];
    if user.color != HighlightColor::Accent {
        row.push(("color", EditValue::Str(user.color.id().into())));
    }
    if user.notify {
        row.push(("notify", EditValue::Bool(true)));
    }
    row
}

/// The rules with the `kind` lines replaced by `new` (phrases are listed before regular
/// expressions, the order the page shows them in).
fn replace_kind(
    all: &[HighlightRule],
    kind: RuleKind,
    new: Vec<HighlightRule>,
) -> Vec<HighlightRule> {
    let (mut phrases, mut regexes): (Vec<_>, Vec<_>) = all
        .iter()
        .cloned()
        .partition(|rule| rule.kind == RuleKind::Phrase);
    match kind {
        RuleKind::Phrase => phrases = new,
        RuleKind::Regex => regexes = new,
    }
    phrases.extend(regexes);
    phrases
}

fn write_rules(rules: &[HighlightRule], cx: &mut App) -> Result<(), String> {
    config::edit(
        &[SettingEdit::set(
            "highlights.rules",
            EditValue::Tables(rules.iter().map(rule_row).collect()),
        )],
        cx,
    )
    .map_err(|error| friendly(&error))
}

/// The page's own reset: none (the page-level button would clear phrases *and* regular
/// expressions, and users have the file for that).
fn no_reset() -> Reset {
    Reset {
        field: "highlights.rules",
        is_dirty: Rc::new(|_| false),
    }
}

fn color_dropdown(
    this: &WeakEntity<SettingsView>,
    field: &'static str,
    get: fn(&gpui_chat_settings::Settings) -> HighlightColor,
) -> SettingField<SharedString> {
    let this = this.clone();
    SettingField::dropdown(
        HighlightColor::ALL
            .iter()
            .map(|color| (color.id().into(), color.label().into()))
            .collect(),
        move |cx| get(current(cx)).id().into(),
        move |value: SharedString, cx| {
            SettingsView::write(
                &this,
                vec![SettingEdit::set(field, EditValue::Str(value.to_string()))],
                cx,
            );
        },
    )
    .default_value("accent")
}

pub fn page(this: &WeakEntity<SettingsView>) -> SettingPage {
    let mentions = SettingGroup::new().title("Mentions").items([
        SettingItem::new(
            "Highlight my name",
            SettingField::switch(
                |cx| current(cx).highlights.mentions,
                bool_writer(this, "highlights.mentions"),
            )
            .default_value(true),
        )
        .description(
            "Messages that say your login or display name (as a whole word, in any case, with or \
             without @) get a coloured row and count as mentions. Your own messages never do.",
        ),
        SettingItem::new(
            "Mention colour",
            color_dropdown(this, "highlights.mention_color", |s| {
                s.highlights.mention_color
            }),
        )
        .description("One of the theme's own colours, so it suits every theme."),
        SettingItem::new(
            "Notify for mentions",
            SettingField::switch(
                |cx| current(cx).highlights.mention_notify,
                bool_writer(this, "highlights.mention_notify"),
            )
            .default_value(true),
        )
        .description("Raise a system notification for a mention."),
    ]);

    let notifications = SettingGroup::new()
        .title("Notifications")
        .items([SettingItem::new(
            "System notifications",
            SettingField::switch(
                |cx| current(cx).highlights.notifications,
                bool_writer(this, "highlights.notifications"),
            )
            .default_value(true),
        )
        .description(
            "Notify for a highlight that asks for it while the window is in the background or \
             the channel is not on screen: at most one every few seconds, the rest as one \
             summary. macOS asks for permission the first time. Only the packaged app can \
             notify, not `cargo run`.",
        )]);

    let rules = SettingGroup::new()
        .title("Custom highlights")
        .description(
            "One entry per line, applied to messages as they arrive. Options go in braces before \
             the text: a colour (accent, red, green or yellow), notify, and for phrases anywhere \
             (also match inside words), as in {green, notify} nice play. History, your own \
             messages and removed messages never notify. A regular expression that does not \
             compile is refused here and the rule is named.",
        )
        .items([
            SettingItem::new(
                "Phrases",
                fields::lines_sized(
                    "highlight-phrases",
                    "Phrases",
                    "nice play\n{green, notify} !giveaway\n{anywhere} gg",
                    FIELD,
                    |cx| {
                        current(cx)
                            .highlights
                            .rules
                            .iter()
                            .filter(|rule| rule.kind == RuleKind::Phrase)
                            .map(format_rule)
                            .collect()
                    },
                    Rc::new(|lines: Vec<String>, cx: &mut App| {
                        let new = parse_all(&lines, |line| checked_rule(line, RuleKind::Phrase))?;
                        let all = current(cx).highlights.rules.clone();
                        write_rules(&replace_kind(&all, RuleKind::Phrase, new), cx)
                    }),
                    no_reset(),
                ),
            )
            .description("Plain text; whole words unless {anywhere}."),
            SettingItem::new(
                "Regular expressions",
                fields::lines_sized(
                    "highlight-regexes",
                    "Regular expressions",
                    r"\bgg( wp)?\b",
                    FIELD,
                    |cx| {
                        current(cx)
                            .highlights
                            .rules
                            .iter()
                            .filter(|rule| rule.kind == RuleKind::Regex)
                            .map(format_rule)
                            .collect()
                    },
                    Rc::new(|lines: Vec<String>, cx: &mut App| {
                        let new = parse_all(&lines, |line| checked_rule(line, RuleKind::Regex))?;
                        let all = current(cx).highlights.rules.clone();
                        write_rules(&replace_kind(&all, RuleKind::Regex, new), cx)
                    }),
                    no_reset(),
                ),
            )
            .description(
                "Rust regex syntax, ignoring case unless the pattern says (?-i). Always fast to \
                 search, whatever the pattern.",
            ),
            SettingItem::new(
                "Highlighted users",
                fields::lines_sized(
                    "highlight-users",
                    "Highlighted users",
                    "streamer\n{green, notify} a_friend",
                    FIELD,
                    |cx| {
                        current(cx)
                            .highlights
                            .users
                            .iter()
                            .map(format_user)
                            .collect()
                    },
                    Rc::new(|lines: Vec<String>, cx: &mut App| {
                        let users = parse_all(&lines, checked_user)?;
                        config::edit(
                            &[SettingEdit::set(
                                "highlights.users",
                                EditValue::Tables(users.iter().map(user_row).collect()),
                            )],
                            cx,
                        )
                        .map_err(|error| friendly(&error))
                    }),
                    no_reset(),
                ),
            )
            .description("Logins; everything they say is highlighted."),
        ]);

    SettingPage::new("Highlights")
        .icon(Icon::new(gpui_kit::assets::IconName::Bell))
        .groups([mentions, notifications, rules])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phrase(text: &str) -> HighlightRule {
        parse_rule(text, RuleKind::Phrase).unwrap()
    }

    #[test]
    fn a_bare_line_is_a_phrase_with_the_defaults() {
        assert_eq!(
            phrase("nice play"),
            HighlightRule {
                kind: RuleKind::Phrase,
                pattern: "nice play".into(),
                whole_word: true,
                color: HighlightColor::Accent,
                notify: false,
            }
        );
    }

    #[test]
    fn options_in_braces_set_colour_notify_and_anywhere() {
        let rule = phrase("{Green, notify, anywhere} gg");
        assert_eq!(
            (
                rule.color,
                rule.notify,
                rule.whole_word,
                rule.pattern.as_str()
            ),
            (HighlightColor::Green, true, false, "gg")
        );
        assert_eq!(phrase("{} {literal}").pattern, "{literal}");
        assert_eq!(phrase("{red}x").pattern, "x");
    }

    #[test]
    fn a_line_prints_the_way_it_was_typed_and_round_trips() {
        for line in [
            "nice play",
            "{green, notify} !giveaway",
            "{red} a",
            "{notify, anywhere} gg",
            "{} {literal text}",
        ] {
            let rule = phrase(line);
            let printed = format_rule(&rule);
            assert_eq!(
                parse_rule(&printed, RuleKind::Phrase).unwrap(),
                rule,
                "{line}"
            );
        }
        assert_eq!(
            format_rule(&phrase("{notify, green} x")),
            "{green, notify} x"
        );
        assert_eq!(format_rule(&phrase("plain")), "plain");
        let regex = parse_rule(r"{yellow} \bgg\b", RuleKind::Regex).unwrap();
        assert_eq!(format_rule(&regex), r"{yellow} \bgg\b");
    }

    #[test]
    fn a_bad_option_or_brace_is_explained() {
        assert!(
            parse_rule("{gren} x", RuleKind::Phrase)
                .unwrap_err()
                .contains("gren")
        );
        assert!(
            parse_rule("{green x", RuleKind::Phrase)
                .unwrap_err()
                .contains("closing")
        );
        assert!(
            parse_rule("{green}", RuleKind::Phrase)
                .unwrap_err()
                .contains("nothing")
        );
        assert!(
            parse_rule("{anywhere} a.*", RuleKind::Regex)
                .unwrap_err()
                .contains("phrases")
        );
        assert!(parse_user("{anywhere} bob").is_err());
    }

    #[test]
    fn the_first_bad_line_is_named() {
        let lines = vec!["fine".to_owned(), "{nope} x".to_owned()];
        let error = parse_all(&lines, |l| parse_rule(l, RuleKind::Phrase)).unwrap_err();
        assert!(error.starts_with("Line 2: "), "{error}");
    }

    #[test]
    fn a_bad_regex_is_reported_on_its_own_line_of_its_own_field() {
        // The regular expressions field has two lines; the second is bad. Whatever phrases the
        // file holds, it is "Line 2" of this field.
        let lines = vec![r"\bgg\b".to_owned(), "(".to_owned()];
        let error = parse_all(&lines, |l| checked_rule(l, RuleKind::Regex)).unwrap_err();
        assert_eq!(error, "Line 2: `(`: unclosed group");
        let error = parse_all(&["{red} bad user".to_owned()], checked_user).unwrap_err();
        assert!(error.starts_with("Line 1: "), "{error}");
        let error =
            parse_all(&["a".repeat(501)], |l| checked_rule(l, RuleKind::Phrase)).unwrap_err();
        assert!(error.contains("longer than 500 characters"), "{error}");
    }

    #[test]
    fn users_round_trip() {
        let user = parse_user("{green, notify} Alice").unwrap();
        assert_eq!(
            user,
            HighlightUser {
                login: "Alice".into(),
                color: HighlightColor::Green,
                notify: true
            }
        );
        assert_eq!(format_user(&user), "{green, notify} Alice");
    }

    #[test]
    fn editing_one_kind_keeps_the_other_and_lists_phrases_first() {
        let regex = parse_rule("^!clip", RuleKind::Regex).unwrap();
        let old = vec![regex.clone(), phrase("old")];
        let new = replace_kind(&old, RuleKind::Phrase, vec![phrase("new"), phrase("newer")]);
        assert_eq!(
            new.iter().map(|r| r.pattern.as_str()).collect::<Vec<_>>(),
            ["new", "newer", "^!clip"]
        );
        let cleared = replace_kind(&new, RuleKind::Regex, Vec::new());
        assert_eq!(cleared.len(), 2);
    }

    #[test]
    fn only_non_default_values_are_written() {
        let row = rule_row(&phrase("x"));
        assert_eq!(row.len(), 2);
        let row = rule_row(&phrase("{red, notify, anywhere} x"));
        let keys: Vec<_> = row.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["kind", "pattern", "whole_word", "color", "notify"]);
        assert_eq!(user_row(&parse_user("bob").unwrap()).len(), 1);
    }
}
