//! Hot reload of the user's `themes/` directory.
//!
//! Like the settings watch: the directory is watched (so editors that save by rename work),
//! events are coalesced over a debounce window, then every `*.toml` is re-read. An event is
//! sent only when the outcome (themes or errors) differs from the last one sent.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::error::SettingsError;
use crate::themes::{LoadReport, load_user_themes};
use crate::watch::WatchOptions;

/// Keeps the themes watch alive; dropping it stops the worker thread.
pub struct ThemeWatcher {
    _watcher: RecommendedWatcher,
}

impl std::fmt::Debug for ThemeWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThemeWatcher").finish_non_exhaustive()
    }
}

/// Watches `dir` (created if missing), calling `on_change` from a background thread with the
/// full set of user themes whenever it changes. `initial` is what the caller already loaded.
pub fn watch_themes(
    dir: impl Into<PathBuf>,
    initial: LoadReport,
    options: WatchOptions,
    on_change: impl FnMut(LoadReport) + Send + 'static,
) -> Result<ThemeWatcher, SettingsError> {
    let dir: PathBuf = dir.into();
    let watch_err = |message: String| SettingsError::Watch {
        path: dir.clone(),
        message,
    };
    std::fs::create_dir_all(&dir).map_err(|e| watch_err(e.to_string()))?;

    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(raw_tx).map_err(|e| watch_err(e.to_string()))?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| watch_err(e.to_string()))?;

    let worker_dir = dir.clone();
    std::thread::Builder::new()
        .name("theme-watcher".into())
        .spawn(move || run_worker(&worker_dir, initial, options, &raw_rx, on_change))
        .map_err(|e| watch_err(e.to_string()))?;
    Ok(ThemeWatcher { _watcher: watcher })
}

fn run_worker(
    dir: &Path,
    initial: LoadReport,
    options: WatchOptions,
    raw_rx: &Receiver<notify::Result<notify::Event>>,
    mut on_change: impl FnMut(LoadReport),
) {
    let mut current = initial;
    while let Ok(first) = raw_rx.recv() {
        let mut relevant = is_relevant(&first);
        loop {
            match raw_rx.recv_timeout(options.debounce) {
                Ok(next) => relevant |= is_relevant(&next),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if !relevant {
            continue;
        }
        let report = load_user_themes(dir, &current.themes);
        if report != current {
            current = report.clone();
            on_change(report);
        }
    }
}

/// Reading a theme (including our own reload) raises Access events; everything else may mean
/// a file was added, changed, renamed or removed.
fn is_relevant(event: &notify::Result<notify::Event>) -> bool {
    match event {
        Err(_) => true,
        Ok(event) => !matches!(event.kind, EventKind::Access(_)),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);
    const GOOD: &str = "[dark]\nbackground=\"#000\"\ntext=\"#fff\"\naccent=\"#0f0\"\n";

    fn opts() -> WatchOptions {
        WatchOptions {
            debounce: Duration::from_millis(60),
        }
    }

    #[test]
    fn new_edited_and_removed_theme_files_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let themes = dir.path().join("themes");
        let (tx, rx) = channel();
        let _watcher = watch_themes(&themes, LoadReport::default(), opts(), move |report| {
            let _ = tx.send(report);
        })
        .unwrap();
        assert!(themes.is_dir(), "the directory is created for the watch");

        fs::write(themes.join("mine.toml"), GOOD).unwrap();
        let report = rx.recv_timeout(WAIT).expect("theme added");
        assert_eq!(report.themes.len(), 1);
        assert_eq!(report.themes[0].id, "mine");

        fs::write(themes.join("mine.toml"), GOOD.replace("#0f0", "#f00")).unwrap();
        let report = rx.recv_timeout(WAIT).expect("theme edited");
        assert_eq!(report.themes[0].dark.unwrap().accent.to_string(), "#ff0000");

        fs::write(themes.join("mine.toml"), "[dark\n").unwrap();
        let report = rx.recv_timeout(WAIT).expect("theme broken");
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.themes.len(), 1, "last good version stays");

        fs::remove_file(themes.join("mine.toml")).unwrap();
        let report = rx.recv_timeout(WAIT).expect("theme removed");
        assert!(report.themes.is_empty() && report.errors.is_empty());
    }

    #[test]
    fn a_rename_style_save_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let themes = dir.path().join("themes");
        let (tx, rx) = channel();
        let _watcher = watch_themes(&themes, LoadReport::default(), opts(), move |report| {
            let _ = tx.send(report);
        })
        .unwrap();

        // What vim / VS Code do: write a sibling, then rename it over the theme.
        let temp = themes.join("mine.toml.swp-new");
        fs::write(&temp, GOOD).unwrap();
        fs::rename(&temp, themes.join("mine.toml")).unwrap();
        let report = rx.recv_timeout(WAIT).expect("theme created by rename");
        assert_eq!(report.themes.len(), 1);

        fs::write(&temp, GOOD.replace("#0f0", "#00f")).unwrap();
        fs::rename(&temp, themes.join("mine.toml")).unwrap();
        let report = rx.recv_timeout(WAIT).expect("theme replaced by rename");
        assert_eq!(report.themes[0].dark.unwrap().accent.to_string(), "#0000ff");
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "no stray events"
        );
    }
}
