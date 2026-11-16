//! Events flowing from the network backends to the UI.

use std::sync::Arc;
use std::time::Duration;

use crate::ids::{ChannelId, MessageId};
use crate::message::{Message, Timestamp};

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

/// What the local user is in a channel, from the `USERSTATE` the server sends after a join and
/// after each message (PR 5.2): decides whether moderation controls are offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelRole {
    /// Anyone else, and everyone while chat is anonymous.
    #[default]
    Viewer,
    /// A moderator (or staff / admin) of the channel.
    Moderator,
    /// The broadcaster: moderator rights plus mods, VIPs and raids.
    Broadcaster,
}

impl ChannelRole {
    /// Whether the user may delete messages, time out and ban here.
    pub fn can_moderate(self) -> bool {
        self != Self::Viewer
    }

    pub fn is_broadcaster(self) -> bool {
        self == Self::Broadcaster
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

/// A moderation action reported by recent-messages history (PR 5.5): something that happened to
/// the chat *before* the history was fetched, applied by `MessageStore::apply_moderation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Moderation {
    /// One message was deleted (`CLEARMSG`).
    Delete { message_id: MessageId },
    /// A timeout, ban or chat clear at `at` (`tmi-sent-ts`): it removed the messages sent at or
    /// before that time, not the ones the user (or anyone) sent afterwards.
    Clear { target: ClearTarget, at: Timestamp },
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
    /// Recent messages fetched for a channel (PR 5.5), oldest first, every one flagged
    /// [`crate::MessageFlags::history`]. They are *merged* into what the channel already shows
    /// ([`crate::MessageStore::merge_history`]: ordered by time, nothing shown twice), not
    /// appended, because live messages may have arrived while they were being fetched.
    /// `moderation` is what the answer said was removed, in order; it is applied to the merged
    /// store (messages from before a reconnect included), not only to `messages`.
    History {
        channel: ChannelId,
        messages: Vec<Arc<Message>>,
        moderation: Vec<Moderation>,
    },
    /// `CLEARCHAT`: whole channel, or one user's messages (timeout / ban). `at` is its
    /// `tmi-sent-ts` when the line had one.
    ClearChat {
        channel: ChannelId,
        target: ClearTarget,
        at: Option<Timestamp>,
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
    /// `USERSTATE`: what the logged-in user is in the channel (PR 5.2). Anonymous connections
    /// get none.
    UserState {
        channel: ChannelId,
        role: ChannelRole,
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
            | Self::History { channel, .. }
            | Self::ClearChat { channel, .. }
            | Self::DeleteMessage { channel, .. }
            | Self::RoomState { channel, .. }
            | Self::UserState { channel, .. } => Some(channel),
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
            at: None,
        };
        assert_eq!(e.channel(), Some(&ch));
        let e = ChatEvent::Connection {
            channel: None,
            state: ConnectionState::Connected,
        };
        assert_eq!(e.channel(), None);
    }
}
