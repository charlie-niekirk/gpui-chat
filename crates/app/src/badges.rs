//! Twitch badges in the app (PR 2.5): the shared registry the chat views read, and the service
//! that loads a channel's badges for the live source.
//!
//! * **[`Badges`]** is a GPUI global holding the [`BadgeRegistry`]. [`Badges::install`] stores the
//!   bundled global snapshot (moderator, VIP, broadcaster, staff, ... see
//!   `gpui_chat_emotes::badges::snapshot`) before the first window opens, so every source,
//!   `--firehose` and `--replay` included, shows those badges with no network. The channel sets
//!   the live source loads later announce themselves: the loader pokes a `Notify`, a task on the
//!   UI thread bumps an epoch on the global, and views that observe it re-render; a view lays its
//!   messages out again only if *its channel's* registry version moved
//!   ([`BadgeRegistry::version`]).
//! * **[`BadgeService`]** asks its source for the global badges once at start (off the UI thread,
//!   through [`BadgeSource::global`]) and keeps the bundled snapshot if that fails; the anonymous
//!   source simply returns the snapshot again, a Helix source (PR 3.1) fetches the real set. It
//!   loads a channel's badges when its `ROOMSTATE` supplies the room id,
//!   exactly like [`crate::emote_service::EmoteService`] does for emote sets: once per room id, a
//!   load that failed is requested again when `ROOMSTATE` repeats (a reconnect re-joins and so
//!   repeats it), a load in flight is never duplicated, and a closed channel is forgotten (its
//!   badges dropped, a load still running discarded). It never touches the events: badges are
//!   resolved at paint time from the registry, not in the message.
//!
//! Failures degrade silently: the source's error is logged as a warning, the badges it would have
//! supplied are not shown, and the global ones (and the global subscriber badge) still are.
//! A Helix source (PR 3.1) plugs in as another [`BadgeSource`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use gpui_chat_core::{ChannelId, ChatEvent};
use gpui_chat_emotes::badges::snapshot;
use gpui_chat_emotes::{AnonymousBadgeSource, BadgeRegistry, BadgeSource, HttpError, ReqwestHttp};
use gpui_kit::{App, Global, Task};
use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::runtime;

/// The badge registry as a GPUI global. Views `observe_global::<Badges>` to learn that sets
/// changed, and read [`Badges::registry`] while shaping.
pub struct Badges {
    registry: Arc<BadgeRegistry>,
    changed: Arc<Notify>,
    /// Bumped on the UI thread whenever the loader stored a set; only its change matters.
    epoch: u64,
    _pump: Option<Task<()>>,
}

impl Global for Badges {}

impl Badges {
    /// Installs the global with the bundled global badges stored, and the task that turns the
    /// loader's "something changed" into a global update on the UI thread.
    pub fn install(cx: &mut App) {
        let registry = Arc::new(BadgeRegistry::new());
        Self::install_with(registry, cx);
    }

    fn install_with(registry: Arc<BadgeRegistry>, cx: &mut App) {
        match snapshot::bundled() {
            Ok(set) => registry.store_global(set),
            // A build problem (the snapshot is checked by a test), never a runtime condition.
            Err(error) => tracing::warn!(%error, "no bundled badges; global badges are not shown"),
        }
        let changed = Arc::new(Notify::new());
        let pump = {
            let changed = changed.clone();
            cx.spawn(async move |cx| {
                loop {
                    changed.notified().await;
                    // `global_mut` notifies the global's observers (the chat views).
                    cx.update(|cx| cx.global_mut::<Self>().epoch += 1);
                }
            })
        };
        cx.set_global(Self {
            registry,
            changed,
            epoch: 0,
            _pump: Some(pump),
        });
    }

    /// The registry views look badges up in.
    pub fn registry(&self) -> &Arc<BadgeRegistry> {
        &self.registry
    }

    /// The notifier a loader wakes after storing a set (tests drive it by hand).
    #[cfg(test)]
    pub fn changed_handle(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    /// What a [`BadgeService`] needs to store sets and tell the UI.
    fn handles(&self) -> (Arc<BadgeRegistry>, Arc<Notify>) {
        (self.registry.clone(), self.changed.clone())
    }
}

/// How far a channel's load got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Load {
    /// The last attempt failed.
    Incomplete,
    InFlight,
    Complete,
}

#[derive(Debug)]
struct Room {
    room_id: String,
    load: Load,
    /// The number of the latest attempt: a finishing attempt only records its outcome if no newer
    /// one was started meanwhile.
    token: u64,
}

#[derive(Default)]
struct State {
    next_token: u64,
    rooms: HashMap<ChannelId, Room>,
}

struct Inner {
    source: Arc<dyn BadgeSource>,
    registry: Arc<BadgeRegistry>,
    changed: Arc<Notify>,
    handle: Handle,
    state: Mutex<State>,
}

impl Inner {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Loads channel badges for the live source. Cheap to clone; all clones share one state.
#[derive(Clone)]
pub struct BadgeService {
    inner: Arc<Inner>,
}

impl BadgeService {
    /// A service over `source` that runs its requests on `handle` and stores what they return in
    /// `registry`, waking `changed` after each store.
    pub fn new(
        source: Arc<dyn BadgeSource>,
        registry: Arc<BadgeRegistry>,
        changed: Arc<Notify>,
        handle: Handle,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                source,
                registry,
                changed,
                handle,
                state: Mutex::default(),
            }),
        }
    }

    /// Starts the anonymous service on the backend runtime. `None` if the global is not
    /// installed or the HTTP client cannot be built (channel badges are then not shown).
    pub fn start(cx: &App) -> Option<Self> {
        let (registry, changed) = cx.try_global::<Badges>()?.handles();
        let http = match runtime::enter(cx, ReqwestHttp::new) {
            Ok(http) => http,
            Err(error) => {
                log_unavailable(&error);
                return None;
            }
        };
        let source = Arc::new(AnonymousBadgeSource::new(Arc::new(http)));
        let service = Self::new(
            source,
            registry,
            changed,
            cx.global::<runtime::Backend>().handle(),
        );
        service.load_global();
        Some(service)
    }

    /// Asks the source for the global badges (once, at start) and stores them over the bundled
    /// snapshot. A failure is a warning and leaves the snapshot in place.
    fn load_global(&self) {
        let inner = self.inner.clone();
        drop(self.inner.handle.spawn(async move {
            match inner.source.global().await {
                Ok(set) => {
                    inner.registry.store_global(set);
                    inner.changed.notify_one();
                }
                Err(error) => {
                    tracing::warn!(%error, "global badges unavailable; keeping the bundled ones");
                }
            }
        }));
    }

    /// Fetches `channel`'s badges for `room_id` now, whatever their state.
    fn load_channel(&self, channel: ChannelId, room_id: String) {
        let token = {
            let mut state = self.inner.state();
            state.next_token += 1;
            let token = state.next_token;
            state.rooms.insert(
                channel.clone(),
                Room {
                    room_id: room_id.clone(),
                    load: Load::InFlight,
                    token,
                },
            );
            token
        };
        let inner = self.inner.clone();
        drop(self.inner.handle.spawn(async move {
            let result = inner.source.channel(&room_id).await;
            if let Err(error) = &result {
                tracing::warn!(channel = %channel, %error, "channel badges unavailable");
            }
            let ok = result.is_ok();
            // The channel may have been closed (or its room id changed) while this loaded.
            let mut state = inner.state();
            let Some(room) = state.rooms.get_mut(&channel) else {
                return;
            };
            if room.room_id != room_id {
                return;
            }
            if let Ok(set) = result {
                inner.registry.store_channel(&channel, set);
                inner.changed.notify_one();
            }
            if room.token == token {
                room.load = if ok { Load::Complete } else { Load::Incomplete };
            }
        }));
    }

    /// The channel was closed: forget its room and badges.
    pub fn forget(&self, channel: &ChannelId) {
        self.inner.state().rooms.remove(channel);
        self.inner.registry.forget_channel(channel);
    }

    /// Looks at an event on its way to the UI: a `ROOMSTATE` with a new room id (or repeating
    /// one whose last load failed) starts loading the channel's badges.
    pub fn observe(&self, event: &ChatEvent) {
        let ChatEvent::RoomState { channel, update } = event else {
            return;
        };
        let Some(room_id) = &update.room_id else {
            return;
        };
        let needed = match self.inner.state().rooms.get(channel) {
            Some(room) => room.room_id != *room_id || room.load == Load::Incomplete,
            None => true,
        };
        if needed {
            self.load_channel(channel.clone(), room_id.clone());
        }
    }
}

fn log_unavailable(error: &HttpError) {
    tracing::warn!(%error, "channel badges unavailable");
}

#[cfg(test)]
mod tests;
