//! `keymap.toml`: the user's key bindings, parsed and checked without GPUI.
//!
//! One line per action; the value **replaces** the action's default shortcuts:
//!
//! ```toml
//! SplitRight = "cmd-shift-r"              # rebind
//! ToggleSidebar = ["cmd-b", "cmd-\\"]     # several shortcuts
//! ClosePane = []                          # unbind ("none" works too)
//! FocusPaneLeft = { keys = "alt-left", context = "AppShell" }   # pick the key context
//! ```
//!
//! This module checks what it can know without the application: the TOML itself, that each
//! action name is one the app has (`known_actions`, with a "did you mean" for typos), that each
//! keystroke is well formed (modifiers, a key GPUI knows) and that a context is not empty or
//! unbalanced. Turning an entry into a GPUI key binding, and the checks that need GPUI (the
//! context grammar, two actions on one key), belong to the app.
//!
//! A problem with one entry skips that entry and keeps the others; a file that does not parse
//! at all keeps the previous entries ([`load_keymap`]). Every problem is returned, never only
//! logged: the app shows them in the window.

use std::fmt;
use std::path::Path;

/// One action's bindings from `keymap.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapEntry {
    /// The action's name, as the app knows it (`known_actions`, spelled the way that list does).
    pub action: String,
    /// The keystrokes, each as written (`cmd-shift-r`, or a sequence such as `cmd-k cmd-s`).
    /// Empty: the action is unbound.
    pub keys: Vec<String>,
    /// The key context the bindings apply in; `None` keeps the one the action's default
    /// shortcut has.
    pub context: Option<String>,
}

impl KeymapEntry {
    /// Whether this entry removes the action's shortcuts without adding any.
    pub fn is_unbind(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Something wrong in `keymap.toml`, in words for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapProblem {
    /// The action it is about, when there is one (`None`: the file as a whole).
    pub action: Option<String>,
    /// What is wrong, and what is done about it.
    pub message: String,
}

impl KeymapProblem {
    fn file(message: impl Into<String>) -> Self {
        Self {
            action: None,
            message: message.into(),
        }
    }

    fn entry(action: &str, message: impl Into<String>) -> Self {
        Self {
            action: Some(action.to_owned()),
            message: message.into(),
        }
    }
}

impl fmt::Display for KeymapProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.action {
            Some(action) => write!(f, "{action}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// What a reading of `keymap.toml` came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeymapReport {
    /// The entries in effect.
    pub entries: Vec<KeymapEntry>,
    /// Everything that was wrong, one line each; empty when the file is fine (or absent).
    pub problems: Vec<KeymapProblem>,
}

/// Parses the text of a `keymap.toml`. Err: the text is not TOML (message with line and
/// column); a bad entry is a [`KeymapProblem`] in the report instead.
pub fn parse_keymap(text: &str, known_actions: &[&str]) -> Result<KeymapReport, String> {
    let table: toml::Table = text.parse().map_err(|error: toml::de::Error| {
        let location = error
            .span()
            .map(|span| line_column(text, span.start))
            .map_or_else(String::new, |(line, column)| format!(":{line}:{column}"));
        format!("keymap.toml{location}: {}", error.message())
    })?;
    let mut report = KeymapReport::default();
    for (name, value) in &table {
        let Some(action) = resolve_action(name, known_actions) else {
            report.problems.push(KeymapProblem::entry(
                name,
                format!("unknown action{}", suggestion(name, known_actions)),
            ));
            continue;
        };
        match entry_from_value(action, value) {
            Ok(entry) => report.entries.push(entry),
            Err(message) => report.problems.push(KeymapProblem::entry(name, message)),
        }
    }
    Ok(report)
}

/// Reads `path`. A missing file is an empty keymap (the defaults); a file that cannot be read
/// or is not TOML keeps `previous` and says why.
pub fn load_keymap(path: &Path, known_actions: &[&str], previous: &[KeymapEntry]) -> KeymapReport {
    let keep_previous = |message: String| KeymapReport {
        entries: previous.to_vec(),
        problems: vec![KeymapProblem::file(format!(
            "{message}; keeping the previous key bindings"
        ))],
    };
    match std::fs::read_to_string(path) {
        Ok(text) => parse_keymap(&text, known_actions).unwrap_or_else(keep_previous),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => KeymapReport::default(),
        Err(error) => keep_previous(format!("could not read {}: {error}", path.display())),
    }
}

fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |last| last.chars().count())
        + 1;
    (line, column)
}

fn entry_from_value(action: &str, value: &toml::Value) -> Result<KeymapEntry, String> {
    match value {
        toml::Value::Table(table) => {
            if let Some(unknown) = table
                .keys()
                .find(|key| !matches!(key.as_str(), "keys" | "context"))
            {
                return Err(format!(
                    "unknown field `{unknown}` (a table takes `keys` and `context`)"
                ));
            }
            let keys = table
                .get("keys")
                .ok_or("a table needs `keys` (a keystroke, or a list of them; `[]` unbinds)")?;
            let context = match table.get("context") {
                None => None,
                Some(toml::Value::String(context)) => Some(check_context(context)?),
                Some(_) => return Err("`context` must be a string".to_owned()),
            };
            Ok(KeymapEntry {
                action: action.to_owned(),
                keys: keys_from_value(keys)?,
                context,
            })
        }
        other => Ok(KeymapEntry {
            action: action.to_owned(),
            keys: keys_from_value(other)?,
            context: None,
        }),
    }
}

fn keys_from_value(value: &toml::Value) -> Result<Vec<String>, String> {
    match value {
        toml::Value::String(key) if key.trim().eq_ignore_ascii_case("none") => Ok(Vec::new()),
        toml::Value::String(key) => Ok(vec![check_keystrokes(key)?]),
        toml::Value::Array(items) => items
            .iter()
            .map(|item| match item {
                toml::Value::String(key) => check_keystrokes(key),
                _ => Err("keystrokes are strings".to_owned()),
            })
            .collect(),
        _ => Err(
            "expected a keystroke (\"cmd-k\"), a list of them, `[]` to unbind, or a table with `keys`"
                .to_owned(),
        ),
    }
}

/// The text with its keystrokes checked and spaces normalised, or what is wrong with it.
fn check_keystrokes(text: &str) -> Result<String, String> {
    let strokes: Vec<&str> = text.split_whitespace().collect();
    if strokes.is_empty() {
        return Err("empty keystroke".to_owned());
    }
    for stroke in &strokes {
        validate_keystroke(stroke).map_err(|reason| format!("keystroke \"{stroke}\": {reason}"))?;
    }
    Ok(strokes.join(" "))
}

fn check_context(context: &str) -> Result<String, String> {
    let context = context.trim();
    if context.is_empty() {
        return Err("`context` is empty".to_owned());
    }
    let mut depth = 0_i32;
    for c in context.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            break;
        }
    }
    if depth != 0 {
        return Err(format!("context \"{context}\" has unbalanced parentheses"));
    }
    Ok(context.to_owned())
}

/// The names of the modifiers a keystroke may start with (`cmd`, `super` and `win` are one).
const MODIFIERS: &[&str] = &[
    "ctrl",
    "alt",
    "shift",
    "fn",
    "secondary",
    "cmd",
    "super",
    "win",
];

/// Keys that have a name rather than a character.
const NAMED_KEYS: &[&str] = &[
    "enter",
    "escape",
    "tab",
    "space",
    "backspace",
    "delete",
    "up",
    "down",
    "left",
    "right",
    "pageup",
    "pagedown",
    "home",
    "end",
    "insert",
    "back",
    "forward",
];

/// Checks one keystroke (no spaces) the way GPUI reads it: `[ctrl-][alt-][shift-][cmd-][fn-]key`,
/// modifiers in any order and case, the key a single character (`-` is written `cmd--`) or a
/// name (`enter`, `escape`, `tab`, `space`, `backspace`, `delete`, the arrows, `pageup`, `pagedown`,
/// `home`, `end`, `insert`, `f1` to `f35`). `secondary` is `cmd` on macOS and `ctrl` elsewhere.
///
/// GPUI itself accepts any word as a key (`cmd-ctl-k` would bind a key called `ctl`), so this is
/// stricter on purpose: a mistyped modifier is reported instead of binding nothing. Likewise a
/// **printable** key (a character or `space`) must carry `ctrl`, `cmd` (`super`, `win`), `fn` or
/// `secondary`: a binding on `r`, `shift-d` or `alt-e` fires before text input does, so the input
/// box could no longer type that character (on macOS `alt-` also makes accents and dead keys).
/// Keys that type nothing (`escape`, the arrows, `f1`…, `tab`, `enter`, …) may stand alone.
pub fn validate_keystroke(stroke: &str) -> Result<(), String> {
    let mut components = stroke.split('-').peekable();
    let mut key = None;
    // ctrl, cmd, fn or secondary: a modifier that does not produce text.
    let mut command_modifier = false;
    while let Some(component) = components.next() {
        if let Some(modifier) = MODIFIERS.iter().find(|m| component.eq_ignore_ascii_case(m)) {
            command_modifier |= !matches!(*modifier, "alt" | "shift");
            continue;
        }
        if let Some(next) = components.peek() {
            if next.is_empty() && stroke.ends_with('-') && component.is_empty() {
                // `cmd--`: the key is `-` itself.
                key = Some("-");
                break;
            }
            return Err(format!(
                "`{component}` is not a modifier (use ctrl, alt, shift, cmd, fn or secondary)"
            ));
        }
        key = Some(component);
    }
    let key = key.ok_or("it has no key, only modifiers")?;
    if key.is_empty() {
        return Err("it has no key after the last `-`".to_owned());
    }
    let lower = key.to_ascii_lowercase();
    let printable = if key.chars().count() == 1 {
        if key.chars().all(char::is_whitespace) {
            return Err("the key is blank".to_owned());
        }
        true
    } else if lower == "space" {
        true
    } else if NAMED_KEYS.contains(&lower.as_str()) || is_function_key(&lower) {
        false
    } else {
        return Err(format!(
            "`{key}` is not a key (a character, or one of enter, escape, tab, space, backspace, \
             delete, up, down, left, right, pageup, pagedown, home, end, insert, f1 to f35)"
        ));
    };
    if printable && !command_modifier {
        return Err(format!(
            "`{key}` types a character, so it needs ctrl, cmd, fn or secondary (a shortcut on it \
             alone, or with only shift or alt, would stop the input box typing it)"
        ));
    }
    Ok(())
}

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=35).contains(&n))
        && key[1..].bytes().all(|b| b.is_ascii_digit())
}

/// The known action `name` stands for: exactly, or when only the capitalisation differs.
fn resolve_action<'a>(name: &str, known: &[&'a str]) -> Option<&'a str> {
    known
        .iter()
        .copied()
        .find(|k| *k == name)
        .or_else(|| known.iter().copied().find(|k| k.eq_ignore_ascii_case(name)))
}

fn suggestion(name: &str, known: &[&str]) -> String {
    let lower = name.to_ascii_lowercase();
    known
        .iter()
        .map(|k| (edit_distance(&lower, &k.to_ascii_lowercase()), *k))
        .filter(|(distance, _)| *distance <= 2.max(name.len() / 4))
        .min()
        .map_or_else(String::new, |(_, k)| format!("; did you mean `{k}`?"))
}

/// Levenshtein distance over bytes (action names are ASCII).
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<u8> = b.bytes().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.bytes().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(above + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN: &[&str] = &["SplitRight", "ClosePane", "ToggleSidebar", "OpenPalette"];

    fn parse(text: &str) -> KeymapReport {
        parse_keymap(text, KNOWN).expect("valid TOML")
    }

    fn entry(action: &str, keys: &[&str], context: Option<&str>) -> KeymapEntry {
        KeymapEntry {
            action: action.to_owned(),
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
            context: context.map(str::to_owned),
        }
    }

    #[test]
    fn a_string_a_list_and_a_table_are_entries() {
        let report = parse(
            r#"
            SplitRight = "cmd-shift-r"
            ToggleSidebar = ["cmd-b", "cmd-\\"]
            OpenPalette = { keys = "cmd-p", context = "AppShell" }
            "#,
        );
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(
            report.entries,
            [
                entry("OpenPalette", &["cmd-p"], Some("AppShell")),
                entry("SplitRight", &["cmd-shift-r"], None),
                entry("ToggleSidebar", &["cmd-b", "cmd-\\"], None),
            ]
        );
    }

    #[test]
    fn an_empty_list_or_none_unbinds() {
        let report = parse("ClosePane = []\nSplitRight = \"none\"\n");
        assert!(report.problems.is_empty());
        assert!(report.entries.iter().all(KeymapEntry::is_unbind));
        assert_eq!(report.entries.len(), 2);
        let report = parse("ClosePane = { keys = [], context = \"AppShell\" }");
        assert!(report.entries[0].is_unbind());
    }

    #[test]
    fn a_chord_is_one_binding_and_its_spacing_is_tidied() {
        let report = parse("OpenPalette = \"cmd-k   cmd-p\"");
        assert_eq!(report.entries[0].keys, ["cmd-k cmd-p"]);
    }

    #[test]
    fn an_unknown_action_is_reported_with_a_suggestion_and_the_rest_still_apply() {
        let report = parse(
            "Splitright = \"cmd-r\"\nSplitRigth = \"cmd-l\"\nClosePane = \"cmd-x\"\nQuxQuxQux = \"cmd-q\"",
        );
        // Capitalisation alone is forgiven.
        assert_eq!(
            report.entries,
            [
                entry("ClosePane", &["cmd-x"], None),
                entry("SplitRight", &["cmd-r"], None)
            ]
        );
        let messages: Vec<String> = report.problems.iter().map(ToString::to_string).collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(
            messages
                .iter()
                .any(|m| m.contains("QuxQuxQux: unknown action") && !m.contains("did you mean"))
        );
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("SplitRigth: unknown action; did you mean `SplitRight`?"))
        );
    }

    #[test]
    fn bad_keystrokes_skip_only_their_entry() {
        let report = parse(
            r#"
            SplitRight = "cmd-ctl-r"
            ClosePane = ["cmd-x", "cmd-"]
            ToggleSidebar = "cmd-b"
            "#,
        );
        assert_eq!(report.entries, [entry("ToggleSidebar", &["cmd-b"], None)]);
        let messages: Vec<String> = report.problems.iter().map(ToString::to_string).collect();
        assert!(
            messages[0].starts_with("ClosePane: keystroke \"cmd-\""),
            "{messages:?}"
        );
        assert!(
            messages[1].contains("`ctl` is not a modifier"),
            "{messages:?}"
        );
    }

    #[test]
    fn wrong_value_types_and_table_fields_are_reported() {
        let report = parse(
            r#"
            SplitRight = 3
            ClosePane = { key = "cmd-x" }
            ToggleSidebar = { keys = "cmd-b", context = "" }
            OpenPalette = { keys = "cmd-p", context = "(AppShell" }
            "#,
        );
        assert!(report.entries.is_empty());
        let messages: Vec<String> = report.problems.iter().map(ToString::to_string).collect();
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(messages[0].contains("unknown field `key`"), "{messages:?}");
        assert!(messages[1].contains("unbalanced"), "{messages:?}");
        assert!(messages[2].contains("expected a keystroke"));
        assert!(messages[3].contains("`context` is empty"));
    }

    #[test]
    fn toml_syntax_errors_carry_the_position() {
        let error = parse_keymap("SplitRight = \n", KNOWN).unwrap_err();
        assert!(error.starts_with("keymap.toml:1:"), "{error}");
        let error = parse_keymap("ok = []\nbad line\n", KNOWN).unwrap_err();
        assert!(error.starts_with("keymap.toml:2:"), "{error}");
    }

    #[test]
    fn a_binding_that_would_swallow_typing_is_reported() {
        let report = parse(
            "ClosePane = \"r\"\nSplitRight = [\"cmd-r\", \"shift-d\"]\nToggleSidebar = \"escape\"",
        );
        let messages: Vec<String> = report.problems.iter().map(ToString::to_string).collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(
            messages[0].starts_with("ClosePane: keystroke \"r\""),
            "{messages:?}"
        );
        assert!(
            messages[0].contains("needs ctrl, cmd, fn or secondary"),
            "{messages:?}"
        );
        assert!(
            messages[1].starts_with("SplitRight: keystroke \"shift-d\""),
            "{messages:?}"
        );
        // The entry with the problem is skipped as a whole; the fine one applies.
        assert_eq!(report.entries, [entry("ToggleSidebar", &["escape"], None)]);
    }

    #[test]
    fn keystroke_syntax() {
        for good in [
            "cmd-k",
            "ctrl-shift-]",
            "cmd--",
            "CMD-K",
            "alt-f12",
            "secondary-t",
            "up",
            "enter",
            "cmd-alt-shift-d",
            "cmd-\\",
            "super-pageup",
            "cmd-ctrl-left",
            "f1",
            "cmd-K",
            "cmd-,",
            "cmd-+",
            "ctrl-space",
            "fn-a",
            "tab",
            "shift-tab",
            "escape",
            "alt-left",
            "shift-f5",
            "alt-enter",
            "fn-space",
        ] {
            assert_eq!(validate_keystroke(good), Ok(()), "{good}");
        }
        for bad in [
            // A printable key needs ctrl, cmd, fn or secondary, or the input box cannot type it.
            "r",
            "R",
            "shift-d",
            "alt-e",
            "alt-shift-é",
            "space",
            "shift-space",
            "alt-space",
            "shift--",
            "",
            "cmd-",
            "cmd",
            "shift",
            "ctl-k",
            "cmd-kk",
            "cmd-foo-k",
            "-k",
            "cmd-f36",
            "cmd-f0",
            "cmd-fx",
            "cmd-k->k",
        ] {
            assert!(validate_keystroke(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn a_missing_file_is_the_defaults_and_a_broken_one_keeps_the_previous_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.toml");
        let report = load_keymap(&path, KNOWN, &[]);
        assert_eq!(report, KeymapReport::default());

        std::fs::write(&path, "ClosePane = \"cmd-x\"\n").unwrap();
        let good = load_keymap(&path, KNOWN, &[]);
        assert_eq!(good.entries, [entry("ClosePane", &["cmd-x"], None)]);

        std::fs::write(&path, "ClosePane = \n").unwrap();
        let broken = load_keymap(&path, KNOWN, &good.entries);
        assert_eq!(broken.entries, good.entries);
        assert_eq!(broken.problems.len(), 1);
        let shown = broken.problems[0].to_string();
        assert!(
            shown.contains("keymap.toml:1:") && shown.contains("keeping the previous"),
            "{shown}"
        );
    }

    #[test]
    fn edit_distance_is_levenshtein() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("same", "same"), 0);
    }
}
