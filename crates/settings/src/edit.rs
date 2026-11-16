//! Format-preserving edits of `settings.toml` for the settings window.
//!
//! [`Settings::save`] rewrites the whole file from the typed struct, which drops comments and
//! keys this build does not know. The settings window must not do that to a hand-edited file, so
//! it changes single keys through [`Settings::update_file`] instead: the file is parsed with
//! `toml_edit`, the keys are set (or removed) in place, and everything else (comments, blank
//! lines, key order, unknown keys and sections, the trailing comment of an edited value) stays as
//! it was.
//!
//! Nothing is written unless the *result* is a valid settings file. A file that is not valid TOML
//! is therefore never touched, and a file with one out-of-range value only accepts an edit that
//! fixes it. The caller gets the error to show.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::error::SettingsError;
use crate::schema::Settings;
use crate::store::atomic_write;

/// A value to write.
#[derive(Debug, Clone, PartialEq)]
pub enum EditValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    StrList(Vec<String>),
    /// An array of tables, written as `[[section.key]]` blocks, one per row (each row is its
    /// `key = value` pairs, scalars only). An empty list removes the key. The old blocks are
    /// replaced as a whole, so comments inside them are lost.
    Tables(Vec<Vec<(&'static str, EditValue)>>),
}

impl EditValue {
    /// A float as the user would type it: `1.4_f32` is written `1.4`, not `1.399999976158142`.
    pub fn float(value: f32) -> Self {
        Self::Float(value.to_string().parse().unwrap_or(f64::from(value)))
    }

    fn into_toml(self) -> Value {
        match self {
            Self::Bool(v) => Value::from(v),
            Self::Int(v) => Value::from(v),
            Self::Float(v) => Value::from(v),
            Self::Str(v) => Value::from(v),
            Self::StrList(items) => Value::Array(items.into_iter().collect::<Array>()),
            // Handled before a value is made (see `apply_one`); a row holds scalars only.
            Self::Tables(_) => Value::Array(Array::new()),
        }
    }
}

/// One key to change: set it, or remove it so the default (or "unset") applies again.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingEdit {
    /// `"section.key"`, as in the file: `"appearance.font_size"`.
    pub field: &'static str,
    /// `None` removes the key.
    pub value: Option<EditValue>,
}

impl SettingEdit {
    pub fn set(field: &'static str, value: EditValue) -> Self {
        Self {
            field,
            value: Some(value),
        }
    }

    pub fn remove(field: &'static str) -> Self {
        Self { field, value: None }
    }
}

impl Settings {
    /// Applies `edits` to the settings file at `path` and writes it back atomically, keeping
    /// everything the edits do not touch. A missing file is created. Returns the settings the new
    /// file holds.
    ///
    /// Fails, writing nothing, when the file cannot be read, is not valid TOML, or would not be
    /// valid settings after the edits (see the module docs). When the edits change nothing, the
    /// file is not rewritten.
    pub fn update_file(path: &Path, edits: &[SettingEdit]) -> Result<Settings, SettingsError> {
        let old = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(SettingsError::Read {
                    path: path.to_owned(),
                    source: Arc::new(e),
                });
            }
        };
        let (text, settings) = apply_edits(&old, path, edits)?;
        if text != old {
            atomic_write(path, text.as_bytes()).map_err(|source| SettingsError::Write {
                path: path.to_owned(),
                source: Arc::new(source),
            })?;
        }
        Ok(settings)
    }
}

/// [`Settings::update_file`] on text: the edited text and the settings it parses to. `path` is
/// for error messages only.
pub fn apply_edits(
    text: &str,
    path: &Path,
    edits: &[SettingEdit],
) -> Result<(String, Settings), SettingsError> {
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        let (line, column) = e
            .span()
            .map(|span| crate::store::line_col(text, span.start))
            .unzip();
        SettingsError::Parse {
            path: path.to_owned(),
            message: e.message().to_owned(),
            line,
            column,
        }
    })?;
    for edit in edits {
        apply_one(&mut doc, path, edit)?;
    }
    let edited = doc.to_string();
    let settings = Settings::parse(&edited, path)?;
    Ok((edited, settings))
}

fn apply_one(doc: &mut DocumentMut, path: &Path, edit: &SettingEdit) -> Result<(), SettingsError> {
    let (section, key) = edit
        .field
        .split_once('.')
        .expect("a field is written `section.key`");
    let wrong_shape = |what: &str| SettingsError::Invalid {
        path: path.to_owned(),
        field: edit.field,
        reason: format!("cannot be changed here: {what}"),
    };
    let Some(value) = edit.value.clone() else {
        if let Some(table) = doc.get_mut(section).and_then(Item::as_table_like_mut) {
            table.remove(key);
        }
        return Ok(());
    };
    let table = doc
        .entry(section)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| wrong_shape(&format!("`{section}` is not a table")))?;
    if let EditValue::Tables(rows) = value {
        if rows.is_empty() {
            table.remove(key);
            return Ok(());
        }
        let mut blocks = ArrayOfTables::new();
        for row in rows {
            let mut block = Table::new();
            for (name, value) in row {
                block.insert(name, Item::Value(value.into_toml()));
            }
            blocks.push(block);
        }
        table.insert(key, Item::ArrayOfTables(blocks));
        return Ok(());
    }
    // A whole number is written as an integer (`font_size = 16`) unless the value already there
    // is a float literal (`14.0`): the file keeps the style its author used.
    let existing_is_float = table
        .get(key)
        .and_then(Item::as_value)
        .is_some_and(Value::is_float);
    let value = match value {
        EditValue::Float(v) if v.fract() == 0.0 && v.abs() < 1e15 && !existing_is_float => {
            EditValue::Int(v as i64)
        }
        other => other,
    };
    let mut new = value.into_toml();
    match table.get_mut(key) {
        Some(item) => {
            // Keep the comment after the old value (`font_size = 14.0  # 6 to 72`).
            if let Some(old) = item.as_value() {
                *new.decor_mut() = old.decor().clone();
            }
            *item = Item::Value(new);
        }
        None => {
            table.insert(key, Item::Value(new));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ThemeMode;

    fn p() -> &'static Path {
        Path::new("settings.toml")
    }

    const HAND_EDITED: &str = "\
# My chat settings. Do not lose this comment.
schema_version = 1
future_top_level = \"kept\"

[appearance]
font_size = 14.0   # 6 to 72
theme = \"dark\"
# a note about the theme
theme_name = \"nord\"
future_key = [1, 2]

# the chat section
[chat]
timestamps = true

[from_the_future]
x = 1
";

    fn edit(text: &str, edits: &[SettingEdit]) -> (String, Settings) {
        apply_edits(text, p(), edits).unwrap()
    }

    #[test]
    fn comments_unknown_keys_and_order_survive() {
        let (out, settings) = edit(
            HAND_EDITED,
            &[
                SettingEdit::set("appearance.font_size", EditValue::float(18.0)),
                SettingEdit::set("chat.timestamps", EditValue::Bool(false)),
            ],
        );
        assert_eq!(settings.appearance.font_size, 18.0);
        assert!(!settings.chat.timestamps);
        // Exactly the two values differ; nothing else moved.
        let expected = HAND_EDITED
            .replace(
                "font_size = 14.0   # 6 to 72",
                "font_size = 18.0   # 6 to 72",
            )
            .replace("timestamps = true", "timestamps = false");
        assert_eq!(out, expected);
    }

    #[test]
    fn new_keys_and_sections_are_added_and_the_rest_kept() {
        let (out, settings) = edit(
            HAND_EDITED,
            &[
                SettingEdit::set("emotes.scale", EditValue::float(1.5)),
                SettingEdit::set("appearance.line_height", EditValue::float(1.6)),
                SettingEdit::set(
                    "streamlink.extra_args",
                    EditValue::StrList(vec!["--twitch-low-latency".into()]),
                ),
            ],
        );
        assert_eq!(settings.emotes.scale, 1.5);
        assert_eq!(settings.appearance.line_height, 1.6);
        assert_eq!(settings.streamlink.extra_args, ["--twitch-low-latency"]);
        assert!(out.contains("1.6") && !out.contains("1.59999"), "{out}");
        for kept in [
            "# My chat settings. Do not lose this comment.",
            "future_top_level = \"kept\"",
            "# a note about the theme",
            "future_key = [1, 2]",
            "# the chat section",
            "[from_the_future]",
        ] {
            assert!(out.contains(kept), "lost {kept:?} in:\n{out}");
        }
    }

    #[test]
    fn an_empty_or_missing_file_gets_just_the_edited_keys() {
        let (out, settings) = edit(
            "",
            &[SettingEdit::set(
                "appearance.theme",
                EditValue::Str("light".into()),
            )],
        );
        assert_eq!(out, "[appearance]\ntheme = \"light\"\n");
        assert_eq!(settings.appearance.theme, ThemeMode::Light);
    }

    #[test]
    fn removing_a_key_keeps_the_rest_of_the_section() {
        let (out, settings) = edit(
            "[appearance]\ntheme_name_light = \"rose-pine\" # pair\ntheme = \"light\"\n",
            &[SettingEdit::remove("appearance.theme_name_light")],
        );
        assert_eq!(settings.appearance.theme_name_light, None);
        assert_eq!(out, "[appearance]\ntheme = \"light\"\n");
        // Removing what is not there is fine.
        edit("", &[SettingEdit::remove("appearance.theme_name_light")]);
    }

    #[test]
    fn a_float_is_written_the_way_it_is_typed() {
        for (value, text) in [(1.4_f32, "1.4"), (0.7, "0.7"), (3.5, "3.5"), (2.35, "2.35")] {
            let (out, _) = edit(
                "",
                &[SettingEdit::set("emotes.scale", EditValue::float(value))],
            );
            assert_eq!(out, format!("[emotes]\nscale = {text}\n"));
        }
    }

    #[test]
    fn a_whole_number_keeps_the_style_of_the_literal_it_replaces() {
        let set = |text: &str, v: f32| {
            edit(
                text,
                &[SettingEdit::set(
                    "appearance.font_size",
                    EditValue::float(v),
                )],
            )
        };
        // An integer literal stays an integer, and so does a new key.
        let (out, settings) = set("[appearance]\nfont_size = 14\n", 16.0);
        assert_eq!(out, "[appearance]\nfont_size = 16\n");
        assert_eq!(settings.appearance.font_size, 16.0);
        assert_eq!(set("", 16.0).0, "[appearance]\nfont_size = 16\n");
        // A float literal stays a float.
        assert_eq!(
            set("[appearance]\nfont_size = 14.0 # pt\n", 16.0).0,
            "[appearance]\nfont_size = 16.0 # pt\n"
        );
        // A fraction is a float whatever was there.
        assert_eq!(
            set("[appearance]\nfont_size = 14\n", 14.5).0,
            "[appearance]\nfont_size = 14.5\n"
        );
    }

    fn rule(pattern: &str, color: &str) -> Vec<(&'static str, EditValue)> {
        vec![
            ("kind", EditValue::Str("phrase".into())),
            ("pattern", EditValue::Str(pattern.into())),
            ("color", EditValue::Str(color.into())),
            ("notify", EditValue::Bool(true)),
        ]
    }

    #[test]
    fn ignoring_a_user_adds_to_the_list_in_place_and_a_bad_rule_is_refused() {
        let text = "[ignores]\n# the usual suspects\nusers = [\"spam\"]\nshow_collapsed = true\n\n[chat]\ntimestamps = false\n";
        let (out, settings) = edit(
            text,
            &[SettingEdit::set(
                "ignores.users",
                EditValue::StrList(vec!["spam".into(), "bob".into()]),
            )],
        );
        assert_eq!(settings.ignores.users, ["spam", "bob"]);
        assert!(settings.ignores.show_collapsed);
        assert!(
            out.contains("# the usual suspects") && out.contains("timestamps = false"),
            "{out}"
        );
        let (out, settings) = edit(
            &out,
            &[SettingEdit::set(
                "ignores.rules",
                EditValue::Tables(vec![vec![
                    ("kind", EditValue::Str("regex".into())),
                    ("pattern", EditValue::Str("^!\\w+".into())),
                ]]),
            )],
        );
        assert_eq!(settings.ignores.rules.len(), 1);
        assert!(out.contains("[[ignores.rules]]"), "{out}");
        // A regular expression that does not compile is never written.
        let error = apply_edits(
            &out,
            Path::new("settings.toml"),
            &[SettingEdit::set(
                "ignores.rules",
                EditValue::Tables(vec![vec![
                    ("kind", EditValue::Str("regex".into())),
                    ("pattern", EditValue::Str("(".into())),
                ]]),
            )],
        )
        .unwrap_err();
        assert!(error.to_string().contains("ignores.rules"), "{error}");
    }

    #[test]
    fn a_list_of_tables_becomes_blocks_and_the_rest_of_the_file_stays() {
        let text = "# mine\n[highlights]\nmentions = false # off\n\n[chat]\ntimestamps = false\n";
        let (out, settings) = edit(
            text,
            &[SettingEdit::set(
                "highlights.rules",
                EditValue::Tables(vec![rule("nice play", "green"), rule("gg", "red")]),
            )],
        );
        assert_eq!(settings.highlights.rules.len(), 2);
        assert_eq!(settings.highlights.rules[0].pattern, "nice play");
        assert_eq!(
            settings.highlights.rules[1].color,
            gpui_chat_core::HighlightColor::Red
        );
        assert!(!settings.highlights.mentions);
        assert!(
            out.contains("# mine") && out.contains("mentions = false # off"),
            "{out}"
        );
        assert!(out.contains("[[highlights.rules]]"), "{out}");
        // Replacing the list replaces every block; nothing of the old ones is left.
        let (again, settings) = edit(
            &out,
            &[SettingEdit::set(
                "highlights.rules",
                EditValue::Tables(vec![rule("only", "yellow")]),
            )],
        );
        assert_eq!(settings.highlights.rules.len(), 1);
        assert!(!again.contains("nice play"), "{again}");
        assert!(again.contains("timestamps = false"), "{again}");
        // An empty list removes the key.
        let (none, settings) = edit(
            &again,
            &[SettingEdit::set(
                "highlights.rules",
                EditValue::Tables(vec![]),
            )],
        );
        assert!(settings.highlights.rules.is_empty());
        assert!(!none.contains("[[highlights.rules]]"), "{none}");
    }

    #[test]
    fn a_missing_section_gets_the_blocks_and_a_bad_pattern_is_refused_untouched() {
        let (out, _) = edit(
            "",
            &[SettingEdit::set(
                "highlights.users",
                EditValue::Tables(vec![vec![
                    ("login", EditValue::Str("alice".into())),
                    ("color", EditValue::Str("green".into())),
                ]]),
            )],
        );
        assert!(
            out.contains("[[highlights.users]]") && out.contains("login = \"alice\""),
            "{out}"
        );
        let err = apply_edits(
            "",
            p(),
            &[SettingEdit::set(
                "highlights.rules",
                EditValue::Tables(vec![vec![
                    ("kind", EditValue::Str("regex".into())),
                    ("pattern", EditValue::Str("(".into())),
                ]]),
            )],
        )
        .unwrap_err();
        assert!(
            matches!(&err, SettingsError::Invalid { field: "highlights.rules", reason, .. } if reason.contains("unclosed group")),
            "{err}"
        );
    }

    #[test]
    fn inline_tables_are_edited_in_place() {
        let (out, settings) = edit(
            "chat = { scrollback = 500, timestamps = true }\n",
            &[SettingEdit::set("chat.timestamps", EditValue::Bool(false))],
        );
        assert!(!settings.chat.timestamps);
        assert_eq!(settings.chat.scrollback, 500);
        assert!(out.contains("scrollback = 500"), "{out}");
    }

    #[test]
    fn invalid_toml_is_an_error_with_a_position() {
        let err = apply_edits(
            "[appearance\nfont_size = 1",
            p(),
            &[SettingEdit::set("chat.timestamps", EditValue::Bool(false))],
        )
        .unwrap_err();
        assert!(
            matches!(err, SettingsError::Parse { line: Some(_), .. }),
            "{err}"
        );
    }

    #[test]
    fn a_value_that_would_make_the_file_invalid_is_refused() {
        let err = apply_edits(
            "",
            p(),
            &[SettingEdit::set(
                "appearance.font_size",
                EditValue::Float(500.0),
            )],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SettingsError::Invalid {
                field: "appearance.font_size",
                ..
            }
        ));
    }

    #[test]
    fn a_section_that_is_not_a_table_is_an_error_not_a_panic() {
        let err = apply_edits(
            "chat = 5\n",
            p(),
            &[SettingEdit::set("chat.timestamps", EditValue::Bool(false))],
        )
        .unwrap_err();
        // `chat = 5` is not valid settings, whatever is edited.
        assert!(matches!(
            err,
            SettingsError::Invalid { .. } | SettingsError::Parse { .. }
        ));
    }

    #[test]
    fn a_file_with_a_bad_value_only_accepts_the_edit_that_fixes_it() {
        let bad = "[appearance]\nfont_size = 500 # too big\n\n[chat]\nscrollback = 200\n";
        // Another key: still invalid, so refused and the text is what the caller keeps.
        assert!(
            apply_edits(
                bad,
                p(),
                &[SettingEdit::set("chat.timestamps", EditValue::Bool(false))]
            )
            .is_err()
        );
        // The bad key itself: accepted, comment kept.
        let (out, settings) = edit(
            bad,
            &[SettingEdit::set(
                "appearance.font_size",
                EditValue::float(16.0),
            )],
        );
        assert_eq!(settings.appearance.font_size, 16.0);
        assert!(out.contains("font_size = 16 # too big"), "{out}");
    }

    #[test]
    fn update_file_writes_only_when_the_result_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, HAND_EDITED).unwrap();

        let settings = Settings::update_file(
            &path,
            &[SettingEdit::set("emotes.animate", EditValue::Bool(false))],
        )
        .unwrap();
        assert!(!settings.emotes.animate);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("# My chat settings."));
        assert!(on_disk.contains("animate = false"));

        // A refused edit leaves the file byte for byte as it was.
        let before = on_disk;
        let err = Settings::update_file(
            &path,
            &[SettingEdit::set("emotes.scale", EditValue::Float(99.0))],
        );
        assert!(err.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // Invalid TOML is never touched.
        std::fs::write(&path, "oops = = 1\n").unwrap();
        assert!(
            Settings::update_file(
                &path,
                &[SettingEdit::set("chat.timestamps", EditValue::Bool(false))]
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "oops = = 1\n");
    }

    #[test]
    fn the_history_switch_writes_only_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "# mine\n[chat]\nscrollback = 500 # keep\n").unwrap();
        Settings::update_file(
            &path,
            &[SettingEdit::set("chat.history", EditValue::Bool(false))],
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# mine") && text.contains("scrollback = 500 # keep"),
            "{text}"
        );
        assert!(text.contains("history = false"), "{text}");
        assert!(!Settings::try_load(&path).unwrap().chat.history);
    }

    #[test]
    fn update_file_creates_the_file_and_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.toml");
        Settings::update_file(
            &path,
            &[SettingEdit::set("chat.scrollback", EditValue::Int(300))],
        )
        .unwrap();
        assert_eq!(Settings::try_load(&path).unwrap().chat.scrollback, 300);
    }

    #[test]
    fn an_edit_that_changes_nothing_does_not_rewrite_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[chat]\ntimestamps = true\n").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        Settings::update_file(
            &path,
            &[SettingEdit::set("chat.timestamps", EditValue::Bool(true))],
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
    }
}
