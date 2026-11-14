//! `api.ivr.fi`'s badge endpoints: the anonymous source of a channel's badges (and of the global
//! ones, for the snapshot generator).
//!
//! * `GET https://api.ivr.fi/v2/twitch/badges/channel?id=<user id>` → the channel's badges,
//!   `404` when the channel does not exist (an empty set here);
//! * `GET https://api.ivr.fi/v2/twitch/badges/global` → the global badges.
//!
//! Both answer with the shape of Helix's `chat/badges` endpoints, flattened to
//! `[{ "set_id", "versions": [{ "id", "title", "image_url_1x", "image_url_2x", "image_url_4x" }] }]`
//! (the `4x` URL is Twitch's `/3` size, 72 px). Entries that do not fit are skipped with a
//! warning; a version without a `1x` image is skipped too, since it could not be drawn.

use gpui_chat_core::{Badge, BadgeId, ImageScale, ImageUrls};
use serde::Deserialize;
use serde_json::Value;

use super::{BadgeError, BadgeSet};
use crate::lenient::parse_entries;

/// The default API base.
pub const BASE_URL: &str = "https://api.ivr.fi";

/// URL of a channel's badges.
pub fn channel_url(base: &str, user_id: &str) -> String {
    format!("{base}/v2/twitch/badges/channel?id={user_id}")
}

/// URL of the global badges.
pub fn global_url(base: &str) -> String {
    format!("{base}/v2/twitch/badges/global")
}

#[derive(Deserialize)]
struct RawSet {
    set_id: String,
    versions: Vec<Value>,
}

#[derive(Deserialize)]
struct RawVersion {
    id: String,
    title: String,
    image_url_1x: String,
    image_url_2x: Option<String>,
    image_url_4x: Option<String>,
}

/// Parses an IVR (or Helix-shaped) badge list.
pub fn parse(body: &[u8]) -> Result<BadgeSet, BadgeError> {
    let sets: Vec<Value> =
        serde_json::from_slice(body).map_err(|e| BadgeError::Parse(e.to_string()))?;
    let mut out = BadgeSet::new();
    for set in parse_entries::<RawSet>("badge set", sets) {
        for version in parse_entries::<RawVersion>("badge version", set.versions) {
            let mut urls = ImageUrls::new().with(ImageScale::X1, version.image_url_1x);
            if let Some(url) = version.image_url_2x {
                urls = urls.with(ImageScale::X2, url);
            }
            if let Some(url) = version.image_url_4x {
                urls = urls.with(ImageScale::X4, url);
            }
            out.insert(Badge {
                id: BadgeId::new(set.set_id.as_str(), version.id),
                title: version.title,
                urls,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::badge_fixture;

    #[test]
    fn parses_a_channel_response_and_skips_what_does_not_fit() {
        let set = parse(&badge_fixture("ivr-channel-sample.json")).unwrap();
        let sub = set.get("subscriber", "0").unwrap();
        assert_eq!(sub.title, "Subscriber");
        assert_eq!(
            sub.urls.get(ImageScale::X1),
            Some("https://static-cdn.jtvnw.net/badges/v1/42212029-4b13-4e76-91b5-c8d7d520a08a/1")
        );
        assert_eq!(
            sub.urls.get(ImageScale::X2),
            Some("https://static-cdn.jtvnw.net/badges/v1/42212029-4b13-4e76-91b5-c8d7d520a08a/2")
        );
        assert_eq!(
            sub.urls.get(ImageScale::X4),
            Some("https://static-cdn.jtvnw.net/badges/v1/42212029-4b13-4e76-91b5-c8d7d520a08a/3")
        );
        assert_eq!(sub.urls.get(ImageScale::X3), None);
        assert_eq!(
            set.get("subscriber", "108").unwrap().title,
            "9-Year Subscriber"
        );
        assert!(set.get("bits", "1000").is_some());
        // `broken` lacks every field but the id; `half` has only a 1x image: it is kept.
        assert!(set.get("broken", "1").is_none());
        let half = set.get("half", "1").unwrap();
        assert_eq!(half.urls.get(ImageScale::X2), None);
        assert!(half.urls.get(ImageScale::X1).is_some());
    }

    #[test]
    fn parses_the_global_sample() {
        let set = parse(&badge_fixture("ivr-global-sample.json")).unwrap();
        assert_eq!(set.get("moderator", "1").unwrap().title, "Moderator");
        assert!(set.get("vip", "1").is_some());
        assert!(set.get("broadcaster", "1").is_some());
    }

    #[test]
    fn not_json_or_not_a_list_is_a_parse_error() {
        assert!(matches!(parse(b"nope"), Err(BadgeError::Parse(_))));
        assert!(matches!(parse(b"{}"), Err(BadgeError::Parse(_))));
        assert!(parse(b"[]").unwrap().is_empty());
    }

    #[test]
    fn urls() {
        assert_eq!(
            channel_url(BASE_URL, "1"),
            "https://api.ivr.fi/v2/twitch/badges/channel?id=1"
        );
        assert_eq!(
            global_url(BASE_URL),
            "https://api.ivr.fi/v2/twitch/badges/global"
        );
    }
}
