//! Keeps a GPUI `ListState` in step with a [`gpui_chat_core::MessageStore`] incrementally.
//!
//! The list's items are the store's messages `first_seq..next_seq`, item `i` being sequence
//! number `first_seq + i`. Sequence numbers only grow, so between two looks at the store the
//! change is always "some messages left at the front (scrollback cap), some arrived at the
//! back". [`ListMirror::sync`] turns that into at most two splices; nothing is reset, so
//! measured heights and the scroll anchor of untouched items survive every batch.

/// The edit that brings the list up to date: remove `removed` items at the front, then append
/// `appended` items at the back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub removed: usize,
    pub appended: usize,
}

impl Change {
    pub fn is_empty(self) -> bool {
        self.removed == 0 && self.appended == 0
    }
}

/// The sequence-number range the list currently shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListMirror {
    first_seq: u64,
    next_seq: u64,
}

impl ListMirror {
    pub fn new(first_seq: u64, next_seq: u64) -> Self {
        Self {
            first_seq,
            next_seq: next_seq.max(first_seq),
        }
    }

    pub fn first_seq(self) -> u64 {
        self.first_seq
    }

    pub fn len(self) -> usize {
        (self.next_seq - self.first_seq) as usize
    }

    /// Moves to the store's current range and returns the edit for the list. Messages pushed
    /// and evicted between two syncs (a burst larger than the scrollback cap) never appear.
    pub fn sync(&mut self, store_first: u64, store_next: u64) -> Change {
        let store_next = store_next.max(store_first);
        if store_first < self.first_seq || store_next < self.next_seq {
            // Sequence numbers never go back for one store; treat it as a different store.
            let change = Change {
                removed: self.len(),
                appended: (store_next - store_first) as usize,
            };
            *self = Self::new(store_first, store_next);
            return change;
        }
        let kept_from = store_first.max(self.first_seq).min(self.next_seq);
        let change = Change {
            removed: (kept_from - self.first_seq) as usize,
            appended: (store_next - self.next_seq.max(store_first)) as usize,
        };
        *self = Self::new(store_first, store_next);
        change
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies `change` to a plain vector of sequence numbers, as the list would.
    fn apply(items: &mut Vec<u64>, change: Change, mirror: ListMirror) {
        items.drain(..change.removed);
        let next = mirror.first_seq + mirror.len() as u64;
        items.extend(next - change.appended as u64..next);
    }

    fn check(items: &[u64], mirror: ListMirror) {
        let expected: Vec<u64> =
            (mirror.first_seq..mirror.first_seq + mirror.len() as u64).collect();
        assert_eq!(items, expected);
    }

    #[test]
    fn appends_while_under_the_cap() {
        let mut mirror = ListMirror::new(0, 0);
        assert_eq!(
            mirror.sync(0, 3),
            Change {
                removed: 0,
                appended: 3
            }
        );
        assert_eq!(
            mirror.sync(0, 5),
            Change {
                removed: 0,
                appended: 2
            }
        );
        assert_eq!(mirror.len(), 5);
        assert!(mirror.sync(0, 5).is_empty(), "no new messages, no edit");
    }

    #[test]
    fn evicts_at_the_front_when_the_cap_is_reached() {
        let mut mirror = ListMirror::new(0, 10);
        // Cap 10: 4 more arrived, the 4 oldest left.
        assert_eq!(
            mirror.sync(4, 14),
            Change {
                removed: 4,
                appended: 4
            }
        );
        assert_eq!(mirror.first_seq(), 4);
        assert_eq!(mirror.len(), 10);
    }

    #[test]
    fn a_burst_larger_than_the_cap_replaces_everything_once() {
        let mut mirror = ListMirror::new(0, 10);
        // 25 arrived with cap 10: everything shown is gone, the newest 10 replace it.
        assert_eq!(
            mirror.sync(25, 35),
            Change {
                removed: 10,
                appended: 10
            }
        );
        assert_eq!(mirror.first_seq(), 25);
    }

    #[test]
    fn a_cleared_store_empties_the_list() {
        let mut mirror = ListMirror::new(3, 8);
        // `MessageStore::clear` keeps counting: first == next.
        assert_eq!(
            mirror.sync(8, 8),
            Change {
                removed: 5,
                appended: 0
            }
        );
        assert_eq!(mirror.len(), 0);
    }

    #[test]
    fn a_store_going_backwards_is_a_full_replacement() {
        let mut mirror = ListMirror::new(5, 9);
        assert_eq!(
            mirror.sync(0, 2),
            Change {
                removed: 4,
                appended: 2
            }
        );
        assert_eq!(mirror, ListMirror::new(0, 2));
    }

    #[test]
    fn random_walk_matches_a_model_of_the_list() {
        // A store with a cap, pushing bursts of various sizes; the edited vector must always
        // equal the store's sequence range.
        let cap = 50u64;
        let mut next = 0u64;
        let mut mirror = ListMirror::new(0, 0);
        let mut items = Vec::new();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            next += match state % 10 {
                0..=5 => state % 4,
                6..=8 => state % 40,
                _ => state % 200,
            };
            let first = next.saturating_sub(cap);
            let change = mirror.sync(first, next);
            apply(&mut items, change, mirror);
            check(&items, mirror);
            assert!(items.len() as u64 <= cap);
        }
    }
}
