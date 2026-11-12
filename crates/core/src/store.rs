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
}
