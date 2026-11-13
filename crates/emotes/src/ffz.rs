//! FrankerFaceZ client.
//!
//! **Source choice.** The official API (`https://api.frankerfacez.com/v1`) timed out in the
//! orchestrator's early probe but answered quickly (0.1–0.3 s) when re-tested for this PR,
//! so it is the **primary** source. BetterTTV mirrors FFZ's data
//! (`https://api.betterttv.net/3/cached/frankerfacez/...`, fast and cached), so it is the
//! **fallback**: when the official request fails for any reason except a 404 (timeout —
//! [`PRIMARY_TIMEOUT`] — connection error, 5xx, bad JSON), the mirror is asked instead.
//! If both fail the error is [`EmoteError::BothSourcesFailed`].
//!
//! Endpoints:
//! * Global: `GET /v1/set/global` (official) · `GET /3/cached/frankerfacez/emotes/global`
//!   (mirror). The official document lists several sets; only those in `default_sets` are
//!   global (the others are user-specific "Subwoofer" effects).
//! * Room: `GET /v1/room/id/{twitch_user_id}` (official; 404 `No such room` → empty set) ·
//!   `GET /3/cached/frankerfacez/users/twitch/{id}` (mirror; returns `[]` for none).
//!
//! Mapping: URLs are FFZ's `1`/`2`/`4` scales (the `animated` URL map is preferred when an
//! emote has one; the mirror already puts animated URLs under `1x`/`2x`/`4x`).
//! `animated` is true when such a map/flag exists.
//!
//! **Modifiers.** FFZ "modifier" emotes (`modifier: true`) overlay the previous emote and
//! are marked **zero-width** — but only plain ones (hats and the like). Modifiers whose
//! `modifier_flags` has bit 0 set (`Hidden`) are *effects* (`ffzW`, `ffzX`, `ffzCursed`,
//! `ffzHyper`, ...) that change how the previous emote is drawn rather than being an
//! image to stack; those are **dropped** (debug log) until effects are implemented. The
//! BTTV mirror does not expose `modifier_flags`, so mirror entries with `modifier: true`
//! cannot be told apart and are conservatively dropped too.
//!
//! Parsing is per entry (and per set): a malformed emote is skipped with a warning, the
//! rest load; `null` is treated like a missing field.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::{Emote, EmoteProvider, ImageScale, ImageUrls};
use serde::Deserialize;
use serde_json::Value;

use crate::error::EmoteError;
use crate::http::{HttpClient, HttpError, get_ok};
use crate::lenient::{null_default, parse_entries};
use crate::set::{EmoteScope, EmoteSet, SetEmote};
use crate::{absolute_url, validate_user_id};

pub const DEFAULT_BASE_URL: &str = "https://api.frankerfacez.com/v1";
pub const DEFAULT_MIRROR_URL: &str = "https://api.betterttv.net/3/cached/frankerfacez";

/// How long the official API gets before the mirror is tried.
pub const PRIMARY_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Clone)]
pub struct FfzClient {
    http: Arc<dyn HttpClient>,
    base_url: String,
    mirror_url: String,
    primary_timeout: Duration,
}

impl FfzClient {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self {
            http,
            base_url: DEFAULT_BASE_URL.to_owned(),
            mirror_url: DEFAULT_MIRROR_URL.to_owned(),
            primary_timeout: PRIMARY_TIMEOUT,
        }
    }

    /// Overrides the official and mirror base URLs (tests, local servers).
    #[must_use]
    pub fn with_base_urls(
        mut self,
        official: impl Into<String>,
        mirror: impl Into<String>,
    ) -> Self {
        self.base_url = official.into();
        self.mirror_url = mirror.into();
        self
    }

    /// Overrides how long the official API gets before falling back to the mirror.
    #[must_use]
    pub fn with_primary_timeout(mut self, timeout: Duration) -> Self {
        self.primary_timeout = timeout;
        self
    }

    pub async fn global(&self) -> Result<EmoteSet, EmoteError> {
        let official = format!("{}/set/global", self.base_url);
        let mirror = format!("{}/emotes/global", self.mirror_url);
        self.with_fallback(
            &official,
            parse_global,
            &mirror,
            parse_mirror(EmoteScope::Global),
            false,
        )
        .await
    }

    /// The emotes of a Twitch user's room. No FFZ room → empty set.
    pub async fn room(&self, twitch_user_id: &str) -> Result<EmoteSet, EmoteError> {
        let id = validate_user_id(twitch_user_id)?;
        let official = format!("{}/room/id/{id}", self.base_url);
        let mirror = format!("{}/users/twitch/{id}", self.mirror_url);
        self.with_fallback(
            &official,
            parse_room,
            &mirror,
            parse_mirror(EmoteScope::Channel),
            true,
        )
        .await
    }

    /// Official source first; on any failure (request or parse) the mirror. With
    /// `not_found_is_empty` a 404 from either source is an empty set (no FFZ room).
    async fn with_fallback(
        &self,
        official: &str,
        parse_official: fn(&[u8]) -> Result<EmoteSet, EmoteError>,
        mirror: &str,
        parse_mirror: impl Fn(&[u8]) -> Result<EmoteSet, EmoteError>,
        not_found_is_empty: bool,
    ) -> Result<EmoteSet, EmoteError> {
        let empty = || EmoteSet::empty(EmoteProvider::Ffz, EmoteScope::Channel);
        let primary = match self.fetch_primary(official).await {
            Ok(body) => parse_official(&body),
            Err(e) => Err(e),
        };
        let primary = match primary {
            Ok(set) => return Ok(set),
            Err(e) if not_found_is_empty && e.is_not_found() => return Ok(empty()),
            Err(e) => e,
        };
        tracing::warn!(%primary, "FFZ API failed, trying the BTTV mirror");
        match get_ok(&*self.http, mirror)
            .await
            .and_then(|b| parse_mirror(&b))
        {
            Ok(set) => Ok(set),
            Err(e) if not_found_is_empty && e.is_not_found() => Ok(empty()),
            Err(fallback) => Err(both_failed(primary, fallback)),
        }
    }

    async fn fetch_primary(&self, url: &str) -> Result<Vec<u8>, EmoteError> {
        match tokio::time::timeout(self.primary_timeout, get_ok(&*self.http, url)).await {
            Ok(result) => result,
            Err(_) => Err(EmoteError::Http(HttpError::Timeout)),
        }
    }
}

fn both_failed(primary: EmoteError, fallback: EmoteError) -> EmoteError {
    EmoteError::BothSourcesFailed {
        primary: Box::new(primary),
        fallback: Box::new(fallback),
    }
}

// ---- official API ---------------------------------------------------------------------

#[derive(Deserialize)]
struct RawGlobal {
    #[serde(default, deserialize_with = "null_default")]
    default_sets: Vec<u64>,
    #[serde(default, deserialize_with = "null_default")]
    sets: HashMap<String, Value>,
}

#[derive(Deserialize)]
struct RawRoomDoc {
    room: Option<RawRoom>,
    #[serde(default, deserialize_with = "null_default")]
    sets: HashMap<String, Value>,
}

#[derive(Deserialize)]
struct RawRoom {
    set: Option<u64>,
}

#[derive(Deserialize)]
struct RawSet {
    #[serde(default, deserialize_with = "null_default")]
    emoticons: Vec<Value>,
}

#[derive(Deserialize)]
struct RawEmote {
    id: u64,
    name: String,
    width: Option<u16>,
    height: Option<u16>,
    #[serde(default, deserialize_with = "null_default")]
    modifier: bool,
    /// Bit 0 = hidden (effect modifiers), see the module docs.
    #[serde(default, deserialize_with = "null_default")]
    modifier_flags: u64,
    owner: Option<RawOwner>,
    #[serde(default, deserialize_with = "null_default")]
    urls: HashMap<String, Option<String>>,
    /// Present (with the same `1`/`2`/`4` keys) only on animated emotes.
    animated: Option<HashMap<String, Option<String>>>,
}

#[derive(Deserialize)]
struct RawOwner {
    name: Option<String>,
    display_name: Option<String>,
}

/// `modifier_flags` bit 0: the modifier is a hidden effect, not an overlay image.
const MODIFIER_HIDDEN: u64 = 1;

/// Converts one raw set (a malformed set is skipped with a warning).
fn parse_set(what: &str, value: Value) -> Vec<SetEmote> {
    match serde_json::from_value::<RawSet>(value) {
        Ok(set) => parse_entries::<RawEmote>(what, set.emoticons)
            .iter()
            .filter_map(convert)
            .collect(),
        Err(error) => {
            tracing::warn!(what, %error, "skipping malformed FFZ set");
            Vec::new()
        }
    }
}

pub(crate) fn parse_global(body: &[u8]) -> Result<EmoteSet, EmoteError> {
    let mut raw: RawGlobal = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    let mut emotes = Vec::new();
    for id in &raw.default_sets {
        if let Some(set) = raw.sets.remove(&id.to_string()) {
            emotes.extend(parse_set("FFZ global emote", set));
        }
    }
    Ok(EmoteSet::new(
        EmoteProvider::Ffz,
        EmoteScope::Global,
        emotes,
    ))
}

pub(crate) fn parse_room(body: &[u8]) -> Result<EmoteSet, EmoteError> {
    let mut raw: RawRoomDoc = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    let wanted = raw
        .room
        .and_then(|r| r.set)
        .map(|id| id.to_string())
        .filter(|id| raw.sets.contains_key(id));
    let mut emotes = Vec::new();
    match wanted {
        Some(id) => {
            if let Some(set) = raw.sets.remove(&id) {
                emotes.extend(parse_set("FFZ room emote", set));
            }
        }
        None => {
            // No (or unknown) main set: take whatever sets came with the document.
            let mut sets: Vec<_> = raw.sets.into_iter().collect();
            sets.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, set) in sets {
                emotes.extend(parse_set("FFZ room emote", set));
            }
        }
    }
    Ok(EmoteSet::new(
        EmoteProvider::Ffz,
        EmoteScope::Channel,
        emotes,
    ))
}

fn scale_key(key: &str) -> Option<ImageScale> {
    match key {
        "1" | "1x" => Some(ImageScale::X1),
        "2" | "2x" => Some(ImageScale::X2),
        "3" | "3x" => Some(ImageScale::X3),
        "4" | "4x" => Some(ImageScale::X4),
        _ => None,
    }
}

fn urls_from(map: &HashMap<String, Option<String>>) -> ImageUrls {
    let mut urls = ImageUrls::new();
    for (key, url) in map {
        if let (Some(scale), Some(url)) = (scale_key(key), url.as_deref().filter(|u| !u.is_empty()))
        {
            urls = urls.with(scale, absolute_url(url));
        }
    }
    urls
}

fn convert(raw: &RawEmote) -> Option<SetEmote> {
    if raw.modifier && raw.modifier_flags & MODIFIER_HIDDEN != 0 {
        tracing::debug!(name = %raw.name, "dropping FFZ effect modifier");
        return None;
    }
    let animated = raw.animated.as_ref().filter(|m| !m.is_empty());
    let urls = urls_from(animated.unwrap_or(&raw.urls));
    if urls.is_empty() {
        return None;
    }
    let owner = raw
        .owner
        .as_ref()
        .and_then(|o| o.display_name.clone().or_else(|| o.name.clone()))
        .map(Arc::<str>::from);
    Some(SetEmote {
        emote: Arc::new(Emote {
            id: raw.id.to_string().into(),
            name: raw.name.as_str().into(),
            provider: EmoteProvider::Ffz,
            urls,
            animated: animated.is_some(),
            zero_width: raw.modifier,
            width: raw.width,
            height: raw.height,
        }),
        original_name: None,
        owner,
    })
}

// ---- BTTV mirror ----------------------------------------------------------------------

#[derive(Deserialize)]
struct MirrorEmote {
    id: u64,
    code: String,
    #[serde(default, deserialize_with = "null_default")]
    animated: bool,
    #[serde(default, deserialize_with = "null_default")]
    modifier: bool,
    #[serde(default, deserialize_with = "null_default")]
    images: HashMap<String, Option<String>>,
    user: Option<MirrorUser>,
}

#[derive(Deserialize)]
struct MirrorUser {
    name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

/// Returns a parser for the mirror's flat emote arrays. Mirror entries flagged `modifier`
/// are dropped: the mirror has no `modifier_flags`, so effects cannot be told from overlays.
pub(crate) fn parse_mirror(scope: EmoteScope) -> impl Fn(&[u8]) -> Result<EmoteSet, EmoteError> {
    move |body| {
        let values: Vec<Value> = serde_json::from_slice(body).map_err(EmoteError::parse)?;
        let emotes = parse_entries::<MirrorEmote>("FFZ mirror emote", values)
            .into_iter()
            .filter_map(|e| {
                if e.modifier {
                    tracing::debug!(name = %e.code, "dropping FFZ modifier (mirror has no modifier_flags)");
                    return None;
                }
                // `1x`/`2x`/`4x` are animated when the emote is; `*_static` keys are ignored.
                let urls = urls_from(&e.images);
                if urls.is_empty() {
                    return None;
                }
                let owner = e
                    .user
                    .and_then(|u| u.display_name.or(u.name))
                    .map(Arc::<str>::from);
                Some(SetEmote {
                    emote: Arc::new(Emote {
                        id: e.id.to_string().into(),
                        name: e.code.into(),
                        provider: EmoteProvider::Ffz,
                        urls,
                        animated: e.animated,
                        zero_width: false,
                        width: None,
                        height: None,
                    }),
                    original_name: None,
                    owner,
                })
            })
            .collect();
        Ok(EmoteSet::new(EmoteProvider::Ffz, scope, emotes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeHttp, fixture};

    const OFFICIAL_GLOBAL: &str = "https://api.frankerfacez.com/v1/set/global";
    const MIRROR_GLOBAL: &str = "https://api.betterttv.net/3/cached/frankerfacez/emotes/global";

    #[test]
    fn global_uses_only_default_sets() {
        let set = parse_global(&fixture("ffz-global.json")).unwrap();
        assert_eq!(set.scope, EmoteScope::Global);
        // Set 3 (6 kept). Set 1539687's four effect modifiers (hidden flag) are dropped,
        // and the Subwoofer set 1532818 is user-specific, not in `default_sets`.
        assert_eq!(set.len(), 6);
        for effect in ["ffzW", "ffzX", "ffzY", "ffzCursed", "ffzHyper"] {
            assert!(set.get(effect).is_none(), "{effect}");
        }
        let plain = set.get("ZrehplaR").unwrap();
        assert!(!plain.emote.zero_width);
        assert_eq!(plain.owner.as_deref(), Some("dansalvato"));
        assert_eq!(plain.emote.id.as_str(), "9");
        assert_eq!(
            plain.emote.urls.get(ImageScale::X2),
            Some("https://cdn.frankerfacez.com/emote/9/2")
        );
        assert_eq!(plain.emote.urls.get(ImageScale::X3), None);
        assert_eq!(
            (plain.emote.width, plain.emote.height),
            (Some(33), Some(30))
        );
    }

    #[test]
    fn room_fixture() {
        let set = parse_room(&fixture("ffz-room-pajlada.json")).unwrap();
        assert_eq!(set.scope, EmoteScope::Channel);
        assert_eq!(set.len(), 6);
        assert_eq!(&*set.emotes()[0].emote.name, "pajaGhetti");
        assert!(!set.emotes()[0].emote.animated);
    }

    #[test]
    fn room_animated_emotes_prefer_animated_urls() {
        let set = parse_room(&fixture("ffz-room-animated.json")).unwrap();
        let mhm = set.get("Mhm").unwrap();
        assert!(mhm.emote.animated);
        assert_eq!(
            mhm.emote.urls.get(ImageScale::X1),
            Some("https://cdn.frankerfacez.com/emote/724419/animated/1")
        );
        assert!(set.iter().any(|e| !e.animated));
    }

    #[test]
    fn mirror_fixtures() {
        let global = parse_mirror(EmoteScope::Global)(&fixture("ffz-mirror-global.json")).unwrap();
        // 15 captured, 4 flagged `modifier` (ffzW/X/Y/Cursed) and dropped: the mirror
        // cannot say whether a modifier is a hidden effect.
        assert_eq!(global.len(), 11);
        assert!(global.get("ffzCursed").is_none());
        assert!(!global.get("ZrehplaR").unwrap().emote.zero_width);

        let room =
            parse_mirror(EmoteScope::Channel)(&fixture("ffz-mirror-user-animated.json")).unwrap();
        let mhm = room.get("Mhm").unwrap();
        assert!(mhm.emote.animated);
        assert_eq!(
            mhm.emote.urls.get(ImageScale::X4),
            Some("https://cdn.betterttv.net/frankerfacez_emote/724419/animated/4")
        );
        assert_eq!(mhm.owner.as_deref(), Some("RaidinDawgZ"));

        let pj =
            parse_mirror(EmoteScope::Channel)(&fixture("ffz-mirror-user-pajlada.json")).unwrap();
        assert_eq!(pj.len(), 6);
    }

    #[test]
    fn plain_modifiers_stay_zero_width_and_hidden_ones_are_dropped() {
        let urls = r#""urls":{"1":"//cdn.frankerfacez.com/emote/1/1"}"#;
        let body = format!(
            r#"{{"default_sets":[3],"sets":{{"3":{{"emoticons":[
              {{"id":1,"name":"Hat","modifier":true,"modifier_flags":0,{urls}}},
              {{"id":2,"name":"HatNull","modifier":true,"modifier_flags":null,{urls}}},
              {{"id":3,"name":"Effect","modifier":true,"modifier_flags":9,{urls}}},
              {{"id":4,"name":"Plain","modifier":false,"modifier_flags":1,{urls}}}]}}}}}}"#
        );
        let set = parse_global(body.as_bytes()).unwrap();
        assert!(set.get("Hat").unwrap().emote.zero_width);
        assert!(set.get("HatNull").unwrap().emote.zero_width);
        assert!(set.get("Effect").is_none(), "hidden modifier = effect");
        assert!(!set.get("Plain").unwrap().emote.zero_width);
        assert_eq!(
            set.get("Hat").unwrap().emote.urls.get(ImageScale::X1),
            Some("https://cdn.frankerfacez.com/emote/1/1"),
            "protocol-relative URL fixed up"
        );
    }

    #[test]
    fn one_bad_entry_is_skipped_and_the_rest_load() {
        let body = br#"{"room":{"set":5},"sets":{"5":{"emoticons":[
            {"id":1,"name":"Good","urls":{"1":"https://x/1","2":null,"4":""}},
            {"id":"nope","name":"BadId","urls":{"1":"https://x/2"}},
            {"name":"NoId","urls":{"1":"https://x/3"}},
            42,
            {"id":2,"name":"NoUrls","urls":null},
            {"id":3,"name":"AlsoGood","modifier":null,"urls":{"1":"https://x/4"},"animated":null}]}}}"#;
        let set = parse_room(body).unwrap();
        let names: Vec<_> = set.iter().map(|e| &*e.name).collect();
        assert_eq!(names, ["Good", "AlsoGood"]);
        let good = set.get("Good").unwrap();
        assert_eq!(good.emote.urls.get(ImageScale::X1), Some("https://x/1"));
        assert_eq!(
            good.emote.urls.get(ImageScale::X2),
            None,
            "null url ignored"
        );

        // A malformed set does not take the others down either.
        let body = br#"{"default_sets":[1,2],"sets":{"1":"garbage","2":{"emoticons":[
            {"id":9,"name":"Fine","urls":{"1":"https://x/9"}}]}}}"#;
        assert_eq!(parse_global(body).unwrap().len(), 1);

        // Mirror entries are lenient too.
        let mirror = br#"[{"id":1,"code":"Good","animated":null,"images":{"1x":"https://m/1"}},
                          {"id":"x","code":"Bad"}, 7]"#;
        let set = parse_mirror(EmoteScope::Channel)(mirror).unwrap();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn non_json_body_is_a_parse_error() {
        assert!(matches!(parse_global(b"nope"), Err(EmoteError::Parse(_))));
        assert!(matches!(parse_room(b"\"nope\""), Err(EmoteError::Parse(_))));
        assert!(matches!(
            parse_mirror(EmoteScope::Global)(b"{}"),
            Err(EmoteError::Parse(_))
        ));
    }

    fn client(fake: &Arc<FakeHttp>) -> FfzClient {
        FfzClient::new(fake.clone())
    }

    #[tokio::test]
    async fn global_prefers_the_official_api() {
        let fake = Arc::new(
            FakeHttp::new()
                .json(OFFICIAL_GLOBAL, "ffz-global.json")
                .json(MIRROR_GLOBAL, "ffz-mirror-global.json"),
        );
        let set = client(&fake).global().await.unwrap();
        assert_eq!(set.len(), 6);
        assert_eq!(fake.requested(), [OFFICIAL_GLOBAL]);
    }

    #[tokio::test]
    async fn global_falls_back_to_the_mirror() {
        for official in [
            FakeHttp::new().status(OFFICIAL_GLOBAL, 502, "bad gateway"),
            FakeHttp::new().fail(OFFICIAL_GLOBAL, HttpError::Network("reset".into())),
            FakeHttp::new().status(OFFICIAL_GLOBAL, 200, "<html>"),
        ] {
            let fake = Arc::new(official.json(MIRROR_GLOBAL, "ffz-mirror-global.json"));
            let set = client(&fake).global().await.unwrap();
            assert_eq!(set.len(), 11);
            assert_eq!(fake.requested(), [OFFICIAL_GLOBAL, MIRROR_GLOBAL]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn slow_official_api_times_out_to_the_mirror() {
        let fake = Arc::new(
            FakeHttp::new()
                .hang(OFFICIAL_GLOBAL)
                .json(MIRROR_GLOBAL, "ffz-mirror-global.json"),
        );
        let set = client(&fake).global().await.unwrap();
        assert_eq!(set.len(), 11);
    }

    #[tokio::test]
    async fn both_sources_failing_reports_both() {
        let fake = Arc::new(
            FakeHttp::new()
                .status(OFFICIAL_GLOBAL, 500, "")
                .fail(MIRROR_GLOBAL, HttpError::Timeout),
        );
        assert_eq!(
            client(&fake).global().await.unwrap_err(),
            EmoteError::BothSourcesFailed {
                primary: Box::new(EmoteError::Status(500)),
                fallback: Box::new(EmoteError::Http(HttpError::Timeout)),
            }
        );
    }

    #[tokio::test]
    async fn room_404_is_empty_without_asking_the_mirror() {
        let fake = Arc::new(FakeHttp::new().status(
            "https://api.frankerfacez.com/v1/room/id/1",
            404,
            "{\"status\":404,\"error\":\"Not Found\",\"message\":\"No such room\"}",
        ));
        let set = client(&fake).room("1").await.unwrap();
        assert!(set.is_empty());
        assert_eq!(fake.requested().len(), 1);
    }

    #[tokio::test]
    async fn room_falls_back_to_the_mirror_on_bad_json() {
        let official = "https://api.frankerfacez.com/v1/room/id/11148817";
        let mirror = "https://api.betterttv.net/3/cached/frankerfacez/users/twitch/11148817";
        for body in ["<html>", "\"nope\""] {
            let fake = Arc::new(
                FakeHttp::new()
                    .status(official, 200, body)
                    .json(mirror, "ffz-mirror-user-pajlada.json"),
            );
            assert_eq!(client(&fake).room("11148817").await.unwrap().len(), 6);
            assert_eq!(fake.requested(), [official, mirror]);
        }
        // Both sources broken: both errors reported.
        let fake = Arc::new(
            FakeHttp::new()
                .status(official, 200, "<html>")
                .status(mirror, 200, "<html>"),
        );
        assert!(matches!(
            client(&fake).room("11148817").await,
            Err(EmoteError::BothSourcesFailed { .. })
        ));
    }

    #[tokio::test]
    async fn room_falls_back_and_mirror_empty_is_empty() {
        let official = "https://api.frankerfacez.com/v1/room/id/11148817";
        let mirror = "https://api.betterttv.net/3/cached/frankerfacez/users/twitch/11148817";
        let fake = Arc::new(
            FakeHttp::new()
                .status(official, 503, "")
                .json(mirror, "ffz-mirror-user-pajlada.json"),
        );
        assert_eq!(client(&fake).room("11148817").await.unwrap().len(), 6);

        let fake = Arc::new(
            FakeHttp::new()
                .status(official, 503, "")
                .status(mirror, 200, "[]"),
        );
        assert!(client(&fake).room("11148817").await.unwrap().is_empty());

        assert!(matches!(
            client(&fake).room("bad id").await,
            Err(EmoteError::InvalidUserId(_))
        ));
    }
}
