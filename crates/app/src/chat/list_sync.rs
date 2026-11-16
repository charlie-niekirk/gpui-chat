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

    /// Sequence number after the last item.
    pub fn next_seq(self) -> u64 {
        self.next_seq
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

/// One edit of the list: replace `remove` items from `at` by `insert` new (unmeasured) ones.
/// A list of these is applied in order, each `at` being an index into the list as the earlier
/// ones left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Splice {
    pub at: usize,
    pub remove: usize,
    pub insert: usize,
}

/// The edits that turn the list `old` into `new` (both ascending): an entry of `old` that is
/// `None` is an item that has to go (a message the store dropped). Items in both stay, so
/// measured heights and the scroll anchor of everything that stays survive.
pub fn diff_rows(old: &[Option<u64>], new: &[u64]) -> Vec<Splice> {
    let mut splices = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == Some(new[j]) {
            i += 1;
            j += 1;
            continue;
        }
        let at = j;
        let (mut remove, mut insert) = (0, 0);
        // Old items that are gone, and new items that are not there yet, up to the next common one.
        while i < old.len() && (j >= new.len() || old[i].is_none_or(|seq| seq < new[j])) {
            remove += 1;
            i += 1;
        }
        while j < new.len() && (i >= old.len() || old[i].is_none_or(|seq| new[j] < seq)) {
            insert += 1;
            j += 1;
        }
        debug_assert!(remove + insert > 0, "the walk always moves");
        splices.push(Splice { at, remove, insert });
    }
    splices
}

/// The list's rows: the sequence numbers of the store's messages that are drawn, in order.
///
/// Normally every message of the store is a row ([`ListMirror`]: item `i` is `first_seq + i`, no
/// allocation). When some messages are hidden (ignored users, a filter) the rows are an explicit
/// ascending list of sequence numbers instead, and every lookup between a message and its row goes
/// through it. Either way the list is only ever *spliced*, never reset, so measured rows and the
/// scroll position survive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rows {
    mirror: ListMirror,
    /// `None`: every message of the mirror is a row.
    shown: Option<Vec<u64>>,
}

impl Rows {
    pub fn new(first_seq: u64, next_seq: u64) -> Self {
        Self {
            mirror: ListMirror::new(first_seq, next_seq),
            shown: None,
        }
    }

    /// The first sequence number of the store range seen (hidden messages included).
    #[cfg(test)]
    pub fn first_seq(&self) -> u64 {
        self.mirror.first_seq()
    }

    /// The sequence number after the store range seen.
    pub fn next_seq(&self) -> u64 {
        self.mirror.next_seq()
    }

    /// Whether some messages may be hidden.
    #[cfg(test)]
    pub fn is_filtered(&self) -> bool {
        self.shown.is_some()
    }

    /// The number of rows.
    pub fn len(&self) -> usize {
        self.shown
            .as_ref()
            .map_or_else(|| self.mirror.len(), Vec::len)
    }

    /// The message row `ix` shows.
    pub fn seq_at(&self, ix: usize) -> Option<u64> {
        match &self.shown {
            Some(shown) => shown.get(ix).copied(),
            None => (ix < self.mirror.len()).then(|| self.mirror.first_seq() + ix as u64),
        }
    }

    /// The row that shows message `seq`; `None` when it is hidden or gone.
    pub fn index_of(&self, seq: u64) -> Option<usize> {
        match &self.shown {
            Some(shown) => shown.binary_search(&seq).ok(),
            None => seq
                .checked_sub(self.mirror.first_seq())
                .map(|ix| ix as usize)
                .filter(|&ix| ix < self.mirror.len()),
        }
    }

    /// Whether message `seq` has a row.
    pub fn contains(&self, seq: u64) -> bool {
        self.index_of(seq).is_some()
    }

    /// The message of the last row.
    pub fn last_seq(&self) -> Option<u64> {
        match &self.shown {
            Some(shown) => shown.last().copied(),
            None => self
                .mirror
                .len()
                .checked_sub(1)
                .map(|n| self.mirror.first_seq() + n as u64),
        }
    }

    /// The messages that have a row, oldest first.
    pub fn seqs(&self) -> Vec<u64> {
        match &self.shown {
            Some(shown) => shown.clone(),
            None => (self.mirror.first_seq()..self.mirror.next_seq()).collect(),
        }
    }

    /// Hidden rows are not counted: how many rows show a message at or after `seq`.
    pub fn rows_from(&self, seq: u64) -> usize {
        match &self.shown {
            Some(shown) => shown.len() - shown.partition_point(|&s| s < seq),
            None => {
                (self.mirror.next_seq()
                    - seq.clamp(self.mirror.first_seq(), self.mirror.next_seq()))
                    as usize
            }
        }
    }

    /// Moves to the store's current range `first..next`: the edit for the list, as for
    /// [`ListMirror::sync`] (rows dropped at the front, rows appended at the back). `shows` says
    /// whether a message that arrived gets a row.
    pub fn sync(&mut self, first: u64, next: u64, shows: &dyn Fn(u64) -> bool) -> Change {
        let Some(shown) = &mut self.shown else {
            return self.mirror.sync(first, next);
        };
        let next = next.max(first);
        let (old_first, old_next) = (self.mirror.first_seq(), self.mirror.next_seq());
        let reset = first < old_first || next < old_next;
        let removed = if reset {
            shown.len()
        } else {
            shown.partition_point(|&seq| seq < first)
        };
        shown.drain(..removed);
        let from = if reset { first } else { old_next.max(first) };
        let before = shown.len();
        if reset {
            shown.clear();
        }
        shown.extend((from..next).filter(|&seq| shows(seq)));
        let appended = shown.len() - if reset { 0 } else { before };
        self.mirror = ListMirror::new(first, next);
        Change { removed, appended }
    }

    /// Changes which messages have a row to `shows` (`None`: all of them), keeping the store range
    /// `first..next`. Returns the edits for the list. `shows` is asked about every message when a
    /// list is built, so call it when the answer changed, not per message.
    pub fn show_only(
        &mut self,
        shows: Option<&dyn Fn(u64) -> bool>,
        first: u64,
        next: u64,
    ) -> Vec<Splice> {
        let old: Vec<Option<u64>> = self.seqs().into_iter().map(Some).collect();
        let new_mirror = ListMirror::new(first, next);
        let new: Vec<u64> = match shows {
            Some(shows) => (new_mirror.first_seq()..new_mirror.next_seq())
                .filter(|&seq| shows(seq))
                .collect(),
            None => (new_mirror.first_seq()..new_mirror.next_seq()).collect(),
        };
        let splices = diff_rows(&old, &new);
        self.mirror = new_mirror;
        self.shown = shows.map(|_| new);
        splices
    }

    /// Replays a history merge that renumbered the store (see [`gpui_chat_core::Rebuild`]): the
    /// rows are renumbered with `map` (`None`: the message left) and the new store range is
    /// `first..next`; messages that were not there before get a row if `shows` says so. Returns the
    /// edits for the list.
    pub fn renumber(
        &mut self,
        map: &dyn Fn(u64) -> Option<u64>,
        first: u64,
        next: u64,
        shows: Option<&dyn Fn(u64) -> bool>,
    ) -> Vec<Splice> {
        let old: Vec<Option<u64>> = self.seqs().into_iter().map(map).collect();
        let new_mirror = ListMirror::new(first, next);
        let new: Vec<u64> = match shows {
            Some(shows) => (first..new_mirror.next_seq())
                .filter(|&seq| shows(seq))
                .collect(),
            None => (first..new_mirror.next_seq()).collect(),
        };
        let splices = diff_rows(&old, &new);
        self.mirror = new_mirror;
        self.shown = shows.map(|_| new);
        splices
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

    /// Applies `splices` to a plain vector, as the list would; `new` supplies the inserted items.
    fn apply_splices(items: &mut Vec<Option<u64>>, splices: &[Splice], new: &[u64]) {
        for splice in splices {
            let replacement: Vec<Option<u64>> = new[splice.at..splice.at + splice.insert]
                .iter()
                .map(|&seq| Some(seq))
                .collect();
            items.splice(splice.at..splice.at + splice.remove, replacement);
        }
    }

    fn some(seqs: &[u64]) -> Vec<Option<u64>> {
        seqs.iter().map(|&s| Some(s)).collect()
    }

    #[test]
    fn a_diff_turns_the_old_rows_into_the_new_ones() {
        let cases: [(&[Option<u64>], &[u64]); 7] = [
            (&[], &[]),
            (&[], &[1, 2]),
            (&[Some(1), Some(2)], &[]),
            (&[Some(1), Some(2), Some(3)], &[1, 2, 3]),
            (&[Some(2), Some(4)], &[1, 2, 3, 4, 5]),
            (&[None, None, Some(5), Some(7)], &[5, 6, 7, 8]),
            (&[Some(1), Some(3), Some(5)], &[2, 4]),
        ];
        for (old, new) in cases {
            let mut items = old.to_vec();
            apply_splices(&mut items, &diff_rows(old, new), new);
            assert_eq!(items, some(new), "{old:?} -> {new:?}");
        }
        // What stays is not touched: one insert in the middle is one splice.
        assert_eq!(
            diff_rows(&[Some(1), Some(3)], &[1, 2, 3]),
            [Splice {
                at: 1,
                remove: 0,
                insert: 1
            }]
        );
        assert!(diff_rows(&some(&[4, 5]), &[4, 5]).is_empty());
    }

    #[test]
    fn random_diffs_always_reproduce_the_target() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..500 {
            let pick = |bits: u64| -> Vec<u64> { (0..40).filter(|i| bits >> i & 1 == 1).collect() };
            let old: Vec<Option<u64>> = pick(next())
                .into_iter()
                .map(|s| if next() % 7 == 0 { None } else { Some(s) })
                .collect();
            let new = pick(next());
            let mut items = old.clone();
            apply_splices(&mut items, &diff_rows(&old, &new), &new);
            assert_eq!(items, some(&new));
        }
    }

    #[test]
    fn rows_without_a_filter_are_the_mirror() {
        let mut rows = Rows::new(10, 15);
        assert!(!rows.is_filtered());
        assert_eq!(rows.len(), 5);
        assert_eq!(
            (rows.seq_at(0), rows.seq_at(4), rows.seq_at(5)),
            (Some(10), Some(14), None)
        );
        assert_eq!(
            (rows.index_of(12), rows.index_of(9), rows.index_of(15)),
            (Some(2), None, None)
        );
        assert_eq!(rows.last_seq(), Some(14));
        assert_eq!(
            rows.sync(12, 20, &|_| false),
            Change {
                removed: 2,
                appended: 5
            },
            "the filter is not asked while nothing is hidden"
        );
        assert_eq!(rows.rows_from(18), 2);
    }

    #[test]
    fn a_filter_shows_only_what_it_says_and_follows_the_store() {
        let even = |seq: u64| seq.is_multiple_of(2);
        let mut rows = Rows::new(0, 10);
        let mut items = some(&rows.seqs());
        let splices = rows.show_only(Some(&even), 0, 10);
        apply_splices(&mut items, &splices, &rows.seqs());
        assert_eq!(rows.seqs(), [0, 2, 4, 6, 8]);
        assert_eq!(items, some(&[0, 2, 4, 6, 8]));
        assert!(rows.is_filtered());
        assert_eq!(
            (rows.index_of(4), rows.index_of(5), rows.seq_at(1)),
            (Some(2), None, Some(2))
        );
        assert!(rows.contains(8) && !rows.contains(9));
        assert_eq!(rows.last_seq(), Some(8));
        assert_eq!(rows.rows_from(5), 2);
        // The store evicts 0..4 and gets 10..14: rows drop at the front, matching ones append.
        let change = rows.sync(4, 14, &even);
        assert_eq!(
            change,
            Change {
                removed: 2,
                appended: 2
            }
        );
        assert_eq!(rows.seqs(), [4, 6, 8, 10, 12]);
        // Nothing new: nothing to do.
        assert!(rows.sync(4, 14, &even).is_empty());
        // The filter goes away: the hidden messages come back in between.
        let splices = rows.show_only(None, 4, 14);
        let mut items = some(&[4, 6, 8, 10, 12]);
        apply_splices(&mut items, &splices, &rows.seqs());
        assert_eq!(items, some(&(4..14).collect::<Vec<_>>()));
        assert!(!rows.is_filtered());
    }

    #[test]
    fn a_store_that_went_backwards_replaces_every_row_of_a_filtered_list() {
        let all = |_: u64| true;
        let mut rows = Rows::new(5, 9);
        rows.show_only(Some(&all), 5, 9);
        assert_eq!(
            rows.sync(0, 3, &all),
            Change {
                removed: 4,
                appended: 3
            }
        );
        assert_eq!(rows.seqs(), [0, 1, 2]);
    }

    #[test]
    fn a_renumbering_keeps_the_rows_that_stay_and_inserts_the_new_ones() {
        // Rows 2, 4 of the store 0..6; history puts two messages before them (numbers shift by 2)
        // and the filter lets the new even ones in.
        let even = |seq: u64| seq.is_multiple_of(2);
        let mut rows = Rows::new(0, 6);
        rows.show_only(Some(&even), 0, 6);
        assert_eq!(rows.seqs(), [0, 2, 4]);
        // The items are what the list holds: the old rows under their new numbers.
        let mut items = some(&[2, 4, 6]);
        let splices = rows.renumber(&|old| Some(old + 2), 0, 8, Some(&even));
        apply_splices(&mut items, &splices, &rows.seqs());
        assert_eq!(rows.seqs(), [0, 2, 4, 6]);
        assert_eq!(items, some(&[0, 2, 4, 6]));
        // A message that left (the front was trimmed by the cap) is removed.
        let mut rows = Rows::new(0, 4);
        rows.show_only(Some(&|_| true), 0, 4);
        let mut items = vec![None, None, Some(0), Some(1)];
        let splices = rows.renumber(&|old| old.checked_sub(2), 0, 2, Some(&|_| true));
        apply_splices(&mut items, &splices, &rows.seqs());
        assert_eq!(items, some(&[0, 1]));
    }

    /// A random walk of pushes, evictions and filter changes: the edits applied to a plain vector
    /// always give the rows the structure reports, and those are what the filter says of the range.
    #[test]
    fn random_walk_with_a_changing_filter_matches_a_model() {
        let cap = 60u64;
        let mut next = 0u64;
        let mut modulus = 1u64;
        let mut rows = Rows::new(0, 0);
        let mut items: Vec<Option<u64>> = Vec::new();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..1_500 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let first_before = next.saturating_sub(cap);
            if state.is_multiple_of(5) {
                // Another filter (modulus 1: nothing hidden).
                modulus = state / 7 % 4 + 1;
                let shows = move |seq: u64| seq.is_multiple_of(modulus);
                let splices = if modulus == 1 {
                    rows.show_only(None, first_before, next)
                } else {
                    rows.show_only(Some(&shows), first_before, next)
                };
                apply_splices(&mut items, &splices, &rows.seqs());
            } else {
                next += state % 25;
                let first = next.saturating_sub(cap);
                let shows = move |seq: u64| seq.is_multiple_of(modulus);
                let change = rows.sync(first, next, &shows);
                items.drain(..change.removed);
                let seqs = rows.seqs();
                items.extend(
                    seqs[seqs.len() - change.appended..]
                        .iter()
                        .map(|&s| Some(s)),
                );
            }
            let first = next.saturating_sub(cap);
            let expected: Vec<u64> = (first..next).filter(|s| s % modulus == 0).collect();
            assert_eq!(rows.seqs(), expected);
            assert_eq!(items, some(&expected));
            assert_eq!(rows.len(), expected.len());
        }
    }
}
