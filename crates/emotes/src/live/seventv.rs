//! The 7TV EventAPI (`wss://events.7tv.io/v3`): frames in, [`Dispatch`]es out. Pure parsing, no
//! I/O, tested against `fixtures/emotes/7tv-events-*.jsonl`.
//!
//! A frame is `{"op": n, "d": {...}}`. The server says `Hello` (op 1, with the heartbeat interval),
//! sends a `Heartbeat` (2) every interval, asks the client to `Reconnect` (4) before a restart,
//! acknowledges commands (5) and delivers events as `Dispatch` (0): `{"type", "body"}`. The
//! client subscribes (op 35) and unsubscribes (36) per event type and *condition*.
//!
//! Events used:
//!
//! * `emote_set.update` for a set id (`object_id`): `pushed` (added), `pulled` (removed) and
//!   `updated` (renamed: `value` is the new entry, `old_value` the old) lists of fields with
//!   `key: "emotes"`.
//! * `user.update` for a 7TV user id: an `updated` field `connections` (with the connection's
//!   `index`) holding nested changes; one whose `key` is `emote_set` (`value` / `old_value` are
//!   objects with the set's `id`) is the channel switching its active set. That is the shape
//!   Chatterino reads, see `Client::onUserUpdate` in
//!   <https://github.com/Chatterino/chatterino2/blob/master/src/providers/seventv/eventapi/Client.cpp>
//!   and `UserConnectionUpdateDispatch` in `eventapi/Dispatch.cpp`; a nested `emote_set_id` change
//!   (the connection's field name in the REST API, `value` a plain id) is accepted as well.
//! * `cosmetic.create`, `entitlement.create` and `entitlement.delete` with the condition
//!   `{platform: TWITCH, ctx: channel, id: <room id>}`: paint and badge definitions, and who
//!   gets or loses one (the user's Twitch id is in their `connections`).

use std::time::Duration;

use serde_json::{Value, json};

use crate::cosmetics::{Paint, parse_paint, parse_seventv_badge};
use crate::set::EmoteChange;
use crate::seventv::parse_active_emote;
use gpui_chat_core::Badge;

pub const DEFAULT_URL: &str = "wss://events.7tv.io/v3";

/// The heartbeat interval the server may ask for is clamped to this range (it is untrusted data
/// and decides when a silent connection is given up on).
pub const MIN_HEARTBEAT: Duration = Duration::from_secs(5);
pub const MAX_HEARTBEAT: Duration = Duration::from_secs(120);

const OP_DISPATCH: u64 = 0;
const OP_HELLO: u64 = 1;
const OP_HEARTBEAT: u64 = 2;
const OP_RECONNECT: u64 = 4;
const OP_ACK: u64 = 5;
const OP_ERROR: u64 = 6;
const OP_END_OF_STREAM: u64 = 7;
const OP_SUBSCRIBE: u64 = 35;
const OP_UNSUBSCRIBE: u64 = 36;

/// What the client listens to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subscription {
    /// `emote_set.update` of an emote set id.
    EmoteSet(String),
    /// `user.update` of a 7TV user id.
    User(String),
    /// `cosmetic.create`, `entitlement.create` and `entitlement.delete` in a channel (Twitch
    /// room id).
    Cosmetics(String),
}

impl Subscription {
    fn commands(&self) -> Vec<(&'static str, Value)> {
        match self {
            Self::EmoteSet(id) => {
                vec![("emote_set.update", json!({ "object_id": id }))]
            }
            Self::User(id) => vec![("user.update", json!({ "object_id": id }))],
            Self::Cosmetics(room) => {
                let condition = json!({ "platform": "TWITCH", "ctx": "channel", "id": room });
                [
                    "cosmetic.create",
                    "entitlement.create",
                    "entitlement.delete",
                ]
                .into_iter()
                .map(|kind| (kind, condition.clone()))
                .collect()
            }
        }
    }

    /// The frames that start listening.
    pub fn subscribe_frames(&self) -> Vec<String> {
        self.frames(OP_SUBSCRIBE)
    }

    /// The frames that stop listening.
    pub fn unsubscribe_frames(&self) -> Vec<String> {
        self.frames(OP_UNSUBSCRIBE)
    }

    fn frames(&self, op: u64) -> Vec<String> {
        self.commands()
            .into_iter()
            .map(|(kind, condition)| {
                json!({ "op": op, "d": { "type": kind, "condition": condition } }).to_string()
            })
            .collect()
    }
}

/// An entitlement's subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cosmetic {
    Paint,
    Badge,
}

/// An event.
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatch {
    /// Emotes added, removed or renamed in a set.
    EmoteSet {
        set_id: String,
        changes: Vec<EmoteChange>,
    },
    /// A 7TV user switched the emote set of a connection. `set_id` is the new one (`None`: no
    /// active set), `connection` the index of the connection that changed.
    SetSwitch {
        user_id: String,
        set_id: Option<String>,
        connection: Option<usize>,
    },
    Paint(Paint),
    Badge(Badge),
    /// A user got (`granted`) or lost a paint or badge.
    Entitlement {
        granted: bool,
        kind: Cosmetic,
        ref_id: String,
        /// The user's Twitch id; `None` for a user without a Twitch connection.
        twitch_id: Option<String>,
    },
}

/// A parsed frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello {
        heartbeat: Duration,
    },
    Heartbeat,
    /// The server wants the client to reconnect (and resubscribe).
    Reconnect,
    /// The server closed the stream (`op` 7), with its reason code.
    EndOfStream(Option<u64>),
    Ack,
    /// The server refused a command (`op` 6).
    Error(String),
    Dispatch(Dispatch),
    /// A frame or event this client does not use.
    Ignored,
}

/// How long to stay away after the server ended the stream with `code`. 4006 (restart) and a
/// missing code come back at once (the usual backoff applies); a client error (4000–4004, 4009–
/// 4011: bad payload, unknown operation, already subscribed, ...) would only be repeated, so it
/// waits five minutes; 4005 (rate limited) and the rest wait two.
pub fn end_of_stream_hold_off(code: Option<u64>) -> Duration {
    match code {
        None | Some(4006) => Duration::ZERO,
        Some(4005 | 4007 | 4008) => Duration::from_secs(120),
        Some(_) => Duration::from_secs(300),
    }
}

/// Parses one text frame. Garbage is [`Frame::Ignored`] (logged at debug level).
pub fn parse_frame(text: &str) -> Frame {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        tracing::debug!("7TV EventAPI: unparsable frame");
        return Frame::Ignored;
    };
    let data = frame.get("d");
    match frame.get("op").and_then(Value::as_u64) {
        Some(OP_HELLO) => {
            let ms = data
                .and_then(|d| d.get("heartbeat_interval"))
                .and_then(Value::as_u64)
                .unwrap_or(45_000);
            Frame::Hello {
                heartbeat: Duration::from_millis(ms).clamp(MIN_HEARTBEAT, MAX_HEARTBEAT),
            }
        }
        Some(OP_HEARTBEAT) => Frame::Heartbeat,
        Some(OP_RECONNECT) => Frame::Reconnect,
        Some(OP_ACK) => Frame::Ack,
        Some(OP_ERROR) => Frame::Error(data.map(Value::to_string).unwrap_or_default()),
        Some(OP_END_OF_STREAM) => {
            Frame::EndOfStream(data.and_then(|d| d.get("code")).and_then(Value::as_u64))
        }
        Some(OP_DISPATCH) => data
            .and_then(parse_dispatch)
            .map_or(Frame::Ignored, Frame::Dispatch),
        _ => Frame::Ignored,
    }
}

fn parse_dispatch(d: &Value) -> Option<Dispatch> {
    let body = d.get("body")?;
    match d.get("type")?.as_str()? {
        "emote_set.update" => {
            let set_id = body.get("id")?.as_str()?.to_owned();
            let changes = emote_changes(body);
            (!changes.is_empty()).then_some(Dispatch::EmoteSet { set_id, changes })
        }
        "user.update" => set_switch(body),
        "cosmetic.create" => {
            let object = body.get("object")?;
            let data = object.get("data")?;
            match object.get("kind")?.as_str()? {
                "PAINT" => parse_paint(data).map(Dispatch::Paint),
                "BADGE" => parse_seventv_badge(data).map(Dispatch::Badge),
                _ => None,
            }
        }
        kind @ ("entitlement.create" | "entitlement.delete") => {
            let object = body.get("object")?;
            let cosmetic = match object.get("kind")?.as_str()? {
                "PAINT" => Cosmetic::Paint,
                "BADGE" => Cosmetic::Badge,
                _ => return None,
            };
            let twitch_id = object
                .pointer("/user/connections")
                .and_then(Value::as_array)
                .and_then(|connections| {
                    connections.iter().find_map(|connection| {
                        (connection.get("platform")?.as_str()? == "TWITCH")
                            .then(|| connection.get("id")?.as_str().map(str::to_owned))
                            .flatten()
                    })
                });
            Some(Dispatch::Entitlement {
                granted: kind == "entitlement.create",
                kind: cosmetic,
                ref_id: object.get("ref_id")?.as_str()?.to_owned(),
                twitch_id,
            })
        }
        _ => None,
    }
}

/// The emote changes in an `emote_set.update` body: pushed, pulled, updated, in that order.
fn emote_changes(body: &Value) -> Vec<EmoteChange> {
    let fields = |key: &str| {
        body.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|field| field.get("key").and_then(Value::as_str) == Some("emotes"))
    };
    let mut changes = Vec::new();
    for field in fields("pushed") {
        if let Some(entry) = field.get("value").and_then(parse_active_emote) {
            changes.push(EmoteChange::Added(entry));
        }
    }
    for field in fields("pulled") {
        let old = field.get("old_value");
        if let Some(id) = old.and_then(|v| v.get("id")).and_then(Value::as_str) {
            changes.push(EmoteChange::Removed {
                id: id.into(),
                name: old
                    .and_then(|v| v.get("name"))
                    .and_then(Value::as_str)
                    .map(Into::into),
            });
        }
    }
    for field in fields("updated") {
        let (new, old) = (field.get("value"), field.get("old_value"));
        let id = new
            .or(old)
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str);
        fn name(v: Option<&Value>) -> Option<&str> {
            v.and_then(|v| v.get("name")).and_then(Value::as_str)
        }
        if let (Some(id), Some(to)) = (id, name(new))
            && name(old) != Some(to)
        {
            changes.push(EmoteChange::Renamed {
                id: id.into(),
                from: name(old).map(Into::into),
                to: to.into(),
            });
        }
    }
    changes
}

/// `user.update` → [`Dispatch::SetSwitch`] when a connection's emote set changed (see the module
/// docs for the two shapes).
fn set_switch(body: &Value) -> Option<Dispatch> {
    let user_id = body.get("id")?.as_str()?.to_owned();
    for field in body.get("updated")?.as_array()? {
        if field.get("key").and_then(Value::as_str) != Some("connections") {
            continue;
        }
        let connection = field
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|i| usize::try_from(i).ok());
        for change in field.get("value").and_then(Value::as_array)? {
            let set_id = match change.get("key").and_then(Value::as_str) {
                // Objects with an `id` (null: no set any more).
                Some("emote_set") => change.get("value").and_then(|v| v.get("id")),
                Some("emote_set_id") => change.get("value"),
                _ => continue,
            };
            return Some(Dispatch::SetSwitch {
                user_id,
                set_id: set_id
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned),
                connection,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests;
