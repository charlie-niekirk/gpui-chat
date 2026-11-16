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
//! the global animation clock selects (greyscale in a deleted message). **Zero-width stacks**
//! (7TV overlays) share one box, as wide and tall as the widest and tallest layer; each layer
//! is drawn centred in it, the base first and the overlays on top. Hovering a box shows a
//! tooltip listing every emote in it ([`super::tooltip`]).
//!
//! **Badges** are drawn before the name as square boxes of one size (the same inline machinery as
//! emotes, one layer each; a faint placeholder while the image loads) with a tooltip of their
//! own. They are looked up when the message is shaped, in the channel's badge sets and then the
//! global ones ([`crate::badges`]); a badge with no definition or no usable image is not shown.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::{ChannelId, HighlightColor, Message, MessageId, ReplyRef};
use gpui_kit::{
    App, AvailableSpace, Bounds, Corners, CursorStyle, Element, ElementId, Font, GlobalElementId,
    Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId, MouseButton,
    MouseDownEvent, MouseExitEvent, MouseMoveEvent, Pixels, Point, ScrollEvent, StrikethroughStyle,
    Style, TextRun, UnderlineStyle, Window, fill, point, px, size, underline_y_offset,
};

use super::colors;
use super::display::{self, BadgeSlot, DisplayEmote, DisplayOptions, Role};
use super::events;
use super::flow::{AtomKind, Flow};
use super::layout::{LayoutCache, LayoutKey, MessageLayout, ReplyFit, ReplyLine, ShapedMessage};
use super::selection;
use super::selection_layer::Surface;
use super::tooltip::{EmoteHover, Target};
use super::view::SearchHighlight;
use crate::badges::Badges;
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
    /// Behind a message Twitch highlighted (channel points).
    pub highlight_bg: Hsla,
    /// The colours a highlight rule can name (PR 5.3), from the theme's own: accent, danger,
    /// success, warning.
    pub rule_colors: [Hsla; 4],
    /// The accent colours of event rows (PR 5.1).
    pub events: events::Colors,
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
            rule_colors: [tokens.accent, tokens.danger, tokens.success, tokens.warning],
            events: events::Colors::from_tokens(tokens),
        }
    }

    /// The theme colour a highlight rule named.
    pub fn rule_color(&self, color: HighlightColor) -> Hsla {
        self.rule_colors[match color {
            HighlightColor::Accent => 0,
            HighlightColor::Red => 1,
            HighlightColor::Green => 2,
            HighlightColor::Yellow => 3,
        }]
    }

    /// The colour of a span. `name` is the author's, `header` the event header's (see
    /// [`events::Colors::header_text`]).
    fn role(&self, role: Role, name: Hsla, header: Hsla) -> Hsla {
        match role {
            Role::Timestamp | Role::System | Role::Source => self.muted,
            Role::Header => header,
            Role::Name | Role::ActionText => name,
            Role::SelfMention => self.self_mention,
            Role::Link => self.link,
            Role::Separator
            | Role::Text
            | Role::Mention
            | Role::Emote
            | Role::Badge
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

/// How badges are drawn in one render.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BadgeStyle {
    /// Logical side of a badge's square box (part of the layout generation: it follows the font
    /// size).
    pub size: f32,
}

/// What the context line of a reply does when it is clicked: open the thread with this root.
pub type OpenThread = Rc<dyn Fn(&MessageId, &mut Window, &mut App)>;

/// The row the pointer is over (its store sequence number), shared by a view's rows: each row
/// that has it in its style keeps it current, and the view shows the Reply button on that row
/// only and resolves the right-click menu against it.
pub type HoveredRow = Rc<Cell<Option<u64>>>;

/// Opacity of the flash behind a message a jump brought into view.
const FLASH_TINT: f32 = 0.35;
/// Opacity of the tint behind a row a highlight rule hit.
const RULE_TINT: f32 = 0.16;

/// The reply context line is a fraction of the text's size: it is a quotation, not the message.
const REPLY_SCALE: f32 = 0.85;
/// Space above the reply line, so it reads as belonging to the message under it, not to the one
/// above.
const REPLY_GAP: f32 = 4.0;

/// Everything about how messages are drawn that is shared by every message in one render.
#[derive(Clone)]
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
    pub badges: BadgeStyle,
    /// The channel the view shows: where badge definitions are looked up.
    pub channel: ChannelId,
    /// The images the view painted (pinned while shown) and its animation timer.
    pub images: Rc<RefCell<FrameImages>>,
    pub hover: Rc<RefCell<EmoteHover>>,
    /// Draw a reply's context line above it. Off in the thread popover, where the thread is
    /// already the context.
    pub reply_line: bool,
    /// Clicking a reply's context line; without it the line is not interactive.
    pub on_open_thread: Option<OpenThread>,
    /// Where each row reports whether the pointer is over it; `None` when nothing needs to know
    /// (a chat without a composer).
    pub hovered_row: Option<HoveredRow>,
    /// Where rows report their geometry and find their part of the selection (PR 5.6); all
    /// selection and link handling lives there ([`super::selection_layer`]).
    pub selection: Option<Rc<Surface>>,
    /// The row to flash for a moment after a jump to it (the Mentions view, a notification):
    /// its sequence number, shared with the view that sets and clears it.
    pub flash: Option<Rc<Cell<Option<u64>>>>,
    /// The text the find bar is looking for (PR 5.7): its matches are drawn behind the text.
    pub search: Option<Rc<SearchHighlight>>,
}

/// One message in the chat list.
pub struct MessageElement {
    seq: u64,
    message: Arc<Message>,
    style: Rc<MessageStyle>,
    cache: Rc<RefCell<LayoutCache>>,
    /// Drawn as `name: <message deleted>` (PR 5.2): same element, selection and copy, another text.
    collapsed: bool,
    /// Drawn as `1 ignored message` (PR 5.7).
    ignored: bool,
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
            collapsed: false,
            ignored: false,
        }
    }

    /// Draws a removed message as its collapsed line (`chat.deleted_messages = "collapse"`).
    #[must_use]
    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    /// Draws an ignored message as the line `1 ignored message` (`ignores.show_collapsed`).
    #[must_use]
    pub fn ignored(mut self, ignored: bool) -> Self {
        self.ignored = ignored;
        self
    }
}

/// Decides each emote's box (starting its image load). Without an image store every emote is
/// text.
fn emote_boxes(
    emotes: &[DisplayEmote],
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
        .map(|entry| images.emote_box(&entry.emote, scale, style.emotes.scale, style.emotes.light))
        .collect()
}

/// A name paint and where its box is (x of the name's start in the unwrapped line, its width).
struct NamePaint {
    paint: std::sync::Arc<gpui_chat_emotes::Paint>,
    x0: f32,
    width: f32,
}

/// The paint the author's name is drawn with, if it has one that can be drawn and stays readable
/// on `background`. A read lock and a hash probe per message painted; nothing is allocated.
fn name_paint(
    message: &Message,
    extent: Option<(f32, f32)>,
    background: Hsla,
    cx: &App,
) -> Option<NamePaint> {
    let (x0, x1) = extent?;
    let user_id = message.author.as_ref()?.user_id.as_deref()?;
    let paint = cx.try_global::<Badges>()?.cosmetics().paint(user_id)?;
    colors::paint_readable(&paint, background).then(|| NamePaint {
        paint,
        x0,
        width: (x1 - x0).max(1.0),
    })
}

/// The badges of `message` that can be drawn: each one that has a definition (this channel's, then
/// the global one) and an image that has not failed for good, in message order. Starts the image
/// loads. Without a badge registry or image store there are none.
fn badge_slots(
    message: &Message,
    style: &MessageStyle,
    window: &Window,
    cx: &mut App,
) -> Vec<BadgeSlot> {
    let Some(author) = message.author.as_ref() else {
        return Vec::new();
    };
    // A message sent in another channel of a shared chat session shows the badges the author has
    // *there* (`source-badges`), looked up in that channel's sets (the generic ones when that
    // channel is not known: this channel's own subscriber badge is not theirs).
    let (ids, channel) = match message
        .shared
        .as_ref()
        .filter(|_| events::source_label(message).is_some())
    {
        Some(shared) => (
            &shared.source_badges,
            shared.source_login.as_deref().map_or_else(
                || ChannelId::new(shared.source_room_id.as_str()),
                ChannelId::new,
            ),
        ),
        None => (&author.badges, style.channel.clone()),
    };
    if ids.is_empty() {
        return Vec::new();
    }
    let (Some(badges), true) = (cx.try_global::<Badges>(), cx.has_global::<Images>()) else {
        return Vec::new();
    };
    let registry = badges.registry().clone();
    let scale = window.scale_factor();
    let size = style.badges.size;
    let images = cx.global_mut::<Images>();
    ids.iter()
        .filter_map(|id| {
            let badge = registry.lookup(&channel, id)?;
            let url = images.badge_box(&badge, size, scale)?;
            Some(BadgeSlot { badge, url, size })
        })
        .collect()
}

/// Shapes the context line of `reply`: a smaller, single line.
fn shape_reply(reply: &ReplyRef, style: &MessageStyle, window: &Window) -> ReplyLine {
    let text = ReplyLine::ellipsized(&display::reply_text(reply));
    let font_size = style.font_size * REPLY_SCALE;
    let run = TextRun {
        len: text.len(),
        font: style.font.clone(),
        color: style.palette.muted,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let layout = window
        .text_system()
        .layout_line(&text, font_size, &[run], None);
    let height = (style.line_height.as_f32() * REPLY_SCALE).round() + REPLY_GAP;
    let line = ReplyLine::from_layout(reply.root_id().clone(), font_size, &text, &layout, height);
    match display::reply_name_range(reply) {
        Some(range) => line.with_user(range, &reply.parent_login),
        None => line,
    }
}

/// Shapes `message` as one line with the window text system.
fn shape(
    message: &Message,
    mode: Mode,
    style: &MessageStyle,
    window: &Window,
    cx: &mut App,
) -> ShapedMessage {
    let Mode { collapsed, ignored } = mode;
    let display = if ignored {
        display::build_ignored(message, style.options)
    } else if collapsed {
        display::build_collapsed(message, style.options)
    } else {
        let slots = badge_slots(message, style, window, cx);
        display::build(message, style.options, &slots)
    };
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
    let look = if ignored { None } else { events::look(message) };
    let shaped = ShapedMessage::from_layout(display, &layout, &boxes).with_look(look);
    match &message.reply {
        Some(reply) if style.reply_line && !collapsed && !ignored => {
            shaped.with_reply(shape_reply(reply, style, window))
        }
        _ => shaped,
    }
}

/// The message a layout is asked for: its sequence number, the message and whether it is shown
/// collapsed (PR 5.2).
#[derive(Clone, Copy)]
struct Subject<'a> {
    seq: u64,
    message: &'a Arc<Message>,
    collapsed: bool,
    ignored: bool,
}

/// How a message is drawn instead of in full (see [`Subject`]).
#[derive(Clone, Copy)]
struct Mode {
    collapsed: bool,
    ignored: bool,
}

/// The cached layout of a message for a content width (width minus padding).
fn layout_for(
    cache: &RefCell<LayoutCache>,
    subject: Subject<'_>,
    style: &MessageStyle,
    content_width: Pixels,
    window: &Window,
    cx: &mut App,
) -> MessageLayout {
    let Subject {
        seq,
        message,
        collapsed,
        ignored,
    } = subject;
    let key = LayoutKey::new(style.generation, window.scale_factor())
        .collapsed(collapsed)
        .ignored(ignored);
    cache.borrow_mut().layout(
        seq,
        message,
        key,
        content_width.as_f32(),
        style.line_height.as_f32(),
        || shape(message, Mode { collapsed, ignored }, style, window, cx),
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

/// `size` centred in `outer`.
fn centred(outer: Bounds<Pixels>, size_: (f32, f32)) -> Bounds<Pixels> {
    let (w, h) = (px(size_.0), px(size_.1));
    Bounds::new(
        point(
            outer.origin.x + (outer.size.width - w) / 2.0,
            outer.origin.y + (outer.size.height - h) / 2.0,
        ),
        size(w, h),
    )
}

/// What prepaint hands to paint.
pub struct Prepainted {
    layout: MessageLayout,
    /// For messages with emotes: hover detection.
    hitbox: Option<Hitbox>,
    /// The reply context line as laid out for this width, with its hit area when it can be
    /// clicked.
    reply: Option<PreparedReply>,
    /// Hit areas of the links (for their cursor).
    links: Vec<Hitbox>,
}

/// The reply context line for one frame.
struct PreparedReply {
    fit: ReplyFit,
    /// Where it is drawn.
    rect: Bounds<Pixels>,
    hitbox: Option<Hitbox>,
    /// The parent's name in it, when it can open their card: where, and whose.
    user: Option<(Hitbox, Arc<str>)>,
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
            regions.push((rect, inline.atom, inline.content.clone()));
            // Base first, overlays on top, each centred in the stack's box.
            for (n, layer) in inline.layers.iter().enumerate() {
                let layer_rect = centred(rect, layer.size);
                match cx
                    .global_mut::<Images>()
                    .lookup(&layer.url, &mut frame, view)
                {
                    Lookup::Ready(image) if image.is_drawable() => {
                        let (index, next) = image.frame(style.emotes.clock);
                        if let Some(next) = next {
                            frame.animate_in(next);
                        }
                        let result = window.paint_image(
                            layer_rect,
                            layer_rect,
                            Corners::default(),
                            image.render_image().clone(),
                            index,
                            grayscale,
                        );
                        if let Err(error) = result {
                            tracing::debug!(%error, "failed to paint an emote");
                        }
                    }
                    // One placeholder per stack: the base's, an overlay still loading shows
                    // nothing yet.
                    Lookup::Loading | Lookup::Retrying => {
                        if n == 0 {
                            window.paint_quad(fill(layer_rect, placeholder).corner_radii(px(3.0)));
                        }
                    }
                    // Failed for good: its URL was logged, so the view re-shapes it as text.
                    Lookup::Failed { relayout } => {
                        if relayout {
                            window.request_animation_frame();
                        }
                    }
                    Lookup::Ready(_) => {}
                }
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
                        .map(|(_, atom, content)| (*atom, content.clone()))
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

impl MessageElement {
    /// Keeps [`MessageStyle::hovered_row`] pointing at this row while the pointer is over it (not
    /// over something drawn above it, such as the thread popover), and wakes the view when that
    /// changes. Checked at paint (rows move under a still pointer when the list scrolls) and on
    /// every mouse move.
    fn track_hover(&self, hitbox: &Hitbox, window: &mut Window, cx: &mut App) {
        let Some(cell) = self.style.hovered_row.clone() else {
            return;
        };
        let (seq, view) = (self.seq, window.current_view());
        let sync = move |over: bool, cx: &mut App| match (over, cell.get()) {
            (true, current) if current != Some(seq) => {
                cell.set(Some(seq));
                cx.notify(view);
            }
            (false, Some(current)) if current == seq => {
                cell.set(None);
                cx.notify(view);
            }
            _ => {}
        };
        sync(hitbox.is_hovered(window), cx);
        let moved = hitbox.clone();
        let on_move = sync.clone();
        window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
            if phase.bubble() {
                on_move(moved.is_hovered(window), cx);
            }
        });
        window.on_mouse_event(move |_: &MouseExitEvent, _, _, cx| sync(false, cx));
    }

    /// Draws what the find bar is looking for behind the text: every match in the author and the
    /// body (not the timestamp or the badges), the one the list was scrolled to stronger.
    fn paint_search(&self, origin: Point<Pixels>, layout: &MessageLayout, window: &mut Window) {
        let Some(search) = &self.style.search else {
            return;
        };
        let MessageLayout { shaped, flow } = layout;
        let text = &shaped.display.text;
        let layout_id = Rc::as_ptr(&layout.shaped) as usize;
        let ranges = search
            .ranges
            .borrow_mut()
            .get(self.seq, layout_id, text, &search.needle);
        if ranges.is_empty() {
            return;
        }
        let current = search.current == Some(self.seq);
        let color = self
            .style
            .palette
            .rule_color(HighlightColor::Yellow)
            .opacity(if current { 0.7 } else { 0.38 });
        for range in ranges.iter().cloned() {
            let role = shaped
                .display
                .spans
                .iter()
                .find(|span| span.range.contains(&range.start))
                .map(|span| span.role);
            if matches!(role, Some(Role::Timestamp | Role::Badge | Role::Separator)) {
                continue;
            }
            for rect in selection::highlight(flow, &shaped.atoms, range) {
                let line = &flow.lines[rect.line];
                let bounds = Bounds::new(
                    origin + point(px(rect.x), px(line.top)),
                    size(px(rect.width), px(line.height)),
                );
                window.paint_quad(fill(bounds, color));
            }
        }
    }

    /// Where the flowed text starts: inside the padding, under the reply line.
    fn content_origin(&self, bounds: Bounds<Pixels>, layout: &MessageLayout) -> Point<Pixels> {
        bounds.origin + self.style.padding + point(px(0.0), px(layout.reply_height()))
    }

    /// The reply line cut to `content` width and where it goes; registers its hit area.
    fn prepare_reply(
        &self,
        line: &ReplyLine,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) -> PreparedReply {
        let content = bounds.size.width - self.style.padding.x * 2.0;
        let fit = line.fit(content.as_f32());
        // Without the gap above: that is not part of the line.
        let rect = Bounds::new(
            bounds.origin + self.style.padding + point(px(0.0), px(REPLY_GAP)),
            size(px(fit.width), px(line.height - REPLY_GAP)),
        );
        let hitbox = self
            .style
            .on_open_thread
            .is_some()
            .then(|| window.insert_hitbox(rect, HitboxBehavior::Normal));
        // The parent's name opens their card where the surface can (the list and the popover can,
        // a bare element cannot).
        let user = self
            .style
            .selection
            .as_ref()
            .and(line.user.as_ref())
            .zip(line.user_span(&fit))
            .map(|((_, login), (x, width))| {
                let bounds = Bounds::new(
                    rect.origin + point(px(x), px(0.0)),
                    size(px(width), rect.size.height),
                );
                (
                    window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    login.clone(),
                )
            });
        PreparedReply {
            fit,
            rect,
            hitbox,
            user,
        }
    }

    /// Draws the reply line (muted, the text colour under the pointer) and makes it open the
    /// thread when clicked.
    fn paint_reply(&self, line: &ReplyLine, prepared: &PreparedReply, window: &mut Window) {
        let style = &*self.style;
        let hovered = prepared
            .hitbox
            .as_ref()
            .is_some_and(|hitbox| hitbox.is_hovered(window));
        let color = if self.message.flags.deleted || !hovered {
            style.palette.muted
        } else {
            style.palette.text
        };
        let text_height = line.height - REPLY_GAP;
        let baseline = (text_height - line.ascent.as_f32() - line.descent.as_f32()) / 2.0
            + line.ascent.as_f32();
        let fit = prepared.fit;
        let last = line.atoms.len().saturating_sub(1);
        for glyph in &line.glyphs {
            let index = glyph.atom as usize;
            let Some(atom) = line.atoms.get(index) else {
                continue;
            };
            if atom.kind != AtomKind::Glyphs {
                continue;
            }
            let x = match fit.ellipsis_x {
                _ if index < fit.atoms => line.offsets[index],
                Some(x) if index == last => x,
                _ => continue,
            };
            let position = point(
                prepared.rect.origin.x + px(x + glyph.x - atom.src_x),
                prepared.rect.origin.y + px(baseline),
            );
            let result = if glyph.is_emoji {
                window.paint_emoji(position, glyph.font_id, glyph.id, line.font_size)
            } else {
                window.paint_glyph(position, glyph.font_id, glyph.id, line.font_size, color)
            };
            if let Err(error) = result {
                tracing::debug!(%error, "failed to paint a glyph");
            }
        }

        let (Some(hitbox), Some(open)) = (prepared.hitbox.clone(), style.on_open_thread.clone())
        else {
            return;
        };
        window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
        let root = line.root.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase.bubble() && event.button == MouseButton::Left && hitbox.is_hovered(window) {
                open(&root, window, cx);
                cx.stop_propagation();
            }
        });
        // Registered after the line's own handler, so it runs first (bubbling is in reverse
        // order): a press on the name opens the card and not the thread.
        if let (Some((name, login)), Some(surface)) = (&prepared.user, &style.selection) {
            window.set_cursor_style(CursorStyle::PointingHand, name);
            let (name, login, surface) = (name.clone(), login.clone(), surface.clone());
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase.bubble() && event.button == MouseButton::Left && name.is_hovered(window) {
                    surface.open_user(&login, event.position, window, cx);
                    cx.stop_propagation();
                }
            });
        }
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
        let (seq, message, style, cache, collapsed, ignored) = (
            self.seq,
            self.message.clone(),
            self.style.clone(),
            self.cache.clone(),
            self.collapsed,
            self.ignored,
        );
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |known, available, window, cx| {
                let width = known.width.unwrap_or(match available.width {
                    AvailableSpace::Definite(width) => width,
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => px(f32::INFINITY),
                });
                let content = width - style.padding.x * 2.0;
                let layout = layout_for(
                    &cache,
                    Subject {
                        seq,
                        message: &message,
                        collapsed,
                        ignored,
                    },
                    &style,
                    content,
                    window,
                    cx,
                );
                let width = if width.as_f32().is_finite() {
                    width
                } else {
                    px(layout.content_width()) + style.padding.x * 2.0
                };
                size(width, px(layout.height()) + style.padding.y * 2.0)
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
            Subject {
                seq: self.seq,
                message: &self.message,
                collapsed: self.collapsed,
                ignored: self.ignored,
            },
            &self.style,
            content,
            window,
            cx,
        );
        let reply = layout
            .shaped
            .reply
            .as_ref()
            .map(|line| self.prepare_reply(line, bounds, window));
        let origin = self.content_origin(bounds, &layout);
        let links = self
            .style
            .selection
            .as_ref()
            .map_or_else(Vec::new, |surface| {
                surface.prepaint_row(self.seq, bounds, origin, &layout, window)
            });
        if layout.shaped.inlines.is_empty() && self.style.hovered_row.is_none() {
            return Prepainted {
                layout,
                hitbox: None,
                reply,
                links,
            };
        }
        // Normal behaviour: it only answers "is the pointer over this message"; wheel events
        // still reach the list.
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
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
            reply,
            links,
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
        let mut flags = message.flags;
        // A collapsed line is muted text, not struck-through text.
        flags.deleted &= !self.collapsed;
        if self.ignored {
            // What was ignored is not tinted either.
            flags.deleted = false;
            flags.highlight = None;
            flags.mention = false;
            flags.highlighted = false;
        }
        let MessageLayout { shaped, flow } = &prepainted.layout;

        if let Some(hitbox) = &prepainted.hitbox {
            self.track_hover(hitbox, window, cx);
        }

        window.paint_layer(bounds, |window| {
            // The message a jump just brought into view.
            if style
                .flash
                .as_ref()
                .is_some_and(|flash| flash.get() == Some(self.seq))
            {
                let color = palette.rule_color(HighlightColor::Accent);
                window.paint_quad(fill(bounds, color.opacity(FLASH_TINT)));
            }
            // Event rows (PR 5.1) have a bar and a tint of their own, which also covers a
            // channel-points highlight; a mention of the viewer is the stronger signal.
            let look = shaped.look;
            // A highlight rule's colour (PR 5.3): a tint and a bar like an event card's, in the
            // rule's theme colour. A removed message is struck and dimmed, not highlighted.
            let rule = flags
                .highlight
                .filter(|_| !flags.deleted)
                .map(|color| palette.rule_color(color));
            if let Some(color) = rule {
                window.paint_quad(fill(bounds, color.opacity(RULE_TINT)));
                if look.is_none() {
                    let bar = Bounds::new(
                        bounds.origin,
                        size(px(events::BAR_WIDTH), bounds.size.height),
                    );
                    window.paint_quad(fill(bar, color.opacity(0.9)));
                }
            } else if flags.mention {
                window.paint_quad(fill(bounds, palette.mention_bg));
            } else if look.is_none() && flags.highlighted {
                window.paint_quad(fill(bounds, palette.highlight_bg));
            }
            if let Some(look) = &look {
                // A mention keeps its own tint behind the bar.
                look.paint(
                    bounds,
                    &palette.events,
                    !(flags.mention || rule.is_some()),
                    window,
                );
            }
            let header_color = look.as_ref().map_or(palette.muted, |look| {
                if look.quiet {
                    palette.muted
                } else {
                    palette.events.header_text(look.accent, palette.background)
                }
            });

            let name = message.author.as_ref().map_or(palette.text, |author| {
                let color = author
                    .color
                    .unwrap_or_else(|| colors::fallback_color(&author.login));
                colors::readable(color, palette.background)
            });
            // A 7TV paint on the author's name (PR 6.2): glyph by glyph, each in the paint's colour
            // where it stands in the name (GPUI draws glyphs in one colour each, so a gradient is
            // sampled per glyph). Not on a removed or ignored line, and only when the paint stays
            // readable on this theme; otherwise the name keeps its plain colour.
            let name_paint = if flags.deleted || self.collapsed || self.ignored {
                None
            } else {
                name_paint(message, shaped.name_extent, palette.background, cx)
            };
            if let (Some(line), Some(prepared)) = (&shaped.reply, &prepainted.reply) {
                self.paint_reply(line, prepared, window);
            }
            let origin = self.content_origin(bounds, &prepainted.layout);
            // The selection goes behind the text.
            if let Some(surface) = &style.selection {
                surface.paint_row(
                    self.seq,
                    origin,
                    &prepainted.layout,
                    &prepainted.links,
                    window,
                );
            }
            if !self.collapsed && !self.ignored {
                self.paint_search(origin, &prepainted.layout, window);
            }
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
                // A badge's title only stands in for its image: never draw it as text.
                let role = shaped.display.spans[shaped.atom_spans[index] as usize].role;
                if role == Role::Badge {
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
                    let mut color = if flags.deleted {
                        palette.muted
                    } else {
                        palette.role(role, name, header_color)
                    };
                    if role == Role::Name
                        && let Some(paint) = &name_paint
                    {
                        // The glyph's middle, in the name's own box.
                        let x = glyph.x - paint.x0 + atom.width / 2.0;
                        if let Some(sampled) = paint.paint.color_at(
                            x,
                            line_height.as_f32() / 2.0,
                            paint.width,
                            line_height.as_f32(),
                        ) {
                            color = colors::paint_hsla(sampled);
                        }
                        for shadow in &paint.paint.shadows {
                            // A glow (no offset) has nothing to be drawn behind the glyph with.
                            if shadow.x != 0.0 || shadow.y != 0.0 {
                                let at = position + point(px(shadow.x), px(shadow.y));
                                let _ = window.paint_glyph(
                                    at,
                                    glyph.font_id,
                                    glyph.id,
                                    style.font_size,
                                    colors::paint_hsla(shadow.color),
                                );
                            }
                        }
                    }
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
