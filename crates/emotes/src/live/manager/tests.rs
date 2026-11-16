use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::{ChannelId, EmoteProvider, WordResolver};
use serde_json::{Value, json};

use super::*;
use crate::badges::BadgeRegistry;
use crate::live::testserver::{Harness, server_greeting};
use crate::loader::ProviderResults;
use crate::set::EmoteScope;
use crate::testutil::FakeHttp;

const HELLO: &str = r#"{"op":1,"d":{"heartbeat_interval":45000,"session_id":"s"}}"#;

#[derive(Default)]
struct Recording {
    notices: Mutex<Vec<(String, String)>>,
    cosmetic_pokes: Mutex<usize>,
}

impl LiveSink for Recording {
    fn notice(&self, channel: &ChannelId, text: String) {
        self.notices
            .lock()
            .unwrap()
            .push((channel.to_string(), text));
    }
    fn cosmetics_changed(&self) {
        *self.cosmetic_pokes.lock().unwrap() += 1;
    }
}

struct World {
    live: LiveEmotes,
    registry: Arc<EmoteRegistry>,
    cosmetics: Arc<Cosmetics>,
    sink: Arc<Recording>,
    seven: Harness,
    bttv: Harness,
}

fn emote_json(id: &str, name: &str) -> Value {
    json!({"id": id, "name": name, "flags": 0, "data": {"name": name, "flags": 0, "animated": false,
        "host": {"url": format!("//cdn.7tv.app/emote/{id}"), "files": [{"name": "1x.webp", "width": 28, "height": 28, "format": "WEBP"}]}}})
}

fn seven_set(emotes: &[(&str, &str)]) -> EmoteSet {
    let body = json!({"id": "SET1", "name": "S", "emotes": emotes.iter().map(|(i, n)| emote_json(i, n)).collect::<Vec<_>>()});
    crate::seventv::parse_set(&serde_json::to_vec(&body).unwrap(), Some("USER1"), Some(0))
        .unwrap()
        .0
}

fn bttv_set(names: &[(&str, &str)]) -> EmoteSet {
    let emotes = names
        .iter()
        .map(|(id, code)| json!({"id": id, "code": code, "imageType": "png"}))
        .collect::<Vec<_>>();
    crate::bttv::parse_channel(
        &serde_json::to_vec(&json!({"channelEmotes": emotes, "sharedEmotes": []})).unwrap(),
    )
    .unwrap()
}

async fn world(emotes: Providers, http: FakeHttp) -> World {
    let seven = server_greeting(Some(HELLO.to_owned())).await;
    let bttv = server_greeting(None).await;
    let registry = Arc::new(EmoteRegistry::new(emotes));
    let cosmetics = Arc::new(Cosmetics::new(
        Arc::new(BadgeRegistry::new()),
        CosmeticOptions {
            paints: false,
            seventv_badges: false,
            ..CosmeticOptions::default()
        },
    ));
    let sink = Arc::new(Recording::default());
    let live = LiveEmotes::new(
        &Handle::current(),
        registry.clone(),
        cosmetics.clone(),
        SevenTvClient::with_base_url(Arc::new(http), "https://7tv.test/v3"),
        sink.clone(),
        Endpoints {
            seven_tv: seven.url.clone(),
            bttv: bttv.url.clone(),
        },
        Backoff {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(50),
            healthy_after: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(2),
        },
    );
    World {
        live,
        registry,
        cosmetics,
        sink,
        seven,
        bttv,
    }
}

fn store(registry: &EmoteRegistry, channel: &ChannelId, seven: EmoteSet, bttv: EmoteSet) {
    registry.store_channel(
        channel,
        &ProviderResults {
            seven_tv: Ok(seven),
            bttv: Ok(bttv),
            ffz: Ok(EmoteSet::empty(EmoteProvider::Ffz, EmoteScope::Channel)),
            requested: Providers::ALL,
        },
    );
}

async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

fn frames(harness: &Harness, connection: usize) -> Vec<Value> {
    harness
        .received
        .lock()
        .unwrap()
        .get(connection)
        .map_or_else(Vec::new, |frames| {
            frames
                .iter()
                .filter_map(|f| serde_json::from_str(f).ok())
                .collect()
        })
}

fn resolves(registry: &EmoteRegistry, channel: &ChannelId, word: &str) -> bool {
    registry.resolver(channel).resolve(word).is_some()
}

fn update_frame(set: &str, body: Value) -> String {
    let mut body = body;
    body["id"] = json!(set);
    json!({"op": 0, "d": {"type": "emote_set.update", "body": body}}).to_string()
}

async fn notices(world: &World, count: usize) -> Vec<(String, String)> {
    eventually(|| world.sink.notices.lock().unwrap().len() >= count).await;
    world.sink.notices.lock().unwrap().clone()
}

#[tokio::test]
async fn a_7tv_add_and_remove_reach_the_channels_set_and_make_one_quiet_line() {
    let world = world(Providers::ALL, FakeHttp::new()).await;
    let channel = ChannelId::new("forsen");
    store(
        &world.registry,
        &channel,
        seven_set(&[("E1", "Old")]),
        bttv_set(&[]),
    );
    let origin = world
        .registry
        .channel_set(&channel, EmoteProvider::SevenTv)
        .unwrap()
        .origin()
        .cloned();
    world
        .live
        .open_channel(&channel, "22484632", origin.as_ref());

    // It subscribes to the set and the owner, after the server's Hello.
    eventually(|| frames(&world.seven, 0).len() >= 2).await;
    let subs: Vec<String> = frames(&world.seven, 0)
        .iter()
        .map(|f| {
            format!(
                "{}:{}",
                f["d"]["type"].as_str().unwrap(),
                f["d"]["condition"]["object_id"].as_str().unwrap()
            )
        })
        .collect();
    assert!(
        subs.contains(&"emote_set.update:SET1".to_owned()),
        "{subs:?}"
    );
    assert!(subs.contains(&"user.update:USER1".to_owned()), "{subs:?}");
    // Cosmetics are off in this world: no cosmetic subscriptions.
    assert!(
        frames(&world.seven, 0)
            .iter()
            .all(|f| f["d"]["type"] != "cosmetic.create")
    );

    assert!(resolves(&world.registry, &channel, "Old"));
    assert!(!resolves(&world.registry, &channel, "PogU"));
    world.seven.push.send(update_frame("SET1", json!({"pushed": [{"key": "emotes", "index": 1, "type": "object", "value": emote_json("E2", "PogU")}]}))).unwrap();
    world.seven.push.send(update_frame("SET1", json!({"pulled": [{"key": "emotes", "index": 0, "type": "object", "old_value": {"id": "E1", "name": "Old"}}]}))).unwrap();
    eventually(|| resolves(&world.registry, &channel, "PogU")).await;
    eventually(|| !resolves(&world.registry, &channel, "Old")).await;

    // Both changes, one line (they arrived within the window).
    let lines = notices(&world, 1).await;
    assert_eq!(
        lines,
        [(
            "forsen".to_owned(),
            "7TV: added PogU · removed Old".to_owned()
        )]
    );
}

#[tokio::test]
async fn bttv_create_update_and_delete_apply_and_unknown_emotes_are_ignored() {
    let world = world(Providers::ALL, FakeHttp::new()).await;
    let channel = ChannelId::new("forsen");
    store(
        &world.registry,
        &channel,
        seven_set(&[]),
        bttv_set(&[("B1", "Kept"), ("B2", "Doomed")]),
    );
    world.live.open_channel(&channel, "22484632", None);
    eventually(|| !frames(&world.bttv, 0).is_empty()).await;
    assert_eq!(
        frames(&world.bttv, 0)[0],
        json!({"name": "join_channel", "data": {"name": "twitch:22484632"}})
    );

    let create = json!({"name": "emote_create", "data": {"channel": "twitch:22484632", "emote": {"id": "B3", "code": "NewOne", "imageType": "png"}}});
    let rename = json!({"name": "emote_update", "data": {"channel": "twitch:22484632", "emote": {"id": "B1", "code": "Renamed", "imageType": "png"}}});
    let delete =
        json!({"name": "emote_delete", "data": {"channel": "twitch:22484632", "emoteId": "B2"}});
    let ghost =
        json!({"name": "emote_delete", "data": {"channel": "twitch:22484632", "emoteId": "NOPE"}});
    let elsewhere = json!({"name": "emote_create", "data": {"channel": "twitch:1", "emote": {"id": "B9", "code": "Elsewhere", "imageType": "png"}}});
    for frame in [create, rename, delete, ghost, elsewhere] {
        world.bttv.push.send(frame.to_string()).unwrap();
    }
    eventually(|| resolves(&world.registry, &channel, "NewOne")).await;
    eventually(|| !resolves(&world.registry, &channel, "Doomed")).await;
    assert!(resolves(&world.registry, &channel, "Renamed"));
    assert!(
        !resolves(&world.registry, &channel, "Kept"),
        "the old name is gone"
    );
    assert!(!resolves(&world.registry, &channel, "Elsewhere"));

    let lines = notices(&world, 1).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        lines[0].1,
        "BTTV: added NewOne · removed Doomed · renamed Kept → Renamed"
    );
}

#[tokio::test]
async fn a_set_switch_loads_the_new_set_and_resubscribes() {
    let new_set = json!({"id": "SET2", "name": "Fresh Set", "emotes": [emote_json("N1", "Brand")]});
    let http = FakeHttp::new().body(
        "https://7tv.test/v3/emote-sets/SET2",
        200,
        serde_json::to_vec(&new_set).unwrap(),
    );
    let world = world(Providers::ALL, http).await;
    let channel = ChannelId::new("forsen");
    store(
        &world.registry,
        &channel,
        seven_set(&[("E1", "Old")]),
        bttv_set(&[]),
    );
    let origin = world
        .registry
        .channel_set(&channel, EmoteProvider::SevenTv)
        .unwrap()
        .origin()
        .cloned();
    world
        .live
        .open_channel(&channel, "22484632", origin.as_ref());
    eventually(|| frames(&world.seven, 0).len() >= 2).await;

    // A change of another connection (index 1) is not ours; ours (index 0) is.
    let switch = |index: u64| {
        json!({"op": 0, "d": {"type": "user.update", "body": {"id": "USER1", "kind": 1,
        "updated": [{"key": "connections", "index": index, "type": "array", "value": [{"key": "emote_set_id", "old_value": "SET1", "value": "SET2"}]}]}}}).to_string()
    };
    world.seven.push.send(switch(1)).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        resolves(&world.registry, &channel, "Old"),
        "someone else's connection"
    );
    world.seven.push.send(switch(0)).unwrap();
    eventually(|| resolves(&world.registry, &channel, "Brand")).await;
    assert!(!resolves(&world.registry, &channel, "Old"));
    assert_eq!(
        notices(&world, 1).await[0].1,
        "7TV: switched to emote set Fresh Set (1 emotes)"
    );
    // Now listening to the new set, not the old one.
    eventually(|| {
        frames(&world.seven, 0)
            .iter()
            .any(|f| f["op"] == 35 && f["d"]["condition"]["object_id"] == "SET2")
            && frames(&world.seven, 0)
                .iter()
                .any(|f| f["op"] == 36 && f["d"]["condition"]["object_id"] == "SET1")
    })
    .await;
    // And an update to the new set applies.
    world.seven.push.send(update_frame("SET2", json!({"pushed": [{"key": "emotes", "index": 1, "type": "object", "value": emote_json("N2", "Later")}]}))).unwrap();
    eventually(|| resolves(&world.registry, &channel, "Later")).await;
}

#[tokio::test]
async fn closing_a_channel_unsubscribes_and_the_last_one_closes_the_sockets() {
    let world = world(Providers::ALL, FakeHttp::new()).await;
    let channel = ChannelId::new("forsen");
    store(
        &world.registry,
        &channel,
        seven_set(&[("E1", "Old")]),
        bttv_set(&[]),
    );
    let origin = world
        .registry
        .channel_set(&channel, EmoteProvider::SevenTv)
        .unwrap()
        .origin()
        .cloned();
    world
        .live
        .open_channel(&channel, "22484632", origin.as_ref());
    eventually(|| frames(&world.seven, 0).len() >= 2 && !frames(&world.bttv, 0).is_empty()).await;

    world.live.close_channel(&channel);
    eventually(|| {
        let f = frames(&world.seven, 0);
        f.iter().filter(|f| f["op"] == 36).count() == 2
    })
    .await;
    eventually(|| {
        frames(&world.bttv, 0)
            .iter()
            .any(|f| f["name"] == "part_channel")
    })
    .await;
    // Nothing left to listen to: no reconnect after the close.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*world.seven.accepted.lock().unwrap(), 1);
    assert_eq!(*world.bttv.accepted.lock().unwrap(), 1);
    // An update that still arrives for the closed channel changes and says nothing.
    assert!(world.sink.notices.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_provider_that_is_off_is_never_connected_to() {
    let only_bttv = Providers {
        seven_tv: false,
        bttv: true,
        ffz: true,
    };
    let world = world(only_bttv, FakeHttp::new()).await;
    let channel = ChannelId::new("forsen");
    store(
        &world.registry,
        &channel,
        seven_set(&[("E1", "Old")]),
        bttv_set(&[]),
    );
    let origin = world
        .registry
        .channel_set(&channel, EmoteProvider::SevenTv)
        .unwrap()
        .origin()
        .cloned();
    world
        .live
        .open_channel(&channel, "22484632", origin.as_ref());
    eventually(|| *world.bttv.accepted.lock().unwrap() == 1).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(*world.seven.accepted.lock().unwrap(), 0, "7TV is off");

    // Switch 7TV on and BTTV off while the channel stays open.
    world.live.configure(
        Providers {
            seven_tv: true,
            bttv: false,
            ffz: true,
        },
        CosmeticOptions {
            paints: false,
            seventv_badges: false,
            ..CosmeticOptions::default()
        },
    );
    eventually(|| *world.seven.accepted.lock().unwrap() == 1).await;
    eventually(|| {
        frames(&world.bttv, 0)
            .iter()
            .any(|f| f["name"] == "part_channel")
    })
    .await;
    assert_eq!(*world.bttv.accepted.lock().unwrap(), 1);
}

#[tokio::test]
async fn cosmetics_are_subscribed_only_when_wanted_and_feed_the_store() {
    let world = world(
        Providers {
            seven_tv: false,
            bttv: false,
            ffz: false,
        },
        FakeHttp::new(),
    )
    .await;
    let channel = ChannelId::new("forsen");
    world.live.open_channel(&channel, "22484632", None);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        *world.seven.accepted.lock().unwrap(),
        0,
        "no emotes, no paints: no socket"
    );

    world.live.configure(
        Providers {
            seven_tv: false,
            bttv: false,
            ffz: false,
        },
        CosmeticOptions::default(),
    );
    eventually(|| frames(&world.seven, 0).len() >= 3).await;
    let types: Vec<String> = frames(&world.seven, 0)
        .iter()
        .map(|f| f["d"]["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        {
            let mut t = types.clone();
            t.sort();
            t
        },
        [
            "cosmetic.create",
            "entitlement.create",
            "entitlement.delete"
        ]
    );
    let condition = &frames(&world.seven, 0)[0]["d"]["condition"];
    assert_eq!(
        condition,
        &json!({"platform": "TWITCH", "ctx": "channel", "id": "22484632"})
    );

    // A paint definition and an entitlement make a name paintable.
    let paint = json!({"op": 0, "d": {"type": "cosmetic.create", "body": {"id": "P1", "kind": 10, "object": {"kind": "PAINT", "data":
        {"id": "P1", "name": "N", "function": "LINEAR_GRADIENT", "angle": 90, "repeat": false, "image_url": "", "stops": [{"at": 0, "color": -1}, {"at": 1, "color": -16777216}], "shadows": []}}}}});
    let entitlement = json!({"op": 0, "d": {"type": "entitlement.create", "body": {"id": "0", "kind": 10, "object": {"kind": "PAINT", "ref_id": "P1",
        "user": {"id": "U", "connections": [{"id": "98712408", "platform": "TWITCH"}]}}}}});
    world.seven.push.send(paint.to_string()).unwrap();
    world.seven.push.send(entitlement.to_string()).unwrap();
    eventually(|| {
        world.cosmetics.options().paints || *world.sink.cosmetic_pokes.lock().unwrap() >= 2
    })
    .await;
    eventually(|| *world.sink.cosmetic_pokes.lock().unwrap() >= 2).await;
    // Paints are on now (options were switched), so the paint is there.
    world.cosmetics.set_options(CosmeticOptions::default());
    assert!(world.cosmetics.paint("98712408").is_some());
}
