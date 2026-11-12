//! Picking which 1x–4x variant of an image to download.
//!
//! An emote is laid out at `base size × emote_scale` logical pixels, and the window maps one
//! logical pixel to `display_scale` device pixels. So it needs `display_scale × emote_scale`
//! source pixels per base pixel to look sharp. We take the smallest variant at least that
//! dense (less to download, decode and keep in memory) and fall back to the largest one
//! below it when the provider has nothing denser.

use gpui_chat_core::{ImageScale, ImageUrls};

/// Densities within this much above a whole number still round down (`2.02` → 2x): the
/// difference is invisible and avoids doubling the download for a rounding error.
const TOLERANCE: f32 = 0.05;

/// The variant density wanted for a display scale (e.g. 2.0 on Retina) and the user's
/// emote-scale setting (e.g. 1.5).
pub fn wanted_scale(display_scale: f32, emote_scale: f32) -> ImageScale {
    let density = display_scale * emote_scale;
    if !density.is_finite() || density <= 1.0 + TOLERANCE {
        return ImageScale::X1;
    }
    match (density - TOLERANCE).ceil() as u32 {
        0 | 1 => ImageScale::X1,
        2 => ImageScale::X2,
        3 => ImageScale::X3,
        _ => ImageScale::X4,
    }
}

/// A chosen variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selected<'a> {
    /// The variant's density relative to the provider's 1x size.
    pub scale: ImageScale,
    pub url: &'a str,
}

impl Selected<'_> {
    /// Size in logical pixels to draw an image of `pixel_size` from this variant at, given the
    /// emote-scale setting. Independent of which variant was picked.
    pub fn logical_size(&self, pixel_size: (u32, u32), emote_scale: f32) -> (f32, f32) {
        logical_size(pixel_size, self.scale, emote_scale)
    }
}

/// The best URL for this display and emote scale, or `None` if `urls` is empty.
pub fn select(urls: &ImageUrls, display_scale: f32, emote_scale: f32) -> Option<Selected<'_>> {
    urls.best_for(wanted_scale(display_scale, emote_scale))
        .map(|(scale, url)| Selected { scale, url })
}

/// Logical draw size of an image whose pixels come from the `variant` density:
/// `pixels / variant × emote_scale`.
pub fn logical_size(pixel_size: (u32, u32), variant: ImageScale, emote_scale: f32) -> (f32, f32) {
    let f = f32::from(variant.factor());
    (
        pixel_size.0 as f32 / f * emote_scale,
        pixel_size.1 as f32 / f * emote_scale,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wanted_scale_rounds_up_to_sharp_enough() {
        assert_eq!(wanted_scale(1.0, 1.0), ImageScale::X1);
        assert_eq!(wanted_scale(1.0, 1.04), ImageScale::X1);
        assert_eq!(wanted_scale(1.0, 1.25), ImageScale::X2);
        assert_eq!(wanted_scale(2.0, 1.0), ImageScale::X2);
        assert_eq!(wanted_scale(2.0, 1.01), ImageScale::X2);
        assert_eq!(wanted_scale(2.0, 1.25), ImageScale::X3);
        assert_eq!(wanted_scale(1.5, 2.0), ImageScale::X3);
        assert_eq!(wanted_scale(2.0, 2.0), ImageScale::X4);
        assert_eq!(wanted_scale(3.0, 4.0), ImageScale::X4);
        assert_eq!(wanted_scale(0.5, 1.0), ImageScale::X1);
        assert_eq!(wanted_scale(f32::NAN, 1.0), ImageScale::X1);
    }

    #[test]
    fn select_uses_available_variants() {
        // Twitch: 1x, 2x and 4x (`3.0`), no 3x.
        let twitch = gpui_chat_core::Emote::twitch("25", "Kappa").urls;
        let s = select(&twitch, 2.0, 1.0).unwrap();
        assert_eq!(s.scale, ImageScale::X2);
        assert!(s.url.ends_with("/2.0"));
        // 3x wanted, not offered: go up to 4x rather than blur a 2x.
        let s = select(&twitch, 2.0, 1.5).unwrap();
        assert_eq!(s.scale, ImageScale::X4);
        assert!(s.url.ends_with("/3.0"));
        // Only 1x offered: use it even on Retina.
        let low = ImageUrls::new().with(ImageScale::X1, "a");
        assert_eq!(select(&low, 2.0, 1.0).unwrap().scale, ImageScale::X1);
        assert_eq!(select(&ImageUrls::new(), 1.0, 1.0), None);
    }

    #[test]
    fn logical_size_is_independent_of_variant() {
        // Kappa: 28 px at 1x, 56 at 2x, 112 at 4x.
        assert_eq!(logical_size((28, 28), ImageScale::X1, 1.0), (28.0, 28.0));
        assert_eq!(logical_size((56, 56), ImageScale::X2, 1.0), (28.0, 28.0));
        assert_eq!(logical_size((112, 112), ImageScale::X4, 1.5), (42.0, 42.0));
        let s = Selected {
            scale: ImageScale::X2,
            url: "u",
        };
        assert_eq!(s.logical_size((84, 56), 1.0), (42.0, 28.0));
    }
}
