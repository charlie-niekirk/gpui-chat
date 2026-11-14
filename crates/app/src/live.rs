//! The live Twitch source: one anonymous [`TwitchClient`] whose events go through the bridge.
//!
//! The channel list is the single source of truth for what is joined: the shell calls
//! [`LiveSource::join`] when a channel is opened (or restored) and [`LiveSource::part`] when it
//! is closed. The client keeps channels joined across reconnects and reports connection and
//! room state as ordinary [`ChatEvent`]s, so nothing here is specific to a view.
//!
//! `--firehose` and `--replay` *replace* this source (no client is started, nothing is joined and
//! the channel list is not saved), see `main::start_chat`.

use gpui_chat_core::{ChannelId, ChatEvent};
use gpui_chat_emotes::Providers;
use gpui_chat_twitch::{ClientConfig, TwitchClient};
use gpui_kit::App;
use tokio::sync::mpsc;

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
}

impl LiveSource {
    /// Starts an anonymous client on the backend runtime and forwards its events to `sender`.
    pub fn start(sender: BridgeSender<ChatEvent>, cx: &App) -> Self {
        let (client, events) = runtime::enter(cx, || TwitchClient::spawn(ClientConfig::default()));
        let enabled = emote_service::providers(&config::current(cx).emotes);
        let emotes = EmoteService::start(enabled, cx);
        // Dropped on purpose: the task ends when the client's event channel closes or the UI
        // side of the bridge is gone, and the runtime stops on quit.
        drop(runtime::spawn_backend(
            cx,
            forward(events, sender, emotes.clone()),
        ));
        Self { client, emotes }
    }

    pub fn join(&self, channel: &ChannelId) {
        self.client.join(channel);
    }

    pub fn part(&self, channel: &ChannelId) {
        self.client.part(channel);
        if let Some(emotes) = &self.emotes {
            emotes.forget(channel);
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

/// Moves events from the client into the bridge, resolving third-party emotes on the way (and
/// starting a channel's emote load when its room id shows up). `send().await` is the
/// backpressure: while the UI is behind, the client's own buffers fill and it sheds chat
/// messages, never state changes.
async fn forward(
    mut events: mpsc::Receiver<ChatEvent>,
    sender: BridgeSender<ChatEvent>,
    emotes: Option<EmoteService>,
) {
    while let Some(event) = events.recv().await {
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
        let task = tokio::spawn(forward(events_rx, sender, None));
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
