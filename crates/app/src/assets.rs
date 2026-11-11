//! Application asset source: bundled fonts plus GPUI Kit's icon set.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

use crate::fonts;

/// Serves the bundled font files and falls through to Kit's icons for everything else.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = fonts::embedded(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut listed: Vec<SharedString> = fonts::embedded_paths()
            .filter(|candidate| candidate.starts_with(path))
            .map(SharedString::from)
            .collect();
        listed.extend(gpui_kit::assets::Assets.list(path)?);
        Ok(listed)
    }
}
