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

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_chat_core::{Author, ChannelId, ChatEvent, Outgoing, Timestamp};
use gpui_chat_emotes::Providers;
use gpui_chat_twitch::auth::AuthState;
use gpui_chat_twitch::{
    Auth, ClientConfig, Credentials, CredentialsProvider, HelixClient, SendError, TwitchClient,
    echo_message, irc_login, settled_state,
};
use gpui_kit::App;
use tokio::sync::{mpsc, oneshot, watch};

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
        let connected_as = ConnectedAs::default();
        if let Some((auth, _)) = &account {
            // Every connection attempt asks for the login as it is now, with a fresh token, and
            // notes which login it connected as for `follow_login`.
            let (auth, noted) = (auth.clone(), connected_as.clone());
            config.credentials_provider = Some(CredentialsProvider::new(move |refusals| {
                let (auth, connected_as) = (auth.clone(), noted.clone());
                async move {
                    // What the login state asks for, noted before the (possibly slow) token fetch
                    // so the watcher sees no difference while the connection is being made.
                    connected_as.set(irc_login(&settled_state(&auth).await).map(str::to_owned));
                    let credentials = Credentials::from_auth(&auth, refusals).await;
                    // What it actually got (anonymous if the token could not be had).
                    connected_as
                        .set((!credentials.is_anonymous()).then(|| credentials.login.clone()));
                    credentials
                }
            }));
        }
        let (client, events) = runtime::enter(cx, || TwitchClient::spawn(config));
        if let Some((auth, _)) = &account {
            let reconnecting = client.clone();
            drop(runtime::spawn_backend(
                cx,
                follow_login(auth.subscribe(), connected_as, move || {
                    reconnecting.reconnect();
                }),
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
        let (_, helix) = self.account.clone()?;
        let (sender, emotes) = (self.sender.clone(), self.emotes.clone());
        let (done, result) = oneshot::channel();
        drop(runtime::spawn_backend(cx, async move {
            let outcome = post(&helix, &sender, emotes.as_ref(), request).await;
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

/// Sends `request` and, when Twitch accepted it, echoes it into the chat as the account that sent
/// it (taken from the request's own token, so a login switch while it was in flight cannot
/// misattribute it).
async fn post(
    helix: &HelixClient,
    sender: &BridgeSender<ChatEvent>,
    emotes: Option<&EmoteService>,
    request: SendRequest,
) -> Result<(), SendError> {
    let sent = helix
        .send_chat_message(&request.room_id, &request.outgoing.wire_text(), None)
        .await?;
    let author = Author {
        user_id: Some(sent.user_id),
        ..Author::new(&sent.login, &sent.login)
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
    Ok(())
}

fn now() -> Timestamp {
    Timestamp::from_unix_millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64),
    )
}

/// The login the IRC connection was last set up as (`None`: anonymous), written by the credentials
/// provider on every connection attempt and read by [`follow_login`].
#[derive(Clone, Default)]
struct ConnectedAs(Arc<Mutex<Option<String>>>);

impl ConnectedAs {
    fn set(&self, login: Option<String>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = login;
    }

    fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Calls `reconnect` whenever the account IRC should log in as differs from the one it is
/// connected as: login, logout, another user. Compares against what the connection actually used
/// (`connected_as`), not against an earlier look at the login state, so a first connection that
/// went anonymous because the keychain was slow to answer is corrected as soon as the login
/// arrives. Nothing happens while the stored login is still being read. Ends with the runtime.
async fn follow_login(
    mut states: watch::Receiver<AuthState>,
    connected_as: ConnectedAs,
    reconnect: impl Fn(),
) {
    loop {
        let wanted = {
            let state = states.borrow_and_update();
            (!matches!(*state, AuthState::Restoring)).then(|| irc_login(&state).map(str::to_owned))
        };
        if let Some(wanted) = wanted
            && connected_as.get() != wanted
        {
            tracing::info!(login = ?wanted, "chat login changed: reconnecting");
            // Asked for; the provider notes what it really got when the attempt starts.
            connected_as.set(wanted);
            reconnect();
        }
        if states.changed().await.is_err() {
            return;
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use gpui_chat_core::ConnectionState;
    use gpui_chat_twitch::auth::Account;

    use super::*;
    use crate::bridge;

    fn logged_in(login: &str, missing: &[&str]) -> AuthState {
        AuthState::LoggedIn(Account {
            login: login.into(),
            user_id: "1".into(),
            missing_scopes: missing.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    /// Runs `follow_login` over a scripted login state; `reconnects` counts its calls.
    struct Follow {
        states: watch::Sender<AuthState>,
        connected_as: ConnectedAs,
        reconnects: Arc<AtomicUsize>,
    }

    impl Follow {
        fn start(initial: AuthState, connected_as: Option<&str>) -> Self {
            let (states, receiver) = watch::channel(initial);
            let connected = ConnectedAs::default();
            connected.set(connected_as.map(str::to_owned));
            let reconnects = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&reconnects);
            tokio::spawn(follow_login(receiver, connected.clone(), move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }));
            Self {
                states,
                connected_as: connected,
                reconnects,
            }
        }

        async fn settle(&self) -> usize {
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.reconnects.load(Ordering::SeqCst)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_login_that_arrives_after_an_anonymous_first_connection_reconnects() {
        // The keychain was slow: the restore wait ran out and the client connected anonymously.
        let follow = Follow::start(AuthState::Restoring, None);
        assert_eq!(follow.settle().await, 0, "still reading the stored login");
        follow.states.send_replace(logged_in("viewer", &[]));
        assert_eq!(follow.settle().await, 1, "the login arrived: log chat in");
        // The provider notes what the new connection used; no further reconnects.
        follow.connected_as.set(Some("viewer".into()));
        follow
            .states
            .send_replace(logged_in("viewer", &["user:read:follows"]));
        assert_eq!(follow.settle().await, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_already_uses_the_login_is_left_alone() {
        let follow = Follow::start(logged_in("viewer", &[]), Some("viewer"));
        assert_eq!(follow.settle().await, 0);
        // The login settling while the first connection is being made, as noted by the provider.
        let follow = Follow::start(AuthState::Restoring, Some("viewer"));
        follow.states.send_replace(logged_in("viewer", &[]));
        assert_eq!(follow.settle().await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn logout_and_another_account_reconnect_once_each() {
        let follow = Follow::start(logged_in("viewer", &[]), Some("viewer"));
        follow
            .states
            .send_replace(AuthState::LoggedOut { notice: None });
        assert_eq!(follow.settle().await, 1);
        assert_eq!(follow.connected_as.get(), None);
        follow.states.send_replace(logged_in("other", &[]));
        assert_eq!(follow.settle().await, 2);
        // The same state again (a notice dismissed, a re-login as the same user) changes nothing.
        follow.states.send_replace(logged_in("other", &[]));
        assert_eq!(follow.settle().await, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_login_without_chat_read_stays_anonymous() {
        let follow = Follow::start(AuthState::Restoring, None);
        follow
            .states
            .send_replace(logged_in("viewer", &["chat:read"]));
        assert_eq!(follow.settle().await, 0);
    }

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
