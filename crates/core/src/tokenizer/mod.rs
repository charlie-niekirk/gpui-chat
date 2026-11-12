//! Splits message text into [`Fragment`]s.
//!
//! Pipeline for one message (see [`Tokenizer::tokenize`]):
//!
//! 1. Pre-resolved Twitch emote ranges (from the IRC `emotes` tag) are validated and
//!    become [`Fragment::Emote`]s. **Range offsets count Unicode code points**, not
//!    bytes or UTF-16 units. Out-of-range, inverted and overlapping ranges are ignored.
//! 2. Text between them is split on whitespace. Each word is tried, in order, as:
//!    a third-party emote ([`WordResolver`], matched against the *whole* word, as
//!    emote names may contain punctuation); a cheermote (`Cheer100`); otherwise it is
//!    scanned for emoji sequences, and the pieces between emoji are checked for
//!    `@mentions` and links (after trimming surrounding punctuation).
//! 3. A zero-width emote directly following an emote (only whitespace between) is
//!    stacked onto it as a `zero_width_overlays` entry.
//!
//! Plain words and whitespace are merged into [`Fragment::Text`] runs, so
//! concatenating the fragments' text reproduces the message (except whitespace
//! between an emote and its zero-width overlays).

mod emoji;
mod links;

use std::collections::HashMap;
use std::sync::Arc;

use crate::emote::Emote;
use crate::message::Fragment;

/// Looks up a whole whitespace-delimited word as an emote (BTTV / FFZ / 7TV).
///
/// Implemented for closures `Fn(&str) -> Option<Arc<Emote>>`; use [`NoResolver`] when
/// there is nothing to resolve. The word is passed exactly as typed (case-sensitive
/// emote names are the resolver's business).
pub trait WordResolver {
    fn resolve(&self, word: &str) -> Option<Arc<Emote>>;
}

impl<F> WordResolver for F
where
    F: Fn(&str) -> Option<Arc<Emote>>,
{
    fn resolve(&self, word: &str) -> Option<Arc<Emote>> {
        self(word)
    }
}

/// A resolver that never matches.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoResolver;

impl WordResolver for NoResolver {
    fn resolve(&self, _word: &str) -> Option<Arc<Emote>> {
        None
    }
}

/// Prefixes of Twitch's standard cheermotes; pass to
/// [`Tokenizer::with_cheermote_prefixes`] for messages that carry bits. Channel
/// specific prefixes come from Helix (not part of this PR).
pub const DEFAULT_CHEERMOTE_PREFIXES: &[&str] = &[
    "Cheer",
    "DoodleCheer",
    "BibleThump",
    "cheerwhal",
    "Corgo",
    "uni",
    "ShowLove",
    "Party",
    "SeemsGood",
    "Pride",
    "Kappa",
    "FrankerZ",
    "HeyGuys",
    "DansGame",
    "EleGiggle",
    "TriHard",
    "Kreygasm",
    "4Head",
    "SwiftRage",
    "NotLikeThis",
    "FailFish",
    "VoHiYo",
    "PJSalt",
    "MrDestructoid",
    "bday",
    "RIPCheer",
    "Shamrock",
    "BitBoss",
    "Streamlabs",
    "Muxy",
    "HolidayCheer",
    "Goal",
    "Anon",
    "Charity",
];

/// A Twitch emote occurrence from the IRC `emotes` tag.
///
/// `start` and `end` are **inclusive Unicode code point indices** into the message
/// text (exactly as in the tag, e.g. `25:0-4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmoteRange {
    pub emote: Arc<Emote>,
    pub start: usize,
    pub end: usize,
}

/// One `id:start-end` entry of an `emotes` tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEmoteRange {
    pub id: String,
    pub start: usize,
    pub end: usize,
}

/// Parses an IRC `emotes` tag value (`25:0-4,12-16/1902:6-10`). Malformed entries
/// are skipped; an empty tag yields nothing.
pub fn parse_emotes_tag(tag: &str) -> Vec<RawEmoteRange> {
    let mut out = Vec::new();
    for entry in tag.split('/') {
        let Some((id, ranges)) = entry.split_once(':') else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        for range in ranges.split(',') {
            let Some((start, end)) = range.split_once('-') else {
                continue;
            };
            if let (Ok(start), Ok(end)) = (start.parse(), end.parse()) {
                out.push(RawEmoteRange {
                    id: id.to_owned(),
                    start,
                    end,
                });
            }
        }
    }
    out
}

/// Byte offset of each code point of `text`, plus `text.len()` at the end.
fn char_offsets(text: &str) -> Vec<usize> {
    let mut offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    offsets.push(text.len());
    offsets
}

/// Parses the `emotes` tag of a message and builds Twitch [`Emote`]s from it, naming
/// each one by the text it covers. Ranges that do not fit `text` are dropped; emotes
/// with the same id share one `Arc`.
pub fn twitch_emote_ranges(tag: &str, text: &str) -> Vec<EmoteRange> {
    let raw = parse_emotes_tag(tag);
    if raw.is_empty() {
        return Vec::new();
    }
    let offsets = char_offsets(text);
    let chars = offsets.len() - 1;
    let mut cache: HashMap<String, Arc<Emote>> = HashMap::new();
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        if r.start > r.end || r.end >= chars {
            continue;
        }
        let name = &text[offsets[r.start]..offsets[r.end + 1]];
        let emote = cache
            .entry(r.id.clone())
            .or_insert_with(|| Arc::new(Emote::twitch(r.id.as_str(), name)))
            .clone();
        out.push(EmoteRange {
            emote,
            start: r.start,
            end: r.end,
        });
    }
    out
}

/// Result of tokenizing one message body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tokenized {
    pub fragments: Vec<Fragment>,
    /// The text mentions the configured own login, as `@login` or as a plain word.
    /// Feed this into `MessageFlags::mention`.
    pub mentions_self: bool,
}

/// Configurable tokenizer; cheap to build per message.
#[derive(Clone, Copy)]
pub struct Tokenizer<'a> {
    own_login: Option<&'a str>,
    resolver: &'a dyn WordResolver,
    cheermote_prefixes: &'a [&'a str],
}

impl Default for Tokenizer<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Tokenizer<'a> {
    /// No own login, no resolver, no cheermotes.
    pub fn new() -> Self {
        Self {
            own_login: None,
            resolver: &NoResolver,
            cheermote_prefixes: &[],
        }
    }

    /// The local user's login, for `mentions_self` / [`Fragment::Mention::is_self`]
    /// (compared case-insensitively).
    #[must_use]
    pub fn with_own_login(mut self, login: &'a str) -> Self {
        self.own_login = Some(login);
        self
    }

    /// Third-party emote lookup (BTTV / FFZ / 7TV).
    #[must_use]
    pub fn with_resolver(mut self, resolver: &'a dyn WordResolver) -> Self {
        self.resolver = resolver;
        self
    }

    /// Cheermote prefixes to recognise (`Cheer` matches `Cheer100`, case-insensitive).
    /// Only enable for messages that carry bits.
    #[must_use]
    pub fn with_cheermote_prefixes(mut self, prefixes: &'a [&'a str]) -> Self {
        self.cheermote_prefixes = prefixes;
        self
    }

    /// Tokenizes `text`. `twitch_ranges` come from the IRC `emotes` tag (see
    /// [`twitch_emote_ranges`]); pass `&[]` when there are none. Never panics, whatever
    /// the ranges contain.
    ///
    /// U+E0000 (appended by some clients to bypass duplicate-message checks) is removed
    /// first; `twitch_ranges` must be relative to the *original* text, they are
    /// re-based here.
    pub fn tokenize(&self, text: &str, twitch_ranges: &[EmoteRange]) -> Tokenized {
        if text.contains(IGNORED_CHAR) {
            let (clean, ranges) = strip_ignored(text, twitch_ranges);
            return self.tokenize_clean(&clean, &ranges);
        }
        self.tokenize_clean(text, twitch_ranges)
    }

    fn tokenize_clean(&self, text: &str, twitch_ranges: &[EmoteRange]) -> Tokenized {
        let mut builder = Builder::default();
        let ranges = normalise_ranges(text, twitch_ranges);
        let mut pos = 0;
        for (start, end, emote) in ranges {
            self.words(&mut builder, &text[pos..start]);
            builder.push_emote(emote.clone());
            pos = end;
        }
        self.words(&mut builder, &text[pos..]);
        builder.finish()
    }

    /// Tokenizes a stretch of text containing no pre-resolved emotes.
    fn words(&self, b: &mut Builder, text: &str) {
        let mut word_start: Option<usize> = None;
        let mut ws_start = 0;
        for (i, c) in text.char_indices() {
            if c.is_whitespace() {
                if let Some(start) = word_start.take() {
                    self.word(b, &text[start..i]);
                    ws_start = i;
                }
            } else if word_start.is_none() {
                if ws_start < i {
                    b.push_text(&text[ws_start..i]);
                }
                word_start = Some(i);
            }
        }
        match word_start {
            Some(start) => self.word(b, &text[start..]),
            None => b.push_text(&text[ws_start..]),
        }
    }

    fn word(&self, b: &mut Builder, word: &str) {
        if let Some(emote) = self.resolver.resolve(word) {
            b.push_emote(emote);
            return;
        }
        if let Some(cheer) = self.cheer(word) {
            b.push(cheer);
            return;
        }
        if word.is_ascii() {
            self.segment(b, word);
            return;
        }
        let mut seg_start = 0;
        let mut pos = 0;
        while pos < word.len() {
            if let Some(len) = emoji::emoji_len(&word[pos..]) {
                self.segment(b, &word[seg_start..pos]);
                b.push(Fragment::Emoji {
                    text: word[pos..pos + len].to_owned(),
                });
                pos += len;
                seg_start = pos;
            } else {
                pos += word[pos..].chars().next().map_or(1, char::len_utf8);
            }
        }
        self.segment(b, &word[seg_start..]);
    }

    fn cheer(&self, word: &str) -> Option<Fragment> {
        if self.cheermote_prefixes.is_empty() {
            return None;
        }
        // The amount is the trailing run of digits; prefixes may themselves contain
        // digits (`4Head100` is prefix `4Head`, 100 bits), so try each split of the run.
        let run_start = word
            .rfind(|c: char| !c.is_ascii_digit())
            .map(|i| i + word[i..].chars().next().map_or(1, char::len_utf8))?;
        (run_start..word.len())
            .filter(|&i| i > 0 && !word[i..].starts_with('0'))
            .find_map(|i| {
                let (prefix, digits) = word.split_at(i);
                let bits: u32 = digits.parse().ok()?;
                self.cheermote_prefixes
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(prefix))
                    .then(|| Fragment::Cheer {
                        prefix: prefix.to_owned(),
                        bits,
                    })
            })
    }

    /// A piece of a word that contains no emoji: look for a mention or link between
    /// the surrounding punctuation.
    fn segment(&self, b: &mut Builder, seg: &str) {
        if seg.is_empty() {
            return;
        }
        let (lead, core, trail) = trim_punctuation(seg);
        if core.is_empty() {
            b.push_text(seg);
            return;
        }
        if let Some(rest) = core.strip_prefix('@') {
            let login_len = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let login = &rest[..login_len];
            if !login.is_empty() && login.chars().count() <= 25 {
                let is_self = self.is_own(login);
                b.mentions_self |= is_self;
                b.push_text(lead);
                b.push(Fragment::Mention {
                    text: core[..1 + login_len].to_owned(),
                    login: login.to_lowercase(),
                    is_self,
                });
                b.push_text(&core[1 + login_len..]);
                b.push_text(trail);
                return;
            }
        } else if let Some(url) = links::link_url(core) {
            b.push_text(lead);
            b.push(Fragment::Link {
                text: core.to_owned(),
                url,
            });
            b.push_text(trail);
            return;
        }
        if self.is_own(core) {
            b.mentions_self = true;
        }
        b.push_text(seg);
    }

    fn is_own(&self, login: &str) -> bool {
        self.own_login
            .is_some_and(|own| !own.is_empty() && own.eq_ignore_ascii_case(login))
    }
}

/// Invisible character some clients append to dodge duplicate-message detection.
const IGNORED_CHAR: char = '\u{E0000}';

/// Removes [`IGNORED_CHAR`] from `text` and re-bases the code-point `ranges` onto the
/// result. A range whose last character was removed shrinks; one consisting only of
/// removed characters becomes inverted and is dropped later.
fn strip_ignored(text: &str, ranges: &[EmoteRange]) -> (String, Vec<EmoteRange>) {
    let removed: Vec<usize> = text
        .chars()
        .enumerate()
        .filter_map(|(i, c)| (c == IGNORED_CHAR).then_some(i))
        .collect();
    let clean: String = text.chars().filter(|&c| c != IGNORED_CHAR).collect();
    let ranges = ranges
        .iter()
        .map(|r| EmoteRange {
            emote: r.emote.clone(),
            start: r
                .start
                .saturating_sub(removed.partition_point(|&i| i < r.start)),
            end: r
                .end
                .saturating_sub(removed.partition_point(|&i| i <= r.end)),
        })
        .collect();
    (clean, ranges)
}

/// Validates ranges against `text` and returns `(byte_start, byte_end_exclusive, emote)`
/// sorted and non-overlapping. Invalid (inverted or past the end) and overlapping
/// ranges are dropped; of two overlapping ranges the earlier-starting one wins.
fn normalise_ranges<'r>(
    text: &str,
    ranges: &'r [EmoteRange],
) -> Vec<(usize, usize, &'r Arc<Emote>)> {
    if ranges.is_empty() {
        return Vec::new();
    }
    let offsets = char_offsets(text);
    let chars = offsets.len() - 1;
    let mut valid: Vec<&EmoteRange> = ranges
        .iter()
        .filter(|r| r.start <= r.end && r.end < chars)
        .collect();
    // Longest first on equal starts, so a duplicate shorter range is the one dropped.
    valid.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
    let mut out = Vec::with_capacity(valid.len());
    let mut next_free = 0; // first code point index not yet covered
    for r in valid {
        if r.start < next_free {
            continue;
        }
        out.push((offsets[r.start], offsets[r.end + 1], &r.emote));
        next_free = r.end + 1;
    }
    out
}

/// Splits `s` into (leading punctuation, core, trailing punctuation).
fn trim_punctuation(s: &str) -> (&str, &str, &str) {
    let rest = s.trim_start_matches(['(', '[', '{', '<', '"', '\'']);
    let lead_len = s.len() - rest.len();
    let mut end = rest.len();
    while let Some(c) = rest[..end].chars().next_back() {
        let strip = match c {
            '.' | ',' | '!' | '?' | ';' | ':' | '\'' | '"' | '*' => true,
            // A closing bracket belongs to the core only if it has an opener there.
            ')' => unbalanced(&rest[..end], '(', ')'),
            ']' => unbalanced(&rest[..end], '[', ']'),
            '}' => unbalanced(&rest[..end], '{', '}'),
            '>' => unbalanced(&rest[..end], '<', '>'),
            _ => false,
        };
        if !strip {
            break;
        }
        end -= c.len_utf8();
    }
    (&s[..lead_len], &rest[..end], &rest[end..])
}

fn unbalanced(s: &str, open: char, close: char) -> bool {
    s.matches(close).count() > s.matches(open).count()
}

#[derive(Default)]
struct Builder {
    fragments: Vec<Fragment>,
    text: String,
    mentions_self: bool,
}

impl Builder {
    fn push_text(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn flush_text(&mut self) {
        if !self.text.is_empty() {
            self.fragments
                .push(Fragment::Text(std::mem::take(&mut self.text)));
        }
    }

    fn push(&mut self, fragment: Fragment) {
        self.flush_text();
        self.fragments.push(fragment);
    }

    fn push_emote(&mut self, emote: Arc<Emote>) {
        if emote.zero_width
            && self.text.chars().all(char::is_whitespace)
            && let Some(Fragment::Emote {
                zero_width_overlays,
                ..
            }) = self.fragments.last_mut()
        {
            zero_width_overlays.push(emote);
            self.text.clear();
            return;
        }
        self.push(Fragment::Emote {
            emote,
            zero_width_overlays: Vec::new(),
        });
    }

    fn finish(mut self) -> Tokenized {
        self.flush_text();
        Tokenized {
            fragments: self.fragments,
            mentions_self: self.mentions_self,
        }
    }
}

#[cfg(test)]
mod tests;
