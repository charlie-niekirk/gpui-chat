//! In-memory LRU bounded by bytes rather than entry count.

use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

/// Identifies an image in every cache layer: its URL.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageKey(Arc<str>);

impl ImageKey {
    pub fn new(url: impl Into<Arc<str>>) -> Self {
        Self(url.into())
    }

    pub fn url(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for ImageKey {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ImageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl fmt::Display for ImageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ImageKey {
    fn from(url: &str) -> Self {
        Self::new(url)
    }
}

struct Entry<V> {
    value: Arc<V>,
    bytes: usize,
    tick: u64,
}

/// Least-recently-used map with a byte budget. Lookups that promote and inserts are
/// `O(log n)`. The most recently inserted entry is never evicted by its own insert, so an
/// entry larger than the whole budget is still held (alone) rather than thrashing.
pub(crate) struct ByteLru<V> {
    map: HashMap<ImageKey, Entry<V>>,
    /// Recency order: lowest tick = least recently used.
    order: BTreeMap<u64, ImageKey>,
    next_tick: u64,
    bytes: usize,
    budget: usize,
}

impl<V> ByteLru<V> {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: BTreeMap::new(),
            next_tick: 0,
            bytes: 0,
            budget,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn budget(&self) -> usize {
        self.budget
    }

    fn tick(&mut self) -> u64 {
        self.next_tick += 1;
        self.next_tick
    }

    /// Returns the value and marks it most recently used.
    pub(crate) fn get(&mut self, url: &str) -> Option<Arc<V>> {
        let tick = self.tick();
        let entry = self.map.get_mut(url)?;
        let key = self.order.remove(&entry.tick).expect("order and map agree");
        entry.tick = tick;
        self.order.insert(tick, key);
        Some(entry.value.clone())
    }

    /// Returns the value without changing recency.
    pub(crate) fn peek(&self, url: &str) -> Option<&Arc<V>> {
        self.map.get(url).map(|e| &e.value)
    }

    /// Inserts (or replaces) and evicts least recently used entries until within budget.
    /// Returns what was evicted, including a replaced value under the same key.
    pub(crate) fn insert(
        &mut self,
        key: ImageKey,
        value: Arc<V>,
        bytes: usize,
    ) -> Vec<(ImageKey, Arc<V>)> {
        let mut evicted = Vec::new();
        if let Some(old) = self.remove(key.borrow()) {
            evicted.push((key.clone(), old));
        }
        let tick = self.tick();
        self.order.insert(tick, key.clone());
        self.map.insert(key, Entry { value, bytes, tick });
        self.bytes += bytes;
        self.evict_to_budget(&mut evicted);
        evicted
    }

    pub(crate) fn remove(&mut self, url: &str) -> Option<Arc<V>> {
        let entry = self.map.remove(url)?;
        self.order.remove(&entry.tick);
        self.bytes -= entry.bytes;
        Some(entry.value)
    }

    /// Changes the budget, evicting as needed.
    pub(crate) fn set_budget(&mut self, budget: usize) -> Vec<(ImageKey, Arc<V>)> {
        self.budget = budget;
        let mut evicted = Vec::new();
        self.evict_to_budget(&mut evicted);
        evicted
    }

    pub(crate) fn clear(&mut self) -> Vec<(ImageKey, Arc<V>)> {
        self.order.clear();
        self.bytes = 0;
        self.map.drain().map(|(k, e)| (k, e.value)).collect()
    }

    fn evict_to_budget(&mut self, evicted: &mut Vec<(ImageKey, Arc<V>)>) {
        while self.bytes > self.budget && self.map.len() > 1 {
            let Some((_, key)) = self.order.pop_first() else {
                break;
            };
            let entry = self.map.remove(&key).expect("order and map agree");
            self.bytes -= entry.bytes;
            evicted.push((key, entry.value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(evicted: &[(ImageKey, Arc<u32>)]) -> Vec<&str> {
        evicted.iter().map(|(k, _)| k.url()).collect()
    }

    #[test]
    fn evicts_least_recently_used_by_bytes() {
        let mut lru = ByteLru::new(300);
        assert!(lru.insert("a".into(), Arc::new(1), 100).is_empty());
        assert!(lru.insert("b".into(), Arc::new(2), 100).is_empty());
        assert!(lru.insert("c".into(), Arc::new(3), 100).is_empty());
        assert_eq!(lru.bytes(), 300);
        // Touch `a`, so `b` is now the oldest.
        assert_eq!(lru.get("a").as_deref(), Some(&1));
        let evicted = lru.insert("d".into(), Arc::new(4), 150);
        assert_eq!(keys(&evicted), ["b", "c"]);
        assert_eq!(lru.bytes(), 250);
        assert_eq!(lru.len(), 2);
        assert!(lru.peek("a").is_some() && lru.peek("d").is_some());
    }

    #[test]
    fn oversized_entry_is_kept_alone() {
        let mut lru = ByteLru::new(100);
        lru.insert("a".into(), Arc::new(1), 50);
        let evicted = lru.insert("big".into(), Arc::new(2), 500);
        assert_eq!(keys(&evicted), ["a"]);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.bytes(), 500);
        assert!(lru.get("big").is_some());
    }

    #[test]
    fn replace_budget_and_clear() {
        let mut lru = ByteLru::new(1000);
        lru.insert("a".into(), Arc::new(1), 100);
        let evicted = lru.insert("a".into(), Arc::new(2), 200);
        assert_eq!(keys(&evicted), ["a"]);
        assert_eq!(*evicted[0].1, 1);
        assert_eq!(lru.bytes(), 200);
        lru.insert("b".into(), Arc::new(3), 200);
        assert_eq!(keys(&lru.set_budget(250)), ["a"]);
        assert_eq!(lru.budget(), 250);
        assert_eq!(keys(&lru.clear()), ["b"]);
        assert_eq!((lru.len(), lru.bytes()), (0, 0));
        assert!(lru.remove("b").is_none());
    }
}
