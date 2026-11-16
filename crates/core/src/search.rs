//! In-channel search (PR 5.7): finding text in a channel's messages without rescanning them.
//!
//! * [`Needle`] / [`find_ranges`] are the matching: case-insensitive (Unicode lowercasing, so
//!   `É` finds `é`), and they report byte ranges into the *original* text, which is what the chat
//!   view needs to highlight a match in a row.
//! * [`search_text`] is what a message offers to be searched: its author's display name and login,
//!   the system line of an event and the text of the body (emotes by their code, links as typed).
//! * [`SearchIndex`] keeps that text, already lowercased, for every message of a [`MessageStore`],
//!   updated incrementally (new messages appended, evicted ones dropped, rebuilt only after a
//!   history merge renumbered the store).
//! * [`Search`] holds the matches of one query over the index and keeps them current *without
//!   rescanning the scrollback*: a query that extends the last one (typing) only looks at the
//!   messages that matched before, going back to a shorter query (backspace) restores its saved
//!   matches and only scans what arrived since, and a new message is checked once, when it arrives.
//!   A scan of the whole index happens only when the query changes to something unrelated (the
//!   first character, a paste) or when what may be shown changes (a filter).
//! * [`parse_query`] takes the `from:login` token out of what was typed.

use std::collections::VecDeque;
use std::ops::Range;

use crate::message::Message;
use crate::store::MessageStore;

/// The longest query that is searched for (characters).
pub const MAX_QUERY_CHARS: usize = 200;

/// A query ready to match: trimmed and lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needle {
    folded: String,
}

impl Needle {
    /// `None` for a query with nothing to look for (empty, or only spaces).
    pub fn new(query: &str) -> Option<Self> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
        Some(Self {
            folded: fold(&query),
        })
    }

    /// The lowercased query.
    pub fn as_str(&self) -> &str {
        &self.folded
    }

    /// Whether `folded` text (already lowercased) contains the query.
    pub fn is_in(&self, folded: &str) -> bool {
        folded.contains(self.folded.as_str())
    }
}

/// `text` in lowercase. ASCII text keeps every byte offset.
pub fn fold(text: &str) -> String {
    if text.is_ascii() {
        text.to_ascii_lowercase()
    } else {
        text.to_lowercase()
    }
}

/// One character of a folded text and where it came from.
struct FoldedChar {
    folded_start: usize,
    orig: Range<usize>,
}

/// The byte ranges of `text` that `needle` matches, left to right, not overlapping. The ranges are
/// into `text` itself even when lowercasing changed the length of some character.
pub fn find_ranges(text: &str, needle: &Needle) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    if text.is_ascii() {
        let folded = text.to_ascii_lowercase();
        let mut from = 0;
        while let Some(at) = folded[from..].find(needle.as_str()) {
            let start = from + at;
            let end = start + needle.as_str().len();
            found.push(start..end);
            from = end;
        }
        return found;
    }
    let mut folded = String::with_capacity(text.len());
    let mut chars = Vec::with_capacity(text.len());
    for (start, c) in text.char_indices() {
        chars.push(FoldedChar {
            folded_start: folded.len(),
            orig: start..start + c.len_utf8(),
        });
        folded.extend(c.to_lowercase());
    }
    let mut from = 0;
    while let Some(at) = folded[from..].find(needle.as_str()) {
        let start = from + at;
        let end = start + needle.as_str().len();
        from = end;
        // The character the match starts in, and the one its last byte is in.
        let first = chars
            .partition_point(|c| c.folded_start <= start)
            .saturating_sub(1);
        let last = chars
            .partition_point(|c| c.folded_start < end)
            .saturating_sub(1);
        found.push(chars[first].orig.start..chars[last].orig.end);
    }
    found
}

/// What a message offers to be searched: author (display name and login), the system line of an
/// event, then the body, lowercased and separated by newlines (a one-line query never matches
/// across them).
pub fn search_text(message: &Message) -> String {
    let mut out = String::new();
    if let Some(author) = &message.author {
        out.push_str(&author.display_name);
        if !author.login.eq_ignore_ascii_case(&author.display_name) {
            out.push(' ');
            out.push_str(&author.login);
        }
        out.push('\n');
    }
    if let Some(system) = &message.system_text {
        out.push_str(system);
        out.push('\n');
    }
    for fragment in &message.fragments {
        fragment.write_plain_text(&mut out);
    }
    fold(&out)
}

/// What the find bar's text says: the words to look for and, when it had one, the user of a
/// `from:login` token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// The text to search for, with the `from:` token taken out.
    pub text: String,
    /// The login the `from:` token named (lowercase, no `@`). The last token wins.
    pub from: Option<String>,
}

/// Splits `input` into the search text and a `from:login` filter. A token is `from:` (any case)
/// followed by the login; `from:` alone, or with something that cannot be a login, stays text.
pub fn parse_query(input: &str) -> Query {
    let mut from = None;
    let mut rest: Vec<&str> = Vec::new();
    for word in input.split_whitespace() {
        let login = word
            .get(..5)
            .filter(|prefix| prefix.eq_ignore_ascii_case("from:"))
            .map(|_| word[5..].trim_start_matches('@').to_lowercase())
            .filter(|login| {
                !login.is_empty()
                    && login.len() <= 25
                    && login
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
        match login {
            Some(login) => from = Some(login),
            None => rest.push(word),
        }
    }
    Query {
        text: rest.join(" "),
        from,
    }
}

/// What [`SearchIndex::sync`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexChange {
    /// Nothing changed.
    None,
    /// Evicted messages dropped and/or new ones added at the ends.
    Edited,
    /// Every sequence number moved or the store was cleared: all of it was indexed again.
    Rebuilt,
}

/// The searchable text of every message of one store, in step with it.
#[derive(Debug, Default)]
pub struct SearchIndex {
    first_seq: u64,
    docs: VecDeque<Box<str>>,
    serial: u64,
    ready: bool,
}

impl SearchIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sequence number after the last indexed message.
    pub fn next_seq(&self) -> u64 {
        self.first_seq + self.docs.len() as u64
    }

    pub fn first_seq(&self) -> u64 {
        self.first_seq
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// The folded text of message `seq`.
    pub fn get(&self, seq: u64) -> Option<&str> {
        let at = usize::try_from(seq.checked_sub(self.first_seq)?).ok()?;
        self.docs.get(at).map(|doc| &**doc)
    }

    /// Reads the text of messages `seqs` again.
    pub fn refresh(&mut self, store: &MessageStore, seqs: &[u64]) {
        for &seq in seqs {
            if let Some(at) = seq.checked_sub(self.first_seq).map(|at| at as usize)
                && let Some(doc) = self.docs.get_mut(at)
                && let Some(message) = store.get_by_seq(seq)
            {
                *doc = search_text(message).into_boxed_str();
            }
        }
    }

    /// Brings the index up to date with `store`. Cost: the messages that arrived since the last
    /// call (the whole store the first time, and after a history merge renumbered it).
    pub fn sync(&mut self, store: &MessageStore) -> IndexChange {
        let (first, next) = (store.first_seq(), store.next_seq());
        let renumbered = store.rebuild_serial() != self.serial;
        if !self.ready || renumbered || first < self.first_seq || next < self.next_seq() {
            self.serial = store.rebuild_serial();
            self.ready = true;
            self.first_seq = first;
            self.docs.clear();
            self.docs
                .extend(store.iter().map(|m| search_text(m).into_boxed_str()));
            return IndexChange::Rebuilt;
        }
        let mut changed = false;
        while self.first_seq < first && !self.docs.is_empty() {
            self.docs.pop_front();
            self.first_seq += 1;
            changed = true;
        }
        if self.docs.is_empty() {
            self.first_seq = first.max(self.first_seq);
        }
        let from = self.next_seq().max(first);
        for (seq, message) in store.iter_since(from) {
            debug_assert_eq!(seq, self.next_seq());
            self.docs.push_back(search_text(message).into_boxed_str());
            changed = true;
        }
        if changed {
            IndexChange::Edited
        } else {
            IndexChange::None
        }
    }
}

/// Matches saved for a shorter query, for the way back when the user deletes characters.
#[derive(Debug)]
struct Stage {
    needle: Needle,
    matches: Vec<u64>,
    /// Messages below this were looked at for `matches`.
    scanned: u64,
}

/// The matches of one query over a store's messages, kept current as messages come and go.
#[derive(Debug, Default)]
pub struct Search {
    index: SearchIndex,
    needle: Option<Needle>,
    matches: Vec<u64>,
    scanned: u64,
    stages: Vec<Stage>,
}

impl Search {
    pub fn new() -> Self {
        Self::default()
    }

    /// The matching sequence numbers, oldest first.
    pub fn matches(&self) -> &[u64] {
        &self.matches
    }

    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// The query in force, if there is something to look for.
    pub fn needle(&self) -> Option<&Needle> {
        self.needle.as_ref()
    }

    /// How many messages are indexed.
    pub fn indexed(&self) -> usize {
        self.index.len()
    }

    /// The position (from 0) of message `seq` among the matches.
    pub fn position_of(&self, seq: u64) -> Option<usize> {
        self.matches.binary_search(&seq).ok()
    }

    /// The first match after `seq` (`forward`) or before it, wrapping round. With no `seq`, the
    /// newest match when going back and the oldest going forward.
    pub fn step(&self, from: Option<u64>, forward: bool) -> Option<u64> {
        let last = self.matches.len().checked_sub(1)?;
        let Some(seq) = from else {
            return Some(self.matches[if forward { 0 } else { last }]);
        };
        let at = self.matches.partition_point(|&m| m <= seq);
        let index = if forward {
            if at > last { 0 } else { at }
        } else {
            let before = self.matches.partition_point(|&m| m < seq);
            before.checked_sub(1).unwrap_or(last)
        };
        Some(self.matches[index])
    }

    /// Takes `store`'s changes into the index and the matches. `allowed` says which messages may
    /// match at all (the ones the chat shows). Returns whether the matches changed.
    pub fn sync(&mut self, store: &MessageStore, allowed: &dyn Fn(u64) -> bool) -> bool {
        match self.index.sync(store) {
            IndexChange::None => false,
            IndexChange::Rebuilt => {
                // Every number moved (or the store was cleared): the saved stages hold the old ones.
                self.stages.clear();
                self.recompute(allowed);
                true
            }
            IndexChange::Edited => {
                let first = self.index.first_seq();
                let evicted = self.matches.partition_point(|&seq| seq < first);
                self.matches.drain(..evicted);
                let before = self.matches.len();
                self.scan_new(allowed);
                evicted > 0 || self.matches.len() != before
            }
        }
    }

    /// The messages `seqs` were replaced in place (an echo swapped for Twitch's copy): their text
    /// is read again and each is checked once more. Returns whether the matches changed.
    pub fn replaced(
        &mut self,
        store: &MessageStore,
        seqs: &[u64],
        allowed: &dyn Fn(u64) -> bool,
    ) -> bool {
        self.index.refresh(store, seqs);
        // The saved stages may hold the old verdicts of these messages.
        self.stages.clear();
        let Some(needle) = &self.needle else {
            return false;
        };
        let before = self.matches.clone();
        for &seq in seqs {
            if let Ok(at) = self.matches.binary_search(&seq) {
                self.matches.remove(at);
            }
            if self.index.get(seq).is_some_and(|doc| needle.is_in(doc)) && allowed(seq) {
                let at = self.matches.partition_point(|&m| m < seq);
                self.matches.insert(at, seq);
            }
        }
        before != self.matches
    }

    /// Forgets the index: the next [`Search::sync`] reads every message again (what to do when too
    /// many messages were replaced to name them).
    pub fn invalidate_all(&mut self) {
        self.index = SearchIndex::new();
        self.stages.clear();
        self.matches.clear();
        self.scanned = 0;
    }

    /// Looks for `query` (what the user typed, `from:` already taken out). Returns whether the
    /// matches changed.
    pub fn set_query(&mut self, query: &str, allowed: &dyn Fn(u64) -> bool) -> bool {
        let needle = Needle::new(query);
        if needle == self.needle {
            return false;
        }
        match (&self.needle, &needle) {
            (_, None) => {
                self.stages.clear();
                self.matches.clear();
                self.scanned = self.index.next_seq();
            }
            // Typing: what matches the longer query matched the shorter one.
            (Some(old), Some(new)) if new.as_str().starts_with(old.as_str()) => {
                let kept: Vec<u64> = self
                    .matches
                    .iter()
                    .copied()
                    .filter(|&seq| self.index.get(seq).is_some_and(|doc| new.is_in(doc)))
                    .collect();
                let previous = std::mem::replace(&mut self.matches, kept);
                if let Some(old) = self.needle.take() {
                    self.stages.push(Stage {
                        needle: old,
                        matches: previous,
                        scanned: self.scanned,
                    });
                }
            }
            // Deleting: an earlier stage may be the very query again.
            (_, Some(new)) if self.stages.iter().any(|stage| stage.needle == *new) => {
                let at = self
                    .stages
                    .iter()
                    .rposition(|stage| stage.needle == *new)
                    .unwrap_or(0);
                self.stages.truncate(at + 1);
                if let Some(stage) = self.stages.pop() {
                    let first = self.index.first_seq();
                    let from = stage.matches.partition_point(|&seq| seq < first);
                    self.matches = stage.matches[from..].to_vec();
                    self.scanned = stage.scanned.max(first);
                }
                self.needle = needle;
                self.scan_new(allowed);
                return true;
            }
            _ => {
                self.stages.clear();
                self.needle = needle;
                self.recompute(allowed);
                return true;
            }
        }
        self.needle = needle;
        true
    }

    /// What may match changed (a filter): look at everything again.
    pub fn refilter(&mut self, allowed: &dyn Fn(u64) -> bool) {
        self.stages.clear();
        self.recompute(allowed);
    }

    fn recompute(&mut self, allowed: &dyn Fn(u64) -> bool) {
        self.matches.clear();
        self.scanned = self.index.first_seq();
        self.scan_new(allowed);
    }

    /// Looks at the messages not looked at yet.
    fn scan_new(&mut self, allowed: &dyn Fn(u64) -> bool) {
        let next = self.index.next_seq();
        if let Some(needle) = &self.needle {
            let from = self.scanned.max(self.index.first_seq());
            for seq in from..next {
                if self.index.get(seq).is_some_and(|doc| needle.is_in(doc)) && allowed(seq) {
                    self.matches.push(seq);
                }
            }
        }
        self.scanned = next;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::emote::Emote;
    use crate::message::{Author, Fragment, Timestamp};

    fn chat(n: u64, login: &str, text: &str) -> Arc<Message> {
        Arc::new(Message::chat(
            format!("m{n}"),
            Timestamp(n as i64),
            Author::new(login, login),
            vec![Fragment::Text(text.into())],
        ))
    }

    fn store(lines: &[(&str, &str)]) -> MessageStore {
        let mut store = MessageStore::new();
        for (n, (login, text)) in lines.iter().enumerate() {
            store.push(chat(n as u64, login, text));
        }
        store
    }

    fn all(_: u64) -> bool {
        true
    }

    fn needle(text: &str) -> Needle {
        Needle::new(text).unwrap()
    }

    #[test]
    fn ranges_are_into_the_original_text_whatever_the_case() {
        let ranges = find_ranges("Hello hello HELLO", &needle("hello"));
        assert_eq!(ranges, [0..5, 6..11, 12..17]);
        assert_eq!(
            find_ranges("aaaa", &needle("aa")),
            [0..2, 2..4],
            "no overlap"
        );
        assert!(find_ranges("nothing", &needle("x")).is_empty());
    }

    #[test]
    fn non_ascii_text_matches_ignoring_case_with_original_offsets() {
        let text = "Été ÉTÉ";
        let ranges = find_ranges(text, &needle("été"));
        assert_eq!(ranges.len(), 2);
        assert_eq!(&text[ranges[0].clone()], "Été");
        assert_eq!(&text[ranges[1].clone()], "ÉTÉ");
        // `İ` lowercases to two characters: the range still covers whole original characters.
        let text = "xİy";
        let ranges = find_ranges(text, &needle("y"));
        assert_eq!(&text[ranges[0].clone()], "y");
        let ranges = find_ranges(text, &needle("i"));
        assert_eq!(&text[ranges[0].clone()], "İ");
    }

    #[test]
    fn an_empty_query_has_no_needle() {
        assert!(Needle::new("").is_none());
        assert!(Needle::new("   ").is_none());
        assert_eq!(Needle::new("  Hi ").unwrap().as_str(), "hi");
    }

    #[test]
    fn the_searchable_text_has_author_system_line_and_body_with_emotes_by_code() {
        let mut message = Message::chat(
            "id",
            Timestamp(0),
            Author::new("alice", "Alice"),
            vec![
                Fragment::Text("hi ".into()),
                Fragment::Emote {
                    emote: Arc::new(Emote::twitch("25", "Kappa")),
                    zero_width_overlays: Vec::new(),
                },
            ],
        );
        message.system_text = Some("Alice subscribed".into());
        let text = search_text(&message);
        assert!(text.contains("alice"));
        assert!(text.contains("alice subscribed"));
        assert!(text.contains("hi kappa"));
        assert!(
            !needle("alice hi").is_in(&text),
            "no match across the author and the body"
        );
    }

    #[test]
    fn a_from_token_is_taken_out_of_the_query() {
        assert_eq!(
            parse_query("from:Alice hello world"),
            Query {
                text: "hello world".into(),
                from: Some("alice".into())
            }
        );
        assert_eq!(parse_query("hello FROM:@bob").from.as_deref(), Some("bob"));
        assert_eq!(parse_query("from:").text, "from:");
        assert_eq!(parse_query("from:not-valid").from, None);
        assert_eq!(parse_query("a from:x b from:y").from.as_deref(), Some("y"));
        assert_eq!(parse_query("  spaced   out  ").text, "spaced out");
        assert_eq!(parse_query(""), Query::default());
    }

    #[test]
    fn the_index_follows_the_store_incrementally() {
        let mut store = MessageStore::with_capacity(3);
        let mut index = SearchIndex::new();
        assert_eq!(index.sync(&store), IndexChange::Rebuilt);
        assert_eq!(index.sync(&store), IndexChange::None);
        store.push(chat(0, "a", "one"));
        store.push(chat(1, "a", "two"));
        assert_eq!(index.sync(&store), IndexChange::Edited);
        assert_eq!(index.len(), 2);
        for n in 2..6 {
            store.push(chat(n, "a", &format!("m{n}")));
        }
        assert_eq!(index.sync(&store), IndexChange::Edited);
        assert_eq!((index.first_seq(), index.next_seq()), (3, 6));
        assert!(index.get(2).is_none());
        assert!(index.get(3).unwrap().contains("m3"));
        // More arrived than the store holds: still exactly its messages.
        for n in 6..20 {
            store.push(chat(n, "a", "x"));
        }
        index.sync(&store);
        assert_eq!((index.first_seq(), index.next_seq()), (17, 20));
        assert_eq!(index.len(), store.len());
    }

    #[test]
    fn history_that_renumbers_the_store_rebuilds_the_index() {
        let mut store = store(&[("a", "later one"), ("a", "later two")]);
        let mut index = SearchIndex::new();
        index.sync(&store);
        let older = Arc::new(Message::chat(
            "old",
            Timestamp(-10),
            Author::new("a", "a"),
            vec![Fragment::Text("earlier".into())],
        ));
        assert_eq!(store.merge_history(vec![older]), 1);
        assert_eq!(index.sync(&store), IndexChange::Rebuilt);
        let texts: Vec<_> = (index.first_seq()..index.next_seq())
            .map(|seq| index.get(seq).unwrap().to_owned())
            .collect();
        assert!(
            texts[0].contains("earlier") && texts[2].contains("later two"),
            "{texts:?}"
        );
    }

    fn matches_of(search: &Search) -> Vec<u64> {
        search.matches().to_vec()
    }

    #[test]
    fn typing_narrows_and_backspacing_goes_back() {
        let store = store(&[
            ("a", "hello world"),
            ("b", "help me"),
            ("c", "nothing"),
            ("d", "hello again"),
        ]);
        let mut search = Search::new();
        search.sync(&store, &all);
        assert!(search.set_query("h", &all));
        assert_eq!(matches_of(&search), [0, 1, 2, 3], "nothing has an h too");
        assert!(search.set_query("he", &all));
        assert_eq!(matches_of(&search), [0, 1, 3]);
        assert!(search.set_query("hel", &all));
        assert_eq!(matches_of(&search), [0, 1, 3]);
        assert!(search.set_query("hell", &all));
        assert_eq!(matches_of(&search), [0, 3]);
        assert!(
            !search.set_query("HELL", &all),
            "the same query again changes nothing"
        );
        assert!(search.set_query("hel", &all));
        assert_eq!(matches_of(&search), [0, 1, 3]);
        assert!(search.set_query("", &all));
        assert!(search.matches().is_empty() && search.needle().is_none());
        // Something unrelated is a fresh scan.
        assert!(search.set_query("again", &all));
        assert_eq!(matches_of(&search), [3]);
    }

    #[test]
    fn matches_cover_the_author_and_new_messages_are_checked_once_on_arrival() {
        let mut store = store(&[("Alice", "hi"), ("bob", "yo")]);
        let mut search = Search::new();
        search.sync(&store, &all);
        search.set_query("alice", &all);
        assert_eq!(matches_of(&search), [0]);
        store.push(chat(2, "alice", "again"));
        store.push(chat(3, "carol", "no"));
        assert!(search.sync(&store, &all));
        assert_eq!(matches_of(&search), [0, 2]);
        assert!(!search.sync(&store, &all), "nothing new, nothing changed");
    }

    #[test]
    fn backspacing_after_messages_arrived_catches_up_with_them() {
        let mut store = store(&[("a", "hello"), ("a", "help")]);
        let mut search = Search::new();
        search.sync(&store, &all);
        search.set_query("hel", &all);
        search.set_query("hell", &all);
        assert_eq!(matches_of(&search), [0]);
        store.push(chat(2, "a", "helmet"));
        store.push(chat(3, "a", "hello again"));
        search.sync(&store, &all);
        assert_eq!(matches_of(&search), [0, 3]);
        search.set_query("hel", &all);
        assert_eq!(
            matches_of(&search),
            [0, 1, 2, 3],
            "the saved matches plus what came since"
        );
    }

    #[test]
    fn evicted_messages_leave_the_matches() {
        let mut store = MessageStore::with_capacity(3);
        let mut search = Search::new();
        for n in 0..3 {
            store.push(chat(n, "a", "hit"));
        }
        search.sync(&store, &all);
        search.set_query("hit", &all);
        assert_eq!(matches_of(&search), [0, 1, 2]);
        store.push(chat(3, "a", "hit"));
        store.push(chat(4, "a", "miss"));
        assert!(search.sync(&store, &all));
        assert_eq!(matches_of(&search), [2, 3]);
    }

    #[test]
    fn only_allowed_messages_match_and_refiltering_looks_again() {
        let store = store(&[("a", "x"), ("b", "x"), ("a", "x")]);
        let mut search = Search::new();
        let only_a = |seq: u64| seq != 1;
        search.sync(&store, &only_a);
        search.set_query("x", &only_a);
        assert_eq!(matches_of(&search), [0, 2]);
        search.refilter(&all);
        assert_eq!(matches_of(&search), [0, 1, 2]);
    }

    #[test]
    fn stepping_wraps_in_both_directions() {
        let store = store(&[("a", "x"), ("a", "y"), ("a", "x"), ("a", "y"), ("a", "x")]);
        let mut search = Search::new();
        search.sync(&store, &all);
        search.set_query("x", &all);
        assert_eq!(matches_of(&search), [0, 2, 4]);
        assert_eq!(
            search.step(None, false),
            Some(4),
            "the newest to start with"
        );
        assert_eq!(search.step(None, true), Some(0));
        assert_eq!(search.step(Some(4), false), Some(2));
        assert_eq!(search.step(Some(2), false), Some(0));
        assert_eq!(search.step(Some(0), false), Some(4), "wraps to the newest");
        assert_eq!(search.step(Some(0), true), Some(2));
        assert_eq!(search.step(Some(4), true), Some(0), "wraps to the oldest");
        // From a message that is not a match: the next one in that direction.
        assert_eq!(search.step(Some(1), true), Some(2));
        assert_eq!(search.step(Some(1), false), Some(0));
        assert_eq!(search.position_of(2), Some(1));
        assert_eq!(search.position_of(1), None);
        let empty = Search::new();
        assert_eq!(empty.step(None, true), None);
    }

    #[test]
    fn a_history_merge_rebuilds_the_matches_with_the_new_numbers() {
        let mut store = store(&[("a", "hit one"), ("a", "miss")]);
        let mut search = Search::new();
        search.sync(&store, &all);
        search.set_query("hit", &all);
        assert_eq!(matches_of(&search), [0]);
        let older = Arc::new(Message::chat(
            "old",
            Timestamp(-10),
            Author::new("a", "a"),
            vec![Fragment::Text("old hit".into())],
        ));
        store.merge_history(vec![older]);
        assert!(search.sync(&store, &all));
        let texts: Vec<_> = search
            .matches()
            .iter()
            .map(|&seq| store.get_by_seq(seq).unwrap().plain_text())
            .collect();
        assert_eq!(texts, ["old hit", "hit one"]);
    }

    /// The point of the structure: at the size of the biggest scrollback a keystroke does not scan
    /// the store. A narrowing keystroke looks at the previous matches only, and a message that
    /// arrives is looked at once.
    #[test]
    fn a_keystroke_in_a_huge_scrollback_looks_only_at_what_matched_before() {
        let mut store = MessageStore::with_capacity(200_000);
        for n in 0..100_000u64 {
            let text = if n % 1000 == 0 {
                "rare needle"
            } else {
                "common chatter"
            };
            store.push(chat(n, "a", text));
        }
        let mut search = Search::new();
        search.sync(&store, &all);
        assert_eq!(search.indexed(), 100_000);
        search.set_query("n", &all);
        assert_eq!(search.count(), 100_000, "every line has an n");
        search.set_query("ne", &all);
        assert_eq!(
            search.count(),
            100,
            "the narrowing starts from the n matches"
        );
        // Count how many messages a further keystroke asks about.
        let looked = std::cell::Cell::new(0usize);
        let counting = |_: u64| {
            looked.set(looked.get() + 1);
            true
        };
        search.set_query("nee", &counting);
        assert_eq!(search.count(), 100);
        assert_eq!(
            looked.get(),
            0,
            "narrowing never consults the filter again for what already passed"
        );
        store.push(chat(100_000, "a", "needle again"));
        search.sync(&store, &counting);
        assert_eq!(search.count(), 101);
        assert_eq!(looked.get(), 1, "the new message is the only one looked at");
    }

    #[test]
    fn a_renumbering_does_not_leave_stale_stages_for_backspace() {
        let mut store = store(&[("a", "abc one"), ("a", "abd two"), ("a", "zzz")]);
        let mut search = Search::new();
        search.sync(&store, &all);
        search.set_query("a", &all);
        search.set_query("ab", &all);
        search.set_query("abc", &all);
        assert_eq!(matches_of(&search), [0]);
        let older = Arc::new(Message::chat(
            "old",
            Timestamp(-10),
            Author::new("a", "a"),
            vec![Fragment::Text("abc earlier".into())],
        ));
        store.merge_history(vec![older]);
        assert!(search.sync(&store, &all));
        assert_eq!(matches_of(&search), [3, 4], "everything got a new number");
        // Backspace: the saved "ab" stage held the old numbers (0, 1); they must not come back.
        search.set_query("ab", &all);
        let texts: Vec<_> = search
            .matches()
            .iter()
            .map(|&seq| store.get_by_seq(seq).unwrap().plain_text())
            .collect();
        assert_eq!(texts, ["abc earlier", "abc one", "abd two"]);
    }

    #[test]
    fn a_replaced_message_is_read_again() {
        let mut store = store(&[("a", "echo text"), ("a", "other")]);
        let mut search = Search::new();
        search.sync(&store, &all);
        search.set_query("kappa", &all);
        assert!(search.matches().is_empty());
        let real = chat(0, "a", "echo text Kappa");
        assert_eq!(store.replace(real), Some(0));
        assert!(search.replaced(&store, &[0], &all));
        assert_eq!(matches_of(&search), [0]);
        // And the other way: the text no longer matches.
        assert_eq!(store.replace(chat(0, "a", "gone")), Some(0));
        assert!(search.replaced(&store, &[0], &all));
        assert!(search.matches().is_empty());
        search.invalidate_all();
        assert!(!search.sync(&store, &all) || search.count() == 0);
    }
}
