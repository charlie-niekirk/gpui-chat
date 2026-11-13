//! Settings as a GPUI global.
//!
//! `settings.toml` is loaded once at startup (a bad file leaves the defaults in effect and is
//! reported). A [`SettingsStore`] entity holds the settings in effect plus the last load error,
//! and a file watcher from `gpui-chat-settings` keeps it current: the watcher's thread pushes
//! [`SettingsEvent`]s into a [`crate::bridge`] channel, and the bridge applies them to the
//! store on the UI thread, so observers (`cx.observe(&store, ..)`) run on the main thread.
//!
//! Observers see equal settings as a no-op in effect: a reload that leaves the file's content
//! unchanged re-applies the same values.

use gpui_chat_settings::{
    LoadOutcome, Settings, SettingsError, SettingsEvent, SettingsPaths, SettingsWatcher,
    WatchOptions, watch,
};
use gpui_kit::{App, AppContext as _, Entity, Global};

use crate::bridge::{self, BridgeReceiver};

/// Settings events are rare and tiny; a small queue is plenty.
const EVENT_QUEUE: usize = 16;

/// The settings in effect and the problem with the file on disk, if any.
#[derive(Debug)]
pub struct SettingsStore {
    settings: Settings,
    error: Option<SettingsError>,
}

impl SettingsStore {
    pub fn new(outcome: LoadOutcome) -> Self {
        Self {
            settings: outcome.settings,
            error: outcome.error,
        }
    }

    /// The settings in effect. After a failed reload these are still the last good ones.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Why the file on disk is not in effect (path and parser message in its `Display`);
    /// `None` when it loaded fine.
    pub fn error(&self) -> Option<&SettingsError> {
        self.error.as_ref()
    }

    /// Applies watcher events in order. `Reloaded` replaces the settings and clears the error;
    /// `Failed` records the error and keeps the previous settings. Returns whether the settings
    /// or the shown error changed, so a no-op batch (equal settings, a repeated error) does not
    /// notify observers.
    pub fn apply_events(&mut self, events: impl IntoIterator<Item = SettingsEvent>) -> bool {
        let before = (
            self.settings.clone(),
            self.error.as_ref().map(ToString::to_string),
        );
        for event in events {
            match event {
                SettingsEvent::Reloaded(settings) => {
                    self.settings = settings;
                    self.error = None;
                }
                SettingsEvent::Failed(error) => self.error = Some(error),
            }
        }
        before
            != (
                self.settings.clone(),
                self.error.as_ref().map(ToString::to_string),
            )
    }
}

#[derive(Clone)]
struct SettingsHandle(Entity<SettingsStore>);

impl Global for SettingsHandle {}

/// Keeps the file watcher alive; dropped on quit.
struct WatcherGuard(Option<SettingsWatcher>);

impl Global for WatcherGuard {}

/// The settings store entity, for `cx.observe` / `cx.observe_in`.
pub fn store(cx: &App) -> Entity<SettingsStore> {
    cx.global::<SettingsHandle>().0.clone()
}

/// The settings currently in effect.
pub fn current(cx: &App) -> &Settings {
    cx.global::<SettingsHandle>().0.read(cx).settings()
}

/// The baseline handed to the watcher. It never equals a loaded file (`Settings::parse`
/// always yields `SCHEMA_VERSION` or newer), so the first good load after startup is always
/// reported. That matters when the file was already broken at startup: the watcher did not see
/// that failure, and a fix that happens to equal the defaults would otherwise go unreported
/// and leave the error banner up.
fn watch_baseline() -> Settings {
    Settings {
        schema_version: 0,
        ..Settings::default()
    }
}

/// Starts watching `path`, sending events into a new bridge channel.
fn start_watcher(
    path: &std::path::Path,
) -> Result<(SettingsWatcher, BridgeReceiver<SettingsEvent>), SettingsError> {
    let (tx, rx) = bridge::channel(EVENT_QUEUE, EVENT_QUEUE);
    let watcher = watch(
        path,
        watch_baseline(),
        WatchOptions::default(),
        move |event| {
            // Runs on the watcher's own thread, so blocking for room is fine. A closed bridge
            // means the app is quitting.
            let _ = tx.blocking_send(event);
        },
    )?;
    Ok((watcher, rx))
}

/// Installs a settings store holding `settings` (no file, no watcher), for headless tests.
#[cfg(test)]
pub fn install_for_test(settings: Settings, cx: &mut App) -> Entity<SettingsStore> {
    let store = cx.new(|_| {
        SettingsStore::new(LoadOutcome {
            settings,
            error: None,
        })
    });
    cx.set_global(SettingsHandle(store.clone()));
    store
}

/// Loads `settings.toml`, stores the settings as a global and starts watching the file.
///
/// Returns the receiver the caller must [`bridge::attach`] to the [`store`] once a window
/// exists (events queue up until then), or `None` when the file is not being watched (no home
/// directory, or the watcher could not start; both are logged and the app carries on with
/// what it has).
pub fn init(cx: &mut App) -> Option<BridgeReceiver<SettingsEvent>> {
    let paths = match SettingsPaths::from_env() {
        Ok(paths) => paths,
        Err(error) => {
            tracing::warn!(%error, "no settings location; using default settings");
            let defaults = LoadOutcome {
                settings: Settings::default(),
                error: None,
            };
            let store = cx.new(|_| SettingsStore::new(defaults));
            cx.set_global(SettingsHandle(store));
            return None;
        }
    };
    let file = paths.settings_file();
    let outcome = Settings::load_or_default(&file);
    if let Some(error) = &outcome.error {
        tracing::warn!(%error, "settings file not loaded; using defaults");
    }
    let store = cx.new(|_| SettingsStore::new(outcome));
    cx.set_global(SettingsHandle(store));

    match start_watcher(&file) {
        Ok((watcher, receiver)) => {
            cx.set_global(WatcherGuard(Some(watcher)));
            cx.on_app_quit(|cx| {
                // Stops the watch and lets its worker thread end.
                cx.global_mut::<WatcherGuard>().0.take();
                async {}
            })
            .detach();
            Some(receiver)
        }
        Err(error) => {
            tracing::warn!(%error, "settings changes will not be picked up live");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_chat_settings::ThemeMode;

    use super::*;

    fn store_with(settings: Settings) -> SettingsStore {
        SettingsStore::new(LoadOutcome {
            settings,
            error: None,
        })
    }

    fn failure() -> SettingsError {
        SettingsError::Parse {
            path: "settings.toml".into(),
            message: "expected `=`".into(),
            line: Some(3),
            column: Some(1),
        }
    }

    fn with_size(size: f32) -> Settings {
        let mut settings = Settings::default();
        settings.appearance.font_size = size;
        settings
    }

    #[test]
    fn reload_replaces_settings_and_clears_the_error() {
        let mut store = SettingsStore::new(LoadOutcome {
            settings: Settings::default(),
            error: Some(failure()),
        });
        assert!(store.error().is_some());
        store.apply_events([SettingsEvent::Reloaded(with_size(20.0))]);
        assert_eq!(store.settings().appearance.font_size, 20.0);
        assert!(store.error().is_none());
    }

    #[test]
    fn failure_keeps_previous_settings_and_records_the_error() {
        let mut store = store_with(with_size(18.0));
        store.apply_events([SettingsEvent::Failed(failure())]);
        assert_eq!(store.settings().appearance.font_size, 18.0);
        let shown = store.error().unwrap().to_string();
        assert!(shown.contains("settings.toml"), "{shown}");
        assert!(shown.contains("expected `=`"), "{shown}");
    }

    #[test]
    fn apply_events_reports_whether_anything_changed() {
        let mut store = store_with(with_size(18.0));
        assert!(!store.apply_events([SettingsEvent::Reloaded(with_size(18.0))]));
        assert!(store.apply_events([SettingsEvent::Failed(failure())]));
        // The same error again changes nothing visible.
        assert!(!store.apply_events([SettingsEvent::Failed(failure())]));
        // Recovery with unchanged settings still clears the banner.
        assert!(store.apply_events([SettingsEvent::Reloaded(with_size(18.0))]));
        assert!(store.error().is_none());
        // A batch that ends where it started is not a change.
        assert!(!store.apply_events([
            SettingsEvent::Failed(failure()),
            SettingsEvent::Reloaded(with_size(18.0)),
        ]));
    }

    #[test]
    fn a_batch_is_applied_in_order() {
        let mut store = store_with(Settings::default());
        store.apply_events([
            SettingsEvent::Reloaded(with_size(16.0)),
            SettingsEvent::Failed(failure()),
            SettingsEvent::Reloaded(with_size(22.0)),
        ]);
        assert_eq!(store.settings().appearance.font_size, 22.0);
        assert!(store.error().is_none(), "later recovery wins");

        store.apply_events([
            SettingsEvent::Reloaded(with_size(10.0)),
            SettingsEvent::Failed(failure()),
        ]);
        assert_eq!(store.settings().appearance.font_size, 10.0);
        assert!(store.error().is_some(), "later failure wins, settings kept");
    }

    #[test]
    fn fixing_a_file_that_was_broken_at_startup_is_reported_even_if_it_equals_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "this is = = not toml").unwrap();
        let outcome = Settings::load_or_default(&path);
        assert!(outcome.error.is_some());

        let (_watcher, rx) = start_watcher(&path).unwrap();
        // Restore a file equal to the defaults. Without the sentinel baseline the watcher
        // would consider this "unchanged" and stay silent.
        std::fs::write(&path, "").unwrap();
        let mut rx = rx;
        match wait_for_event(&mut rx) {
            SettingsEvent::Reloaded(settings) => {
                assert_eq!(settings.appearance.theme, ThemeMode::System);
            }
            other => panic!("expected Reloaded, got {other:?}"),
        }
    }

    fn wait_for_event(rx: &mut BridgeReceiver<SettingsEvent>) -> SettingsEvent {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(10), rx.first()).await })
            .expect("timed out waiting for the watcher")
            .expect("watcher channel closed")
    }
}
