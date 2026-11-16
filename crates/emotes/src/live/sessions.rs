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
    /// The server has said `Hello` on this connection: only then are subscriptions accepted. A
    /// change before that waits for the `Hello`, which subscribes to everything at once.
    ready: bool,
}

impl SevenTvSession {
    pub(crate) fn new(shared: Weak<Shared>) -> Self {
        Self {
            shared,
            subs: HashSet::new(),
            heartbeat: Duration::from_secs(45),
            notices: Notices::default(),
            ready: false,
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
        self.ready = false;
        Vec::new()
    }

    fn on_text(&mut self, text: &str, now: Instant) -> Reaction {
        let mut reaction = Reaction::default();
        match seventv::parse_frame(text) {
            Frame::Hello { heartbeat } => {
                self.heartbeat = heartbeat;
                self.ready = true;
                reaction.send = self.subscribe_all();
            }
            Frame::Reconnect => reaction.reconnect = true,
            Frame::EndOfStream(code) => {
                reaction.reconnect = true;
                reaction.hold_off = seventv::end_of_stream_hold_off(code);
                if !reaction.hold_off.is_zero() {
                    tracing::warn!(
                        ?code,
                        "7TV EventAPI ended the stream; staying away for a while"
                    );
                }
            }
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
                // Before `Hello` nothing is sent: it subscribes to everything, this included.
                if self.subs.insert(sub.clone()) && self.ready {
                    sub.subscribe_frames()
                } else {
                    Vec::new()
                }
            }
            Change::Unsubscribe(sub) => {
                if self.subs.remove(&sub) && self.ready {
                    sub.unsubscribe_frames()
                } else {
                    Vec::new()
                }
            }
            Change::Clear => {
                let ready = self.ready;
                self.subs
                    .drain()
                    .filter(|_| ready)
                    .flat_map(|sub| sub.unsubscribe_frames())
                    .collect()
            }
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
        let room_id = match &event {
            bttv::Event::Created { room_id, .. }
            | bttv::Event::Updated { room_id, .. }
            | bttv::Event::Deleted { room_id, .. } => room_id.clone(),
        };
        for channel in shared.channels_for_room(&room_id) {
            let change = match &event {
                bttv::Event::Created { emote, .. } => EmoteChange::Added(emote.clone()),
                bttv::Event::Deleted { emote_id, .. } => EmoteChange::Removed {
                    id: emote_id.as_str().into(),
                    name: None,
                },
                // A rename keeps everything else the emote had (an animated emote stays animated,
                // whatever little the update says); only an emote this channel does not know and
                // that came with its image is added.
                bttv::Event::Updated { id, code, full, .. } => {
                    let known = shared
                        .registry
                        .channel_set(&channel, EmoteProvider::Bttv)
                        .is_some_and(|set| set.contains_id(id));
                    match full {
                        Some(emote) if !known => EmoteChange::Added(emote.clone()),
                        _ => EmoteChange::Renamed {
                            id: id.as_str().into(),
                            from: None,
                            to: code.as_str().into(),
                        },
                    }
                }
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &str = r#"{"op":1,"d":{"heartbeat_interval":45000}}"#;

    fn sub(id: &str) -> Subscription {
        Subscription::EmoteSet(id.to_owned())
    }

    #[test]
    fn subscriptions_go_out_once_the_server_has_said_hello_and_only_once() {
        let mut session = SevenTvSession::new(Weak::new());
        // A channel opened before the connection (or before `Hello`): nothing to send yet...
        assert!(session.on_open().is_empty());
        assert!(session.apply(Change::Subscribe(sub("A"))).is_empty());
        assert!(session.apply(Change::Subscribe(sub("B"))).is_empty());
        // ... `Hello` subscribes to everything, once each.
        let frames = session.on_text(HELLO, Instant::now()).send;
        assert_eq!(frames.len(), 2, "{frames:?}");
        // From then on a change is sent as it happens.
        assert_eq!(session.apply(Change::Subscribe(sub("C"))).len(), 1);
        assert_eq!(session.apply(Change::Unsubscribe(sub("A"))).len(), 1);
        assert!(
            session.apply(Change::Subscribe(sub("C"))).is_empty(),
            "already subscribed"
        );
        // A new connection starts over: not ready, `Hello` subscribes to what is left.
        assert!(session.on_open().is_empty());
        assert!(session.apply(Change::Subscribe(sub("D"))).is_empty());
        assert_eq!(session.on_text(HELLO, Instant::now()).send.len(), 3);
    }

    #[test]
    fn the_end_of_the_stream_asks_for_a_pause_that_fits_the_code() {
        let mut session = SevenTvSession::new(Weak::new());
        let mut end = |code: u64| {
            session.on_text(
                &format!(r#"{{"op":7,"d":{{"code":{code}}}}}"#),
                Instant::now(),
            )
        };
        let restart = end(4006);
        assert!(restart.reconnect && restart.hold_off.is_zero());
        let refused = end(4002);
        assert!(refused.reconnect && refused.hold_off >= Duration::from_secs(60));
    }
}
