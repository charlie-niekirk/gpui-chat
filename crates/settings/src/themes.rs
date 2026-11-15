//! The set of known themes: the bundled pack, the user's `themes/*.toml`, and picking the one
//! the settings ask for.

use std::path::Path;

use crate::schema::{AppearanceSettings, ThemeMode};
use crate::theme::{Palette, Theme, ThemeError, ThemeOrigin, ThemeVariant, normalize_id};

/// Id of the built-in theme, the fallback for an unknown `theme_name`.
pub const DEFAULT_THEME_ID: &str = "default";

macro_rules! bundled {
    ($($file:literal),+ $(,)?) => {
        &[$(($file, include_str!(concat!("../../../assets/themes/", $file, ".toml")))),+]
    };
}

/// The bundled pack: Omarchy's stock set, plus the built-in default. `(file stem, text)`.
const BUNDLED: &[(&str, &str)] = bundled![
    "default",
    "catppuccin",
    "ethereal",
    "everforest",
    "flexoki-light",
    "gruvbox",
    "hackerman",
    "jade-horse",
    "kanagawa",
    "last-horizon",
    "lumon",
    "lupine",
    "matte-black",
    "miasma",
    "nord",
    "retro-82",
    "ristretto",
    "rose-pine",
    "solitude",
    "starship",
    "tokyo-night",
    "vantablack",
    "white",
];

/// Every bundled theme. The files are compiled in and checked by tests, so a failure here is
/// a bug; such a theme is skipped (and logged) rather than taking the app down.
pub fn bundled_themes() -> Vec<Theme> {
    BUNDLED
        .iter()
        .filter_map(
            |(stem, text)| match Theme::parse(stem, stem, text, ThemeOrigin::Bundled) {
                Ok(theme) => Some(theme),
                Err(error) => {
                    tracing::error!(%error, "bundled theme is invalid");
                    None
                }
            },
        )
        .collect()
}

/// The user themes found in a directory, and what could not be loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadReport {
    pub themes: Vec<Theme>,
    pub errors: Vec<ThemeError>,
}

/// Loads `dir/*.toml`. A missing directory is just "no user themes". A file that fails to
/// parse is reported in `errors`, and if `previous` holds a theme from that same file, that
/// last good version stays in effect (so saving a half-edited theme does not flash the UI back
/// to the default).
pub fn load_user_themes(dir: &Path, previous: &[Theme]) -> LoadReport {
    let mut report = LoadReport::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return report,
        Err(error) => {
            report.errors.push(ThemeError::Read {
                source_name: dir.display().to_string(),
                message: error.to_string(),
            });
            return report;
        }
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
                && path
                    .file_name()
                    .is_some_and(|name| !name.to_string_lossy().starts_with('.'))
                && path.is_file()
        })
        .collect();
    files.sort();
    for path in files {
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let shown = path.display().to_string();
        let parsed = std::fs::read_to_string(&path)
            .map_err(|error| ThemeError::Read {
                source_name: shown.clone(),
                message: error.to_string(),
            })
            .and_then(|text| Theme::parse(&stem, &shown, &text, ThemeOrigin::User(path.clone())));
        match parsed {
            Ok(theme) => report.themes.push(theme),
            Err(error) => {
                if let Some(old) = previous
                    .iter()
                    .find(|t| t.origin == ThemeOrigin::User(path.clone()))
                {
                    report.themes.push(old.clone());
                }
                report.errors.push(error);
            }
        }
    }
    report
}

/// The theme and variant the settings select, with its palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTheme {
    pub id: String,
    pub name: String,
    /// The variant whose palette is used. Can differ from the wanted one when the theme only
    /// has the other.
    pub variant: ThemeVariant,
    pub palette: Palette,
    /// Set when `theme_name` named no known theme and the default was used instead.
    pub unknown_name: Option<String>,
}

/// All themes known to the app. User themes with the id of a bundled one replace it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeRegistry {
    themes: Vec<Theme>,
}

impl ThemeRegistry {
    /// Just the bundled pack.
    pub fn bundled() -> Self {
        Self::with_user_themes(Vec::new())
    }

    /// The bundled pack plus `user` themes (which win on an id clash), listed with the default
    /// first and the rest by name.
    pub fn with_user_themes(user: Vec<Theme>) -> Self {
        let mut themes = bundled_themes();
        for theme in user {
            themes.retain(|t| t.id != theme.id);
            themes.push(theme);
        }
        themes.sort_by_key(|t| {
            (
                t.id != DEFAULT_THEME_ID,
                t.name.to_lowercase(),
                t.id.clone(),
            )
        });
        Self { themes }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Theme> {
        self.themes.iter()
    }

    /// Finds a theme by `theme_name` (see [`normalize_id`]).
    pub fn get(&self, name: &str) -> Option<&Theme> {
        let id = normalize_id(name);
        self.themes.iter().find(|t| t.id == id)
    }

    /// Picks the palette for `appearance`. `system` is the operating system's variant, used
    /// when `appearance.theme` is `system`.
    ///
    /// The wanted variant is the pinned one (`light` / `dark`) or `system`. A light wanted
    /// variant uses `theme_name_light` if set, else `theme_name`. A theme without the wanted
    /// variant still wins: its only palette is used.
    pub fn resolve(&self, appearance: &AppearanceSettings, system: ThemeVariant) -> ResolvedTheme {
        let want = match appearance.theme {
            ThemeMode::Dark => ThemeVariant::Dark,
            ThemeMode::Light => ThemeVariant::Light,
            ThemeMode::System => system,
        };
        let requested = match (want, appearance.theme_name_light.as_deref()) {
            (ThemeVariant::Light, Some(light)) => light,
            _ => appearance.theme_name.as_str(),
        };
        let (theme, unknown_name) = match self.get(requested) {
            Some(theme) => (theme, None),
            None => (
                self.get(DEFAULT_THEME_ID)
                    .or_else(|| self.themes.first())
                    .expect("the bundled default theme exists"),
                Some(requested.to_owned()),
            ),
        };
        let (variant, palette) = theme.palette(want);
        ResolvedTheme {
            id: theme.id.clone(),
            name: theme.name.clone(),
            variant,
            palette,
            unknown_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Color;

    fn appearance(mode: ThemeMode, name: &str, light: Option<&str>) -> AppearanceSettings {
        AppearanceSettings {
            theme: mode,
            theme_name: name.into(),
            theme_name_light: light.map(str::to_owned),
            ..AppearanceSettings::default()
        }
    }

    #[test]
    fn every_bundled_file_parses_and_ids_are_unique() {
        let themes = bundled_themes();
        assert_eq!(
            themes.len(),
            BUNDLED.len(),
            "a bundled theme failed to parse"
        );
        let mut ids: Vec<_> = themes.iter().map(|t| t.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), themes.len());
        for needed in [
            "default",
            "tokyo-night",
            "catppuccin",
            "gruvbox",
            "nord",
            "everforest",
            "kanagawa",
            "rose-pine",
        ] {
            assert!(ids.iter().any(|id| id == needed), "{needed} is not bundled");
        }
    }

    #[test]
    fn bundled_palettes_are_readable() {
        for theme in bundled_themes() {
            for (variant, palette) in [("dark", theme.dark), ("light", theme.light)]
                .into_iter()
                .filter_map(|(v, p)| p.map(|p| (v, p)))
            {
                let at = format!("{} {variant}", theme.id);
                assert!(
                    palette.text.contrast(palette.background) >= 4.5,
                    "{at}: text on background"
                );
                assert!(
                    palette.text_muted.contrast(palette.background) >= 3.0,
                    "{at}: muted text on background"
                );
                assert!(
                    palette.accent_text.contrast(palette.accent) >= 3.0,
                    "{at}: text on accent"
                );
                assert!(
                    palette.accent.contrast(palette.background) >= 1.8,
                    "{at}: accent on background"
                );
                // The variant must look like its name.
                let dark = palette.background.luminance() < 0.2;
                assert_eq!(dark, variant == "dark", "{at}: background luminance");
            }
        }
    }

    #[test]
    fn the_default_theme_keeps_the_original_colours() {
        let registry = ThemeRegistry::bundled();
        let dark = registry.get("default").unwrap().dark.unwrap();
        assert_eq!(dark.background, Color::from_u32(0x0e0e10));
        assert_eq!(dark.accent, Color::from_u32(0x9147ff));
        let light = registry.get("default").unwrap().light.unwrap();
        assert_eq!(light.background, Color::from_u32(0xffffff));
    }

    #[test]
    fn follow_system_picks_the_variant_of_the_os() {
        let registry = ThemeRegistry::bundled();
        let sel = appearance(ThemeMode::System, "catppuccin", None);
        let dark = registry.resolve(&sel, ThemeVariant::Dark);
        let light = registry.resolve(&sel, ThemeVariant::Light);
        assert_eq!(
            (dark.variant, light.variant),
            (ThemeVariant::Dark, ThemeVariant::Light)
        );
        assert_eq!(dark.palette.background, Color::from_u32(0x1e1e2e));
        assert_eq!(light.palette.background, Color::from_u32(0xeff1f5));
        // A pinned mode ignores the OS.
        let pinned = appearance(ThemeMode::Dark, "catppuccin", None);
        assert_eq!(
            registry.resolve(&pinned, ThemeVariant::Light).variant,
            ThemeVariant::Dark
        );
    }

    #[test]
    fn theme_name_light_pairs_two_themes_for_follow_system() {
        let registry = ThemeRegistry::bundled();
        let sel = appearance(ThemeMode::System, "tokyo-night", Some("Rose Pine"));
        assert_eq!(registry.resolve(&sel, ThemeVariant::Dark).id, "tokyo-night");
        let day = registry.resolve(&sel, ThemeVariant::Light);
        assert_eq!(
            (day.id.as_str(), day.variant),
            ("rose-pine", ThemeVariant::Light)
        );
        // Pinned dark never looks at the light name.
        let pinned = appearance(ThemeMode::Dark, "tokyo-night", Some("rose-pine"));
        assert_eq!(
            registry.resolve(&pinned, ThemeVariant::Light).id,
            "tokyo-night"
        );
    }

    #[test]
    fn a_single_variant_theme_wins_over_the_os() {
        let registry = ThemeRegistry::bundled();
        let sel = appearance(ThemeMode::System, "nord", None);
        let resolved = registry.resolve(&sel, ThemeVariant::Light);
        assert_eq!(
            (resolved.id.as_str(), resolved.variant),
            ("nord", ThemeVariant::Dark)
        );
        assert!(resolved.unknown_name.is_none());
    }

    #[test]
    fn unknown_names_fall_back_to_the_default() {
        let registry = ThemeRegistry::bundled();
        let resolved = registry.resolve(
            &appearance(ThemeMode::Dark, "no-such-theme", None),
            ThemeVariant::Dark,
        );
        assert_eq!(resolved.id, DEFAULT_THEME_ID);
        assert_eq!(resolved.unknown_name.as_deref(), Some("no-such-theme"));
    }

    #[test]
    fn user_themes_are_listed_and_override_bundled_ones() {
        let user = Theme::parse(
            "Nord",
            "Nord.toml",
            "name = \"My Nord\"\n[dark]\nbackground=\"#000\"\ntext=\"#fff\"\naccent=\"#f00\"\n",
            ThemeOrigin::User("Nord.toml".into()),
        )
        .unwrap();
        let registry = ThemeRegistry::with_user_themes(vec![user]);
        assert_eq!(registry.get("nord").unwrap().name, "My Nord");
        assert_eq!(registry.iter().filter(|t| t.id == "nord").count(), 1);
        assert_eq!(registry.iter().next().unwrap().id, DEFAULT_THEME_ID);
    }

    #[test]
    fn user_theme_directory_is_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let good = "[dark]\nbackground=\"#000\"\ntext=\"#fff\"\naccent=\"#0f0\"\n";
        std::fs::write(dir.path().join("mine.toml"), good).unwrap();
        std::fs::write(dir.path().join("broken.toml"), "[dark\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        std::fs::write(dir.path().join(".hidden.toml"), good).unwrap();

        let report = load_user_themes(dir.path(), &[]);
        let ids: Vec<_> = report.themes.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["mine"]);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].to_string().contains("broken.toml"));

        // Once a file has loaded, a later bad edit keeps the last good version.
        std::fs::write(dir.path().join("mine.toml"), "[dark\n").unwrap();
        let again = load_user_themes(dir.path(), &report.themes);
        assert_eq!(again.themes, report.themes);
        assert_eq!(again.errors.len(), 2);

        assert_eq!(
            load_user_themes(&dir.path().join("missing"), &[]),
            LoadReport::default()
        );
    }
}
