//! Application theme tokens.
//!
//! [`Tokens`] is our own, stable token set (names a theme pack can extend in PR 4.4). The
//! dark or light variant comes from `appearance.theme`: `light` / `dark` pin it, `system`
//! follows the OS appearance (and only then does an OS change affect the app). On every
//! change the matching values and the font are also pushed into GPUI Kit's [`Theme`] so Kit
//! components (sidebar, title bar, dialogs) match the shell. [`apply_settings`] is the single
//! entry point for startup and for live settings reloads.

use gpui_chat_settings::{AppearanceSettings, ThemeMode as ThemeSetting};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, Window, rgb};

use crate::fonts::{self, FontConfig};

/// The appearance section last applied by [`apply_settings`], to skip redundant re-applies.
#[derive(Clone, Debug)]
struct AppliedAppearance(AppearanceSettings);

impl Global for AppliedAppearance {}

/// The `appearance.theme` setting currently in effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThemePreference(ThemeSetting);

impl Global for ThemePreference {}

/// Semantic colour tokens used by the app shell.
#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    /// Behind everything; visible at window edges.
    pub window_bg: Hsla,
    /// Chat / main content area.
    pub main_bg: Hsla,
    pub title_bar_bg: Hsla,
    pub sidebar_bg: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    /// Errors and warnings, e.g. the settings-error banner.
    pub danger: Hsla,
}

impl Global for Tokens {}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

impl Tokens {
    pub fn dark() -> Self {
        Self {
            window_bg: hex(0x18181b),
            main_bg: hex(0x0e0e10),
            title_bar_bg: hex(0x1f1f23),
            sidebar_bg: hex(0x18181b),
            border: hex(0x2f2f35),
            text: hex(0xefeff1),
            text_muted: hex(0xadadb8),
            accent: hex(0x9147ff),
            danger: hex(0xff6b6b),
        }
    }

    pub fn light() -> Self {
        Self {
            window_bg: hex(0xf7f7f8),
            main_bg: hex(0xffffff),
            title_bar_bg: hex(0xebebef),
            sidebar_bg: hex(0xf7f7f8),
            border: hex(0xd7d7dc),
            text: hex(0x0e0e10),
            text_muted: hex(0x53535f),
            accent: hex(0x772ce8),
            danger: hex(0xc4221f),
        }
    }

    pub fn for_mode(mode: ThemeMode) -> Self {
        if mode.is_dark() {
            Self::dark()
        } else {
            Self::light()
        }
    }
}

/// Applies `appearance` (theme mode and font) to the whole app: records the theme preference,
/// resolves the font against installed families, selects Kit's light/dark theme and pushes
/// our tokens on top. Call at startup and whenever settings change. `window` is only used to
/// read the system appearance when the preference is `system`.
pub fn apply_settings(appearance: &AppearanceSettings, window: Option<&mut Window>, cx: &mut App) {
    cx.set_global(AppliedAppearance(appearance.clone()));
    cx.set_global(ThemePreference(appearance.theme));
    fonts::apply(appearance, cx);
    match appearance.theme {
        ThemeSetting::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeSetting::Dark => Theme::change(ThemeMode::Dark, None, cx),
        ThemeSetting::System => Theme::sync_system_appearance(window, cx),
    }
    apply(cx);
}

/// [`apply_settings`], unless `appearance` is what was last applied (a failed reload, or a
/// reload that changed other sections only): then nothing is recomputed, in particular the
/// installed-font enumeration is skipped. Returns whether it applied.
pub fn apply_settings_if_changed(
    appearance: &AppearanceSettings,
    window: Option<&mut Window>,
    cx: &mut App,
) -> bool {
    let unchanged = cx
        .try_global::<AppliedAppearance>()
        .is_some_and(|applied| applied.0 == *appearance);
    if !unchanged {
        apply_settings(appearance, window, cx);
    }
    !unchanged
}

/// Whether the OS appearance currently drives the theme (`appearance.theme = "system"`).
pub fn follows_system(cx: &App) -> bool {
    cx.try_global::<ThemePreference>()
        .is_none_or(|preference| preference.0 == ThemeSetting::System)
}

/// Window-appearance observer body: re-syncs with the OS, but only when the user chose
/// `system`; an explicit `light` / `dark` is left alone.
pub fn system_appearance_changed(window: &mut Window, cx: &mut App) {
    if follows_system(cx) {
        Theme::sync_system_appearance(Some(window), cx);
        apply(cx);
    }
}

/// Pushes [`Tokens`] for the current Kit theme mode (and the font config) into the Kit
/// theme and stores them as a global.
fn apply(cx: &mut App) {
    let mode = Theme::global(cx).mode;
    let tokens = Tokens::for_mode(mode);
    let font = cx.try_global::<FontConfig>().cloned();
    Theme::update(cx, |theme| {
        let colors = &mut theme.colors;
        colors.background = tokens.main_bg;
        colors.foreground = tokens.text;
        colors.muted_foreground = tokens.text_muted;
        colors.border = tokens.border;
        // Brand colour goes on `primary`; Kit's `accent` is the neutral hover background of
        // menu/list items and ghost buttons, so it is left alone.
        colors.primary = tokens.accent;
        colors.primary_foreground = hex(0xffffff);
        colors.title_bar = tokens.title_bar_bg;
        colors.title_bar_border = tokens.border;
        colors.sidebar = tokens.sidebar_bg;
        colors.sidebar_border = tokens.border;
        colors.sidebar_foreground = tokens.text;
        if let Some(font) = font {
            theme.font_family = font.family.clone();
            theme.mono_font_family = font.family;
            theme.font_size = font.size;
            theme.mono_font_size = font.size;
        }
    });
    cx.set_global(tokens);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_and_light_differ() {
        let (dark, light) = (Tokens::dark(), Tokens::light());
        assert_ne!(dark.main_bg, light.main_bg);
        assert_ne!(dark.text, light.text);
        assert!(dark.main_bg.l < light.main_bg.l);
    }

    fn appearance(theme: ThemeSetting, size: f32, family: &str) -> AppearanceSettings {
        AppearanceSettings {
            theme,
            font_size: size,
            font_family: family.into(),
            ..AppearanceSettings::default()
        }
    }

    fn setup(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            fonts::init(&AppearanceSettings::default(), cx);
        });
    }

    #[gpui_kit::test]
    fn explicit_theme_is_applied_and_ignores_os_changes(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        let window = cx.add_window(|_, _| gpui_kit::Empty);
        cx.update(|cx| {
            apply_settings(&appearance(ThemeSetting::Dark, 14.0, "Menlo"), None, cx);
            assert!(!follows_system(cx));
            assert_eq!(Theme::global(cx).mode, ThemeMode::Dark);
            assert_eq!(cx.global::<Tokens>().main_bg, Tokens::dark().main_bg);

            apply_settings(&appearance(ThemeSetting::Light, 14.0, "Menlo"), None, cx);
            assert_eq!(Theme::global(cx).mode, ThemeMode::Light);
            assert_eq!(cx.global::<Tokens>().main_bg, Tokens::light().main_bg);
        });
        // The OS appearance observer must not override a pinned theme.
        window
            .update(cx, |_, window, cx| {
                system_appearance_changed(window, cx);
                assert_eq!(Theme::global(cx).mode, ThemeMode::Light);
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn unchanged_appearance_is_not_reapplied(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            let dark = appearance(ThemeSetting::Dark, 14.0, "Menlo");
            assert!(apply_settings_if_changed(&dark, None, cx));
            assert!(!apply_settings_if_changed(&dark, None, cx));
            // Something Kit's theme would otherwise be reset by a re-apply.
            Theme::update(cx, |theme| theme.font_size = gpui_kit::px(99.0));
            assert!(!apply_settings_if_changed(&dark, None, cx));
            assert_eq!(Theme::global(cx).font_size, gpui_kit::px(99.0));
            assert!(apply_settings_if_changed(
                &appearance(ThemeSetting::Dark, 15.0, "Menlo"),
                None,
                cx
            ));
            assert_eq!(Theme::global(cx).font_size, gpui_kit::px(15.0));
        });
    }

    #[gpui_kit::test]
    fn system_theme_follows_the_window_appearance(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        let window = cx.add_window(|_, _| gpui_kit::Empty);
        window
            .update(cx, |_, window, cx| {
                apply_settings(&appearance(ThemeSetting::Dark, 14.0, "Menlo"), None, cx);
                apply_settings(
                    &appearance(ThemeSetting::System, 14.0, "Menlo"),
                    Some(window),
                    cx,
                );
                assert!(follows_system(cx));
                let system = ThemeMode::from(window.appearance());
                assert_eq!(Theme::global(cx).mode, system);
                assert_eq!(
                    cx.global::<Tokens>().main_bg,
                    Tokens::for_mode(system).main_bg
                );
            })
            .unwrap();
    }

    #[gpui_kit::test]
    fn font_family_and_size_reach_the_font_config_and_kit_theme(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            // A family that is certainly not installed must never reach GPUI.
            apply_settings(
                &appearance(ThemeSetting::System, 20.0, "No Such Family"),
                None,
                cx,
            );
            let config = cx.global::<FontConfig>().clone();
            assert_ne!(config.family, "No Such Family");
            assert_eq!(config.size, gpui_kit::px(20.0));
            let theme = Theme::global(cx);
            assert_eq!(theme.font_family, config.family);
            assert_eq!(theme.font_size, gpui_kit::px(20.0));
        });
    }

    #[test]
    fn mode_selects_variant() {
        assert_eq!(
            Tokens::for_mode(ThemeMode::Dark).main_bg,
            Tokens::dark().main_bg
        );
        assert_eq!(
            Tokens::for_mode(ThemeMode::Light).main_bg,
            Tokens::light().main_bg
        );
    }
}
