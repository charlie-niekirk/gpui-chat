//! Typed settings: schema, load/save, file watching and paths.
//!
//! No GPUI and no async runtime: everything here builds and tests on Linux.
//! The app wires this crate in (PR 0.4) as a global plus a bridge from the
//! watcher's channel/callback to the UI thread.
//!
//! Typical use:
//!
//! ```no_run
//! use gpui_chat_settings::{SettingsPaths, Settings, SettingsEvent, WatchOptions, watch_channel};
//!
//! let paths = SettingsPaths::from_env().expect("home directory");
//! let outcome = Settings::load_or_default(&paths.settings_file());
//! if let Some(err) = &outcome.error {
//!     eprintln!("settings problem (using defaults): {err}");
//! }
//! let (_watcher, events) =
//!     watch_channel(paths.settings_file(), outcome.settings.clone(), WatchOptions::default())
//!         .expect("watcher");
//! // Later, on some thread: `events.recv()` yields `SettingsEvent`s.
//! # let _ = events;
//! ```
//!
//! See `docs/SETTINGS.md` for the file format and the compatibility rules.

mod error;
mod paths;
mod schema;
mod store;
mod watch;

pub use error::SettingsError;
pub use paths::{
    CACHE_DIR_ENV, CONFIG_DIR_ENV, KEYMAP_FILE_NAME, PathsError, SETTINGS_FILE_NAME, SettingsPaths,
    THEMES_DIR_NAME,
};
pub use schema::{AppearanceSettings, ChatSettings, SCHEMA_VERSION, Settings, ThemeMode};
pub use store::LoadOutcome;
pub use watch::{
    DEFAULT_DEBOUNCE, SettingsEvent, SettingsWatcher, WatchOptions, watch, watch_channel,
};
