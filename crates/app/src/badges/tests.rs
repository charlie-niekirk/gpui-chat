use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_chat_core::{Badge, BadgeId, ImageUrls, RoomStateUpdate};
use gpui_chat_emotes::{BadgeError, BadgeFuture, BadgeSet};
use gpui_kit::TestAppContext;

use super::*;

/// A source that answers every channel with one `subscriber/0` badge titled after the room, or
/// fails while `down`. Records the room ids it was asked for.
#[derive(Default)]
struct FakeSource {
    requests: Mutex<Vec<String>>,
    down: AtomicBool,
    /// What `global()` answers (an error if `None`).
    global: Mutex<Option<BadgeSet>>,
    global_calls: std::sync::atomic::AtomicUsize,
    /// Holds channel answers back until released (to catch a channel closed mid-load).
    hold: AtomicBool,
}

impl FakeSource {
    fn requested(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl BadgeSource for FakeSource {
    fn global(&self) -> BadgeFuture<'_> {
        self.global_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let set = self.global.lock().unwrap().clone();
        Box::pin(async move { set.ok_or(BadgeError::Status(503)) })
    }

    fn channel<'a>(&'a self, room_id: &'a str) -> BadgeFuture<'a> {
        self.requests.lock().unwrap().push(room_id.to_owned());
        Box::pin(async move {
            while self.hold.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if self.down.load(Ordering::SeqCst) {
                return Err(BadgeError::Status(503));
            }
            let mut set = BadgeSet::new();
            set.insert(Badge {
                id: BadgeId::new("subscriber", "0"),
                title: format!("Sub of {room_id}"),
                urls: ImageUrls::new(),
            });
            Ok(set)
        })
    }
}

struct Fixture {
    service: BadgeService,
    source: Arc<FakeSource>,
    registry: Arc<BadgeRegistry>,
    changed: Arc<Notify>,
}

fn fixture() -> Fixture {
    let source = Arc::new(FakeSource::default());
    let registry = Arc::new(BadgeRegistry::new());
    let changed = Arc::new(Notify::new());
    let service = BadgeService::new(
        source.clone(),
        registry.clone(),
        changed.clone(),
        Handle::current(),
    );
    Fixture {
        service,
        source,
        registry,
        changed,
    }
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

fn title(registry: &BadgeRegistry, channel: &str) -> Option<String> {
    registry
        .lookup_str(&ChannelId::new(channel), "subscriber", "0")
        .map(|badge| badge.title.clone())
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

fn load_of(f: &Fixture, channel: &str) -> Option<Load> {
    f.service
        .inner
        .state()
        .rooms
        .get(&ChannelId::new(channel))
        .map(|room| room.load)
}

#[tokio::test]
async fn room_state_loads_the_channels_badges_once_per_room_id() {
    let f = fixture();
    assert_eq!(title(&f.registry, "forsen"), None);
    f.service.observe(&room("forsen", "22484632"));
    f.service.observe(&room("forsen", "22484632"));
    eventually(|| title(&f.registry, "forsen").is_some()).await;
    assert_eq!(
        title(&f.registry, "forsen").as_deref(),
        Some("Sub of 22484632")
    );
    eventually(|| load_of(&f, "forsen") == Some(Load::Complete)).await;
    // Complete: a repeat (a reconnect re-joins) asks for nothing; a new room id is a new load.
    f.service.observe(&room("forsen", "22484632"));
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(f.source.requested(), ["22484632"]);
    f.service.observe(&room("forsen", "1"));
    eventually(|| f.source.requested().len() == 2).await;
    // The UI was told (a stored set wakes the notify; a permit is waiting).
    tokio::time::timeout(Duration::from_secs(1), f.changed.notified())
        .await
        .expect("the UI is woken");
}

#[tokio::test]
async fn other_events_and_rooms_without_an_id_load_nothing() {
    let f = fixture();
    f.service.observe(&ChatEvent::RoomState {
        channel: ChannelId::new("a"),
        update: RoomStateUpdate::default(),
    });
    f.service.observe(&ChatEvent::Notice {
        channel: None,
        msg_id: None,
        text: "x".into(),
    });
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(f.source.requested().is_empty());
}

#[tokio::test]
async fn a_failed_load_degrades_silently_and_is_retried_when_room_state_repeats() {
    let f = fixture();
    f.source.down.store(true, Ordering::SeqCst);
    f.service.observe(&room("forsen", "22484632"));
    eventually(|| load_of(&f, "forsen") == Some(Load::Incomplete)).await;
    // No badges, no error to the UI: the channel just has none of its own.
    assert_eq!(title(&f.registry, "forsen"), None);

    f.source.down.store(false, Ordering::SeqCst);
    f.service.observe(&room("forsen", "22484632"));
    eventually(|| title(&f.registry, "forsen").is_some()).await;
    eventually(|| load_of(&f, "forsen") == Some(Load::Complete)).await;
    assert_eq!(f.source.requested().len(), 2);
}

#[tokio::test]
async fn a_load_in_flight_is_not_duplicated() {
    let f = fixture();
    f.source.hold.store(true, Ordering::SeqCst);
    f.service.observe(&room("forsen", "22484632"));
    f.service.observe(&room("forsen", "22484632"));
    f.service.observe(&room("forsen", "22484632"));
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(f.source.requested().len(), 1);
    f.source.hold.store(false, Ordering::SeqCst);
    eventually(|| load_of(&f, "forsen") == Some(Load::Complete)).await;
    assert_eq!(f.source.requested().len(), 1);
}

#[tokio::test]
async fn closing_a_channel_forgets_its_badges_and_discards_a_load_still_running() {
    let f = fixture();
    f.service.observe(&room("forsen", "22484632"));
    f.service.observe(&room("xqc", "71092938"));
    eventually(|| title(&f.registry, "forsen").is_some() && title(&f.registry, "xqc").is_some())
        .await;
    f.service.forget(&ChannelId::new("forsen"));
    assert_eq!(title(&f.registry, "forsen"), None);
    assert!(
        title(&f.registry, "xqc").is_some(),
        "other channels are kept"
    );
    assert_eq!(load_of(&f, "forsen"), None);

    // Closed while its load is still running: the late answer is not stored.
    f.source.hold.store(true, Ordering::SeqCst);
    f.service.observe(&room("late", "5"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    f.service.forget(&ChannelId::new("late"));
    f.source.hold.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(title(&f.registry, "late"), None);
}

#[gpui_kit::test]
fn install_stores_the_bundled_global_badges_and_a_change_bumps_the_epoch(cx: &mut TestAppContext) {
    cx.update(Badges::install);
    let (registry, changed) = cx.update(|cx| cx.global::<Badges>().handles());
    // Moderator, VIP and the rest are there before any network.
    let moderator = registry
        .lookup(&ChannelId::new("anyone"), &BadgeId::new("moderator", "1"))
        .expect("bundled moderator badge");
    assert_eq!(moderator.title, "Moderator");

    assert_eq!(cx.update(|cx| cx.global::<Badges>().epoch), 0);
    changed.notify_one();
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.global::<Badges>().epoch), 1);
}

fn global_set(title: &str) -> BadgeSet {
    let mut set = BadgeSet::new();
    set.insert(Badge {
        id: BadgeId::new("moderator", "1"),
        title: title.to_owned(),
        urls: ImageUrls::new(),
    });
    set
}

fn moderator(registry: &BadgeRegistry) -> Option<String> {
    registry
        .lookup_str(&ChannelId::new("any"), "moderator", "1")
        .map(|badge| badge.title.clone())
}

#[tokio::test]
async fn the_sources_global_badges_replace_the_bundled_ones_and_wake_the_ui() {
    let f = fixture();
    // The bundled snapshot was installed first.
    f.registry.store_global(snapshot::bundled().unwrap());
    assert_eq!(moderator(&f.registry).as_deref(), Some("Moderator"));
    let before = f.registry.version(&ChannelId::new("any"));

    *f.source.global.lock().unwrap() = Some(global_set("Source Mod"));
    f.service.load_global();
    eventually(|| moderator(&f.registry).as_deref() == Some("Source Mod")).await;
    assert_eq!(
        f.source.global_calls.load(Ordering::SeqCst),
        1,
        "asked once"
    );
    assert!(f.registry.version(&ChannelId::new("any")) > before);
    tokio::time::timeout(Duration::from_secs(1), f.changed.notified())
        .await
        .expect("the UI is woken");
}

#[tokio::test]
async fn a_failing_global_load_keeps_the_bundled_badges() {
    let f = fixture();
    f.registry.store_global(snapshot::bundled().unwrap());
    let before = f.registry.version(&ChannelId::new("any"));
    // `global` is `None`: the source answers with an error.
    f.service.load_global();
    eventually(|| f.source.global_calls.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(moderator(&f.registry).as_deref(), Some("Moderator"));
    assert_eq!(f.registry.version(&ChannelId::new("any")), before);
}
