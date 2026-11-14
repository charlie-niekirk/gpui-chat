//! The live Twitch source: one [`TwitchClient`] whose events go through the bridge, anonymous until
//! the user logs in (PR 3.2: then it reconnects with their token, and back to anonymous on logout).
//!
//! The channel list is the single source of truth for what is joined: the shell calls
//! [`LiveSource::join`] when a channel is opened (or restored) and [`LiveSource::part`] when it
//! is closed. The client keeps channels joined across reconnects and reports connection and
//! room state as ordinary [`ChatEvent`]s, so nothing here is specific to a view.
//!
//! `--firehose` and `--replay` *replace* this source (no client is started, nothing is joined and
//! the channel list is not saved), see `main::start_chat`.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui_chat_core::{Author, ChannelId, ChatEvent, Outgoing, Timestamp};
use gpui_chat_emotes::Providers;
use gpui_chat_twitch::auth::AuthState;
use gpui_chat_twitch::{
    Auth, ClientConfig, Credentials, CredentialsProvider, HelixClient, SendError, TwitchClient,
    echo_message, irc_login,
};
use gpui_kit::App;
use tokio::sync::{mpsc, oneshot};

use crate::badges::BadgeService;
use crate::bridge::BridgeSender;
use crate::config;
use crate::emote_service::{self, EmoteService};
use crate::runtime;

/// Handle to the running client; the shell owns it.
#[derive(Clone)]
pub struct LiveSource {
    client: TwitchClient,
    /// BTTV / FFZ / 7TV sets and their resolution in messages; `None` if it could not start.
    emotes: Option<EmoteService>,
    /// Channel badges (the global ones are bundled and need no service); `None` if it could not
    /// start.
    badges: Option<BadgeService>,
    /// The login owner and its Helix client; `None` if they could not be built (no sending).
    account: Option<(Auth, HelixClient)>,
    /// Where a sent message's echo goes: the same path as every incoming event.
    sender: BridgeSender<ChatEvent>,
}

/// What [`LiveSource::send`] needs to know about one message.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub channel: ChannelId,
    /// The channel's Twitch room (broadcaster) id, from its `ROOMSTATE`.
    pub room_id: String,
    pub outgoing: Outgoing,
}

impl LiveSource {
    /// Starts the client on the backend runtime and forwards its events to `sender`.
    /// `account` (the login owner and its Helix client) switches badges to Helix while logged in,
    /// makes the IRC connection use the user's token (a reconnect on login and logout) and lets
    /// [`LiveSource::send`] post messages.
    pub fn start(
        sender: BridgeSender<ChatEvent>,
        account: Option<(Auth, HelixClient)>,
        cx: &App,
    ) -> Self {
        let mut config = ClientConfig::default();
        if let Some((auth, _)) = &account {
            // Every connection attempt asks for the login as it is now, with a fresh token.
            let auth = auth.clone();
            config.credentials_provider = Some(CredentialsProvider::new(move || {
                let auth = auth.clone();
                async move { Credentials::from_auth(&auth).await }
            }));
        }
        let (client, events) = runtime::enter(cx, || TwitchClient::spawn(config));
        if let Some((auth, _)) = &account {
            drop(runtime::spawn_backend(
                cx,
                reconnect_on_login_change(auth.clone(), client.clone()),
            ));
        }
        let enabled = emote_service::providers(&config::current(cx).emotes);
        let emotes = EmoteService::start(enabled, cx);
        let badges = BadgeService::start(account.clone(), cx);
        // Dropped on purpose: the task ends when the client's event channel closes or the UI
        // side of the bridge is gone, and the runtime stops on quit.
        drop(runtime::spawn_backend(
            cx,
            forward(events, sender.clone(), emotes.clone(), badges.clone()),
        ));
        Self {
            client,
            emotes,
            badges,
            account,
            sender,
        }
    }

    /// Whether messages can be sent (a login owner exists; whether anyone is logged in is the
    /// account's state).
    pub fn can_send(&self) -> bool {
        self.account.is_some()
    }

    /// Posts a message through Helix as the logged-in user. The result arrives on the returned
    /// channel; on success the message has also been handed to the chat as an echo (carrying the id
    /// Twitch gave it, so an IRC copy of it is not shown twice). `None` without a login owner.
    pub fn send(
        &self,
        request: SendRequest,
        cx: &App,
    ) -> Option<oneshot::Receiver<Result<(), SendError>>> {
        let (auth, helix) = self.account.clone()?;
        let (sender, emotes) = (self.sender.clone(), self.emotes.clone());
        let (done, result) = oneshot::channel();
        drop(runtime::spawn_backend(cx, async move {
            let outcome = post(&auth, &helix, &sender, emotes.as_ref(), request).await;
            // The composer may be gone (pane closed): nobody to tell.
            let _ = done.send(outcome);
        }));
        Some(result)
    }

    pub fn join(&self, channel: &ChannelId) {
        self.client.join(channel);
    }

    pub fn part(&self, channel: &ChannelId) {
        self.client.part(channel);
        if let Some(emotes) = &self.emotes {
            emotes.forget(channel);
        }
        if let Some(badges) = &self.badges {
            badges.forget(channel);
        }
    }

    /// Fetches the global and every open channel's emote sets again (`Reload Emotes`).
    pub fn reload_emotes(&self) {
        if let Some(emotes) = &self.emotes {
            emotes.reload();
        }
    }

    /// Applies the `emotes.seventv` / `bttv` / `ffz` switches.
    pub fn set_emote_providers(&self, providers: Providers) {
        if let Some(emotes) = &self.emotes {
            emotes.set_providers(providers);
        }
    }
}

/// Sends `request` and, when Twitch accepted it, echoes it into the chat.
async fn post(
    auth: &Auth,
    helix: &HelixClient,
    sender: &BridgeSender<ChatEvent>,
    emotes: Option<&EmoteService>,
    request: SendRequest,
) -> Result<(), SendError> {
    let sent = helix
        .send_chat_message(&request.room_id, &request.outgoing.wire_text(), None)
        .await?;
    // Logged out while the request was in flight: it went out, but there is no one to show it as.
    if let Some(account) = auth.state().account() {
        let author = Author {
            user_id: Some(account.user_id.clone()),
            ..Author::new(&account.login, &account.login)
        };
        let message = echo_message(
            sent.message_id,
            author,
            &request.outgoing.text,
            request.outgoing.action,
            now(),
        );
        let event = ChatEvent::Message {
            channel: request.channel,
            message: std::sync::Arc::new(message),
        };
        let event = match emotes {
            Some(emotes) => emotes.process(event),
            None => event,
        };
        // A closed bridge means the UI is gone; the send itself succeeded.
        let _ = sender.send(event).await;
    }
    Ok(())
}

fn now() -> Timestamp {
    Timestamp::from_unix_millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64),
    )
}

/// Reconnects the client when the account IRC should log in as changes (login, logout, another
/// account), so it reads as the user right after logging in and goes anonymous again on logout.
/// The first connection already uses whatever the stored login turns out to be (the credentials
/// provider waits for it), so this only reacts to changes after that. Ends with the runtime.
async fn reconnect_on_login_change(auth: Auth, client: TwitchClient) {
    let mut states = auth.subscribe();
    let mut applied = {
        let settled = states
            .wait_for(|state| !matches!(state, AuthState::Restoring))
            .await;
        match settled {
            Ok(state) => irc_login(&state).map(str::to_owned),
            Err(_) => return,
        }
    };
    while states.changed().await.is_ok() {
        let now = irc_login(&states.borrow_and_update()).map(str::to_owned);
        if now != applied {
            tracing::info!(login = ?now, "chat login changed: reconnecting");
            applied = now;
            client.reconnect();
        }
    }
}

/// Moves events from the client into the bridge, resolving third-party emotes on the way (and
/// starting a channel's emote and badge loads when its room id shows up). `send().await` is the
/// backpressure: while the UI is behind, the client's own buffers fill and it sheds chat
/// messages, never state changes.
async fn forward(
    mut events: mpsc::Receiver<ChatEvent>,
    sender: BridgeSender<ChatEvent>,
    emotes: Option<EmoteService>,
    badges: Option<BadgeService>,
) {
    while let Some(event) = events.recv().await {
        if let Some(badges) = &badges {
            badges.observe(&event);
        }
        let event = match &emotes {
            Some(emotes) => emotes.process(event),
            None => event,
        };
        if sender.send(event).await.is_err() {
            // The UI is gone; dropping `events` shuts the client down.
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::ConnectionState;

    use super::*;
    use crate::bridge;

    #[tokio::test]
    async fn forward_passes_events_in_order_and_stops_when_the_ui_is_gone() {
        let (events_tx, events_rx) = mpsc::channel(8);
        let (sender, mut receiver) = bridge::channel(8, 8);
        let task = tokio::spawn(forward(events_rx, sender, None, None));
        let connection = |state| ChatEvent::Connection {
            channel: None,
            state,
        };
        events_tx
            .send(connection(ConnectionState::Connecting))
            .await
            .unwrap();
        events_tx
            .send(connection(ConnectionState::Connected))
            .await
            .unwrap();
        let first = receiver.first().await.unwrap();
        assert_eq!(
            receiver.drain(first),
            vec![
                connection(ConnectionState::Connecting),
                connection(ConnectionState::Connected)
            ]
        );
        drop(receiver);
        events_tx
            .send(connection(ConnectionState::Connecting))
            .await
            .unwrap();
        task.await.unwrap();
        assert!(events_tx.is_closed(), "the client sees the receiver gone");
    }
}
