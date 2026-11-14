//! Shaped and flowed message layouts, and the per-message layout cache.
//!
//! A message is laid out in two stages with different lifetimes:
//!
//! 1. **Shaping** ([`ShapedMessage`]): the display text is shaped once as a single unwrapped line
//!    with the window text system (`WindowTextSystem::layout_line`), then cut into atoms at
//!    grapheme-cluster boundaries (`unicode-segmentation`, extended clusters) with their advance
//!    widths. Depends on the text, fonts and size, so it is redone only when the message or the
//!    [`LayoutKey`] (text-config generation, scale factor) changes.
//! 2. **Flowing** ([`super::flow`]): the atoms are placed into lines for a width. Cheap (one pass
//!    over the atoms, no shaping), so a resize re-flows every visible message without touching
//!    the text system.
//!
//! Emotes are decided at shaping time ([`EmoteBox`]): an emote with an image becomes one
//! [`AtomKind::Inline`] atom of the image's logical size in place of its name's clusters (the name
//! stays in the display text for copy and hit-testing; its glyphs are not painted), so the box
//! is the final size before the image arrives. An emote without an image stays text.
//!
//! A zero-width emote stacked on a base (7TV overlays) shares the base's atom: the stack is one
//! inline atom covering the base's name, the overlays' names and the spaces between them, as wide
//! and as tall as its widest and tallest layer. Layers without an image are left out of the stack
//! (an overlay that failed to load simply is not drawn); a stack with no image at all stays text.
//!
//! Badges are the same mechanism, ahead of the name: each badge the display text has a slot for
//! ([`super::display::BadgeSlot`]: its title as a [`super::display::Role::Badge`] span) becomes one
//! inline atom of a fixed square size, so its box is final before the image arrives and a badge
//! that later fails for good only changes the layout through the per-URL invalidation.
//!
//! [`LayoutCache`] keeps both per store sequence number, keyed by (width, text-config generation,
//! scale factor); a changed message (`Arc` pointer differs, e.g. marked deleted) is re-shaped.
//! Entries for evicted messages are dropped when the view syncs with the store.

use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::{Badge, Emote, Message};
use gpui_chat_media::ImageKey;
use gpui_kit::{FontId, GlyphId, LineLayout, Pixels};
use unicode_segmentation::UnicodeSegmentation as _;

use super::display::{DisplayEmote, DisplayText};
use super::flow::{self, Atom, AtomKind, Flow};
use crate::images::EmoteBox;

/// Slack for comparing glyph positions (logical pixels).
const RTL_EPSILON: f32 = 0.01;

/// One shaped glyph, attached to the atom (grapheme cluster) it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphRef {
    pub font_id: FontId,
    pub id: GlyphId,
    /// X in the unwrapped line.
    pub x: f32,
    /// Index into [`ShapedMessage::atoms`].
    pub atom: u32,
    pub is_emoji: bool,
}

/// One image of an inline box, drawn centred in the box.
#[derive(Clone, Debug)]
pub struct InlineLayer {
    pub url: ImageKey,
    /// Logical size of this layer's image.
    pub size: (f32, f32),
}

/// What an inline box stands for, for the tooltip.
#[derive(Clone, Debug)]
pub enum InlineContent {
    /// An emote and the zero-width overlays stacked on it (every emote of the stack, drawn or
    /// not).
    Emotes(Vec<Arc<Emote>>),
    /// A badge.
    Badge(Arc<Badge>),
}

/// An emote (with the zero-width overlays stacked on it) or a badge drawn as images: which atom
/// it is and what to draw.
#[derive(Clone, Debug)]
pub struct InlineImage {
    /// Index into [`ShapedMessage::atoms`] (an [`AtomKind::Inline`] atom).
    pub atom: u32,
    /// The images to draw, bottom first (the base, then each overlay; a badge has one).
    pub layers: Vec<InlineLayer>,
    pub content: InlineContent,
}

impl InlineImage {
    /// Whether one of the layers draws an image whose URL is in `urls`.
    pub fn uses_any(&self, urls: &HashSet<&str>) -> bool {
        self.layers
            .iter()
            .any(|layer| urls.contains(layer.url.url()))
    }
}

/// A stack of emotes, or one badge, about to become one inline atom.
struct Stack {
    /// Display-text range from the base's name to the last overlay's name.
    range: std::ops::Range<usize>,
    layers: Vec<InlineLayer>,
    emotes: Vec<Arc<Emote>>,
    badge: Option<Arc<Badge>>,
}

impl Stack {
    /// The box: as wide as the widest layer and as tall as the tallest.
    fn size(&self) -> (f32, f32) {
        self.layers.iter().fold((0.0, 0.0), |(w, h), layer| {
            (w.max(layer.size.0), h.max(layer.size.1))
        })
    }
}

/// One stack per badge: its title's range, drawn as a box of the slot's size. Badges come before
/// everything else in the text, so these precede the emote stacks.
fn badge_stacks(display: &DisplayText) -> impl Iterator<Item = Stack> + '_ {
    display.badges.iter().map(|entry| Stack {
        range: display.spans[entry.span].range.clone(),
        layers: vec![InlineLayer {
            url: entry.slot.url.clone(),
            size: (entry.slot.size, entry.slot.size),
        }],
        emotes: Vec::new(),
        badge: Some(entry.slot.badge.clone()),
    })
}

/// Groups the display emotes into stacks (a base and the overlays after it). `boxes` has one
/// entry per display emote (or none at all); a stack without any image layer is dropped.
fn stacks(display: &DisplayText, boxes: &[EmoteBox]) -> Vec<Stack> {
    let mut stacks: Vec<Stack> = Vec::new();
    for (
        DisplayEmote {
            span,
            emote,
            overlay,
        },
        emote_box,
    ) in display.emotes.iter().zip(boxes)
    {
        let range = display.spans[*span].range.clone();
        let layer = match emote_box {
            EmoteBox::Image { url, size } => Some(InlineLayer {
                url: url.clone(),
                size: *size,
            }),
            EmoteBox::Text => None,
        };
        match stacks.last_mut() {
            Some(stack) if *overlay => {
                stack.range.end = range.end;
                stack.emotes.push(emote.clone());
                stack.layers.extend(layer);
            }
            _ => stacks.push(Stack {
                range,
                layers: layer.into_iter().collect(),
                emotes: vec![emote.clone()],
                badge: None,
            }),
        }
    }
    stacks.retain(|stack| !stack.layers.is_empty());
    stacks
}

/// A message's display text shaped as one line and cut into atoms.
#[derive(Clone, Debug)]
pub struct ShapedMessage {
    pub display: DisplayText,
    pub atoms: Vec<Atom>,
    /// The badges and emotes drawn as images, in atom order.
    pub inlines: Vec<InlineImage>,
    /// Span index (into `display.spans`) of each atom.
    pub atom_spans: Vec<u32>,
    pub glyphs: Vec<GlyphRef>,
    pub ascent: Pixels,
    pub descent: Pixels,
}

impl ShapedMessage {
    /// Cuts `layout` (the shaped `display.text`) into grapheme-cluster atoms, and each emote of
    /// `display.emotes` whose box (`boxes`, in the same order) is an image into one inline atom.
    pub fn from_layout(display: DisplayText, layout: &LineLayout, boxes: &[EmoteBox]) -> Self {
        let line_width = layout.width.as_f32();
        // Every glyph's start offset and x, in text order.
        let mut marks: Vec<(usize, f32)> = layout
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter())
            .map(|glyph| (glyph.index, glyph.position.x.as_f32()))
            .collect();
        marks.sort_by_key(|&(index, _)| index);
        // A glyph's visual right edge is the next greater glyph x (or the line end). Combining
        // marks share their base's x, so they do not cut its advance short.
        let mut edges: Vec<f32> = marks.iter().map(|&(_, x)| x).collect();
        edges.sort_by(f32::total_cmp);
        edges.dedup();
        let right_of = |x: f32| {
            let i = edges.partition_point(|&edge| edge <= x);
            edges.get(i).copied().unwrap_or(line_width)
        };
        // For a cluster without glyphs of its own (the tail of a ligature): where the next
        // glyph starts.
        let x_at = |offset: usize| {
            let i = marks.partition_point(|&(index, _)| index < offset);
            marks.get(i).map_or(line_width, |&(_, x)| x)
        };

        let mut atoms: Vec<Atom> = Vec::new();
        for (start, cluster) in display.text.grapheme_indices(true) {
            let end = start + cluster.len();
            let glyphs = &marks[marks.partition_point(|&(index, _)| index < start)
                ..marks.partition_point(|&(index, _)| index < end)];
            let (left, right) = if glyphs.is_empty() {
                let x = x_at(start);
                (x, x)
            } else {
                glyphs
                    .iter()
                    .fold((f32::INFINITY, 0.0f32), |(left, right), &(_, x)| {
                        (left.min(x), right.max(right_of(x)))
                    })
            };
            // A cluster that starts before the previous atom's right edge does not follow it
            // visually: it is inside a right-to-left run (glyph x falls while the text offset
            // grows), possibly with a left-to-right run such as digits nested in it, which jumps
            // back within the run's span. Keep the whole run as one unbreakable atom so its
            // visual order survives; wrapping inside it is left to proper bidi support. Plain
            // left-to-right text never merges: each cluster starts exactly where the previous
            // glyph's advance (kerning included) ends.
            if let Some(previous) = atoms.last_mut()
                && !glyphs.is_empty()
                && left < previous.src_x + previous.width - RTL_EPSILON
            {
                let previous_right = previous.src_x + previous.width;
                previous.range.end = end;
                previous.src_x = left;
                previous.width = previous_right.max(right) - left;
                previous.kind = AtomKind::Glyphs;
                continue;
            }
            atoms.push(Atom {
                range: start..end,
                src_x: left,
                width: (right - left).max(0.0),
                height: 0.0,
                kind: if cluster.chars().all(char::is_whitespace) {
                    AtomKind::Space
                } else {
                    AtomKind::Glyphs
                },
            });
        }
        let (atoms, inlines) = inline_emotes(atoms, &display, boxes);
        let atom_spans = atoms
            .iter()
            .map(|atom| display.span_at(atom.range.start).unwrap_or(0) as u32)
            .collect();
        let glyphs = layout
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|glyph| (run.font_id, glyph)))
            .map(|(font_id, glyph)| GlyphRef {
                font_id,
                id: glyph.id,
                x: glyph.position.x.as_f32(),
                atom: atoms
                    .partition_point(|atom| atom.range.end <= glyph.index)
                    .min(atoms.len().saturating_sub(1)) as u32,
                is_emoji: glyph.is_emoji,
            })
            .collect();
        Self {
            display,
            atoms,
            inlines,
            atom_spans,
            glyphs,
            ascent: layout.ascent,
            descent: layout.descent,
        }
    }
}

/// Replaces the atoms of every emote stack that has an image with one inline atom of its box
/// size. A stack whose span does not start and end on atom boundaries (inside a merged
/// right-to-left run) stays text.
fn inline_emotes(
    atoms: Vec<Atom>,
    display: &DisplayText,
    boxes: &[EmoteBox],
) -> (Vec<Atom>, Vec<InlineImage>) {
    let mut stacks = badge_stacks(display)
        .chain(stacks(display, boxes))
        .peekable();
    if stacks.peek().is_none() {
        return (atoms, Vec::new());
    }
    let mut out = Vec::with_capacity(atoms.len());
    let mut inlines = Vec::new();
    let mut i = 0;
    while i < atoms.len() {
        while stacks
            .peek()
            .is_some_and(|stack| stack.range.start < atoms[i].range.start)
        {
            stacks.next();
        }
        if let Some(stack) = stacks.peek()
            && stack.range.start == atoms[i].range.start
        {
            let last = atoms[i..]
                .iter()
                .position(|atom| atom.range.end >= stack.range.end)
                .map(|n| i + n)
                .filter(|&j| atoms[j].range.end == stack.range.end);
            if let Some(last) = last {
                let (width, height) = stack.size();
                out.push(Atom {
                    range: stack.range.clone(),
                    src_x: atoms[i..=last]
                        .iter()
                        .map(|atom| atom.src_x)
                        .fold(f32::INFINITY, f32::min),
                    width,
                    height,
                    kind: AtomKind::Inline,
                });
                let stack = stacks.next().expect("peeked");
                inlines.push(InlineImage {
                    atom: (out.len() - 1) as u32,
                    layers: stack.layers,
                    content: match stack.badge {
                        Some(badge) => InlineContent::Badge(badge),
                        None => InlineContent::Emotes(stack.emotes),
                    },
                });
                i = last + 1;
                continue;
            }
            stacks.next();
        }
        out.push(atoms[i].clone());
        i += 1;
    }
    (out, inlines)
}

/// What a shaped layout depends on besides the message itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayoutKey {
    /// Bumped by the view whenever font family, size, line height or display options change.
    pub generation: u64,
    /// `f32::to_bits` of the window scale factor.
    pub scale_bits: u32,
}

impl LayoutKey {
    pub fn new(generation: u64, scale_factor: f32) -> Self {
        Self {
            generation,
            scale_bits: scale_factor.to_bits(),
        }
    }
}

/// A shaped message and its flow for one width.
#[derive(Clone, Debug)]
pub struct MessageLayout {
    pub shaped: Rc<ShapedMessage>,
    pub flow: Rc<Flow>,
}

struct Entry {
    message: Arc<Message>,
    key: LayoutKey,
    shaped: Rc<ShapedMessage>,
    flow: Rc<Flow>,
    /// `f32::to_bits` of the width `flow` was made for.
    width_bits: u32,
}

/// Counters for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub shaped: u64,
    pub flowed: u64,
    pub hits: u64,
}

/// Per-message layouts by store sequence number.
#[derive(Default)]
pub struct LayoutCache {
    entries: BTreeMap<u64, Entry>,
    stats: CacheStats,
}

impl LayoutCache {
    /// The layout of message `seq` at `width`. Shapes with `shape` only when nothing reusable is
    /// cached (new message, changed message, or a different `key`); re-flows when only the width
    /// changed.
    pub fn layout(
        &mut self,
        seq: u64,
        message: &Arc<Message>,
        key: LayoutKey,
        width: f32,
        line_height: f32,
        shape: impl FnOnce() -> ShapedMessage,
    ) -> MessageLayout {
        let width_bits = width.to_bits();
        if let Some(entry) = self.entries.get_mut(&seq)
            && entry.key == key
            && Arc::ptr_eq(&entry.message, message)
        {
            if entry.width_bits != width_bits {
                entry.flow = Rc::new(flow::flow(&entry.shaped.atoms, width, line_height));
                entry.width_bits = width_bits;
                self.stats.flowed += 1;
            } else {
                self.stats.hits += 1;
            }
            return MessageLayout {
                shaped: entry.shaped.clone(),
                flow: entry.flow.clone(),
            };
        }
        let shaped = Rc::new(shape());
        let flow = Rc::new(flow::flow(&shaped.atoms, width, line_height));
        self.stats.shaped += 1;
        self.stats.flowed += 1;
        self.entries.insert(
            seq,
            Entry {
                message: message.clone(),
                key,
                shaped: shaped.clone(),
                flow: flow.clone(),
                width_bits,
            },
        );
        MessageLayout { shaped, flow }
    }

    /// Drops the layouts that draw any of `urls`, so they are shaped again (with the boxes the
    /// image store knows by now) the next time they are used. Returns the store sequence
    /// numbers of the dropped layouts; the caller asks its list to measure those rows again.
    /// Layouts without these URLs, and messages without emotes, are untouched.
    pub fn invalidate_urls(&mut self, urls: &[ImageKey]) -> Vec<u64> {
        if urls.is_empty() {
            return Vec::new();
        }
        let urls: HashSet<&str> = urls.iter().map(ImageKey::url).collect();
        let mut dropped = Vec::new();
        self.entries.retain(|seq, entry| {
            let affected = entry
                .shaped
                .inlines
                .iter()
                .any(|inline| inline.uses_any(&urls));
            if affected {
                dropped.push(*seq);
            }
            !affected
        });
        dropped
    }

    /// Drops the layouts of messages before `first_seq` (evicted from the store).
    pub fn evict_before(&mut self, first_seq: u64) {
        self.entries = self.entries.split_off(&first_seq);
    }

    /// Drops every entry whose key is not `key` (after a settings change), so stale layouts do
    /// not linger for messages that are never drawn again.
    pub fn retain_key(&mut self, key_generation: u64) {
        self.entries
            .retain(|_, entry| entry.key.generation == key_generation);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The cached flow of message `seq`, if any.
    #[cfg(test)]
    pub fn flow_of(&self, seq: u64) -> Option<Rc<Flow>> {
        self.entries.get(&seq).map(|entry| entry.flow.clone())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn stats(&self) -> CacheStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{Author, Fragment, Timestamp};
    use gpui_kit::{ShapedGlyph, ShapedRun, point, px};

    use super::*;
    use crate::chat::display::{self, DisplayOptions};

    /// What the headless text system produces: one glyph per char, 10 px each, 20 px for
    /// characters outside the BMP (emoji).
    fn fake_layout(text: &str) -> LineLayout {
        let mut x = 0.0;
        let glyphs = text
            .char_indices()
            .map(|(index, c)| {
                let glyph = ShapedGlyph {
                    id: GlyphId(c as u32),
                    position: point(px(x), px(0.0)),
                    index,
                    is_emoji: c.len_utf16() == 2,
                };
                x += if c.len_utf16() == 2 { 20.0 } else { 10.0 };
                glyph
            })
            .collect();
        LineLayout {
            font_size: px(10.0),
            width: px(x),
            ascent: px(8.0),
            descent: px(2.0),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs,
            }],
            len: text.len(),
        }
    }

    fn shaped(text: &str) -> ShapedMessage {
        let message = message(text);
        let display = display::build(&message, DisplayOptions::default(), &[]);
        let layout = fake_layout(&display.text);
        ShapedMessage::from_layout(display, &layout, &[])
    }

    fn message(text: &str) -> Arc<Message> {
        Arc::new(Message::chat(
            "1",
            Timestamp(0),
            Author::new("al", "Al"),
            vec![Fragment::Text(text.into())],
        ))
    }

    /// "Al: hi Kappa LUL" with the two emotes boxed as `boxes`.
    fn shaped_with_emotes(boxes: &[EmoteBox]) -> ShapedMessage {
        let emote = |id: &str, name: &str| Fragment::Emote {
            emote: Arc::new(gpui_chat_core::Emote::twitch(id, name)),
            zero_width_overlays: vec![],
        };
        let message = Message::chat(
            "1",
            Timestamp(0),
            Author::new("al", "Al"),
            vec![
                Fragment::Text("hi ".into()),
                emote("25", "Kappa"),
                Fragment::Text(" ".into()),
                emote("425618", "LUL"),
            ],
        );
        let display = display::build(&message, DisplayOptions::default(), &[]);
        let layout = fake_layout(&display.text);
        ShapedMessage::from_layout(display, &layout, boxes)
    }

    fn image(url: &str, w: f32, h: f32) -> EmoteBox {
        EmoteBox::Image {
            url: ImageKey::new(url),
            size: (w, h),
        }
    }

    #[test]
    fn image_emotes_become_one_inline_atom_of_their_box_size() {
        let s = shaped_with_emotes(&[image("k", 28.0, 28.0), image("l", 56.0, 28.0)]);
        let texts: Vec<(&str, AtomKind)> = s
            .atoms
            .iter()
            .map(|atom| (&s.display.text[atom.range.clone()], atom.kind))
            .collect();
        assert_eq!(
            &texts[7..],
            [
                ("Kappa", AtomKind::Inline),
                (" ", AtomKind::Space),
                ("LUL", AtomKind::Inline)
            ]
        );
        let kappa = &s.atoms[7];
        assert_eq!((kappa.src_x, kappa.width, kappa.height), (70.0, 28.0, 28.0));
        assert_eq!((s.atoms[9].width, s.atoms[9].height), (56.0, 28.0));
        let inlines: Vec<(u32, &str)> = s
            .inlines
            .iter()
            .map(|i| (i.atom, i.layers[0].url.url()))
            .collect();
        assert_eq!(inlines, [(7, "k"), (9, "l")]);
        // The names' glyphs belong to the inline atoms (the painter skips them).
        let kappa_glyphs = s.glyphs.iter().filter(|g| g.atom == 7).count();
        assert_eq!(kappa_glyphs, 5);

        // A 28 px box in 20 px lines: the box's line is 28 high and the next word wraps after it.
        let flow = flow::flow(&s.atoms, 1000.0, 20.0);
        assert_eq!(flow.height(), 28.0);
        // "Al: hi " is 70 px: the box starts there and the space after it at 98.
        assert_eq!(flow.placements[8].x, 98.0);
        let narrow = flow::flow(&s.atoms, 110.0, 20.0);
        assert_eq!(narrow.line_count(), 2);
        assert_eq!(narrow.height(), 28.0 + 28.0);
    }

    #[test]
    fn emotes_without_an_image_stay_text() {
        let s = shaped_with_emotes(&[EmoteBox::Text, image("l", 28.0, 28.0)]);
        assert_eq!(s.inlines.len(), 1);
        assert_eq!(s.inlines[0].layers[0].url.url(), "l");
        // "Kappa" is five glyph atoms of 10 px.
        assert!(
            s.atoms[7..12]
                .iter()
                .all(|a| a.kind == AtomKind::Glyphs && a.width == 10.0)
        );
        // No boxes at all (shaped before the image store existed): everything is text.
        let plain = shaped_with_emotes(&[]);
        assert!(plain.inlines.is_empty());
        assert!(plain.atoms.iter().all(|a| a.kind != AtomKind::Inline));
    }

    /// "Al: hi Base Over1 Over2 end": a base with two zero-width overlays stacked on it.
    fn shaped_stack(boxes: &[EmoteBox]) -> ShapedMessage {
        let emote = |name: &str, zero_width: bool| {
            let mut e = gpui_chat_core::Emote::twitch(name, name);
            e.zero_width = zero_width;
            Arc::new(e)
        };
        let message = Message::chat(
            "1",
            Timestamp(0),
            Author::new("al", "Al"),
            vec![
                Fragment::Text("hi ".into()),
                Fragment::Emote {
                    emote: emote("Base", false),
                    zero_width_overlays: vec![emote("Over1", true), emote("Over2", true)],
                },
                Fragment::Text(" end".into()),
            ],
        );
        let display = display::build(&message, DisplayOptions::default(), &[]);
        assert_eq!(display.text, "Al: hi Base Over1 Over2 end");
        let layout = fake_layout(&display.text);
        ShapedMessage::from_layout(display, &layout, boxes)
    }

    #[test]
    fn an_emote_and_its_overlays_are_one_box_as_big_as_the_biggest_layer() {
        let s = shaped_stack(&[
            image("base", 28.0, 28.0),
            image("o1", 56.0, 20.0),
            image("o2", 24.0, 40.0),
        ]);
        let texts: Vec<(&str, AtomKind)> = s
            .atoms
            .iter()
            .map(|atom| (&s.display.text[atom.range.clone()], atom.kind))
            .collect();
        // One inline atom covers "Base Over1 Over2"; the space before "end" stays.
        assert_eq!(
            &texts[7..],
            [
                ("Base Over1 Over2", AtomKind::Inline),
                (" ", AtomKind::Space),
                ("e", AtomKind::Glyphs),
                ("n", AtomKind::Glyphs),
                ("d", AtomKind::Glyphs)
            ]
        );
        let stack = &s.atoms[7];
        assert_eq!(
            (stack.width, stack.height),
            (56.0, 40.0),
            "widest x tallest"
        );
        assert_eq!(s.inlines.len(), 1);
        let layers: Vec<(&str, (f32, f32))> = s.inlines[0]
            .layers
            .iter()
            .map(|l| (l.url.url(), l.size))
            .collect();
        assert_eq!(
            layers,
            [
                ("base", (28.0, 28.0)),
                ("o1", (56.0, 20.0)),
                ("o2", (24.0, 40.0))
            ]
        );
        let InlineContent::Emotes(emotes) = &s.inlines[0].content else {
            panic!("not an emote stack");
        };
        let names: Vec<&str> = emotes.iter().map(|e| &*e.name as &str).collect();
        assert_eq!(names, ["Base", "Over1", "Over2"]);
        // The box is one unbreakable unit in the flow, and it takes one slot, not three.
        let flow = flow::flow(&s.atoms, 1000.0, 20.0);
        assert_eq!(flow.height(), 40.0);
        assert_eq!(
            flow.placements[8].x,
            70.0 + 56.0,
            "the space follows the box"
        );
    }

    #[test]
    fn layers_without_an_image_are_left_out_of_the_stack() {
        // A failed overlay: not drawn, but still listed for the tooltip.
        let s = shaped_stack(&[
            image("base", 28.0, 28.0),
            EmoteBox::Text,
            image("o2", 40.0, 28.0),
        ]);
        assert_eq!(s.inlines.len(), 1);
        let urls: Vec<&str> = s.inlines[0].layers.iter().map(|l| l.url.url()).collect();
        assert_eq!(urls, ["base", "o2"]);
        assert!(matches!(&s.inlines[0].content, InlineContent::Emotes(e) if e.len() == 3));
        assert_eq!(s.atoms[7].width, 40.0);

        // A failed base with a drawable overlay: the overlay is the stack.
        let s = shaped_stack(&[EmoteBox::Text, image("o1", 30.0, 30.0), EmoteBox::Text]);
        assert_eq!(s.inlines.len(), 1);
        assert_eq!(s.inlines[0].layers.len(), 1);
        assert_eq!(s.atoms[7].kind, AtomKind::Inline);

        // No image anywhere: all text, "Base Over1 Over2" as glyphs.
        let s = shaped_stack(&[EmoteBox::Text, EmoteBox::Text, EmoteBox::Text]);
        assert!(s.inlines.is_empty());
        assert!(s.atoms.iter().all(|a| a.kind != AtomKind::Inline));
    }

    #[test]
    fn invalidating_urls_drops_only_the_layouts_that_draw_them() {
        let mut cache = LayoutCache::default();
        let key = LayoutKey::new(1, 1.0);
        let plain = message("hello");
        let with = |boxes: &[EmoteBox]| shaped_with_emotes(boxes);
        // seq 0: text only; 1: draws "k" and "l"; 2: draws only "l"; 3: draws "k" as an overlay.
        cache.layout(0, &plain, key, 1000.0, 20.0, || shaped("hello"));
        cache.layout(1, &plain, key, 1000.0, 20.0, || {
            with(&[image("k", 28.0, 28.0), image("l", 28.0, 28.0)])
        });
        cache.layout(2, &plain, key, 1000.0, 20.0, || {
            with(&[EmoteBox::Text, image("l", 28.0, 28.0)])
        });
        cache.layout(3, &plain, key, 1000.0, 20.0, || {
            shaped_stack(&[
                image("base", 28.0, 28.0),
                image("k", 28.0, 28.0),
                EmoteBox::Text,
            ])
        });
        let shaped_before = cache.stats().shaped;

        assert!(cache.invalidate_urls(&[]).is_empty());
        assert!(cache.invalidate_urls(&[ImageKey::new("nope")]).is_empty());
        assert_eq!(cache.len(), 4);

        assert_eq!(cache.invalidate_urls(&[ImageKey::new("k")]), [1, 3]);
        assert_eq!(cache.len(), 2);
        assert!(cache.flow_of(0).is_some() && cache.flow_of(2).is_some());
        assert!(cache.flow_of(1).is_none() && cache.flow_of(3).is_none());
        // Untouched layouts are hits, the dropped ones are shaped again.
        cache.layout(0, &plain, key, 1000.0, 20.0, || {
            panic!("text row re-shaped")
        });
        cache.layout(2, &plain, key, 1000.0, 20.0, || {
            panic!("other emote re-shaped")
        });
        cache.layout(1, &plain, key, 1000.0, 20.0, || shaped("hello"));
        assert_eq!(cache.stats().shaped, shaped_before + 1);
    }

    #[test]
    fn atoms_are_grapheme_clusters_with_glyph_advances() {
        // "Al: " + "e\u{301}" (e + combining acute) + " " + a ZWJ family.
        let s = shaped("e\u{301} 👨‍👩‍👧");
        let texts: Vec<&str> = s
            .atoms
            .iter()
            .map(|atom| &s.display.text[atom.range.clone()])
            .collect();
        assert_eq!(texts, ["A", "l", ":", " ", "e\u{301}", " ", "👨‍👩‍👧"]);
        // The combining mark is part of its base's cluster: 2 glyphs, 20 px.
        assert_eq!(s.atoms[4].width, 20.0);
        assert_eq!(s.atoms[3].kind, AtomKind::Space);
        // Three 20 px emoji + two 10 px ZWJ glyphs in the fake shaper.
        assert_eq!(s.atoms[6].width, 80.0);
        // Every glyph points at the atom that contains it.
        for glyph in &s.glyphs {
            let atom = &s.atoms[glyph.atom as usize];
            assert!(glyph.x >= atom.src_x && glyph.x < atom.src_x + atom.width.max(1.0));
        }
        assert_eq!(s.atom_spans[0], 0, "name span");
        assert_eq!(
            *s.atom_spans.last().unwrap() as usize,
            s.display.spans.len() - 1
        );
    }

    /// Shapes `text` as the platform would draw it: every char 10 px wide, placed left to right
    /// in `visual` order (byte offsets of the chars, leftmost first).
    fn shape_visual(text: &str, visual: &[usize]) -> ShapedMessage {
        let display = DisplayText {
            text: text.into(),
            spans: vec![crate::chat::display::Span {
                range: 0..text.len(),
                role: crate::chat::display::Role::Text,
            }],
            emotes: Vec::new(),
            badges: Vec::new(),
        };
        assert_eq!(visual.len(), text.chars().count());
        let glyphs = visual
            .iter()
            .enumerate()
            .map(|(slot, &index)| ShapedGlyph {
                id: GlyphId(index as u32),
                position: point(px(slot as f32 * 10.0), px(0.0)),
                index,
                is_emoji: false,
            })
            .collect();
        let layout = LineLayout {
            font_size: px(10.0),
            width: px(visual.len() as f32 * 10.0),
            ascent: px(8.0),
            descent: px(2.0),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs,
            }],
            len: text.len(),
        };
        ShapedMessage::from_layout(display, &layout, &[])
    }

    /// `(text, src_x, width)` per atom.
    fn atom_summary(s: &ShapedMessage) -> Vec<(&str, f32, f32)> {
        s.atoms
            .iter()
            .map(|a| (&s.display.text[a.range.clone()], a.src_x, a.width))
            .collect()
    }

    /// Flowed on one line, every glyph is painted exactly where the shaper put it.
    fn assert_paints_in_shaped_order(s: &ShapedMessage) {
        let flow = flow::flow(&s.atoms, 1000.0, 10.0);
        assert_eq!(flow.line_count(), 1);
        for glyph in &s.glyphs {
            let atom = &s.atoms[glyph.atom as usize];
            let x = flow.placements[glyph.atom as usize].x + glyph.x - atom.src_x;
            assert_eq!(x, glyph.x, "glyph at text offset {}", glyph.id.0);
        }
    }

    /// Byte offsets of `text`'s chars, in text order.
    fn offsets(text: &str) -> Vec<usize> {
        text.char_indices().map(|(i, _)| i).collect()
    }

    #[test]
    fn right_to_left_runs_stay_whole_and_in_visual_order() {
        // "ab שלום cd": the Hebrew word is drawn right to left.
        let text = "ab שלום cd";
        let o = offsets(text);
        // Visual: a b ␠ ם ו ל ש ␠ c d
        let s = shape_visual(
            text,
            &[o[0], o[1], o[2], o[6], o[5], o[4], o[3], o[7], o[8], o[9]],
        );
        assert_eq!(
            atom_summary(&s),
            [
                ("a", 0.0, 10.0),
                ("b", 10.0, 10.0),
                (" ", 20.0, 10.0),
                ("שלום", 30.0, 40.0),
                (" ", 70.0, 10.0),
                ("c", 80.0, 10.0),
                ("d", 90.0, 10.0),
            ]
        );
        assert_paints_in_shaped_order(&s);
    }

    #[test]
    fn digits_nested_in_hebrew_stay_in_the_run() {
        // Logical "אב 12 גד" draws as "דג 12 בא": the digits are a left-to-right run inside the
        // right-to-left one, so their x jumps back up within the run's span.
        let text = "אב 12 גד";
        let o = offsets(text);
        let s = shape_visual(text, &[o[7], o[6], o[5], o[3], o[4], o[2], o[1], o[0]]);
        assert_eq!(atom_summary(&s), [("אב 12 גד", 0.0, 80.0)]);
        assert_paints_in_shaped_order(&s);
    }

    #[test]
    fn arabic_with_digits_between_latin_words() {
        // Logical "x سلام 2024 عليكم y"; visual: x ␠ [م ك ي ل ع ␠ 2 0 2 4 ␠ م ا ل س] ␠ y.
        let text = "x سلام 2024 عليكم y";
        let o = offsets(text);
        let visual = [
            o[0], o[1], o[16], o[15], o[14], o[13], o[12], o[11], o[7], o[8], o[9], o[10], o[6],
            o[5], o[4], o[3], o[2], o[17], o[18],
        ];
        let s = shape_visual(text, &visual);
        assert_eq!(
            atom_summary(&s),
            [
                ("x", 0.0, 10.0),
                (" ", 10.0, 10.0),
                ("سلام 2024 عليكم", 20.0, 150.0),
                (" ", 170.0, 10.0),
                ("y", 180.0, 10.0),
            ]
        );
        assert_paints_in_shaped_order(&s);
    }

    #[test]
    fn left_to_right_text_with_kerning_is_never_merged() {
        // Uneven advances (kerning pairs) still start each glyph at the previous one's end.
        let text = "AVAWa";
        let display = DisplayText {
            text: text.into(),
            spans: vec![crate::chat::display::Span {
                range: 0..5,
                role: crate::chat::display::Role::Text,
            }],
            emotes: Vec::new(),
            badges: Vec::new(),
        };
        let xs = [0.0, 8.7, 16.1, 25.3, 33.9];
        let glyphs = xs
            .iter()
            .enumerate()
            .map(|(index, &x)| ShapedGlyph {
                id: GlyphId(index as u32),
                position: point(px(x), px(0.0)),
                index,
                is_emoji: false,
            })
            .collect();
        let layout = LineLayout {
            font_size: px(10.0),
            width: px(42.3),
            ascent: px(8.0),
            descent: px(2.0),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs,
            }],
            len: 5,
        };
        let s = ShapedMessage::from_layout(display, &layout, &[]);
        assert_eq!(s.atoms.len(), 5);
        assert_paints_in_shaped_order(&s);
    }

    #[test]
    fn empty_text_shapes_to_nothing() {
        let display = DisplayText::default();
        let s = ShapedMessage::from_layout(display, &LineLayout::default(), &[]);
        assert!(s.atoms.is_empty() && s.glyphs.is_empty());
        assert_eq!(flow::flow(&s.atoms, 100.0, 10.0).line_count(), 1);
    }

    #[test]
    fn cache_reshapes_only_when_the_message_or_key_changes() {
        let mut cache = LayoutCache::default();
        let m = message("hello world");
        let key = LayoutKey::new(1, 2.0);
        let shapes = std::cell::Cell::new(0);
        let shape = |text: &str| {
            shapes.set(shapes.get() + 1);
            shaped(text)
        };

        let first = cache.layout(7, &m, key, 1000.0, 10.0, || shape("hello world"));
        assert_eq!(first.flow.line_count(), 1);
        // Same width: a hit, nothing recomputed.
        cache.layout(7, &m, key, 1000.0, 10.0, || shape("hello world"));
        assert_eq!(shapes.get(), 1);
        assert_eq!(cache.stats().hits, 1);

        // Resize: re-flow only, and the result wraps.
        let narrow = cache.layout(7, &m, key, 80.0, 10.0, || shape("hello world"));
        assert_eq!(shapes.get(), 1, "a width change must not re-shape");
        // "Al: hello world" at 10 px a char in 80 px: "Al:" | "hello" | "world".
        assert_eq!(narrow.flow.line_count(), 3);
        assert!(Rc::ptr_eq(&first.shaped, &narrow.shaped));

        // Settings change (new generation) or another scale factor: re-shape.
        cache.layout(7, &m, LayoutKey::new(2, 2.0), 80.0, 10.0, || {
            shape("hello world")
        });
        assert_eq!(shapes.get(), 2);
        cache.layout(7, &m, LayoutKey::new(2, 1.0), 80.0, 10.0, || {
            shape("hello world")
        });
        assert_eq!(shapes.get(), 3);

        // The store replaced the message (marked deleted): a new `Arc`, re-shape.
        let deleted = Arc::new(m.with_deleted());
        cache.layout(7, &deleted, LayoutKey::new(2, 1.0), 80.0, 10.0, || {
            shape("hello world")
        });
        assert_eq!(shapes.get(), 4);
    }

    #[test]
    fn cache_drops_evicted_and_stale_entries() {
        let mut cache = LayoutCache::default();
        let m = message("x");
        for seq in 0..10 {
            cache.layout(seq, &m, LayoutKey::new(1, 1.0), 100.0, 10.0, || shaped("x"));
        }
        cache.evict_before(6);
        assert_eq!(cache.len(), 4);
        cache.layout(9, &m, LayoutKey::new(2, 1.0), 100.0, 10.0, || shaped("x"));
        cache.retain_key(2);
        assert_eq!(cache.len(), 1);
    }
    /// "Mod Sub Al: hi": two badges (titles "Mod" and "Sub") ahead of the name.
    fn shaped_badges(slots: &[crate::chat::display::BadgeSlot]) -> ShapedMessage {
        let message = Message::chat(
            "1",
            Timestamp(0),
            Author::new("al", "Al"),
            vec![Fragment::Text("hi".into())],
        );
        let display = display::build(&message, DisplayOptions::default(), slots);
        assert_eq!(display.text, "Mod Sub Al: hi");
        let layout = fake_layout(&display.text);
        ShapedMessage::from_layout(display, &layout, &[])
    }

    fn slot(title: &str, url: &str, size: f32) -> crate::chat::display::BadgeSlot {
        crate::chat::display::BadgeSlot {
            badge: Arc::new(Badge {
                id: gpui_chat_core::BadgeId::new(title, "1"),
                title: title.to_owned(),
                urls: gpui_chat_core::ImageUrls::new(),
            }),
            url: ImageKey::new(url),
            size,
        }
    }

    #[test]
    fn each_badge_is_one_square_inline_atom_ahead_of_the_name() {
        let s = shaped_badges(&[slot("Mod", "m", 18.0), slot("Sub", "s", 18.0)]);
        let texts: Vec<(&str, AtomKind)> = s
            .atoms
            .iter()
            .map(|atom| (&s.display.text[atom.range.clone()], atom.kind))
            .collect();
        assert_eq!(
            &texts[..4],
            [
                ("Mod", AtomKind::Inline),
                (" ", AtomKind::Space),
                ("Sub", AtomKind::Inline),
                (" ", AtomKind::Space),
            ]
        );
        assert_eq!((s.atoms[0].width, s.atoms[0].height), (18.0, 18.0));
        let drawn: Vec<(u32, &str, &str)> = s
            .inlines
            .iter()
            .map(|i| {
                let InlineContent::Badge(badge) = &i.content else {
                    panic!("not a badge");
                };
                (i.atom, i.layers[0].url.url(), &*badge.title as &str)
            })
            .collect();
        assert_eq!(drawn, [(0, "m", "Mod"), (2, "s", "Sub")]);
        // "Al: hi" follows after the two boxes and spaces: 18 + 10 + 18 + 10.
        let flow = flow::flow(&s.atoms, 1000.0, 20.0);
        assert_eq!(flow.placements[4].x, 56.0);
        assert_eq!(flow.height(), 20.0, "an 18 px box fits the 20 px line");
    }

    #[test]
    fn a_message_without_badge_slots_has_none() {
        let s = shaped_badges_none();
        assert!(s.inlines.is_empty());
        assert!(s.display.badges.is_empty());
    }

    fn shaped_badges_none() -> ShapedMessage {
        shaped("hi")
    }
}
