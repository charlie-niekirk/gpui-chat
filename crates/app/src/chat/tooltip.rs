//! Emote hover tooltips: a larger preview, the emote's name and its provider. A zero-width stack
//! lists every emote in it side by side, base first.
//!
//! GPUI's own tooltip machinery (`div().tooltip(..)`) hangs off a stateful element's hitbox and
//! is crate-private, while emotes are regions inside one [`super::element::MessageElement`]. So
//! the chat view keeps one [`EmoteHover`] shared with its elements:
//!
//! * each message with emotes inserts a non-blocking hitbox (scroll wheel events still reach the
//!   list) and, on mouse move, tells the hover which of its emotes is under the pointer;
//! * after [`SHOW_DELAY`] over the same emote the tooltip view is built and the view re-renders;
//!   the message that owns the emote then requests it with `Window::set_tooltip` in its
//!   prepaint, every frame while it stays hovered. GPUI positions it next to the pointer and
//!   draws it above everything;
//! * moving off the emote, scrolling, or the message scrolling out of view (it is no longer
//!   prepainted, so nobody requests the tooltip) hides it.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::Emote;
use gpui_chat_media::ImageKey;
use gpui_kit::{
    AnyTooltip, AnyView, App, AppContext as _, Bounds, Context, Corners, EntityId, FontWeight,
    IntoElement, ParentElement, Pixels, Point, Render, Styled, Task, Window, canvas, div, fill, px,
};

use crate::fonts::FontConfig;
use crate::images::{FrameImages, Images, Lookup};
use crate::theme::Tokens;

/// How long the pointer rests on an emote before its tooltip shows.
pub const SHOW_DELAY: Duration = Duration::from_millis(250);

/// One emote in one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub seq: u64,
    /// The emote's atom index in the message's layout.
    pub atom: u32,
}

/// Whether the pointer is still over an emote's box (the tooltip's visibility check).
pub fn still_over(rect: Bounds<Pixels>, pointer: Point<Pixels>) -> bool {
    rect.contains(&pointer)
}

/// The view's hover state, shared with its message elements.
#[derive(Default)]
pub struct EmoteHover {
    target: Option<Target>,
    /// Built once the pointer has rested on `target` for [`SHOW_DELAY`].
    shown: Option<(AnyView, Point<Pixels>)>,
    _show: Option<Task<()>>,
}

impl EmoteHover {
    /// The current target, shown or still waiting for the delay.
    pub fn target(&self) -> Option<Target> {
        self.target
    }

    /// The tooltip to request for `target`, if it is the one shown. `rect` is the emote's box in
    /// window coordinates this frame.
    ///
    /// GPUI asks `check_visible_and_update` every frame before drawing it. It hides the tooltip
    /// as soon as the pointer is no longer over the box, also when nothing moved the pointer:
    /// new rows pushed the emote away, or the list scrolled. Hit-testing cannot be used there:
    /// GPUI runs it before the frame's hit test is computed, and hitbox ids are not reused
    /// between frames, so a hitbox from this frame never tests as hovered at that point (its
    /// div tooltips check bounds for the same reason). Leaving the window is handled by the
    /// element's `MouseExitEvent` listener, since the pointer position is not updated then.
    pub fn tooltip_for(
        this: &Rc<RefCell<Self>>,
        target: Target,
        rect: Bounds<Pixels>,
    ) -> Option<AnyTooltip> {
        let hover = this.borrow();
        if hover.target != Some(target) {
            return None;
        }
        let (view, mouse_position) = hover.shown.clone()?;
        let weak = Rc::downgrade(this);
        Some(AnyTooltip {
            view,
            mouse_position,
            check_visible_and_update: Rc::new(move |_, window, _| {
                let visible = still_over(rect, window.mouse_position());
                if !visible
                    && let Some(this) = weak.upgrade()
                    && let Ok(mut hover) = this.try_borrow_mut()
                    && hover.target == Some(target)
                {
                    // Coming back to the emote starts the show delay again.
                    hover.clear();
                }
                visible
            }),
        })
    }

    /// A message's mouse-move report: `over` is the emote of message `seq` under the pointer, if
    /// any. Starts the show delay for a new target; hides when the pointer left this message's
    /// target. `view` is re-rendered whenever what is shown changes.
    pub fn hover(
        this: &Rc<RefCell<Self>>,
        seq: u64,
        over: Option<(u32, Vec<Arc<Emote>>)>,
        view: EntityId,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut hover = this.borrow_mut();
        match over {
            Some((atom, emotes)) => {
                let target = Target { seq, atom };
                if hover.target == Some(target) {
                    return;
                }
                let was_shown = hover.shown.take().is_some();
                hover.target = Some(target);
                hover._show = Some(Self::show_later(
                    Rc::downgrade(this),
                    target,
                    emotes,
                    view,
                    window,
                    cx,
                ));
                drop(hover);
                if was_shown {
                    cx.notify(view);
                }
            }
            None => {
                if hover.target.is_some_and(|target| target.seq == seq) {
                    let was_shown = hover.clear();
                    drop(hover);
                    if was_shown {
                        cx.notify(view);
                    }
                }
            }
        }
    }

    /// Hides everything (scrolling). Returns whether a tooltip was shown.
    pub fn clear(&mut self) -> bool {
        self.target = None;
        self._show = None;
        self.shown.take().is_some()
    }

    fn show_later(
        this: Weak<RefCell<Self>>,
        target: Target,
        emotes: Vec<Arc<Emote>>,
        view: EntityId,
        window: &Window,
        cx: &App,
    ) -> Task<()> {
        let delay = cx.background_executor().timer(SHOW_DELAY);
        window.spawn(cx, async move |cx| {
            delay.await;
            let _ = cx.update(|window, cx| {
                let Some(this) = this.upgrade() else {
                    return;
                };
                if this.borrow().target != Some(target) {
                    return;
                }
                let tooltip = cx.new(|_| EmoteTooltip::new(emotes));
                this.borrow_mut().shown = Some((tooltip.into(), window.mouse_position()));
                cx.notify(view);
            });
        })
    }
}

/// The tooltip view: for each emote of the box (one, or a zero-width stack), preview, name and
/// provider.
pub struct EmoteTooltip {
    emotes: Vec<Arc<Emote>>,
    frame: Rc<RefCell<FrameImages>>,
}

impl EmoteTooltip {
    pub fn new(emotes: Vec<Arc<Emote>>) -> Self {
        Self {
            emotes,
            frame: Rc::default(),
        }
    }
}

impl Render for EmoteTooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        let family = cx.global::<FontConfig>().family.clone();
        let light = tokens.main_bg.l > 0.5;
        self.frame.borrow_mut().begin_frame();
        Images::sweep(window, cx);
        let entity = cx.entity_id();
        let placeholder = tokens.text_muted.opacity(0.15);
        // A stack's columns are narrow: "7TV · zero-width", not "7TV emote · zero-width".
        let noun = if self.emotes.len() > 1 { "" } else { " emote" };
        let layers: Vec<_> = self
            .emotes
            .iter()
            .map(|emote| {
                let preview: Option<(ImageKey, (f32, f32))> = cx
                    .try_global::<Images>()
                    .is_some()
                    .then(|| {
                        cx.global_mut::<Images>()
                            .preview(emote, window.scale_factor(), light)
                    })
                    .flatten();
                let animated = preview.as_ref().is_some_and(|(url, _)| {
                    cx.try_global::<Images>()
                        .is_some_and(|images| images.is_animated(url))
                });
                let frame = self.frame.clone();
                let preview = preview.map(|(url, (w, h))| {
                    canvas(
                        |_, _, _| {},
                        move |bounds: Bounds<Pixels>, (), window, cx| {
                            paint_preview(&url, bounds, &frame, entity, placeholder, window, cx);
                        },
                    )
                    .w(px(w))
                    .h(px(h))
                });
                let kind = format!(
                    "{}{noun}{}{}",
                    emote.provider.name(),
                    if emote.zero_width {
                        " · zero-width"
                    } else {
                        ""
                    },
                    if animated { " · animated" } else { "" }
                );
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .children(preview)
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child(emote.name.to_string()),
                    )
                    .child(div().text_xs().text_color(tokens.text_muted).child(kind))
            })
            .collect();
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap_5()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .bg(tokens.window_bg)
            .shadow_md()
            .font_family(family)
            .text_sm()
            .text_color(tokens.text)
            .children(layers)
    }
}

/// Paints the preview at the global clock's frame, and schedules the next frame.
fn paint_preview(
    url: &ImageKey,
    bounds: Bounds<Pixels>,
    frame: &RefCell<FrameImages>,
    entity: EntityId,
    placeholder: gpui_kit::Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(images) = cx.try_global::<Images>() else {
        return;
    };
    let animate = crate::config::current(cx).emotes.animate;
    let clock = animate.then(|| images.clock(std::time::Instant::now()));
    let mut frame = frame.borrow_mut();
    match cx.global_mut::<Images>().lookup(url, &mut frame, entity) {
        Lookup::Ready(image) if image.is_drawable() => {
            let (index, next) = image.frame(clock);
            if let Some(next) = next {
                frame.animate_in(next);
            }
            if let Err(error) = window.paint_image(
                bounds,
                bounds,
                Corners::default(),
                image.render_image().clone(),
                index,
                false,
            ) {
                tracing::debug!(%error, "failed to paint an emote preview");
            }
        }
        Lookup::Loading | Lookup::Retrying => {
            window.paint_quad(fill(bounds, placeholder).corner_radii(px(4.0)));
        }
        Lookup::Ready(_) | Lookup::Failed { .. } => {}
    }
    Images::arm_retry(cx);
    frame.schedule_wake(entity, animate, window.is_window_active(), cx);
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Modifiers, TestAppContext, point, size};

    use super::*;

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui_kit::test]
    fn the_tooltip_hides_once_the_pointer_is_off_the_emote(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| Empty);
        let hover = Rc::new(RefCell::new(EmoteHover::default()));
        let target = Target { seq: 1, atom: 2 };
        {
            let mut h = hover.borrow_mut();
            h.target = Some(target);
            h.shown = Some((view.into(), point(px(20.0), px(20.0))));
        }
        let rect = Bounds::new(point(px(10.0), px(10.0)), size(px(28.0), px(28.0)));
        // Only the hovered emote asks for it.
        assert!(EmoteHover::tooltip_for(&hover, Target { seq: 1, atom: 3 }, rect).is_none());

        cx.simulate_mouse_move(point(px(20.0), px(20.0)), None, Modifiers::none());
        let check = |rect, cx: &mut gpui_kit::VisualTestContext| {
            let tooltip = EmoteHover::tooltip_for(&hover, target, rect)?;
            Some(cx.update(|window, cx| {
                (tooltip.check_visible_and_update)(Bounds::default(), window, cx)
            }))
        };
        assert_eq!(check(rect, cx), Some(true));
        // New rows pushed the emote 50 px up under a still pointer: hidden, and forgotten so
        // coming back waits for the show delay again.
        let moved = Bounds::new(point(px(10.0), px(-40.0)), rect.size);
        assert_eq!(check(moved, cx), Some(false));
        assert_eq!(hover.borrow().target(), None);
        assert_eq!(check(rect, cx), None);
    }
}
