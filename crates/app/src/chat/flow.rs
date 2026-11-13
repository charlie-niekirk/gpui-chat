//! Line flow: places a message's atoms (grapheme clusters, later inline images) into wrapped
//! lines. Pure geometry in `f32` logical pixels, independent of GPUI, so it is unit-tested
//! directly.
//!
//! Rules:
//! * Lines break **before a word** (a run of non-space atoms) that does not fit after the
//!   spaces preceding it; those spaces stay at the end of the previous line, *hanging* past the
//!   edge (they do not count towards its width and are not drawn).
//! * A word wider than the whole line is hard-broken between atoms: it continues on the current
//!   line and breaks wherever the next atom does not fit. Atoms are whole grapheme clusters, so
//!   an emoji ZWJ sequence, a flag or a base letter with its combining marks never splits.
//! * An atom wider than the line on its own (a huge emoji in a very narrow pane) gets a line to
//!   itself and overflows rather than looping.
//!
//! **Seams for later PRs.** PR 2.2 adds [`AtomKind::Inline`] atoms (emote images with a known
//! logical width) to the same stream; flow treats them like glyph atoms, and the painter draws
//! an image instead of glyphs. PR 5.6 (selection across messages) hit-tests with
//! [`Flow::index_at`] / [`Flow::position_of`], which map between a point in the message and a
//! byte offset in its display text.

use std::ops::Range;

/// What an atom is, for wrapping and painting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomKind {
    /// Whitespace: a break opportunity after it; hangs at line ends.
    Space,
    /// A grapheme cluster drawn with glyphs.
    Glyphs,
    /// An inline box of known size (an emote image, PR 2.2). Unbreakable, like a glyph cluster.
    #[cfg_attr(not(test), allow(dead_code))]
    Inline,
}

/// One unbreakable piece of a message: a grapheme cluster or an inline box.
#[derive(Clone, Debug, PartialEq)]
pub struct Atom {
    /// Byte range in the display text.
    pub range: Range<usize>,
    /// X of the atom's first glyph in the unwrapped shaped line (to place its glyphs).
    pub src_x: f32,
    /// Advance width.
    pub width: f32,
    pub kind: AtomKind,
}

/// Where an atom landed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Placement {
    pub line: u32,
    /// Left edge on its line, relative to the content origin.
    pub x: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// Indices into the atom list.
    pub atoms: Range<usize>,
    /// Width of the line's content, without hanging spaces.
    pub width: f32,
}

/// The result of flowing atoms into lines. Always has at least one line.
#[derive(Clone, Debug, PartialEq)]
pub struct Flow {
    /// One per atom, in atom order.
    pub placements: Vec<Placement>,
    pub lines: Vec<Line>,
    /// The width this was flowed for (`f32::INFINITY` when unconstrained).
    pub max_width: f32,
}

impl Flow {
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The widest line's content width.
    pub fn content_width(&self) -> f32 {
        self.lines.iter().map(|line| line.width).fold(0.0, f32::max)
    }

    /// The byte offset in the display text closest to `x` on `line` (a caret position: the
    /// start or end of the atom under `x`, whichever is nearer). Clamps to the line's ends.
    #[cfg_attr(not(test), allow(dead_code))] // First used by selection (PR 5.6).
    pub fn index_at(&self, atoms: &[Atom], line: usize, x: f32) -> usize {
        let line = &self.lines[line.min(self.lines.len() - 1)];
        let mut caret = atoms.get(line.atoms.start).map_or(0, |a| a.range.start);
        for i in line.atoms.clone() {
            let atom = &atoms[i];
            let left = self.placements[i].x;
            if atom.kind == AtomKind::Space && left >= line.width {
                break; // a hanging space past the content
            }
            if x < left + atom.width / 2.0 {
                return atom.range.start;
            }
            caret = atom.range.end;
        }
        caret
    }

    /// The line and x of the caret before byte `index` (or after the last atom).
    #[cfg_attr(not(test), allow(dead_code))] // First used by selection (PR 5.6).
    pub fn position_of(&self, atoms: &[Atom], index: usize) -> (usize, f32) {
        let i = atoms.partition_point(|atom| atom.range.end <= index);
        match atoms.get(i) {
            Some(atom) => {
                let place = self.placements[i];
                let x = if index > atom.range.start {
                    place.x + atom.width
                } else {
                    place.x
                };
                (place.line as usize, x)
            }
            None => {
                let last = self.lines.len() - 1;
                (last, self.lines[last].width)
            }
        }
    }
}

/// Slack when checking whether content fits a line (logical pixels), so float rounding in the
/// summed advances never decides a break.
const FIT_EPSILON: f32 = 0.01;

/// Line-in-progress state of [`flow`].
struct Cursor {
    line: u32,
    line_start: usize,
    x: f32,
    /// Right edge of the last non-space atom on the line.
    content_end: f32,
    has_content: bool,
}

impl Cursor {
    /// Ends the current line before atom `at`.
    fn break_before(&mut self, at: usize, lines: &mut Vec<Line>) {
        lines.push(Line {
            atoms: self.line_start..at,
            width: self.content_end,
        });
        self.line += 1;
        self.line_start = at;
        self.x = 0.0;
        self.content_end = 0.0;
        self.has_content = false;
    }
}

/// Flows `atoms` into lines no wider than `max_width` (non-positive or NaN means unconstrained).
pub fn flow(atoms: &[Atom], max_width: f32) -> Flow {
    let max = if max_width > 0.0 {
        max_width
    } else {
        f32::INFINITY
    };
    let mut placements = vec![Placement::default(); atoms.len()];
    let mut lines = Vec::new();
    let mut at = Cursor {
        line: 0,
        line_start: 0,
        x: 0.0,
        content_end: 0.0,
        has_content: false,
    };
    let mut i = 0;
    while i < atoms.len() {
        if atoms[i].kind == AtomKind::Space {
            // Spaces always follow content (or open the message), so they stay on this line and
            // hang past its edge if the next word wraps.
            placements[i] = Placement {
                line: at.line,
                x: at.x,
            };
            at.x += atoms[i].width;
            i += 1;
            continue;
        }
        let word_end = atoms[i..]
            .iter()
            .position(|atom| atom.kind == AtomKind::Space)
            .map_or(atoms.len(), |n| i + n);
        // Both fit checks add the word's atoms one by one, exactly as placement advances `x`
        // (from the current x, or from 0 after a break), and allow a hair of slack: a word that
        // fits can never pass the whole-word check and then hard-break its last atom on rounding.
        let overflows = |end: f32| end > max + FIT_EPSILON;
        let word = &atoms[i..word_end];
        let word_end_x = word.iter().fold(at.x, |x, atom| x + atom.width);
        let word_width = word.iter().fold(0.0, |x, atom| x + atom.width);
        // A word that fits on a line of its own moves to the next line whole.
        if at.has_content && overflows(word_end_x) && !overflows(word_width) {
            at.break_before(i, &mut lines);
        }
        for (j, atom) in atoms.iter().enumerate().take(word_end).skip(i) {
            // Hard break inside a word wider than a line, wherever the next atom overflows.
            if at.has_content && overflows(at.x + atom.width) {
                at.break_before(j, &mut lines);
            }
            placements[j] = Placement {
                line: at.line,
                x: at.x,
            };
            at.x += atom.width;
            at.content_end = at.x;
            at.has_content = true;
        }
        i = word_end;
    }
    lines.push(Line {
        atoms: at.line_start..atoms.len(),
        width: at.content_end,
    });
    Flow {
        placements,
        lines,
        max_width: max,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Atoms for `text`, one per char, each `w` wide (`#` is 3 wide, an inline box).
    fn atoms(text: &str, w: f32) -> Vec<Atom> {
        let mut x = 0.0;
        text.char_indices()
            .map(|(i, c)| {
                let (width, kind) = match c {
                    ' ' => (w, AtomKind::Space),
                    '#' => (3.0 * w, AtomKind::Inline),
                    _ => (w, AtomKind::Glyphs),
                };
                let atom = Atom {
                    range: i..i + c.len_utf8(),
                    src_x: x,
                    width,
                    kind,
                };
                x += width;
                atom
            })
            .collect()
    }

    /// The text of each line, hanging spaces trimmed.
    fn lines(text: &str, flow: &Flow, atoms: &[Atom]) -> Vec<String> {
        flow.lines
            .iter()
            .map(|line| {
                let range = line.atoms.clone();
                if range.is_empty() {
                    return String::new();
                }
                text[atoms[range.start].range.start..atoms[range.end - 1].range.end]
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn wrap(text: &str, max: f32) -> Vec<String> {
        let atoms = atoms(text, 1.0);
        lines(text, &flow(&atoms, max), &atoms)
    }

    #[test]
    fn short_text_is_one_line() {
        assert_eq!(wrap("hello world", 80.0), ["hello world"]);
        assert_eq!(wrap("hello world", 11.0), ["hello world"]);
    }

    #[test]
    fn breaks_at_whitespace_and_trailing_spaces_hang() {
        assert_eq!(wrap("hello world", 10.0), ["hello", "world"]);
        assert_eq!(wrap("aa bb cc dd", 5.0), ["aa bb", "cc dd"]);
        let a = atoms("aa bb cc", 1.0);
        let f = flow(&a, 5.0);
        // "aa bb" fits exactly; the space after it hangs and does not count.
        assert_eq!(f.lines[0].width, 5.0);
        assert_eq!(f.placements[6], Placement { line: 1, x: 0.0 });
    }

    #[test]
    fn runs_of_spaces_hang_at_the_break() {
        let text = "aaaa    bbbb";
        let a = atoms(text, 1.0);
        let f = flow(&a, 6.0);
        assert_eq!(lines(text, &f, &a), ["aaaa", "bbbb"]);
        assert_eq!(f.lines[0].width, 4.0);
        assert_eq!(f.placements[8], Placement { line: 1, x: 0.0 });
    }

    #[test]
    fn over_long_words_are_hard_broken_and_fill_the_current_line() {
        assert_eq!(wrap("abcdefghij", 4.0), ["abcd", "efgh", "ij"]);
        // After a short word the long one continues on the same line, then breaks.
        assert_eq!(wrap("hi abcdefghij", 5.0), ["hi ab", "cdefg", "hij"]);
        // A word that fits on a fresh line moves there instead of being broken.
        assert_eq!(wrap("hi abcde", 5.0), ["hi", "abcde"]);
    }

    #[test]
    fn grapheme_clusters_are_never_split() {
        // One atom per cluster: a flag (two code points) and a ZWJ family stay whole.
        let text = "ab🇩🇪👨‍👩‍👧cd";
        let ranges = [0..1, 1..2, 2..10, 10..28, 28..29, 29..30];
        let widths = [1.0, 1.0, 2.0, 2.0, 1.0, 1.0];
        let mut x = 0.0;
        let a: Vec<Atom> = ranges
            .iter()
            .zip(widths)
            .map(|(range, width)| {
                let atom = Atom {
                    range: range.clone(),
                    src_x: x,
                    width,
                    kind: AtomKind::Glyphs,
                };
                x += width;
                atom
            })
            .collect();
        let f = flow(&a, 3.0);
        assert_eq!(lines(text, &f, &a), ["ab", "🇩🇪", "👨‍👩‍👧c", "d"]);
    }

    #[test]
    fn a_word_that_fits_exactly_is_not_split_by_float_rounding() {
        // 0.3 + 0.7 = 1.0, then 1.0 + (0.1 + 0.3) rounds to 1.4 (fits) while (1.0 + 0.1) + 0.3
        // rounds just above 1.4: the two ways of summing must not disagree about "bc".
        let width = |c| match c {
            'a' => 0.3,
            ' ' => 0.7,
            'b' => 0.1,
            _ => 0.3,
        };
        let mut x = 0.0f32;
        let a: Vec<Atom> = "a bc"
            .char_indices()
            .map(|(i, c)| {
                let atom = Atom {
                    range: i..i + 1,
                    src_x: x,
                    width: width(c),
                    kind: if c == ' ' {
                        AtomKind::Space
                    } else {
                        AtomKind::Glyphs
                    },
                };
                x += atom.width;
                atom
            })
            .collect();
        let x = a[0].width + a[1].width;
        assert!(
            (x + a[2].width) + a[3].width > 1.4 && x + (a[2].width + a[3].width) <= 1.4,
            "the rounding this test relies on"
        );
        assert_eq!(lines("a bc", &flow(&a, 1.4), &a), ["a bc"]);
        // Slightly narrower than the content: the whole word moves, it is never hard-broken.
        assert_eq!(lines("a bc", &flow(&a, 1.3), &a), ["a", "bc"]);
    }

    #[test]
    fn an_atom_wider_than_the_line_overflows_on_its_own_line() {
        let text = "a#b";
        let a = atoms(text, 1.0);
        let f = flow(&a, 2.0);
        assert_eq!(lines(text, &f, &a), ["a", "#", "b"]);
        assert_eq!(f.lines[1].width, 3.0);
    }

    #[test]
    fn inline_boxes_flow_like_glyphs() {
        let text = "x # y";
        let a = atoms(text, 1.0);
        assert_eq!(lines(text, &flow(&a, 5.0), &a), ["x #", "y"]);
    }

    #[test]
    fn unconstrained_and_empty() {
        let a = atoms("a b c", 1.0);
        assert_eq!(flow(&a, 0.0).line_count(), 1);
        assert_eq!(flow(&a, f32::NAN).line_count(), 1);
        let empty = flow(&[], 10.0);
        assert_eq!(empty.line_count(), 1);
        assert_eq!(empty.content_width(), 0.0);
    }

    #[test]
    fn rewrapping_the_same_atoms_at_another_width() {
        let text = "the quick brown fox jumps over the lazy dog";
        let a = atoms(text, 1.0);
        let narrow = flow(&a, 10.0);
        let wide = flow(&a, 100.0);
        assert_eq!(wide.line_count(), 1);
        assert_eq!(
            lines(text, &narrow, &a),
            ["the quick", "brown fox", "jumps over", "the lazy", "dog"]
        );
        assert!(narrow.lines.iter().all(|line| line.width <= 10.0));
    }

    #[test]
    fn hit_testing_round_trips_between_points_and_offsets() {
        let text = "hello world foo";
        let a = atoms(text, 2.0);
        let f = flow(&a, 22.0); // "hello world" (22) | "foo"
        assert_eq!(lines(text, &f, &a), ["hello world", "foo"]);
        // Start, middle of a glyph (rounds to nearer edge), past the end of a line.
        assert_eq!(f.index_at(&a, 0, 0.0), 0);
        assert_eq!(f.index_at(&a, 0, 2.9), 1);
        assert_eq!(f.index_at(&a, 0, 3.1), 2);
        assert_eq!(
            f.index_at(&a, 0, 500.0),
            11,
            "end of 'world', not the hanging space"
        );
        assert_eq!(f.index_at(&a, 1, 2.0), 13);
        assert_eq!(
            f.index_at(&a, 9, 0.0),
            12,
            "lines past the end clamp to the last"
        );
        for index in [0, 3, 6, 12, 14] {
            let (line, x) = f.position_of(&a, index);
            assert_eq!(f.index_at(&a, line, x), index, "index {index}");
        }
        assert_eq!(f.position_of(&a, text.len()), (1, 6.0));
    }
}
