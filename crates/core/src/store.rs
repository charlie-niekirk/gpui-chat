//! Per-channel message ring buffer.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::ids::MessageId;
use crate::message::Message;

/// Default number of messages kept per channel.
pub const DEFAULT_CAPACITY: usize = 1_000;

struct Entry {
    seq: u64,
    message: Arc<Message>,
}

/// One thread as held by a [`MessageStore`], see [`MessageStore::thread`].
#[derive(Debug)]
pub struct Thread<'a> {
    /// The message the thread hangs off; `None` once it has scrolled out of the store.
    pub root: Option<&'a Arc<Message>>,
    /// The replies still retained, oldest first.
    pub replies: Vec<&'a Arc<Message>>,
}

/// A bounded, ordered buffer of the most recent messages of one channel.
///
/// * `push` and eviction are O(1); lookup by id is O(1) (hash map + the fact that
///   sequence numbers are contiguous, so `seq - first_seq` is the deque index).
/// * Every pushed message gets a **sequence number**, increasing by one and never
///   reused. The UI can remember the last `seq` it rendered and ask for what is newer
///   ([`MessageStore::iter_since`]); [`MessageStore::first_seq`] tells it how much has
///   scrolled out.
/// * [`MessageStore::generation`] increases on *every* change (push, eviction, delete,
///   resize, clear), so "did anything change?" is one integer comparison.
/// * Duplicate ids are allowed (nothing is rejected); the id index points at the newest
///   one. Use [`MessageStore::contains`] first to de-duplicate (e.g. history overlap).
pub struct MessageStore {
    entries: VecDeque<Entry>,
    by_id: HashMap<MessageId, u64>,
    capacity: usize,
    next_seq: u64,
    generation: u64,
}

impl Default for MessageStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageStore {
    /// A store with [`DEFAULT_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A store holding at most `capacity` messages (minimum 1).
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: VecDeque::with_capacity(capacity.min(DEFAULT_CAPACITY)),
            by_id: HashMap::new(),
            capacity,
            next_seq: 0,
            generation: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Increases on every change of the store's contents or capacity.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Sequence number the next pushed message will get.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Sequence number of the oldest retained message (equals [`next_seq`] when empty).
    ///
    /// [`next_seq`]: MessageStore::next_seq
    pub fn first_seq(&self) -> u64 {
        self.entries.front().map_or(self.next_seq, |e| e.seq)
    }

    /// Appends `message`, evicting the oldest one if the store is full. Returns the
    /// evicted message, if any.
    pub fn push(&mut self, message: Arc<Message>) -> Option<Arc<Message>> {
        let evicted = if self.entries.len() >= self.capacity {
            self.evict_front()
        } else {
            None
        };
        let seq = self.next_seq;
        self.next_seq += 1;
        self.by_id.insert(message.id.clone(), seq);
        self.entries.push_back(Entry { seq, message });
        self.generation += 1;
        evicted
    }

    /// Merges recent-messages `history` (PR 5.5) into the store by time and returns how many
    /// messages were added.
    ///
    /// * A history message whose id is already retained is skipped (the live copy wins), as is a
    ///   repeat of an id within `history`, so overlap between a fetch and what was already seen
    ///   (a reconnect refetching part of it, a message that arrived live meanwhile) shows nothing
    ///   twice.
    /// * The new messages take their place by timestamp among the retained ones, each side keeping
    ///   its own order; on equal timestamps history goes first. Messages newer than everything
    ///   retained are simply pushed.
    /// * Placing a message *before* a retained one (the usual case on join: live messages arrived
    ///   while the fetch ran) rebuilds the buffer, which gives every retained message a **new
    ///   sequence number**: views see all of it replaced, never a gap or a changed meaning of an
    ///   old number. When the result exceeds the capacity the oldest messages go, as with `push`.
    pub fn merge_history(&mut self, history: Vec<Arc<Message>>) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut fresh: Vec<Arc<Message>> = history
            .into_iter()
            .filter(|m| !self.by_id.contains_key(&m.id) && seen.insert(m.id.clone()))
            .collect();
        if fresh.is_empty() {
            return 0;
        }
        fresh.sort_by_key(|m| m.timestamp);
        let added = fresh.len();
        let newest = self.entries.back().map(|e| e.message.timestamp);
        if newest.is_none_or(|newest| fresh[0].timestamp >= newest) {
            for message in fresh {
                self.push(message);
            }
            return added;
        }
        let retained: Vec<Arc<Message>> = self.entries.drain(..).map(|e| e.message).collect();
        self.by_id.clear();
        let (mut old, mut new) = (
            retained.into_iter().peekable(),
            fresh.into_iter().peekable(),
        );
        loop {
            let take_new = match (old.peek(), new.peek()) {
                (Some(o), Some(n)) => n.timestamp <= o.timestamp,
                (None, Some(_)) => true,
                (Some(_), None) => false,
                (None, None) => break,
            };
            let next = if take_new { new.next() } else { old.next() };
            if let Some(message) = next {
                self.push(message);
            }
        }
        added
    }

    /// Changes the capacity (minimum 1). Shrinking evicts the oldest messages;
    /// returns how many were evicted.
    pub fn set_capacity(&mut self, capacity: usize) -> usize {
        let capacity = capacity.max(1);
        if capacity == self.capacity {
            return 0;
        }
        self.capacity = capacity;
        let mut evicted = 0;
        while self.entries.len() > capacity {
            self.evict_front();
            evicted += 1;
        }
        if self.entries.capacity() > capacity.max(DEFAULT_CAPACITY) * 2 {
            self.entries.shrink_to(capacity);
        }
        self.generation += 1;
        evicted
    }

    /// Removes every message. Sequence numbers keep counting.
    pub fn clear(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.entries.clear();
        self.by_id.clear();
        self.generation += 1;
    }

    fn evict_front(&mut self) -> Option<Arc<Message>> {
        let entry = self.entries.pop_front()?;
        // A newer message may have reused the id; only drop the index entry if it
        // still points at the evicted one.
        if self.by_id.get(entry.message.id.as_str()) == Some(&entry.seq) {
            self.by_id.remove(entry.message.id.as_str());
        }
        Some(entry.message)
    }

    fn index_of_seq(&self, seq: u64) -> Option<usize> {
        let index = usize::try_from(seq.checked_sub(self.first_seq())?).ok()?;
        (index < self.entries.len()).then_some(index)
    }

    /// Whether a message with this id is retained.
    pub fn contains(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    /// Looks a message up by id.
    pub fn get(&self, id: &str) -> Option<&Arc<Message>> {
        let seq = *self.by_id.get(id)?;
        self.get_by_seq(seq)
    }

    /// The sequence number of the message with this id.
    pub fn seq_of(&self, id: &str) -> Option<u64> {
        self.by_id.get(id).copied()
    }

    /// Looks a message up by sequence number (`None` once evicted or if not yet pushed).
    pub fn get_by_seq(&self, seq: u64) -> Option<&Arc<Message>> {
        let index = self.index_of_seq(seq)?;
        self.entries.get(index).map(|e| &e.message)
    }

    /// The `index`-th retained message, oldest first.
    pub fn get_index(&self, index: usize) -> Option<&Arc<Message>> {
        self.entries.get(index).map(|e| &e.message)
    }

    /// Messages oldest to newest.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Arc<Message>> + ExactSizeIterator {
        self.entries.iter().map(|e| &e.message)
    }

    /// Messages oldest to newest with their sequence numbers.
    pub fn iter_with_seq(
        &self,
    ) -> impl DoubleEndedIterator<Item = (u64, &Arc<Message>)> + ExactSizeIterator {
        self.entries.iter().map(|e| (e.seq, &e.message))
    }

    /// Messages with `seq >= since`, oldest to newest.
    pub fn iter_since(
        &self,
        since: u64,
    ) -> impl DoubleEndedIterator<Item = (u64, &Arc<Message>)> + ExactSizeIterator {
        let start = since.saturating_sub(self.first_seq());
        let start = usize::try_from(start)
            .unwrap_or(usize::MAX)
            .min(self.entries.len());
        self.entries.range(start..).map(|e| (e.seq, &e.message))
    }

    /// The thread rooted at `root_id`: the root message if it is still retained, then every
    /// retained reply to it (a reply names its root in `reply-thread-parent-msg-id`, or its parent
    /// when it has none), oldest first. Scans the buffer, so call it for a thread being looked at,
    /// not for every message.
    pub fn thread(&self, root_id: &str) -> Thread<'_> {
        let root = self.get(root_id);
        let replies = self
            .entries
            .iter()
            .map(|entry| &entry.message)
            .filter(|message| {
                message.id.as_str() != root_id
                    && message
                        .reply
                        .as_ref()
                        .is_some_and(|reply| reply.root_id().as_str() == root_id)
            })
            .collect();
        Thread { root, replies }
    }

    /// Replaces the message with the same id as `message` by `message`, keeping its place (and
    /// sequence number) in the store. Returns that sequence number, or `None` if no retained
    /// message has the id. Used to swap a local echo for the real message.
    pub fn replace(&mut self, message: Arc<Message>) -> Option<u64> {
        let seq = self.by_id.get(message.id.as_str()).copied()?;
        let index = self.index_of_seq(seq)?;
        self.entries[index].message = message;
        self.generation += 1;
        Some(seq)
    }

    /// Marks the message with this id as deleted (replacing it with a copy that has
    /// `flags.deleted` set). Returns `false` if the id is unknown or already deleted.
    pub fn mark_deleted(&mut self, id: &str) -> bool {
        match self.by_id.get(id).copied() {
            Some(seq) => self.mark_deleted_seq(seq),
            None => false,
        }
    }

    /// Marks every retained message by `login` as deleted; returns how many changed.
    pub fn mark_user_deleted(&mut self, login: &str) -> usize {
        self.mark_where(|m| {
            m.author
                .as_ref()
                .is_some_and(|a| a.login.eq_ignore_ascii_case(login))
        })
    }

    /// Marks every retained message as deleted; returns how many changed.
    pub fn mark_all_deleted(&mut self) -> usize {
        self.mark_where(|_| true)
    }

    fn mark_where(&mut self, mut pred: impl FnMut(&Message) -> bool) -> usize {
        let mut changed = 0;
        for entry in &mut self.entries {
            if !entry.message.flags.deleted && pred(&entry.message) {
                entry.message = Arc::new(entry.message.with_deleted());
                changed += 1;
            }
        }
        if changed > 0 {
            self.generation += 1;
        }
        changed
    }

    fn mark_deleted_seq(&mut self, seq: u64) -> bool {
        let Some(index) = self.index_of_seq(seq) else {
            return false;
        };
        let entry = &mut self.entries[index];
        if entry.message.flags.deleted {
            return false;
        }
        entry.message = Arc::new(entry.message.with_deleted());
        self.generation += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Author, Fragment, MessageKind, Timestamp};

    fn msg(id: &str) -> Arc<Message> {
        msg_by(id, "someone")
    }

    fn msg_by(id: &str, login: &str) -> Arc<Message> {
        Arc::new(Message::chat(
            id,
            Timestamp(0),
            Author::new(login, login),
            vec![Fragment::Text(id.to_owned())],
        ))
    }

    fn ids(store: &MessageStore) -> Vec<String> {
        store.iter().map(|m| m.id.to_string()).collect()
    }

    fn reply(id: &str, parent: &str, root: Option<&str>) -> Arc<Message> {
        let mut m = Message::chat(
            id,
            Timestamp(0),
            Author::new("replier", "replier"),
            vec![Fragment::Text(id.to_owned())],
        );
        m.reply = Some(crate::message::ReplyRef {
            parent_id: parent.into(),
            parent_login: "p".into(),
            parent_display_name: "P".into(),
            parent_body: "b".into(),
            thread_parent_id: root.map(Into::into),
        });
        Arc::new(m)
    }

    #[test]
    fn a_thread_is_its_root_and_the_replies_that_name_it() {
        let mut store = MessageStore::with_capacity(10);
        store.push(msg("root"));
        store.push(msg("other"));
        store.push(reply("r1", "root", Some("root")));
        store.push(reply("x1", "other", Some("other")));
        // A reply to a reply carries the same root; one without a thread tag falls back to its parent.
        store.push(reply("r2", "r1", Some("root")));
        store.push(reply("r3", "root", None));
        let thread = store.thread("root");
        assert_eq!(thread.root.unwrap().id.as_str(), "root");
        let ids: Vec<_> = thread.replies.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["r1", "r2", "r3"]);
        assert!(store.thread("none").replies.is_empty());
        assert!(store.thread("none").root.is_none());
    }

    #[test]
    fn a_thread_outlives_its_root_leaving_the_store() {
        let mut store = MessageStore::with_capacity(2);
        store.push(msg("root"));
        store.push(reply("r1", "root", Some("root")));
        store.push(reply("r2", "root", Some("root")));
        let thread = store.thread("root");
        assert!(thread.root.is_none(), "the root was evicted");
        assert_eq!(thread.replies.len(), 2);
    }

    #[test]
    fn push_keeps_order_and_evicts_oldest() {
        let mut store = MessageStore::with_capacity(3);
        for i in 0..3 {
            assert!(store.push(msg(&i.to_string())).is_none());
        }
        assert_eq!(ids(&store), ["0", "1", "2"]);
        let evicted = store.push(msg("3")).unwrap();
        assert_eq!(evicted.id.as_str(), "0");
        assert_eq!(ids(&store), ["1", "2", "3"]);
        assert_eq!(store.len(), 3);
        assert!(!store.contains("0"));
        assert!(store.get("0").is_none());
        assert_eq!(store.get("2").unwrap().id.as_str(), "2");
    }

    #[test]
    fn sequence_numbers_are_monotonic_and_survive_eviction() {
        let mut store = MessageStore::with_capacity(2);
        assert_eq!((store.first_seq(), store.next_seq()), (0, 0));
        for i in 0..5 {
            store.push(msg(&i.to_string()));
        }
        assert_eq!((store.first_seq(), store.next_seq()), (3, 5));
        assert_eq!(store.seq_of("4"), Some(4));
        assert_eq!(store.seq_of("1"), None);
        assert_eq!(store.get_by_seq(3).unwrap().id.as_str(), "3");
        assert!(store.get_by_seq(2).is_none());
        assert!(store.get_by_seq(5).is_none());
        let since: Vec<u64> = store.iter_since(4).map(|(s, _)| s).collect();
        assert_eq!(since, [4]);
        // Asking for something older than retained yields everything retained.
        let all: Vec<u64> = store.iter_since(0).map(|(s, _)| s).collect();
        assert_eq!(all, [3, 4]);
        assert_eq!(store.iter_since(99).count(), 0);
    }

    #[test]
    fn generation_changes_on_every_mutation() {
        let mut store = MessageStore::with_capacity(2);
        let mut last = store.generation();
        let mut bumped = |store: &MessageStore| {
            let now = store.generation();
            let changed = now > last;
            last = now;
            changed
        };
        store.push(msg("a"));
        assert!(bumped(&store));
        assert!(store.mark_deleted("a"));
        assert!(bumped(&store));
        // No-ops do not bump.
        assert!(!store.mark_deleted("a"));
        assert!(!store.mark_deleted("missing"));
        assert_eq!(store.mark_user_deleted("nobody"), 0);
        assert!(!bumped(&store));
        store.set_capacity(5);
        assert!(bumped(&store));
        store.set_capacity(5);
        assert!(!bumped(&store));
        store.clear();
        assert!(bumped(&store));
        store.clear();
        assert!(!bumped(&store));
    }

    #[test]
    fn resize_shrinks_from_the_front_and_grows() {
        let mut store = MessageStore::with_capacity(5);
        for i in 0..5 {
            store.push(msg(&i.to_string()));
        }
        assert_eq!(store.set_capacity(2), 3);
        assert_eq!(ids(&store), ["3", "4"]);
        assert_eq!(store.capacity(), 2);
        assert!(!store.contains("0"));
        assert_eq!(store.first_seq(), 3);
        assert_eq!(store.set_capacity(4), 0);
        store.push(msg("5"));
        store.push(msg("6"));
        assert_eq!(ids(&store), ["3", "4", "5", "6"]);
        // Zero is clamped to one.
        assert_eq!(store.set_capacity(0), 3);
        assert_eq!(store.capacity(), 1);
        assert_eq!(ids(&store), ["6"]);
    }

    #[test]
    fn default_capacity_is_one_thousand() {
        let mut store = MessageStore::new();
        assert_eq!(store.capacity(), 1_000);
        for i in 0..1_500 {
            store.push(msg(&i.to_string()));
        }
        assert_eq!(store.len(), 1_000);
        assert_eq!(store.get_index(0).unwrap().id.as_str(), "500");
        assert_eq!(store.iter().next_back().unwrap().id.as_str(), "1499");
    }

    #[test]
    fn replace_keeps_the_place_and_the_sequence_number() {
        let mut store = MessageStore::with_capacity(5);
        for id in ["a", "b", "c"] {
            store.push(msg(id));
        }
        let generation = store.generation();
        let newer = msg_by("b", "someone-else");
        assert_eq!(store.replace(newer.clone()), Some(1));
        assert!(Arc::ptr_eq(store.get("b").unwrap(), &newer));
        assert!(Arc::ptr_eq(store.get_by_seq(1).unwrap(), &newer));
        assert_eq!(store.len(), 3);
        assert_eq!(store.next_seq(), 3, "nothing was appended");
        assert_eq!(store.generation(), generation + 1);
        assert_eq!(store.replace(msg("unknown")), None);
        assert_eq!(store.generation(), generation + 1, "a miss changes nothing");
    }

    #[test]
    fn delete_one_message_replaces_it_without_touching_order() {
        let mut store = MessageStore::with_capacity(5);
        let original = msg("b");
        store.push(msg("a"));
        store.push(original.clone());
        store.push(msg("c"));
        assert!(store.mark_deleted("b"));
        assert!(store.get("b").unwrap().flags.deleted);
        assert!(!original.flags.deleted, "shared Arc is never mutated");
        assert_eq!(ids(&store), ["a", "b", "c"]);
        assert!(!store.get("a").unwrap().flags.deleted);
    }

    #[test]
    fn delete_by_user_and_all() {
        let mut store = MessageStore::with_capacity(10);
        store.push(msg_by("1", "alice"));
        store.push(msg_by("2", "bob"));
        store.push(msg_by("3", "alice"));
        store.push(Arc::new(Message::plain(
            "4",
            Timestamp(0),
            MessageKind::System,
            "x",
        )));
        assert_eq!(store.mark_user_deleted("Alice"), 2);
        assert!(store.get("1").unwrap().flags.deleted);
        assert!(!store.get("2").unwrap().flags.deleted);
        assert_eq!(store.mark_all_deleted(), 2, "already-deleted are skipped");
        assert!(store.iter().all(|m| m.flags.deleted));
    }

    #[test]
    fn duplicate_ids_index_the_newest_and_survive_eviction() {
        let mut store = MessageStore::with_capacity(2);
        store.push(msg("dup"));
        store.push(msg("x"));
        store.push(msg("dup")); // evicts the first "dup"
        assert_eq!(store.len(), 2);
        assert!(
            store.contains("dup"),
            "index must not be dropped by eviction"
        );
        assert_eq!(store.seq_of("dup"), Some(2));
        assert!(store.mark_deleted("dup"));
        assert!(store.get_by_seq(2).unwrap().flags.deleted);
    }

    #[test]
    fn clear_empties_but_keeps_counting() {
        let mut store = MessageStore::with_capacity(3);
        store.push(msg("a"));
        store.push(msg("b"));
        store.clear();
        assert!(store.is_empty());
        assert!(!store.contains("a"));
        assert_eq!((store.first_seq(), store.next_seq()), (2, 2));
        store.push(msg("c"));
        assert_eq!(store.seq_of("c"), Some(2));
    }

    #[test]
    fn push_is_cheap_at_scale() {
        // Guards against accidental O(n) push/evict (e.g. Vec::remove(0)).
        let mut store = MessageStore::with_capacity(1_000);
        let start = std::time::Instant::now();
        for i in 0..200_000 {
            store.push(msg(&i.to_string()));
        }
        assert_eq!(store.len(), 1_000);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "200k pushes took {:?}",
            start.elapsed()
        );
    }

    fn at(id: &str, ts: i64) -> Arc<Message> {
        let mut m = Message::chat(
            id,
            Timestamp(ts),
            Author::new("someone", "someone"),
            vec![Fragment::Text(id.to_owned())],
        );
        m.flags.history = true;
        Arc::new(m)
    }

    fn live(id: &str, ts: i64) -> Arc<Message> {
        let mut m = (*at(id, ts)).clone();
        m.flags.history = false;
        Arc::new(m)
    }

    #[test]
    fn history_on_an_empty_store_is_just_appended() {
        let mut store = MessageStore::new();
        assert_eq!(store.merge_history(vec![at("a", 1), at("b", 2)]), 2);
        assert_eq!(ids(&store), ["a", "b"]);
        assert_eq!((store.first_seq(), store.next_seq()), (0, 2));
    }

    #[test]
    fn history_goes_before_live_messages_that_arrived_first() {
        let mut store = MessageStore::new();
        store.push(live("l1", 100));
        store.push(live("l2", 110));
        let before = store.next_seq();
        assert_eq!(store.merge_history(vec![at("h1", 10), at("h2", 20)]), 2);
        assert_eq!(ids(&store), ["h1", "h2", "l1", "l2"]);
        assert!(store.get("h1").unwrap().flags.history);
        assert!(!store.get("l1").unwrap().flags.history);
        // Everything got a new sequence number: no old one means something else now.
        assert!(store.first_seq() >= before);
        assert_eq!(store.get_by_seq(0), None);
    }

    #[test]
    fn history_interleaves_by_time_and_keeps_each_sides_order() {
        let mut store = MessageStore::new();
        store.push(live("l1", 10));
        store.push(live("l2", 50));
        // Out of order on the way in: the merge sorts the history itself.
        assert_eq!(
            store.merge_history(vec![at("h2", 30), at("h1", 5), at("h3", 70)]),
            3
        );
        assert_eq!(ids(&store), ["h1", "l1", "h2", "l2", "h3"]);
    }

    #[test]
    fn a_message_already_shown_is_not_shown_again() {
        let mut store = MessageStore::new();
        store.push(live("m1", 10));
        store.push(live("m2", 20));
        let generation = store.generation();
        // Overlap with what is retained, and a repeat inside the history itself.
        assert_eq!(
            store.merge_history(vec![at("m1", 10), at("m2", 20), at("m2", 20)]),
            0
        );
        assert_eq!(store.generation(), generation, "nothing changed");
        assert_eq!(
            store.merge_history(vec![at("m2", 20), at("m3", 30), at("m3", 30)]),
            1
        );
        assert_eq!(ids(&store), ["m1", "m2", "m3"]);
        assert!(
            !store.get("m2").unwrap().flags.history,
            "the live copy stays"
        );
    }

    #[test]
    fn a_gap_is_filled_between_what_was_seen_before_and_after() {
        let mut store = MessageStore::new();
        for (id, ts) in [("a", 10), ("b", 20), ("e", 60), ("f", 70)] {
            store.push(live(id, ts));
        }
        // Reconnect: the service returns from a bit before the last message seen.
        assert_eq!(
            store.merge_history(vec![at("b", 20), at("c", 30), at("d", 40), at("e", 60)]),
            2
        );
        assert_eq!(ids(&store), ["a", "b", "c", "d", "e", "f"]);
    }

    #[test]
    fn merged_history_respects_the_capacity() {
        let mut store = MessageStore::with_capacity(3);
        store.push(live("l1", 100));
        store.push(live("l2", 110));
        store.merge_history(vec![at("h1", 1), at("h2", 2), at("h3", 3)]);
        // The oldest go first, as with `push`; the live ones survive.
        assert_eq!(ids(&store), ["h3", "l1", "l2"]);
        assert!(!store.contains("h1"));
        assert_eq!(
            store
                .seq_of("l2")
                .and_then(|s| store.get_by_seq(s))
                .unwrap()
                .id
                .as_str(),
            "l2"
        );
    }
}
