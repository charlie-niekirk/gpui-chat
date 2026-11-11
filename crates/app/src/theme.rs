//! Application theme tokens.
//!
//! [`Tokens`] is our own, stable token set (names a theme pack can extend in PR 4.4). Dark
//! and light variants follow the system appearance; on every change the matching values are
//! also pushed into GPUI Kit's [`Theme`] so Kit components (sidebar, title bar, dialogs)
//! match the shell.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, Window, rgb};

use crate::fonts::FontConfig;

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

/// Follows the system appearance: selects Kit's light/dark theme, then applies our tokens
/// and fonts on top. Call at startup and whenever the window appearance changes.
pub fn sync_with_system(window: Option<&mut Window>, cx: &mut App) {
    Theme::sync_system_appearance(window, cx);
    apply(cx);
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
        colors.accent = tokens.accent;
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
