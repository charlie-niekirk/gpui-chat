//! Hot reload of the user's `keymap.toml`.
//!
//! Like the theme watch: the *directory* is watched (so editors that save by rename work and a
//! file created later is seen), events about other files are ignored, the rest is coalesced over a
//! debounce window, then the file is re-read. A report is sent only when it differs from the last
//! one, so touching the file without changing it, or re-saving a broken file, is silent.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::error::SettingsError;
use crate::keymap::{KeymapReport, load_keymap};
use crate::watch::WatchOptions;

/// Keeps the keymap watch alive; dropping it stops the worker thread.
pub struct KeymapWatcher {
    _watcher: RecommendedWatcher,
}

impl std::fmt::Debug for KeymapWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeymapWatcher").finish_non_exhaustive()
    }
}

/// Watches `path`'s directory (created if missing), calling `on_change` from a background thread
/// with the new report whenever it changes. `initial` is what the caller already loaded;
/// `known_actions` is the list the file is checked against (see [`crate::parse_keymap`]).
pub fn watch_keymap(
    path: impl Into<PathBuf>,
    known_actions: Vec<String>,
    initial: KeymapReport,
    options: WatchOptions,
    on_change: impl FnMut(KeymapReport) + Send + 'static,
) -> Result<KeymapWatcher, SettingsError> {
    let path: PathBuf = path.into();
    let watch_err = |message: String| SettingsError::Watch {
        path: path.clone(),
        message,
    };
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&dir).map_err(|e| watch_err(e.to_string()))?;

    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(raw_tx).map_err(|e| watch_err(e.to_string()))?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| watch_err(e.to_string()))?;

    let worker_path = path.clone();
    std::thread::Builder::new()
        .name("keymap-watcher".into())
        .spawn(move || {
            run_worker(
                &worker_path,
                &known_actions,
                initial,
                options,
                &raw_rx,
                on_change,
            );
        })
        .map_err(|e| watch_err(e.to_string()))?;
    Ok(KeymapWatcher { _watcher: watcher })
}

fn run_worker(
    path: &Path,
    known_actions: &[String],
    initial: KeymapReport,
    options: WatchOptions,
    raw_rx: &Receiver<notify::Result<notify::Event>>,
    mut on_change: impl FnMut(KeymapReport),
) {
    let known: Vec<&str> = known_actions.iter().map(String::as_str).collect();
    let mut current = initial;
    while let Ok(first) = raw_rx.recv() {
        let mut relevant = is_relevant(&first, path);
        loop {
            match raw_rx.recv_timeout(options.debounce) {
                Ok(next) => relevant |= is_relevant(&next, path),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if !relevant {
            continue;
        }
        let report = load_keymap(path, &known, &current.entries);
        if report != current {
            current = report.clone();
            on_change(report);
        }
    }
}

/// An event that may mean `keymap.toml` was created, changed, renamed or removed. Reading it
/// (including our own reload) raises Access events, which are not.
fn is_relevant(event: &notify::Result<notify::Event>, path: &Path) -> bool {
    match event {
        Err(_) => true,
        Ok(event) => {
            !matches!(event.kind, EventKind::Access(_))
                && (event.paths.is_empty()
                    || event
                        .paths
                        .iter()
                        .any(|p| p.file_name() == path.file_name()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);
    const KNOWN: &[&str] = &["SplitRight", "ClosePane"];

    fn opts() -> WatchOptions {
        WatchOptions {
            debounce: Duration::from_millis(60),
        }
    }

    fn start(path: &Path) -> (KeymapWatcher, std::sync::mpsc::Receiver<KeymapReport>) {
        let (tx, rx) = channel();
        let watcher = watch_keymap(
            path,
            KNOWN.iter().map(|k| (*k).to_owned()).collect(),
            KeymapReport::default(),
            opts(),
            move |report| {
                let _ = tx.send(report);
            },
        )
        .unwrap();
        (watcher, rx)
    }

    #[test]
    fn creating_editing_breaking_and_removing_the_file_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config").join("keymap.toml");
        let (_watcher, rx) = start(&path);
        assert!(path.parent().unwrap().is_dir(), "the directory is created");

        fs::write(&path, "SplitRight = \"cmd-r\"\n").unwrap();
        let report = rx.recv_timeout(WAIT).expect("created");
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].keys, ["cmd-r"]);

        fs::write(
            &path,
            "SplitRight = \"cmd-r\"\nClosePane = []\nNope = \"cmd-n\"\n",
        )
        .unwrap();
        let report = rx.recv_timeout(WAIT).expect("edited");
        assert_eq!(report.entries.len(), 2);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);

        fs::write(&path, "SplitRight = \n").unwrap();
        let report = rx.recv_timeout(WAIT).expect("broken");
        assert_eq!(report.entries.len(), 2, "last good bindings stay");
        assert_eq!(report.problems.len(), 1);

        fs::remove_file(&path).unwrap();
        let report = rx.recv_timeout(WAIT).expect("removed");
        assert_eq!(report, KeymapReport::default());
    }

    #[test]
    fn a_rename_style_save_is_picked_up_and_other_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.toml");
        let (_watcher, rx) = start(&path);

        fs::write(dir.path().join("settings.toml"), "x = 1").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "a sibling file is not a keymap change"
        );

        // What vim / VS Code do: write a sibling, then rename it over the keymap.
        let temp = dir.path().join("keymap.toml.swp-new");
        fs::write(&temp, "ClosePane = \"cmd-x\"\n").unwrap();
        fs::rename(&temp, &path).unwrap();
        let report = rx.recv_timeout(WAIT).expect("created by rename");
        assert_eq!(report.entries[0].keys, ["cmd-x"]);
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "no stray events"
        );
    }
}
