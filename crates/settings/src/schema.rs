//! The settings schema.
//!
//! Growth rules (see `docs/SETTINGS.md`):
//! * Every struct is `#[serde(default)]`, so a file may omit any key, section or
//!   the whole file. New fields therefore never break existing files.
//! * Unknown keys are **ignored** on load (no `deny_unknown_fields`), so a file
//!   written by a newer build still loads in an older one. They are not
//!   preserved when the app saves.
//! * Bump [`SCHEMA_VERSION`] only for changes a default cannot express
//!   (renames, changed meaning). Adding a field is not such a change.

use serde::{Deserialize, Serialize};

/// Version written to new files. Files without `schema_version` are treated as
/// version 1.
pub const SCHEMA_VERSION: u32 = 1;

/// What a file with no `schema_version` key is taken to be. Deliberately a
/// separate constant from [`SCHEMA_VERSION`]: when the schema moves to 2, an
/// old unversioned file must still read as 1 so migrations run on it.
pub const UNVERSIONED_SCHEMA_VERSION: u32 = 1;

fn unversioned_schema_version() -> u32 {
    UNVERSIONED_SCHEMA_VERSION
}

/// Allowed range for [`AppearanceSettings::font_size`] (points).
pub(crate) const FONT_SIZE_RANGE: (f32, f32) = (6.0, 72.0);
/// Allowed range for [`AppearanceSettings::line_height`] (multiplier).
pub(crate) const LINE_HEIGHT_RANGE: (f32, f32) = (1.0, 3.0);
/// Allowed range for [`ChatSettings::scrollback`] (messages per channel).
pub(crate) const SCROLLBACK_RANGE: (usize, usize) = (10, 100_000);
/// Allowed range for [`EmoteSettings::scale`] (multiplier).
pub(crate) const EMOTE_SCALE_RANGE: (f32, f32) = (0.5, 4.0);

/// Top-level settings, stored as `settings.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Schema version of the file. See [`SCHEMA_VERSION`]. A file without this
    /// key reads as [`UNVERSIONED_SCHEMA_VERSION`]. `save` always writes the
    /// current [`SCHEMA_VERSION`].
    #[serde(default = "unversioned_schema_version")]
    pub schema_version: u32,
    pub appearance: AppearanceSettings,
    pub chat: ChatSettings,
    pub emotes: EmoteSettings,
    // Later phases add sections here, each `#[serde(default)]`:
    // streamlink (1.6: binary, player, quality,
    // extra args), account (3.x: non-secret bits only; tokens live in the keychain).
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            appearance: AppearanceSettings::default(),
            chat: ChatSettings::default(),
            emotes: EmoteSettings::default(),
        }
    }
}

/// `[appearance]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    /// Preferred font family, the head of the font chain. Default is
    /// `JetBrainsMono Nerd Font` (the Omarchy default), used if installed.
    pub font_family: String,
    /// Families tried, in order, when `font_family` is not installed. The
    /// default is the bundled `JetBrains Mono`, then the system monospace font.
    pub font_fallbacks: Vec<String>,
    /// Font size in points. Valid range 6 to 72.
    pub font_size: f32,
    /// Line height as a multiple of the font size. Valid range 1.0 to 3.0.
    pub line_height: f32,
    /// Light, dark, or follow the operating system.
    pub theme: ThemeMode,
    /// Name of the colour theme (bundled or `themes/<name>.toml`). Only
    /// `"default"` exists until PR 4.4; unknown names are the app's to resolve.
    pub theme_name: String,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            font_family: "JetBrainsMono Nerd Font".to_owned(),
            font_fallbacks: vec!["JetBrains Mono".to_owned(), "monospace".to_owned()],
            font_size: 14.0,
            line_height: 1.4,
            theme: ThemeMode::default(),
            theme_name: "default".to_owned(),
        }
    }
}

impl AppearanceSettings {
    /// `font_family` followed by `font_fallbacks`, for the app to resolve
    /// against installed fonts.
    pub fn font_chain(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.font_family.as_str())
            .chain(self.font_fallbacks.iter().map(String::as_str))
    }
}

/// `appearance.theme`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    Dark,
    /// Follow the operating system appearance. Also the fallback for any
    /// value this build does not know (e.g. a mode added by a newer version),
    /// so such a file still loads instead of being rejected. Serde requires
    /// the `other` variant to be last.
    #[default]
    #[serde(other)]
    System,
}

/// `[chat]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatSettings {
    /// Maximum messages kept per channel. Valid range 10 to 100000.
    pub scrollback: usize,
    /// Show a timestamp before each message.
    pub timestamps: bool,
}

impl Default for ChatSettings {
    fn default() -> Self {
        Self {
            scrollback: 1000,
            timestamps: true,
        }
    }
}

/// `[emotes]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmoteSettings {
    /// Size of emotes relative to their provider's base size (a Twitch emote is
    /// 28 px at 1.0). Also picks which resolution is downloaded. Valid range
    /// 0.5 to 4.0.
    pub scale: f32,
    /// Play animated emotes. When off they show their first frame.
    pub animate: bool,
}

impl Default for EmoteSettings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            animate: true,
        }
    }
}

impl Settings {
    /// Check value ranges. Returns `(field, reason)` for the first problem.
    ///
    /// Parsing accepts any value of the right type; this is the semantic check
    /// run after parsing (and available to the settings UI before saving).
    pub fn validate(&self) -> Result<(), (&'static str, String)> {
        let a = &self.appearance;
        if a.font_family.trim().is_empty() {
            return Err(("appearance.font_family", "must not be empty".into()));
        }
        if !in_range_f32(a.font_size, FONT_SIZE_RANGE) {
            return Err((
                "appearance.font_size",
                format!(
                    "{} is outside {} to {}",
                    a.font_size, FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1
                ),
            ));
        }
        if !in_range_f32(a.line_height, LINE_HEIGHT_RANGE) {
            return Err((
                "appearance.line_height",
                format!(
                    "{} is outside {} to {}",
                    a.line_height, LINE_HEIGHT_RANGE.0, LINE_HEIGHT_RANGE.1
                ),
            ));
        }
        if a.theme_name.trim().is_empty() {
            return Err(("appearance.theme_name", "must not be empty".into()));
        }
        let c = &self.chat;
        if !(SCROLLBACK_RANGE.0..=SCROLLBACK_RANGE.1).contains(&c.scrollback) {
            return Err((
                "chat.scrollback",
                format!(
                    "{} is outside {} to {}",
                    c.scrollback, SCROLLBACK_RANGE.0, SCROLLBACK_RANGE.1
                ),
            ));
        }
        let e = &self.emotes;
        if !in_range_f32(e.scale, EMOTE_SCALE_RANGE) {
            return Err((
                "emotes.scale",
                format!(
                    "{} is outside {} to {}",
                    e.scale, EMOTE_SCALE_RANGE.0, EMOTE_SCALE_RANGE.1
                ),
            ));
        }
        Ok(())
    }
}

fn in_range_f32(v: f32, (lo, hi): (f32, f32)) -> bool {
    v.is_finite() && (lo..=hi).contains(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_plan() {
        let s = Settings::default();
        assert_eq!(s.schema_version, SCHEMA_VERSION);
        assert_eq!(s.appearance.font_family, "JetBrainsMono Nerd Font");
        assert_eq!(s.appearance.theme, ThemeMode::System);
        assert_eq!(s.chat.scrollback, 1000);
        assert_eq!((s.emotes.scale, s.emotes.animate), (1.0, true));
        assert!(s.validate().is_ok());
        let chain: Vec<_> = s.appearance.font_chain().collect();
        assert_eq!(
            chain,
            ["JetBrainsMono Nerd Font", "JetBrains Mono", "monospace"]
        );
    }

    #[test]
    fn validate_rejects_out_of_range_values() {
        let mut s = Settings::default();
        s.appearance.font_size = 0.0;
        assert_eq!(s.validate().unwrap_err().0, "appearance.font_size");
        s.appearance.font_size = f32::NAN;
        assert_eq!(s.validate().unwrap_err().0, "appearance.font_size");

        let mut s = Settings::default();
        s.appearance.line_height = 9.0;
        assert_eq!(s.validate().unwrap_err().0, "appearance.line_height");

        let mut s = Settings::default();
        s.chat.scrollback = 0;
        assert_eq!(s.validate().unwrap_err().0, "chat.scrollback");

        let mut s = Settings::default();
        s.emotes.scale = 8.0;
        assert_eq!(s.validate().unwrap_err().0, "emotes.scale");

        let mut s = Settings::default();
        s.appearance.font_family = "  ".into();
        assert_eq!(s.validate().unwrap_err().0, "appearance.font_family");
    }
}
