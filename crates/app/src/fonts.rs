//! Bundled JetBrains Mono and the default font-family chain.
//!
//! The four upstream TTFs (OFL-1.1, see `assets/fonts/OFL.txt`) are compiled in and
//! registered with the text system at startup. The family actually used is picked once:
//! `JetBrainsMono Nerd Font` if installed, else the bundled `JetBrains Mono`, else a system
//! monospace. GPUI panics when it lays out text in a family that does not exist, so the pick
//! is made against the installed-family list instead of being left to a fallback cascade.
//!
//! Family and size are hard-coded defaults until PR 0.4 wires them to settings.

use std::borrow::Cow;

use gpui_kit::{App, Global, Pixels, SharedString, px};

/// Family name of the bundled fonts, as stored in the TTF name tables.
pub const BUNDLED_FAMILY: &str = "JetBrains Mono";
/// Preferred family when the user has the Nerd Font patched build installed.
pub const NERD_FAMILY: &str = "JetBrainsMono Nerd Font";
/// GPUI's virtual system UI family; the last resort.
const SYSTEM_UI_FAMILY: &str = ".SystemUIFont";

/// Default font size (hard-coded until settings are wired in PR 0.4).
pub const DEFAULT_FONT_SIZE: Pixels = px(14.0);

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

/// Registers the bundled fonts and stores the resolved [`FontConfig`] as a global.
pub fn init(cx: &mut App) {
    let faces: Vec<Cow<'static, [u8]>> = FONT_FILES
        .iter()
        .map(|(_, bytes)| Cow::Borrowed(*bytes))
        .collect();
    if let Err(error) = cx.text_system().add_fonts(faces) {
        tracing::warn!(%error, "failed to register bundled JetBrains Mono");
    }
    let installed = cx.text_system().all_font_names();
    let family = pick_family(&installed);
    tracing::info!(%family, "resolved UI font family");
    cx.set_global(FontConfig {
        family,
        size: DEFAULT_FONT_SIZE,
    });
}

/// Walks the default chain and returns the first family present in `installed`.
pub fn pick_family(installed: &[String]) -> SharedString {
    [NERD_FAMILY, BUNDLED_FAMILY]
        .into_iter()
        .chain(SYSTEM_MONO_FAMILIES.iter().copied())
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
        assert_eq!(pick_family(&installed), NERD_FAMILY);
    }

    #[test]
    fn bundled_font_is_next() {
        let installed = names(&[BUNDLED_FAMILY, "Menlo"]);
        assert_eq!(pick_family(&installed), BUNDLED_FAMILY);
    }

    #[test]
    fn system_monospace_then_system_ui() {
        let mono = SYSTEM_MONO_FAMILIES[0];
        assert_eq!(pick_family(&names(&[mono])), mono);
        assert_eq!(pick_family(&names(&["Helvetica"])), SYSTEM_UI_FAMILY);
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
