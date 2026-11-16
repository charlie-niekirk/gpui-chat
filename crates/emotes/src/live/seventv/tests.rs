use serde_json::Value;

use super::*;
use crate::cosmetics::PaintFill;

fn lines(name: &str) -> Vec<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/emotes")
        .join(name);
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn real() -> Vec<Frame> {
    lines("7tv-events-real.jsonl")
        .iter()
        .map(|l| parse_frame(l))
        .collect()
}

fn synthetic() -> Vec<Frame> {
    lines("7tv-events-synthetic.jsonl")
        .iter()
        .map(|l| parse_frame(l))
        .collect()
}

#[test]
fn the_session_frames_are_recognised() {
    let frames = real();
    assert_eq!(
        frames[0],
        Frame::Hello {
            heartbeat: Duration::from_millis(47_625)
        }
    );
    assert_eq!(frames[1], Frame::Heartbeat);
    assert_eq!(frames[2], Frame::Ack);
    let s = synthetic();
    assert_eq!(s[6], Frame::Reconnect);
    assert_eq!(s[7], Frame::EndOfStream(Some(4000)));
    assert!(matches!(&s[8], Frame::Error(text) if text.contains("bad subscription")));
    assert_eq!(parse_frame("not json"), Frame::Ignored);
    assert_eq!(parse_frame(r#"{"op":99}"#), Frame::Ignored);
    assert_eq!(
        parse_frame(r#"{"op":0,"d":{"type":"emote.create","body":{}}}"#),
        Frame::Ignored
    );
}

#[test]
fn a_pushed_emote_is_an_addition_with_its_images() {
    let Frame::Dispatch(Dispatch::EmoteSet { set_id, changes }) = &real()[3] else {
        panic!("{:?}", real()[3]);
    };
    assert_eq!(set_id, "01KDJXA8Q4WN2CW7N9JX4X7DWY");
    let [EmoteChange::Added(entry)] = changes.as_slice() else {
        panic!("{changes:?}");
    };
    assert_eq!(&*entry.emote.name, "Tutturu");
    assert!(
        entry
            .emote
            .urls
            .get(gpui_chat_core::ImageScale::X1)
            .unwrap()
            .ends_with("/1x.webp")
    );
    assert!(!entry.emote.zero_width);
}

#[test]
fn pulled_and_updated_emotes_are_removals_and_renames() {
    let s = synthetic();
    let Frame::Dispatch(Dispatch::EmoteSet { changes, .. }) = &s[0] else {
        panic!()
    };
    // Only fields about emotes count.
    assert_eq!(
        changes,
        &[EmoteChange::Removed {
            id: "EMOTE_OLD".into(),
            name: Some("OldOne".into())
        }]
    );
    let Frame::Dispatch(Dispatch::EmoteSet { changes, .. }) = &s[1] else {
        panic!()
    };
    // A real rename, but a flag-only change is no rename.
    assert_eq!(
        changes,
        &[EmoteChange::Renamed {
            id: "EMOTE_A".into(),
            from: Some("Alpha".into()),
            to: "Beta".into()
        }]
    );
}

#[test]
fn a_connection_switching_its_set_is_a_set_switch() {
    let s = synthetic();
    assert_eq!(
        s[2],
        Frame::Dispatch(Dispatch::SetSwitch {
            user_id: "USER1".into(),
            set_id: Some("SET2".into()),
            connection: Some(1),
        })
    );
    // Cleared: no active set.
    assert_eq!(
        s[3],
        Frame::Dispatch(Dispatch::SetSwitch {
            user_id: "USER1".into(),
            set_id: None,
            connection: Some(0)
        })
    );
    // Any other user change is not interesting.
    assert_eq!(s[4], Frame::Ignored);
}

#[test]
fn cosmetics_and_entitlements_carry_what_the_chat_needs() {
    let r = real();
    let Frame::Dispatch(Dispatch::Paint(paint)) = &r[4] else {
        panic!("{:?}", r[4])
    };
    assert!(matches!(
        paint.fill,
        PaintFill::Linear { .. } | PaintFill::Radial { .. }
    ));
    let Frame::Dispatch(Dispatch::Badge(badge)) = &r[5] else {
        panic!("{:?}", r[5])
    };
    assert_eq!(badge.id.set.as_ref(), "seventv");
    let Frame::Dispatch(Dispatch::Entitlement {
        granted,
        kind,
        twitch_id,
        ..
    }) = &r[6]
    else {
        panic!("{:?}", r[6])
    };
    assert!(*granted);
    assert_eq!(*kind, Cosmetic::Paint);
    assert_eq!(twitch_id.as_deref(), Some("270252923"));
    // A user with several connections: the Twitch one is used.
    let Frame::Dispatch(Dispatch::Entitlement {
        twitch_id, kind, ..
    }) = &r[7]
    else {
        panic!()
    };
    assert_eq!(*kind, Cosmetic::Badge);
    assert_eq!(twitch_id.as_deref(), Some("751253701"));
    // A delete is the same shape, not granted.
    let Frame::Dispatch(Dispatch::Entitlement {
        granted,
        ref_id,
        twitch_id,
        ..
    }) = &synthetic()[5]
    else {
        panic!()
    };
    assert!(!granted);
    assert_eq!(
        (ref_id.as_str(), twitch_id.as_deref()),
        ("PAINT1", Some("98712408"))
    );
}

#[test]
fn subscriptions_become_the_frames_the_server_expects() {
    let frames = Subscription::EmoteSet("S".into()).subscribe_frames();
    assert_eq!(frames.len(), 1);
    let v: Value = serde_json::from_str(&frames[0]).unwrap();
    assert_eq!(v["op"], 35);
    assert_eq!(v["d"]["type"], "emote_set.update");
    assert_eq!(v["d"]["condition"]["object_id"], "S");

    let user = Subscription::User("U".into()).unsubscribe_frames();
    let v: Value = serde_json::from_str(&user[0]).unwrap();
    assert_eq!(
        (v["op"].as_u64(), v["d"]["type"].as_str()),
        (Some(36), Some("user.update"))
    );

    let cosmetics = Subscription::Cosmetics("123".into()).subscribe_frames();
    let types: Vec<String> = cosmetics
        .iter()
        .map(|f| {
            serde_json::from_str::<Value>(f).unwrap()["d"]["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        types,
        [
            "cosmetic.create",
            "entitlement.create",
            "entitlement.delete"
        ]
    );
    let v: Value = serde_json::from_str(&cosmetics[0]).unwrap();
    assert_eq!(
        v["d"]["condition"],
        serde_json::json!({"platform": "TWITCH", "ctx": "channel", "id": "123"})
    );
}
