//! Application theme tokens.
//!
//! [`Tokens`] is our own, stable token set; a theme (`gpui-chat-settings`: bundled pack or the
//! user's `themes/*.toml`) supplies its values as a [`Palette`]. Which theme and variant is in
//! effect comes from `appearance.theme` (`light` / `dark` pin the variant, `system` follows the
//! OS appearance), `appearance.theme_name` and `appearance.theme_name_light`; the resolution
//! itself is pure and lives in `ThemeRegistry::resolve`. On every change the palette is also
//! pushed into GPUI Kit's [`Theme`] colours (we do not drive Kit's own JSON theme files: its
//! schema is ~170 keys, ours is 13, and the Kit colours not listed in [`apply_kit_colors`]
//! keep Kit's defaults for the variant) so Kit components (sidebar, title bar, menus, dialogs,
//! inputs) match the shell. [`apply_settings`] is the single entry point for startup and for
//! live settings reloads; [`ThemeReloads`] re-applies when the user's theme files change.

use gpui_chat_settings::{
    AppearanceSettings, Color, LoadReport, Palette, SettingsPaths, ThemeError,
    ThemeMode as ThemeSetting, ThemeOrigin, ThemeRegistry, ThemeVariant, ThemeWatcher,
    WatchOptions, load_user_themes, watch_themes,
};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, AppContext as _, Entity, Global, Hsla, Window, rgb};

use crate::bridge::{self, BridgeReceiver};
use crate::fonts::{self, FontConfig};

/// The appearance section last applied by [`apply_settings`], to skip redundant re-applies.
#[derive(Clone, Debug)]
struct AppliedAppearance(AppearanceSettings);

impl Global for AppliedAppearance {}

/// The `appearance.theme` setting currently in effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThemePreference(ThemeSetting);

impl Global for ThemePreference {}

/// The OS light/dark appearance as last observed (only used when the preference is `system`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SystemVariant(ThemeVariant);

impl Global for SystemVariant {}

/// Every known theme: the bundled pack plus the user's files, and what could not be loaded.
#[derive(Clone, Debug)]
struct Themes {
    registry: ThemeRegistry,
}

impl Global for Themes {}

/// Semantic colour tokens used by the app shell.
#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    /// Behind everything; visible at window edges.
    pub window_bg: Hsla,
    /// Chat / main content area.
    pub main_bg: Hsla,
    pub title_bar_bg: Hsla,
    pub sidebar_bg: Hsla,
    /// Raised surfaces: menus, popovers, dialogs, the emote picker and autocomplete list.
    pub surface: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    /// Text and icons drawn on an `accent` fill.
    pub accent_text: Hsla,
    /// Errors and warnings, e.g. the settings-error banner, and the live dot.
    pub danger: Hsla,
    /// `danger` drawn as text: readable (4.5:1) on every surface and under a danger tint.
    pub danger_text: Hsla,
    /// `accent` drawn as text or a thin icon, same rule.
    pub accent_link: Hsla,
    /// Connected.
    pub success: Hsla,
    /// Connecting / reconnecting.
    pub warning: Hsla,
    /// Whether the resolved variant is light. Use this, not the luminance of a colour, to pick
    /// light or dark assets (a dark theme can have a mid-tone background).
    pub light: bool,
}

impl Global for Tokens {}

fn hsla(color: Color) -> Hsla {
    rgb(color.to_u32()).into()
}

impl Tokens {
    pub fn from_palette(palette: &Palette, variant: ThemeVariant) -> Self {
        Self {
            window_bg: hsla(palette.window),
            main_bg: hsla(palette.background),
            title_bar_bg: hsla(palette.title_bar),
            sidebar_bg: hsla(palette.sidebar),
            surface: hsla(palette.surface),
            border: hsla(palette.border),
            text: hsla(palette.text),
            text_muted: hsla(palette.text_muted),
            accent: hsla(palette.accent),
            accent_text: hsla(palette.accent_text),
            danger: hsla(palette.danger),
            danger_text: hsla(palette.danger_text),
            accent_link: hsla(palette.accent_link),
            success: hsla(palette.success),
            warning: hsla(palette.warning),
            light: variant == ThemeVariant::Light,
        }
    }

    /// The tokens of the built-in theme's variant (what a fresh install shows).
    #[cfg(test)]
    pub fn for_variant(variant: ThemeVariant) -> Self {
        let registry = ThemeRegistry::bundled();
        let theme = registry
            .get(gpui_chat_settings::DEFAULT_THEME_ID)
            .expect("the default theme is bundled");
        Self::from_palette(&theme.palette(variant).1, variant)
    }

    #[cfg(test)]
    pub fn dark() -> Self {
        Self::for_variant(ThemeVariant::Dark)
    }

    #[cfg(test)]
    pub fn light() -> Self {
        Self::for_variant(ThemeVariant::Light)
    }

    #[cfg(test)]
    pub fn for_mode(mode: ThemeMode) -> Self {
        Self::for_variant(variant_of(mode))
    }
}

fn variant_of(mode: ThemeMode) -> ThemeVariant {
    if mode.is_dark() {
        ThemeVariant::Dark
    } else {
        ThemeVariant::Light
    }
}

fn mode_of(variant: ThemeVariant) -> ThemeMode {
    match variant {
        ThemeVariant::Dark => ThemeMode::Dark,
        ThemeVariant::Light => ThemeMode::Light,
    }
}

/// The registry in effect, installing the bundled pack when nothing was set up (tests).
fn themes(cx: &mut App) -> Themes {
    if let Some(themes) = cx.try_global::<Themes>() {
        return themes.clone();
    }
    let themes = Themes {
        registry: ThemeRegistry::bundled(),
    };
    cx.set_global(themes.clone());
    themes
}

fn system_variant(window: Option<&Window>, cx: &App) -> ThemeVariant {
    let appearance = window.map_or_else(|| cx.window_appearance(), Window::appearance);
    variant_of(ThemeMode::from(appearance))
}

/// Loads the user's `themes/` directory and starts watching it. Call once at startup, before
/// [`apply_settings`]. Returns the receiver to hand to [`attach_reloads`],
/// or `None` when the directory cannot be watched (no home directory, or the watcher could not
/// start; logged, and the themes found at startup still work).
pub fn init(cx: &mut App) -> Option<BridgeReceiver<LoadReport>> {
    let Ok(paths) = SettingsPaths::from_env() else {
        cx.set_global(Themes {
            registry: ThemeRegistry::bundled(),
        });
        return None;
    };
    let dir = paths.themes_dir();
    let report = load_user_themes(&dir, &[]);
    log_errors(&report.errors);
    set_notices(cx, |n| {
        n.files = report.errors.iter().map(ToString::to_string).collect()
    });
    cx.set_global(Themes {
        registry: ThemeRegistry::with_user_themes(report.themes.clone()),
    });
    let (tx, rx) = bridge::channel(EVENT_QUEUE, EVENT_QUEUE);
    let watcher = watch_themes(&dir, report, WatchOptions::default(), move |report| {
        // Runs on the watcher's own thread; a closed bridge means the app is quitting.
        let _ = tx.blocking_send(report);
    });
    match watcher {
        Ok(watcher) => {
            cx.set_global(ThemeWatchGuard(Some(watcher)));
            cx.on_app_quit(|cx| {
                cx.global_mut::<ThemeWatchGuard>().0.take();
                async {}
            })
            .detach();
            Some(rx)
        }
        Err(error) => {
            tracing::warn!(%error, "theme file changes will not be picked up live");
            None
        }
    }
}

/// A theme the user can pick: its `theme_name` id, its display name and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeChoice {
    pub id: String,
    pub name: String,
    /// A file in the user's `themes/` directory (possibly replacing a bundled one).
    pub user: bool,
}

/// Every theme that can be selected: the bundled pack and the user's `themes/` files, the
/// default first, then by name. Reflects the directory as last loaded.
pub fn choices(cx: &App) -> Vec<ThemeChoice> {
    let bundled;
    let registry = match cx.try_global::<Themes>() {
        Some(themes) => &themes.registry,
        None => {
            bundled = ThemeRegistry::bundled();
            &bundled
        }
    };
    registry
        .iter()
        .map(|theme| ThemeChoice {
            id: theme.id.clone(),
            name: theme.name.clone(),
            user: matches!(theme.origin, ThemeOrigin::User(_)),
        })
        .collect()
}

/// The id of the theme `name` (a `theme_name` setting) selects, if there is one: case, spaces
/// and `_` vs `-` do not matter.
pub fn choice_id(name: &str, cx: &App) -> Option<String> {
    let id = gpui_chat_settings::normalize_id(name);
    choices(cx).into_iter().find(|c| c.id == id).map(|c| c.id)
}

/// Problems with the theme setup that the user should see (the app has no log viewer): bad or
/// ignored theme files, and `theme_name`s that matched nothing.
#[derive(Clone, Debug, Default)]
struct ThemeNotices {
    /// From the `themes/` directory (parse errors, duplicate names, low contrast).
    files: Vec<String>,
    /// From resolving the settings (unknown names).
    selection: Vec<String>,
}

impl Global for ThemeNotices {}

/// What to tell the user about their themes: one line each, empty when all is well.
pub fn notices(cx: &App) -> Vec<String> {
    cx.try_global::<ThemeNotices>().map_or_else(Vec::new, |n| {
        n.files.iter().chain(&n.selection).cloned().collect()
    })
}

fn set_notices(cx: &mut App, update: impl FnOnce(&mut ThemeNotices)) {
    let mut current = cx.try_global::<ThemeNotices>().cloned().unwrap_or_default();
    update(&mut current);
    cx.set_global(current);
}

/// Theme directory events are rare and small.
const EVENT_QUEUE: usize = 8;

/// Keeps the themes-directory watcher alive; dropped on quit.
struct ThemeWatchGuard(Option<ThemeWatcher>);

impl Global for ThemeWatchGuard {}

fn log_errors(errors: &[ThemeError]) {
    for error in errors {
        tracing::warn!(%error, "user theme not loaded");
    }
}

/// Entity that receives user-theme reloads from the watcher thread (through the bridge) and
/// re-applies the theme on the UI thread. The bridge holds it weakly, so a global keeps it alive.
struct ThemeReloads;

struct ThemeReloadsHandle(#[allow(dead_code)] Entity<ThemeReloads>);

impl Global for ThemeReloadsHandle {}

/// Connects the themes-directory watcher to the UI: when a theme file is added, edited or
/// removed, the registry is replaced and the active theme re-applied, so editing the theme
/// that is in use recolours the app live.
pub fn attach_reloads(receiver: BridgeReceiver<LoadReport>, cx: &mut App) {
    let entity: Entity<ThemeReloads> = cx.new(|_| ThemeReloads);
    // Not tied to a window's frames: the settings window must see a theme change while the main
    // window is minimised.
    bridge::attach_unframed(&entity, receiver, cx, |_, reports, cx| {
        let Some(report) = reports.into_iter().last() else {
            return false;
        };
        log_errors(&report.errors);
        set_notices(cx, |n| {
            n.files = report.errors.iter().map(ToString::to_string).collect()
        });
        cx.set_global(Themes {
            registry: ThemeRegistry::with_user_themes(report.themes),
        });
        reapply(cx);
        false
    })
    .detach();
    cx.set_global(ThemeReloadsHandle(entity));
}

/// Applies `appearance` (theme, variant and font) to the whole app: records the preference,
/// resolves the font against installed families, resolves the theme for the current OS
/// appearance and pushes its colours into Kit and our tokens. Call at startup and whenever
/// settings change. `window` is only used to read the system appearance when the preference
/// is `system`.
pub fn apply_settings(appearance: &AppearanceSettings, window: Option<&mut Window>, cx: &mut App) {
    cx.set_global(AppliedAppearance(appearance.clone()));
    cx.set_global(ThemePreference(appearance.theme));
    fonts::apply(appearance, cx);
    cx.set_global(SystemVariant(system_variant(window.as_deref(), cx)));
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

/// Window-appearance observer body: re-reads the OS appearance and re-resolves the theme, but
/// only when the user chose `system`; an explicit `light` / `dark` is left alone.
pub fn system_appearance_changed(window: &mut Window, cx: &mut App) {
    if follows_system(cx) {
        cx.set_global(SystemVariant(system_variant(Some(window), cx)));
        apply(cx);
    }
}

/// Re-resolves the theme with the settings and OS appearance already recorded (the theme
/// files changed).
fn reapply(cx: &mut App) {
    if cx.has_global::<AppliedAppearance>() {
        apply(cx);
    }
}

/// Resolves the theme from the recorded settings, then pushes it into the Kit theme (variant
/// and colours, plus the font config) and stores the [`Tokens`] global.
fn apply(cx: &mut App) {
    let appearance = cx
        .try_global::<AppliedAppearance>()
        .map(|applied| applied.0.clone())
        .unwrap_or_default();
    let system = cx
        .try_global::<SystemVariant>()
        .map_or(ThemeVariant::Dark, |system| system.0);
    let resolved = themes(cx).registry.resolve(&appearance, system);
    let unknown: Vec<String> = resolved
        .unknown
        .iter()
        .map(|unknown| {
            format!(
                "{} = \"{}\" is not a known theme; using \"{}\" instead",
                unknown.setting, unknown.name, resolved.name
            )
        })
        .collect();
    for line in &unknown {
        tracing::warn!("{line}");
    }
    set_notices(cx, |n| n.selection = unknown);
    let tokens = Tokens::from_palette(&resolved.palette, resolved.variant);
    let font = cx.try_global::<FontConfig>().cloned();
    // Selecting the mode loads Kit's colours for it; ours go on top.
    Theme::change(mode_of(resolved.variant), None, cx);
    Theme::update(cx, |theme| {
        apply_kit_colors(&mut theme.colors, &resolved.palette, &tokens);
        if let Some(font) = font {
            theme.font_family = font.family.clone();
            theme.mono_font_family = font.family;
            theme.font_size = font.size;
            theme.mono_font_size = font.size;
        }
    });
    cx.set_global(tokens);
}

/// Pushes a palette into Kit's colours. Everything Kit's components in this app draw from is
/// set here, so no Kit default (a light grey in a dark theme, say) leaks through; colours not
/// listed (charts, tables, ...) are not used by the app.
fn apply_kit_colors(colors: &mut gpui_kit::component::ThemeColor, p: &Palette, tokens: &Tokens) {
    let h = hsla;
    // A hover/active step: the base nudged towards the text colour.
    let toward = |base: Color, amount: f32| h(base.mix(p.text, amount));
    let danger_text = h(contrast_text(p.danger));
    let hover = toward(p.background, 0.08);

    colors.background = h(p.background);
    colors.foreground = h(p.text);
    colors.muted_foreground = h(p.text_muted);
    colors.muted = h(p.surface);
    colors.border = h(p.border);
    colors.input = h(p.border);
    colors.ring = h(p.accent);
    colors.caret = h(p.text);
    colors.selection = h(p.accent).opacity(0.3);
    colors.overlay = gpui_kit::black().opacity(0.5);

    // Brand colour goes on `primary`; Kit's `accent` is the neutral hover background of
    // menu/list items and ghost buttons.
    colors.primary = h(p.accent);
    colors.primary_foreground = h(p.accent_text);
    colors.primary_hover = h(p.accent.mix(p.text, 0.12));
    colors.primary_active = h(p.accent.mix(p.text, 0.22));
    colors.accent = hover;
    colors.accent_foreground = h(p.text);
    colors.link = h(p.accent);
    colors.link_hover = h(p.accent.mix(p.text, 0.15));
    colors.link_active = h(p.accent.mix(p.text, 0.25));

    colors.secondary = h(p.surface);
    colors.secondary_foreground = h(p.text);
    colors.secondary_hover = toward(p.surface, 0.08);
    colors.secondary_active = toward(p.surface, 0.16);
    colors.popover = h(p.surface);
    colors.popover_foreground = h(p.text);

    colors.danger = h(p.danger);
    colors.danger_foreground = danger_text;
    colors.danger_hover = h(p.danger.mix(p.text, 0.12));
    colors.danger_active = h(p.danger.mix(p.text, 0.22));
    colors.success = h(p.success);
    colors.success_foreground = h(contrast_text(p.success));
    colors.success_hover = h(p.success.mix(p.text, 0.12));
    colors.success_active = h(p.success.mix(p.text, 0.22));
    colors.warning = h(p.warning);
    colors.warning_foreground = h(contrast_text(p.warning));
    colors.warning_hover = h(p.warning.mix(p.text, 0.12));
    colors.warning_active = h(p.warning.mix(p.text, 0.22));
    colors.info = h(p.accent);
    colors.info_foreground = h(p.accent_text);
    colors.info_hover = h(p.accent.mix(p.text, 0.12));
    colors.info_active = h(p.accent.mix(p.text, 0.22));

    colors.list = h(p.background);
    colors.list_even = h(p.background);
    colors.list_head = h(p.surface);
    colors.list_hover = hover;
    colors.list_active = h(p.accent).opacity(0.18);
    colors.list_active_border = h(p.accent);
    colors.drop_target = h(p.accent).opacity(0.15);
    colors.drag_border = h(p.accent);

    colors.title_bar = tokens.title_bar_bg;
    colors.title_bar_border = h(p.border);
    colors.sidebar = tokens.sidebar_bg;
    colors.sidebar_border = h(p.border);
    colors.sidebar_foreground = h(p.text);
    colors.sidebar_accent = toward(p.sidebar, 0.08);
    colors.sidebar_accent_foreground = h(p.text);
    colors.sidebar_primary = h(p.accent);
    colors.sidebar_primary_foreground = h(p.accent_text);
    colors.window_border = h(p.border);

    colors.scrollbar = gpui_kit::transparent_black();
    colors.scrollbar_thumb = toward(p.background, 0.3);
    colors.scrollbar_thumb_hover = toward(p.background, 0.45);
    colors.skeleton = h(p.surface);
    colors.switch = toward(p.surface, 0.2);
    colors.switch_thumb = h(p.text);
    colors.slider_bar = h(p.accent);
    colors.slider_thumb = h(p.text);
    colors.progress_bar = h(p.accent);
    colors.accordion = h(p.background);
    colors.group_box = h(p.surface);
    colors.group_box_foreground = h(p.text);
    colors.tab = gpui_kit::transparent_black();
    colors.tab_active = h(p.background);
    colors.tab_active_foreground = h(p.text);
    colors.tab_foreground = h(p.text_muted);
    colors.tab_bar = h(p.surface);
    colors.tab_bar_segmented = h(p.surface);

    // Kit's buttons draw from the `button*` families (not from `primary` & co.): plain,
    // primary (accent), secondary (a raised surface), and the status variants.
    colors.button = h(p.surface);
    colors.button_hover = toward(p.surface, 0.08);
    colors.button_active = toward(p.surface, 0.16);
    colors.button_foreground = h(p.text);
    colors.button_primary = h(p.accent);
    colors.button_primary_hover = h(p.accent.mix(p.text, 0.12));
    colors.button_primary_active = h(p.accent.mix(p.text, 0.22));
    colors.button_primary_foreground = h(p.accent_text);
    colors.button_secondary = h(p.surface);
    colors.button_secondary_hover = toward(p.surface, 0.08);
    colors.button_secondary_active = toward(p.surface, 0.16);
    colors.button_secondary_foreground = h(p.text);
    colors.button_info = h(p.accent);
    colors.button_info_hover = h(p.accent.mix(p.text, 0.12));
    colors.button_info_active = h(p.accent.mix(p.text, 0.22));
    colors.button_info_foreground = h(p.accent_text);
    colors.button_danger = h(p.danger);
    colors.button_danger_hover = h(p.danger.mix(p.text, 0.12));
    colors.button_danger_active = h(p.danger.mix(p.text, 0.22));
    colors.button_danger_foreground = danger_text;
    colors.button_success = h(p.success);
    colors.button_success_hover = h(p.success.mix(p.text, 0.12));
    colors.button_success_active = h(p.success.mix(p.text, 0.22));
    colors.button_success_foreground = h(contrast_text(p.success));
    colors.button_warning = h(p.warning);
    colors.button_warning_hover = h(p.warning.mix(p.text, 0.12));
    colors.button_warning_active = h(p.warning.mix(p.text, 0.22));
    colors.button_warning_foreground = h(contrast_text(p.warning));
}

/// Black or white, whichever reads better on `fill`.
fn contrast_text(fill: Color) -> Color {
    if fill.contrast(Color::WHITE) >= fill.contrast(Color::BLACK) {
        Color::WHITE
    } else {
        Color::BLACK
    }
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
    fn theme_choices_list_the_pack_with_the_default_first(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            let choices = choices(cx);
            assert_eq!(choices[0].id, gpui_chat_settings::DEFAULT_THEME_ID);
            assert!(choices.iter().any(|c| c.id == "tokyo-night"));
            assert!(choices.iter().all(|c| !c.user), "no user themes loaded");
            // A `theme_name` as typed by hand finds its theme; nonsense finds none.
            assert_eq!(choice_id("Tokyo Night", cx).as_deref(), Some("tokyo-night"));
            assert_eq!(choice_id("tokyo_night", cx).as_deref(), Some("tokyo-night"));
            assert_eq!(choice_id("no such theme", cx), None);
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

    fn named(theme: ThemeSetting, name: &str, light: Option<&str>) -> AppearanceSettings {
        AppearanceSettings {
            theme,
            theme_name: name.into(),
            theme_name_light: light.map(str::to_owned),
            ..AppearanceSettings::default()
        }
    }

    fn pretend_os_is(variant: ThemeVariant, cx: &mut App) {
        cx.set_global(SystemVariant(variant));
        apply(cx);
    }

    fn registry_color(id: &str, variant: ThemeVariant) -> Hsla {
        let registry = ThemeRegistry::bundled();
        hsla(registry.get(id).unwrap().palette(variant).1.background)
    }

    #[gpui_kit::test]
    fn switching_themes_recolours_tokens_and_kit(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            for id in ["tokyo-night", "gruvbox", "nord", "everforest", "kanagawa"] {
                apply_settings(&named(ThemeSetting::Dark, id, None), None, cx);
                let expected = registry_color(id, ThemeVariant::Dark);
                assert_eq!(cx.global::<Tokens>().main_bg, expected, "{id} tokens");
                assert_eq!(Theme::global(cx).colors.background, expected, "{id} kit");
                assert_eq!(
                    Theme::global(cx).colors.popover,
                    cx.global::<Tokens>().surface,
                    "{id} popover"
                );
                assert_eq!(
                    Theme::global(cx).colors.primary,
                    cx.global::<Tokens>().accent,
                    "{id} accent"
                );
            }
            // Back to the default restores its colours.
            apply_settings(&named(ThemeSetting::Dark, "default", None), None, cx);
            assert_eq!(cx.global::<Tokens>().main_bg, Tokens::dark().main_bg);
        });
    }

    #[gpui_kit::test]
    fn a_light_theme_puts_kit_in_light_mode(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            // `rose-pine` has both; `white` only light.
            apply_settings(&named(ThemeSetting::Light, "rose-pine", None), None, cx);
            assert_eq!(Theme::global(cx).mode, ThemeMode::Light);
            assert_eq!(
                cx.global::<Tokens>().main_bg,
                registry_color("rose-pine", ThemeVariant::Light)
            );
            apply_settings(&named(ThemeSetting::Dark, "white", None), None, cx);
            assert_eq!(
                Theme::global(cx).mode,
                ThemeMode::Light,
                "white has no dark"
            );
            assert!(cx.global::<Tokens>().main_bg.l > 0.9);
        });
    }

    #[gpui_kit::test]
    fn follow_system_switches_light_and_dark(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            apply_settings(&named(ThemeSetting::System, "catppuccin", None), None, cx);
            assert!(follows_system(cx));
            pretend_os_is(ThemeVariant::Dark, cx);
            assert_eq!(Theme::global(cx).mode, ThemeMode::Dark);
            assert_eq!(
                cx.global::<Tokens>().main_bg,
                registry_color("catppuccin", ThemeVariant::Dark)
            );
            pretend_os_is(ThemeVariant::Light, cx);
            assert_eq!(Theme::global(cx).mode, ThemeMode::Light);
            assert_eq!(
                cx.global::<Tokens>().main_bg,
                registry_color("catppuccin", ThemeVariant::Light)
            );

            // Two themes paired through `theme_name_light`.
            apply_settings(
                &named(ThemeSetting::System, "tokyo-night", Some("rose-pine")),
                None,
                cx,
            );
            pretend_os_is(ThemeVariant::Dark, cx);
            assert_eq!(
                cx.global::<Tokens>().main_bg,
                registry_color("tokyo-night", ThemeVariant::Dark)
            );
            pretend_os_is(ThemeVariant::Light, cx);
            assert_eq!(
                cx.global::<Tokens>().main_bg,
                registry_color("rose-pine", ThemeVariant::Light)
            );
        });
    }

    #[gpui_kit::test]
    fn a_user_theme_is_picked_up_and_edits_apply_live(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        let user = |accent: &str| {
            gpui_chat_settings::Theme::parse(
                "mine",
                "mine.toml",
                &format!("[dark]\nbackground=\"#102030\"\ntext=\"#ffffff\"\naccent=\"{accent}\"\n"),
                gpui_chat_settings::ThemeOrigin::User("mine.toml".into()),
            )
            .unwrap()
        };
        cx.update(|cx| {
            cx.set_global(Themes {
                registry: ThemeRegistry::with_user_themes(vec![user("#ff0000")]),
            });
            apply_settings(&named(ThemeSetting::Dark, "mine", None), None, cx);
            assert_eq!(
                cx.global::<Tokens>().main_bg,
                hsla(Color::from_u32(0x102030))
            );
            assert_eq!(
                cx.global::<Tokens>().accent,
                hsla(Color::from_u32(0xff0000))
            );

            // The file is edited: the registry is replaced and the theme re-applied.
            cx.set_global(Themes {
                registry: ThemeRegistry::with_user_themes(vec![user("#00ff00")]),
            });
            reapply(cx);
            assert_eq!(
                cx.global::<Tokens>().accent,
                hsla(Color::from_u32(0x00ff00))
            );

            // The file is deleted: the unknown name falls back to the default theme.
            cx.set_global(Themes {
                registry: ThemeRegistry::bundled(),
            });
            reapply(cx);
            assert_eq!(cx.global::<Tokens>().main_bg, Tokens::dark().main_bg);
        });
    }

    #[gpui_kit::test]
    fn changing_only_the_theme_name_reapplies(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            let nord = named(ThemeSetting::Dark, "nord", None);
            assert!(apply_settings_if_changed(&nord, None, cx));
            assert!(!apply_settings_if_changed(&nord, None, cx));
            assert!(apply_settings_if_changed(
                &named(ThemeSetting::Dark, "gruvbox", None),
                None,
                cx
            ));
            assert_eq!(
                cx.global::<Tokens>().main_bg,
                registry_color("gruvbox", ThemeVariant::Dark)
            );
        });
    }

    #[gpui_kit::test]
    fn tokens_know_their_variant_and_have_readable_status_text(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            // A dark theme with a bright mid-tone background is still dark.
            cx.set_global(Themes {
                registry: ThemeRegistry::with_user_themes(vec![
                    gpui_chat_settings::Theme::parse(
                        "bright-dark",
                        "bright-dark.toml",
                        "[dark]\nbackground=\"#d0d0d0\"\ntext=\"#000000\"\naccent=\"#2060c0\"\n",
                        gpui_chat_settings::ThemeOrigin::Bundled,
                    )
                    .unwrap(),
                ]),
            });
            apply_settings(&named(ThemeSetting::Dark, "bright-dark", None), None, cx);
            let tokens = *cx.global::<Tokens>();
            assert!(tokens.main_bg.l > 0.5, "the background is light...");
            assert!(!tokens.light, "...but the variant is dark");
            apply_settings(&named(ThemeSetting::Light, "rose-pine", None), None, cx);
            assert!(cx.global::<Tokens>().light);
        });
    }

    #[gpui_kit::test]
    fn unknown_theme_names_and_bad_files_become_notices(cx: &mut gpui_kit::TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            assert!(notices(cx).is_empty());
            apply_settings(&named(ThemeSetting::Dark, "tokyonight", None), None, cx);
            let shown = notices(cx);
            assert_eq!(shown.len(), 1);
            assert!(
                shown[0].contains("appearance.theme_name") && shown[0].contains("tokyonight"),
                "{shown:?}"
            );
            // Fixing the name clears it.
            apply_settings(&named(ThemeSetting::Dark, "tokyo-night", None), None, cx);
            assert!(notices(cx).is_empty());
            // Theme file problems are listed too, and kept until the next reload clears them.
            set_notices(cx, |n| {
                n.files = vec!["theme x.toml: invalid `dark.accent`".into()]
            });
            assert_eq!(notices(cx).len(), 1);
            apply_settings(&named(ThemeSetting::Dark, "nord", None), None, cx);
            assert_eq!(
                notices(cx).len(),
                1,
                "file problems outlive a settings change"
            );
            set_notices(cx, |n| n.files.clear());
            assert!(notices(cx).is_empty());
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
