//! Hot reload of `settings.toml`.
//!
//! The *directory* is watched (non-recursively), not the file, because many
//! editors save by writing a temp file and renaming it over the original, which
//! replaces the inode a file-level watch would be attached to. Events are
//! filtered to the settings file name, coalesced over a debounce window, then
//! the file is re-read with [`Settings::reload`]. An event is emitted only when
//! the outcome differs from the last one emitted.
//!
//! No async runtime: events go to a callback or a [`std::sync::mpsc`] channel
//! from a plain worker thread. The caller bridges to its own executor.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::error::SettingsError;
use crate::schema::Settings;

/// Default quiet period after the last file event before reloading.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(200);

/// Watcher configuration.
#[derive(Debug, Clone, Copy)]
pub struct WatchOptions {
    /// Quiet period after the last relevant file event before the file is
    /// re-read. Covers editors that write in several steps.
    pub debounce: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
        }
    }
}

/// What the watcher reports.
#[derive(Debug, Clone)]
pub enum SettingsEvent {
    /// The file changed and now yields these settings (deleting the file
    /// yields the defaults). Not sent when the result equals the previously
    /// reported settings, **except** right after a [`Failed`](Self::Failed):
    /// the first successful load following a failure is always sent, even if
    /// the settings are unchanged, so observers can clear their error display.
    Reloaded(Settings),
    /// The file changed but could not be used. The previously reported
    /// settings stay in effect and the file is left untouched. Repeats of the
    /// same error are suppressed until the file loads again.
    Failed(SettingsError),
}

/// Keeps the watch alive. Dropping it stops watching and ends the worker
/// thread.
pub struct SettingsWatcher {
    _watcher: RecommendedWatcher,
}

impl std::fmt::Debug for SettingsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsWatcher").finish_non_exhaustive()
    }
}

/// Watch `path`, calling `on_event` from a background thread.
///
/// `initial` is what the app currently uses; it is the baseline for change
/// detection and the "last good" value kept on errors. The parent directory is
/// created if missing. The callback must not block for long.
pub fn watch(
    path: impl Into<PathBuf>,
    initial: Settings,
    options: WatchOptions,
    on_event: impl FnMut(SettingsEvent) + Send + 'static,
) -> Result<SettingsWatcher, SettingsError> {
    let path: PathBuf = path.into();
    let watch_err = |message: String| SettingsError::Watch {
        path: path.clone(),
        message,
    };

    let file_name = path
        .file_name()
        .ok_or_else(|| watch_err("path has no file name".into()))?
        .to_owned();
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_owned(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir).map_err(|e| watch_err(e.to_string()))?;

    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(raw_tx).map_err(|e| watch_err(e.to_string()))?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| watch_err(e.to_string()))?;

    let worker_path = path.clone();
    std::thread::Builder::new()
        .name("settings-watcher".into())
        .spawn(move || {
            run_worker(
                &worker_path,
                &file_name,
                initial,
                options.debounce,
                &raw_rx,
                on_event,
            );
        })
        .map_err(|e| watch_err(e.to_string()))?;

    Ok(SettingsWatcher { _watcher: watcher })
}

/// [`watch`] delivering events over a std channel. Dropping the returned
/// watcher stops the worker. Dropping only the receiver does not: events are
/// then discarded until the watcher is dropped too.
pub fn watch_channel(
    path: impl Into<PathBuf>,
    initial: Settings,
    options: WatchOptions,
) -> Result<(SettingsWatcher, Receiver<SettingsEvent>), SettingsError> {
    let (tx, rx): (Sender<SettingsEvent>, _) = mpsc::channel();
    let watcher = watch(path, initial, options, move |event| {
        // A closed receiver is not an error; the watcher is dropped with it.
        let _ = tx.send(event);
    })?;
    Ok((watcher, rx))
}

fn run_worker(
    path: &Path,
    file_name: &OsString,
    initial: Settings,
    debounce: Duration,
    raw_rx: &Receiver<notify::Result<notify::Event>>,
    mut on_event: impl FnMut(SettingsEvent),
) {
    let mut current = initial;
    let mut last_error: Option<String> = None;

    // Block for the first event; the loop ends when the watcher (and with it
    // the sender inside notify's handler) is dropped.
    while let Ok(first) = raw_rx.recv() {
        let mut relevant = is_relevant(&first, file_name);
        // Coalesce: keep draining until the directory has been quiet.
        loop {
            match raw_rx.recv_timeout(debounce) {
                Ok(next) => relevant |= is_relevant(&next, file_name),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if !relevant {
            continue;
        }

        let outcome = Settings::reload(path, &current);
        match outcome.error {
            None => {
                let recovered = last_error.take().is_some();
                if recovered || outcome.settings != current {
                    current = outcome.settings.clone();
                    on_event(SettingsEvent::Reloaded(outcome.settings));
                }
            }
            Some(error) => {
                let rendered = error.to_string();
                if last_error.as_deref() != Some(rendered.as_str()) {
                    last_error = Some(rendered);
                    on_event(SettingsEvent::Failed(error));
                }
            }
        }
    }
}

/// Does this raw notify result concern the settings file?
fn is_relevant(event: &notify::Result<notify::Event>, file_name: &OsString) -> bool {
    match event {
        // Backend trouble: re-check the file rather than miss a change.
        Err(_) => true,
        Ok(event) => {
            // Reading the file (including our own reload) raises Access events.
            if matches!(event.kind, EventKind::Access(_)) {
                return false;
            }
            event.need_rescan()
                || event.paths.is_empty()
                || event
                    .paths
                    .iter()
                    .any(|p| p.file_name() == Some(file_name.as_os_str()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ThemeMode;
    use std::fs;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(10);

    fn opts() -> WatchOptions {
        WatchOptions {
            debounce: Duration::from_millis(60),
        }
    }

    fn expect_reloaded(rx: &Receiver<SettingsEvent>) -> Settings {
        match rx.recv_timeout(WAIT).expect("timed out waiting for event") {
            SettingsEvent::Reloaded(s) => s,
            other => panic!("expected Reloaded, got {other:?}"),
        }
    }

    fn expect_no_event(rx: &Receiver<SettingsEvent>) {
        assert!(
            rx.recv_timeout(Duration::from_millis(600)).is_err(),
            "unexpected event"
        );
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        (dir, path)
    }

    #[test]
    fn in_place_edit_is_picked_up() {
        let (_dir, path) = setup();
        Settings::default().save(&path).unwrap();
        let (_w, rx) = watch_channel(&path, Settings::default(), opts()).unwrap();

        fs::write(&path, "[appearance]\nfont_size = 20.0\ntheme = \"dark\"\n").unwrap();

        let s = expect_reloaded(&rx);
        assert_eq!(s.appearance.font_size, 20.0);
        assert_eq!(s.appearance.theme, ThemeMode::Dark);
    }

    #[test]
    fn rename_style_save_is_picked_up() {
        let (dir, path) = setup();
        Settings::default().save(&path).unwrap();
        let (_w, rx) = watch_channel(&path, Settings::default(), opts()).unwrap();

        // What vim/VS Code/etc. do: write a sibling, rename over the original.
        let tmp = dir.path().join("settings.toml.swp-new");
        fs::write(&tmp, "[chat]\nscrollback = 321\n").unwrap();
        fs::rename(&tmp, &path).unwrap();

        assert_eq!(expect_reloaded(&rx).chat.scrollback, 321);

        // And a second one, to prove the watch survives the inode swap.
        fs::write(&tmp, "[chat]\nscrollback = 654\n").unwrap();
        fs::rename(&tmp, &path).unwrap();
        assert_eq!(expect_reloaded(&rx).chat.scrollback, 654);
    }

    #[test]
    fn delete_then_recreate_style_save_is_picked_up() {
        let (_dir, path) = setup();
        Settings::default().save(&path).unwrap();
        let (_w, rx) = watch_channel(&path, Settings::default(), opts()).unwrap();

        fs::remove_file(&path).unwrap();
        fs::write(&path, "[chat]\ntimestamps = false\n").unwrap();

        assert!(!expect_reloaded(&rx).chat.timestamps);
    }

    #[test]
    fn our_own_atomic_save_is_picked_up_and_leaves_no_temp_files() {
        let (dir, path) = setup();
        let (_w, rx) = watch_channel(&path, Settings::default(), opts()).unwrap();

        let mut s = Settings::default();
        s.appearance.font_family = "Iosevka".into();
        s.save(&path).unwrap();

        assert_eq!(expect_reloaded(&rx).appearance.font_family, "Iosevka");
        let count = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn file_created_after_watch_started_is_picked_up() {
        let (_dir, path) = setup();
        let (_w, rx) = watch_channel(&path, Settings::default(), opts()).unwrap();
        fs::write(&path, "[chat]\nscrollback = 77\n").unwrap();
        assert_eq!(expect_reloaded(&rx).chat.scrollback, 77);
    }

    #[test]
    fn bad_edit_reports_failure_once_and_recovers() {
        let (_dir, path) = setup();
        let good = {
            let mut s = Settings::default();
            s.chat.scrollback = 500;
            s
        };
        good.save(&path).unwrap();
        let (_w, rx) = watch_channel(&path, good.clone(), opts()).unwrap();

        let broken = "[chat\nscrollback = ";
        fs::write(&path, broken).unwrap();
        match rx.recv_timeout(WAIT).unwrap() {
            SettingsEvent::Failed(SettingsError::Parse { .. }) => {}
            other => panic!("expected Failed(Parse), got {other:?}"),
        }
        // Same error again is suppressed, and the user's file is untouched.
        fs::write(&path, broken).unwrap();
        expect_no_event(&rx);
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);

        // Fixing it reports new settings (not the stale pre-error ones).
        fs::write(&path, "[chat]\nscrollback = 800\n").unwrap();
        assert_eq!(expect_reloaded(&rx).chat.scrollback, 800);
    }

    #[test]
    fn unchanged_content_emits_nothing() {
        let (_dir, path) = setup();
        Settings::default().save(&path).unwrap();
        let (_w, rx) = watch_channel(&path, Settings::default(), opts()).unwrap();
        // Touch with equivalent content.
        Settings::default().save(&path).unwrap();
        expect_no_event(&rx);
    }

    #[test]
    fn unrelated_files_in_the_directory_are_ignored() {
        let (dir, path) = setup();
        Settings::default().save(&path).unwrap();
        let (_w, rx) = watch_channel(&path, Settings::default(), opts()).unwrap();
        fs::write(dir.path().join("keymap.toml"), "x = 1").unwrap();
        fs::write(dir.path().join("notes.txt"), "hello").unwrap();
        expect_no_event(&rx);
    }

    #[test]
    fn rapid_writes_are_debounced_into_one_event() {
        let (_dir, path) = setup();
        Settings::default().save(&path).unwrap();
        let (_w, rx) = watch_channel(
            &path,
            Settings::default(),
            WatchOptions {
                debounce: Duration::from_millis(250),
            },
        )
        .unwrap();
        for n in [11, 12, 13, 14] {
            fs::write(&path, format!("[chat]\nscrollback = {n}0\n")).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        // On a loaded runner the writes may be split across more than one
        // debounce window, so assert on where the stream ends up, not on how
        // many events it took.
        let mut last = expect_reloaded(&rx);
        while let Ok(event) = rx.recv_timeout(Duration::from_millis(800)) {
            match event {
                SettingsEvent::Reloaded(s) => last = s,
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(last.chat.scrollback, 140);
    }

    #[test]
    fn restoring_identical_content_after_a_failure_still_emits_reloaded() {
        let (_dir, path) = setup();
        let good = "[chat]\nscrollback = 500\n";
        fs::write(&path, good).unwrap();
        let baseline = Settings::try_load(&path).unwrap();
        let (_w, rx) = watch_channel(&path, baseline.clone(), opts()).unwrap();

        fs::write(&path, "[chat\nbroken").unwrap();
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            SettingsEvent::Failed(_)
        ));

        // Byte-identical to what the app already has: still must be reported.
        fs::write(&path, good).unwrap();
        assert_eq!(expect_reloaded(&rx), baseline);
        // ...and only once; a further identical write is a no-op again.
        fs::write(&path, good).unwrap();
        expect_no_event(&rx);
    }

    #[test]
    fn missing_directory_is_created_for_the_watch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.toml");
        let (_w, rx) = watch_channel(&path, Settings::default(), opts()).unwrap();
        fs::write(&path, "[chat]\nscrollback = 99\n").unwrap();
        assert_eq!(expect_reloaded(&rx).chat.scrollback, 99);
    }

    #[test]
    fn callback_api_delivers_events_and_dropping_stops_the_worker() {
        let (_dir, path) = setup();
        let (tx, rx) = mpsc::channel();
        let w = watch(&path, Settings::default(), opts(), move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        fs::write(&path, "[chat]\nscrollback = 42\n").unwrap();
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            SettingsEvent::Reloaded(_)
        ));

        drop(w);
        // Worker exits, dropping the callback and its sender: channel closes.
        match rx.recv_timeout(WAIT) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("expected disconnect, got {other:?}"),
        }
    }
}
