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
//! [`GLOBAL_ZERO_WIDTH`] names (global set only), matching what other clients do. The API's
//! `modifier` flag (the `w!`, `c!`, ... chat-effect pseudo-emotes) is also treated as
//! zero-width so those never take a slot of their own.

use std::sync::Arc;

use gpui_chat_core::{Emote, EmoteProvider, ImageScale, ImageUrls};
use serde::Deserialize;

use crate::error::EmoteError;
use crate::http::{HttpClient, get_ok};
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
    #[serde(default)]
    animated: bool,
    #[serde(default)]
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
    #[serde(rename = "channelEmotes", default)]
    channel_emotes: Vec<RawEmote>,
    #[serde(rename = "sharedEmotes", default)]
    shared_emotes: Vec<RawEmote>,
}

pub(crate) fn parse_global(body: &[u8]) -> Result<EmoteSet, EmoteError> {
    let raw: Vec<RawEmote> = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    Ok(EmoteSet {
        provider: EmoteProvider::Bttv,
        scope: EmoteScope::Global,
        emotes: raw.into_iter().map(|e| convert(e, true)).collect(),
    })
}

pub(crate) fn parse_channel(body: &[u8]) -> Result<EmoteSet, EmoteError> {
    let raw: RawChannel = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    Ok(EmoteSet {
        provider: EmoteProvider::Bttv,
        scope: EmoteScope::Channel,
        emotes: raw
            .channel_emotes
            .into_iter()
            .chain(raw.shared_emotes)
            .map(|e| convert(e, false))
            .collect(),
    })
}

fn convert(raw: RawEmote, global: bool) -> SetEmote {
    let urls = ImageUrls::new()
        .with(ImageScale::X1, format!("{CDN}/{}/1x.webp", raw.id))
        .with(ImageScale::X2, format!("{CDN}/{}/2x.webp", raw.id))
        .with(ImageScale::X4, format!("{CDN}/{}/3x.webp", raw.id));
    let zero_width = raw.modifier || (global && GLOBAL_ZERO_WIDTH.contains(&raw.code.as_str()));
    let owner = raw
        .user
        .and_then(|u| u.display_name.or(u.name))
        .map(Arc::<str>::from);
    SetEmote {
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
    }
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
        assert_eq!(set.len(), 12);
        let mask = set.get("cvMask").unwrap();
        assert!(mask.emote.zero_width, "known overlay list");
        let fx = set.get("c!").unwrap();
        assert!(fx.emote.zero_width, "API modifier flag");
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
        assert_eq!(&*set.emotes[0].emote.name, "pajaDank");
        assert_eq!(set.emotes[0].owner, None);
        assert!(!set.emotes[0].emote.animated);
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
        assert!(!set.emotes[0].emote.zero_width);
    }

    #[test]
    fn bad_json_is_a_parse_error() {
        assert!(matches!(parse_global(b"<html>"), Err(EmoteError::Parse(_))));
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
        assert_eq!(c.global().await.unwrap().len(), 12);
        assert_eq!(c.channel("11148817").await.unwrap().len(), 9);
    }
}
