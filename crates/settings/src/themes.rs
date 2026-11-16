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

/// Bundled names that are another theme's variant under the name Omarchy gives it:
/// `(alias id, display name, target id, variant)`. The alias is a one-variant theme.
const ALIASES: &[(&str, &str, &str, ThemeVariant)] = &[(
    "catppuccin-latte",
    "Catppuccin Latte",
    "catppuccin",
    ThemeVariant::Light,
)];

/// Every bundled theme. The files are compiled in and checked by tests, so a failure here is
/// a bug; such a theme is skipped (and logged) rather than taking the app down.
pub fn bundled_themes() -> Vec<Theme> {
    let mut themes = parse_bundled();
    for (id, name, target, variant) in ALIASES {
        let Some(target) = themes.iter().find(|t| t.id == *target) else {
            continue;
        };
        let palette = match variant {
            ThemeVariant::Dark => target.dark,
            ThemeVariant::Light => target.light,
        };
        let alias = Theme {
            id: (*id).to_owned(),
            name: (*name).to_owned(),
            dark: palette.filter(|_| *variant == ThemeVariant::Dark),
            light: palette.filter(|_| *variant == ThemeVariant::Light),
            origin: ThemeOrigin::Bundled,
        };
        if alias.dark.is_some() || alias.light.is_some() {
            themes.push(alias);
        }
    }
    themes
}

fn parse_bundled() -> Vec<Theme> {
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
            Ok(theme) => {
                for (variant, palette) in [
                    (ThemeVariant::Dark, theme.dark),
                    (ThemeVariant::Light, theme.light),
                ] {
                    let Some(palette) = palette else { continue };
                    let ratio = palette.text.contrast(palette.background);
                    if ratio < MIN_USER_CONTRAST {
                        report.errors.push(ThemeError::LowContrast {
                            source_name: shown.clone(),
                            variant: variant.key(),
                            ratio: format!("{ratio:.1}"),
                        });
                    }
                }
                // The file that sorts later wins; the earlier one is reported and dropped.
                if let Some(index) = report.themes.iter().position(|t| t.id == theme.id) {
                    let earlier = report.themes.remove(index);
                    report.errors.push(ThemeError::DuplicateId {
                        source_name: origin_name(&earlier),
                        kept: shown.clone(),
                        id: theme.id.clone(),
                    });
                }
                report.themes.push(theme);
            }
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

/// Below this text/background contrast a user theme gets a warning.
const MIN_USER_CONTRAST: f32 = 3.0;

fn origin_name(theme: &Theme) -> String {
    match &theme.origin {
        ThemeOrigin::User(path) => path.display().to_string(),
        ThemeOrigin::Bundled => theme.id.clone(),
    }
}

/// A theme name in the settings that matches no theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTheme {
    /// `"appearance.theme_name"` or `"appearance.theme_name_light"`.
    pub setting: &'static str,
    pub name: String,
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
    /// Names in the settings that matched no theme (and were replaced by a fallback).
    pub unknown: Vec<UnknownTheme>,
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
        for mut theme in user {
            // A user theme replaces the bundled one of the same id, but keeps the variant it
            // does not define, so a dark-only `catppuccin.toml` still follows the OS to Latte.
            if let Some(bundled) = themes.iter().find(|t| t.id == theme.id) {
                theme.dark = theme.dark.or(bundled.dark);
                theme.light = theme.light.or(bundled.light);
            }
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
        let mut unknown = Vec::new();
        let mut lookup = |setting: &'static str, name: &str| {
            let found = self.get(name);
            if found.is_none() {
                unknown.push(UnknownTheme {
                    setting,
                    name: name.to_owned(),
                });
            }
            found
        };
        // A light variant prefers `theme_name_light`, then `theme_name`, then the default.
        let light_choice = (want == ThemeVariant::Light)
            .then_some(appearance.theme_name_light.as_deref())
            .flatten()
            .and_then(|name| lookup("appearance.theme_name_light", name));
        let theme = light_choice
            .or_else(|| lookup("appearance.theme_name", &appearance.theme_name))
            .or_else(|| self.get(DEFAULT_THEME_ID))
            .or_else(|| self.themes.first())
            .expect("the bundled default theme exists");
        let (variant, palette) = theme.palette(want);
        ResolvedTheme {
            id: theme.id.clone(),
            name: theme.name.clone(),
            variant,
            palette,
            unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Color, TEXT_CONTRAST};

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
            BUNDLED.len() + ALIASES.len(),
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
                // Every colour the app draws text in must reach 4.5:1 (WCAG AA) on every surface
                // it can sit on (PR 6.3): body and muted text on all five surfaces, the text on an
                // accent fill (buttons, selected rows) on that fill.
                for (surface_name, surface) in [
                    ("background", palette.background),
                    ("window", palette.window),
                    ("title_bar", palette.title_bar),
                    ("sidebar", palette.sidebar),
                    ("surface", palette.surface),
                ] {
                    for (what, colour) in
                        [("text", palette.text), ("text_muted", palette.text_muted)]
                    {
                        let ratio = colour.contrast(surface);
                        assert!(
                            ratio >= TEXT_CONTRAST,
                            "{at}: {what} on {surface_name} is {ratio:.2}:1"
                        );
                    }
                }
                let ratio = palette.accent_text.contrast(palette.accent);
                assert!(
                    ratio >= TEXT_CONTRAST,
                    "{at}: text on accent is {ratio:.2}:1"
                );
                assert!(
                    palette.accent.contrast(palette.background) >= 1.8,
                    "{at}: accent on background"
                );
                // Status colours used as text must be readable on every surface they sit on,
                // plain and under a tint.
                for (what, colour) in [
                    ("danger_text", palette.danger_text),
                    ("accent_link", palette.accent_link),
                ] {
                    let tint_source = if what == "danger_text" {
                        palette.danger
                    } else {
                        palette.accent
                    };
                    for (surface_name, surface) in [
                        ("background", palette.background),
                        ("window", palette.window),
                        ("title_bar", palette.title_bar),
                        ("sidebar", palette.sidebar),
                        ("surface", palette.surface),
                    ] {
                        for (tint, bg) in [
                            ("plain", surface),
                            ("tinted", surface.mix(tint_source, 0.18)),
                        ] {
                            let ratio = colour.contrast(bg);
                            assert!(
                                ratio >= TEXT_CONTRAST,
                                "{at}: {what} on {tint} {surface_name} is {ratio:.2}:1"
                            );
                        }
                    }
                }
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
        assert!(resolved.unknown.is_empty());
    }

    #[test]
    fn unknown_names_fall_back_to_the_default() {
        let registry = ThemeRegistry::bundled();
        let resolved = registry.resolve(
            &appearance(ThemeMode::Dark, "no-such-theme", None),
            ThemeVariant::Dark,
        );
        assert_eq!(resolved.id, DEFAULT_THEME_ID);
        assert_eq!(
            resolved.unknown,
            [UnknownTheme {
                setting: "appearance.theme_name",
                name: "no-such-theme".into()
            }]
        );
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

    const GOOD: &str = "[dark]\nbackground=\"#000\"\ntext=\"#fff\"\naccent=\"#0f0\"\n";

    #[test]
    fn a_user_file_overriding_a_two_variant_theme_keeps_the_other_variant() {
        let bundled = ThemeRegistry::bundled();
        let latte = bundled.get("catppuccin").unwrap().light.unwrap();
        let user = Theme::parse(
            "catppuccin",
            "catppuccin.toml",
            GOOD,
            ThemeOrigin::User("catppuccin.toml".into()),
        )
        .unwrap();
        let registry = ThemeRegistry::with_user_themes(vec![user]);
        let theme = registry.get("catppuccin").unwrap();
        assert_eq!(
            theme.dark.unwrap().background,
            Color::BLACK,
            "user dark wins"
        );
        assert_eq!(theme.light, Some(latte), "Latte is kept for follow-system");
        let sel = appearance(ThemeMode::System, "catppuccin", None);
        assert_eq!(
            registry
                .resolve(&sel, ThemeVariant::Light)
                .palette
                .background,
            Color::from_u32(0xeff1f5)
        );
    }

    #[test]
    fn catppuccin_latte_is_the_light_catppuccin() {
        let registry = ThemeRegistry::bundled();
        let latte = registry.get("catppuccin-latte").unwrap();
        assert!(latte.dark.is_none());
        assert_eq!(latte.light, registry.get("catppuccin").unwrap().light);
        // Even with a dark OS the Latte alias stays light.
        let resolved = registry.resolve(
            &appearance(ThemeMode::System, "catppuccin-latte", None),
            ThemeVariant::Dark,
        );
        assert_eq!(resolved.variant, ThemeVariant::Light);
    }

    #[test]
    fn an_unknown_light_name_falls_back_to_theme_name_before_the_default() {
        let registry = ThemeRegistry::bundled();
        let sel = appearance(ThemeMode::Light, "catppuccin", Some("not-a-theme"));
        let resolved = registry.resolve(&sel, ThemeVariant::Light);
        assert_eq!(
            (resolved.id.as_str(), resolved.variant),
            ("catppuccin", ThemeVariant::Light)
        );
        assert_eq!(
            resolved.unknown,
            [UnknownTheme {
                setting: "appearance.theme_name_light",
                name: "not-a-theme".into()
            }]
        );
        // Both unknown: the default, and both are reported.
        let both = appearance(ThemeMode::Light, "nope", Some("nada"));
        let resolved = registry.resolve(&both, ThemeVariant::Light);
        assert_eq!(resolved.id, DEFAULT_THEME_ID);
        assert_eq!(resolved.unknown.len(), 2);
        // The light name is not consulted for a dark variant.
        let dark = registry.resolve(
            &appearance(ThemeMode::Dark, "nord", Some("nada")),
            ThemeVariant::Dark,
        );
        assert!(dark.unknown.is_empty());
    }

    #[test]
    fn two_user_files_with_one_id_are_reported_and_the_later_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Tokyo Night.toml"), GOOD).unwrap();
        std::fs::write(
            dir.path().join("tokyo_night.toml"),
            GOOD.replace("#0f0", "#f00"),
        )
        .unwrap();
        let report = load_user_themes(dir.path(), &[]);
        assert_eq!(report.themes.len(), 1);
        assert_eq!(
            report.themes[0].dark.unwrap().accent,
            Color::from_u32(0xff0000)
        );
        let [
            ThemeError::DuplicateId {
                source_name,
                kept,
                id,
            },
        ] = &report.errors[..]
        else {
            panic!("expected one DuplicateId, got {:?}", report.errors);
        };
        assert!(source_name.ends_with("Tokyo Night.toml"), "{source_name}");
        assert!(kept.ends_with("tokyo_night.toml"), "{kept}");
        assert_eq!(id, "tokyo-night");
        assert!(report.errors[0].to_string().contains("same name"));
    }

    #[test]
    fn a_user_theme_with_unreadable_text_is_loaded_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("murky.toml"),
            "[dark]\nbackground=\"#222222\"\ntext=\"#333333\"\naccent=\"#0f0\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("fine.toml"), GOOD).unwrap();
        let report = load_user_themes(dir.path(), &[]);
        assert_eq!(report.themes.len(), 2, "still usable");
        let [
            ThemeError::LowContrast {
                source_name,
                variant,
                ratio,
            },
        ] = &report.errors[..]
        else {
            panic!("expected one LowContrast, got {:?}", report.errors);
        };
        assert!(source_name.ends_with("murky.toml"));
        assert_eq!((*variant, ratio.as_str()), ("dark", "1.3"));
    }

    #[test]
    fn a_themes_own_muted_and_accent_text_are_pulled_to_4_5_to_1() {
        let theme = Theme::parse(
            "faint",
            "faint.toml",
            "[dark]\nbackground=\"#202020\"\ntitle_bar=\"#383838\"\ntext=\"#f0f0f0\"\naccent=\"#808080\"\ntext_muted=\"#454545\"\naccent_text=\"#7a7a7a\"\n",
            ThemeOrigin::Bundled,
        )
        .unwrap();
        let p = theme.dark.unwrap();
        for surface in [p.background, p.window, p.title_bar, p.sidebar, p.surface] {
            assert!(p.text_muted.contrast(surface) >= TEXT_CONTRAST);
        }
        assert!(p.accent_text.contrast(p.accent) >= TEXT_CONTRAST);
    }

    #[test]
    fn derived_text_colours_are_readable_even_for_a_saturated_mid_tone_theme() {
        let theme = Theme::parse(
            "mid",
            "mid.toml",
            "[dark]\nbackground=\"#7a3b3b\"\ntext=\"#ffffff\"\naccent=\"#c04040\"\ndanger=\"#d03030\"\n",
            ThemeOrigin::Bundled,
        )
        .unwrap();
        let p = theme.dark.unwrap();
        assert!(p.danger_text.contrast(p.background) >= TEXT_CONTRAST);
        assert!(p.accent_link.contrast(p.background) >= TEXT_CONTRAST);
    }
}
