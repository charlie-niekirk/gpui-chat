//! Persisted UI state (`state.toml`): window placement, the open channels and the split-pane
//! layout.
//!
//! This is machine-written UI state, deliberately separate from user settings
//! (`crates/settings`). It lives next to them in the platform config dir, in a directory
//! named after `APP_NAME`.

use std::path::{Path, PathBuf};

use gpui_chat_core::APP_NAME;
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "state.toml";

/// How the window was last shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

/// Window rectangle in logical pixels plus display mode.
///
/// On macOS GPUI reports and consumes window bounds **relative to the display the window is
/// on** (`display.bounds()` always has origin 0,0), so `x`/`y` are only meaningful together
/// with `display`. For maximized and fullscreen windows the rectangle is the restore size.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub mode: Mode,
    /// Stable UUID of the display the window was on (`PlatformDisplay::uuid`). `None` when
    /// the platform could not provide one; restore then assumes the primary display.
    #[serde(default)]
    pub display: Option<String>,
}

/// The open channels, in sidebar order, and which one is selected (logins, lowercase, no `#`).
/// Entries are validated when restored, see [`crate::channel_list::ChannelList::from_saved`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedChannels {
    #[serde(default)]
    pub open: Vec<String>,
    #[serde(default)]
    pub selected: Option<String>,
}

/// The emotes used last in the input box (PR 3.3), newest first, by the name that is typed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedEmotes {
    #[serde(default)]
    pub recent: Vec<String>,
}

/// How a split lays out its panes: side by side (`row`) or stacked (`column`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SavedAxis {
    #[default]
    Row,
    Column,
}

/// One node of the saved pane tree (PR 4.1): a pane (`channel`, absent for an empty pane) or, when
/// `panes` is not empty, a split of them along `axis` with their relative `sizes`. Validated when
/// restored, see [`crate::splits::Layout::from_saved`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedPane {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<SavedAxis>,
    /// Fractions of the split, written rounded so the file stays readable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sizes: Vec<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<SavedPane>,
}

/// The split-pane layout: the tree and which pane has focus (its index among the panes, in
/// reading order). A file without it opens one pane on `[channels] selected`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedLayout {
    #[serde(default)]
    pub focused: usize,
    #[serde(default)]
    pub root: SavedPane,
}

/// Contents of `state.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub window: Option<WindowPlacement>,
    #[serde(default)]
    pub channels: Option<SavedChannels>,
    #[serde(default)]
    pub emotes: Option<SavedEmotes>,
    #[serde(default)]
    pub layout: Option<SavedLayout>,
}

/// A connected display as far as restore logic cares.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayInfo {
    /// Stable display UUID, if the platform provides one.
    pub uuid: Option<String>,
    pub is_primary: bool,
    pub width: f32,
    pub height: f32,
}

impl WindowPlacement {
    /// Index into `displays` of the display this placement can be restored on, or `None` if
    /// the window should be centred on the primary display instead (saved display gone,
    /// non-finite values, window larger than the display, or not enough of it on-screen).
    pub fn restore_display(&self, displays: &[DisplayInfo]) -> Option<usize> {
        if !self.is_sane() {
            return None;
        }
        let index = match &self.display {
            Some(uuid) => displays
                .iter()
                .position(|display| display.uuid.as_deref() == Some(uuid)),
            None => displays.iter().position(|display| display.is_primary),
        }?;
        self.fits(&displays[index]).then_some(index)
    }

    /// The window is no larger than the display and enough of it lies on it to be grabbed.
    fn fits(&self, display: &DisplayInfo) -> bool {
        const MIN_VISIBLE: f32 = 120.0;
        if self.width > display.width || self.height > display.height {
            return false;
        }
        let overlap_w = (self.x + self.width).min(display.width) - self.x.max(0.0);
        let overlap_h = (self.y + self.height).min(display.height) - self.y.max(0.0);
        overlap_w >= MIN_VISIBLE.min(self.width) && overlap_h >= MIN_VISIBLE.min(self.height)
    }

    pub fn is_sane(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.width > 0.0
            && self.height > 0.0
    }
}

/// Whole-file (de)serialisation, used by the tests; the app reads section by section ([`load_from`])
/// and writes one section at a time ([`update_at`]).
#[cfg(test)]
impl AppState {
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn render(&self) -> String {
        toml::to_string_pretty(self).expect("AppState serialises to TOML")
    }
}

/// `<config dir>/<APP_NAME>/state.toml`, or `None` if the platform has no config dir.
pub fn state_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join(APP_NAME).join(FILE_NAME))
}

/// One section of the file, or `None` if it is absent or does not have the expected shape (a typo
/// in one section must not cost the others).
fn section<T: serde::de::DeserializeOwned>(table: &toml::Table, key: &str) -> Option<T> {
    let value = table.get(key)?.clone();
    match value.try_into() {
        Ok(section) => Some(section),
        Err(error) => {
            tracing::warn!(%error, section = key, "ignoring malformed section of the state file");
            None
        }
    }
}

/// Loads state from `path`. A missing or unreadable file yields defaults, and so does a file that
/// is not TOML at all; otherwise each section is read on its own, so a malformed `[channels]`
/// leaves `[window]` intact and vice versa.
pub fn load_from(path: &Path) -> AppState {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, path = %path.display(), "could not read state file");
            }
            return AppState::default();
        }
    };
    match text.parse::<toml::Table>() {
        Ok(table) => AppState {
            window: section(&table, "window"),
            channels: section(&table, "channels"),
            emotes: section(&table, "emotes"),
            layout: section(&table, "layout"),
        },
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "ignoring corrupt state file");
            crate::problems::report_from_anywhere(
                "state",
                crate::problems::Problem::warning("Saved window and channel state could not be read")
                    .line("Starting fresh. The unreadable file is kept as state.toml.bad when a new one is written.")
                    .details(format!("{}: {error}", path.display())),
            );
            AppState::default()
        }
    }
}

/// Writes `text` to `path`, creating its directory. Written atomically via a temp file.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Writes the whole state to `path` (used by tests; the app updates one section at a time).
#[cfg(test)]
pub fn save_to(path: &Path, state: &AppState) -> std::io::Result<()> {
    write_atomic(path, &state.render())
}

pub fn load() -> AppState {
    state_path()
        .map(|path| load_from(&path))
        .unwrap_or_default()
}

/// Serialises writers (the debounced background save and the synchronous quit flush).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Replaces the top-level `key` of the file at `path` with `value`, under the save lock, leaving
/// every other key (and any section this build cannot read) exactly as it is. Failures are logged.
///
/// A file that is not TOML at all is moved to `state.toml.bad` first (so nothing is silently
/// destroyed) and a fresh file is started; a file that cannot be read is left alone.
fn update_at(path: &Path, key: &str, value: toml::Value) {
    update_many_at(path, vec![(key, value)]);
}

/// [`update_at`] for several keys in one read-modify-write.
fn update_many_at(path: &Path, entries: Vec<(&str, toml::Value)>) {
    let _guard = SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut table = match std::fs::read_to_string(path) {
        Ok(text) => text.parse::<toml::Table>().unwrap_or_else(|error| {
            let backup = path.with_extension("toml.bad");
            tracing::warn!(%error, path = %path.display(), backup = %backup.display(),
                "state file is not valid TOML; keeping a copy and starting a new one");
            if let Err(error) = std::fs::rename(path, &backup) {
                tracing::warn!(%error, "could not back up the corrupt state file");
            }
            toml::Table::new()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not read state file; not saving");
            return;
        }
    };
    for (key, value) in entries {
        table.insert(key.to_owned(), value);
    }
    let text = toml::to_string_pretty(&table).expect("a TOML table serialises");
    if let Err(error) = write_atomic(path, &text) {
        tracing::warn!(%error, path = %path.display(), "could not save state");
    }
}

fn update_section<T: Serialize>(key: &str, section: &T) {
    let Some(path) = state_path() else { return };
    match toml::Value::try_from(section) {
        Ok(value) => update_at(&path, key, value),
        Err(error) => tracing::warn!(%error, section = key, "could not serialise state"),
    }
}

/// Best-effort save of the window placement; failures are logged, never fatal. Safe to call
/// from any thread. Leaves the rest of the file as it is.
pub fn save_window(placement: WindowPlacement) {
    update_section("window", &placement);
}

/// Best-effort save of the open channels and selection; same rules as [`save_window`].
pub fn save_channels(channels: SavedChannels) {
    update_section("channels", &channels);
}

/// Best-effort save of the split-pane layout; same rules as [`save_window`].
pub fn save_layout(layout: SavedLayout) {
    update_section("layout", &layout);
}

/// The channel list and the layout in one write of the file (they change together).
pub fn save_channels_and_layout(channels: SavedChannels, layout: SavedLayout) {
    let Some(path) = state_path() else { return };
    match (
        toml::Value::try_from(&channels),
        toml::Value::try_from(&layout),
    ) {
        (Ok(channels), Ok(layout)) => {
            update_many_at(&path, vec![("channels", channels), ("layout", layout)]);
        }
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(%error, "could not serialise the channels and layout");
        }
    }
}

/// Best-effort save of the recent emotes; same rules as [`save_window`].
#[cfg_attr(test, allow(dead_code))] // tests do not write the real state file
pub fn save_emotes(emotes: SavedEmotes) {
    update_section("emotes", &emotes);
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: &str = "11111111-1111-1111-1111-111111111111";
    const EXTERNAL: &str = "22222222-2222-2222-2222-222222222222";

    fn placement() -> WindowPlacement {
        WindowPlacement {
            x: 100.0,
            y: 80.0,
            width: 900.0,
            height: 600.0,
            mode: Mode::Windowed,
            display: Some(EXTERNAL.to_string()),
        }
    }

    fn channels() -> SavedChannels {
        SavedChannels {
            open: vec!["forsen".into(), "xqc".into(), "a_b".into()],
            selected: Some("xqc".into()),
        }
    }

    /// `forsen | (xqc over an empty pane)`, the empty one focused.
    fn layout() -> SavedLayout {
        let pane = |channel: Option<&str>| SavedPane {
            channel: channel.map(str::to_owned),
            ..SavedPane::default()
        };
        SavedLayout {
            focused: 2,
            root: SavedPane {
                axis: Some(SavedAxis::Row),
                sizes: vec![0.25, 0.75],
                panes: vec![
                    pane(Some("forsen")),
                    SavedPane {
                        axis: Some(SavedAxis::Column),
                        sizes: vec![0.5, 0.5],
                        panes: vec![pane(Some("xqc")), pane(None)],
                        ..SavedPane::default()
                    },
                ],
                ..SavedPane::default()
            },
        }
    }

    fn emotes() -> SavedEmotes {
        SavedEmotes {
            recent: vec!["Kappa".into(), "monkaW".into()],
        }
    }

    fn display(uuid: &str, is_primary: bool, width: f32, height: f32) -> DisplayInfo {
        DisplayInfo {
            uuid: Some(uuid.to_string()),
            is_primary,
            width,
            height,
        }
    }

    /// Laptop panel (primary) plus a larger external monitor.
    fn displays() -> Vec<DisplayInfo> {
        vec![
            display(PRIMARY, true, 1512.0, 982.0),
            display(EXTERNAL, false, 2560.0, 1440.0),
        ]
    }

    #[test]
    fn toml_roundtrip() {
        let state = AppState {
            window: Some(WindowPlacement {
                mode: Mode::Maximized,
                ..placement()
            }),
            channels: Some(channels()),
            emotes: Some(emotes()),
            layout: Some(layout()),
        };
        assert_eq!(AppState::parse(&state.render()).unwrap(), state);
    }

    #[test]
    fn empty_file_is_default() {
        assert_eq!(AppState::parse("").unwrap(), AppState::default());
    }

    #[test]
    fn file_roundtrip_and_missing_and_corrupt() {
        let dir =
            std::env::temp_dir().join(format!("{APP_NAME}-state-test-{}", std::process::id()));
        let path = dir.join("nested").join(FILE_NAME);
        assert_eq!(load_from(&path), AppState::default());
        let state = AppState {
            window: Some(placement()),
            channels: Some(channels()),
            emotes: Some(emotes()),
            layout: Some(layout()),
        };
        save_to(&path, &state).unwrap();
        assert_eq!(load_from(&path), state);
        std::fs::write(&path, "window = 3 ???").unwrap();
        assert_eq!(load_from(&path), AppState::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channels_are_written_as_a_toml_table_in_order() {
        let text = AppState {
            window: None,
            channels: Some(channels()),
            ..AppState::default()
        }
        .render();
        assert!(text.contains("[channels]"), "{text}");
        assert!(text.contains(r#"selected = "xqc""#), "{text}");
        let parsed = AppState::parse(&text).unwrap();
        assert_eq!(parsed.channels.unwrap().open, ["forsen", "xqc", "a_b"]);
    }

    #[test]
    fn recent_emotes_are_a_list_of_names() {
        let text = AppState {
            emotes: Some(emotes()),
            ..AppState::default()
        }
        .render();
        assert!(text.contains("[emotes]"), "{text}");
        assert_eq!(
            AppState::parse(&text).unwrap().emotes.unwrap().recent,
            ["Kappa", "monkaW"]
        );
        // A file from before the section existed, or with a bad one, has none.
        assert_eq!(AppState::parse("").unwrap().emotes, None);
    }

    #[test]
    fn files_from_before_channels_existed_still_load() {
        let old = "[window]\nx = 1.0\ny = 2.0\nwidth = 900.0\nheight = 600.0\n";
        let state = AppState::parse(old).unwrap();
        assert!(state.window.is_some());
        assert_eq!(state.channels, None);
        // A table with only one of the keys is fine too.
        let partial = AppState::parse("[channels]\nopen = [\"a_b\"]\n").unwrap();
        assert_eq!(partial.channels.unwrap().selected, None);
    }

    #[test]
    fn the_layout_is_a_nested_table_next_to_the_channels() {
        let text = AppState {
            channels: Some(channels()),
            layout: Some(layout()),
            ..AppState::default()
        }
        .render();
        assert!(text.contains("[layout.root]"), "{text}");
        assert!(text.contains(r#"axis = "column""#), "{text}");
        let parsed = AppState::parse(&text).unwrap();
        assert_eq!(parsed.layout, Some(layout()));
        assert_eq!(parsed.channels, Some(channels()));
    }

    #[test]
    fn files_from_before_the_layout_existed_still_load() {
        // What PR 1.5 to 3.5 wrote: channels, no layout.
        let old = "[channels]\nopen = [\"forsen\", \"xqc\"]\nselected = \"xqc\"\n";
        let state = AppState::parse(old).unwrap();
        assert_eq!(state.layout, None);
        assert_eq!(state.channels.unwrap().selected.as_deref(), Some("xqc"));
        // A layout with only a root pane needs no other keys.
        let bare = AppState::parse("[layout.root]\nchannel = \"forsen\"\n").unwrap();
        let layout = bare.layout.unwrap();
        assert_eq!(layout.focused, 0);
        assert_eq!(layout.root.channel.as_deref(), Some("forsen"));
    }

    #[test]
    fn channels_and_layout_are_written_together() {
        let (path, dir) = temp_state("together");
        update_at(&path, "window", value(&placement()));
        update_many_at(
            &path,
            vec![
                ("channels", value(&channels())),
                ("layout", value(&layout())),
            ],
        );
        let state = load_from(&path);
        assert_eq!(state.window, Some(placement()));
        assert_eq!(state.channels, Some(channels()));
        assert_eq!(state.layout, Some(layout()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_layout_keeps_the_channels() {
        let (path, dir) = temp_state("layout");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "[channels]\nopen = [\"a_b\"]\n\n[layout]\nroot = \"left\"\n",
        )
        .unwrap();
        let state = load_from(&path);
        assert_eq!(state.layout, None);
        assert_eq!(state.channels.unwrap().open, ["a_b"]);
        update_at(&path, "layout", value(&layout()));
        let state = load_from(&path);
        assert_eq!(state.layout, Some(layout()));
        assert_eq!(state.channels.unwrap().open, ["a_b"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn value<T: Serialize>(section: &T) -> toml::Value {
        toml::Value::try_from(section).unwrap()
    }

    fn temp_state(tag: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("{APP_NAME}-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.join(FILE_NAME), dir)
    }

    #[test]
    fn updating_one_section_keeps_the_others() {
        let (path, dir) = temp_state("update");
        update_at(&path, "window", value(&placement()));
        update_at(&path, "channels", value(&channels()));
        update_at(&path, "emotes", value(&emotes()));
        let state = load_from(&path);
        assert_eq!(state.window, Some(placement()));
        assert_eq!(state.channels, Some(channels()));
        assert_eq!(state.emotes, Some(emotes()));
        // Saving the recents again replaces only them.
        update_at(&path, "emotes", value(&SavedEmotes { recent: vec![] }));
        let state = load_from(&path);
        assert_eq!(state.emotes, Some(SavedEmotes::default()));
        assert_eq!(state.channels, Some(channels()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_section_does_not_cost_the_other_on_load_or_save() {
        let (path, dir) = temp_state("malformed");
        let window = toml::to_string_pretty(&toml::Table::from_iter([(
            "window".to_owned(),
            value(&placement()),
        )]))
        .unwrap();

        // Broken [channels] (wrong type): the window still loads, and saving the window
        // keeps the broken text for the user to fix instead of discarding it.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, format!("{window}\n[channels]\nopen = 7\n")).unwrap();
        let state = load_from(&path);
        assert_eq!(state.window, Some(placement()));
        assert_eq!(state.channels, None);
        let moved = WindowPlacement {
            x: 5.0,
            ..placement()
        };
        update_at(&path, "window", value(&moved));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("open = 7"), "{text}");
        assert_eq!(load_from(&path).window, Some(moved));

        // Broken [window]: channels load, and saving channels keeps the window text.
        std::fs::write(
            &path,
            "[window]\nx = \"left\"\n\n[channels]\nopen = [\"a_b\"]\n",
        )
        .unwrap();
        let state = load_from(&path);
        assert_eq!(state.window, None);
        assert_eq!(state.channels.unwrap().open, ["a_b"]);
        update_at(&path, "channels", value(&channels()));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#"x = "left""#), "{text}");
        assert_eq!(load_from(&path).channels, Some(channels()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_toml_is_backed_up_before_a_new_one_is_started() {
        let (path, dir) = temp_state("garbage");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "\u{0}\u{0} not toml [[").unwrap();
        assert_eq!(load_from(&path), AppState::default());
        update_at(&path, "channels", value(&channels()));
        let state = load_from(&path);
        assert_eq!(state.window, None);
        assert_eq!(state.channels, Some(channels()));
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bad")).unwrap(),
            "\u{0}\u{0} not toml [["
        );
        // A wrongly typed top-level value is a malformed section, not a crash.
        std::fs::write(&path, "channels = 7\n").unwrap();
        assert_eq!(load_from(&path), AppState::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restores_on_the_saved_display_when_present() {
        // The saved display is the second one: restore must target it, not the primary.
        assert_eq!(placement().restore_display(&displays()), Some(1));
        // x beyond the primary's width is fine because it is relative to the external display.
        let far = WindowPlacement {
            x: 1800.0,
            ..placement()
        };
        assert_eq!(far.restore_display(&displays()), Some(1));
    }

    #[test]
    fn missing_display_falls_back_to_centred() {
        let only_primary = vec![display(PRIMARY, true, 1512.0, 982.0)];
        assert_eq!(placement().restore_display(&only_primary), None);
        assert_eq!(placement().restore_display(&[]), None);
    }

    #[test]
    fn rect_larger_than_display_falls_back_to_centred() {
        let small = vec![
            display(PRIMARY, true, 1512.0, 982.0),
            display(EXTERNAL, false, 800.0, 600.0),
        ];
        assert_eq!(placement().restore_display(&small), None);
        // Fits in size but pushed off the display.
        let off = WindowPlacement {
            x: 2500.0,
            ..placement()
        };
        assert_eq!(off.restore_display(&displays()), None);
        let sliver = WindowPlacement {
            x: 2560.0 - 20.0,
            ..placement()
        };
        assert_eq!(sliver.restore_display(&displays()), None);
    }

    #[test]
    fn partially_off_display_is_kept_while_grabbable() {
        let edge = WindowPlacement {
            x: 2560.0 - 200.0,
            ..placement()
        };
        assert_eq!(edge.restore_display(&displays()), Some(1));
    }

    #[test]
    fn no_saved_display_means_primary() {
        let legacy = WindowPlacement {
            display: None,
            ..placement()
        };
        assert_eq!(legacy.restore_display(&displays()), Some(0));
        let no_primary = vec![display(EXTERNAL, false, 2560.0, 1440.0)];
        assert_eq!(legacy.restore_display(&no_primary), None);
    }

    #[test]
    fn rejects_non_finite() {
        assert!(placement().is_sane());
        let nan = WindowPlacement {
            width: f32::NAN,
            ..placement()
        };
        assert!(!nan.is_sane());
        assert_eq!(nan.restore_display(&displays()), None);
        let flat = WindowPlacement {
            height: 0.0,
            ..placement()
        };
        assert!(!flat.is_sane());
    }
}
