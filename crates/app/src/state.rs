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

/// Window rectangle in logical pixels (global screen coordinates) plus display mode. For
/// maximized and fullscreen windows the rectangle is the restore size.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub mode: Mode,
}

/// Contents of `state.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub window: Option<WindowPlacement>,
}

/// An axis-aligned rectangle, used for display bounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowPlacement {
    /// Whether enough of the window lies on one of `displays` to be grabbed and used. A
    /// monitor that has since been unplugged must not strand the window off-screen.
    pub fn is_visible_on(&self, displays: &[Rect]) -> bool {
        const MIN_VISIBLE: f32 = 120.0;
        displays.iter().any(|display| {
            let overlap_w =
                (self.x + self.width).min(display.x + display.width) - self.x.max(display.x);
            let overlap_h =
                (self.y + self.height).min(display.y + display.height) - self.y.max(display.y);
            overlap_w >= MIN_VISIBLE.min(self.width) && overlap_h >= MIN_VISIBLE.min(self.height)
        })
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

/// Best-effort save of the window placement; failures are logged, never fatal.
pub fn save_window(placement: WindowPlacement) {
    let Some(path) = state_path() else { return };
    let mut state = load_from(&path);
    state.window = Some(placement);
    if let Err(error) = save_to(&path, &state) {
        tracing::warn!(%error, path = %path.display(), "could not save window state");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement() -> WindowPlacement {
        WindowPlacement {
            x: 100.0,
            y: 80.0,
            width: 900.0,
            height: 600.0,
            mode: Mode::Windowed,
        }
    }

    const DISPLAY: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 1728.0,
        height: 1117.0,
    };

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
        let dir = std::env::temp_dir().join(format!("gpui-chat-state-test-{}", std::process::id()));
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
    fn visibility_check() {
        assert!(placement().is_visible_on(&[DISPLAY]));
        // Entirely on a monitor that is no longer attached.
        let gone = WindowPlacement {
            x: 3000.0,
            ..placement()
        };
        assert!(!gone.is_visible_on(&[DISPLAY]));
        // Mostly off-screen but a usable part remains.
        let edge = WindowPlacement {
            x: 1728.0 - 200.0,
            ..placement()
        };
        assert!(edge.is_visible_on(&[DISPLAY]));
        // Only a sliver on-screen.
        let sliver = WindowPlacement {
            x: 1728.0 - 20.0,
            ..placement()
        };
        assert!(!sliver.is_visible_on(&[DISPLAY]));
        assert!(!placement().is_visible_on(&[]));
    }

    #[test]
    fn rejects_non_finite() {
        assert!(placement().is_sane());
        assert!(
            !WindowPlacement {
                width: f32::NAN,
                ..placement()
            }
            .is_sane()
        );
        assert!(
            !WindowPlacement {
                height: 0.0,
                ..placement()
            }
            .is_sane()
        );
    }
}
