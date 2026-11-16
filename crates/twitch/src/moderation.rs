//! Moderation through Helix (PR 5.2): Twitch removed the IRC chat commands, so every timeout, ban,
//! room-mode change and the rest is a Helix request made as the logged-in user.
//!
//! [`build_request`] is the pure half: a [`ModCommand`] plus the ids it needs becomes the method,
//! path and JSON body of the right endpoint ([`required_scope`] says which OAuth scope it takes).
//! [`HelixClient::moderate`] is the I/O half: it checks the login has that scope (a login from
//! before the scope was asked for gets *"Log in again to grant …"*, not a failed request), looks
//! up the target user's id when the command names a login, sends the request and turns every way
//! it can fail into one [`ModError`] whose `Display` is the sentence shown to the user.
//!
//! What a moderation action *does* to the chat arrives over IRC (`CLEARCHAT`, `CLEARMSG`,
//! `ROOMSTATE`), the same way it does for actions taken on twitch.tv, so nothing here touches the
//! message store.

use gpui_chat_core::{ModCommand, RoomChange};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::auth::{AuthError, AuthState};
use crate::helix::{HelixClient, HelixError, HelixResponse, encode_query_value};

/// The user-facing line for a request that did not complete.
fn helix_message(error: &HelixError) -> String {
    match error {
        HelixError::Auth(AuthError::NotLoggedIn) => {
            "You are not logged in. Log in to moderate.".to_owned()
        }
        HelixError::Auth(AuthError::SessionEnded) => {
            "Your Twitch session ended. Log in again to moderate.".to_owned()
        }
        HelixError::Auth(other) => format!("Could not authorise the request: {other}."),
        HelixError::Network(why) => format!("Could not reach Twitch ({why})."),
    }
}

/// Why a moderation command did not go through. `Display` is written for the user.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModError {
    /// Not logged in, the login ended, or the request never completed.
    #[error("{}", helix_message(.0))]
    Helix(#[from] HelixError),
    /// The login was not granted the OAuth scope the command needs: moderation is enabled by a
    /// login that asks for it (account menu → *Enable moderation tools…*).
    #[error(
        "Log in again to enable moderation (grants {scope}): account menu → Enable moderation tools."
    )]
    MissingScope { scope: String },
    /// No Twitch account has that login.
    #[error("No Twitch user is called {0}.")]
    UnknownUser(String),
    /// Twitch refused the request, with its own message.
    #[error("{message}")]
    Rejected { status: u16, message: String },
    /// Too many requests; Twitch asked to wait.
    #[error("Twitch is rate limiting you. Try again in a moment.")]
    RateLimited,
}

impl ModError {
    /// Twitch answered 403: the user is not (or no longer) allowed to moderate here.
    pub fn is_forbidden(&self) -> bool {
        matches!(self, Self::Rejected { status: 403, .. })
    }
}

/// The OAuth scope `command` needs.
pub fn required_scope(command: &ModCommand) -> &'static str {
    match command {
        ModCommand::Timeout { .. } | ModCommand::Ban { .. } | ModCommand::Unban { .. } => {
            "moderator:manage:banned_users"
        }
        ModCommand::DeleteMessage { .. } | ModCommand::ClearChat => {
            "moderator:manage:chat_messages"
        }
        ModCommand::Room(_) => "moderator:manage:chat_settings",
        ModCommand::Announce { .. } => "moderator:manage:announcements",
        ModCommand::Shoutout { .. } => "moderator:manage:shoutouts",
        ModCommand::AddMod { .. } | ModCommand::RemoveMod { .. } => "channel:manage:moderators",
        ModCommand::AddVip { .. } | ModCommand::RemoveVip { .. } => "channel:manage:vips",
        ModCommand::Raid { .. } | ModCommand::Unraid => "channel:manage:raids",
    }
}

/// The ids a request is made with.
#[derive(Debug, Clone, Copy)]
pub struct Ids<'a> {
    /// The channel's room id.
    pub broadcaster: &'a str,
    /// The logged-in user.
    pub moderator: &'a str,
    /// The user the command is about ([`ModCommand::target`]), when it has one.
    pub target: Option<&'a str>,
}

/// A request to send: method, path below the Helix base (query included) and JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct ModRequest {
    pub method: Method,
    pub path: String,
    pub body: Option<Value>,
}

fn query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={}", encode_query_value(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The Helix request for `command`. `Err(UnknownUser)` when the command names a user and
/// `ids.target` is missing (the caller could not resolve the login).
pub fn build_request(command: &ModCommand, ids: Ids<'_>) -> Result<ModRequest, ModError> {
    let Ids {
        broadcaster,
        moderator,
        target,
    } = ids;
    let target_id = || {
        target.ok_or_else(|| ModError::UnknownUser(command.target().unwrap_or_default().to_owned()))
    };
    let as_moderator = [("broadcaster_id", broadcaster), ("moderator_id", moderator)];
    let request = |method, path: &str, pairs: &[(&str, &str)], body| ModRequest {
        method,
        path: format!("{path}?{}", query(pairs)),
        body,
    };
    Ok(match command {
        ModCommand::Timeout {
            seconds, reason, ..
        } => request(
            Method::POST,
            "moderation/bans",
            &as_moderator,
            Some(json!({ "data": with_reason(json!({
                "user_id": target_id()?,
                "duration": seconds,
            }), reason.as_deref()) })),
        ),
        ModCommand::Ban { reason, .. } => request(
            Method::POST,
            "moderation/bans",
            &as_moderator,
            Some(json!({ "data": with_reason(json!({
                "user_id": target_id()?,
            }), reason.as_deref()) })),
        ),
        ModCommand::Unban { .. } => request(
            Method::DELETE,
            "moderation/bans",
            &[
                ("broadcaster_id", broadcaster),
                ("moderator_id", moderator),
                ("user_id", target_id()?),
            ],
            None,
        ),
        ModCommand::DeleteMessage { message_id } => request(
            Method::DELETE,
            "moderation/chat",
            &[
                ("broadcaster_id", broadcaster),
                ("moderator_id", moderator),
                ("message_id", message_id),
            ],
            None,
        ),
        ModCommand::ClearChat => request(Method::DELETE, "moderation/chat", &as_moderator, None),
        ModCommand::Room(change) => request(
            Method::PATCH,
            "chat/settings",
            &as_moderator,
            Some(room_body(*change)),
        ),
        ModCommand::Announce { text } => request(
            Method::POST,
            "chat/announcements",
            &as_moderator,
            Some(json!({ "message": text, "color": "primary" })),
        ),
        ModCommand::Shoutout { .. } => request(
            Method::POST,
            "chat/shoutouts",
            &[
                ("from_broadcaster_id", broadcaster),
                ("to_broadcaster_id", target_id()?),
                ("moderator_id", moderator),
            ],
            None,
        ),
        ModCommand::AddMod { .. } => request(
            Method::POST,
            "moderation/moderators",
            &[("broadcaster_id", broadcaster), ("user_id", target_id()?)],
            None,
        ),
        ModCommand::RemoveMod { .. } => request(
            Method::DELETE,
            "moderation/moderators",
            &[("broadcaster_id", broadcaster), ("user_id", target_id()?)],
            None,
        ),
        ModCommand::AddVip { .. } => request(
            Method::POST,
            "channels/vips",
            &[("broadcaster_id", broadcaster), ("user_id", target_id()?)],
            None,
        ),
        ModCommand::RemoveVip { .. } => request(
            Method::DELETE,
            "channels/vips",
            &[("broadcaster_id", broadcaster), ("user_id", target_id()?)],
            None,
        ),
        ModCommand::Raid { .. } => request(
            Method::POST,
            "raids",
            &[
                ("from_broadcaster_id", broadcaster),
                ("to_broadcaster_id", target_id()?),
            ],
            None,
        ),
        ModCommand::Unraid => request(
            Method::DELETE,
            "raids",
            &[("broadcaster_id", broadcaster)],
            None,
        ),
    })
}

/// `body` with `reason` when there is one: an empty reason is left out, not sent as `""`.
fn with_reason(mut body: Value, reason: Option<&str>) -> Value {
    if let Some(reason) = reason.filter(|reason| !reason.is_empty()) {
        body["reason"] = json!(reason);
    }
    body
}

/// The `PATCH chat/settings` body for one change. Only the changed mode is sent.
fn room_body(change: RoomChange) -> Value {
    match change {
        RoomChange::Slow(Some(seconds)) => {
            json!({ "slow_mode": true, "slow_mode_wait_time": seconds })
        }
        RoomChange::Slow(None) => json!({ "slow_mode": false }),
        RoomChange::Followers(Some(minutes)) => {
            json!({ "follower_mode": true, "follower_mode_duration": minutes })
        }
        RoomChange::Followers(None) => json!({ "follower_mode": false }),
        RoomChange::Subscribers(on) => json!({ "subscriber_mode": on }),
        RoomChange::EmoteOnly(on) => json!({ "emote_mode": on }),
        RoomChange::Unique(on) => json!({ "unique_chat_mode": on }),
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
}

/// Maps a completed moderation exchange to its outcome.
fn interpret(response: &HelixResponse) -> Result<(), ModError> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }
    let twitch_message = serde_json::from_slice::<ErrorBody>(&response.body)
        .map(|body| body.message.trim().to_owned())
        .unwrap_or_default();
    match response.status {
        429 => Err(ModError::RateLimited),
        401 => match missing_scope(&twitch_message) {
            Some(scope) => Err(ModError::MissingScope { scope }),
            None => Err(ModError::Rejected {
                status: 401,
                message:
                    "Twitch did not accept your login for moderation. Log in again from the account menu."
                        .to_owned(),
            }),
        },
        status => {
            let message = if twitch_message.is_empty() {
                match status {
                    403 => "You are not allowed to do that in this channel.".to_owned(),
                    400 | 422 => "Twitch refused the request.".to_owned(),
                    status => format!("Twitch answered {status}."),
                }
            } else {
                twitch_message
            };
            Err(ModError::Rejected { status, message })
        }
    }
}

/// `Missing scope: moderator:manage:banned_users` → the scope.
fn missing_scope(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let after = &lower[lower.find("scope:")? + "scope:".len()..];
    let scope = after.split_whitespace().next()?.trim_end_matches('.');
    (!scope.is_empty()).then(|| scope.to_owned())
}

#[derive(Deserialize)]
struct Users {
    #[serde(default)]
    data: Vec<User>,
}

#[derive(Deserialize)]
struct User {
    id: String,
}

impl HelixClient {
    /// Runs `command` in the chat of `broadcaster_id` as the logged-in user. `target_id` is the
    /// id of the user the command names when the caller already knows it (a message's author);
    /// otherwise the login is looked up first.
    pub async fn moderate(
        &self,
        broadcaster_id: &str,
        command: &ModCommand,
        target_id: Option<&str>,
    ) -> Result<(), ModError> {
        let scope = required_scope(command);
        if let AuthState::LoggedIn(account) = self.auth().state()
            && !account.has_moderation_scope(scope)
        {
            return Err(ModError::MissingScope {
                scope: scope.to_owned(),
            });
        }
        let lease = self.auth().access_token().await.map_err(HelixError::from)?;
        let looked_up;
        let target = match (command.target(), target_id) {
            (None, _) => None,
            (Some(_), Some(id)) => Some(id),
            (Some(login), None) => {
                looked_up = self.user_id(login).await?;
                Some(looked_up.as_str())
            }
        };
        let request = build_request(
            command,
            Ids {
                broadcaster: broadcaster_id,
                moderator: &lease.user_id,
                target,
            },
        )?;
        let response = self
            .request_json(request.method, &request.path, request.body.as_ref())
            .await?;
        interpret(&response)
    }

    /// The Twitch user id of `login` (`GET users?login=`).
    async fn user_id(&self, login: &str) -> Result<String, ModError> {
        let response = self
            .get(&format!("users?{}", query(&[("login", login)])))
            .await?;
        interpret(&response)?;
        serde_json::from_slice::<Users>(&response.body)
            .ok()
            .and_then(|users| users.data.into_iter().next())
            .map(|user| user.id)
            .ok_or_else(|| ModError::UnknownUser(login.to_owned()))
    }
}

#[cfg(test)]
mod tests;
