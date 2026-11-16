//! [`LiveEmotes`]: which channels listen to what, and the connections behind it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use gpui_chat_core::{ChannelId, EmoteProvider};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use super::sessions::{BttvSession, SevenTvSession};
use super::seventv::Subscription;
use super::socket::{self, Backoff, Change};
use super::{bttv, seventv};
use crate::cosmetics::{CosmeticOptions, Cosmetics};
use crate::resolver::{EmoteRegistry, Providers};
use crate::set::{EmoteSet, SetOrigin};
use crate::seventv::SevenTvClient;

/// Where live changes go.
pub trait LiveSink: Send + Sync + 'static {
    /// A quiet line for `channel`'s chat ("7TV: added PogU").
    fn notice(&self, channel: &ChannelId, text: String);
    /// Paints or badges changed: the UI should repaint.
    fn cosmetics_changed(&self);
}

/// The websocket endpoints (tests point them at a local server).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub seven_tv: String,
    pub bttv: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            seven_tv: seventv::DEFAULT_URL.to_owned(),
            bttv: bttv::DEFAULT_URL.to_owned(),
        }
    }
}

/// What one open channel listens to.
#[derive(Debug, Clone)]
pub(crate) struct Route {
    pub room_id: String,
    pub set_id: Option<String>,
    pub owner_id: Option<String>,
    pub connection: Option<usize>,
}

#[derive(Default)]
struct State {
    routes: HashMap<ChannelId, Route>,
    emotes: Providers,
    cosmetics: CosmeticOptions,
    seven_current: HashSet<Subscription>,
    bttv_current: HashSet<String>,
}

pub(crate) struct Shared {
    state: Mutex<State>,
    pub registry: Arc<EmoteRegistry>,
    pub cosmetics: Arc<Cosmetics>,
    pub sink: Arc<dyn LiveSink>,
    seven_client: SevenTvClient,
    handle: Handle,
    seven_tx: mpsc::UnboundedSender<Change<Subscription>>,
    bttv_tx: mpsc::UnboundedSender<Change<String>>,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the connections listen to exactly what the open channels and switches call for.
    fn reconcile(&self) {
        let mut state = self.state();
        let mut seven = HashSet::new();
        let mut bttv = HashSet::new();
        for route in state.routes.values() {
            if state.emotes.seven_tv {
                seven.extend(route.set_id.iter().cloned().map(Subscription::EmoteSet));
                seven.extend(route.owner_id.iter().cloned().map(Subscription::User));
            }
            if state.cosmetics.wants_seventv() {
                seven.insert(Subscription::Cosmetics(route.room_id.clone()));
            }
            if state.emotes.bttv {
                bttv.insert(route.room_id.clone());
            }
        }
        for gone in state.seven_current.difference(&seven) {
            let _ = self.seven_tx.send(Change::Unsubscribe(gone.clone()));
        }
        for new in seven.difference(&state.seven_current) {
            let _ = self.seven_tx.send(Change::Subscribe(new.clone()));
        }
        for gone in state.bttv_current.difference(&bttv) {
            let _ = self.bttv_tx.send(Change::Unsubscribe(gone.clone()));
        }
        for new in bttv.difference(&state.bttv_current) {
            let _ = self.bttv_tx.send(Change::Subscribe(new.clone()));
        }
        state.seven_current = seven;
        state.bttv_current = bttv;
    }

    /// The open channels whose active 7TV set is `set_id`.
    pub(crate) fn channels_for_set(&self, set_id: &str) -> Vec<ChannelId> {
        self.state()
            .routes
            .iter()
            .filter(|(_, route)| route.set_id.as_deref() == Some(set_id))
            .map(|(channel, _)| channel.clone())
            .collect()
    }

    /// The open channels of a Twitch room.
    pub(crate) fn channels_for_room(&self, room_id: &str) -> Vec<ChannelId> {
        self.state()
            .routes
            .iter()
            .filter(|(_, route)| route.room_id == room_id)
            .map(|(channel, _)| channel.clone())
            .collect()
    }

    pub(crate) fn is_open(&self, channel: &ChannelId) -> bool {
        self.state().routes.contains_key(channel)
    }

    /// A 7TV user changed the set of one of their connections: every open channel they own
    /// follows (the set is fetched; the channel keeps its old one until it arrives).
    pub(crate) fn seven_set_switched(
        self: &Arc<Self>,
        user_id: &str,
        set_id: Option<String>,
        connection: Option<usize>,
    ) {
        let targets: Vec<ChannelId> = self
            .state()
            .routes
            .iter()
            .filter(|(_, route)| {
                route.owner_id.as_deref() == Some(user_id)
                    && route.set_id != set_id
                    && match (route.connection, connection) {
                        (Some(ours), Some(changed)) => ours == changed,
                        _ => true,
                    }
            })
            .map(|(channel, _)| channel.clone())
            .collect();
        for channel in targets {
            let Some(new_id) = set_id.clone() else {
                // The channel has no active set any more.
                self.registry.replace_channel_set(
                    &channel,
                    EmoteSet::empty(EmoteProvider::SevenTv, crate::EmoteScope::Channel),
                );
                if let Some(route) = self.state().routes.get_mut(&channel) {
                    route.set_id = None;
                }
                self.reconcile();
                self.sink
                    .notice(&channel, "7TV: emote set removed".to_owned());
                continue;
            };
            let this = self.clone();
            let owner = user_id.to_owned();
            drop(self.handle.spawn(async move {
                let connection = this.state().routes.get(&channel).and_then(|r| r.connection);
                match this.seven_client.set(&new_id, Some(&owner), connection).await {
                    Ok((set, name)) => {
                        {
                            let mut state = this.state();
                            // Closed or moved on meanwhile.
                            let Some(route) = state.routes.get_mut(&channel) else {
                                return;
                            };
                            if route.owner_id.as_deref() != Some(owner.as_str()) {
                                return;
                            }
                            route.set_id = Some(new_id.clone());
                        }
                        let count = set.len();
                        this.registry.replace_channel_set(&channel, set);
                        this.reconcile();
                        this.sink.notice(
                            &channel,
                            format!("7TV: switched to emote set {name} ({count} emotes)"),
                        );
                    }
                    Err(error) => {
                        tracing::warn!(%error, set = %new_id, "7TV: could not load the emote set the channel switched to");
                    }
                }
            }));
        }
    }
}

/// Live emote updates for the open channels. Dropping it closes the connections.
pub struct LiveEmotes {
    shared: Arc<Shared>,
}

impl LiveEmotes {
    /// Starts the two (idle) connection tasks on `handle`. Nothing connects until a channel is
    /// opened with a provider on.
    pub fn new(
        handle: &Handle,
        registry: Arc<EmoteRegistry>,
        cosmetics: Arc<Cosmetics>,
        seven_client: SevenTvClient,
        sink: Arc<dyn LiveSink>,
        endpoints: Endpoints,
        backoff: Backoff,
    ) -> Self {
        let (seven_tx, seven_rx) = mpsc::unbounded_channel();
        let (bttv_tx, bttv_rx) = mpsc::unbounded_channel();
        let state = State {
            emotes: registry.enabled(),
            cosmetics: cosmetics.options(),
            ..State::default()
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            registry,
            cosmetics,
            sink,
            seven_client,
            handle: handle.clone(),
            seven_tx,
            bttv_tx,
        });
        let weak = Arc::downgrade(&shared);
        drop(handle.spawn(socket::run(
            "7tv",
            endpoints.seven_tv,
            SevenTvSession::new(Weak::clone(&weak)),
            seven_rx,
            backoff,
        )));
        drop(handle.spawn(socket::run(
            "bttv",
            endpoints.bttv,
            BttvSession::new(weak),
            bttv_rx,
            backoff,
        )));
        Self { shared }
    }

    /// Starts listening for `channel` (Twitch room `room_id`). `origin` is the loaded 7TV set's
    /// ids, when the channel has one. Calling it again for a channel updates what it listens to.
    pub fn open_channel(&self, channel: &ChannelId, room_id: &str, origin: Option<&SetOrigin>) {
        let route = Route {
            room_id: room_id.to_owned(),
            set_id: origin.map(|o| o.set_id.to_string()),
            owner_id: origin.and_then(|o| o.owner_id.as_ref().map(ToString::to_string)),
            connection: origin.and_then(|o| o.connection),
        };
        self.shared.state().routes.insert(channel.clone(), route);
        self.shared.reconcile();
    }

    /// Whether `channel` is being listened for.
    pub fn is_open(&self, channel: &ChannelId) -> bool {
        self.shared.is_open(channel)
    }

    /// Stops listening for `channel`.
    pub fn close_channel(&self, channel: &ChannelId) {
        self.shared.state().routes.remove(channel);
        self.shared.reconcile();
    }

    /// Applies the provider and cosmetic switches: a provider that is off is not listened to
    /// (its connection closes once nothing else needs it).
    pub fn configure(&self, emotes: Providers, cosmetics: CosmeticOptions) {
        {
            let mut state = self.shared.state();
            if state.emotes == emotes && state.cosmetics == cosmetics {
                return;
            }
            state.emotes = emotes;
            state.cosmetics = cosmetics;
        }
        self.shared.reconcile();
    }
}

#[cfg(test)]
mod tests;
