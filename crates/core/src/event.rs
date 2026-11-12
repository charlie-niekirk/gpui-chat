//! Events flowing from the network backends to the UI.

use std::sync::Arc;
use std::time::Duration;

use crate::ids::{ChannelId, MessageId};
use crate::message::Message;

/// Followers-only mode of a room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FollowersOnly {
    #[default]
    Off,
    /// Any follower may chat.
    All,
    /// Followers of at least this many minutes.
    Minutes(u32),
}

/// A *partial* room-state update, as sent by `ROOMSTATE` (on join every field is
/// present; later only the changed ones). `None` means "unchanged".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoomStateUpdate {
    /// Twitch numeric room (broadcaster) id; needed to load channel emotes.
    pub room_id: Option<String>,
    pub emote_only: Option<bool>,
    pub followers_only: Option<FollowersOnly>,
    /// R9K / unique-chat mode.
    pub unique_chat: Option<bool>,
    /// Slow mode delay; zero means off.
    pub slow_mode: Option<Duration>,
    pub subscribers_only: Option<bool>,
}

/// The full, merged room state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoomState {
    pub room_id: Option<String>,
    pub emote_only: bool,
    pub followers_only: FollowersOnly,
    pub unique_chat: bool,
    pub slow_mode: Duration,
    pub subscribers_only: bool,
}

impl RoomState {
    /// Merges a partial update in.
    pub fn apply(&mut self, update: &RoomStateUpdate) {
        if let Some(id) = &update.room_id {
            self.room_id = Some(id.clone());
        }
        if let Some(v) = update.emote_only {
            self.emote_only = v;
        }
        if let Some(v) = update.followers_only {
            self.followers_only = v;
        }
        if let Some(v) = update.unique_chat {
            self.unique_chat = v;
        }
        if let Some(v) = update.slow_mode {
            self.slow_mode = v;
        }
        if let Some(v) = update.subscribers_only {
            self.subscribers_only = v;
        }
    }
}

/// What a `CLEARCHAT` removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearTarget {
    /// The whole channel's chat was cleared.
    Channel,
    /// One user's messages were removed. `duration` is the timeout length;
    /// `None` is a permanent ban.
    User {
        login: String,
        duration: Option<Duration>,
    },
}

/// State of the chat connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    /// Lost the connection; trying again (`attempt` starts at 1).
    Reconnecting {
        attempt: u32,
    },
    Disconnected {
        reason: Option<String>,
    },
}

/// Everything a backend can tell the UI about chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    /// A new message (chat, action, user-notice, ...) for a channel.
    Message {
        channel: ChannelId,
        message: Arc<Message>,
    },
    /// `CLEARCHAT`: whole channel, or one user's messages (timeout / ban).
    ClearChat {
        channel: ChannelId,
        target: ClearTarget,
    },
    /// `CLEARMSG`: one message was deleted. `login` is the author when reported.
    DeleteMessage {
        channel: ChannelId,
        message_id: MessageId,
        login: Option<String>,
    },
    /// `ROOMSTATE`: partial room-state update.
    RoomState {
        channel: ChannelId,
        update: RoomStateUpdate,
    },
    /// `NOTICE`. `channel` is `None` for notices not tied to a channel (e.g. login
    /// failures). `msg_id` is Twitch's notice id (`msg_banned`, `slow_on`, ...).
    Notice {
        channel: Option<ChannelId>,
        msg_id: Option<String>,
        text: String,
    },
    /// Connection state. `channel` is `None` for the whole connection, `Some` for the
    /// join state of one channel.
    Connection {
        channel: Option<ChannelId>,
        state: ConnectionState,
    },
}

impl ChatEvent {
    /// The channel the event belongs to, if it has one.
    pub fn channel(&self) -> Option<&ChannelId> {
        match self {
            Self::Message { channel, .. }
            | Self::ClearChat { channel, .. }
            | Self::DeleteMessage { channel, .. }
            | Self::RoomState { channel, .. } => Some(channel),
            Self::Notice { channel, .. } | Self::Connection { channel, .. } => channel.as_ref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_state_applies_partial_updates() {
        let mut state = RoomState::default();
        state.apply(&RoomStateUpdate {
            room_id: Some("12345".into()),
            slow_mode: Some(Duration::from_secs(30)),
            followers_only: Some(FollowersOnly::Minutes(10)),
            ..Default::default()
        });
        state.apply(&RoomStateUpdate {
            emote_only: Some(true),
            ..Default::default()
        });
        assert_eq!(state.room_id.as_deref(), Some("12345"));
        assert_eq!(state.slow_mode, Duration::from_secs(30));
        assert_eq!(state.followers_only, FollowersOnly::Minutes(10));
        assert!(state.emote_only);
        state.apply(&RoomStateUpdate {
            slow_mode: Some(Duration::ZERO),
            ..Default::default()
        });
        assert_eq!(state.slow_mode, Duration::ZERO);
        assert!(state.emote_only, "untouched fields keep their value");
    }

    #[test]
    fn event_channel_accessor() {
        let ch = ChannelId::new("forsen");
        let e = ChatEvent::ClearChat {
            channel: ch.clone(),
            target: ClearTarget::Channel,
        };
        assert_eq!(e.channel(), Some(&ch));
        let e = ChatEvent::Connection {
            channel: None,
            state: ConnectionState::Connected,
        };
        assert_eq!(e.channel(), None);
    }
}
