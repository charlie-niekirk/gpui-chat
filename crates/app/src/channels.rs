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

use std::collections::HashMap;
use std::time::Instant;

use gpui_chat_core::{
    ChannelId, ChatEvent, ClearTarget, ConnectionState, DEFAULT_CAPACITY, MessageStore, RoomState,
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
}

impl ChannelStore {
    pub fn new(channel: ChannelId, capacity: usize) -> Self {
        Self {
            channel,
            messages: MessageStore::with_capacity(capacity),
            room: RoomState::default(),
            connection: None,
            notices: 0,
        }
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
                self.messages.push(message);
                true
            }
            ChatEvent::ClearChat { target, .. } => match target {
                ClearTarget::Channel => self.messages.mark_all_deleted() > 0,
                ClearTarget::User { login, .. } => self.messages.mark_user_deleted(&login) > 0,
            },
            ChatEvent::DeleteMessage { message_id, .. } => {
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

    /// Applies `events` in order; returns whether anything visible changed.
    pub fn apply_all(&mut self, events: impl IntoIterator<Item = ChatEvent>) -> bool {
        let mut changed = false;
        for event in events {
            changed |= self.apply(event);
        }
        changed
    }
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
        let store = self.ensure_quiet(channel, cx);
        cx.notify();
        store
    }

    /// Drops the store of `channel` (its messages go with it).
    pub fn remove(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
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
            let store = match self.stores.get(&channel) {
                Some(store) => store.clone(),
                None if self.auto_create => {
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
        }));
        assert!(store.messages().iter().all(|m| m.flags.deleted));
        assert!(!store.apply(ChatEvent::ClearChat {
            channel: ch,
            target: ClearTarget::Channel,
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
}
