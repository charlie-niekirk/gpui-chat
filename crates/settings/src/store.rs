//! Loading and saving `settings.toml`.
//!
//! Loading never modifies the file and never panics: a file that cannot be
//! read, parsed or validated yields a [`SettingsError`] and the caller keeps
//! its fallback (defaults at startup, the last good value on reload).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::SettingsError;
use crate::schema::{SCHEMA_VERSION, Settings};

/// Result of a tolerant load: always usable settings, plus the problem (if
/// any) the app should surface, e.g. as a notification.
#[derive(Debug, Clone)]
pub struct LoadOutcome {
    /// The settings to use: the parsed file on success, otherwise the fallback.
    pub settings: Settings,
    /// Why the file was not used. `None` when it loaded (or did not exist).
    pub error: Option<SettingsError>,
}

impl Settings {
    /// Parse settings from TOML text. `path` is used only for error messages.
    ///
    /// Missing keys take defaults, unknown keys are ignored, wrong types and
    /// out-of-range values are errors.
    pub fn parse(text: &str, path: &Path) -> Result<Settings, SettingsError> {
        let settings: Settings = toml::from_str(text).map_err(|e| {
            let (line, column) = e.span().map(|span| line_col(text, span.start)).unzip();
            SettingsError::Parse {
                path: path.to_owned(),
                message: e.message().to_owned(),
                line,
                column,
            }
        })?;
        settings
            .validate()
            .map_err(|(field, reason)| SettingsError::Invalid {
                path: path.to_owned(),
                field,
                reason,
            })?;
        if settings.schema_version > SCHEMA_VERSION {
            tracing::warn!(
                file_version = settings.schema_version,
                supported = SCHEMA_VERSION,
                "settings file is from a newer version; unknown keys are ignored"
            );
        }
        Ok(settings)
    }

    /// Strict load. A missing file is not an error and yields the defaults;
    /// anything else that goes wrong is.
    pub fn try_load(path: &Path) -> Result<Settings, SettingsError> {
        match fs::read_to_string(path) {
            Ok(text) => Settings::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(SettingsError::Read {
                path: path.to_owned(),
                source: Arc::new(e),
            }),
        }
    }

    /// Startup load: on any problem, use the defaults and report the error.
    pub fn load_or_default(path: &Path) -> LoadOutcome {
        Settings::reload(path, &Settings::default())
    }

    /// Reload: on any problem, keep `last_good` and report the error. A
    /// missing file loads the defaults (deleting the file resets settings).
    pub fn reload(path: &Path, last_good: &Settings) -> LoadOutcome {
        match Settings::try_load(path) {
            Ok(settings) => LoadOutcome {
                settings,
                error: None,
            },
            Err(error) => {
                tracing::warn!(%error, "settings not loaded; keeping previous values");
                LoadOutcome {
                    settings: last_good.clone(),
                    error: Some(error),
                }
            }
        }
    }

    /// Serialise to TOML text. Always stamps the current [`SCHEMA_VERSION`],
    /// whatever version the settings were loaded from.
    pub fn to_toml(&self) -> Result<String, SettingsError> {
        let mut current = self.clone();
        current.schema_version = SCHEMA_VERSION;
        toml::to_string_pretty(&current).map_err(|e| SettingsError::Serialize(e.to_string()))
    }

    /// Save atomically: write a temp file in the same directory, fsync it,
    /// rename over `path`, then fsync the directory (best effort). Readers and
    /// watchers see either the old or the new file, never a partial one.
    ///
    /// Creates the parent directory if needed. If `path` is a symlink (dotfile
    /// managers do this) the file it points to is replaced, not the link.
    ///
    /// Saving rewrites the whole file from the typed struct: comments and
    /// unknown keys in the existing file are lost. Callers must not save over a
    /// file that failed to load (see [`LoadOutcome::error`]) without the
    /// user's say-so.
    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        let text = self.to_toml()?;
        atomic_write(path, text.as_bytes()).map_err(|source| SettingsError::Write {
            path: path.to_owned(),
            source: Arc::new(source),
        })
    }
}

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    // Span offsets are byte offsets; fall back to the previous char boundary.
    let mut end = offset;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let before = &text[..end];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Write through a symlink rather than replacing it.
    let target: PathBuf = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_owned(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&dir)?;
    let name = target
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
        })?
        .to_string_lossy();
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &target)
    })();

    match result {
        Ok(()) => {
            // Make the rename durable. Opening a directory fails on some
            // platforms; this is best effort.
            if let Ok(d) = fs::File::open(&dir) {
                let _ = d.sync_all();
            }
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ThemeMode;

    fn file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("settings.toml")
    }

    fn custom() -> Settings {
        let mut s = Settings::default();
        s.appearance.font_family = "Fira Code".into();
        s.appearance.font_fallbacks = vec!["Menlo".into()];
        s.appearance.font_size = 16.5;
        s.appearance.line_height = 1.6;
        s.appearance.theme = ThemeMode::Dark;
        s.appearance.theme_name = "nord".into();
        s.chat.scrollback = 250;
        s.chat.timestamps = false;
        s
    }

    #[test]
    fn round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let original = custom();
        original.save(&path).unwrap();
        let outcome = Settings::load_or_default(&path);
        assert!(outcome.error.is_none());
        assert_eq!(outcome.settings, original);
    }

    #[test]
    fn default_round_trips_through_text() {
        let text = Settings::default().to_toml().unwrap();
        let back = Settings::parse(&text, Path::new("x")).unwrap();
        assert_eq!(back, Settings::default());
    }

    #[test]
    fn missing_file_gives_defaults_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = Settings::load_or_default(&file(&dir));
        assert!(outcome.error.is_none());
        assert_eq!(outcome.settings, Settings::default());
    }

    #[test]
    fn empty_file_gives_defaults() {
        let s = Settings::parse("", Path::new("x")).unwrap();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn missing_fields_and_sections_take_defaults() {
        let s = Settings::parse(
            "[appearance]\nfont_size = 18.0\n\n[chat]\ntimestamps = false\n",
            Path::new("x"),
        )
        .unwrap();
        let d = Settings::default();
        assert_eq!(s.appearance.font_size, 18.0);
        assert_eq!(s.appearance.font_family, d.appearance.font_family);
        assert_eq!(s.appearance.theme, d.appearance.theme);
        assert!(!s.chat.timestamps);
        assert_eq!(s.chat.scrollback, d.chat.scrollback);
        assert_eq!(s.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn emote_provider_switches_default_on_and_old_files_still_load() {
        // A file from before the switches existed: `[emotes]` without them.
        let s =
            Settings::parse("[emotes]\nscale = 1.5\nanimate = false\n", Path::new("x")).unwrap();
        assert_eq!(s.emotes.scale, 1.5);
        assert!(s.emotes.seventv && s.emotes.bttv && s.emotes.ffz);

        let s =
            Settings::parse("[emotes]\nseventv = false\nffz = false\n", Path::new("x")).unwrap();
        assert!(!s.emotes.seventv && s.emotes.bttv && !s.emotes.ffz);
        assert_eq!(s.emotes.scale, Settings::default().emotes.scale);
        // And they survive a save.
        let back = Settings::parse(&s.to_toml().unwrap(), Path::new("x")).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn integers_are_accepted_for_float_fields() {
        let s = Settings::parse(
            "[appearance]\nfont_size = 14\nline_height = 2\n",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(s.appearance.font_size, 14.0);
        assert_eq!(s.appearance.line_height, 2.0);
    }

    #[test]
    fn unknown_keys_and_sections_are_ignored() {
        let s = Settings::parse(
            "schema_version = 7\nfuture_top = 1\n[appearance]\nfont_size = 15.0\nglow = true\n\n[from_the_future]\nx = [1, 2]\n",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(s.appearance.font_size, 15.0);
        assert_eq!(s.schema_version, 7);
    }

    #[test]
    fn bad_toml_keeps_defaults_and_reports_location() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let broken = "[appearance]\nfont_size = \n";
        fs::write(&path, broken).unwrap();
        let outcome = Settings::load_or_default(&path);
        assert_eq!(outcome.settings, Settings::default());
        match outcome.error.expect("error reported") {
            SettingsError::Parse { line, .. } => assert!(line.is_some()),
            other => panic!("unexpected error: {other:?}"),
        }
        // Loading must never touch the user's file.
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn wrong_types_are_reported_with_the_line() {
        let err = Settings::parse(
            "[chat]\nscrollback = 10\ntimestamps = \"yes\"\n",
            Path::new("settings.toml"),
        )
        .unwrap_err();
        match &err {
            SettingsError::Parse { line, message, .. } => {
                assert_eq!(*line, Some(3));
                assert!(!message.is_empty());
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(err.to_string().contains("settings.toml:3"));
    }

    #[test]
    fn unknown_theme_mode_falls_back_instead_of_rejecting_the_file() {
        let s = Settings::parse(
            "[appearance]\ntheme = \"high-contrast\"\nfont_size = 17.0\n",
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(s.appearance.theme, ThemeMode::System);
        assert_eq!(s.appearance.font_size, 17.0);
        // Known values still parse as themselves.
        for (text, mode) in [
            ("system", ThemeMode::System),
            ("light", ThemeMode::Light),
            ("dark", ThemeMode::Dark),
        ] {
            let s = Settings::parse(
                &format!("[appearance]\ntheme = \"{text}\"\n"),
                Path::new("x"),
            )
            .unwrap();
            assert_eq!(s.appearance.theme, mode);
        }
        // A non-string value is still a type error.
        let err = Settings::parse("[appearance]\ntheme = 3\n", Path::new("x")).unwrap_err();
        assert!(matches!(err, SettingsError::Parse { .. }));
    }

    #[test]
    fn missing_schema_version_reads_as_the_unversioned_version() {
        let s = Settings::parse("[chat]\nscrollback = 50\n", Path::new("x")).unwrap();
        assert_eq!(s.schema_version, crate::schema::UNVERSIONED_SCHEMA_VERSION);
        assert_eq!(crate::schema::UNVERSIONED_SCHEMA_VERSION, 1);
    }

    #[test]
    fn save_stamps_the_current_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let newer = Settings {
            schema_version: SCHEMA_VERSION + 6,
            ..Settings::default()
        };
        newer.save(&path).unwrap();
        assert_eq!(
            Settings::try_load(&path).unwrap().schema_version,
            SCHEMA_VERSION
        );
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(&format!("schema_version = {SCHEMA_VERSION}"))
        );
    }

    #[test]
    fn out_of_range_values_are_invalid() {
        let err = Settings::parse("[appearance]\nfont_size = -3.0\n", Path::new("x")).unwrap_err();
        assert!(matches!(
            err,
            SettingsError::Invalid {
                field: "appearance.font_size",
                ..
            }
        ));
        let err = Settings::parse("[chat]\nscrollback = -1\n", Path::new("x")).unwrap_err();
        assert!(matches!(err, SettingsError::Parse { .. }));
    }

    #[test]
    fn reload_keeps_last_good_on_error_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let good = custom();
        good.save(&path).unwrap();

        fs::write(&path, "not [valid").unwrap();
        let bad = Settings::reload(&path, &good);
        assert_eq!(bad.settings, good);
        assert!(bad.error.is_some());

        Settings::default().save(&path).unwrap();
        let fixed = Settings::reload(&path, &good);
        assert_eq!(fixed.settings, Settings::default());
        assert!(fixed.error.is_none());
    }

    #[test]
    fn non_utf8_file_is_a_read_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let outcome = Settings::load_or_default(&path);
        assert!(matches!(outcome.error, Some(SettingsError::Read { .. })));
        assert_eq!(outcome.settings, Settings::default());
    }

    #[test]
    fn directory_at_settings_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        fs::create_dir(&path).unwrap();
        assert!(Settings::try_load(&path).is_err());
    }

    #[test]
    fn save_leaves_no_temp_files_and_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        for i in 0..5 {
            let mut s = Settings::default();
            s.chat.scrollback = 100 + i;
            s.save(&path).unwrap();
        }
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["settings.toml"]);
        assert_eq!(Settings::try_load(&path).unwrap().chat.scrollback, 104);
    }

    #[test]
    fn failed_save_cleans_up_and_leaves_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        custom().save(&path).unwrap();
        // A path whose parent is a regular file cannot be written.
        let blocked = path.join("child.toml");
        let err = Settings::default().save(&blocked).unwrap_err();
        assert!(matches!(err, SettingsError::Write { .. }));
        assert_eq!(Settings::try_load(&path).unwrap(), custom());
        let leftovers = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("settings.toml");
        Settings::default().save(&path).unwrap();
        assert!(path.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles").join("settings.toml");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "").unwrap();
        let link = dir.path().join("settings.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        custom().save(&link).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(Settings::try_load(&real).unwrap(), custom());
    }

    #[test]
    fn line_col_counts_characters() {
        assert_eq!(line_col("ab\ncd", 0), (1, 1));
        assert_eq!(line_col("ab\ncd", 4), (2, 2));
        assert_eq!(line_col("é=1\nx", 2), (1, 2));
        assert_eq!(line_col("ab", 99), (1, 3));
    }
}
