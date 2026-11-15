//! Sending chat (PR 3.2): Helix *Send Chat Message* (`POST /helix/chat/messages`) and the local echo
//! of what was sent.
//!
//! [`HelixClient::send_chat_message`] posts the message as the logged-in user (scope
//! `user:write:chat`) and turns every way it can go wrong into one [`SendError`] whose `Display` is
//! the line the pane shows under the input:
//!
//! * `200` with `is_sent: false`: Twitch **dropped** the message and says why (`drop_reason`: a
//!   duplicate, the channel's chat modes, AutoMod, ...). [`SendError::Dropped`].
//! * `400` / `403` / `422`: the request was refused (banned, timed out, too long, chat disabled,
//!   ...) with Twitch's own message. [`SendError::Rejected`].
//! * `401`: the token was rejected even after the refresh [`HelixClient`] already tried, or lacks
//!   the scope: the user has to log in again. [`SendError::Rejected`].
//! * `429`: still rate limited after waiting for the reset. [`SendError::RateLimited`].
//! * No token, or the network failed: [`SendError::Helix`].
//!
//! Twitch does not necessarily deliver a user's own message back over IRC, so [`echo_message`]
//! builds the [`Message`] the app shows when Twitch accepted the send. It carries the id Twitch
//! returned, so if the same message does arrive over IRC the app can recognise it and show it once.

use gpui_chat_core::{Author, Message, MessageId, MessageKind, Timestamp, Tokenizer};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

use crate::auth::AuthError;
use crate::helix::{HelixClient, HelixError, HelixResponse};

/// The Helix path of Send Chat Message.
pub const SEND_CHAT_PATH: &str = "chat/messages";

/// A message Twitch accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    /// The id the message has in chat (what the IRC copy's `id` tag will be).
    pub message_id: String,
    /// The account the message was sent as: the one whose token made the request, whatever the
    /// login state is by the time the answer arrives.
    pub user_id: String,
    pub login: String,
}

/// Why a message did not go out. `Display` is written for the user.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SendError {
    /// Not logged in, the login ended, or the request never completed.
    #[error("{}", helix_message(.0))]
    Helix(#[from] HelixError),
    /// Twitch accepted the request but did not post the message.
    #[error("{message}")]
    Dropped { code: String, message: String },
    /// Twitch refused the request.
    #[error("{message}")]
    Rejected { status: u16, message: String },
    /// Too many messages; Twitch asked to wait.
    #[error("Twitch is rate limiting your messages. Try again in a moment.")]
    RateLimited,
    /// The reply was not what the endpoint documents.
    #[error("Twitch sent a reply this app does not understand.")]
    BadResponse,
}

/// The user-facing line for a request that did not complete.
fn helix_message(error: &HelixError) -> String {
    match error {
        HelixError::Auth(AuthError::NotLoggedIn) => {
            "You are not logged in. Log in to send messages.".to_owned()
        }
        HelixError::Auth(AuthError::SessionEnded) => {
            "Your Twitch session ended. Log in again to send messages.".to_owned()
        }
        HelixError::Auth(other) => format!("Could not authorise the message: {other}."),
        HelixError::Network(why) => format!("Could not reach Twitch ({why})."),
    }
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    data: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    is_sent: bool,
    drop_reason: Option<DropReason>,
}

#[derive(Deserialize)]
struct DropReason {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
}

/// Maps a completed Send Chat Message exchange to its outcome.
fn interpret(response: &HelixResponse) -> Result<String, SendError> {
    let twitch_message = || {
        serde_json::from_slice::<ErrorBody>(&response.body)
            .map(|body| body.message.trim().to_owned())
            .unwrap_or_default()
    };
    match response.status {
        200 => {
            let reply: Reply =
                serde_json::from_slice(&response.body).map_err(|_| SendError::BadResponse)?;
            let item = reply
                .data
                .into_iter()
                .next()
                .ok_or(SendError::BadResponse)?;
            if item.is_sent {
                return Ok(item.message_id);
            }
            let (code, message) = item
                .drop_reason
                .map_or_else(Default::default, |reason| (reason.code, reason.message));
            let message = match (message.trim(), code.as_str()) {
                ("", "") => "Twitch did not post the message.".to_owned(),
                ("", code) => format!("Twitch did not post the message ({code})."),
                (message, _) => message.to_owned(),
            };
            Err(SendError::Dropped { code, message })
        }
        429 => Err(SendError::RateLimited),
        401 => Err(SendError::Rejected {
            status: 401,
            message:
                "Twitch did not accept your login for chatting. Log in again from the account menu."
                    .to_owned(),
        }),
        status => {
            let message = twitch_message();
            let message = if message.is_empty() {
                match status {
                    403 => "You are not allowed to chat in this channel.".to_owned(),
                    400 | 422 => "Twitch refused the message.".to_owned(),
                    status => format!("Twitch answered {status}."),
                }
            } else {
                message
            };
            Err(SendError::Rejected { status, message })
        }
    }
}

impl HelixClient {
    /// Sends `message` to the chat of `broadcaster_id` as the logged-in user. `reply_parent` is
    /// the id of the message being replied to (PR 3.4).
    ///
    /// The text goes as it is: an action is `/me <text>` (see
    /// [`gpui_chat_core::Outgoing::wire_text`]).
    pub async fn send_chat_message(
        &self,
        broadcaster_id: &str,
        message: &str,
        reply_parent: Option<&str>,
    ) -> Result<SentMessage, SendError> {
        let lease = self.auth().access_token().await.map_err(HelixError::from)?;
        let mut body = json!({
            "broadcaster_id": broadcaster_id,
            "sender_id": lease.user_id,
            "message": message,
        });
        if let Some(parent) = reply_parent {
            body["reply_parent_message_id"] = json!(parent);
        }
        let response = self.post_json(SEND_CHAT_PATH, &body).await?;
        Ok(SentMessage {
            message_id: interpret(&response)?,
            user_id: lease.user_id,
            login: lease.login,
        })
    }
}

/// The line chat shows for a message the user just sent: `text` tokenized the way an incoming
/// message is (links, mentions, emoji; third-party emotes are resolved by the app afterwards, like
/// for every message), `id` as returned by Twitch. It is flagged [`gpui_chat_core::MessageFlags::echo`]:
/// the copy Twitch delivers over IRC replaces it. `author` is what is known about the user (the
/// IRC copy would also carry their colour and badges).
pub fn echo_message(
    id: impl Into<MessageId>,
    author: Author,
    text: &str,
    action: bool,
    timestamp: Timestamp,
) -> Message {
    let fragments = Tokenizer::new().tokenize(text, &[]).fragments;
    let mut message = Message::chat(id, timestamp, author, fragments);
    message.flags.echo = true;
    if action {
        message.kind = MessageKind::Action;
    }
    message
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::Fragment;

    use super::*;

    fn response(status: u16, body: serde_json::Value) -> HelixResponse {
        HelixResponse {
            status,
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    #[test]
    fn an_accepted_message_returns_its_id() {
        let ok = response(
            200,
            json!({ "data": [{ "message_id": "abc-123", "is_sent": true }] }),
        );
        assert_eq!(interpret(&ok), Ok("abc-123".to_owned()));
    }

    #[test]
    fn a_drop_says_why() {
        let dropped = response(
            200,
            json!({ "data": [{
                "message_id": "",
                "is_sent": false,
                "drop_reason": { "code": "msg_duplicate", "message": "Your message was not sent because it is identical to the previous one you sent." }
            }] }),
        );
        let error = interpret(&dropped).unwrap_err();
        assert_eq!(
            error,
            SendError::Dropped {
                code: "msg_duplicate".into(),
                message: "Your message was not sent because it is identical to the previous one you sent.".into()
            }
        );
        assert!(error.to_string().starts_with("Your message was not sent"));
    }

    #[test]
    fn a_drop_without_text_falls_back_to_the_code() {
        let coded = response(
            200,
            json!({ "data": [{ "is_sent": false, "drop_reason": { "code": "channel_settings" } }] }),
        );
        assert_eq!(
            interpret(&coded).unwrap_err().to_string(),
            "Twitch did not post the message (channel_settings)."
        );
        let bare = response(200, json!({ "data": [{ "is_sent": false }] }));
        assert_eq!(
            interpret(&bare).unwrap_err().to_string(),
            "Twitch did not post the message."
        );
    }

    #[test]
    fn refusals_carry_twitchs_own_message() {
        let banned = response(
            403,
            json!({ "error": "Forbidden", "status": 403, "message": "The user is banned from this channel." }),
        );
        assert_eq!(
            interpret(&banned),
            Err(SendError::Rejected {
                status: 403,
                message: "The user is banned from this channel.".into()
            })
        );
        let empty = HelixResponse {
            status: 403,
            body: Vec::new(),
        };
        assert_eq!(
            interpret(&empty).unwrap_err().to_string(),
            "You are not allowed to chat in this channel."
        );
        let teapot = HelixResponse {
            status: 503,
            body: b"<html>".to_vec(),
        };
        assert_eq!(
            interpret(&teapot).unwrap_err().to_string(),
            "Twitch answered 503."
        );
    }

    #[test]
    fn a_401_after_the_refresh_asks_for_a_new_login() {
        let rejected = response(
            401,
            json!({ "status": 401, "message": "Missing scope: user:write:chat" }),
        );
        let error = interpret(&rejected).unwrap_err();
        assert!(matches!(error, SendError::Rejected { status: 401, .. }));
        assert!(error.to_string().contains("Log in again"));
    }

    #[test]
    fn a_request_that_did_not_complete_reads_as_a_sentence() {
        let logged_out = SendError::Helix(HelixError::Auth(AuthError::NotLoggedIn));
        assert_eq!(
            logged_out.to_string(),
            "You are not logged in. Log in to send messages."
        );
        let ended = SendError::Helix(HelixError::Auth(AuthError::SessionEnded));
        assert!(ended.to_string().contains("Log in again"));
        let offline = SendError::Helix(HelixError::Network("dns failure".into()));
        assert_eq!(offline.to_string(), "Could not reach Twitch (dns failure).");
    }

    #[test]
    fn rate_limits_and_garbage() {
        assert_eq!(
            interpret(&response(429, json!({}))),
            Err(SendError::RateLimited)
        );
        for body in [json!({ "data": [] }), json!({ "nope": 1 })] {
            assert_eq!(interpret(&response(200, body)), Err(SendError::BadResponse));
        }
        let not_json = HelixResponse {
            status: 200,
            body: b"nope".to_vec(),
        };
        assert_eq!(interpret(&not_json), Err(SendError::BadResponse));
    }

    #[test]
    fn the_echo_is_a_tokenized_chat_or_action_line_with_the_given_id() {
        let author = Author::new("viewer", "viewer");
        let chat = echo_message(
            "id-1",
            author.clone(),
            "hi https://example.com",
            false,
            Timestamp(5),
        );
        assert_eq!(chat.id.as_str(), "id-1");
        assert_eq!(chat.kind, MessageKind::Chat);
        assert_eq!(chat.timestamp, Timestamp(5));
        assert!(
            chat.fragments
                .iter()
                .any(|f| matches!(f, Fragment::Link { .. }))
        );
        assert!(!chat.flags.mention);
        assert!(chat.flags.echo, "an echo says it is one");
        let action = echo_message("id-2", author, "waves", true, Timestamp(6));
        assert_eq!(action.kind, MessageKind::Action);
        assert_eq!(action.author.as_ref().unwrap().login, "viewer");
    }
}
