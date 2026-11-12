//! Bundled JetBrains Mono and the default font-family chain.
//!
//! The four upstream TTFs (OFL-1.1, see `assets/fonts/OFL.txt`) are compiled in and
//! registered with the text system at startup. The family actually used is picked from the
//! user's `appearance.font_family` + `font_fallbacks`, then `JetBrainsMono Nerd Font`, the
//! bundled `JetBrains Mono` and a system monospace, taking the first one that is installed.
//! GPUI panics when it lays out text in a family that does not exist, so the pick is always
//! validated against the installed-family list instead of being left to a fallback cascade.
//! [`apply`] re-runs the pick when settings change.

use std::borrow::Cow;

use gpui_chat_settings::AppearanceSettings;
use gpui_kit::{App, Global, Pixels, SharedString, px};

/// Family name of the bundled fonts, as stored in the TTF name tables.
pub const BUNDLED_FAMILY: &str = "JetBrains Mono";
/// Preferred family when the user has the Nerd Font patched build installed.
pub const NERD_FAMILY: &str = "JetBrainsMono Nerd Font";
/// GPUI's virtual system UI family; the last resort.
const SYSTEM_UI_FAMILY: &str = ".SystemUIFont";

/// System monospace families, tried in order after the JetBrains Mono ones.
const SYSTEM_MONO_FAMILIES: &[&str] = if cfg!(target_os = "macos") {
    &["Menlo", "Monaco", "Courier New"]
} else if cfg!(target_os = "windows") {
    &["Cascadia Mono", "Consolas", "Courier New"]
} else {
    &["DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono"]
};

macro_rules! bundled {
    ($file:literal) => {
        (
            concat!("fonts/", $file),
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../assets/fonts/",
                $file
            ))
            .as_slice(),
        )
    };
}

const FONT_FILES: [(&str, &[u8]); 4] = [
    bundled!("JetBrainsMono-Regular.ttf"),
    bundled!("JetBrainsMono-Bold.ttf"),
    bundled!("JetBrainsMono-Italic.ttf"),
    bundled!("JetBrainsMono-BoldItalic.ttf"),
];

/// The family and size every piece of shell text uses.
#[derive(Clone, Debug)]
pub struct FontConfig {
    pub family: SharedString,
    pub size: Pixels,
}

impl Global for FontConfig {}

/// Bytes of a bundled font addressed by asset path (`fonts/<file>.ttf`).
pub fn embedded(path: &str) -> Option<&'static [u8]> {
    FONT_FILES
        .iter()
        .find_map(|(name, bytes)| (*name == path).then_some(*bytes))
}

/// Asset paths of the bundled fonts.
pub fn embedded_paths() -> impl Iterator<Item = &'static str> {
    FONT_FILES.iter().map(|(name, _)| *name)
}

/// Registers the bundled fonts, then resolves and stores the [`FontConfig`] for `appearance`.
pub fn init(appearance: &AppearanceSettings, cx: &mut App) {
    let faces: Vec<Cow<'static, [u8]>> = FONT_FILES
        .iter()
        .map(|(_, bytes)| Cow::Borrowed(*bytes))
        .collect();
    if let Err(error) = cx.text_system().add_fonts(faces) {
        tracing::warn!(%error, "failed to register bundled JetBrains Mono");
    }
    apply(appearance, cx);
}

/// Resolves the font for `appearance` against the installed families and stores it as the
/// [`FontConfig`] global. Callers re-apply the theme afterwards (see `theme::apply_settings`)
/// so Kit components pick the new font up too.
pub fn apply(appearance: &AppearanceSettings, cx: &mut App) {
    let installed = cx.text_system().all_font_names();
    let config = resolve(appearance, &installed);
    tracing::info!(family = %config.family, size = config.size.as_f32(), "resolved UI font");
    cx.set_global(config);
}

/// The family and size to use for `appearance`, given the installed family names.
pub fn resolve(appearance: &AppearanceSettings, installed: &[String]) -> FontConfig {
    FontConfig {
        family: pick_family(appearance.font_chain(), installed),
        size: px(appearance.font_size),
    }
}

/// Walks the user's `preferred` families, then the default chain, and returns the first one
/// present in `installed`. Never returns a family that is not installed, except the virtual
/// system UI family as the very last resort.
pub fn pick_family<'a>(
    preferred: impl IntoIterator<Item = &'a str>,
    installed: &[String],
) -> SharedString {
    preferred
        .into_iter()
        .chain([NERD_FAMILY, BUNDLED_FAMILY])
        .chain(SYSTEM_MONO_FAMILIES.iter().copied())
        .map(str::trim)
        .find(|candidate| installed.iter().any(|name| name == candidate))
        .unwrap_or(SYSTEM_UI_FAMILY)
        .to_string()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn nerd_font_wins_when_installed() {
        let installed = names(&[BUNDLED_FAMILY, NERD_FAMILY, "Menlo"]);
        assert_eq!(pick_family([], &installed), NERD_FAMILY);
    }

    #[test]
    fn bundled_font_is_next() {
        let installed = names(&[BUNDLED_FAMILY, "Menlo"]);
        assert_eq!(pick_family([], &installed), BUNDLED_FAMILY);
    }

    #[test]
    fn system_monospace_then_system_ui() {
        let mono = SYSTEM_MONO_FAMILIES[0];
        assert_eq!(pick_family([], &names(&[mono])), mono);
        assert_eq!(pick_family([], &names(&["Helvetica"])), SYSTEM_UI_FAMILY);
    }

    #[test]
    fn preferred_family_wins_when_installed() {
        let installed = names(&[BUNDLED_FAMILY, NERD_FAMILY, "Menlo", "Fira Code"]);
        assert_eq!(pick_family(["Fira Code"], &installed), "Fira Code");
        // The first installed entry of the user's chain is used, not simply the first entry.
        assert_eq!(pick_family(["Nope", "Menlo"], &installed), "Menlo");
    }

    #[test]
    fn missing_preferred_family_falls_back_through_the_default_chain() {
        let installed = names(&[BUNDLED_FAMILY, "Menlo"]);
        assert_eq!(pick_family(["Not Installed"], &installed), BUNDLED_FAMILY);
        // Generic names like `monospace` are not installed families and are skipped.
        assert_eq!(pick_family(["monospace"], &installed), BUNDLED_FAMILY);
        assert_eq!(pick_family([""], &names(&["Helvetica"])), SYSTEM_UI_FAMILY);
    }

    #[test]
    fn resolve_uses_settings_chain_and_size() {
        let mut appearance = AppearanceSettings {
            font_family: "Menlo".into(),
            font_size: 20.0,
            ..AppearanceSettings::default()
        };
        let config = resolve(&appearance, &names(&[BUNDLED_FAMILY, "Menlo"]));
        assert_eq!(config.family, "Menlo");
        assert_eq!(config.size, px(20.0));
        // User fallbacks are honoured between the primary family and the built-in chain.
        appearance.font_family = "Missing".into();
        appearance.font_fallbacks = vec!["Menlo".into()];
        let config = resolve(&appearance, &names(&[BUNDLED_FAMILY, "Menlo"]));
        assert_eq!(config.family, "Menlo");
    }

    #[test]
    fn bundled_files_are_embedded() {
        assert_eq!(embedded_paths().count(), 4);
        for path in embedded_paths() {
            let bytes = embedded(path).expect("embedded");
            // TrueType / OpenType magic numbers.
            assert!(bytes.starts_with(&[0, 1, 0, 0]) || bytes.starts_with(b"OTTO"));
        }
        assert!(embedded("fonts/missing.ttf").is_none());
    }
}
