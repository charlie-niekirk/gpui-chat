//! Third-party emotes for the live source (PR 2.4): loading BTTV / FFZ / 7TV sets and resolving
//! them in chat messages.
//!
//! * The **global** sets load at startup. A channel's sets load when its `ROOMSTATE` supplies the
//!   room id (the Twitch user id the providers are keyed by); a room id seen again does not load
//!   again, a changed one does.
//! * **Failures are retried.** Each load is `Complete` only if every requested provider answered.
//!   Incomplete global sets are requested again whenever the connection reports `Connected` (the
//!   first connect and every reconnect), and a channel whose last load was incomplete is requested
//!   again when its `ROOMSTATE` repeats (a reconnect re-joins and so repeats it). A load still in
//!   flight is never duplicated.
//! * Every event on its way from the Twitch client to the UI passes [`EmoteService::process`]: a
//!   message's text runs are re-scanned with the channel's [`EmoteResolver`] (7TV → BTTV → FFZ,
//!   channel before global) and words that name an emote become `Fragment::Emote`s, zero-width
//!   ones stacked on the emote before them (`gpui_chat_core::resolve_fragments`). This runs on the
//!   backend thread, not the UI's. Messages that arrive before the sets have loaded keep their
//!   words as text, as do messages already in a store when a set finishes loading.
//! * Per-provider switches (`emotes.seventv` / `bttv` / `ffz`) apply to the next message; turning
//!   one on reloads, since its sets may never have been fetched, and a provider that is off is
//!   never requested. [`EmoteService::reload`] (the `Reload Emotes` action) fetches everything
//!   again; a provider that fails keeps the sets it had.
//!
//! Offline sources (`--firehose`, `--replay`) have no service: nothing is fetched.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use gpui_chat_core::{
    ChannelId, ChatEvent, ConnectionState, Message, MessageKind, resolve_fragments,
};
use gpui_chat_emotes::{EmoteLoader, EmoteRegistry, Providers};
use gpui_chat_settings::EmoteSettings;
use gpui_kit::App;
use tokio::runtime::Handle;

use crate::runtime;

/// The providers `settings` asks for.
pub fn providers(settings: &EmoteSettings) -> Providers {
    Providers {
        seven_tv: settings.seventv,
        bttv: settings.bttv,
        ffz: settings.ffz,
    }
}

/// How far a set of loads got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Load {
    /// Not requested yet, or the last attempt had a provider error.
    Incomplete,
    InFlight,
    /// Every requested provider answered.
    Complete,
}

/// A load's state and the number of the latest attempt: a finishing attempt only records its
/// outcome if no newer one was started meanwhile.
#[derive(Clone, Copy, Debug)]
struct Attempt {
    load: Load,
    token: u64,
}

#[derive(Debug)]
struct Room {
    room_id: String,
    attempt: Attempt,
}

struct State {
    next_token: u64,
    global: Attempt,
    rooms: HashMap<ChannelId, Room>,
}

impl State {
    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }
}

struct Inner {
    loader: EmoteLoader,
    registry: EmoteRegistry,
    handle: Handle,
    state: Mutex<State>,
}

impl Inner {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Cheap to clone; all clones share one registry.
#[derive(Clone)]
pub struct EmoteService {
    inner: Arc<Inner>,
}

impl EmoteService {
    /// A service over `loader` that runs its requests on `handle`. Loads nothing yet.
    pub fn new(loader: EmoteLoader, enabled: Providers, handle: Handle) -> Self {
        Self {
            inner: Arc::new(Inner {
                loader,
                registry: EmoteRegistry::new(enabled),
                handle,
                state: Mutex::new(State {
                    next_token: 0,
                    global: Attempt {
                        load: Load::Incomplete,
                        token: 0,
                    },
                    rooms: HashMap::new(),
                }),
            }),
        }
    }

    /// Starts the service on the backend runtime and begins loading the global sets. `None` if
    /// the HTTP client cannot be built (emotes are then plain text).
    pub fn start(enabled: Providers, cx: &App) -> Option<Self> {
        let loader = match runtime::enter(cx, EmoteLoader::with_default_http) {
            Ok(loader) => loader,
            Err(error) => {
                tracing::warn!(%error, "third-party emotes unavailable");
                return None;
            }
        };
        let service = Self::new(loader, enabled, cx.global::<runtime::Backend>().handle());
        service.load_global();
        Some(service)
    }

    /// Fetches the global sets now, whatever their state.
    fn load_global(&self) {
        let token = {
            let mut state = self.inner.state();
            let token = state.token();
            state.global = Attempt {
                load: Load::InFlight,
                token,
            };
            token
        };
        let inner = self.inner.clone();
        drop(self.inner.handle.spawn(async move {
            let results = inner
                .loader
                .load_global_with(inner.registry.enabled())
                .await;
            inner.registry.store_global(&results);
            let mut state = inner.state();
            if state.global.token == token {
                state.global.load = if results.all_ok() {
                    Load::Complete
                } else {
                    Load::Incomplete
                };
            }
        }));
    }

    /// Fetches `channel`'s sets for `room_id` now, whatever their state.
    fn load_channel(&self, channel: ChannelId, room_id: String) {
        let token = {
            let mut state = self.inner.state();
            let token = state.token();
            state.rooms.insert(
                channel.clone(),
                Room {
                    room_id: room_id.clone(),
                    attempt: Attempt {
                        load: Load::InFlight,
                        token,
                    },
                },
            );
            token
        };
        let inner = self.inner.clone();
        drop(self.inner.handle.spawn(async move {
            let results = inner
                .loader
                .load_channel_with(&room_id, inner.registry.enabled())
                .await;
            // The channel may have been closed (or its room id changed) while this loaded.
            let mut state = inner.state();
            let Some(room) = state.rooms.get_mut(&channel) else {
                return;
            };
            if room.room_id != room_id {
                return;
            }
            inner.registry.store_channel(&channel, &results);
            if room.attempt.token == token {
                room.attempt.load = if results.all_ok() {
                    Load::Complete
                } else {
                    Load::Incomplete
                };
            }
        }));
    }

    /// Fetches the global sets and every open channel's sets again.
    pub fn reload(&self) {
        self.load_global();
        let rooms: Vec<(ChannelId, String)> = self
            .inner
            .state()
            .rooms
            .iter()
            .map(|(channel, room)| (channel.clone(), room.room_id.clone()))
            .collect();
        for (channel, room_id) in rooms {
            self.load_channel(channel, room_id);
        }
    }

    /// Applies the provider switches; turning a provider on reloads.
    pub fn set_providers(&self, providers: Providers) {
        let before = self.inner.registry.enabled();
        if before == providers {
            return;
        }
        self.inner.registry.set_enabled(providers);
        if providers.gains_over(before) {
            self.reload();
        }
    }

    /// The channel was closed: forget its room and sets.
    pub fn forget(&self, channel: &ChannelId) {
        self.inner.state().rooms.remove(channel);
        self.inner.registry.forget_channel(channel);
    }

    /// Passes `event` on: a `ROOMSTATE` with a new room id (or repeating one whose last load was
    /// incomplete) starts loading the channel's sets, `Connected` retries incomplete global sets,
    /// a chat message has its emotes resolved.
    pub fn process(&self, event: ChatEvent) -> ChatEvent {
        match event {
            ChatEvent::RoomState { channel, update } => {
                if let Some(room_id) = &update.room_id {
                    let needed = match self.inner.state().rooms.get(&channel) {
                        Some(room) => {
                            room.room_id != *room_id || room.attempt.load == Load::Incomplete
                        }
                        None => true,
                    };
                    if needed {
                        self.load_channel(channel.clone(), room_id.clone());
                    }
                }
                ChatEvent::RoomState { channel, update }
            }
            ChatEvent::Message { channel, message } => {
                let message = self.resolve(&channel, message);
                ChatEvent::Message { channel, message }
            }
            ChatEvent::Connection {
                channel: None,
                state: ConnectionState::Connected,
            } => {
                if self.inner.state().global.load == Load::Incomplete {
                    self.load_global();
                }
                ChatEvent::Connection {
                    channel: None,
                    state: ConnectionState::Connected,
                }
            }
            other => other,
        }
    }

    /// `message` with its third-party emotes resolved, or the same message if there are none.
    fn resolve(&self, channel: &ChannelId, message: Arc<Message>) -> Arc<Message> {
        if !matches!(
            message.kind,
            MessageKind::Chat | MessageKind::Action | MessageKind::UserNotice
        ) {
            return message;
        }
        let resolver = self.inner.registry.resolver(channel);
        if resolver.is_empty() {
            return message;
        }
        match resolve_fragments(&message.fragments, &resolver) {
            Some(fragments) => Arc::new(Message {
                fragments,
                ..(*message).clone()
            }),
            None => message,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use gpui_chat_core::{Author, Fragment, RoomStateUpdate, Timestamp, Tokenizer};
    use gpui_chat_emotes::{HttpClient, HttpError, HttpFuture, HttpResponse};

    use super::*;

    /// Answers from `fixtures/emotes/`, recording requests; every request fails while `down`.
    #[derive(Default)]
    struct Fixtures {
        replies: HashMap<&'static str, &'static str>,
        requests: Mutex<Vec<String>>,
        down: std::sync::atomic::AtomicBool,
    }

    impl HttpClient for Fixtures {
        fn get<'a>(&'a self, url: &'a str) -> HttpFuture<'a> {
            self.requests.lock().unwrap().push(url.to_owned());
            let down = self.down.load(std::sync::atomic::Ordering::SeqCst);
            let file = self.replies.get(url).copied().filter(|_| !down);
            Box::pin(async move {
                match file {
                    Some(file) => {
                        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                            .join("../../fixtures/emotes")
                            .join(file);
                        Ok(HttpResponse {
                            status: 200,
                            body: std::fs::read(path).unwrap(),
                        })
                    }
                    None => Err(HttpError::Network("no reply".into())),
                }
            })
        }
    }

    fn fixtures() -> Arc<Fixtures> {
        Arc::new(Fixtures {
            replies: HashMap::from([
                ("https://7tv.io/v3/emote-sets/global", "7tv-global.json"),
                (
                    "https://api.betterttv.net/3/cached/emotes/global",
                    "bttv-global.json",
                ),
                (
                    "https://api.frankerfacez.com/v1/set/global",
                    "ffz-global.json",
                ),
                (
                    "https://7tv.io/v3/users/twitch/11148817",
                    "7tv-user-pajlada.json",
                ),
                (
                    "https://api.betterttv.net/3/cached/users/twitch/11148817",
                    "bttv-channel-pajlada.json",
                ),
                (
                    "https://api.frankerfacez.com/v1/room/id/11148817",
                    "ffz-room-pajlada.json",
                ),
            ]),
            requests: Mutex::default(),
            down: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn set_down(http: &Fixtures, down: bool) {
        http.down.store(down, std::sync::atomic::Ordering::SeqCst);
    }

    fn connected() -> ChatEvent {
        ChatEvent::Connection {
            channel: None,
            state: ConnectionState::Connected,
        }
    }

    fn request_count(http: &Fixtures) -> usize {
        http.requests.lock().unwrap().len()
    }

    fn room(channel: &str, room_id: &str) -> ChatEvent {
        ChatEvent::RoomState {
            channel: ChannelId::new(channel),
            update: RoomStateUpdate {
                room_id: Some(room_id.into()),
                ..RoomStateUpdate::default()
            },
        }
    }

    fn message(channel: &str, text: &str) -> ChatEvent {
        let fragments = Tokenizer::new().tokenize(text, &[]).fragments;
        ChatEvent::Message {
            channel: ChannelId::new(channel),
            message: Arc::new(Message::chat(
                "1",
                Timestamp(0),
                Author::new("al", "Al"),
                fragments,
            )),
        }
    }

    /// Emote names (base, overlays) per emote fragment of a message event.
    fn emotes(event: &ChatEvent) -> Vec<(String, Vec<String>)> {
        let ChatEvent::Message { message, .. } = event else {
            panic!("not a message");
        };
        message
            .fragments
            .iter()
            .filter_map(|f| match f {
                Fragment::Emote {
                    emote,
                    zero_width_overlays,
                } => Some((
                    emote.name.to_string(),
                    zero_width_overlays
                        .iter()
                        .map(|e| e.name.to_string())
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    /// Waits until `check` holds (loads run on the runtime's threads).
    async fn eventually(mut check: impl FnMut() -> bool) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached");
    }

    fn service(http: Arc<Fixtures>, enabled: Providers) -> EmoteService {
        EmoteService::new(EmoteLoader::new(http), enabled, Handle::current())
    }

    #[tokio::test]
    async fn room_state_loads_the_channel_and_zero_width_emotes_stack() {
        let http = fixtures();
        let service = service(http.clone(), Providers::ALL);
        service.load_global();
        // Nothing loaded yet: the words stay text.
        let before = service.process(message("pajlada", "NERD ShowerTime"));
        assert!(emotes(&before).is_empty());

        assert!(matches!(
            service.process(room("pajlada", "11148817")),
            ChatEvent::RoomState { .. }
        ));
        eventually(|| {
            let after = service.process(message("pajlada", "hi NERD ShowerTime RainTime end"));
            !emotes(&after).is_empty()
        })
        .await;
        // The channel's 7TV `ShowerTime` and the global 7TV `RainTime` both stack on NERD, in
        // order; they are zero-width, NERD is not.
        eventually(|| {
            emotes(&service.process(message("pajlada", "NERD ShowerTime RainTime")))
                == [(
                    "NERD".to_owned(),
                    vec!["ShowerTime".to_owned(), "RainTime".to_owned()],
                )]
        })
        .await;
        // The same words in a channel with no sets of its own: only the global ones resolve (and
        // RainTime, zero-width, has nothing to stack on, so it is an emote of its own).
        let other = service.process(message("other", "NERD RainTime"));
        assert_eq!(emotes(&other), [("RainTime".to_owned(), vec![])]);
    }

    #[tokio::test]
    async fn a_room_id_loads_once_and_a_disabled_provider_is_never_requested() {
        let http = fixtures();
        let only_7tv = Providers {
            seven_tv: true,
            bttv: false,
            ffz: false,
        };
        let service = service(http.clone(), only_7tv);
        service.process(room("pajlada", "11148817"));
        service.process(room("pajlada", "11148817"));
        eventually(|| !http.requests.lock().unwrap().is_empty()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let requests = http.requests.lock().unwrap().clone();
        assert_eq!(requests, ["https://7tv.io/v3/users/twitch/11148817"]);
    }

    #[tokio::test]
    async fn turning_a_provider_on_reloads_and_closing_a_channel_forgets_it() {
        let http = fixtures();
        let off = Providers {
            seven_tv: false,
            bttv: false,
            ffz: false,
        };
        let service = service(http.clone(), off);
        service.process(room("pajlada", "11148817"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(http.requests.lock().unwrap().is_empty());
        assert!(emotes(&service.process(message("pajlada", "NERD"))).is_empty());

        service.set_providers(Providers::ALL);
        eventually(|| !emotes(&service.process(message("pajlada", "NERD"))).is_empty()).await;
        // Turning one off again resolves nothing from it, with no request.
        let requests = http.requests.lock().unwrap().len();
        service.set_providers(off);
        assert!(emotes(&service.process(message("pajlada", "NERD"))).is_empty());
        assert_eq!(http.requests.lock().unwrap().len(), requests);

        service.set_providers(Providers::ALL);
        service.forget(&ChannelId::new("pajlada"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(emotes(&service.process(message("pajlada", "NERD"))).is_empty());
    }

    #[tokio::test]
    async fn incomplete_global_sets_are_requested_again_on_connected() {
        let http = fixtures();
        set_down(&http, true);
        let service = service(http.clone(), Providers::ALL);
        // The app starts before the network is up: every provider fails.
        service.load_global();
        eventually(|| service.inner.state().global.load == Load::Incomplete).await;
        assert!(emotes(&service.process(message("a", "RainTime"))).is_empty());

        // The network is back and the connection comes up (first connect or reconnect).
        set_down(&http, false);
        service.process(connected());
        eventually(|| !emotes(&service.process(message("a", "RainTime"))).is_empty()).await;
        eventually(|| service.inner.state().global.load == Load::Complete).await;

        // Complete: further `Connected` events (reconnects) ask for nothing.
        let requests = request_count(&http);
        service.process(connected());
        service.process(connected());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(request_count(&http), requests);
    }

    #[tokio::test]
    async fn a_channel_whose_load_failed_is_requested_again_when_room_state_repeats() {
        let http = fixtures();
        set_down(&http, true);
        let service = service(http.clone(), Providers::ALL);
        service.process(room("pajlada", "11148817"));
        eventually(|| {
            service.inner.state().rooms[&ChannelId::new("pajlada")]
                .attempt
                .load
                == Load::Incomplete
        })
        .await;

        // A repeat while the last load failed (a reconnect re-joins): try again.
        set_down(&http, false);
        service.process(room("pajlada", "11148817"));
        eventually(|| !emotes(&service.process(message("pajlada", "NERD"))).is_empty()).await;
        eventually(|| {
            service.inner.state().rooms[&ChannelId::new("pajlada")]
                .attempt
                .load
                == Load::Complete
        })
        .await;

        // Complete: another repeat is a no-op. A different room id is a new channel.
        let requests = request_count(&http);
        service.process(room("pajlada", "11148817"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(request_count(&http), requests);
        service.process(room("pajlada", "1"));
        eventually(|| request_count(&http) > requests).await;
    }

    #[tokio::test]
    async fn a_load_in_flight_is_not_duplicated() {
        let http = fixtures();
        let service = service(http.clone(), Providers::ALL);
        // Two ROOMSTATEs and a Connected back to back, before anything finished.
        service.process(room("pajlada", "11148817"));
        service.load_global();
        service.process(room("pajlada", "11148817"));
        service.process(connected());
        eventually(|| service.inner.state().global.load == Load::Complete).await;
        eventually(|| {
            service.inner.state().rooms[&ChannelId::new("pajlada")]
                .attempt
                .load
                == Load::Complete
        })
        .await;
        // 3 global + 3 channel requests, once each.
        assert_eq!(request_count(&http), 6);
    }

    #[tokio::test]
    async fn a_skipped_provider_does_not_wipe_a_set_when_a_stale_load_lands() {
        // 7TV is on and loaded; a load that started while it was off finishes afterwards.
        let http = fixtures();
        let service = service(http.clone(), Providers::ALL);
        service.process(room("pajlada", "11148817"));
        eventually(|| !emotes(&service.process(message("pajlada", "NERD"))).is_empty()).await;
        let without_7tv = Providers {
            seven_tv: false,
            ..Providers::ALL
        };
        let stale = service
            .inner
            .loader
            .load_channel_with("11148817", without_7tv)
            .await;
        service
            .inner
            .registry
            .store_channel(&ChannelId::new("pajlada"), &stale);
        assert!(!emotes(&service.process(message("pajlada", "NERD"))).is_empty());
    }

    #[tokio::test]
    async fn only_chat_messages_are_touched() {
        let service = service(fixtures(), Providers::ALL);
        let notice = ChatEvent::Notice {
            channel: None,
            msg_id: None,
            text: "NERD".into(),
        };
        assert_eq!(service.process(notice.clone()), notice);
        let system = ChatEvent::Message {
            channel: ChannelId::new("a"),
            message: Arc::new(Message::plain(
                "2",
                Timestamp(0),
                MessageKind::System,
                "NERD",
            )),
        };
        assert_eq!(service.process(system.clone()), system);
    }
}
