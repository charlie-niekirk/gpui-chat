//! The BetterTTV socket (`wss://sockets.betterttv.net/ws`): frames in, [`Event`]s out. Pure
//! parsing, tested against `fixtures/emotes/bttv-events.jsonl`.
//!
//! The client joins a channel with `{"name": "join_channel", "data": {"name": "twitch:<room id>"}}`
//! (and leaves with `part_channel`); the server then pushes `emote_create`, `emote_update`
//! (`data: {channel, emote}`) and `emote_delete` (`data: {channel, emoteId}`) for it. Emotes are
//! the objects the REST API describes (`id`, `code`, `imageType`, ...); their images are the CDN
//! URLs built from the id, exactly as for the REST emotes.

use serde_json::{Value, json};

use crate::bttv::parse_live_emote;
use crate::set::SetEmote;

pub const DEFAULT_URL: &str = "wss://sockets.betterttv.net/ws";

/// The name the socket knows a Twitch channel by.
fn channel_name(room_id: &str) -> String {
    format!("twitch:{room_id}")
}

/// The frame that starts the events of a channel.
pub fn join_frame(room_id: &str) -> String {
    json!({ "name": "join_channel", "data": { "name": channel_name(room_id) } }).to_string()
}

/// The frame that stops them.
pub fn part_frame(room_id: &str) -> String {
    json!({ "name": "part_channel", "data": { "name": channel_name(room_id) } }).to_string()
}

/// A channel's emote event. `room_id` is the Twitch room id.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A new emote (`emote_create`).
    Created {
        room_id: String,
        emote: SetEmote,
    },
    /// A changed emote (`emote_update`). The payload is often just `{id, code}`, so only the new
    /// code is certain; `full` is the emote as described when the payload carried its image type
    /// (enough to add it if it is unknown here).
    Updated {
        room_id: String,
        id: String,
        code: String,
        full: Option<SetEmote>,
    },
    Deleted {
        room_id: String,
        emote_id: String,
    },
}

/// Parses one text frame: `None` for anything this client does not use.
pub fn parse_frame(text: &str) -> Option<Event> {
    let frame: Value = serde_json::from_str(text).ok()?;
    let name = frame.get("name")?.as_str()?;
    let data = frame.get("data")?;
    let room_id = data
        .get("channel")?
        .as_str()?
        .strip_prefix("twitch:")?
        .to_owned();
    match name {
        "emote_create" => Some(Event::Created {
            room_id,
            emote: parse_live_emote(data.get("emote")?)?,
        }),
        "emote_update" => {
            let emote = data.get("emote")?;
            let has_image = emote.get("imageType").is_some() || emote.get("animated").is_some();
            Some(Event::Updated {
                room_id,
                id: emote.get("id")?.as_str()?.to_owned(),
                code: emote.get("code")?.as_str()?.to_owned(),
                full: has_image.then(|| parse_live_emote(emote)).flatten(),
            })
        }
        "emote_delete" => Some(Event::Deleted {
            room_id,
            emote_id: data.get("emoteId")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::ImageScale;

    use super::*;

    fn lines() -> Vec<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/emotes/bttv-events.jsonl");
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn create_and_update_carry_an_emote_with_cdn_images() {
        let lines = lines();
        let Some(Event::Created { room_id, emote }) = parse_frame(&lines[0]) else {
            panic!("{:?}", parse_frame(&lines[0]));
        };
        assert_eq!(room_id, "22484632");
        assert_eq!(&*emote.emote.name, "PogU");
        assert_eq!(emote.emote.id.as_str(), "5e0fa9d40550d42106b8a489");
        assert_eq!(
            emote.emote.urls.get(ImageScale::X2),
            Some("https://cdn.betterttv.net/emote/5e0fa9d40550d42106b8a489/2x.webp")
        );
        assert!(!emote.emote.animated);
        // An update names the emote by id and carries its new code; a gif is animated even
        // without the flag.
        let Some(Event::Updated { id, code, full, .. }) = parse_frame(&lines[1]) else {
            panic!("{:?}", parse_frame(&lines[1]))
        };
        assert_eq!(
            (id.as_str(), code.as_str()),
            ("5f1b0f4e0c6dbd6d1f8b1234", "RenamedGif")
        );
        assert!(full.expect("it says what image it has").emote.animated);
        // A bare `{id, code}` update is just a rename.
        let Some(Event::Updated { code, full, .. }) = parse_frame(&lines[6]) else {
            panic!()
        };
        assert_eq!(code, "JustCode");
        assert!(full.is_none());
    }

    #[test]
    fn delete_names_the_emote_by_id() {
        assert_eq!(
            parse_frame(&lines()[2]),
            Some(Event::Deleted {
                room_id: "22484632".into(),
                emote_id: "5e0fa9d40550d42106b8a489".into()
            })
        );
    }

    #[test]
    fn other_messages_and_other_platforms_are_ignored() {
        let lines = lines();
        for line in &lines[3..6] {
            assert_eq!(parse_frame(line), None, "{line}");
        }
        assert_eq!(parse_frame("garbage"), None);
    }

    #[test]
    fn the_join_and_part_frames_name_the_channel() {
        let v: Value = serde_json::from_str(&join_frame("123")).unwrap();
        assert_eq!(
            v,
            json!({"name": "join_channel", "data": {"name": "twitch:123"}})
        );
        let v: Value = serde_json::from_str(&part_frame("123")).unwrap();
        assert_eq!(v["name"], "part_channel");
    }
}
