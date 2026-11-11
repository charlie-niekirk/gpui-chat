//! Persisted window placement (`state.toml`).
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

/// Contents of `state.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub window: Option<WindowPlacement>,
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

/// Loads state from `path`. Missing or unreadable/corrupt files yield defaults.
pub fn load_from(path: &Path) -> AppState {
    match std::fs::read_to_string(path) {
        Ok(text) => AppState::parse(&text).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "ignoring corrupt state file");
            AppState::default()
        }),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, path = %path.display(), "could not read state file");
            }
            AppState::default()
        }
    }
}

/// Writes state to `path`, creating its directory. Written atomically via a temp file.
pub fn save_to(path: &Path, state: &AppState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, state.render())?;
    std::fs::rename(&tmp, path)
}

pub fn load() -> AppState {
    state_path()
        .map(|path| load_from(&path))
        .unwrap_or_default()
}

/// Serialises writers (the debounced background save and the synchronous quit flush).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Best-effort save of the window placement; failures are logged, never fatal. Safe to call
/// from any thread.
pub fn save_window(placement: WindowPlacement) {
    let Some(path) = state_path() else { return };
    let _guard = SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut state = load_from(&path);
    state.window = Some(placement);
    if let Err(error) = save_to(&path, &state) {
        tracing::warn!(%error, path = %path.display(), "could not save window state");
    }
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
        };
        save_to(&path, &state).unwrap();
        assert_eq!(load_from(&path), state);
        std::fs::write(&path, "window = 3 ???").unwrap();
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
