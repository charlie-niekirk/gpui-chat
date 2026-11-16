//! Moderation in the app (PR 5.2): who runs a [`ModCommand`], what a click on a message means, and
//! when a removed message is drawn collapsed.
//!
//! Everything here is plain data and functions (tested without a window):
//!
//! * [`Moderator`] runs a command. The live source goes through Helix
//!   ([`crate::live::LiveSource::moderate`]); the offline sources (`--replay`) have a *loopback*
//!   that turns the command into the chat events the server would have sent, so the moderation
//!   controls can be tried and screenshotted without a login ([`loopback_events`]).
//! * [`RowAction`] is a button or menu item on a message; [`RowAction::command`] builds the
//!   [`ModCommand`] from the message, so a click and a typed `/ban` are one code path.
//!   [`confirmation`] says which ones ask first.
//! * [`is_collapsed`] decides whether a removed message is drawn as "<message deleted>"
//!   (`chat.deleted_messages = "collapse"`) until the user reveals it.
//!
//! What the controls *look* like is in `chat/row.rs` (buttons, menu) and `pane.rs` (room modes);
//! the system lines ("alice was timed out for 10m") come from `gpui_chat_core::clear_line`, so live
//! and history messages read the same.

use std::time::Duration;

use gpui_chat_core::{
    ChannelId, ChatEvent, ClearTarget, FollowersOnly, Message, MessageKind, ModCommand, RoomChange,
    RoomStateUpdate, Timestamp, format_seconds,
};
use gpui_chat_settings::DeletedMessages;
use gpui_chat_twitch::ModError;
use gpui_kit::App;
use tokio::sync::oneshot;

use crate::bridge::BridgeSender;
use crate::live::LiveSource;
use crate::runtime;

/// One command to run in one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModRequest {
    pub channel: ChannelId,
    /// The channel's Twitch room (broadcaster) id, from its `ROOMSTATE`.
    pub room_id: String,
    pub command: ModCommand,
    /// The id of the user the command names, when known (a message's author): saves the lookup.
    pub target_id: Option<String>,
}

/// Why a command failed, as the sentence to show and what the UI should do besides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModFailure {
    pub message: String,
    pub kind: FailureKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The login was not granted the scope: offer to enable moderation (a login that asks for it).
    MissingScope,
    /// Twitch answered 403: the user is not (or no longer) a moderator here; the controls go away.
    Forbidden,
    Other,
}

/// The outcome of a command.
pub type ModOutcome = Result<(), ModFailure>;

/// Whoever runs moderation commands for a pane.
#[derive(Clone)]
pub enum Moderator {
    /// Helix, as the logged-in user.
    Live(LiveSource),
    /// `--replay`: the command's effect is fed back into the chat, nothing leaves the machine.
    Loopback(BridgeSender<ChatEvent>),
}

impl Moderator {
    /// Runs the command; the outcome arrives on the returned channel. `None` when nothing can run
    /// it (no login owner).
    pub fn run(&self, request: ModRequest, cx: &App) -> Option<oneshot::Receiver<ModOutcome>> {
        match self {
            Self::Live(live) => {
                let result = live.moderate(request, cx)?;
                let (done, outcome) = oneshot::channel();
                drop(runtime::spawn_backend(cx, async move {
                    let outcome = match result.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(describe(&error)),
                        // The backend is shutting down: nothing to say.
                        Err(_) => return,
                    };
                    let _ = done.send(outcome);
                }));
                Some(outcome)
            }
            Self::Loopback(sender) => {
                let sender = sender.clone();
                let (done, outcome) = oneshot::channel();
                drop(runtime::spawn_backend(cx, async move {
                    for event in loopback_events(&request, crate::live::now()) {
                        if sender.send(event).await.is_err() {
                            return;
                        }
                    }
                    let _ = done.send(Ok(()));
                }));
                Some(outcome)
            }
        }
    }
}

/// The failure of a command: Twitch's own wording (or the "enable moderation" sentence for a login
/// that lacks a scope) and what kind it is.
pub fn describe(error: &ModError) -> ModFailure {
    let kind = if matches!(error, ModError::MissingScope { .. }) {
        FailureKind::MissingScope
    } else if error.is_forbidden() {
        FailureKind::Forbidden
    } else {
        FailureKind::Other
    };
    ModFailure {
        message: if kind == FailureKind::Forbidden {
            format!("{error} (Moderation controls are hidden here until you rejoin the channel.)")
        } else {
            error.to_string()
        },
        kind,
    }
}

/// The chat events the server would send for `request`, for the offline loopback.
pub fn loopback_events(request: &ModRequest, now: Timestamp) -> Vec<ChatEvent> {
    let channel = request.channel.clone();
    let clear = |target| ChatEvent::ClearChat {
        channel: channel.clone(),
        target,
        at: Some(now),
    };
    let room = |update| ChatEvent::RoomState {
        channel: channel.clone(),
        update,
    };
    match &request.command {
        ModCommand::Timeout { user, seconds, .. } => vec![clear(ClearTarget::User {
            login: user.clone(),
            duration: Some(Duration::from_secs(*seconds)),
        })],
        ModCommand::Ban { user, .. } => vec![clear(ClearTarget::User {
            login: user.clone(),
            duration: None,
        })],
        ModCommand::DeleteMessage { message_id } => vec![ChatEvent::DeleteMessage {
            channel: channel.clone(),
            message_id: message_id.as_str().into(),
            login: None,
        }],
        ModCommand::ClearChat => vec![clear(ClearTarget::Channel)],
        ModCommand::Room(change) => vec![room(match *change {
            RoomChange::Slow(seconds) => RoomStateUpdate {
                slow_mode: Some(Duration::from_secs(u64::from(seconds.unwrap_or(0)))),
                ..Default::default()
            },
            RoomChange::Followers(minutes) => RoomStateUpdate {
                followers_only: Some(match minutes {
                    None => FollowersOnly::Off,
                    Some(0) => FollowersOnly::All,
                    Some(minutes) => FollowersOnly::Minutes(minutes),
                }),
                ..Default::default()
            },
            RoomChange::Subscribers(on) => RoomStateUpdate {
                subscribers_only: Some(on),
                ..Default::default()
            },
            RoomChange::EmoteOnly(on) => RoomStateUpdate {
                emote_only: Some(on),
                ..Default::default()
            },
            RoomChange::Unique(on) => RoomStateUpdate {
                unique_chat: Some(on),
                ..Default::default()
            },
        })],
        other => other
            .confirmation()
            .map(|text| vec![local_line(&channel, now, text)])
            .unwrap_or_default(),
    }
}

/// A system line in `channel` (the confirmation of a command chat does not report by itself).
pub fn local_line(channel: &ChannelId, at: Timestamp, text: String) -> ChatEvent {
    ChatEvent::Message {
        channel: channel.clone(),
        message: std::sync::Arc::new(Message::plain(
            format!("moderation-local:{}", at.unix_millis()),
            at,
            MessageKind::System,
            text,
        )),
    }
}

/// What a button or menu item on a message does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    Delete,
    /// Time the author out for this many seconds.
    Timeout(u64),
    Ban,
    Unban,
}

impl RowAction {
    /// The command for `message`'s author (or the message itself for Delete); `None` when the
    /// message has no author to act on.
    pub fn command(self, message: &Message) -> Option<ModCommand> {
        let user = || message.author.as_ref().map(|author| author.login.clone());
        Some(match self {
            Self::Delete => ModCommand::DeleteMessage {
                message_id: message.id.as_str().to_owned(),
            },
            Self::Timeout(seconds) => ModCommand::Timeout {
                user: user()?,
                seconds,
                reason: None,
            },
            Self::Ban => ModCommand::Ban {
                user: user()?,
                reason: None,
            },
            Self::Unban => ModCommand::Unban { user: user()? },
        })
    }

    /// The command for the user `login` (the user card, PR 5.4: the user may have no message
    /// left to act on); `None` for Delete, which needs a message.
    pub fn command_for_user(self, login: &str) -> Option<ModCommand> {
        Some(match self {
            Self::Delete => return None,
            Self::Timeout(seconds) => ModCommand::Timeout {
                user: login.to_owned(),
                seconds,
                reason: None,
            },
            Self::Ban => ModCommand::Ban {
                user: login.to_owned(),
                reason: None,
            },
            Self::Unban => ModCommand::Unban {
                user: login.to_owned(),
            },
        })
    }

    /// The menu item / tooltip text.
    pub fn label(self) -> String {
        match self {
            Self::Delete => "Delete message".to_owned(),
            Self::Timeout(seconds) => format!("Timeout {}", format_seconds(seconds)),
            Self::Ban => "Ban".to_owned(),
            Self::Unban => "Unban".to_owned(),
        }
    }
}

/// The question to ask before running `command` from a button or menu item, as `(title, body)`:
/// only a ban asks (it is the one that cannot be undone by the next click, and the one a stray
/// click on a fast-moving chat is likely to hit). Typed commands are deliberate and never ask.
pub fn confirmation(command: &ModCommand) -> Option<(String, String)> {
    match command {
        ModCommand::Ban { user, .. } => Some((
            format!("Ban {user}?"),
            format!("{user} will be banned from this channel until someone unbans them."),
        )),
        _ => None,
    }
}

/// Whether `message` is drawn as "<message deleted>" instead of its text: it was removed, the
/// setting asks for collapsing, the user has not revealed it, and it is a chat line (an event card
/// such as a sub is never collapsed).
pub fn is_collapsed(message: &Message, mode: DeletedMessages, revealed: bool) -> bool {
    mode == DeletedMessages::Collapse
        && message.flags.deleted
        && !revealed
        && matches!(message.kind, MessageKind::Chat | MessageKind::Action)
}

#[cfg(test)]
mod tests;
