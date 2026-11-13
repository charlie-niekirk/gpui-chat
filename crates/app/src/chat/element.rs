//! [`MessageElement`]: one GPUI element per chat message.
//!
//! It implements `Element` directly (no child elements): `request_layout` registers a measured
//! layout node whose measure function asks the [`LayoutCache`] for the message's layout at the
//! offered width (shaping with the window text system on a miss, re-flowing on a width change);
//! `paint` draws the backgrounds, every glyph at its flowed position, and the link underlines /
//! deleted strikethrough. Colours are resolved here, from the current [`Palette`], never cached.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::Message;
use gpui_kit::{
    App, AvailableSpace, Bounds, Element, ElementId, Font, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, Pixels, Point, StrikethroughStyle, Style, TextRun,
    UnderlineStyle, Window, fill, point, px, size, underline_y_offset,
};

use super::colors;
use super::display::{self, DisplayOptions, Role};
use super::flow::AtomKind;
use super::layout::{LayoutCache, LayoutKey, MessageLayout, ShapedMessage};
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

/// Shapes `message` as one line with the window text system.
fn shape(message: &Message, style: &MessageStyle, window: &Window) -> ShapedMessage {
    let display = display::build(message, style.options);
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
    ShapedMessage::from_layout(display, &layout)
}

/// The cached layout of message `seq` for a content width (width minus padding).
fn layout_for(
    cache: &RefCell<LayoutCache>,
    seq: u64,
    message: &Arc<Message>,
    style: &MessageStyle,
    content_width: Pixels,
    window: &Window,
) -> MessageLayout {
    let key = LayoutKey::new(style.generation, window.scale_factor());
    cache
        .borrow_mut()
        .layout(seq, message, key, content_width.as_f32(), || {
            shape(message, style, window)
        })
}

impl IntoElement for MessageElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MessageElement {
    type RequestLayoutState = ();
    type PrepaintState = MessageLayout;

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
            move |known, available, window, _cx| {
                let width = known.width.unwrap_or(match available.width {
                    AvailableSpace::Definite(width) => width,
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => px(f32::INFINITY),
                });
                let content = width - style.padding.x * 2.0;
                let layout = layout_for(&cache, seq, &message, &style, content, window);
                let width = if width.as_f32().is_finite() {
                    width
                } else {
                    px(layout.flow.content_width()) + style.padding.x * 2.0
                };
                let lines = layout.flow.line_count() as f32;
                size(width, style.line_height * lines + style.padding.y * 2.0)
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
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let content = bounds.size.width - self.style.padding.x * 2.0;
        layout_for(
            &self.cache,
            self.seq,
            &self.message,
            &self.style,
            content,
            window,
        )
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let style = &*self.style;
        let palette = &style.palette;
        let message = &*self.message;
        let flags = message.flags;
        let MessageLayout { shaped, flow } = layout;

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
            let line_top = |line: usize| origin.y + line_height * line as f32;

            for glyph in &shaped.glyphs {
                let index = glyph.atom as usize;
                let Some(atom) = shaped.atoms.get(index) else {
                    continue;
                };
                if atom.kind == AtomKind::Space {
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
                    if !is_link || atom.kind == AtomKind::Space {
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
