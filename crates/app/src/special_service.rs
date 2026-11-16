//! What event messages need from the network (PR 5.1), for the live source.
//!
//! * **Cheermotes.** A channel's cheermote set comes from Helix `Get Cheermotes` and needs a
//!   login, so it is loaded when a channel's `ROOMSTATE` supplies the room id *and* the user is
//!   logged in (and for every known channel again when they log in), once per room id, asked again
//!   when `ROOMSTATE` repeats after a failure, like [`crate::emote_service::EmoteService`]. A
//!   message with bits then has its `Cheer` words given their image
//!   ([`gpui_chat_core::CheermoteSet::resolve`]); without a set (logged out, not loaded yet, a
//!   failed load) the text stays and the row says how many bits were cheered.
//! * **Shared-chat source names.** A message from another channel of a shared chat session
//!   carries that channel's room id only; the name is looked up once per id
//!   ([`gpui_chat_twitch::RoomNames`]) and put on the messages that arrive after the answer. (The
//!   UI fills in names of channels it has open without any lookup, see `ChannelHub`.)
//!
//! Every event passes [`SpecialService::process`] on its way to the UI (live messages in `forward`,
//! history in `HistoryService`); `--firehose` / `--replay` have none: no network, no login.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use gpui_chat_core::{
    ChannelId, ChatEvent, CheerTier, Cheermote, CheermoteSet, Emote, EmoteId, EmoteProvider,
    ImageScale, ImageUrls, Message, MessageKind,
};
use gpui_chat_twitch::{Auth, HelixClient, RoomNames};
use gpui_kit::App;
use tokio::runtime::Handle;

use crate::runtime;

/// `GPUI_CHAT_DEMO_CHEERMOTES=1` with `--replay` gives messages with bits the standard cheermote
/// images (the public CDN, no login), so they can be looked at offline.
pub const DEMO_ENV: &str = "GPUI_CHAT_DEMO_CHEERMOTES";

/// The standard `Cheer` and `Kappa` cheermotes, built from Twitch's documented CDN layout
/// (`/actions/<prefix>/dark/animated/<tier>/<scale>.gif`) instead of Helix, for [`DEMO_ENV`].
pub fn demo_cheermotes() -> CheermoteSet {
    const TIERS: [u32; 5] = [1, 100, 1000, 5000, 10_000];
    let cheermote = |prefix: &str| {
        Cheermote {
        prefix: prefix.to_owned(),
        tiers: TIERS
            .iter()
            .map(|&min_bits| {
                let url = |scale: u8| {
                    format!(
                        "https://d3aqoihi2n8ty8.cloudfront.net/actions/{}/dark/animated/{min_bits}/{scale}.gif",
                        prefix.to_ascii_lowercase()
                    )
                };
                CheerTier {
                    min_bits,
                    image: Arc::new(Emote {
                        id: EmoteId::from(format!("cheer:{}:{min_bits}", prefix.to_ascii_lowercase())),
                        name: Arc::from(prefix),
                        provider: EmoteProvider::Twitch,
                        urls: ImageUrls::new()
                            .with(ImageScale::X1, url(1))
                            .with(ImageScale::X2, url(2))
                            .with(ImageScale::X4, url(4)),
                        animated: true,
                        zero_width: false,
                        width: Some(28),
                        height: Some(28),
                    }),
                }
            })
            .collect(),
    }
    };
    CheermoteSet::new([cheermote("Cheer"), cheermote("Kappa")])
}

/// `message` with `set`'s cheermotes drawn, if it carries bits and has any; else the same message.
pub fn with_cheermotes(set: &CheermoteSet, message: Arc<Message>) -> Arc<Message> {
    if message.bits.is_none()
        || !matches!(
            message.kind,
            MessageKind::Chat | MessageKind::Action | MessageKind::UserNotice
        )
    {
        return message;
    }
    match set.resolve(&message.fragments) {
        Some(fragments) => {
            let mut message = (*message).clone();
            message.fragments = fragments;
            Arc::new(message)
        }
        None => message,
    }
}

/// Where one channel's cheermote load stands.
#[derive(Default)]
struct Room {
    room_id: String,
    set: Option<Arc<CheermoteSet>>,
    /// A load is running (never started twice).
    loading: bool,
    /// The last load failed: the next `ROOMSTATE` asks again.
    failed: bool,
}

#[derive(Default)]
struct State {
    rooms: HashMap<ChannelId, Room>,
    logged_in: bool,
}

struct Inner {
    /// `None` for a service that never starts a request (tests).
    handle: Option<Handle>,
    helix: Option<HelixClient>,
    names: Option<RoomNames>,
    state: Mutex<State>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Cheap to clone; all clones share the state.
#[derive(Clone)]
pub struct SpecialService {
    inner: Arc<Inner>,
}

impl SpecialService {
    /// Starts the service on the backend runtime. `account` (the login owner and its Helix
    /// client) enables cheermotes while logged in.
    pub fn start(account: Option<(Auth, HelixClient)>, cx: &App) -> Self {
        let names = match runtime::enter(cx, RoomNames::new) {
            Ok(names) => Some(names),
            Err(error) => {
                tracing::warn!(%error, "shared chat channel names unavailable");
                None
            }
        };
        let (auth, helix) = match account {
            Some((auth, helix)) => (Some(auth), Some(helix)),
            None => (None, None),
        };
        let service = Self::new(Some(cx.global::<runtime::Backend>().handle()), helix, names);
        if let Some(auth) = auth {
            service.follow_login(&auth);
        }
        service
    }

    fn new(handle: Option<Handle>, helix: Option<HelixClient>, names: Option<RoomNames>) -> Self {
        Self {
            inner: Arc::new(Inner {
                handle,
                helix,
                names,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// A service that never starts a request: events pass through, and a set can only come from
    /// the tests.
    #[cfg(test)]
    pub fn inert() -> Self {
        Self::new(None, None, None)
    }

    /// Runs `future` on the backend runtime (not at all for an [`inert`](Self::inert) service).
    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        if let Some(handle) = &self.inner.handle {
            drop(handle.spawn(future));
        }
    }

    /// Loads every known channel's cheermotes when the user logs in (the first loads went out
    /// before the login was restored, or anonymously, and failed or were skipped).
    fn follow_login(&self, auth: &Auth) {
        let service = self.clone();
        // Subscribed here, not in the task, so a login restored before the task runs is not missed.
        let mut states = auth.subscribe();
        self.spawn(async move {
            loop {
                let now = states.borrow_and_update().account().is_some();
                let rooms: Vec<(ChannelId, String)> = {
                    let mut state = service.inner.state();
                    let changed = state.logged_in != now;
                    state.logged_in = now;
                    if changed && now {
                        state
                            .rooms
                            .iter()
                            .map(|(channel, room)| (channel.clone(), room.room_id.clone()))
                            .collect()
                    } else {
                        Vec::new()
                    }
                };
                for (channel, room_id) in rooms {
                    service.load(channel, room_id);
                }
                if states.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    /// Passes `event` on: a `ROOMSTATE` with a room id starts that channel's cheermote load (when
    /// logged in), a message with bits has its cheermotes drawn, a shared-chat message gets its
    /// source channel's name when it is known (and a lookup starts when it is not).
    pub fn process(&self, event: ChatEvent) -> ChatEvent {
        match event {
            ChatEvent::RoomState { channel, update } => {
                if let Some(room_id) = &update.room_id {
                    self.room_seen(&channel, room_id);
                }
                ChatEvent::RoomState { channel, update }
            }
            ChatEvent::Message { channel, message } => {
                let message = self.enrich(&channel, message);
                ChatEvent::Message { channel, message }
            }
            other => other,
        }
    }

    /// A channel was closed: forget its set.
    pub fn forget(&self, channel: &ChannelId) {
        self.inner.state().rooms.remove(channel);
    }

    fn room_seen(&self, channel: &ChannelId, room_id: &str) {
        let start = {
            let mut state = self.inner.state();
            let logged_in = state.logged_in;
            let room = state.rooms.entry(channel.clone()).or_default();
            if room.room_id != room_id {
                *room = Room {
                    room_id: room_id.to_owned(),
                    ..Room::default()
                };
            }
            logged_in && !room.loading && (room.set.is_none() || room.failed)
        };
        if start {
            self.load(channel.clone(), room_id.to_owned());
        }
    }

    fn load(&self, channel: ChannelId, room_id: String) {
        let Some(helix) = self.inner.helix.clone() else {
            return;
        };
        {
            let mut state = self.inner.state();
            let Some(room) = state.rooms.get_mut(&channel) else {
                return;
            };
            if room.room_id != room_id || room.loading {
                return;
            }
            room.loading = true;
        }
        let inner = self.inner.clone();
        self.spawn(async move {
            let result = helix.get_cheermotes(&room_id).await;
            let mut state = inner.state();
            // The channel was closed, or its room changed, while the request ran.
            let Some(room) = state
                .rooms
                .get_mut(&channel)
                .filter(|r| r.room_id == room_id)
            else {
                return;
            };
            room.loading = false;
            match result {
                Ok(set) => {
                    room.set = Some(Arc::new(set));
                    room.failed = false;
                }
                Err(error) => {
                    tracing::debug!(%error, channel = %channel, "cheermotes unavailable");
                    room.failed = true;
                }
            }
        });
    }

    /// `message` with its cheermotes drawn and its shared-chat source named, or the same message.
    fn enrich(&self, channel: &ChannelId, message: Arc<Message>) -> Arc<Message> {
        let set = self
            .inner
            .state()
            .rooms
            .get(channel)
            .and_then(|room| room.set.clone());
        let message = match &set {
            Some(set) => with_cheermotes(set, message),
            None => message,
        };
        let name = message
            .shared
            .as_ref()
            .filter(|shared| shared.source_name.is_none())
            .and_then(|shared| self.source_name(&shared.source_room_id));
        let Some(name) = name else {
            return message;
        };
        let mut message = (*message).clone();
        if let Some(shared) = message.shared.as_mut() {
            shared.source_name = Some(name.to_string());
        }
        Arc::new(message)
    }

    /// The name of a source room if it is known; otherwise (first time this room is seen) a
    /// lookup starts and the answer serves the messages that follow it.
    fn source_name(&self, room_id: &str) -> Option<Arc<str>> {
        let names = self.inner.names.as_ref()?;
        if let Some(known) = names.known(room_id) {
            return known;
        }
        if names.claim(room_id) {
            let (names, room_id) = (names.clone(), room_id.to_owned());
            self.spawn(async move {
                names.lookup(&room_id).await;
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{
        Author, CheerTier, Cheermote, Emote, Fragment, MessageId, RoomStateUpdate, SharedChat,
        Timestamp,
    };

    use super::*;

    fn set() -> Arc<CheermoteSet> {
        Arc::new(CheermoteSet::new([Cheermote {
            prefix: "Cheer".into(),
            tiers: vec![CheerTier {
                min_bits: 1,
                image: Arc::new(Emote::twitch("cheer:cheer:1", "Cheer")),
            }],
        }]))
    }

    fn service() -> SpecialService {
        SpecialService::inert()
    }

    fn cheer_message(bits: Option<u32>) -> Arc<Message> {
        let mut message = Message::chat(
            "m1",
            Timestamp(0),
            Author::new("a", "A"),
            vec![Fragment::Text("Cheer100 gg".into())],
        );
        message.bits = bits;
        Arc::new(message)
    }

    fn with_set(service: &SpecialService, channel: &ChannelId) {
        service.process(ChatEvent::RoomState {
            channel: channel.clone(),
            update: RoomStateUpdate {
                room_id: Some("1000".into()),
                ..Default::default()
            },
        });
        service.inner.state().rooms.get_mut(channel).unwrap().set = Some(set());
    }

    #[test]
    fn a_message_with_bits_gets_its_cheermotes_once_the_set_is_loaded() {
        let (service, channel) = (service(), ChannelId::new("chan"));
        // No set yet: the message is passed on as it is.
        let before = cheer_message(Some(100));
        assert!(Arc::ptr_eq(
            &service.enrich(&channel, before.clone()),
            &before
        ));

        with_set(&service, &channel);
        let after = service.enrich(&channel, cheer_message(Some(100)));
        assert!(matches!(
            &after.fragments[0],
            Fragment::Cheer {
                bits: 100,
                image: Some(_),
                ..
            }
        ));
        assert_eq!(after.plain_text(), "Cheer100 gg");
        // Without bits the same words are only text.
        let plain = cheer_message(None);
        assert!(Arc::ptr_eq(
            &service.enrich(&channel, plain.clone()),
            &plain
        ));
        // Another channel has no set.
        let other = cheer_message(Some(100));
        assert!(Arc::ptr_eq(
            &service.enrich(&ChannelId::new("other"), other.clone()),
            &other
        ));
        // A closed channel is forgotten.
        service.forget(&channel);
        let again = cheer_message(Some(100));
        assert!(Arc::ptr_eq(
            &service.enrich(&channel, again.clone()),
            &again
        ));
    }

    #[test]
    fn a_new_room_id_drops_the_old_set() {
        let (service, channel) = (service(), ChannelId::new("chan"));
        with_set(&service, &channel);
        service.process(ChatEvent::RoomState {
            channel: channel.clone(),
            update: RoomStateUpdate {
                room_id: Some("2000".into()),
                ..Default::default()
            },
        });
        let message = cheer_message(Some(100));
        assert!(Arc::ptr_eq(
            &service.enrich(&channel, message.clone()),
            &message
        ));
    }

    #[test]
    fn a_shared_message_is_untouched_when_no_lookup_service_exists() {
        let service = service();
        let mut message = (*cheer_message(None)).clone();
        message.shared = Some(SharedChat {
            source_room_id: "3000".into(),
            source_id: Some(MessageId::from("x")),
            source_badges: Vec::new(),
            source_name: None,
        });
        let message = Arc::new(message);
        assert!(Arc::ptr_eq(
            &service.enrich(&ChannelId::new("chan"), message.clone()),
            &message
        ));
    }
}
