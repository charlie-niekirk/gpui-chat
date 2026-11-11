use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gpui_chat_core::APP_NAME;

/// Environment variable that overrides the config directory.
pub const CONFIG_DIR_ENV: &str = "GPUI_CHAT_CONFIG_DIR";
/// Environment variable that overrides the cache directory.
pub const CACHE_DIR_ENV: &str = "GPUI_CHAT_CACHE_DIR";

/// File name of the settings file inside the config dir.
pub const SETTINGS_FILE_NAME: &str = "settings.toml";
/// File name of the keymap file inside the config dir (used from PR 4.3).
pub const KEYMAP_FILE_NAME: &str = "keymap.toml";
/// Directory of user themes inside the config dir (used from PR 4.4).
pub const THEMES_DIR_NAME: &str = "themes";

/// Locations the app reads from and writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsPaths {
    config_dir: PathBuf,
    cache_dir: PathBuf,
}

/// The default locations could not be determined.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathsError {
    #[error("could not determine the home directory; set {CONFIG_DIR_ENV}")]
    NoHomeDir,
    #[error("could not determine the cache directory; set {CACHE_DIR_ENV}")]
    NoCacheDir,
}

impl SettingsPaths {
    /// Explicit locations (tests, or callers that already resolved them).
    pub fn new(config_dir: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            cache_dir: cache_dir.into(),
        }
    }

    /// Everything under one base directory: `<base>/config` and `<base>/cache`.
    /// Convenient for tests and sandboxes.
    pub fn with_base_dir(base: impl AsRef<Path>) -> Self {
        let base = base.as_ref();
        Self::new(base.join("config"), base.join("cache"))
    }

    /// Resolve the real locations for this machine.
    ///
    /// * Config: `$GPUI_CHAT_CONFIG_DIR`, else `$XDG_CONFIG_HOME/<APP_NAME>` if
    ///   set to an absolute path, else `~/.config/<APP_NAME>`. The same rule on
    ///   macOS and Linux (PLAN: `~/.config/<app>/`, not `~/Library/...`).
    /// * Cache: `$GPUI_CHAT_CACHE_DIR`, else the platform cache dir
    ///   (`dirs::cache_dir()`: `~/Library/Caches` / `$XDG_CACHE_HOME`) joined
    ///   with `<APP_NAME>`.
    pub fn from_env() -> Result<Self, PathsError> {
        Self::resolve(
            |name| std::env::var_os(name),
            dirs::home_dir(),
            dirs::cache_dir(),
        )
    }

    /// [`from_env`](Self::from_env) with the environment and the `dirs`
    /// results injected, so the rules are testable without touching the
    /// process environment.
    pub(crate) fn resolve(
        env: impl Fn(&str) -> Option<OsString>,
        home: Option<PathBuf>,
        platform_cache: Option<PathBuf>,
    ) -> Result<Self, PathsError> {
        let non_empty = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);

        let config_dir = if let Some(dir) = non_empty(CONFIG_DIR_ENV) {
            dir
        } else if let Some(xdg) = non_empty("XDG_CONFIG_HOME").filter(|p| p.is_absolute()) {
            xdg.join(APP_NAME)
        } else {
            home.ok_or(PathsError::NoHomeDir)?
                .join(".config")
                .join(APP_NAME)
        };

        let cache_dir = if let Some(dir) = non_empty(CACHE_DIR_ENV) {
            dir
        } else {
            platform_cache.ok_or(PathsError::NoCacheDir)?.join(APP_NAME)
        };

        Ok(Self {
            config_dir,
            cache_dir,
        })
    }

    /// `~/.config/<APP_NAME>/` (or the override).
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Platform cache directory for the app (images, emote sets, ...).
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// `<config_dir>/settings.toml`.
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join(SETTINGS_FILE_NAME)
    }

    /// `<config_dir>/keymap.toml`.
    pub fn keymap_file(&self) -> PathBuf {
        self.config_dir.join(KEYMAP_FILE_NAME)
    }

    /// `<config_dir>/themes/`.
    pub fn themes_dir(&self) -> PathBuf {
        self.config_dir.join(THEMES_DIR_NAME)
    }

    /// Create the config directory (and parents) if missing.
    pub fn ensure_config_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)
    }

    /// Create the cache directory (and parents) if missing.
    pub fn ensure_cache_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.cache_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs = pairs.to_vec();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn default_config_dir_is_dot_config_under_home() {
        let p = SettingsPaths::resolve(
            env_of(&[]),
            Some("/home/u".into()),
            Some("/home/u/.cache".into()),
        )
        .unwrap();
        assert_eq!(p.config_dir(), Path::new("/home/u/.config").join(APP_NAME));
        assert_eq!(p.cache_dir(), Path::new("/home/u/.cache").join(APP_NAME));
        assert_eq!(p.settings_file().file_name().unwrap(), "settings.toml");
        assert_eq!(p.keymap_file().file_name().unwrap(), "keymap.toml");
        assert_eq!(p.themes_dir().file_name().unwrap(), "themes");
        assert_eq!(p.settings_file().parent().unwrap(), p.config_dir());
    }

    #[test]
    fn xdg_config_home_is_honoured_only_when_absolute() {
        let abs = SettingsPaths::resolve(
            env_of(&[("XDG_CONFIG_HOME", "/x/cfg")]),
            Some("/home/u".into()),
            Some("/c".into()),
        )
        .unwrap();
        assert_eq!(abs.config_dir(), Path::new("/x/cfg").join(APP_NAME));

        let rel = SettingsPaths::resolve(
            env_of(&[("XDG_CONFIG_HOME", "relative")]),
            Some("/home/u".into()),
            Some("/c".into()),
        )
        .unwrap();
        assert_eq!(
            rel.config_dir(),
            Path::new("/home/u/.config").join(APP_NAME)
        );
    }

    #[test]
    fn env_overrides_win_and_are_used_verbatim() {
        let p = SettingsPaths::resolve(
            env_of(&[
                (CONFIG_DIR_ENV, "/override/config"),
                (CACHE_DIR_ENV, "/override/cache"),
                ("XDG_CONFIG_HOME", "/ignored"),
            ]),
            None,
            None,
        )
        .unwrap();
        assert_eq!(p.config_dir(), Path::new("/override/config"));
        assert_eq!(p.cache_dir(), Path::new("/override/cache"));
    }

    #[test]
    fn empty_override_is_ignored() {
        let p = SettingsPaths::resolve(
            env_of(&[(CONFIG_DIR_ENV, "")]),
            Some("/h".into()),
            Some("/c".into()),
        )
        .unwrap();
        assert_eq!(p.config_dir(), Path::new("/h/.config").join(APP_NAME));
    }

    #[test]
    fn missing_home_and_cache_are_errors() {
        assert_eq!(
            SettingsPaths::resolve(env_of(&[]), None, Some("/c".into())),
            Err(PathsError::NoHomeDir)
        );
        assert_eq!(
            SettingsPaths::resolve(env_of(&[]), Some("/h".into()), None),
            Err(PathsError::NoCacheDir)
        );
    }

    #[test]
    fn with_base_dir_nests_config_and_cache() {
        let p = SettingsPaths::with_base_dir("/b");
        assert_eq!(p.config_dir(), Path::new("/b/config"));
        assert_eq!(p.cache_dir(), Path::new("/b/cache"));
    }

    #[test]
    fn ensure_dirs_creates_them() {
        let tmp = tempfile::tempdir().unwrap();
        let p = SettingsPaths::with_base_dir(tmp.path());
        p.ensure_config_dir().unwrap();
        p.ensure_cache_dir().unwrap();
        assert!(p.config_dir().is_dir());
        assert!(p.cache_dir().is_dir());
    }
}
