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
//! [`LayoutCache`] keeps both per store sequence number, keyed by (width, text-config generation,
//! scale factor); a changed message (`Arc` pointer differs, e.g. marked deleted) is re-shaped.
//! Entries for evicted messages are dropped when the view syncs with the store.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::Message;
use gpui_kit::{FontId, GlyphId, LineLayout, Pixels};
use unicode_segmentation::UnicodeSegmentation as _;

use super::display::DisplayText;
use super::flow::{self, Atom, AtomKind, Flow};

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

/// A message's display text shaped as one line and cut into atoms.
#[derive(Clone, Debug)]
pub struct ShapedMessage {
    pub display: DisplayText,
    pub atoms: Vec<Atom>,
    /// Span index (into `display.spans`) of each atom.
    pub atom_spans: Vec<u32>,
    pub glyphs: Vec<GlyphRef>,
    pub ascent: Pixels,
    pub descent: Pixels,
}

impl ShapedMessage {
    /// Cuts `layout` (the shaped `display.text`) into grapheme-cluster atoms.
    pub fn from_layout(display: DisplayText, layout: &LineLayout) -> Self {
        // Glyph start offsets and x positions, by text offset. Clusters with several glyphs
        // (combining marks) or none (the tail of a ligature) are handled by taking the x of the
        // first glyph at or after a boundary.
        let mut marks: Vec<(usize, f32)> = layout
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter())
            .map(|glyph| (glyph.index, glyph.position.x.as_f32()))
            .collect();
        marks.sort_by_key(|&(index, _)| index);
        let line_width = layout.width.as_f32();
        let x_at = |offset: usize| {
            let i = marks.partition_point(|&(index, _)| index < offset);
            marks.get(i).map_or(line_width, |&(_, x)| x)
        };

        let text = display.text.as_str();
        let atoms: Vec<Atom> = text
            .grapheme_indices(true)
            .map(|(start, cluster)| {
                let end = start + cluster.len();
                let src_x = x_at(start);
                Atom {
                    range: start..end,
                    src_x,
                    // Right-to-left runs can make this negative; such text keeps its order and
                    // does not wrap well yet (see BACKLOG "bidi").
                    width: (x_at(end) - src_x).max(0.0),
                    kind: if cluster.chars().all(char::is_whitespace) {
                        AtomKind::Space
                    } else {
                        AtomKind::Glyphs
                    },
                }
            })
            .collect();
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
            atom_spans,
            glyphs,
            ascent: layout.ascent,
            descent: layout.descent,
        }
    }
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
        shape: impl FnOnce() -> ShapedMessage,
    ) -> MessageLayout {
        let width_bits = width.to_bits();
        if let Some(entry) = self.entries.get_mut(&seq)
            && entry.key == key
            && Arc::ptr_eq(&entry.message, message)
        {
            if entry.width_bits != width_bits {
                entry.flow = Rc::new(flow::flow(&entry.shaped.atoms, width));
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
        let flow = Rc::new(flow::flow(&shaped.atoms, width));
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
        let display = display::build(&message, DisplayOptions::default());
        let layout = fake_layout(&display.text);
        ShapedMessage::from_layout(display, &layout)
    }

    fn message(text: &str) -> Arc<Message> {
        Arc::new(Message::chat(
            "1",
            Timestamp(0),
            Author::new("al", "Al"),
            vec![Fragment::Text(text.into())],
        ))
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

    #[test]
    fn empty_text_shapes_to_nothing() {
        let display = DisplayText::default();
        let s = ShapedMessage::from_layout(display, &LineLayout::default());
        assert!(s.atoms.is_empty() && s.glyphs.is_empty());
        assert_eq!(flow::flow(&s.atoms, 100.0).line_count(), 1);
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

        let first = cache.layout(7, &m, key, 1000.0, || shape("hello world"));
        assert_eq!(first.flow.line_count(), 1);
        // Same width: a hit, nothing recomputed.
        cache.layout(7, &m, key, 1000.0, || shape("hello world"));
        assert_eq!(shapes.get(), 1);
        assert_eq!(cache.stats().hits, 1);

        // Resize: re-flow only, and the result wraps.
        let narrow = cache.layout(7, &m, key, 80.0, || shape("hello world"));
        assert_eq!(shapes.get(), 1, "a width change must not re-shape");
        // "Al: hello world" at 10 px a char in 80 px: "Al:" | "hello" | "world".
        assert_eq!(narrow.flow.line_count(), 3);
        assert!(Rc::ptr_eq(&first.shaped, &narrow.shaped));

        // Settings change (new generation) or another scale factor: re-shape.
        cache.layout(7, &m, LayoutKey::new(2, 2.0), 80.0, || shape("hello world"));
        assert_eq!(shapes.get(), 2);
        cache.layout(7, &m, LayoutKey::new(2, 1.0), 80.0, || shape("hello world"));
        assert_eq!(shapes.get(), 3);

        // The store replaced the message (marked deleted): a new `Arc`, re-shape.
        let deleted = Arc::new(m.with_deleted());
        cache.layout(7, &deleted, LayoutKey::new(2, 1.0), 80.0, || {
            shape("hello world")
        });
        assert_eq!(shapes.get(), 4);
    }

    #[test]
    fn cache_drops_evicted_and_stale_entries() {
        let mut cache = LayoutCache::default();
        let m = message("x");
        for seq in 0..10 {
            cache.layout(seq, &m, LayoutKey::new(1, 1.0), 100.0, || shaped("x"));
        }
        cache.evict_before(6);
        assert_eq!(cache.len(), 4);
        cache.layout(9, &m, LayoutKey::new(2, 1.0), 100.0, || shaped("x"));
        cache.retain_key(2);
        assert_eq!(cache.len(), 1);
    }
}
