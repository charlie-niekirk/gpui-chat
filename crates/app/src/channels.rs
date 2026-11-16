//! Per-channel message state as GPUI entities, fed by the bridge.
//!
//! * [`ChannelStore`] wraps the core [`MessageStore`] of one channel plus its room state and
//!   connection state. It is the entity the chat view renders (PR 1.4).
//! * [`ChannelHub`] owns the stores. The bridge delivers each frame's batch of [`ChatEvent`]s to
//!   the hub ([`ChannelHub::apply_batch`]); the hub splits it by channel, applies each channel's
//!   events in order and notifies each touched store **once**, so a burst of 1 000 messages is one
//!   notification and one layout pass for the view, however many events it holds.
//!
//! Both are plain data structures with the entity plumbing on top, so the batching logic is
//! unit-tested without a window.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use gpui_chat_core::{
    ChannelId, ChatEvent, ClearTarget, ConnectionState, DEFAULT_CAPACITY, EventKind, Message,
    MessageId, MessageKind, MessageStore, Moderation, RoomState,
};
use gpui_kit::{App, AppContext as _, Context, Entity};

use crate::diagnostics::Diagnostics;

/// Messages and state of one channel.
pub struct ChannelStore {
    channel: ChannelId,
    messages: MessageStore,
    room: RoomState,
    connection: Option<ConnectionState>,
    /// Channel notices seen so far (kept as plain counts until the view shows them).
    notices: u64,
    /// Which messages were replaced in place (an echo swapped for Twitch's copy), as
    /// `(serial, seq)`, newest last and bounded; see [`ChannelStore::replaced_since`].
    replaced: VecDeque<(u64, u64)>,
    replaced_serial: u64,
    /// The latest live deletions and clears, so that recent-messages history that arrives *after*
    /// them (a fetch that was in flight, a gap answer) is struck through too, see
    /// [`ChannelStore::apply`].
    live_moderation: VecDeque<Moderation>,
    /// The latest mystery (gift bomb) rows as `(origin id, message id)`: the per-recipient lines
    /// that follow are folded into them (PR 5.1), see [`ChannelStore::fold_gift`].
    gift_rows: VecDeque<(String, MessageId)>,
}

/// How many gift bombs [`ChannelStore`] folds lines into at a time.
const GIFT_ROWS: usize = 16;

/// How many live moderation actions [`ChannelStore`] remembers for history that arrives later.
const MODERATION_LOG: usize = 128;

/// How many replacements [`ChannelStore`] remembers for its views.
const REPLACED_LOG: usize = 64;

impl ChannelStore {
    pub fn new(channel: ChannelId, capacity: usize) -> Self {
        Self {
            channel,
            messages: MessageStore::with_capacity(capacity),
            room: RoomState::default(),
            connection: None,
            notices: 0,
            replaced: VecDeque::new(),
            replaced_serial: 0,
            live_moderation: VecDeque::new(),
            gift_rows: VecDeque::new(),
        }
    }

    /// The sequence numbers of the messages replaced since `seen` (a serial returned earlier; start
    /// with 0), and the serial to pass next time. `None` instead of the list when more were
    /// replaced than are remembered: the view then has to treat every row as changed.
    pub fn replaced_since(&self, seen: u64) -> (Option<Vec<u64>>, u64) {
        let pending = self.replaced_serial - seen;
        if pending == 0 {
            return (Some(Vec::new()), self.replaced_serial);
        }
        if pending > self.replaced.len() as u64 {
            return (None, self.replaced_serial);
        }
        let mut seqs: Vec<u64> = self
            .replaced
            .iter()
            .filter(|(serial, _)| *serial > seen)
            .map(|(_, seq)| *seq)
            .collect();
        seqs.sort_unstable();
        seqs.dedup();
        (Some(seqs), self.replaced_serial)
    }

    pub fn channel(&self) -> &ChannelId {
        &self.channel
    }

    pub fn messages(&self) -> &MessageStore {
        &self.messages
    }

    pub fn room(&self) -> &RoomState {
        &self.room
    }

    /// The join state of this channel (not the connection as a whole, see
    /// [`ChannelHub::connection`]).
    pub fn connection(&self) -> Option<&ConnectionState> {
        self.connection.as_ref()
    }

    // Shown by the notices UI later; asserted in tests.
    #[allow(dead_code)]
    pub fn notices(&self) -> u64 {
        self.notices
    }

    /// Applies one event; returns whether anything visible changed.
    pub fn apply(&mut self, event: ChatEvent) -> bool {
        match event {
            ChatEvent::Message { message, .. } => {
                // A message with an id already shown is not shown twice (PR 3.2, PR 5.5): the
                // user's own message can arrive as the echo made when Twitch accepted it and
                // again as a chat line with the same id; a chat line or user notice can be in
                // recent-messages history and arrive live. The IRC copy of an echo is the real
                // thing (native emotes, colour, badges) and takes the echo's place; any other
                // repeat is dropped, history copy or live.
                if matches!(
                    message.kind,
                    MessageKind::Chat | MessageKind::Action | MessageKind::UserNotice
                ) && let Some(shown) = self.messages.get(message.id.as_str())
                {
                    if shown.flags.echo && !message.flags.echo && self.replace_logged(message) {
                        return true;
                    }
                    return false;
                }
                // One line of a gift bomb whose row is here: it joins the row (PR 5.1).
                if let Some(changed) = self.fold_gift(&message) {
                    return changed;
                }
                self.note_gift_row(&message);
                self.messages.push(message);
                true
            }
            // Recent messages (PR 5.5) take their place by time among what is shown, and what
            // moderators removed (in the answer, and live while it was being fetched) is
            // applied to everything shown, messages from before a reconnect included.
            ChatEvent::History {
                messages,
                moderation,
                ..
            } => {
                let messages = fold_gifts(messages);
                for message in &messages {
                    self.note_gift_row(message);
                }
                let rebuilds = self.messages.rebuild_serial();
                let mut changed = self.messages.merge_history(messages) > 0;
                if self.messages.rebuild_serial() != rebuilds {
                    // The replaced-in-place log names sequence numbers that no longer exist.
                    self.replaced.clear();
                }
                for action in moderation.iter().chain(&self.live_moderation) {
                    changed |= self.messages.apply_moderation(action) > 0;
                }
                changed
            }
            ChatEvent::ClearChat { target, at, .. } => {
                let at = at.or_else(|| self.messages.iter().map(|m| m.timestamp).max());
                if let Some(at) = at {
                    self.remember(Moderation::Clear {
                        target: target.clone(),
                        at,
                    });
                }
                match target {
                    ClearTarget::Channel => self.messages.mark_all_deleted() > 0,
                    ClearTarget::User { login, .. } => self.messages.mark_user_deleted(&login) > 0,
                }
            }
            ChatEvent::DeleteMessage { message_id, .. } => {
                self.remember(Moderation::Delete {
                    message_id: message_id.clone(),
                });
                self.messages.mark_deleted(message_id.as_str())
            }
            ChatEvent::RoomState { update, .. } => {
                let before = self.room.clone();
                self.room.apply(&update);
                before != self.room
            }
            ChatEvent::Notice { .. } => {
                self.notices += 1;
                true
            }
            ChatEvent::Connection { state, .. } => {
                let changed = self.connection.as_ref() != Some(&state);
                self.connection = Some(state);
                changed
            }
        }
    }

    /// Replaces the retained message with `message`'s id by it and logs the replacement for the
    /// views; `false` when no retained message has that id.
    fn replace_logged(&mut self, message: Arc<Message>) -> bool {
        let Some(seq) = self.messages.replace(message) else {
            return false;
        };
        self.replaced_serial += 1;
        if self.replaced.len() >= REPLACED_LOG {
            self.replaced.pop_front();
        }
        self.replaced.push_back((self.replaced_serial, seq));
        true
    }

    /// Remembers `message` as a gift bomb row that later lines can be folded into.
    fn note_gift_row(&mut self, message: &Message) {
        let Some(EventKind::MysteryGift {
            origin_id: Some(origin),
            ..
        }) = &message.event
        else {
            return;
        };
        if self.gift_rows.len() >= GIFT_ROWS {
            self.gift_rows.pop_front();
        }
        self.gift_rows
            .push_back((origin.clone(), message.id.clone()));
    }

    /// One per-recipient line of a gift bomb (`message`) whose row is shown: the recipient is
    /// added to the row (the row is replaced in place) and the line is not shown on its own.
    /// `None` when `message` is not such a line or its row is not here (then it is shown, quietly);
    /// `Some(changed)` when it was folded (`false`: the recipient was there already).
    fn fold_gift(&mut self, message: &Message) -> Option<bool> {
        let Some(EventKind::Gift {
            recipient,
            community_gift_id: Some(origin),
            ..
        }) = &message.event
        else {
            return None;
        };
        let row_id = self
            .gift_rows
            .iter()
            .rev()
            .find(|(known, _)| known == origin)?
            .1
            .clone();
        let row = self.messages.get(row_id.as_str())?.clone();
        let Some(event) = row.event.as_ref()?.with_recipient(recipient.name()) else {
            return Some(false);
        };
        let mut updated = (*row).clone();
        updated.event = Some(event);
        Some(self.replace_logged(Arc::new(updated)))
    }

    fn remember(&mut self, moderation: Moderation) {
        if self.live_moderation.len() >= MODERATION_LOG {
            self.live_moderation.pop_front();
        }
        self.live_moderation.push_back(moderation);
    }

    /// Applies `events` in order; returns whether anything visible changed.
    pub fn apply_all(&mut self, events: impl IntoIterator<Item = ChatEvent>) -> bool {
        let mut changed = false;
        for event in events {
            changed |= self.apply(event);
        }
        changed
    }
}

/// `messages` (recent history, oldest first) with the per-recipient lines of each gift bomb folded
/// into its row when the row is among them; lines whose row is not stay.
fn fold_gifts(messages: Vec<Arc<Message>>) -> Vec<Arc<Message>> {
    let mut out: Vec<Arc<Message>> = Vec::with_capacity(messages.len());
    let mut rows: HashMap<String, usize> = HashMap::new();
    for message in messages {
        if let Some(EventKind::MysteryGift {
            origin_id: Some(origin),
            ..
        }) = &message.event
        {
            rows.insert(origin.clone(), out.len());
        }
        let folded = match &message.event {
            Some(EventKind::Gift {
                recipient,
                community_gift_id: Some(origin),
                ..
            }) => rows.get(origin).copied().map(|at| {
                let event = out[at]
                    .event
                    .as_ref()
                    .and_then(|event| event.with_recipient(recipient.name()));
                (at, event)
            }),
            _ => None,
        };
        match folded {
            Some((at, Some(event))) => {
                let mut row = (*out[at]).clone();
                row.event = Some(event);
                out[at] = Arc::new(row);
            }
            Some((_, None)) => {}
            None => out.push(message),
        }
    }
    out
}

/// Splits a batch into per-channel runs, keeping each channel's events in arrival order and
/// channels in order of first appearance. Events without a channel come back separately.
pub fn split_by_channel(
    events: Vec<ChatEvent>,
) -> (Vec<(ChannelId, Vec<ChatEvent>)>, Vec<ChatEvent>) {
    let mut groups: Vec<(ChannelId, Vec<ChatEvent>)> = Vec::new();
    let mut index: HashMap<ChannelId, usize> = HashMap::new();
    let mut unrouted = Vec::new();
    for event in events {
        let Some(channel) = event.channel().cloned() else {
            unrouted.push(event);
            continue;
        };
        let slot = *index.entry(channel.clone()).or_insert_with(|| {
            groups.push((channel, Vec::new()));
            groups.len() - 1
        });
        groups[slot].1.push(event);
    }
    (groups, unrouted)
}

/// A channel's name and how full its store is, for the HUD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSize {
    pub channel: ChannelId,
    pub len: usize,
    pub capacity: usize,
}

/// All channel stores. Events reach it through [`crate::bridge::attach`].
pub struct ChannelHub {
    stores: HashMap<ChannelId, Entity<ChannelStore>>,
    /// Channels in the order their stores were created (the sidebar keeps its own order).
    order: Vec<ChannelId>,
    capacity: usize,
    diagnostics: Diagnostics,
    /// Whether an event for an unknown channel creates its store. On for the offline sources
    /// (firehose, replay), whose channels exist only through their events; off for the live
    /// client, where the channel list decides which stores exist, so a late event for a channel
    /// the user just closed cannot bring it back.
    auto_create: bool,
    /// Channels the user closed: auto-create skips them, so a source that keeps sending events
    /// for a closed channel (the firehose) cannot bring it back. [`ChannelHub::ensure`] clears it.
    closed: HashSet<ChannelId>,
    /// State of the connection as a whole (`Connection { channel: None, .. }` events).
    connection: Option<ConnectionState>,
    /// Other events that named no channel (connection-wide notices); nothing shows them yet.
    unrouted: u64,
}

impl ChannelHub {
    pub fn new(capacity: usize, diagnostics: Diagnostics) -> Self {
        Self {
            stores: HashMap::new(),
            order: Vec::new(),
            capacity: if capacity == 0 {
                DEFAULT_CAPACITY
            } else {
                capacity
            },
            diagnostics,
            auto_create: true,
            closed: HashSet::new(),
            connection: None,
            unrouted: 0,
        }
    }

    /// See the `auto_create` field.
    pub fn set_auto_create(&mut self, auto_create: bool) {
        self.auto_create = auto_create;
    }

    /// State of the connection as a whole; `None` until the first event (and always for the
    /// offline sources).
    pub fn connection(&self) -> Option<&ConnectionState> {
        self.connection.as_ref()
    }

    /// The store of `channel`, created empty if there is none yet.
    pub fn ensure(&mut self, channel: &ChannelId, cx: &mut Context<Self>) -> Entity<ChannelStore> {
        if let Some(store) = self.stores.get(channel) {
            return store.clone();
        }
        self.closed.remove(channel);
        let store = self.ensure_quiet(channel, cx);
        cx.notify();
        store
    }

    /// Drops the store of `channel` (its messages go with it) and keeps it from being created
    /// again by events, until [`ChannelHub::ensure`] is called for it.
    pub fn remove(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        self.closed.insert(channel.clone());
        if self.stores.remove(channel).is_some() {
            self.order.retain(|c| c != channel);
            cx.notify();
        }
    }

    pub fn channels(&self) -> &[ChannelId] {
        &self.order
    }

    // Only the tests look a store up by name; the shell goes through `ensure`.
    #[cfg(test)]
    pub fn store(&self, channel: &ChannelId) -> Option<&Entity<ChannelStore>> {
        self.stores.get(channel)
    }

    // Shown by the notices UI later; asserted in tests.
    #[allow(dead_code)]
    pub fn unrouted(&self) -> u64 {
        self.unrouted
    }

    /// Message counts of every store, in channel order.
    pub fn sizes(&self, cx: &App) -> Vec<StoreSize> {
        self.order
            .iter()
            .filter_map(|channel| {
                let store = self.stores.get(channel)?.read(cx);
                Some(StoreSize {
                    channel: channel.clone(),
                    len: store.messages().len(),
                    capacity: store.messages().capacity(),
                })
            })
            .collect()
    }

    /// The `apply` of the bridge: one frame's events. Each touched store is updated and
    /// notified once. Returns `true` when the hub itself changed (a channel appeared or the
    /// connection state did), so its observers have something new to show.
    pub fn apply_batch(&mut self, events: Vec<ChatEvent>, cx: &mut Context<Self>) -> bool {
        let started = Instant::now();
        let count = events.len();
        let (groups, unrouted) = split_by_channel(events);
        // Rooms that the batch itself announces, for the shared-chat check below: a replay (or a
        // join burst) can deliver a channel's `ROOMSTATE` and the first shared messages together.
        let announced: Vec<(ChannelId, String)> = groups
            .iter()
            .flat_map(|(channel, events)| {
                events.iter().filter_map(move |event| match event {
                    ChatEvent::RoomState { update, .. } => {
                        update.room_id.clone().map(|id| (channel.clone(), id))
                    }
                    _ => None,
                })
            })
            .collect();
        let mut changed = false;
        for event in unrouted {
            match event {
                ChatEvent::Connection {
                    channel: None,
                    state,
                } => {
                    changed |= self.connection.as_ref() != Some(&state);
                    self.connection = Some(state);
                }
                _ => self.unrouted += 1,
            }
        }
        for (channel, events) in groups {
            let events = self.name_shared_sources(&channel, events, &announced, cx);
            let store = match self.stores.get(&channel) {
                Some(store) => store.clone(),
                None if self.auto_create && !self.closed.contains(&channel) => {
                    changed = true;
                    self.ensure_quiet(&channel, cx)
                }
                None => continue,
            };
            store.update(cx, |store, cx| {
                if store.apply_all(events) {
                    cx.notify();
                }
            });
        }
        // The bridge runs this before the frame's draw starts, so the draw probe cannot see it.
        self.diagnostics
            .record_batch(started, count, started.elapsed());
        changed
    }

    /// Names the source channel of shared-chat messages (PR 5.1) that came from a channel this
    /// hub has open, by its login: Twitch only says which *room* a message was sent in. Every
    /// channel of a session keeps its own copy of each message (ids, replies and mentions
    /// resolve inside one channel), so nothing is dropped; a source that is not open keeps
    /// whatever `SpecialService` looked up (or the generic label). History batches are named too.
    fn name_shared_sources(
        &self,
        channel: &ChannelId,
        events: Vec<ChatEvent>,
        announced: &[(ChannelId, String)],
        cx: &App,
    ) -> Vec<ChatEvent> {
        let is_shared = |message: &Message| {
            message
                .shared
                .as_ref()
                .is_some_and(|shared| shared.source_login.is_none())
        };
        let any = events.iter().any(|event| match event {
            ChatEvent::Message { message, .. } => is_shared(message),
            ChatEvent::History { messages, .. } => messages.iter().any(|m| is_shared(m)),
            _ => false,
        });
        if !any {
            return events;
        }
        // Room id -> login of the other open channels, as far as their `ROOMSTATE`s told (and
        // this batch's).
        let rooms: HashMap<&str, &ChannelId> = self
            .stores
            .iter()
            .filter(|(other, _)| *other != channel)
            .filter_map(|(other, store)| Some((store.read(cx).room().room_id.as_deref()?, other)))
            .chain(
                announced
                    .iter()
                    .filter(|(other, _)| other != channel)
                    .map(|(other, id)| (id.as_str(), other)),
            )
            .collect();
        let named = |message: Arc<Message>| -> Arc<Message> {
            let Some(shared) = message.shared.as_ref().filter(|s| s.source_login.is_none()) else {
                return message;
            };
            let Some(source) = rooms.get(shared.source_room_id.as_str()) else {
                return message;
            };
            let mut message = (*message).clone();
            if let Some(shared) = message.shared.as_mut() {
                shared.source_login = Some(source.login().to_owned());
                shared
                    .source_name
                    .get_or_insert_with(|| source.login().to_owned());
            }
            Arc::new(message)
        };
        events
            .into_iter()
            .map(|event| match event {
                ChatEvent::Message { channel, message } => ChatEvent::Message {
                    channel,
                    message: named(message),
                },
                ChatEvent::History {
                    channel,
                    messages,
                    moderation,
                } => ChatEvent::History {
                    channel,
                    messages: messages.into_iter().map(named).collect(),
                    moderation,
                },
                other => other,
            })
            .collect()
    }

    fn ensure_quiet(
        &mut self,
        channel: &ChannelId,
        cx: &mut Context<Self>,
    ) -> Entity<ChannelStore> {
        let capacity = self.capacity;
        let store = cx.new(|_| ChannelStore::new(channel.clone(), capacity));
        self.stores.insert(channel.clone(), store.clone());
        self.order.push(channel.clone());
        store
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;

    use gpui_chat_core::{Author, FollowersOnly, Message, MessageId, RoomStateUpdate, Timestamp};
    use gpui_kit::{Context, IntoElement, Render, TestAppContext, Window, div};

    use super::*;
    use crate::bridge;

    fn chat(channel: &str, id: &str, login: &str) -> ChatEvent {
        ChatEvent::Message {
            channel: ChannelId::new(channel),
            message: Arc::new(Message::chat(
                id,
                Timestamp(0),
                Author::new(login, login),
                vec![gpui_chat_core::Fragment::Text(format!(
                    "hello from {login}"
                ))],
            )),
        }
    }

    #[test]
    fn a_chat_line_with_an_id_already_shown_is_not_shown_twice() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 10);
        // The echo of the user's own message, then Twitch's IRC copy of it.
        assert!(store.apply(chat("a", "same-id", "me")));
        assert!(!store.apply(chat("a", "same-id", "me")), "nothing changed");
        assert_eq!(store.messages().len(), 1);
        // The other way round: the IRC copy first, the echo second.
        assert!(store.apply(chat("a", "other-id", "me")));
        assert!(!store.apply(chat("a", "other-id", "me")));
        assert_eq!(store.messages().len(), 2);
        // Different ids are different messages.
        assert!(store.apply(chat("a", "third", "me")));
        assert_eq!(store.messages().len(), 3);
    }

    fn stamped(id: &str, ts: i64, history: bool) -> Arc<Message> {
        let mut message = Message::chat(
            id,
            Timestamp(ts),
            Author::new("someone", "someone"),
            vec![gpui_chat_core::Fragment::Text(id.to_owned())],
        );
        message.flags.history = history;
        Arc::new(message)
    }

    fn live_at(channel: &str, id: &str, ts: i64) -> ChatEvent {
        ChatEvent::Message {
            channel: ChannelId::new(channel),
            message: stamped(id, ts, false),
        }
    }

    fn history_of(channel: &str, items: &[(&str, i64)]) -> ChatEvent {
        ChatEvent::History {
            channel: ChannelId::new(channel),
            messages: items
                .iter()
                .map(|(id, ts)| stamped(id, *ts, true))
                .collect(),
            moderation: Vec::new(),
        }
    }

    fn order(store: &ChannelStore) -> Vec<(String, bool)> {
        store
            .messages()
            .iter()
            .map(|m| (m.id.to_string(), m.flags.history))
            .collect()
    }

    #[test]
    fn history_lands_before_live_messages_that_arrived_while_it_was_fetched() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 50);
        assert!(store.apply(live_at("a", "l1", 100)));
        assert!(store.apply(history_of("a", &[("h1", 10), ("h2", 20)])));
        assert_eq!(
            order(&store),
            [
                ("h1".into(), true),
                ("h2".into(), true),
                ("l1".into(), false)
            ]
        );
        // Live chat carries on after it.
        assert!(store.apply(live_at("a", "l2", 110)));
        assert_eq!(store.messages().len(), 4);
    }

    #[test]
    fn a_message_is_never_shown_twice_whichever_copy_comes_first() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 50);
        // Live first, then history repeating it: nothing changes.
        store.apply(live_at("a", "m1", 10));
        assert!(
            !store.apply(history_of("a", &[("m1", 10)])),
            "no visible change"
        );
        // History first, then the live copy of the same id: dropped, the history one stays.
        store.apply(history_of("a", &[("m2", 20)]));
        assert!(!store.apply(live_at("a", "m2", 20)));
        assert_eq!(order(&store), [("m1".into(), false), ("m2".into(), true)]);
    }

    #[test]
    fn after_a_reconnect_only_the_gap_is_filled_in_place() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 50);
        for (id, ts) in [("a1", 10), ("a2", 20)] {
            store.apply(live_at("a", id, ts));
        }
        // Disconnected; what chat said meanwhile (a3, a4) is only in the service. After the
        // re-join live messages resume (a5), and the gap fetch (which overlaps a2) arrives later.
        store.apply(live_at("a", "a5", 50));
        assert!(store.apply(history_of("a", &[("a2", 20), ("a3", 30), ("a4", 40)])));
        let ids: Vec<String> = order(&store).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["a1", "a2", "a3", "a4", "a5"]);
        // The same answer again (a second reconnect) changes nothing.
        assert!(!store.apply(history_of("a", &[("a3", 30), ("a4", 40)])));
    }

    fn by_login(id: &str, login: &str, ts: i64, history: bool) -> Arc<Message> {
        let mut message = Message::chat(
            id,
            Timestamp(ts),
            Author::new(login, login),
            vec![gpui_chat_core::Fragment::Text(id.to_owned())],
        );
        message.flags.history = history;
        Arc::new(message)
    }

    fn history_with(
        messages: Vec<Arc<Message>>,
        moderation: Vec<gpui_chat_core::Moderation>,
    ) -> ChatEvent {
        ChatEvent::History {
            channel: ChannelId::new("a"),
            messages,
            moderation,
        }
    }

    fn deleted(store: &ChannelStore) -> Vec<(String, bool)> {
        store
            .messages()
            .iter()
            .map(|m| (m.id.to_string(), m.flags.deleted))
            .collect()
    }

    #[test]
    fn a_live_delete_that_came_before_the_history_still_strikes_its_copy() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 50);
        store.apply(live_at("a", "l1", 100));
        // The join fetch is in flight; a moderator deletes m2, which the fetch will bring.
        store.apply(ChatEvent::DeleteMessage {
            channel: ChannelId::new("a"),
            message_id: MessageId::from("m2"),
            login: None,
        });
        assert!(store.apply(history_with(
            vec![stamped("m1", 10, true), stamped("m2", 20, true)],
            vec![]
        )));
        assert_eq!(
            deleted(&store),
            [
                ("m1".into(), false),
                ("m2".into(), true),
                ("l1".into(), false)
            ]
        );
    }

    #[test]
    fn a_live_timeout_reaches_history_sent_before_it_but_not_after() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 50);
        store.apply(ChatEvent::ClearChat {
            channel: ChannelId::new("a"),
            target: ClearTarget::User {
                login: "Spam".into(),
                duration: None,
            },
            at: Some(Timestamp(500)),
        });
        store.apply(history_with(
            vec![
                by_login("s1", "spam", 400, true),
                by_login("o1", "other", 450, true),
                by_login("s2", "spam", 600, true),
            ],
            vec![],
        ));
        assert_eq!(
            deleted(&store),
            [
                ("s1".into(), true),
                ("o1".into(), false),
                ("s2".into(), false)
            ]
        );
    }

    #[test]
    fn a_gap_answers_moderation_reaches_messages_seen_before_the_drop() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 50);
        store.apply(by_login_live("a", "spam1", "spam", 100));
        store.apply(by_login_live("a", "ok1", "fine", 110));
        let moderation = vec![
            gpui_chat_core::Moderation::Delete {
                message_id: "ok1".into(),
            },
            gpui_chat_core::Moderation::Clear {
                target: ClearTarget::User {
                    login: "spam".into(),
                    duration: None,
                },
                at: Timestamp(300),
            },
        ];
        // The answer holds nothing we do not have, only what happened to it.
        assert!(store.apply(history_with(
            vec![stamped("spam1", 100, true)],
            moderation.clone()
        )));
        assert_eq!(
            deleted(&store),
            [("spam1".into(), true), ("ok1".into(), true)]
        );
        assert!(
            !store.apply(history_with(vec![], moderation)),
            "again: nothing changes"
        );
    }

    fn by_login_live(channel: &str, id: &str, login: &str, ts: i64) -> ChatEvent {
        ChatEvent::Message {
            channel: ChannelId::new(channel),
            message: by_login(id, login, ts, false),
        }
    }

    #[test]
    fn a_user_notice_in_history_and_live_is_shown_once() {
        let notice = |history: bool| {
            let mut message =
                Message::chat("sub-1", Timestamp(10), Author::new("dave", "Dave"), vec![]);
            message.kind = MessageKind::UserNotice;
            message.flags.history = history;
            Arc::new(message)
        };
        let mut store = ChannelStore::new(ChannelId::new("a"), 50);
        store.apply(history_with(vec![notice(true)], vec![]));
        assert!(!store.apply(ChatEvent::Message {
            channel: ChannelId::new("a"),
            message: notice(false),
        }));
        assert_eq!(store.messages().len(), 1);
        // And the other way round.
        let mut store = ChannelStore::new(ChannelId::new("a"), 50);
        store.apply(ChatEvent::Message {
            channel: ChannelId::new("a"),
            message: notice(false),
        });
        assert!(!store.apply(history_with(vec![notice(true)], vec![])));
        assert_eq!(store.messages().len(), 1);
    }

    #[test]
    fn history_is_flagged_so_notifications_can_skip_it() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 50);
        store.apply(history_of("a", &[("h", 1)]));
        store.apply(live_at("a", "l", 2));
        let flags: Vec<bool> = store.messages().iter().map(|m| m.flags.history).collect();
        assert_eq!(flags, [true, false]);
    }

    #[test]
    fn a_history_batch_reaches_only_its_channels_store() {
        let mut groups =
            split_by_channel(vec![history_of("a", &[("h", 1)]), live_at("b", "l", 2)]).0;
        assert_eq!(groups.len(), 2);
        assert_eq!(groups.remove(0).0, ChannelId::new("a"));
    }

    #[test]
    fn the_irc_copy_replaces_the_echo_in_place() {
        let echo = |id: &str| {
            let mut message = Message::chat(
                id,
                Timestamp(0),
                Author::new("me", "me"),
                vec![gpui_chat_core::Fragment::Text("Kappa".into())],
            );
            message.flags.echo = true;
            ChatEvent::Message {
                channel: ChannelId::new("a"),
                message: Arc::new(message),
            }
        };
        let mut store = ChannelStore::new(ChannelId::new("a"), 10);
        store.apply(chat("a", "before", "x"));
        assert!(store.apply(echo("mine")));
        store.apply(chat("a", "after", "x"));
        assert_eq!(store.replaced_since(0), (Some(vec![]), 0));

        // Twitch's copy arrives: it takes the echo's place, nothing is appended.
        assert!(store.apply(chat("a", "mine", "me")));
        assert_eq!(store.messages().len(), 3);
        let shown = store.messages().get("mine").unwrap();
        assert!(!shown.flags.echo, "the real message is shown now");
        assert_eq!(store.messages().seq_of("mine"), Some(1));
        assert_eq!(store.replaced_since(0), (Some(vec![1]), 1));
        assert_eq!(store.replaced_since(1), (Some(vec![]), 1));

        // A further copy is a plain duplicate.
        assert!(!store.apply(chat("a", "mine", "me")));
        // An echo never replaces anything.
        assert!(!store.apply(echo("mine")));
        assert_eq!(store.replaced_since(0).1, 1);
    }

    #[test]
    fn a_view_that_missed_more_replacements_than_are_kept_is_told_to_redo_everything() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 1000);
        let echo = |id: String| {
            let mut message = Message::chat(
                id,
                Timestamp(0),
                Author::new("me", "me"),
                vec![gpui_chat_core::Fragment::Text("x".into())],
            );
            message.flags.echo = true;
            ChatEvent::Message {
                channel: ChannelId::new("a"),
                message: Arc::new(message),
            }
        };
        for n in 0..REPLACED_LOG + 5 {
            store.apply(echo(format!("m{n}")));
            store.apply(chat("a", &format!("m{n}"), "me"));
        }
        let (all, serial) = store.replaced_since(0);
        assert_eq!(all, None);
        assert_eq!(serial, (REPLACED_LOG + 5) as u64);
        // Caught up within the log: exact.
        let (recent, _) = store.replaced_since(serial - 2);
        assert_eq!(recent.map(|seqs| seqs.len()), Some(2));
    }

    #[test]
    fn local_lines_that_share_an_id_are_not_deduplicated() {
        let notice = |id: &str| ChatEvent::Message {
            channel: ChannelId::new("a"),
            message: Arc::new(Message::plain(id, Timestamp(0), MessageKind::System, "x")),
        };
        let mut store = ChannelStore::new(ChannelId::new("a"), 10);
        assert!(store.apply(notice("n")));
        assert!(store.apply(notice("n")));
        assert_eq!(store.messages().len(), 2);
    }

    #[test]
    fn messages_are_pushed_in_order_and_capped() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 3);
        for n in 0..5 {
            assert!(store.apply(chat("a", &format!("m{n}"), "u")));
        }
        let ids: Vec<_> = store
            .messages()
            .iter()
            .map(|m| m.id.as_str().to_owned())
            .collect();
        assert_eq!(ids, ["m2", "m3", "m4"]);
        assert_eq!(store.messages().first_seq(), 2);
        assert_eq!(store.messages().next_seq(), 5);
    }

    #[test]
    fn moderation_events_mark_messages_deleted() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 10);
        store.apply_all([
            chat("a", "1", "alice"),
            chat("a", "2", "bob"),
            chat("a", "3", "alice"),
        ]);
        let ch = ChannelId::new("a");
        assert!(store.apply(ChatEvent::DeleteMessage {
            channel: ch.clone(),
            message_id: MessageId::from("2"),
            login: None,
        }));
        assert!(!store.apply(ChatEvent::DeleteMessage {
            channel: ch.clone(),
            message_id: MessageId::from("2"),
            login: None,
        }));
        assert!(store.apply(ChatEvent::ClearChat {
            channel: ch.clone(),
            target: ClearTarget::User {
                login: "Alice".into(),
                duration: None,
            },
            at: None,
        }));
        assert!(store.messages().iter().all(|m| m.flags.deleted));
        assert!(!store.apply(ChatEvent::ClearChat {
            channel: ch,
            target: ClearTarget::Channel,
            at: None,
        }));
        assert_eq!(
            store.messages().len(),
            3,
            "deleted lines stay, struck through"
        );
    }

    #[test]
    fn room_and_connection_state_report_real_changes_only() {
        let mut store = ChannelStore::new(ChannelId::new("a"), 10);
        let update = RoomStateUpdate {
            followers_only: Some(FollowersOnly::All),
            ..Default::default()
        };
        let event = |update: &RoomStateUpdate| ChatEvent::RoomState {
            channel: ChannelId::new("a"),
            update: update.clone(),
        };
        assert!(store.apply(event(&update)));
        assert!(
            !store.apply(event(&update)),
            "same state again is not a change"
        );
        assert_eq!(store.room().followers_only, FollowersOnly::All);

        let connected = ChatEvent::Connection {
            channel: Some(ChannelId::new("a")),
            state: ConnectionState::Connected,
        };
        assert!(store.apply(connected.clone()));
        assert!(!store.apply(connected));
        assert_eq!(store.connection(), Some(&ConnectionState::Connected));
        assert!(store.apply(ChatEvent::Notice {
            channel: Some(ChannelId::new("a")),
            msg_id: None,
            text: "hi".into(),
        }));
        assert_eq!(store.notices(), 1);
    }

    #[test]
    fn split_keeps_per_channel_order_and_first_appearance_order() {
        let events = vec![
            chat("b", "1", "u"),
            chat("a", "2", "u"),
            chat("b", "3", "u"),
            ChatEvent::Notice {
                channel: None,
                msg_id: None,
                text: "global".into(),
            },
            chat("a", "4", "u"),
        ];
        let (groups, unrouted) = split_by_channel(events);
        let summary: Vec<_> = groups
            .iter()
            .map(|(channel, events)| {
                let ids: Vec<_> = events
                    .iter()
                    .map(|e| match e {
                        ChatEvent::Message { message, .. } => message.id.as_str().to_owned(),
                        _ => unreachable!(),
                    })
                    .collect();
                (channel.login().to_owned(), ids)
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("b".to_owned(), vec!["1".to_owned(), "3".to_owned()]),
                ("a".to_owned(), vec!["2".to_owned(), "4".to_owned()])
            ]
        );
        assert_eq!(unrouted.len(), 1);
    }

    // ---- entities in a headless window ----------------------------------------------------

    struct Root;
    impl Render for Root {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn frame(window: &gpui_kit::WindowHandle<Root>, cx: &mut TestAppContext) {
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                window.simulate_next_frame(cx);
            })
            .unwrap();
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn a_frame_batch_is_one_notify_per_store(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Root);
        let diagnostics = Diagnostics::new();
        let hub = cx.new(|_| ChannelHub::new(1000, diagnostics.clone()));
        let hub_notifies = Rc::new(Cell::new(0));
        let counter = hub_notifies.clone();
        cx.update(|cx| cx.observe(&hub, move |_, _| counter.set(counter.get() + 1)))
            .detach();
        let (tx, rx) = bridge::channel::<ChatEvent>(4096, 2048);
        let task = cx.update(|cx| {
            bridge::attach(&hub, window.into(), rx, cx, |hub, events, cx| {
                hub.apply_batch(events, cx)
            })
        });

        // 1 000 messages across two channels arrive between two frames.
        for n in 0..1000 {
            let channel = if n % 4 == 0 { "quiet" } else { "busy" };
            tx.try_send(chat(channel, &format!("m{n}"), "u")).unwrap();
        }
        cx.run_until_parked();
        assert_eq!(
            hub.read_with(cx, |hub, _| hub.channels().len()),
            0,
            "nothing before the frame"
        );

        // Observers of the stores are attached only once they exist, so count after the first
        // frame creates them: the first frame notifies the hub (new channels).
        frame(&window, cx);
        assert_eq!(
            hub_notifies.get(),
            1,
            "the hub notifies once, for the new channels"
        );
        let sizes = hub.read_with(cx, |hub, cx| hub.sizes(cx));
        assert_eq!(
            sizes
                .iter()
                .map(|s| (s.channel.login(), s.len))
                .collect::<Vec<_>>(),
            [("quiet", 250), ("busy", 750)]
        );
        assert_eq!(diagnostics.total_batches(), 1);
        assert_eq!(diagnostics.ingest_summary(Instant::now()).last_batch, 1000);

        // Now count store notifications for a second burst.
        let busy = hub.read_with(cx, |hub, _| {
            hub.store(&ChannelId::new("busy")).unwrap().clone()
        });
        let quiet = hub.read_with(cx, |hub, _| {
            hub.store(&ChannelId::new("quiet")).unwrap().clone()
        });
        let (busy_notifies, quiet_notifies) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let (b, q) = (busy_notifies.clone(), quiet_notifies.clone());
        cx.update(|cx| cx.observe(&busy, move |_, _| b.set(b.get() + 1)))
            .detach();
        cx.update(|cx| cx.observe(&quiet, move |_, _| q.set(q.get() + 1)))
            .detach();
        for n in 1000..1500 {
            tx.try_send(chat("busy", &format!("m{n}"), "u")).unwrap();
        }
        frame(&window, cx);
        assert_eq!(busy_notifies.get(), 1, "500 events, one notify");
        assert_eq!(
            quiet_notifies.get(),
            0,
            "an untouched store is not notified"
        );
        assert_eq!(hub_notifies.get(), 1, "no new channel, no hub notify");
        // 750 + 500 would be 1250: the 1 000-message cap evicted the oldest 250.
        assert_eq!(busy.read_with(cx, |s, _| s.messages().len()), 1000);
        assert_eq!(busy.read_with(cx, |s, _| s.messages().first_seq()), 250);

        // The scrollback cap applies per store.
        for n in 1500..2500 {
            tx.try_send(chat("busy", &format!("m{n}"), "u")).unwrap();
        }
        frame(&window, cx);
        assert_eq!(busy.read_with(cx, |s, _| s.messages().len()), 1000);
        assert_eq!(busy_notifies.get(), 2);
        drop(task);
    }

    fn room(channel: &str, room_id: &str) -> ChatEvent {
        ChatEvent::RoomState {
            channel: ChannelId::new(channel),
            update: RoomStateUpdate {
                room_id: Some(room_id.into()),
                ..Default::default()
            },
        }
    }

    /// A message of `channel` (whose room is `room_id`) that was sent in `source`.
    fn shared(channel: &str, room_id: &str, id: &str, source: &str, source_id: &str) -> ChatEvent {
        use gpui_chat_core::{SharedChat, message::RawTags};
        let ChatEvent::Message { channel, message } = chat(channel, id, "guest") else {
            unreachable!()
        };
        let mut message = (*message).clone();
        message.raw_tags = RawTags::new([("room-id".to_owned(), room_id.to_owned())]);
        message.shared = Some(SharedChat {
            source_room_id: source.into(),
            source_id: Some(MessageId::from(source_id)),
            source_badges: Vec::new(),
            source_login: None,
            source_name: None,
        });
        ChatEvent::Message {
            channel,
            message: Arc::new(message),
        }
    }

    fn ids_and_sources(
        hub: &Entity<ChannelHub>,
        channel: &str,
        cx: &mut TestAppContext,
    ) -> Vec<(String, Option<String>)> {
        hub.read_with(cx, |hub, cx| {
            hub.store(&ChannelId::new(channel))
                .unwrap()
                .read(cx)
                .messages()
                .iter()
                .map(|m| {
                    (
                        m.id.as_str().to_owned(),
                        m.shared.as_ref().and_then(|s| s.source_login.clone()),
                    )
                })
                .collect()
        })
    }

    #[gpui_kit::test]
    fn every_channel_keeps_its_copy_of_a_shared_chat_message_named_by_its_source(
        cx: &mut TestAppContext,
    ) {
        let hub = cx.new(|_| ChannelHub::new(100, Diagnostics::new()));
        // One batch, as `--replay-speed 0` and a join burst deliver it: the rooms are announced
        // together with the messages.
        hub.update(cx, |hub, cx| {
            hub.apply_batch(
                vec![
                    room("main", "1000"),
                    room("other", "2000"),
                    // Sent in #other: its own copy there, and the copy delivered to #main.
                    shared("other", "2000", "o1", "2000", "o1"),
                    shared("main", "1000", "m1", "2000", "o1"),
                    // Sent in a channel that is not open.
                    shared("main", "1000", "m2", "3000", "x1"),
                    // Sent in #main itself, and its copy in #other.
                    shared("main", "1000", "m3", "1000", "m3"),
                    shared("other", "2000", "o2", "1000", "m3"),
                    chat("main", "m4", "plain"),
                ],
                cx,
            );
        });
        let name = |s: &str| Some(s.to_owned());
        // Nothing is dropped: replies to `m1` and mentions in it resolve in #main. A message sent
        // in the channel itself needs no source.
        assert_eq!(
            ids_and_sources(&hub, "main", cx),
            [
                ("m1".into(), name("other")),
                ("m2".into(), None),
                ("m3".into(), None),
                ("m4".into(), None)
            ]
        );
        assert_eq!(
            ids_and_sources(&hub, "other", cx),
            [("o1".into(), None), ("o2".into(), name("main"))]
        );
    }

    #[gpui_kit::test]
    fn a_shared_message_whose_source_is_not_open_is_left_for_the_lookup(cx: &mut TestAppContext) {
        let hub = cx.new(|_| ChannelHub::new(100, Diagnostics::new()));
        hub.update(cx, |hub, cx| {
            hub.apply_batch(
                vec![
                    room("main", "1000"),
                    shared("main", "1000", "m1", "2000", "o1"),
                ],
                cx,
            );
            // The source channel is opened later: its messages are named from then on.
            hub.apply_batch(
                vec![
                    room("other", "2000"),
                    shared("main", "1000", "m2", "2000", "o2"),
                ],
                cx,
            );
            hub.apply_batch(vec![shared("main", "1000", "m3", "2000", "o3")], cx);
        });
        assert_eq!(
            ids_and_sources(&hub, "main", cx),
            [
                ("m1".into(), None),
                ("m2".into(), Some("other".to_owned())),
                ("m3".into(), Some("other".to_owned()))
            ]
        );
    }

    // ---- gift bombs ---------------------------------------------------------------------------

    use gpui_chat_core::{EventKind, Recipient, SubPlan};

    fn user_notice(id: &str, event: EventKind) -> Arc<Message> {
        let mut message = Message::chat(id, Timestamp(0), Author::new("bomber", "Bomber"), vec![]);
        message.kind = MessageKind::UserNotice;
        message.event = Some(event);
        Arc::new(message)
    }

    fn bomb(id: &str, count: u32) -> Arc<Message> {
        user_notice(
            id,
            EventKind::MysteryGift {
                plan: SubPlan::Tier1,
                count,
                gifter_total: Some(100),
                anonymous: false,
                origin_id: Some("origin".into()),
                recipients: Vec::new(),
            },
        )
    }

    fn gift(id: &str, to: &str, origin: Option<&str>) -> Arc<Message> {
        user_notice(
            id,
            EventKind::Gift {
                plan: SubPlan::Tier1,
                recipient: Recipient {
                    login: to.to_lowercase(),
                    display_name: to.into(),
                    user_id: None,
                },
                months: 1,
                gifter_total: None,
                anonymous: false,
                community_gift_id: origin.map(str::to_owned),
            },
        )
    }

    fn recipients(store: &ChannelStore, id: &str) -> Vec<String> {
        match &store.messages().get(id).unwrap().event {
            Some(EventKind::MysteryGift { recipients, .. }) => recipients.clone(),
            other => panic!("{other:?}"),
        }
    }

    fn message_event(message: Arc<Message>) -> ChatEvent {
        ChatEvent::Message {
            channel: ChannelId::new("c"),
            message,
        }
    }

    #[test]
    fn the_lines_of_a_gift_bomb_join_its_row_instead_of_adding_rows() {
        let mut store = ChannelStore::new(ChannelId::new("c"), 100);
        store.apply(message_event(bomb("b1", 3)));
        let seq = store.messages().seq_of("b1").unwrap();
        let (_, serial) = store.replaced_since(0);
        assert!(store.apply(message_event(gift("g1", "Ann", Some("origin")))));
        assert!(store.apply(message_event(gift("g2", "Ben", Some("origin")))));
        assert_eq!(store.messages().len(), 1, "no row per recipient");
        assert_eq!(recipients(&store, "b1"), ["Ann", "Ben"]);
        // The view is told the row changed (same sequence number).
        let (replaced, _) = store.replaced_since(serial);
        assert_eq!(replaced, Some(vec![seq]));
        // The same line again (live and in history): nothing changes, nothing shows.
        assert!(!store.apply(message_event(gift("g1b", "Ann", Some("origin")))));
        assert_eq!(store.messages().len(), 1);
        // Another bomb's line, or a plain gift, is a row of its own.
        store.apply(message_event(gift("g3", "Cleo", Some("other-origin"))));
        store.apply(message_event(gift("g4", "Dan", None)));
        assert_eq!(store.messages().len(), 3);
        assert_eq!(recipients(&store, "b1"), ["Ann", "Ben"]);
    }

    #[test]
    fn a_line_whose_row_is_gone_is_shown_on_its_own() {
        let mut store = ChannelStore::new(ChannelId::new("c"), 2);
        store.apply(message_event(bomb("b1", 3)));
        store.apply(message_event(chat_message("x1")));
        store.apply(message_event(chat_message("x2")));
        assert!(store.messages().get("b1").is_none(), "evicted");
        assert!(store.apply(message_event(gift("g1", "Ann", Some("origin")))));
        assert_eq!(store.messages().len(), 2);
    }

    fn chat_message(id: &str) -> Arc<Message> {
        Arc::new(Message::chat(
            id,
            Timestamp(0),
            Author::new("u", "U"),
            vec![],
        ))
    }

    #[test]
    fn history_folds_a_bomb_the_same_way() {
        let mut store = ChannelStore::new(ChannelId::new("c"), 100);
        let mut history = vec![
            chat_message("a"),
            bomb("b1", 12),
            gift("g1", "Ann", Some("origin")),
            gift("g2", "Ben", Some("origin")),
            gift("g3", "Cleo", Some("elsewhere")),
        ];
        for (n, message) in history.iter_mut().enumerate() {
            let mut m = (**message).clone();
            m.timestamp = Timestamp(n as i64 + 1);
            m.flags.history = true;
            *message = Arc::new(m);
        }
        store.apply(ChatEvent::History {
            channel: ChannelId::new("c"),
            messages: history,
            moderation: Vec::new(),
        });
        // a, the bomb with two recipients, and the line of a bomb that is not here.
        assert_eq!(store.messages().len(), 3);
        assert_eq!(recipients(&store, "b1"), ["Ann", "Ben"]);
        // A live line that follows is folded into the history row.
        assert!(store.apply(message_event(gift("g9", "Dan", Some("origin")))));
        assert_eq!(recipients(&store, "b1"), ["Ann", "Ben", "Dan"]);
        assert_eq!(store.messages().len(), 3);
    }

    #[gpui_kit::test]
    fn events_without_a_channel_are_counted_not_lost_silently(cx: &mut TestAppContext) {
        let hub = cx.new(|_| ChannelHub::new(10, Diagnostics::new()));
        hub.update(cx, |hub, cx| {
            let appeared = hub.apply_batch(
                vec![ChatEvent::Notice {
                    channel: None,
                    msg_id: None,
                    text: "login failed".into(),
                }],
                cx,
            );
            assert!(!appeared);
        });
        assert_eq!(hub.read_with(cx, |hub, _| hub.unrouted()), 1);
    }

    #[gpui_kit::test]
    fn live_hubs_only_keep_stores_the_shell_created(cx: &mut TestAppContext) {
        let hub = cx.new(|_| {
            let mut hub = ChannelHub::new(10, Diagnostics::new());
            hub.set_auto_create(false);
            hub
        });
        let channel = ChannelId::new("open");
        hub.update(cx, |hub, cx| {
            // Events for a channel nobody opened are dropped, not turned into a store.
            assert!(!hub.apply_batch(vec![chat("ghost", "1", "u")], cx));
            assert!(hub.channels().is_empty());
            hub.ensure(&channel, cx);
            hub.ensure(&channel, cx);
            assert_eq!(hub.channels().len(), 1, "ensure is idempotent");
            hub.apply_batch(vec![chat("open", "2", "u")], cx);
            assert_eq!(hub.sizes(cx)[0].len, 1);
            // A closed channel stays closed even if the client's last event for it arrives.
            hub.remove(&channel, cx);
            assert!(hub.channels().is_empty());
            hub.apply_batch(
                vec![ChatEvent::Connection {
                    channel: Some(channel.clone()),
                    state: ConnectionState::Disconnected { reason: None },
                }],
                cx,
            );
            assert!(hub.channels().is_empty() && hub.store(&channel).is_none());
        });
    }

    #[gpui_kit::test]
    fn connection_wide_state_is_kept_and_reported_as_a_change(cx: &mut TestAppContext) {
        let hub = cx.new(|_| ChannelHub::new(10, Diagnostics::new()));
        let event = |state| ChatEvent::Connection {
            channel: None,
            state,
        };
        hub.update(cx, |hub, cx| {
            assert_eq!(hub.connection(), None);
            assert!(hub.apply_batch(vec![event(ConnectionState::Connecting)], cx));
            assert!(
                !hub.apply_batch(vec![event(ConnectionState::Connecting)], cx),
                "same state again"
            );
            assert!(hub.apply_batch(vec![event(ConnectionState::Connected)], cx));
            assert_eq!(hub.connection(), Some(&ConnectionState::Connected));
            assert_eq!(hub.unrouted(), 0, "connection events are not 'unrouted'");
        });
    }

    #[gpui_kit::test]
    fn a_closed_channel_is_not_recreated_by_events_until_it_is_opened_again(
        cx: &mut TestAppContext,
    ) {
        // Offline sources auto-create stores, and keep sending for a channel after it was closed.
        let hub = cx.new(|_| ChannelHub::new(10, Diagnostics::new()));
        let channel = ChannelId::new("busy");
        hub.update(cx, |hub, cx| {
            assert!(hub.apply_batch(vec![chat("busy", "1", "u")], cx));
            assert_eq!(hub.channels().len(), 1);
            hub.remove(&channel, cx);
            assert!(!hub.apply_batch(vec![chat("busy", "2", "u")], cx));
            assert!(hub.channels().is_empty(), "stays closed");
            // Other channels are unaffected.
            assert!(hub.apply_batch(vec![chat("other", "3", "u")], cx));
            assert_eq!(hub.channels().len(), 1);
            // Opening it again clears the mark.
            hub.ensure(&channel, cx);
            hub.apply_batch(vec![chat("busy", "4", "u")], cx);
            let sizes = hub.sizes(cx);
            assert_eq!(sizes.iter().find(|s| s.channel == channel).unwrap().len, 1);
        });
    }
}
