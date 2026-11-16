//! The two providers' [`Session`]s: their subscription tables and what an event does.

use std::collections::HashSet;
use std::sync::Weak;
use std::time::{Duration, Instant};

use gpui_chat_core::EmoteProvider;

use super::bttv;
use super::manager::Shared;
use super::notices::Notices;
use super::seventv::{self, Cosmetic, Dispatch, Frame, Subscription};
use super::socket::{Change, Reaction, Session};
use crate::set::EmoteChange;

/// Flushes due lines to the sink (for channels that are still open).
fn flush(shared: &Shared, notices: &mut Notices, now: Instant) {
    for (channel, text) in notices.take_due(now) {
        if shared.is_open(&channel) {
            shared.sink.notice(&channel, text);
        }
    }
}

pub(crate) struct SevenTvSession {
    shared: Weak<Shared>,
    subs: HashSet<Subscription>,
    heartbeat: Duration,
    notices: Notices,
}

impl SevenTvSession {
    pub(crate) fn new(shared: Weak<Shared>) -> Self {
        Self {
            shared,
            subs: HashSet::new(),
            heartbeat: Duration::from_secs(45),
            notices: Notices::default(),
        }
    }

    fn subscribe_all(&self) -> Vec<String> {
        self.subs
            .iter()
            .flat_map(Subscription::subscribe_frames)
            .collect()
    }
}

impl Session for SevenTvSession {
    type Sub = Subscription;

    fn wants_connection(&self) -> bool {
        !self.subs.is_empty()
    }

    fn on_open(&mut self) -> Vec<String> {
        // The server speaks first (`Hello`); subscriptions go out after it.
        Vec::new()
    }

    fn on_text(&mut self, text: &str, now: Instant) -> Reaction {
        let mut reaction = Reaction::default();
        match seventv::parse_frame(text) {
            Frame::Hello { heartbeat } => {
                self.heartbeat = heartbeat;
                reaction.send = self.subscribe_all();
            }
            Frame::Reconnect | Frame::EndOfStream(_) => reaction.reconnect = true,
            Frame::Error(error) => tracing::debug!(%error, "7TV EventAPI refused a command"),
            Frame::Heartbeat | Frame::Ack | Frame::Ignored => {}
            Frame::Dispatch(dispatch) => {
                let Some(shared) = self.shared.upgrade() else {
                    return reaction;
                };
                match dispatch {
                    Dispatch::EmoteSet { set_id, changes } => {
                        for channel in shared.channels_for_set(&set_id) {
                            let applied = shared.registry.apply_changes(
                                &channel,
                                EmoteProvider::SevenTv,
                                &changes,
                            );
                            self.notices
                                .push(&channel, EmoteProvider::SevenTv, &applied, now);
                        }
                    }
                    Dispatch::SetSwitch {
                        user_id,
                        set_id,
                        connection,
                    } => {
                        shared.seven_set_switched(&user_id, set_id, connection);
                    }
                    Dispatch::Paint(paint) => {
                        shared.cosmetics.store_paint(paint);
                        shared.sink.cosmetics_changed();
                    }
                    Dispatch::Badge(badge) => {
                        shared.cosmetics.store_seventv_badge(badge);
                        shared.sink.cosmetics_changed();
                    }
                    Dispatch::Entitlement {
                        granted,
                        kind,
                        ref_id,
                        twitch_id,
                    } => {
                        if let Some(twitch_id) = twitch_id {
                            shared.cosmetics.entitle(
                                &twitch_id,
                                kind == Cosmetic::Paint,
                                &ref_id,
                                granted,
                                now,
                            );
                            shared.sink.cosmetics_changed();
                        }
                    }
                }
            }
        }
        reaction
    }

    fn apply(&mut self, change: Change<Subscription>) -> Vec<String> {
        match change {
            Change::Subscribe(sub) => {
                if self.subs.insert(sub.clone()) {
                    sub.subscribe_frames()
                } else {
                    Vec::new()
                }
            }
            Change::Unsubscribe(sub) => {
                if self.subs.remove(&sub) {
                    sub.unsubscribe_frames()
                } else {
                    Vec::new()
                }
            }
            Change::Clear => self
                .subs
                .drain()
                .flat_map(|sub| sub.unsubscribe_frames())
                .collect(),
        }
    }

    fn idle_limit(&self) -> Option<Duration> {
        // The server sends a heartbeat every interval; three missed ones is a dead connection.
        Some(self.heartbeat * 3)
    }

    fn ping_interval(&self) -> Option<Duration> {
        None
    }

    fn next_tick(&self) -> Option<Instant> {
        self.notices.next_due()
    }

    fn on_tick(&mut self, now: Instant) {
        if let Some(shared) = self.shared.upgrade() {
            flush(&shared, &mut self.notices, now);
        }
    }
}

pub(crate) struct BttvSession {
    shared: Weak<Shared>,
    /// Room ids joined.
    rooms: HashSet<String>,
    notices: Notices,
}

impl BttvSession {
    pub(crate) fn new(shared: Weak<Shared>) -> Self {
        Self {
            shared,
            rooms: HashSet::new(),
            notices: Notices::default(),
        }
    }
}

impl Session for BttvSession {
    type Sub = String;

    fn wants_connection(&self) -> bool {
        !self.rooms.is_empty()
    }

    fn on_open(&mut self) -> Vec<String> {
        self.rooms
            .iter()
            .map(|room| bttv::join_frame(room))
            .collect()
    }

    fn on_text(&mut self, text: &str, now: Instant) -> Reaction {
        let Some(event) = bttv::parse_frame(text) else {
            return Reaction::default();
        };
        let Some(shared) = self.shared.upgrade() else {
            return Reaction::default();
        };
        let (room_id, change) = match event {
            bttv::Event::Emote { room_id, emote } => (room_id, EmoteChange::Added(emote)),
            bttv::Event::Deleted { room_id, emote_id } => (
                room_id,
                EmoteChange::Removed {
                    id: emote_id.as_str().into(),
                    name: None,
                },
            ),
        };
        for channel in shared.channels_for_room(&room_id) {
            let applied = shared.registry.apply_changes(
                &channel,
                EmoteProvider::Bttv,
                std::slice::from_ref(&change),
            );
            self.notices
                .push(&channel, EmoteProvider::Bttv, &applied, now);
        }
        Reaction::default()
    }

    fn apply(&mut self, change: Change<String>) -> Vec<String> {
        match change {
            Change::Subscribe(room) => {
                if self.rooms.insert(room.clone()) {
                    vec![bttv::join_frame(&room)]
                } else {
                    Vec::new()
                }
            }
            Change::Unsubscribe(room) => {
                if self.rooms.remove(&room) {
                    vec![bttv::part_frame(&room)]
                } else {
                    Vec::new()
                }
            }
            Change::Clear => self
                .rooms
                .drain()
                .map(|room| bttv::part_frame(&room))
                .collect(),
        }
    }

    fn idle_limit(&self) -> Option<Duration> {
        // Our ping every 30 s is answered with a pong: silence for three of them is a dead socket.
        Some(Duration::from_secs(90))
    }

    fn ping_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(30))
    }

    fn next_tick(&self) -> Option<Instant> {
        self.notices.next_due()
    }

    fn on_tick(&mut self, now: Instant) {
        if let Some(shared) = self.shared.upgrade() {
            flush(&shared, &mut self.notices, now);
        }
    }
}
