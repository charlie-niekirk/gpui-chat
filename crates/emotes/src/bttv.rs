//! BetterTTV client.
//!
//! * Global: `GET https://api.betterttv.net/3/cached/emotes/global`
//! * Channel: `GET https://api.betterttv.net/3/cached/users/twitch/{twitch_user_id}` —
//!   one document with `channelEmotes` and `sharedEmotes`; both go into the channel set
//!   (channel's own first). Unknown user → 404 → empty set.
//!
//! Images: `https://cdn.betterttv.net/emote/{id}/{1x|2x|3x}.webp`. BTTV's three sizes are
//! 28 / 56 / 112 px, i.e. 1× / 2× / 4×, so `3x` is stored as [`ImageScale::X4`]. WebP is
//! served for animated emotes too.
//!
//! Zero-width: the API has no such flag. Overlay emotes are the hard-coded
//! [`GLOBAL_ZERO_WIDTH`] names (global set only), matching what other clients do.
//!
//! Text effects: entries the API flags `modifier` (`w!`, `c!`, `h!`, ... — they change how
//! the *next* emote is drawn) are **dropped** (logged at debug level) until effects are
//! implemented; as plain or overlay emotes they would draw their placeholder image.
//!
//! Parsing is per entry: a malformed emote is skipped with a warning, the rest load.

use std::sync::Arc;

use gpui_chat_core::{Emote, EmoteProvider, ImageScale, ImageUrls};
use serde::Deserialize;
use serde_json::Value;

use crate::error::EmoteError;
use crate::http::{HttpClient, get_ok};
use crate::lenient::{null_default, parse_entries};
use crate::set::{EmoteScope, EmoteSet, SetEmote};
use crate::validate_user_id;

pub const DEFAULT_BASE_URL: &str = "https://api.betterttv.net/3";
const CDN: &str = "https://cdn.betterttv.net/emote";

/// BTTV *global* emotes that overlay the previous emote. Not exposed by the API.
pub const GLOBAL_ZERO_WIDTH: [&str; 8] = [
    "SoSnowy",
    "IceCold",
    "SantaHat",
    "TopHat",
    "ReinDeer",
    "CandyCane",
    "cvMask",
    "cvHazmat",
];

#[derive(Clone)]
pub struct BttvClient {
    http: Arc<dyn HttpClient>,
    base_url: String,
}

impl BttvClient {
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
        let url = format!("{}/cached/emotes/global", self.base_url);
        let body = get_ok(&*self.http, &url).await?;
        parse_global(&body)
    }

    /// Channel plus shared emotes of a Twitch user id. No BTTV account → empty set.
    pub async fn channel(&self, twitch_user_id: &str) -> Result<EmoteSet, EmoteError> {
        let id = validate_user_id(twitch_user_id)?;
        let url = format!("{}/cached/users/twitch/{id}", self.base_url);
        match get_ok(&*self.http, &url).await {
            Ok(body) => parse_channel(&body),
            Err(e) if e.is_not_found() => {
                Ok(EmoteSet::empty(EmoteProvider::Bttv, EmoteScope::Channel))
            }
            Err(e) => Err(e),
        }
    }
}

#[derive(Deserialize)]
struct RawEmote {
    id: String,
    code: String,
    #[serde(default, deserialize_with = "null_default")]
    animated: bool,
    #[serde(default, deserialize_with = "null_default")]
    modifier: bool,
    width: Option<u16>,
    height: Option<u16>,
    /// Only on shared emotes.
    user: Option<RawUser>,
}

#[derive(Deserialize)]
struct RawUser {
    name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct RawChannel {
    #[serde(rename = "channelEmotes", default, deserialize_with = "null_default")]
    channel_emotes: Vec<Value>,
    #[serde(rename = "sharedEmotes", default, deserialize_with = "null_default")]
    shared_emotes: Vec<Value>,
}

pub(crate) fn parse_global(body: &[u8]) -> Result<EmoteSet, EmoteError> {
    let values: Vec<Value> = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    let emotes = parse_entries::<RawEmote>("BTTV global emote", values)
        .into_iter()
        .filter_map(|e| convert(e, true))
        .collect();
    Ok(EmoteSet::new(
        EmoteProvider::Bttv,
        EmoteScope::Global,
        emotes,
    ))
}

pub(crate) fn parse_channel(body: &[u8]) -> Result<EmoteSet, EmoteError> {
    let raw: RawChannel = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    let emotes = parse_entries::<RawEmote>("BTTV channel emote", raw.channel_emotes)
        .into_iter()
        .chain(parse_entries::<RawEmote>(
            "BTTV shared emote",
            raw.shared_emotes,
        ))
        .filter_map(|e| convert(e, false))
        .collect();
    Ok(EmoteSet::new(
        EmoteProvider::Bttv,
        EmoteScope::Channel,
        emotes,
    ))
}

/// `None` for text-effect modifiers (see the module docs).
fn convert(raw: RawEmote, global: bool) -> Option<SetEmote> {
    if raw.modifier {
        tracing::debug!(code = %raw.code, "dropping BTTV effect modifier");
        return None;
    }
    let urls = ImageUrls::new()
        .with(ImageScale::X1, format!("{CDN}/{}/1x.webp", raw.id))
        .with(ImageScale::X2, format!("{CDN}/{}/2x.webp", raw.id))
        .with(ImageScale::X4, format!("{CDN}/{}/3x.webp", raw.id));
    let zero_width = global && GLOBAL_ZERO_WIDTH.contains(&raw.code.as_str());
    let owner = raw
        .user
        .and_then(|u| u.display_name.or(u.name))
        .map(Arc::<str>::from);
    Some(SetEmote {
        emote: Arc::new(Emote {
            id: raw.id.into(),
            name: raw.code.into(),
            provider: EmoteProvider::Bttv,
            urls,
            animated: raw.animated,
            zero_width,
            width: raw.width,
            height: raw.height,
        }),
        original_name: None,
        owner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpError;
    use crate::testutil::{FakeHttp, fixture};

    #[test]
    fn parses_global_fixture() {
        let set = parse_global(&fixture("bttv-global.json")).unwrap();
        assert_eq!(set.provider, EmoteProvider::Bttv);
        assert_eq!(set.scope, EmoteScope::Global);
        // 12 captured entries, 3 of them text-effect modifiers (c!, h!, l!) that are dropped.
        assert_eq!(set.len(), 9);
        let mask = set.get("cvMask").unwrap();
        assert!(mask.emote.zero_width, "known overlay list");
        for effect in ["c!", "h!", "l!"] {
            assert!(
                set.get(effect).is_none(),
                "{effect} is an effect, not an emote"
            );
        }
        let plain = set.get(":tf:").unwrap();
        assert!(!plain.emote.zero_width);
        assert_eq!(
            plain.emote.urls.get(ImageScale::X1),
            Some("https://cdn.betterttv.net/emote/54fa8f1401e468494b85b537/1x.webp")
        );
        assert_eq!(
            plain.emote.urls.get(ImageScale::X4),
            Some("https://cdn.betterttv.net/emote/54fa8f1401e468494b85b537/3x.webp")
        );
        assert_eq!(plain.emote.urls.get(ImageScale::X3), None);
    }

    #[test]
    fn parses_channel_fixture_with_shared() {
        let set = parse_channel(&fixture("bttv-channel-pajlada.json")).unwrap();
        assert_eq!(set.scope, EmoteScope::Channel);
        assert_eq!(set.len(), 6 + 3);
        // Channel emotes first, no owner; shared after with their uploader.
        assert_eq!(&*set.emotes()[0].emote.name, "pajaDank");
        assert_eq!(set.emotes()[0].owner, None);
        assert!(!set.emotes()[0].emote.animated);
        let shared = set.get("BBoomer").unwrap();
        assert!(shared.emote.animated);
        assert_eq!(shared.owner.as_deref(), Some("Leit69"));
        assert_eq!(shared.emote.id.as_str(), "5c447084f13c030e98f74f58");
        assert_eq!(shared.emote.provider, EmoteProvider::Bttv);
    }

    #[test]
    fn zero_width_list_only_applies_to_global() {
        let body = br#"{"channelEmotes":[{"id":"1","code":"SantaHat","animated":false}],"sharedEmotes":[]}"#;
        let set = parse_channel(body).unwrap();
        assert!(!set.emotes()[0].emote.zero_width);
    }

    #[test]
    fn one_bad_entry_is_skipped_and_the_rest_load() {
        let global = br#"[
            {"id":"1","code":"Good","animated":null},
            {"id":2,"code":"BadId"},
            "not an object",
            {"id":"3"},
            {"id":"4","code":"AlsoGood","modifier":null,"width":null}]"#;
        let set = parse_global(global).unwrap();
        let names: Vec<_> = set.iter().map(|e| &*e.name).collect();
        assert_eq!(names, ["Good", "AlsoGood"]);
        assert!(!set.get("Good").unwrap().emote.animated, "null = default");

        let channel = br#"{"channelEmotes":[{"id":"1","code":"A"},{"code":"NoId"}],
                           "sharedEmotes":null}"#;
        let set = parse_channel(channel).unwrap();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn non_json_body_is_a_parse_error() {
        assert!(matches!(parse_global(b"<html>"), Err(EmoteError::Parse(_))));
        assert!(matches!(parse_global(b"{}"), Err(EmoteError::Parse(_))));
        assert!(matches!(
            parse_channel(b"\"nope\""),
            Err(EmoteError::Parse(_))
        ));
    }

    fn client(fake: FakeHttp) -> BttvClient {
        BttvClient::new(Arc::new(fake))
    }

    #[tokio::test]
    async fn channel_404_is_an_empty_set() {
        let url = "https://api.betterttv.net/3/cached/users/twitch/1";
        let c = client(FakeHttp::new().status(url, 404, "{\"message\":\"user not found\"}"));
        let set = c.channel("1").await.unwrap();
        assert!(set.is_empty());
        assert_eq!(set.scope, EmoteScope::Channel);
    }

    #[tokio::test]
    async fn errors_are_reported() {
        let url = "https://api.betterttv.net/3/cached/users/twitch/2";
        let c = client(FakeHttp::new().status(url, 503, "oops"));
        assert_eq!(c.channel("2").await.unwrap_err(), EmoteError::Status(503));
        let c = client(FakeHttp::new().fail(url, HttpError::Timeout));
        assert_eq!(
            c.channel("2").await.unwrap_err(),
            EmoteError::Http(HttpError::Timeout)
        );
        // 404 on a global endpoint is an error.
        let g = "https://api.betterttv.net/3/cached/emotes/global";
        let c = client(FakeHttp::new().status(g, 404, "{}"));
        assert_eq!(c.global().await.unwrap_err(), EmoteError::Status(404));
        // Bad ids never hit the network.
        let fake = Arc::new(FakeHttp::new());
        let c = BttvClient::new(fake.clone());
        assert!(matches!(
            c.channel("../x").await,
            Err(EmoteError::InvalidUserId(_))
        ));
        assert!(fake.requested().is_empty());
    }

    #[tokio::test]
    async fn fetches_both_endpoints() {
        let c = client(
            FakeHttp::new()
                .json(
                    "https://api.betterttv.net/3/cached/emotes/global",
                    "bttv-global.json",
                )
                .json(
                    "https://api.betterttv.net/3/cached/users/twitch/11148817",
                    "bttv-channel-pajlada.json",
                ),
        );
        assert_eq!(c.global().await.unwrap().len(), 9);
        assert_eq!(c.channel("11148817").await.unwrap().len(), 9);
    }
}
