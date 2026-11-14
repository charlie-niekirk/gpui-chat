//! Third-party emotes for the live source (PR 2.4): loading BTTV / FFZ / 7TV sets and resolving
//! them in chat messages.
//!
//! * The **global** sets load at startup. A channel's sets load when its `ROOMSTATE` supplies the
//!   room id (the Twitch user id the providers are keyed by); a room id seen again does not load
//!   again, a changed one does.
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

use gpui_chat_core::{ChannelId, ChatEvent, Message, MessageKind, resolve_fragments};
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

struct Inner {
    loader: EmoteLoader,
    registry: EmoteRegistry,
    handle: Handle,
    /// The room id each open channel's sets were requested for.
    rooms: Mutex<HashMap<ChannelId, String>>,
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
                rooms: Mutex::new(HashMap::new()),
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

    fn rooms(&self) -> std::sync::MutexGuard<'_, HashMap<ChannelId, String>> {
        self.inner
            .rooms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn load_global(&self) {
        let inner = self.inner.clone();
        drop(self.inner.handle.spawn(async move {
            let results = inner
                .loader
                .load_global_with(inner.registry.enabled())
                .await;
            inner.registry.store_global(&results);
        }));
    }

    fn load_channel(&self, channel: ChannelId, room_id: String) {
        let inner = self.inner.clone();
        drop(self.inner.handle.spawn(async move {
            let results = inner
                .loader
                .load_channel_with(&room_id, inner.registry.enabled())
                .await;
            // The channel may have been closed (or its room id changed) while this loaded.
            let current = inner
                .rooms
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&channel)
                .is_some_and(|id| *id == room_id);
            if current {
                inner.registry.store_channel(&channel, &results);
            }
        }));
    }

    /// Fetches the global sets and every open channel's sets again.
    pub fn reload(&self) {
        self.load_global();
        let rooms: Vec<(ChannelId, String)> = self
            .rooms()
            .iter()
            .map(|(channel, id)| (channel.clone(), id.clone()))
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
        self.rooms().remove(channel);
        self.inner.registry.forget_channel(channel);
    }

    /// Passes `event` on: a `ROOMSTATE` with a (new) room id starts loading the channel's sets, a
    /// chat message has its emotes resolved.
    pub fn process(&self, event: ChatEvent) -> ChatEvent {
        match event {
            ChatEvent::RoomState { channel, update } => {
                if let Some(room_id) = &update.room_id {
                    let new = self
                        .rooms()
                        .insert(channel.clone(), room_id.clone())
                        .as_ref()
                        != Some(room_id);
                    if new {
                        self.load_channel(channel.clone(), room_id.clone());
                    }
                }
                ChatEvent::RoomState { channel, update }
            }
            ChatEvent::Message { channel, message } => {
                let message = self.resolve(&channel, message);
                ChatEvent::Message { channel, message }
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

    /// Answers from `fixtures/emotes/`, counting requests per URL.
    #[derive(Default)]
    struct Fixtures {
        replies: HashMap<&'static str, &'static str>,
        requests: Mutex<Vec<String>>,
    }

    impl HttpClient for Fixtures {
        fn get<'a>(&'a self, url: &'a str) -> HttpFuture<'a> {
            self.requests.lock().unwrap().push(url.to_owned());
            let file = self.replies.get(url).copied();
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
        })
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
