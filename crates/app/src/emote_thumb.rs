//! A small still emote image for the completion list and the emote picker (PR 3.3).
//!
//! It goes through the same image store as chat ([`Images`]): requests are de-duplicated and cached,
//! a finished load notifies the view that painted a placeholder, and the image is pinned in the
//! view's [`FrameImages`] while it is on screen. Animated emotes show their first frame (the picker
//! is for finding an emote, not watching it; chat animates them once sent).

use std::cell::RefCell;
use std::rc::Rc;

use gpui_chat_core::Emote;
use gpui_chat_media::{ImageKey, scale};
use gpui_kit::{
    App, Bounds, Corners, EntityId, Hsla, IntoElement, Pixels, Styled as _, Window, canvas, fill,
    point, px, size,
};

use crate::images::{FrameImages, Images, Lookup, themed_url};

/// Base size of a provider's 1x image (Twitch's is 28 px, BTTV's too, 7TV's 32 px): the variant is
/// chosen so one image pixel lands on one device pixel at `side`.
const BASE_SIDE: f32 = 28.0;

/// The image to show for `emote` in a box `side` logical pixels wide, or `None` without images.
pub fn key_for(emote: &Emote, side: f32, display_scale: f32, light: bool) -> Option<ImageKey> {
    let selected = scale::select(&emote.urls, display_scale, side / BASE_SIDE)?;
    Some(ImageKey::new(themed_url(selected.url, light).as_ref()))
}

/// A `side` × `side` box drawing `emote`, fitted inside it keeping the image's proportions.
/// `owner` is the view to repaint when the image finishes loading; `frame` is that view's pins
/// (call `begin_frame` once per render).
pub fn thumb(
    emote: &Emote,
    side: f32,
    placeholder: Hsla,
    light: bool,
    frame: Rc<RefCell<FrameImages>>,
    owner: EntityId,
    window: &Window,
) -> impl IntoElement {
    let key = key_for(emote, side, window.scale_factor(), light);
    image(key, side, 0.0, placeholder, frame, owner)
}

/// A `side` × `side` box drawing the image at `key` (a badge, an avatar), with corners rounded by
/// `radius` (`side / 2.0` is a circle); a placeholder while it loads, nothing if it failed.
/// `owner` and `frame` as for [`thumb`].
pub fn image(
    key: Option<ImageKey>,
    side: f32,
    radius: f32,
    placeholder: Hsla,
    frame: Rc<RefCell<FrameImages>>,
    owner: EntityId,
) -> impl IntoElement {
    image_with(key, side, radius, placeholder, frame, owner, false)
}

/// [`image`], and with `notify_failed` the owner is told to redraw once the image has failed for
/// good, so it can draw something else in its place (the user card's initial). Only for a view
/// that stops asking for the image then: an owner that keeps painting it would redraw forever.
pub fn image_with(
    key: Option<ImageKey>,
    side: f32,
    radius: f32,
    placeholder: Hsla,
    frame: Rc<RefCell<FrameImages>>,
    owner: EntityId,
    notify_failed: bool,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds: Bounds<Pixels>, (), window, cx| {
            if let Some(key) = &key {
                paint(key, bounds, radius, placeholder, &frame, owner, window, cx);
                if notify_failed && Images::failed_for_good(key, cx) {
                    cx.notify(owner);
                }
            }
        },
    )
    .w(px(side))
    .h(px(side))
}

#[allow(clippy::too_many_arguments)]
fn paint(
    key: &ImageKey,
    bounds: Bounds<Pixels>,
    radius: f32,
    placeholder: Hsla,
    frame: &RefCell<FrameImages>,
    owner: EntityId,
    window: &mut Window,
    cx: &mut App,
) {
    if cx.try_global::<Images>().is_none() {
        return;
    }
    let mut frame = frame.borrow_mut();
    match cx.global_mut::<Images>().lookup(key, &mut frame, owner) {
        Lookup::Ready(image) if image.is_drawable() => {
            let target = fit(image.pixel_size(), bounds);
            if let Err(error) = window.paint_image(
                target,
                target,
                Corners::all(px(radius)),
                image.render_image().clone(),
                0,
                false,
            ) {
                tracing::debug!(%error, "failed to paint an emote thumbnail");
            }
        }
        Lookup::Loading | Lookup::Retrying => {
            window.paint_quad(fill(bounds, placeholder).corner_radii(px(radius.max(4.0))));
        }
        Lookup::Ready(_) | Lookup::Failed { .. } => {}
    }
    Images::arm_retry(cx);
}

/// The largest box of the image's proportions that fits in `bounds`, centred.
fn fit(pixels: (u32, u32), bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let (w, h) = (pixels.0.max(1) as f32, pixels.1.max(1) as f32);
    let factor = (f32::from(bounds.size.width) / w).min(f32::from(bounds.size.height) / h);
    let (fw, fh) = (px(w * factor), px(h * factor));
    Bounds::new(
        point(
            bounds.origin.x + (bounds.size.width - fw) / 2.0,
            bounds.origin.y + (bounds.size.height - fh) / 2.0,
        ),
        size(fw, fh),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(10.0), px(20.0)), size(px(w), px(h)))
    }

    #[test]
    fn an_image_is_fitted_and_centred_in_its_box() {
        // Wider than tall: full width, centred vertically.
        let fitted = fit((56, 28), bounds(28.0, 28.0));
        assert_eq!(f32::from(fitted.size.width), 28.0);
        assert_eq!(f32::from(fitted.size.height), 14.0);
        assert_eq!(f32::from(fitted.origin.x), 10.0);
        assert_eq!(f32::from(fitted.origin.y), 27.0);
        // Square: fills it, whatever its pixel size.
        let fitted = fit((112, 112), bounds(28.0, 28.0));
        assert_eq!(f32::from(fitted.size.width), 28.0);
        assert_eq!(fitted.origin, point(px(10.0), px(20.0)));
        // A degenerate image does not divide by zero.
        let _ = fit((0, 0), bounds(28.0, 28.0));
    }

    #[test]
    fn the_variant_follows_the_box_and_the_display() {
        let emote = Emote::twitch("25", "Kappa");
        let url = |side, scale, light| {
            key_for(&emote, side, scale, light)
                .unwrap()
                .url()
                .to_owned()
        };
        // 28 px at 1x → the 1x image; at 2x → the 2x image, themed for the light background.
        assert!(url(28.0, 1.0, false).ends_with("/dark/1.0"));
        assert!(url(28.0, 2.0, false).ends_with("/dark/2.0"));
        assert!(url(28.0, 2.0, true).ends_with("/light/2.0"));
        // An emote with no image has no key.
        let bare = Emote {
            urls: gpui_chat_core::ImageUrls::new(),
            ..emote
        };
        assert!(key_for(&bare, 28.0, 2.0, false).is_none());
    }
}
