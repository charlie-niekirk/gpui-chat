//! The Ignores page of the settings window (PR 5.7): ignored users, phrases and regular expressions,
//! how ignored messages are shown, the Twitch block list, and the bots *Hide bots* hides.
//!
//! Lists are one entry per line, like the Highlights page. A phrase line is the text, optionally
//! preceded by `{anywhere}` (match inside words too):
//!
//! ```text
//! buy followers
//! {anywhere} spoiler
//! ```
//!
//! Everything is written to `[ignores]` / `[filters]` through `config::edit`, which refuses a change
//! that would leave an invalid file; the field then shows the reason under it. Parsing and printing a
//! line are plain functions with tests.

use std::rc::Rc;

use gpui_chat_core::{
    IgnoreRule, RuleKind, highlight::normalise_user, ignore_rule_problem, ignore_user_problem,
};
use gpui_chat_settings::{EditValue, SettingEdit};
use gpui_kit::component::button::Button;
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{App, FontWeight, IntoElement, ParentElement as _, Styled as _, WeakEntity, div};

use super::fields::{self, Reset, Size};
use super::{SettingsView, bool_writer, current, friendly};
use crate::account;
use crate::config;
use crate::ignores::{self, BlockStatus};
use crate::theme::Tokens;

/// Width of the list fields.
const FIELD: Size = Size {
    width: 220.0,
    rows: 4,
};

/// A phrase line as a rule: the text, with `{anywhere}` in front for a match inside words.
fn parse_rule(line: &str, kind: RuleKind) -> Result<IgnoreRule, String> {
    let line = line.trim();
    let (anywhere, text) = match line.strip_prefix('{') {
        Some(rest) => {
            let (inside, text) = rest
                .split_once('}')
                .ok_or_else(|| "the options are missing their closing }".to_owned())?;
            let mut anywhere = false;
            for word in inside.split(',').map(str::trim).filter(|w| !w.is_empty()) {
                if word.eq_ignore_ascii_case("anywhere") {
                    anywhere = true;
                } else {
                    return Err(format!(
                        "unknown option “{word}” (the only option is anywhere)"
                    ));
                }
            }
            let text = text.trim();
            if text.is_empty() {
                return Err("nothing after the options".to_owned());
            }
            (anywhere, text)
        }
        None => (false, line),
    };
    if anywhere && kind == RuleKind::Regex {
        return Err("anywhere is for phrases; a regular expression is unanchored already".into());
    }
    Ok(IgnoreRule {
        kind,
        pattern: text.to_owned(),
        whole_word: !anywhere,
    })
}

/// A line as a rule that compiles, so a bad regular expression is reported on its own line.
fn checked_rule(line: &str, kind: RuleKind) -> Result<IgnoreRule, String> {
    let rule = parse_rule(line, kind)?;
    match ignore_rule_problem(&rule) {
        Some(problem) => Err(problem),
        None => Ok(rule),
    }
}

fn format_rule(rule: &IgnoreRule) -> String {
    let anywhere = rule.kind == RuleKind::Phrase && !rule.whole_word;
    // A text that starts with `{` would read as options: an empty pair says it is not.
    if anywhere {
        format!("{{anywhere}} {}", rule.pattern)
    } else if rule.pattern.starts_with('{') {
        format!("{{}} {}", rule.pattern)
    } else {
        rule.pattern.clone()
    }
}

/// A login line, checked: lowercase, no `@`.
fn checked_login(line: &str) -> Result<String, String> {
    let login = normalise_user(line);
    match ignore_user_problem(&login) {
        Some(problem) => Err(problem),
        None => Ok(login),
    }
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

fn rule_row(rule: &IgnoreRule) -> Vec<(&'static str, EditValue)> {
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
    row
}

/// The rules with the `kind` lines replaced by `new` (phrases are listed before regular
/// expressions, the order the page shows them in).
fn replace_kind(all: &[IgnoreRule], kind: RuleKind, new: Vec<IgnoreRule>) -> Vec<IgnoreRule> {
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

fn write_rules(rules: &[IgnoreRule], cx: &mut App) -> Result<(), String> {
    config::edit(
        &[SettingEdit::set(
            "ignores.rules",
            EditValue::Tables(rules.iter().map(rule_row).collect()),
        )],
        cx,
    )
    .map_err(|error| friendly(&error))
}

/// The lists have no reset of their own: the page-level button would clear phrases *and* regular
/// expressions, and the file is there for that.
fn no_reset() -> Reset {
    Reset {
        field: "ignores.rules",
        is_dirty: Rc::new(|_| false),
    }
}

pub fn page(this: &WeakEntity<SettingsView>) -> SettingPage {
    let users = SettingGroup::new()
        .title("Users and phrases")
        .description(
            "Ignored messages are hidden everywhere: the chat, the Mentions view, the unread and \
             mention dots and notifications. Your own messages are never ignored. Ignoring takes \
             effect at once, on what is already on screen too. A regular expression that does not \
             compile is refused here and the line is named.",
        )
        .items([
            SettingItem::new(
                "Ignored users",
                fields::lines_sized(
                    "ignore-users",
                    "Ignored users",
                    "some_spammer\nanother_bot",
                    FIELD,
                    |cx| current(cx).ignores.users.clone(),
                    Rc::new(|lines: Vec<String>, cx: &mut App| {
                        let users = parse_all(&lines, checked_login)?;
                        config::edit(
                            &[SettingEdit::set("ignores.users", EditValue::StrList(users))],
                            cx,
                        )
                        .map_err(|error| friendly(&error))
                    }),
                    no_reset(),
                ),
            )
            .description("Logins, one per line (also in the message menu and on the user card)."),
            SettingItem::new(
                "Ignored phrases",
                fields::lines_sized(
                    "ignore-phrases",
                    "Ignored phrases",
                    "buy followers\n{anywhere} spoiler",
                    FIELD,
                    |cx| {
                        current(cx)
                            .ignores
                            .rules
                            .iter()
                            .filter(|rule| rule.kind == RuleKind::Phrase)
                            .map(format_rule)
                            .collect()
                    },
                    Rc::new(|lines: Vec<String>, cx: &mut App| {
                        let new = parse_all(&lines, |line| checked_rule(line, RuleKind::Phrase))?;
                        let all = current(cx).ignores.rules.clone();
                        write_rules(&replace_kind(&all, RuleKind::Phrase, new), cx)
                    }),
                    no_reset(),
                ),
            )
            .description("Plain text, ignoring case; whole words unless {anywhere}."),
            SettingItem::new(
                "Ignored regular expressions",
                fields::lines_sized(
                    "ignore-regexes",
                    "Ignored regular expressions",
                    r"^!\w+",
                    FIELD,
                    |cx| {
                        current(cx)
                            .ignores
                            .rules
                            .iter()
                            .filter(|rule| rule.kind == RuleKind::Regex)
                            .map(format_rule)
                            .collect()
                    },
                    Rc::new(|lines: Vec<String>, cx: &mut App| {
                        let new = parse_all(&lines, |line| checked_rule(line, RuleKind::Regex))?;
                        let all = current(cx).ignores.rules.clone();
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
                "Show ignored messages collapsed",
                SettingField::switch(
                    |cx| current(cx).ignores.show_collapsed,
                    bool_writer(this, "ignores.show_collapsed"),
                )
                .default_value(false),
            )
            .description(
                "Instead of nothing, an ignored message is one line, “1 ignored message”, that a \
                 click opens. A chat with a filter on shows no placeholders.",
            ),
        ]);

    let blocks = SettingGroup::new()
        .title("Twitch block list")
        .description(
            "The users you blocked on Twitch can be ignored here too. Reading the list and \
             blocking from the message menu need extra Twitch permissions, asked for only if you \
             enable them.",
        )
        .items([
            SettingItem::new(
                "Use my Twitch block list",
                SettingField::switch(
                    |cx| current(cx).ignores.sync_block_list,
                    bool_writer(this, "ignores.sync_block_list"),
                )
                .default_value(true),
            )
            .description("Blocked users are ignored like the ones listed above."),
            SettingItem::render(|_, _, cx: &mut App| block_row(cx)),
        ]);

    let bots = SettingGroup::new().title("Bots").items([SettingItem::new(
        "Bots to hide",
        fields::lines_sized(
            "filter-bots",
            "Bots to hide",
            "nightbot\nstreamelements",
            FIELD,
            |cx| current(cx).filters.bots.clone(),
            Rc::new(|lines: Vec<String>, cx: &mut App| {
                let bots = parse_all(&lines, checked_login)?;
                config::edit(
                    &[SettingEdit::set("filters.bots", EditValue::StrList(bots))],
                    cx,
                )
                .map_err(|error| friendly(&error))
            }),
            Reset {
                field: "filters.bots",
                is_dirty: Rc::new(|cx| {
                    current(cx).filters.bots != gpui_chat_core::FilterConfig::default().bots
                }),
            },
        ),
    )
    .description(
        "The accounts the *No bots* filter of the find bar hides. Logins, one per line; the \
         reset button brings back the usual chat bots.",
    )]);

    SettingPage::new("Ignores")
        .icon(Icon::new(IconName::Ban))
        .groups([users, blocks, bots])
}

/// Where the Twitch block list stands, with the button that enables it.
fn block_row(cx: &App) -> impl IntoElement + use<> {
    let tokens = *cx.global::<Tokens>();
    let status = ignores::current(cx).map(|hub| hub.read(cx).status().clone());
    let refresh = matches!(status, Some(BlockStatus::Ready(_) | BlockStatus::Failed(_)));
    let (text, enable) = match status {
        None => (
            "Not available in this run (offline modes never use an account).".to_owned(),
            false,
        ),
        Some(status) => {
            let enable = matches!(status, BlockStatus::NeedsScope)
                && account::current(cx).is_some_and(|hub| {
                    hub.read(cx)
                        .state()
                        .account()
                        .is_some_and(|account| !account.block_list_enabled())
                });
            (status.describe(), enable)
        }
    };
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            v_flex().flex_1().min_w_0().child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.text_muted)
                    .child(text),
            ),
        )
        .children(refresh.then(|| {
            Button::new("settings-refresh-block-list")
                .small()
                .label("Refresh")
                .on_click(|_, _, cx| {
                    if let Some(hub) = ignores::current(cx) {
                        hub.update(cx, |hub, cx| hub.refresh(cx));
                    }
                })
        }))
        .children(enable.then(|| {
            Button::new("settings-enable-block-list")
                .small()
                .label("Enable block list sync…")
                .on_click(|_, window, cx| {
                    if let Some(hub) = account::current(cx) {
                        crate::sign_in::open_for_block_list(hub, window, cx);
                    }
                })
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phrase(text: &str) -> IgnoreRule {
        parse_rule(text, RuleKind::Phrase).unwrap()
    }

    #[test]
    fn a_bare_line_is_a_whole_word_phrase() {
        assert_eq!(
            phrase("buy followers"),
            IgnoreRule {
                kind: RuleKind::Phrase,
                pattern: "buy followers".into(),
                whole_word: true
            }
        );
        let anywhere = phrase("{Anywhere} spoiler");
        assert_eq!(
            (anywhere.pattern.as_str(), anywhere.whole_word),
            ("spoiler", false)
        );
        assert_eq!(phrase("{} {literal}").pattern, "{literal}");
    }

    #[test]
    fn a_line_prints_the_way_it_was_typed_and_round_trips() {
        for line in ["buy followers", "{anywhere} spoiler", "{} {literal text}"] {
            let rule = phrase(line);
            assert_eq!(
                parse_rule(&format_rule(&rule), RuleKind::Phrase).unwrap(),
                rule,
                "{line}"
            );
        }
        assert_eq!(format_rule(&phrase("plain")), "plain");
    }

    #[test]
    fn a_bad_option_or_brace_is_explained() {
        assert!(
            parse_rule("{green} x", RuleKind::Phrase)
                .unwrap_err()
                .contains("green")
        );
        assert!(
            parse_rule("{anywhere x", RuleKind::Phrase)
                .unwrap_err()
                .contains("closing")
        );
        assert!(
            parse_rule("{anywhere}", RuleKind::Phrase)
                .unwrap_err()
                .contains("nothing")
        );
        assert!(
            parse_rule("{anywhere} a.*", RuleKind::Regex)
                .unwrap_err()
                .contains("phrases")
        );
    }

    #[test]
    fn a_bad_regex_is_reported_on_its_own_line_of_its_own_field() {
        let lines = vec![r"\bgg\b".to_owned(), "(".to_owned()];
        let error = parse_all(&lines, |l| checked_rule(l, RuleKind::Regex)).unwrap_err();
        assert_eq!(error, "Line 2: `(`: unclosed group");
        let error =
            parse_all(&["a".repeat(501)], |l| checked_rule(l, RuleKind::Phrase)).unwrap_err();
        assert!(error.contains("longer than 500 characters"), "{error}");
        let error = parse_all(&["^".to_owned()], |l| checked_rule(l, RuleKind::Regex)).unwrap_err();
        assert!(error.contains("matches every message"), "{error}");
    }

    #[test]
    fn logins_are_normalised_and_checked() {
        assert_eq!(checked_login("@Some_One").unwrap(), "some_one");
        assert!(checked_login("not a login").is_err());
        let error =
            parse_all(&["fine".to_owned(), "no spaces".to_owned()], checked_login).unwrap_err();
        assert!(error.starts_with("Line 2: "), "{error}");
    }

    #[test]
    fn editing_one_kind_keeps_the_other_and_lists_phrases_first() {
        let regex = parse_rule("^!clip", RuleKind::Regex).unwrap();
        let old = vec![regex, phrase("old")];
        let new = replace_kind(&old, RuleKind::Phrase, vec![phrase("new"), phrase("newer")]);
        assert_eq!(
            new.iter().map(|r| r.pattern.as_str()).collect::<Vec<_>>(),
            ["new", "newer", "^!clip"]
        );
        assert_eq!(replace_kind(&new, RuleKind::Regex, Vec::new()).len(), 2);
    }

    #[test]
    fn only_non_default_values_are_written() {
        assert_eq!(rule_row(&phrase("x")).len(), 2);
        let keys: Vec<_> = rule_row(&phrase("{anywhere} x"))
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(keys, ["kind", "pattern", "whole_word"]);
    }
}
