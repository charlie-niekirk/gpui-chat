//! Text selection across messages (PR 5.6), the pure part: the selection model, hit-testing
//! helpers, the highlight geometry and the text that copying produces. No GPUI: the window side
//! (mouse handling, painting, autoscroll) is [`super::selection_layer`].
//!
//! **Positions.** A [`TextPos`] is a caret in one message's *display* text (what is drawn,
//! [`super::display`]): the message's store sequence number and a byte offset. Positions order by
//! message, then offset, so a selection that starts in one message and ends in another covers
//! everything between them.
//!
//! **Units.** A press picks a unit at the pointer by its click count ([`Granularity`]): a caret
//! (one click), a word (two) or the whole message (three). Dragging extends the selection by the
//! same unit, so a double-click-drag selects whole words. The selection always covers its anchor
//! unit and its head unit.
//!
//! **Copying.** One line per message. A message selected from its first to its last character is
//! copied as `HH:MM name: text` from the stored message (the original text: a reply keeps the
//! `@parent` Twitch put in front of it, emotes are their codes), whatever of it is drawn. A message
//! selected only in part is copied as the part that was selected, without badge titles (badges are
//! images).

use std::ops::{Range, RangeInclusive};

use gpui_chat_core::{Author, Message, MessageKind};

use super::display::{self, DisplayText, Role};
use super::flow::{Atom, Flow};

/// A caret between two characters of one message's display text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPos {
    /// The message's store sequence number.
    pub seq: u64,
    /// Byte offset in its display text. `usize::MAX` is "the end", whatever its length.
    pub offset: usize,
}

impl TextPos {
    pub fn new(seq: u64, offset: usize) -> Self {
        Self { seq, offset }
    }
}

/// What a press selects, and what dragging after it extends by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Granularity {
    Char,
    Word,
    Message,
}

impl Granularity {
    /// One click places a caret, two select a word, three (or more) the message.
    pub fn from_clicks(count: usize) -> Self {
        match count {
            0 | 1 => Self::Char,
            2 => Self::Word,
            _ => Self::Message,
        }
    }
}

/// A selection: the unit it started on (the anchor) and the unit the pointer is on now (the head).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    anchor: Range<TextPos>,
    head: Range<TextPos>,
    pub granularity: Granularity,
}

impl Selection {
    pub fn new(unit: Range<TextPos>, granularity: Granularity) -> Self {
        Self {
            anchor: unit.clone(),
            head: unit,
            granularity,
        }
    }

    /// Moves the head to `unit`; whether anything changed.
    pub fn extend_to(&mut self, unit: Range<TextPos>) -> bool {
        if self.head == unit {
            return false;
        }
        self.head = unit;
        true
    }

    /// Moves the selection to the sequence numbers `map` gives (the store renumbered its messages,
    /// see [`gpui_chat_core::Rebuild`]); byte offsets are into display texts, which a renumbering
    /// does not change. `false` when the message of an end is gone: the selection is then unchanged
    /// and no longer means anything, and the caller drops it.
    pub fn remap(&mut self, map: impl Fn(u64) -> Option<u64>) -> bool {
        let moved = |unit: &Range<TextPos>| {
            Some(
                TextPos::new(map(unit.start.seq)?, unit.start.offset)
                    ..TextPos::new(map(unit.end.seq)?, unit.end.offset),
            )
        };
        let (Some(anchor), Some(head)) = (moved(&self.anchor), moved(&self.head)) else {
            return false;
        };
        self.anchor = anchor;
        self.head = head;
        true
    }

    pub fn start(&self) -> TextPos {
        self.anchor.start.min(self.head.start)
    }

    pub fn end(&self) -> TextPos {
        self.anchor.end.max(self.head.end)
    }

    pub fn is_empty(&self) -> bool {
        self.start() >= self.end()
    }

    /// The messages it touches, by sequence number.
    pub fn seqs(&self) -> RangeInclusive<u64> {
        self.start().seq..=self.end().seq
    }

    /// The selected bytes of message `seq`, whose display text is `len` bytes long; `None` when
    /// none of it is selected.
    pub fn range_in(&self, seq: u64, len: usize) -> Option<Range<usize>> {
        let (start, end) = (self.start(), self.end());
        if seq < start.seq || seq > end.seq {
            return None;
        }
        let from = if seq == start.seq {
            start.offset.min(len)
        } else {
            0
        };
        let to = if seq == end.seq {
            end.offset.min(len)
        } else {
            len
        };
        (from < to).then_some(from..to)
    }
}

/// The unit of message `seq` at `offset` in its display text.
pub fn unit_at(
    display: &DisplayText,
    seq: u64,
    offset: usize,
    granularity: Granularity,
) -> Range<TextPos> {
    let at = |offset| TextPos::new(seq, offset);
    match granularity {
        Granularity::Char => at(offset)..at(offset),
        Granularity::Word => {
            let word = word_at(display, offset);
            at(word.start)..at(word.end)
        }
        Granularity::Message => at(0)..at(usize::MAX),
    }
}

/// The word around the character at byte `offset` (the last character when `offset` is the end):
/// the run of non-whitespace characters around it (or of whitespace, if it is a space), inside its
/// span. A span is one name, mention, link, emote, … so a double-click on a name does not take the
/// `:` after it, and a link or an emote code is taken whole.
pub fn word_at(display: &DisplayText, offset: usize) -> Range<usize> {
    let text = display.text.as_str();
    if text.is_empty() {
        return 0..0;
    }
    let mut at = offset.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    if at == text.len() {
        at = text[..at].char_indices().next_back().map_or(0, |(i, _)| i);
    }
    let span = display
        .span_at(at)
        .map_or(0..text.len(), |i| display.spans[i].range.clone());
    let Some(first) = text[at..].chars().next() else {
        return at..at;
    };
    let space = first.is_whitespace();
    let mut start = at;
    for c in text[span.start..at].chars().rev() {
        if c.is_whitespace() != space {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = at + first.len_utf8();
    for c in text[end..span.end.max(end)].chars() {
        if c.is_whitespace() != space {
            break;
        }
        end += c.len_utf8();
    }
    start..end
}

/// Where a height falls among rows stacked top to bottom (`(top, bottom)` each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowSide {
    /// Above the row's content: before its first character.
    Before,
    Inside,
    /// Below the row: after its last character.
    After,
}

/// The row at height `y`: above every row is the start of the first, below every row the end of
/// the last, and a gap between two rows the start of the lower one. `None` without rows.
pub fn row_at(rows: &[(f32, f32)], y: f32) -> Option<(usize, RowSide)> {
    let (last, _) = rows.iter().enumerate().next_back()?;
    match rows.iter().position(|&(_, bottom)| y < bottom) {
        Some(i) if y >= rows[i].0 => Some((i, RowSide::Inside)),
        Some(i) => Some((i, RowSide::Before)),
        None => Some((last, RowSide::After)),
    }
}

/// The caret at `(x, y)` relative to a message's content origin: above the text is its start,
/// below it its end.
pub fn caret_at(flow: &Flow, atoms: &[Atom], len: usize, x: f32, y: f32) -> usize {
    if y < 0.0 {
        0
    } else if y >= flow.height() {
        len
    } else {
        flow.index_at(atoms, flow.line_at(y), x)
    }
}

/// The atom drawn at `(x, y)` relative to a message's content origin, if any.
pub fn atom_under(flow: &Flow, atoms: &[Atom], x: f32, y: f32) -> Option<usize> {
    if y < 0.0 || y >= flow.height() {
        return None;
    }
    flow.atom_at(atoms, flow.line_at(y), x)
}

/// A piece of the highlight: on `line`, from `x`, `width` wide (relative to the content origin).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineRect {
    pub line: usize,
    pub x: f32,
    pub width: f32,
}

/// What to highlight for the bytes `range` of a message: one rectangle per line, around the atoms
/// on it that overlap `range` (an atom partly selected is highlighted whole; spaces hanging past a
/// line's end are not drawn, so they are not highlighted).
pub fn highlight(flow: &Flow, atoms: &[Atom], range: Range<usize>) -> Vec<LineRect> {
    let mut rects = Vec::new();
    if range.is_empty() {
        return rects;
    }
    for (index, line) in flow.lines.iter().enumerate() {
        let mut extent: Option<(f32, f32)> = None;
        for i in line.atoms.clone() {
            let atom = &atoms[i];
            let left = flow.placements[i].x;
            if left >= line.width || atom.range.start >= range.end || atom.range.end <= range.start
            {
                continue;
            }
            let right = left + atom.width;
            extent = Some(extent.map_or((left, right), |(l, r)| (l.min(left), r.max(right))));
        }
        if let Some((left, right)) = extent
            && right > left
        {
            rects.push(LineRect {
                line: index,
                x: left,
                width: right - left,
            });
        }
    }
    rects
}

/// `text` as one line: control characters (newlines, tabs) become spaces and the U+E0000 tag some
/// clients append is dropped. Nothing else changes.
pub fn one_line(text: &str) -> String {
    text.chars()
        .filter(|&c| c != '\u{E0000}')
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A message as one copied line: `HH:MM name: text` (`HH:MM name text` for `/me`, `HH:MM text` for
/// notices), with the original text of the message.
pub fn message_line(message: &Message) -> String {
    let time = display::clock(message.timestamp.unix_millis());
    let body = one_line(&message.plain_text());
    let name = message.author.as_ref().map(display::author_name);
    let rest = match (message.kind, name) {
        (MessageKind::Chat, Some(name)) => format!("{name}: {body}"),
        (MessageKind::Action, Some(name)) => format!("{name} {body}"),
        (MessageKind::UserNotice, name) => {
            let mut rest = message
                .system_text
                .as_deref()
                .map(one_line)
                .unwrap_or_default();
            if !message.fragments.is_empty() {
                if !rest.is_empty() {
                    rest.push(' ');
                }
                match name {
                    Some(name) => rest.push_str(&format!("{name}: {body}")),
                    None => rest.push_str(&body),
                }
            }
            rest
        }
        _ => body,
    };
    if rest.is_empty() {
        time
    } else {
        format!("{time} {rest}")
    }
}

/// The text of `range` of a display text as drawn, without badge titles (and the space after each
/// badge): badges are pictures. An end that is not on a character boundary (offsets measured on
/// another version of the text) is moved out to the nearest one, never panicking.
pub fn selected_text(display: &DisplayText, range: Range<usize>) -> String {
    let text = display.text.as_str();
    let range = floor_boundary(text, range.start)..ceil_boundary(text, range.end);
    let mut out = String::new();
    let mut after_badge = false;
    for span in &display.spans {
        let badge = span.role == Role::Badge;
        let skip = badge || (after_badge && span.role == Role::Separator);
        after_badge = badge;
        if skip {
            continue;
        }
        let from = span.range.start.max(range.start);
        let to = span.range.end.min(range.end);
        if from < to {
            out.push_str(&display.text[from..to]);
        }
    }
    out
}

/// The nearest character boundary of `text` at or before `index` (clamped to its length).
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The nearest character boundary of `text` at or after `index` (clamped to its length).
fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// What a selection covers of one message: the message, its display text and the selected bytes.
pub struct CopyRow<'a> {
    pub message: &'a Message,
    pub display: &'a DisplayText,
    pub range: Range<usize>,
}

/// The text a selection copies: one line per message, in order (see the module docs). Messages of
/// which nothing printable is selected (only a badge, say) add no line.
pub fn copy_text<'a>(rows: impl IntoIterator<Item = CopyRow<'a>>) -> String {
    let lines: Vec<String> = rows
        .into_iter()
        .filter(|row| !row.range.is_empty())
        .map(|row| {
            if row.range.start == 0 && row.range.end >= row.display.text.len() {
                message_line(row.message)
            } else {
                one_line(&selected_text(row.display, row.range))
            }
        })
        .filter(|line| !line.is_empty())
        .collect();
    lines.join("\n")
}

/// What "Copy username" copies: the display name when it is the login with other capitals, else
/// the login (a localised display name is not a username).
pub fn username(author: &Author) -> &str {
    if author.display_name.eq_ignore_ascii_case(&author.login) {
        &author.display_name
    } else {
        &author.login
    }
}

/// Whether a link may be opened in the browser: only web addresses.
pub fn openable(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_chat_core::{Emote, Fragment, ReplyRef, Timestamp, Tokenizer};

    use super::*;
    use crate::chat::display::{BadgeSlot, DisplayOptions};
    use crate::chat::flow::{AtomKind, flow};

    fn pos(seq: u64, offset: usize) -> TextPos {
        TextPos::new(seq, offset)
    }

    fn chat(text: &str) -> Message {
        Message::chat(
            "1",
            Timestamp(3_661_000), // 01:01
            Author::new("alice", "Alice"),
            Tokenizer::new().tokenize(text, &[]).fragments,
        )
    }

    fn shown(message: &Message) -> DisplayText {
        display::build(message, DisplayOptions { timestamps: true }, &[])
    }

    #[test]
    fn a_selection_covers_both_units_whichever_way_it_was_dragged() {
        let mut s = Selection::new(pos(5, 3)..pos(5, 3), Granularity::Char);
        assert!(s.is_empty());
        assert!(s.extend_to(pos(7, 2)..pos(7, 2)));
        assert!(!s.extend_to(pos(7, 2)..pos(7, 2)), "no change");
        assert_eq!((s.start(), s.end()), (pos(5, 3), pos(7, 2)));
        assert_eq!(s.seqs(), 5..=7);
        // Backwards past the anchor.
        s.extend_to(pos(4, 1)..pos(4, 1));
        assert_eq!((s.start(), s.end()), (pos(4, 1), pos(5, 3)));
        // Words: the anchor word stays selected while the head moves before it.
        let mut w = Selection::new(pos(5, 4)..pos(5, 9), Granularity::Word);
        w.extend_to(pos(5, 0)..pos(5, 2));
        assert_eq!((w.start(), w.end()), (pos(5, 0), pos(5, 9)));
    }

    #[test]
    fn each_message_gets_its_part_of_the_selection() {
        let s = Selection::new(pos(5, 3)..pos(5, 3), Granularity::Char);
        let mut s2 = s.clone();
        s2.extend_to(pos(7, 2)..pos(7, 2));
        assert_eq!(s2.range_in(4, 10), None);
        assert_eq!(s2.range_in(5, 10), Some(3..10));
        assert_eq!(
            s2.range_in(6, 10),
            Some(0..10),
            "a message in between is whole"
        );
        assert_eq!(s2.range_in(7, 10), Some(0..2));
        assert_eq!(s2.range_in(8, 10), None);
        // Nothing selected of a message the selection only touches at an edge.
        let mut edge = s.clone();
        edge.extend_to(pos(6, 0)..pos(6, 0));
        assert_eq!(edge.range_in(6, 10), None);
        // Offsets past the end (a whole-message unit, or a shorter replacement) clamp.
        let whole = Selection::new(pos(3, 0)..pos(3, usize::MAX), Granularity::Message);
        assert_eq!(whole.range_in(3, 12), Some(0..12));
        assert_eq!(
            s.range_in(5, 10),
            None,
            "an empty selection selects nothing"
        );
    }

    #[test]
    fn clicks_pick_the_unit() {
        assert_eq!(Granularity::from_clicks(1), Granularity::Char);
        assert_eq!(Granularity::from_clicks(2), Granularity::Word);
        assert_eq!(Granularity::from_clicks(3), Granularity::Message);
        assert_eq!(Granularity::from_clicks(7), Granularity::Message);
        let display = shown(&chat("hello there"));
        assert_eq!(
            unit_at(&display, 2, 13, Granularity::Char),
            pos(2, 13)..pos(2, 13)
        );
        assert_eq!(
            unit_at(&display, 2, 15, Granularity::Word),
            pos(2, 13)..pos(2, 18)
        );
        assert_eq!(
            unit_at(&display, 2, 15, Granularity::Message),
            pos(2, 0)..pos(2, usize::MAX)
        );
    }

    #[test]
    fn a_word_is_a_run_of_non_spaces_inside_its_span() {
        // "01:01 Alice: hi @Bob, see https://x.com/a?b=c KEKW"
        let display = shown(&chat("hi @Bob, see https://x.com/a?b=c KEKW"));
        let word = |needle: &str, at: usize| {
            let offset = display.text.find(needle).unwrap() + at;
            display.text[word_at(&display, offset)].to_owned()
        };
        assert_eq!(word("Alice", 2), "Alice", "the name, without the colon");
        assert_eq!(word("hi", 0), "hi");
        assert_eq!(word("@Bob", 1), "@Bob", "the mention, without the comma");
        assert_eq!(word("https", 10), "https://x.com/a?b=c", "a link whole");
        assert_eq!(word("KEKW", 3), "KEKW");
        assert_eq!(word("01:01", 0), "01:01");
        // On a space: the run of spaces. At the very end: the last word.
        assert_eq!(word(" see", 0), " ");
        assert_eq!(&display.text[word_at(&display, display.text.len())], "KEKW");
        assert_eq!(word_at(&DisplayText::default(), 0), 0..0);
        // Not on a character boundary: the character it is inside.
        let accented = shown(&chat("a é b"));
        let inside = accented.text.find('é').unwrap() + 1;
        assert_eq!(&accented.text[word_at(&accented, inside)], "é");
    }

    #[test]
    fn rows_by_height_clamp_to_the_first_and_last() {
        let rows = [(0.0, 20.0), (20.0, 50.0), (55.0, 70.0)];
        assert_eq!(row_at(&rows, -3.0), Some((0, RowSide::Before)));
        assert_eq!(row_at(&rows, 0.0), Some((0, RowSide::Inside)));
        assert_eq!(row_at(&rows, 20.0), Some((1, RowSide::Inside)));
        assert_eq!(row_at(&rows, 52.0), Some((2, RowSide::Before)), "a gap");
        assert_eq!(row_at(&rows, 69.0), Some((2, RowSide::Inside)));
        assert_eq!(row_at(&rows, 70.0), Some((2, RowSide::After)));
        assert_eq!(row_at(&[], 1.0), None);
    }

    /// One atom per char, `w` wide; spaces are spaces.
    fn atoms(text: &str, w: f32) -> Vec<Atom> {
        text.char_indices()
            .map(|(i, c)| Atom {
                range: i..i + c.len_utf8(),
                src_x: i as f32 * w,
                width: w,
                height: 0.0,
                kind: if c == ' ' {
                    AtomKind::Space
                } else {
                    AtomKind::Glyphs
                },
            })
            .collect()
    }

    #[test]
    fn carets_above_and_below_the_text_are_its_ends() {
        let text = "hello world foo";
        let a = atoms(text, 2.0);
        let f = flow(&a, 22.0, 10.0); // "hello world" | "foo"
        assert_eq!(caret_at(&f, &a, text.len(), 50.0, -1.0), 0);
        assert_eq!(caret_at(&f, &a, text.len(), 0.0, 25.0), text.len());
        assert_eq!(caret_at(&f, &a, text.len(), 3.1, 2.0), 2);
        assert_eq!(caret_at(&f, &a, text.len(), 2.1, 12.0), 13);
        assert_eq!(atom_under(&f, &a, 3.1, 2.0), Some(1));
        assert_eq!(atom_under(&f, &a, 3.1, -2.0), None);
        assert_eq!(atom_under(&f, &a, 3.1, 20.0), None);
    }

    #[test]
    fn the_highlight_is_one_rectangle_per_line_around_the_selected_atoms() {
        let text = "hello world foo";
        let a = atoms(text, 2.0);
        let f = flow(&a, 22.0, 10.0); // "hello world" (0..22) | "foo" (0..6)
        // "lo world f": the end of line 0 (not the hanging space) and the start of line 1.
        assert_eq!(
            highlight(&f, &a, 3..13),
            [
                LineRect {
                    line: 0,
                    x: 6.0,
                    width: 16.0
                },
                LineRect {
                    line: 1,
                    x: 0.0,
                    width: 2.0
                },
            ]
        );
        // Only the hanging space: nothing to draw.
        assert!(highlight(&f, &a, 11..12).is_empty());
        assert!(highlight(&f, &a, 4..4).is_empty());
        // Part of an atom highlights the whole atom.
        let wide = vec![Atom {
            range: 0..5,
            src_x: 0.0,
            width: 28.0,
            height: 28.0,
            kind: AtomKind::Inline,
        }];
        let wf = flow(&wide, 100.0, 10.0);
        assert_eq!(
            highlight(&wf, &wide, 2..3),
            [LineRect {
                line: 0,
                x: 0.0,
                width: 28.0
            }]
        );
    }

    fn reply(message: &mut Message) {
        message.reply = Some(ReplyRef {
            parent_id: "p".into(),
            parent_login: "bob".into(),
            parent_display_name: "Bob".into(),
            parent_body: "x".into(),
            thread_parent_id: None,
        });
    }

    #[test]
    fn whole_messages_copy_as_time_name_and_original_text() {
        assert_eq!(
            message_line(&chat("hello world")),
            "01:01 Alice: hello world"
        );
        // A reply keeps the mention its display hides; emotes copy as their codes.
        let mut answer = Message::chat(
            "2",
            Timestamp(3_661_000),
            Author::new("alice", "Alice"),
            vec![
                Fragment::Mention {
                    text: "@bob".into(),
                    login: "bob".into(),
                    is_self: false,
                },
                Fragment::Text(" nice ".into()),
                Fragment::Emote {
                    emote: Arc::new(Emote::twitch("25", "Kappa")),
                    zero_width_overlays: vec![],
                },
            ],
        );
        reply(&mut answer);
        assert_eq!(shown(&answer).text, "01:01 Alice: nice Kappa");
        assert_eq!(message_line(&answer), "01:01 Alice: @bob nice Kappa");
        // /me, notices, sub messages; newlines and the tag character never split the line.
        let mut action = chat("waves");
        action.kind = MessageKind::Action;
        assert_eq!(message_line(&action), "01:01 Alice waves");
        let notice = Message::plain("3", Timestamp(0), MessageKind::Notice, "slow\nmode");
        assert_eq!(message_line(&notice), "00:00 slow mode");
        let mut sub = chat("hype\u{E0000}");
        sub.kind = MessageKind::UserNotice;
        sub.system_text = Some("Alice subscribed!".into());
        assert_eq!(message_line(&sub), "01:01 Alice subscribed! Alice: hype");
        sub.fragments.clear();
        assert_eq!(message_line(&sub), "01:01 Alice subscribed!");
    }

    fn badge_slot(title: &str) -> BadgeSlot {
        BadgeSlot {
            badge: Arc::new(gpui_chat_core::Badge {
                id: gpui_chat_core::BadgeId::new(title, "1"),
                title: title.to_owned(),
                urls: gpui_chat_core::ImageUrls::new(),
            }),
            url: gpui_chat_media::ImageKey::new("u"),
            size: 18.0,
        }
    }

    #[test]
    fn copying_gives_one_line_per_message_whole_or_as_selected() {
        let first = chat("first message here");
        let second = chat("second one");
        let mut third = chat("third and last");
        third.timestamp = Timestamp(7_200_000); // 02:00
        let slots = [badge_slot("Moderator")];
        let (d1, d2) = (shown(&first), shown(&second));
        let d3 = display::build(&third, DisplayOptions { timestamps: true }, &slots);
        assert_eq!(d3.text, "02:00 Moderator Alice: third and last");
        let in_first = d1.text.find("message").unwrap();
        let rows = [
            CopyRow {
                message: &first,
                display: &d1,
                range: in_first..d1.text.len(),
            },
            CopyRow {
                message: &second,
                display: &d2,
                range: 0..d2.text.len(),
            },
            // From the badge into the body: the badge's title is not copied.
            CopyRow {
                message: &third,
                display: &d3,
                range: 6..d3.text.find(" last").unwrap(),
            },
        ];
        assert_eq!(
            copy_text(rows),
            "message here\n01:01 Alice: second one\nAlice: third and"
        );
        // Only a badge selected: no line at all; an empty range neither.
        let badge_only = [
            CopyRow {
                message: &third,
                display: &d3,
                range: 6..15,
            },
            CopyRow {
                message: &first,
                display: &d1,
                range: 0..0,
            },
        ];
        assert_eq!(copy_text(badge_only), "");
        assert_eq!(
            selected_text(&d3, 0..d3.text.len()),
            "02:00 Alice: third and last"
        );
    }

    #[test]
    fn offsets_inside_a_character_snap_out_instead_of_panicking() {
        // Offsets measured on a text with a badge title, applied to the same message shown
        // without it: they land inside the multibyte characters.
        let message = chat("héllo wörld ünïcode");
        let display = shown(&message);
        let body = display.text.find('h').unwrap();
        let inside_e = body + 2; // the second byte of 'é'
        let inside_o = display.text.find('ö').unwrap() + 1;
        assert!(!display.text.is_char_boundary(inside_e));
        assert_eq!(selected_text(&display, inside_e..inside_o), "éllo wö");
        // Past the end clamps; a range inside one character still gives that character.
        assert_eq!(
            selected_text(&display, inside_o..usize::MAX),
            "örld ünïcode"
        );
        assert_eq!(selected_text(&display, inside_e..inside_e + 1), "é");
        let rows = [CopyRow {
            message: &message,
            display: &display,
            range: inside_e..inside_o,
        }];
        assert_eq!(copy_text(rows), "éllo wö");
    }

    #[test]
    fn usernames_and_openable_links() {
        assert_eq!(username(&Author::new("alice", "Alice")), "Alice");
        assert_eq!(username(&Author::new("alice", "앨리스")), "alice");
        assert!(openable("https://example.com"));
        assert!(openable("HTTP://example.com/a"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable("file:///etc/passwd"));
        assert!(!openable("http"));
    }

    #[test]
    fn a_selection_is_moved_to_new_sequence_numbers_or_refused_when_an_end_is_gone() {
        let pos = TextPos::new;
        let mut selection = Selection::new(pos(3, 2)..pos(3, 2), Granularity::Char);
        selection.extend_to(pos(5, 4)..pos(5, 4));
        // Everything moved up by ten (history inserted before it).
        assert!(selection.remap(|seq| Some(seq + 10)));
        assert_eq!(
            (selection.start(), selection.end()),
            (pos(13, 2), pos(15, 4))
        );
        assert_eq!(selection.seqs(), 13..=15);
        // A whole-message unit keeps its "end of text" marker.
        let mut message = Selection::new(pos(1, 0)..pos(1, usize::MAX), Granularity::Message);
        assert!(message.remap(|seq| Some(seq * 2)));
        assert_eq!(message.end(), pos(2, usize::MAX));
        // One end dropped by the rebuild: refused, and left as it was.
        let before = selection.clone();
        assert!(!selection.remap(|seq| (seq != 15).then_some(seq)));
        assert_eq!(selection, before);
    }
}
