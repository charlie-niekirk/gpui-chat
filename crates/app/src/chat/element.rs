//! [`MessageElement`]: one GPUI element per chat message.
//!
//! It implements `Element` directly (no child elements): `request_layout` registers a measured
//! layout node whose measure function asks the [`LayoutCache`] for the message's layout at the
//! offered width (shaping with the window text system on a miss, re-flowing on a width change);
//! `paint` draws the backgrounds, every glyph at its flowed position, the emote images, and the
//! link underlines / deleted strikethrough. Colours are resolved here, from the current
//! [`Palette`], never cached.
//!
//! **Lines.** Each flowed line is as tall as the text line height or its tallest emote; text and
//! emotes are centred vertically in it. **Emotes** are boxes of their final size from the moment
//! the message is shaped: a faint placeholder while the image loads, then the image at the frame
//! the global animation clock selects (greyscale in a deleted message). Hovering one shows a
//! tooltip ([`super::tooltip`]).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::{Emote, Message};
use gpui_kit::{
    App, AvailableSpace, Bounds, Corners, Element, ElementId, Font, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId, MouseExitEvent,
    MouseMoveEvent, Pixels, Point, ScrollEvent, StrikethroughStyle, Style, TextRun, UnderlineStyle,
    Window, fill, point, px, size, underline_y_offset,
};

use super::colors;
use super::display::{self, DisplayOptions, Role};
use super::flow::{AtomKind, Flow};
use super::layout::{LayoutCache, LayoutKey, MessageLayout, ShapedMessage};
use super::tooltip::{EmoteHover, Target};
use crate::images::{EmoteBox, FrameImages, Images, Lookup};
use crate::theme::Tokens;

/// Theme colours the element paints with.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// The chat background; usernames are made readable against it.
    pub background: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub link: Hsla,
    /// An `@viewer` mention.
    pub self_mention: Hsla,
    /// Behind a message that mentions the viewer.
    pub mention_bg: Hsla,
    /// Behind a highlighted message (channel points, rules).
    pub highlight_bg: Hsla,
}

impl Palette {
    pub fn from_tokens(tokens: &Tokens) -> Self {
        let link = colors::readable_hsla(tokens.accent, tokens.main_bg);
        Self {
            background: tokens.main_bg,
            text: tokens.text,
            muted: tokens.text_muted,
            link,
            self_mention: link,
            mention_bg: tokens.accent.opacity(0.18),
            highlight_bg: tokens.accent.opacity(0.09),
        }
    }

    fn role(&self, role: Role, name: Hsla) -> Hsla {
        match role {
            Role::Timestamp | Role::System => self.muted,
            Role::Name | Role::ActionText => name,
            Role::SelfMention => self.self_mention,
            Role::Link => self.link,
            Role::Separator
            | Role::Text
            | Role::Mention
            | Role::Emote
            | Role::Emoji
            | Role::Cheer => self.text,
        }
    }
}

/// How emotes are drawn in one render.
#[derive(Clone, Copy, Debug)]
pub struct EmoteStyle {
    /// The `emotes.scale` setting (part of the layout generation).
    pub scale: f32,
    /// The global animation clock for this frame; `None` when animation is off.
    pub clock: Option<Duration>,
    /// The theme is light (Twitch serves light-background variants).
    pub light: bool,
}

/// Everything about how messages are drawn that is shared by every message in one render.
pub struct MessageStyle {
    pub font: Font,
    pub bold_font: Font,
    pub font_size: Pixels,
    pub line_height: Pixels,
    pub options: DisplayOptions,
    /// Changes whenever anything above changes; part of the [`LayoutKey`].
    pub generation: u64,
    /// Horizontal / vertical padding around a message.
    pub padding: Point<Pixels>,
    pub palette: Palette,
    pub emotes: EmoteStyle,
    /// The images the view painted (pinned while shown) and its animation timer.
    pub images: Rc<RefCell<FrameImages>>,
    pub hover: Rc<RefCell<EmoteHover>>,
}

/// One message in the chat list.
pub struct MessageElement {
    seq: u64,
    message: Arc<Message>,
    style: Rc<MessageStyle>,
    cache: Rc<RefCell<LayoutCache>>,
}

impl MessageElement {
    pub fn new(
        seq: u64,
        message: Arc<Message>,
        style: Rc<MessageStyle>,
        cache: Rc<RefCell<LayoutCache>>,
    ) -> Self {
        Self {
            seq,
            message,
            style,
            cache,
        }
    }
}

/// Decides each emote's box (starting its image load). Without an image store every emote is
/// text.
fn emote_boxes(
    emotes: &[(usize, Arc<Emote>)],
    style: &MessageStyle,
    window: &Window,
    cx: &mut App,
) -> Vec<EmoteBox> {
    if emotes.is_empty() || !cx.has_global::<Images>() {
        return Vec::new();
    }
    let images = cx.global_mut::<Images>();
    let scale = window.scale_factor();
    emotes
        .iter()
        .map(|(_, emote)| images.emote_box(emote, scale, style.emotes.scale, style.emotes.light))
        .collect()
}

/// Shapes `message` as one line with the window text system.
fn shape(message: &Message, style: &MessageStyle, window: &Window, cx: &mut App) -> ShapedMessage {
    let display = display::build(message, style.options);
    let boxes = emote_boxes(&display.emotes, style, window, cx);
    let runs: Vec<TextRun> = display
        .spans
        .iter()
        .map(|span| TextRun {
            len: span.range.len(),
            font: if span.role.is_bold() {
                style.bold_font.clone()
            } else {
                style.font.clone()
            },
            // Colour does not affect shaping; it is chosen at paint time.
            color: style.palette.text,
            background_color: None,
            underline: None,
            strikethrough: None,
        })
        .collect();
    let layout = window
        .text_system()
        .layout_line(&display.text, style.font_size, &runs, None);
    ShapedMessage::from_layout(display, &layout, &boxes)
}

/// The cached layout of message `seq` for a content width (width minus padding).
fn layout_for(
    cache: &RefCell<LayoutCache>,
    seq: u64,
    message: &Arc<Message>,
    style: &MessageStyle,
    content_width: Pixels,
    window: &Window,
    cx: &mut App,
) -> MessageLayout {
    let key = LayoutKey::new(style.generation, window.scale_factor());
    cache.borrow_mut().layout(
        seq,
        message,
        key,
        content_width.as_f32(),
        style.line_height.as_f32(),
        || shape(message, style, window, cx),
    )
}

/// Where an inline atom is drawn, relative to the content origin: centred in its line.
fn inline_rect(flow: &Flow, shaped: &ShapedMessage, atom: usize) -> Bounds<Pixels> {
    let a = &shaped.atoms[atom];
    let place = flow.placements[atom];
    let line = &flow.lines[place.line as usize];
    Bounds::new(
        point(px(place.x), px(line.top + (line.height - a.height) / 2.0)),
        size(px(a.width), px(a.height)),
    )
}

/// What prepaint hands to paint.
pub struct Prepainted {
    layout: MessageLayout,
    /// For messages with emotes: hover detection.
    hitbox: Option<Hitbox>,
}

impl MessageElement {
    /// Draws the emote boxes (image, or placeholder while loading) and registers hover handling.
    fn paint_emotes(
        &self,
        prepainted: &Prepainted,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let MessageLayout { shaped, flow } = &prepainted.layout;
        if shaped.inlines.is_empty() || !cx.has_global::<Images>() {
            return;
        }
        let style = &*self.style;
        let view = window.current_view();
        let grayscale = self.message.flags.deleted;
        let placeholder = style.palette.muted.opacity(0.15);
        let mut regions = Vec::with_capacity(shaped.inlines.len());
        let mut frame = style.images.borrow_mut();
        for inline in &shaped.inlines {
            let rect = inline_rect(flow, shaped, inline.atom as usize);
            let rect = Bounds::new(origin + rect.origin, rect.size);
            regions.push((rect, inline.atom, inline.emote.clone()));
            match cx
                .global_mut::<Images>()
                .lookup(&inline.url, &mut frame, view)
            {
                Lookup::Ready(image) if image.is_drawable() => {
                    let (index, next) = image.frame(style.emotes.clock);
                    if let Some(next) = next {
                        frame.animate_in(next);
                    }
                    let result = window.paint_image(
                        rect,
                        rect,
                        Corners::default(),
                        image.render_image().clone(),
                        index,
                        grayscale,
                    );
                    if let Err(error) = result {
                        tracing::debug!(%error, "failed to paint an emote");
                    }
                }
                Lookup::Loading | Lookup::Retrying => {
                    window.paint_quad(fill(rect, placeholder).corner_radii(px(3.0)));
                }
                // Failed for good: the epoch moved, so the next render re-shapes it as text.
                Lookup::Failed { relayout } => {
                    if relayout {
                        window.request_animation_frame();
                    }
                }
                Lookup::Ready(_) => {}
            }
        }
        drop(frame);
        Images::arm_retry(cx);

        let Some(hitbox) = prepainted.hitbox.clone() else {
            return;
        };
        let seq = self.seq;
        let hover = style.hover.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if !phase.bubble() {
                return;
            }
            let over = hitbox
                .is_hovered(window)
                .then(|| {
                    regions
                        .iter()
                        .find(|(rect, ..)| rect.contains(&event.position))
                        .map(|(_, atom, emote)| (*atom, emote.clone()))
                })
                .flatten();
            EmoteHover::hover(&hover, seq, over, view, window, cx);
        });
        // Scrolling, or the pointer leaving the window (its position is not updated then, so the
        // tooltip's own check would still see it over the emote): hide.
        let hide = {
            let hover = style.hover.clone();
            move |cx: &mut App| {
                let mut hover = hover.borrow_mut();
                if hover.target().is_some_and(|target| target.seq == seq) && hover.clear() {
                    drop(hover);
                    cx.notify(view);
                }
            }
        };
        let on_exit = hide.clone();
        window.on_mouse_event(move |_: &ScrollEvent, _, _, cx| hide(cx));
        window.on_mouse_event(move |_: &MouseExitEvent, _, _, cx| on_exit(cx));
    }
}

impl IntoElement for MessageElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MessageElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let (seq, message, style, cache) = (
            self.seq,
            self.message.clone(),
            self.style.clone(),
            self.cache.clone(),
        );
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |known, available, window, cx| {
                let width = known.width.unwrap_or(match available.width {
                    AvailableSpace::Definite(width) => width,
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => px(f32::INFINITY),
                });
                let content = width - style.padding.x * 2.0;
                let layout = layout_for(&cache, seq, &message, &style, content, window, cx);
                let width = if width.as_f32().is_finite() {
                    width
                } else {
                    px(layout.flow.content_width()) + style.padding.x * 2.0
                };
                size(width, px(layout.flow.height()) + style.padding.y * 2.0)
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let content = bounds.size.width - self.style.padding.x * 2.0;
        let layout = layout_for(
            &self.cache,
            self.seq,
            &self.message,
            &self.style,
            content,
            window,
            cx,
        );
        if layout.shaped.inlines.is_empty() {
            return Prepainted {
                layout,
                hitbox: None,
            };
        }
        // Normal behaviour: it only answers "is the pointer over this message"; wheel events
        // still reach the list.
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let origin = bounds.origin + self.style.padding;
        for inline in &layout.shaped.inlines {
            let target = Target {
                seq: self.seq,
                atom: inline.atom,
            };
            let rect = inline_rect(&layout.flow, &layout.shaped, inline.atom as usize);
            let rect = Bounds::new(origin + rect.origin, rect.size);
            if let Some(tooltip) = EmoteHover::tooltip_for(&self.style.hover, target, rect) {
                window.set_tooltip(tooltip);
            }
        }
        Prepainted {
            layout,
            hitbox: Some(hitbox),
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepainted: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let style = &*self.style;
        let palette = &style.palette;
        let message = &*self.message;
        let flags = message.flags;
        let MessageLayout { shaped, flow } = &prepainted.layout;

        window.paint_layer(bounds, |window| {
            if flags.mention {
                window.paint_quad(fill(bounds, palette.mention_bg));
            } else if flags.highlighted {
                window.paint_quad(fill(bounds, palette.highlight_bg));
            }

            let name = message.author.as_ref().map_or(palette.text, |author| {
                let color = author
                    .color
                    .unwrap_or_else(|| colors::fallback_color(&author.login));
                colors::readable(color, palette.background)
            });
            let origin = bounds.origin + style.padding;
            let line_height = style.line_height;
            let baseline = (line_height - shaped.ascent - shaped.descent) / 2.0 + shaped.ascent;
            // Top of the text in `line`: the line-height box centred in the (maybe taller) line.
            let line_top = |line: usize| {
                let line = &flow.lines[line];
                origin.y + px(line.top + (line.height - line_height.as_f32()) / 2.0)
            };

            for glyph in &shaped.glyphs {
                let index = glyph.atom as usize;
                let Some(atom) = shaped.atoms.get(index) else {
                    continue;
                };
                if atom.kind != AtomKind::Glyphs {
                    continue;
                }
                let place = flow.placements[index];
                let position = point(
                    origin.x + px(place.x + (glyph.x - atom.src_x)),
                    line_top(place.line as usize) + baseline,
                );
                let result = if glyph.is_emoji {
                    window.paint_emoji(position, glyph.font_id, glyph.id, style.font_size)
                } else {
                    let role = shaped.display.spans[shaped.atom_spans[index] as usize].role;
                    let color = if flags.deleted {
                        palette.muted
                    } else {
                        palette.role(role, name)
                    };
                    window.paint_glyph(position, glyph.font_id, glyph.id, style.font_size, color)
                };
                if let Err(error) = result {
                    tracing::debug!(%error, "failed to paint a glyph");
                }
            }

            self.paint_emotes(prepainted, origin, window, cx);

            // Link underlines, per line, merged across the atoms of one link span.
            let underline_y = underline_y_offset(line_height, shaped.ascent, shaped.descent);
            let underline = UnderlineStyle {
                thickness: px(1.0),
                color: Some(if flags.deleted {
                    palette.muted
                } else {
                    palette.link
                }),
                wavy: false,
            };
            for (line_ix, line) in flow.lines.iter().enumerate() {
                let mut run: Option<(u32, f32, f32)> = None;
                let flush = |run: &mut Option<(u32, f32, f32)>, window: &mut Window| {
                    if let Some((_, start, end)) = run.take() {
                        window.paint_underline(
                            point(origin.x + px(start), line_top(line_ix) + underline_y),
                            px(end - start),
                            &underline,
                        );
                    }
                };
                for i in line.atoms.clone() {
                    let atom = &shaped.atoms[i];
                    let span = shaped.atom_spans[i];
                    let is_link = shaped.display.spans[span as usize].role == Role::Link;
                    if !is_link || atom.kind != AtomKind::Glyphs {
                        flush(&mut run, window);
                        continue;
                    }
                    let start = flow.placements[i].x;
                    let end = start + atom.width;
                    match &mut run {
                        Some((current, _, run_end)) if *current == span => *run_end = end,
                        _ => {
                            flush(&mut run, window);
                            run = Some((span, start, end));
                        }
                    }
                }
                flush(&mut run, window);

                if flags.deleted && line.width > 0.0 {
                    window.paint_strikethrough(
                        point(origin.x, line_top(line_ix) + baseline - shaped.ascent * 0.3),
                        px(line.width),
                        &StrikethroughStyle {
                            thickness: px(1.0),
                            color: Some(palette.muted),
                        },
                    );
                }
            }
        });
    }
}
