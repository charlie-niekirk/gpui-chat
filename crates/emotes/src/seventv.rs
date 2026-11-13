//! 7TV client (v3 REST API).
//!
//! * Global: `GET https://7tv.io/v3/emote-sets/global`
//! * Channel: `GET https://7tv.io/v3/users/twitch/{twitch_user_id}` — the user document
//!   embeds the active emote set (`emote_set.emotes`). Unknown user → 404 → empty set; a
//!   user with no active set (`emote_set: null`) is also an empty set.
//!
//! Mapping, per entry of an emote set:
//! * `name` (the set entry's name) is the word to type: in a channel set that is the
//!   channel's **alias**, which overrides the emote's own `data.name`; the latter is kept
//!   in [`SetEmote::original_name`] when it differs.
//! * URLs: `host.url` + the `files` entry for each scale (`1x`..`4x`), preferring
//!   `.webp`, then `.gif`, then `.png`; AVIF files are ignored (the media crate has no
//!   AVIF decoder). Width/height are those of the 1x file.
//! * `data.animated` → `animated`.
//! * **Zero-width**: the set entry's `flags` bit 0 (`ActiveEmoteFlag::ZeroWidth`) decides
//!   when the entry carries `flags`; only an entry without them falls back to the emote's
//!   own `data.flags` bit 8 (`EmoteFlag::ZeroWidth`, 256).
//! * Owner: `data.owner.display_name` (else `username`).
//!
//! Parsing is per entry: a malformed emote is skipped with a warning, the rest load;
//! `null` is treated like a missing field.

use std::sync::Arc;

use gpui_chat_core::{Emote, EmoteProvider, ImageScale, ImageUrls};
use serde::Deserialize;
use serde_json::Value;

use crate::error::EmoteError;
use crate::http::{HttpClient, get_ok};
use crate::lenient::{null_default, parse_entries};
use crate::set::{EmoteScope, EmoteSet, SetEmote};
use crate::{absolute_url, validate_user_id};

pub const DEFAULT_BASE_URL: &str = "https://7tv.io/v3";

/// `ActiveEmoteFlag::ZeroWidth`: set on the entry in an emote set.
const ACTIVE_ZERO_WIDTH: u32 = 1 << 0;
/// `EmoteFlag::ZeroWidth`: set on the emote itself.
const EMOTE_ZERO_WIDTH: u32 = 1 << 8;

#[derive(Clone)]
pub struct SevenTvClient {
    http: Arc<dyn HttpClient>,
    base_url: String,
}

impl SevenTvClient {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self::with_base_url(http, DEFAULT_BASE_URL)
    }

    pub fn with_base_url(http: Arc<dyn HttpClient>, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
        }
    }

    pub async fn global(&self) -> Result<EmoteSet, EmoteError> {
        let url = format!("{}/emote-sets/global", self.base_url);
        let body = get_ok(&*self.http, &url).await?;
        parse_global(&body)
    }

    /// The active emote set of a Twitch user id. No 7TV account → empty set.
    pub async fn channel(&self, twitch_user_id: &str) -> Result<EmoteSet, EmoteError> {
        let id = validate_user_id(twitch_user_id)?;
        let url = format!("{}/users/twitch/{id}", self.base_url);
        match get_ok(&*self.http, &url).await {
            Ok(body) => parse_channel(&body),
            Err(e) if e.is_not_found() => {
                Ok(EmoteSet::empty(EmoteProvider::SevenTv, EmoteScope::Channel))
            }
            Err(e) => Err(e),
        }
    }
}

#[derive(Deserialize)]
struct RawSet {
    #[serde(default, deserialize_with = "null_default")]
    emotes: Vec<Value>,
}

#[derive(Deserialize)]
struct RawUser {
    emote_set: Option<RawSet>,
}

#[derive(Deserialize)]
struct RawActive {
    id: String,
    name: String,
    /// Set-entry flags; `None` when absent or `null`.
    flags: Option<u32>,
    /// Missing when the emote was deleted; such entries are skipped.
    data: Option<RawData>,
}

#[derive(Deserialize)]
struct RawData {
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    flags: u32,
    #[serde(default, deserialize_with = "null_default")]
    animated: bool,
    owner: Option<RawOwner>,
    host: Option<RawHost>,
}

#[derive(Deserialize)]
struct RawOwner {
    username: Option<String>,
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct RawHost {
    url: String,
    #[serde(default, deserialize_with = "null_default")]
    files: Vec<RawFile>,
}

#[derive(Deserialize)]
struct RawFile {
    /// `2x.webp`
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    width: u32,
    #[serde(default, deserialize_with = "null_default")]
    height: u32,
    /// `WEBP`, `GIF`, `PNG`, `AVIF`
    format: String,
}

pub(crate) fn parse_global(body: &[u8]) -> Result<EmoteSet, EmoteError> {
    let raw: RawSet = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    Ok(convert_set(raw, EmoteScope::Global))
}

pub(crate) fn parse_channel(body: &[u8]) -> Result<EmoteSet, EmoteError> {
    let raw: RawUser = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    Ok(match raw.emote_set {
        Some(set) => convert_set(set, EmoteScope::Channel),
        None => EmoteSet::empty(EmoteProvider::SevenTv, EmoteScope::Channel),
    })
}

fn convert_set(raw: RawSet, scope: EmoteScope) -> EmoteSet {
    let emotes = parse_entries::<RawActive>("7TV emote", raw.emotes)
        .into_iter()
        .filter_map(convert)
        .collect();
    EmoteSet::new(EmoteProvider::SevenTv, scope, emotes)
}

fn convert(raw: RawActive) -> Option<SetEmote> {
    let data = raw.data?;
    let host = data.host?;
    let base = absolute_url(&host.url);

    // Best file per scale: webp > gif > png.
    let mut best: [Option<(u8, &RawFile)>; 4] = [None; 4];
    for file in &host.files {
        let (Some(scale), Some(rank)) = (file_scale(&file.name), format_rank(&file.format)) else {
            continue;
        };
        let slot = &mut best[usize::from(scale.factor()) - 1];
        if slot.is_none_or(|(r, _)| rank < r) {
            *slot = Some((rank, file));
        }
    }
    let mut urls = ImageUrls::new();
    for scale in ImageScale::ALL {
        if let Some((_, file)) = best[usize::from(scale.factor()) - 1] {
            urls = urls.with(scale, format!("{base}/{}", file.name));
        }
    }
    if urls.is_empty() {
        return None;
    }
    let size = best[0].map(|(_, f)| f);
    let dim = |v: Option<u32>| v.filter(|v| *v > 0).and_then(|v| u16::try_from(v).ok());

    let original_name = (raw.name != data.name).then(|| Arc::<str>::from(data.name.as_str()));
    let owner = data
        .owner
        .and_then(|o| o.display_name.or(o.username))
        .map(Arc::<str>::from);
    Some(SetEmote {
        emote: Arc::new(Emote {
            id: raw.id.into(),
            name: raw.name.into(),
            provider: EmoteProvider::SevenTv,
            urls,
            animated: data.animated,
            zero_width: match raw.flags {
                Some(flags) => flags & ACTIVE_ZERO_WIDTH != 0,
                None => data.flags & EMOTE_ZERO_WIDTH != 0,
            },
            width: dim(size.map(|f| f.width)),
            height: dim(size.map(|f| f.height)),
        }),
        original_name,
        owner,
    })
}

/// `3x.webp` → [`ImageScale::X3`].
fn file_scale(name: &str) -> Option<ImageScale> {
    let stem = name.split('.').next()?;
    match stem.strip_suffix('x')?.parse::<u8>().ok()? {
        1 => Some(ImageScale::X1),
        2 => Some(ImageScale::X2),
        3 => Some(ImageScale::X3),
        4 => Some(ImageScale::X4),
        _ => None,
    }
}

/// Lower is better. `None` for formats we cannot decode.
fn format_rank(format: &str) -> Option<u8> {
    match format {
        "WEBP" => Some(0),
        "GIF" => Some(1),
        "PNG" => Some(2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpError;
    use crate::testutil::{FakeHttp, fixture};

    #[test]
    fn parses_global_fixture() {
        let set = parse_global(&fixture("7tv-global.json")).unwrap();
        assert_eq!(set.provider, EmoteProvider::SevenTv);
        assert_eq!(set.scope, EmoteScope::Global);
        assert!(set.len() >= 6);
        // RainTime/PETPET carry both the entry flag and data bit 8.
        let zw: Vec<_> = set
            .iter()
            .filter(|e| e.zero_width)
            .map(|e| &*e.name)
            .collect();
        assert!(zw.contains(&"RainTime"), "{zw:?}");
        assert!(zw.contains(&"PETPET"), "{zw:?}");
        let plain = set.iter().find(|e| !e.zero_width).unwrap();
        assert!(
            plain
                .urls
                .get(ImageScale::X1)
                .unwrap()
                .ends_with("/1x.webp")
        );
    }

    #[test]
    fn parses_channel_fixture_aliases_urls_and_flags() {
        let set = parse_channel(&fixture("7tv-user-pajlada.json")).unwrap();
        assert_eq!(set.scope, EmoteScope::Channel);

        // Alias overrides the emote's own name; original is kept.
        let pepe = set.get("PepeS").expect("typed by its alias");
        assert_eq!(pepe.original_name.as_deref(), Some("pepeS"));
        assert!(set.get("pepeS").is_none());
        // Unrenamed emotes carry no original name.
        assert!(
            set.emotes()
                .iter()
                .any(|e| e.original_name.is_none() && !e.emote.zero_width)
        );

        // Zero-width via the set entry's flag.
        assert!(set.get("ShowerTime").unwrap().emote.zero_width);

        // URLs: webp for all four scales, protocol-relative host fixed up.
        let very = set.get("VeryPaj").unwrap();
        assert!(very.emote.animated);
        assert_eq!(very.emote.id.as_str(), "01GB4BY8YG000CW87FDNNPRE2J");
        for (scale, file) in [
            (ImageScale::X1, "1x.webp"),
            (ImageScale::X2, "2x.webp"),
            (ImageScale::X3, "3x.webp"),
            (ImageScale::X4, "4x.webp"),
        ] {
            assert_eq!(
                very.emote.urls.get(scale),
                Some(
                    format!("https://cdn.7tv.app/emote/01GB4BY8YG000CW87FDNNPRE2J/{file}").as_str()
                )
            );
        }
        assert_eq!((very.emote.width, very.emote.height), (Some(96), Some(23)));
        assert_eq!(very.owner.as_deref(), Some("LiptonGod"));
    }

    #[test]
    fn format_preference_and_unsupported_files() {
        let body = br#"{"emotes":[{"id":"A","name":"Alias","flags":1,"data":{
            "name":"Orig","flags":0,"animated":true,"owner":null,
            "host":{"url":"//cdn.7tv.app/emote/A","files":[
              {"name":"1x.avif","width":10,"height":10,"format":"AVIF"},
              {"name":"1x.gif","width":10,"height":10,"format":"GIF"},
              {"name":"1x.webp","width":10,"height":12,"format":"WEBP"},
              {"name":"2x.gif","width":20,"height":20,"format":"GIF"},
              {"name":"2x.avif","width":20,"height":20,"format":"AVIF"},
              {"name":"3x.avif","width":30,"height":30,"format":"AVIF"}]}}},
            {"id":"B","name":"Deleted","flags":0,"data":null},
            {"id":"C","name":"NoFiles","flags":0,"data":{"name":"NoFiles","host":{"url":"//x","files":[]}}}]}"#;
        let set = parse_global(body).unwrap();
        assert_eq!(set.len(), 1, "deleted and file-less emotes are skipped");
        let e = &set.emotes()[0];
        assert_eq!(e.original_name.as_deref(), Some("Orig"));
        assert!(e.emote.zero_width);
        assert_eq!(
            e.emote.urls.get(ImageScale::X1),
            Some("https://cdn.7tv.app/emote/A/1x.webp")
        );
        assert_eq!(
            e.emote.urls.get(ImageScale::X2),
            Some("https://cdn.7tv.app/emote/A/2x.gif")
        );
        assert_eq!(
            e.emote.urls.get(ImageScale::X3),
            None,
            "AVIF-only scale dropped"
        );
        assert_eq!(e.emote.height, Some(12));
    }

    #[test]
    fn user_without_active_set_is_empty() {
        let set = parse_channel(&fixture("7tv-user-no-set.json")).unwrap();
        assert!(set.is_empty());
        assert_eq!(set.scope, EmoteScope::Channel);
    }

    #[test]
    fn set_entry_flags_win_over_emote_flags() {
        let emote = |name: &str, entry_flags: &str, data_flags: u32| {
            format!(
                r#"{{"id":"{name}","name":"{name}"{entry_flags},"data":{{"name":"{name}","flags":{data_flags},
                "host":{{"url":"//c/{name}","files":[{{"name":"1x.webp","format":"WEBP"}}]}}}}}}"#
            )
        };
        let body = format!(
            r#"{{"emotes":[{}]}}"#,
            [
                emote("EntryOn", r#","flags":1"#, 0),
                emote("EntryOffWins", r#","flags":0"#, 256),
                emote("NoEntryFlags", "", 256),
                emote("NullEntryFlags", r#","flags":null"#, 256),
                emote("Neither", "", 0),
            ]
            .join(",")
        );
        let set = parse_global(body.as_bytes()).unwrap();
        let zw = |n: &str| set.get(n).unwrap().emote.zero_width;
        assert!(zw("EntryOn"));
        assert!(!zw("EntryOffWins"));
        assert!(zw("NoEntryFlags"));
        assert!(zw("NullEntryFlags"));
        assert!(!zw("Neither"));
    }

    #[test]
    fn one_bad_entry_is_skipped_and_the_rest_load() {
        let host = r#""host":{"url":"//c/x","files":[{"name":"1x.webp","width":null,"height":null,"format":"WEBP"}]}"#;
        let body = format!(
            r#"{{"emotes":[
              {{"id":"1","name":"Good","flags":null,"data":{{"name":"Good","flags":null,"animated":null,"owner":null,{host}}}}},
              {{"id":7,"name":"NumericId","data":null}},
              {{"name":"NoId"}},
              "garbage",
              {{"id":"2","name":"AlsoGood","data":{{"name":"AlsoGood",{host}}}}}]}}"#
        );
        let set = parse_global(body.as_bytes()).unwrap();
        let names: Vec<_> = set.iter().map(|e| &*e.name).collect();
        assert_eq!(names, ["Good", "AlsoGood"]);
        assert_eq!(
            set.get("Good").unwrap().emote.width,
            None,
            "null size = unknown"
        );

        // `emotes: null` is an empty set.
        let set = parse_channel(br#"{"emote_set":{"emotes":null}}"#).unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn non_json_body_is_a_parse_error() {
        assert!(matches!(
            parse_global(b"not json"),
            Err(EmoteError::Parse(_))
        ));
        assert!(matches!(
            parse_channel(b"<html>"),
            Err(EmoteError::Parse(_))
        ));
        assert!(matches!(
            parse_channel(br#"{"emote_set":"oops"}"#),
            Err(EmoteError::Parse(_))
        ));
    }

    #[tokio::test]
    async fn client_fetches_and_maps_errors() {
        let id = "11148817";
        let user = format!("https://7tv.io/v3/users/twitch/{id}");
        let c = SevenTvClient::new(Arc::new(
            FakeHttp::new()
                .json("https://7tv.io/v3/emote-sets/global", "7tv-global.json")
                .json(&user, "7tv-user-pajlada.json")
                .status(
                    "https://7tv.io/v3/users/twitch/1",
                    404,
                    "{\"error_code\":12000}",
                )
                .status("https://7tv.io/v3/users/twitch/2", 500, "boom")
                .fail("https://7tv.io/v3/users/twitch/3", HttpError::Timeout),
        ));
        assert!(!c.global().await.unwrap().is_empty());
        assert!(!c.channel(id).await.unwrap().is_empty());
        assert!(c.channel("1").await.unwrap().is_empty());
        assert_eq!(c.channel("2").await.unwrap_err(), EmoteError::Status(500));
        assert_eq!(
            c.channel("3").await.unwrap_err(),
            EmoteError::Http(HttpError::Timeout)
        );
        assert!(matches!(
            c.channel("x").await,
            Err(EmoteError::InvalidUserId(_))
        ));
    }
}
