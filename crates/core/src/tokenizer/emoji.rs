//! Emoji sequence detection without external data tables.
//!
//! [`emoji_len`] returns the byte length of the emoji *grapheme* at the start of a
//! string: a base emoji plus variation selectors, skin tones, tag sequences and
//! ZWJ-joined components; a regional-indicator pair (flag); or a keycap sequence.
//! The pictographic table is Unicode's `Extended_Pictographic` (as of Unicode 16),
//! trimmed to what matters here; unassigned code points inside the blocks are
//! tolerated on purpose so new emoji work before this table is updated.

const ZWJ: char = '\u{200D}';
const VS15: char = '\u{FE0E}'; // text presentation
const VS16: char = '\u{FE0F}'; // emoji presentation
const KEYCAP: char = '\u{20E3}';

/// `Extended_Pictographic` ranges (sorted, inclusive).
const PICTOGRAPHIC: &[(u32, u32)] = &[
    (0x00A9, 0x00A9),
    (0x00AE, 0x00AE),
    (0x203C, 0x203C),
    (0x2049, 0x2049),
    (0x2122, 0x2122),
    (0x2139, 0x2139),
    (0x2194, 0x2199),
    (0x21A9, 0x21AA),
    (0x231A, 0x231B),
    (0x2328, 0x2328),
    (0x2388, 0x2388),
    (0x23CF, 0x23CF),
    (0x23E9, 0x23F3),
    (0x23F8, 0x23FA),
    (0x24C2, 0x24C2),
    (0x25AA, 0x25AB),
    (0x25B6, 0x25B6),
    (0x25C0, 0x25C0),
    (0x25FB, 0x25FE),
    (0x2600, 0x2605),
    (0x2607, 0x2612),
    (0x2614, 0x2685),
    (0x2690, 0x2705),
    (0x2708, 0x2712),
    (0x2714, 0x2714),
    (0x2716, 0x2716),
    (0x271D, 0x271D),
    (0x2721, 0x2721),
    (0x2728, 0x2728),
    (0x2733, 0x2734),
    (0x2744, 0x2744),
    (0x2747, 0x2747),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2763, 0x2767),
    (0x2795, 0x2797),
    (0x27A1, 0x27A1),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2934, 0x2935),
    (0x2B05, 0x2B07),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x3030, 0x3030),
    (0x303D, 0x303D),
    (0x3297, 0x3297),
    (0x3299, 0x3299),
    (0x1F000, 0x1F0FF),
    (0x1F10D, 0x1F10F),
    (0x1F12F, 0x1F12F),
    (0x1F16C, 0x1F171),
    (0x1F17E, 0x1F17F),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F1AD, 0x1F1E5),
    (0x1F201, 0x1F20F),
    (0x1F21A, 0x1F21A),
    (0x1F22F, 0x1F22F),
    (0x1F232, 0x1F23A),
    (0x1F23C, 0x1F23F),
    (0x1F249, 0x1F3FA),
    (0x1F400, 0x1F53D),
    (0x1F546, 0x1F64F),
    (0x1F680, 0x1F6FF),
    (0x1F774, 0x1F77F),
    (0x1F7D5, 0x1F7FF),
    (0x1F80C, 0x1F80F),
    (0x1F848, 0x1F84F),
    (0x1F85A, 0x1F85F),
    (0x1F888, 0x1F88F),
    (0x1F8AE, 0x1F8FF),
    (0x1F90C, 0x1F93A),
    (0x1F93C, 0x1F945),
    (0x1F947, 0x1FAFF),
    (0x1FC00, 0x1FFFD),
];

/// BMP code points that are emoji *by default* (`Emoji_Presentation`). The other
/// BMP pictographs (©, ™, ❤, ✌, ...) are text unless followed by U+FE0F.
const BMP_EMOJI_PRESENTATION: &[(u32, u32)] = &[
    (0x231A, 0x231B),
    (0x23E9, 0x23EC),
    (0x23F0, 0x23F0),
    (0x23F3, 0x23F3),
    (0x25FD, 0x25FE),
    (0x2614, 0x2615),
    (0x2648, 0x2653),
    (0x267F, 0x267F),
    (0x2693, 0x2693),
    (0x26A1, 0x26A1),
    (0x26AA, 0x26AB),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x26CE, 0x26CE),
    (0x26D4, 0x26D4),
    (0x26EA, 0x26EA),
    (0x26F2, 0x26F3),
    (0x26F5, 0x26F5),
    (0x26FA, 0x26FA),
    (0x26FD, 0x26FD),
    (0x2705, 0x2705),
    (0x270A, 0x270B),
    (0x2728, 0x2728),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2795, 0x2797),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
];

fn in_table(table: &[(u32, u32)], c: char) -> bool {
    let c = c as u32;
    table
        .binary_search_by(|&(lo, hi)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn is_skin_tone(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

fn is_tag(c: char) -> bool {
    ('\u{E0020}'..='\u{E007F}').contains(&c)
}

fn is_pictographic(c: char) -> bool {
    in_table(PICTOGRAPHIC, c)
}

/// Does an emoji sequence start with `c`? `vs16` says whether U+FE0F follows it.
fn starts_emoji(c: char, vs16: bool) -> bool {
    if is_skin_tone(c) {
        return true;
    }
    if c as u32 >= 0x1F000 {
        return is_pictographic(c);
    }
    in_table(BMP_EMOJI_PRESENTATION, c) || (vs16 && is_pictographic(c))
}

/// Byte length of the emoji sequence at the start of `s`, or `None` if `s` does
/// not start with one.
pub(crate) fn emoji_len(s: &str) -> Option<usize> {
    let mut chars = s.chars();
    let first = chars.next()?;

    // Keycap: [0-9#*] VS16? U+20E3. Bare digits are ordinary text.
    if matches!(first, '0'..='9' | '#' | '*') {
        let mut len = first.len_utf8();
        let mut next = chars.next();
        if next == Some(VS16) {
            len += VS16.len_utf8();
            next = chars.next();
        }
        return (next == Some(KEYCAP)).then(|| len + KEYCAP.len_utf8());
    }
    if first.is_ascii() {
        return None; // fast path: most chat text
    }

    // Flag: exactly one pair of regional indicators.
    if is_regional_indicator(first) {
        return match chars.next() {
            Some(second) if is_regional_indicator(second) => {
                Some(first.len_utf8() + second.len_utf8())
            }
            _ => None,
        };
    }

    let next = chars.clone().next();
    if next == Some(VS15) || !starts_emoji(first, next == Some(VS16)) {
        return None;
    }

    let mut len = first.len_utf8();
    loop {
        let mut look = chars.clone();
        match look.next() {
            Some(c) if c == VS16 || is_skin_tone(c) || is_tag(c) || c == KEYCAP => {
                len += c.len_utf8();
                chars = look;
            }
            Some(ZWJ) => match look.next() {
                // A ZWJ only joins when another emoji component follows.
                Some(c) if is_pictographic(c) || is_skin_tone(c) => {
                    len += ZWJ.len_utf8() + c.len_utf8();
                    chars = look;
                }
                _ => break,
            },
            _ => break,
        }
    }
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole(s: &str) -> bool {
        emoji_len(s) == Some(s.len())
    }

    #[test]
    fn simple_emoji() {
        assert!(whole("😀"));
        assert!(whole("🔥"));
        assert!(whole("✅"));
        assert!(whole("⭐"));
        assert!(whole("🫠")); // newer block, covered by the range
    }

    #[test]
    fn text_default_symbols_need_vs16() {
        assert_eq!(emoji_len("©"), None);
        assert_eq!(emoji_len("™"), None);
        assert_eq!(emoji_len("❤"), None);
        assert!(whole("❤\u{FE0F}"));
        assert!(whole("©\u{FE0F}"));
        // Explicit text presentation selector is respected.
        assert_eq!(emoji_len("⭐\u{FE0E}"), None);
    }

    #[test]
    fn skin_tones_and_zwj_stay_single() {
        assert!(whole("👍🏽"));
        assert!(whole("👨\u{200D}👩\u{200D}👧\u{200D}👦"));
        assert!(whole("🏳\u{FE0F}\u{200D}🌈"));
        assert!(whole("❤\u{FE0F}\u{200D}🔥"));
        assert!(whole("🧑🏽\u{200D}💻"));
        assert!(whole("🏃\u{200D}♀\u{FE0F}"));
    }

    #[test]
    fn flags_and_tag_sequences() {
        assert!(whole("🇯🇵"));
        // Only one pair is consumed; the next pair is a separate flag.
        assert_eq!(emoji_len("🇯🇵🇺🇸"), Some("🇯🇵".len()));
        assert_eq!(emoji_len("🇯"), None, "lone regional indicator");
        // England: black flag + tag letters + cancel tag.
        assert!(whole(
            "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}"
        ));
    }

    #[test]
    fn keycaps() {
        assert!(whole("1\u{FE0F}\u{20E3}"));
        assert!(whole("#\u{20E3}"));
        assert_eq!(emoji_len("1"), None);
        assert_eq!(emoji_len("12"), None);
        assert_eq!(emoji_len("#hashtag"), None);
    }

    #[test]
    fn trailing_zwj_does_not_extend() {
        let s = "😀\u{200D}x";
        assert_eq!(emoji_len(s), Some("😀".len()));
    }

    #[test]
    fn non_emoji_is_none() {
        assert_eq!(emoji_len(""), None);
        assert_eq!(emoji_len("a"), None);
        assert_eq!(emoji_len("é"), None);
        assert_eq!(emoji_len("日本"), None);
        assert_eq!(emoji_len("→"), None); // U+2192 is not pictographic
    }

    #[test]
    fn only_the_leading_sequence_is_measured() {
        assert_eq!(emoji_len("😀abc"), Some("😀".len()));
        assert_eq!(emoji_len("😀😀"), Some("😀".len()));
    }

    #[test]
    fn tables_are_sorted_and_disjoint() {
        for table in [PICTOGRAPHIC, BMP_EMOJI_PRESENTATION] {
            for w in table.windows(2) {
                assert!(
                    w[0].1 < w[1].0,
                    "{:X?} overlaps/unsorted vs {:X?}",
                    w[0],
                    w[1]
                );
            }
            assert!(table.iter().all(|(lo, hi)| lo <= hi));
        }
    }
}
