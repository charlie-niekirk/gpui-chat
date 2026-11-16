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
//! * **Live updates and cosmetics (PR 6.2).** Once a channel's sets have loaded, the channel is
//!   handed to [`LiveEmotes`] (7TV's EventAPI and BTTV's socket): emotes added, renamed or removed
//!   while the channel is open change its set at once and leave a quiet system line (see
//!   `gpui_chat_emotes::live`). The same pass over a message adds the author's third-party badges
//!   (7TV, FFZ, Chatterino) after Twitch's own as extra badge ids, and asks for the author's 7TV
//!   style when it is not known yet (batched, off the UI thread); the FFZ and Chatterino holder
//!   lists load once at start. A provider that is off is neither requested nor connected to.
//!
//! Offline sources (`--firehose`, `--replay`) have no service: nothing is fetched.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use gpui_chat_core::{
    ChannelId, ChatEvent, ConnectionState, Message, MessageKind, Timestamp, resolve_fragments,
};
use gpui_chat_emotes::cosmetics::{bulk, lookup::StyleLookup};
use gpui_chat_emotes::{
    Backoff, CosmeticOptions, Cosmetics, EmoteLoader, EmoteRegistry, EmoteSet, Endpoints,
    HttpClient, LiveEmotes, LiveSink, Providers, ReqwestHttp,
};
use gpui_chat_settings::EmoteSettings;
use gpui_kit::App;
use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::badges::Badges;
use crate::bridge::BridgeSender;
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

/// What the service keeps besides emote sets (see the module docs); none in tests that do not
/// need it.
#[derive(Default)]
pub struct Extras {
    /// Third-party badges and paints, and the batched 7TV lookup that fills them.
    cosmetics: Option<(Arc<Cosmetics>, StyleLookup)>,
    /// Live emote updates.
    live: Option<LiveEmotes>,
    /// Where the FFZ and Chatterino holder lists come from, and who is told when they land.
    bulk: Option<(Arc<dyn HttpClient>, Arc<Notify>)>,
}

/// Which holder lists have loaded (or are loading).
#[derive(Default)]
struct BulkState {
    ffz: bool,
    chatterino: bool,
}

struct Inner {
    loader: EmoteLoader,
    registry: Arc<EmoteRegistry>,
    handle: Handle,
    state: Mutex<State>,
    extras: Extras,
    bulk: Mutex<BulkState>,
    /// Which providers failed in each scope (`"global"` or a channel's login), for the one strip
    /// the user sees (see [`Inner::note`]).
    failures: Mutex<HashMap<String, Vec<(&'static str, String)>>>,
}

impl Inner {
    /// Records how `results` went for `scope` and files (or clears) the "emotes did not load"
    /// strip for all scopes together, so a provider that is down is one strip, not one per
    /// channel.
    fn note(&self, scope: &str, results: &gpui_chat_emotes::ProviderResults) {
        let failed: Vec<(&'static str, String)> = results
            .errors()
            .map(|(provider, error)| (provider.name(), error.to_string()))
            .collect();
        self.publish(scope, failed);
    }

    /// The holder list of a third-party badge provider (`"FFZ"`, `"Chatterino"`) loaded, or not.
    fn note_badges(&self, name: &'static str, error: Option<String>) {
        let scope = format!("{name} badges");
        let failed = error
            .map(|error| vec![("badges", error)])
            .unwrap_or_default();
        self.publish(&scope, failed);
    }

    /// Stores `failed` for `scope` and files or clears the one strip for all scopes.
    fn publish(&self, scope: &str, failed: Vec<(&'static str, String)>) {
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if failed.is_empty() {
            failures.remove(scope);
        } else {
            failures.insert(scope.to_owned(), failed);
        }
        let mut names: Vec<&'static str> = failures
            .values()
            .flatten()
            .map(|(provider, _)| *provider)
            .collect();
        names.sort_unstable();
        names.dedup();
        if names.is_empty() {
            crate::problems::clear_from_anywhere("emotes");
            return;
        }
        let mut details: Vec<String> = failures
            .iter()
            .flat_map(|(scope, list)| {
                list.iter()
                    .map(move |(provider, error)| format!("{provider} ({scope}): {error}"))
            })
            .collect();
        details.sort();
        crate::problems::report_from_anywhere(
            "emotes",
            crate::problems::Problem::warning(format!(
                "Some emotes or badges could not be loaded ({})",
                names.join(", ")
            ))
            .line("They are missing until the provider answers again; emotes show as plain text.")
            .fix("Retry", crate::problems::Fix::ReloadEmotes)
            .details(details.join("\n")),
        );
    }

    /// A channel was closed: its failures no longer count.
    fn forget_failures(&self, scope: &str) {
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if failures.remove(scope).is_some() && failures.is_empty() {
            crate::problems::clear_from_anywhere("emotes");
        }
    }

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
    #[cfg(test)]
    pub fn new(loader: EmoteLoader, enabled: Providers, handle: Handle) -> Self {
        Self::with_extras(
            loader,
            enabled,
            handle,
            Arc::new(EmoteRegistry::new(enabled)),
            Extras::default(),
        )
    }

    fn with_extras(
        loader: EmoteLoader,
        enabled: Providers,
        handle: Handle,
        registry: Arc<EmoteRegistry>,
        extras: Extras,
    ) -> Self {
        let _ = enabled;
        Self {
            inner: Arc::new(Inner {
                loader,
                registry,
                handle,
                extras,
                bulk: Mutex::default(),
                failures: Mutex::default(),
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

    /// Starts the service on the backend runtime and begins loading the global sets (and the
    /// holder lists of the third-party badges). `sender` is where live-update lines go. `None` if
    /// the HTTP client cannot be built (emotes are then plain text).
    pub fn start(enabled: Providers, sender: BridgeSender<ChatEvent>, cx: &App) -> Option<Self> {
        let http: Arc<dyn HttpClient> = match runtime::enter(cx, ReqwestHttp::new) {
            Ok(http) => Arc::new(http),
            Err(error) => {
                tracing::warn!(%error, "third-party emotes unavailable");
                crate::problems::report_from_anywhere(
                    "emotes",
                    crate::problems::Problem::warning("Third-party emotes are unavailable")
                        .line("They show as plain text. Restart the app to try again.")
                        .details(error.to_string()),
                );
                return None;
            }
        };
        let loader = EmoteLoader::new(http.clone());
        let handle = cx.global::<runtime::Backend>().handle();
        let registry = Arc::new(EmoteRegistry::new(enabled));
        let mut extras = Extras::default();
        if let Some(badges) = cx.try_global::<Badges>() {
            let (cosmetics, changed) = badges.cosmetics_handles();
            let poke = changed.clone();
            let lookup = StyleLookup::start(
                &handle,
                http.clone(),
                cosmetics.clone(),
                Arc::new(move || poke.notify_one()),
            );
            let sink = Arc::new(AppSink {
                sender,
                handle: handle.clone(),
                changed: changed.clone(),
            });
            extras.live = Some(LiveEmotes::new(
                &handle,
                registry.clone(),
                cosmetics.clone(),
                loader.seven_tv.clone(),
                sink,
                endpoints(),
                Backoff::default(),
            ));
            extras.cosmetics = Some((cosmetics, lookup));
            extras.bulk = Some((http, changed));
        }
        let service = Self::with_extras(loader, enabled, handle, registry, extras);
        service.load_global();
        service.load_bulk_badges();
        Some(service)
    }

    /// Loads the FFZ and Chatterino badge holder lists that are switched on and not loaded yet.
    /// A failure is a warning; the next `Connected` or a switch being turned on tries again.
    fn load_bulk_badges(&self) {
        let (Some((cosmetics, _)), Some((http, changed))) =
            (&self.inner.extras.cosmetics, &self.inner.extras.bulk)
        else {
            return;
        };
        let options = cosmetics.options();
        let (want_ffz, want_chatterino) = {
            let mut bulk = self
                .inner
                .bulk
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let want = (
                options.ffz_badges && !bulk.ffz,
                options.chatterino_badges && !bulk.chatterino,
            );
            bulk.ffz |= want.0;
            bulk.chatterino |= want.1;
            want
        };
        for (wanted, name) in [(want_ffz, "FFZ"), (want_chatterino, "Chatterino")] {
            if !wanted {
                continue;
            }
            let (inner, cosmetics, http, changed) = (
                self.inner.clone(),
                cosmetics.clone(),
                http.clone(),
                changed.clone(),
            );
            drop(self.inner.handle.spawn(async move {
                let loaded = if name == "FFZ" {
                    bulk::load_ffz(&*http).await
                } else {
                    bulk::load_chatterino(&*http).await
                };
                match loaded {
                    Ok(list) => {
                        tracing::debug!(
                            badges = list.badges.len(),
                            holders = list.holders.len(),
                            "{name} badges loaded"
                        );
                        if name == "FFZ" {
                            cosmetics.store_ffz(list.badges, list.holders);
                        } else {
                            cosmetics.store_chatterino(list.badges, list.holders);
                        }
                        changed.notify_one();
                        inner.note_badges(name, None);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "{name} badges unavailable");
                        inner.note_badges(name, Some(error.to_string()));
                        let mut bulk = inner.bulk.lock().unwrap_or_else(PoisonError::into_inner);
                        if name == "FFZ" {
                            bulk.ffz = false;
                        } else {
                            bulk.chatterino = false;
                        }
                    }
                }
            }));
        }
    }

    /// Applies the `[badges]` switches (and tells the live connections whether 7TV's cosmetics
    /// still need one).
    pub fn set_cosmetic_options(&self, options: CosmeticOptions) {
        let Some((cosmetics, _)) = &self.inner.extras.cosmetics else {
            return;
        };
        cosmetics.set_options(options);
        if let Some(live) = &self.inner.extras.live {
            live.configure(self.inner.registry.enabled(), options);
        }
        self.load_bulk_badges();
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
            inner.note("global", &results);
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
            inner.note(channel.login(), &results);
            if room.attempt.token == token {
                room.attempt.load = if results.all_ok() {
                    Load::Complete
                } else {
                    Load::Incomplete
                };
            }
            // Listen for changes: a channel whose providers failed to load still gets its
            // cosmetics, and BTTV (keyed by the room id alone). Done while the room is still
            // known to be open (`state` is held), so a `forget` cannot slip in between and have
            // its channel re-opened afterwards.
            if let Some(live) = &inner.extras.live {
                let origin = inner
                    .registry
                    .channel_set(&channel, gpui_chat_core::EmoteProvider::SevenTv)
                    .and_then(|set| set.origin().cloned());
                live.open_channel(&channel, &room_id, origin.as_ref());
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
        if let (Some(live), Some((cosmetics, _))) =
            (&self.inner.extras.live, &self.inner.extras.cosmetics)
        {
            live.configure(providers, cosmetics.options());
        }
        if providers.gains_over(before) {
            self.reload();
        }
    }

    /// The sets `channel` resolves words against right now, in precedence order (what the input box
    /// completes from and the emote picker lists).
    pub fn sets(&self, channel: &ChannelId) -> Vec<Arc<EmoteSet>> {
        self.inner.registry.resolver(channel).sets().to_vec()
    }

    /// The channel was closed: forget its room and sets.
    pub fn forget(&self, channel: &ChannelId) {
        self.inner.state().rooms.remove(channel);
        self.inner.forget_failures(channel.login());
        self.inner.registry.forget_channel(channel);
        if let Some(live) = &self.inner.extras.live {
            live.close_channel(channel);
        }
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
                self.load_bulk_badges();
                ChatEvent::Connection {
                    channel: None,
                    state: ConnectionState::Connected,
                }
            }
            other => other,
        }
    }

    /// `message` with its author's third-party badges added after Twitch's own, and the author's
    /// 7TV style asked for when it is not known (the answer shows from their next message on, and
    /// the paint on their name at once). The same message if there is nothing to add.
    fn with_cosmetics(&self, message: Arc<Message>) -> Arc<Message> {
        let Some((cosmetics, lookup)) = &self.inner.extras.cosmetics else {
            return message;
        };
        let Some(author) = &message.author else {
            return message;
        };
        let Some(user_id) = author.user_id.as_deref() else {
            return message;
        };
        if cosmetics.options().wants_seventv() {
            lookup.request(user_id);
        }
        let extra = cosmetics.badge_ids(user_id);
        if extra.is_empty() || extra.iter().all(|id| author.badges.contains(id)) {
            return message;
        }
        let mut message = (*message).clone();
        if let Some(author) = &mut message.author {
            for id in extra {
                if !author.badges.contains(&id) {
                    author.badges.push(id);
                }
            }
        }
        Arc::new(message)
    }

    /// `message` with its third-party emotes resolved, or the same message if there are none.
    fn resolve(&self, channel: &ChannelId, message: Arc<Message>) -> Arc<Message> {
        if !matches!(
            message.kind,
            MessageKind::Chat | MessageKind::Action | MessageKind::UserNotice
        ) {
            return message;
        }
        let message = self.with_cosmetics(message);
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

/// The live-update endpoints. `GPUI_CHAT_7TV_EVENTS_URL` and `GPUI_CHAT_BTTV_SOCKET_URL` point
/// them elsewhere (a local server replaying recorded frames, for checking the feature without
/// waiting for an editor); unset, the providers' own.
fn endpoints() -> Endpoints {
    let mut endpoints = Endpoints::default();
    if let Ok(url) = std::env::var("GPUI_CHAT_7TV_EVENTS_URL") {
        endpoints.seven_tv = url;
    }
    if let Ok(url) = std::env::var("GPUI_CHAT_BTTV_SOCKET_URL") {
        endpoints.bttv = url;
    }
    endpoints
}

/// Where live emote changes show up: a quiet system line in the channel's chat, through the same
/// bridge every event takes, and a wake-up of the views when cosmetics changed.
struct AppSink {
    sender: BridgeSender<ChatEvent>,
    handle: Handle,
    changed: Arc<Notify>,
}

impl LiveSink for AppSink {
    fn notice(&self, channel: &ChannelId, text: String) {
        let sender = self.sender.clone();
        let event = emote_notice(channel, text, crate::live::now());
        drop(self.handle.spawn(async move {
            // A closed bridge means the UI is gone.
            let _ = sender.send(event).await;
        }));
    }

    fn cosmetics_changed(&self) {
        self.changed.notify_one();
    }
}

/// The system line for a live emote change. The id carries a counter so two lines of one
/// millisecond (7TV and BTTV) are not taken for one message.
pub fn emote_notice(channel: &ChannelId, text: String, at: Timestamp) -> ChatEvent {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!(
        "emote-live:{}:{}",
        at.unix_millis(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    ChatEvent::Message {
        channel: channel.clone(),
        message: Arc::new(Message::plain(id, at, MessageKind::System, text)),
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

    /// A service with cosmetics (no live connections), over the same fake HTTP.
    fn cosmetic_service(
        http: Arc<Fixtures>,
        options: CosmeticOptions,
    ) -> (EmoteService, Arc<Cosmetics>) {
        let registry = Arc::new(gpui_chat_emotes::BadgeRegistry::new());
        let cosmetics = Arc::new(Cosmetics::new(registry, options));
        let lookup = StyleLookup::start(
            &Handle::current(),
            http.clone(),
            cosmetics.clone(),
            Arc::new(|| {}),
        );
        let extras = Extras {
            cosmetics: Some((cosmetics.clone(), lookup)),
            ..Extras::default()
        };
        let loader = EmoteLoader::new(http);
        let enabled = Providers::ALL;
        let service = EmoteService::with_extras(
            loader,
            enabled,
            Handle::current(),
            Arc::new(EmoteRegistry::new(enabled)),
            extras,
        );
        (service, cosmetics)
    }

    fn message_from(user_id: Option<&str>, badges: &[(&str, &str)]) -> ChatEvent {
        let mut author = Author::new("al", "Al");
        author.user_id = user_id.map(str::to_owned);
        author.badges = badges
            .iter()
            .map(|(set, version)| gpui_chat_core::BadgeId::new(*set, *version))
            .collect();
        ChatEvent::Message {
            channel: ChannelId::new("c"),
            message: Arc::new(Message::chat(
                "1",
                Timestamp(0),
                author,
                Tokenizer::new().tokenize("hi", &[]).fragments,
            )),
        }
    }

    fn badge_names(event: &ChatEvent) -> Vec<String> {
        let ChatEvent::Message { message, .. } = event else {
            panic!("not a message");
        };
        message
            .author
            .as_ref()
            .unwrap()
            .badges
            .iter()
            .map(|b| format!("{}/{}", b.set, b.version))
            .collect()
    }

    #[tokio::test]
    async fn third_party_badges_follow_twitchs_and_each_user_is_looked_up_once() {
        let http = fixtures();
        let (service, cosmetics) = cosmetic_service(http.clone(), CosmeticOptions::default());
        cosmetics.set_style(
            "42",
            gpui_chat_emotes::cosmetics::UserStyle {
                paint_id: None,
                badge_id: Some("B1".into()),
            },
            std::time::Instant::now(),
        );
        let out = service.process(message_from(Some("42"), &[("moderator", "1")]));
        assert_eq!(badge_names(&out), ["moderator/1", "seventv/B1"]);
        // Twice is no different, and a message that already carries them is not changed.
        assert_eq!(
            badge_names(&service.process(out.clone())),
            ["moderator/1", "seventv/B1"]
        );

        // An unknown chatter is asked about, once, in one batched 7TV request.
        service.process(message_from(Some("77"), &[]));
        service.process(message_from(Some("77"), &[]));
        eventually(|| {
            http.requests
                .lock()
                .unwrap()
                .iter()
                .any(|url| url.starts_with("https://7tv.io/v3/gql?"))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let asked: Vec<String> = http
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.contains("gql"))
            .cloned()
            .collect();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(asked[0].contains("77"), "{}", asked[0]);
        // Known chatters (42, whose style is stored) were not asked about.
        assert!(!asked[0].contains("%2242%22"), "{}", asked[0]);

        // Messages without a user id (the offline sources, a system line) are left alone.
        let anonymous = message_from(None, &[("moderator", "1")]);
        assert_eq!(service.process(anonymous.clone()), anonymous);
    }

    #[tokio::test]
    async fn no_7tv_request_when_neither_paints_nor_7tv_badges_are_on() {
        let http = fixtures();
        let options = CosmeticOptions {
            seventv_badges: false,
            paints: false,
            ..CosmeticOptions::default()
        };
        let (service, _) = cosmetic_service(http.clone(), options);
        service.process(message_from(Some("77"), &[]));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            http.requests
                .lock()
                .unwrap()
                .iter()
                .all(|u| !u.contains("gql"))
        );
    }

    #[test]
    fn the_live_line_is_a_quiet_system_message_with_a_distinct_id() {
        let at = Timestamp::from_unix_millis(1_000);
        let (a, b) = (
            emote_notice(&ChannelId::new("c"), "7TV: added A".into(), at),
            emote_notice(&ChannelId::new("c"), "BTTV: removed Bee".into(), at),
        );
        let (ChatEvent::Message { message: a, .. }, ChatEvent::Message { message: b, .. }) = (a, b)
        else {
            panic!("messages");
        };
        assert_eq!(a.kind, MessageKind::System);
        assert!(a.author.is_none());
        assert_ne!(a.id, b.id, "two lines of one millisecond are two messages");
    }

    struct Quiet;

    impl LiveSink for Quiet {
        fn notice(&self, _: &ChannelId, _: String) {}
        fn cosmetics_changed(&self) {}
    }

    #[tokio::test]
    async fn a_channel_is_listened_for_once_its_sets_have_loaded_and_not_after_it_closes() {
        let http = fixtures();
        let registry = Arc::new(EmoteRegistry::new(Providers::ALL));
        let cosmetics = Arc::new(Cosmetics::new(
            Arc::new(gpui_chat_emotes::BadgeRegistry::new()),
            CosmeticOptions::default(),
        ));
        let loader = EmoteLoader::new(http);
        // Nothing listens on these ports: the connections fail and are retried, which is not
        // what this is about.
        let unreachable = Endpoints {
            seven_tv: "ws://127.0.0.1:9".into(),
            bttv: "ws://127.0.0.1:9".into(),
        };
        let live = LiveEmotes::new(
            &Handle::current(),
            registry.clone(),
            cosmetics,
            loader.seven_tv.clone(),
            Arc::new(Quiet),
            unreachable,
            Backoff::default(),
        );
        let extras = Extras {
            live: Some(live),
            ..Extras::default()
        };
        let service =
            EmoteService::with_extras(loader, Providers::ALL, Handle::current(), registry, extras);
        let channel = ChannelId::new("pajlada");
        let is_open = |service: &EmoteService| {
            service
                .inner
                .extras
                .live
                .as_ref()
                .unwrap()
                .is_open(&channel)
        };
        assert!(!is_open(&service));
        service.process(room("pajlada", "11148817"));
        eventually(|| is_open(&service)).await;
        // The 7TV set's ids came with the load: the live connection can subscribe to them.
        let origin = service
            .inner
            .registry
            .channel_set(&channel, gpui_chat_core::EmoteProvider::SevenTv)
            .and_then(|set| set.origin().cloned())
            .expect("the 7TV set's ids");
        assert_eq!(origin.set_id.as_deref(), Some("01F7GF1ZV8000EFV6JZ29CKEDB"));

        service.forget(&channel);
        assert!(!is_open(&service));
    }
}
