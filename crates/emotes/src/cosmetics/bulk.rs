//! The two providers that publish their whole list of badge holders: FrankerFaceZ and Chatterino.
//!
//! * FFZ: `GET https://api.frankerfacez.com/v1/badges/ids` — badge definitions (`id`, `title`,
//!   images at 1×/2×/4×) and `users`: badge id → Twitch user ids. (`replaces` — a bot badge
//!   standing in for the moderator one — is ignored: both are shown.)
//! * Chatterino: `GET https://api.chatterino.com/badges` — each badge has a `tooltip`, three
//!   images (1×, 2×, 3×) and the Twitch user ids that hold it.
//!
//! Both are fetched once at start (FFZ's is about 400 KB). A failure is a warning; the badges
//! are then simply not shown.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_chat_core::{Badge, BadgeId, ImageScale, ImageUrls};
use serde::Deserialize;
use serde_json::Value;

use super::sets;
use crate::error::EmoteError;
use crate::http::{HttpClient, get_ok};
use crate::lenient::{null_default, parse_entries};

pub const FFZ_URL: &str = "https://api.frankerfacez.com/v1/badges/ids";
pub const CHATTERINO_URL: &str = "https://api.chatterino.com/badges";

/// Badge definitions and who holds them (Twitch user id → badge versions).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BadgeHolders {
    pub badges: Vec<Badge>,
    pub holders: HashMap<Arc<str>, Vec<Arc<str>>>,
}

pub async fn load_ffz(http: &dyn HttpClient) -> Result<BadgeHolders, EmoteError> {
    parse_ffz(&get_ok(http, FFZ_URL).await?)
}

pub async fn load_chatterino(http: &dyn HttpClient) -> Result<BadgeHolders, EmoteError> {
    parse_chatterino(&get_ok(http, CHATTERINO_URL).await?)
}

#[derive(Deserialize)]
struct FfzBadge {
    id: u64,
    #[serde(default, deserialize_with = "null_default")]
    title: String,
    #[serde(default, deserialize_with = "null_default")]
    urls: HashMap<String, String>,
}

#[derive(Deserialize)]
struct FfzDocument {
    #[serde(default, deserialize_with = "null_default")]
    badges: Vec<Value>,
    #[serde(default, deserialize_with = "null_default")]
    users: HashMap<String, Vec<Value>>,
}

pub(crate) fn parse_ffz(body: &[u8]) -> Result<BadgeHolders, EmoteError> {
    let doc: FfzDocument = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    let mut badges = Vec::new();
    let mut known = Vec::new();
    for raw in parse_entries::<FfzBadge>("FFZ badge", doc.badges) {
        let mut urls = ImageUrls::new();
        for (key, scale) in [
            ("1", ImageScale::X1),
            ("2", ImageScale::X2),
            ("4", ImageScale::X4),
        ] {
            if let Some(url) = raw
                .urls
                .get(key)
                .filter(|url| crate::trusted_image_url(url))
            {
                urls = urls.with(scale, crate::absolute_url(url));
            }
        }
        if urls.is_empty() {
            continue;
        }
        let version = raw.id.to_string();
        known.push(version.clone());
        badges.push(Badge {
            id: BadgeId::new(sets::FFZ, version),
            title: format!("FFZ: {}", raw.title),
            urls,
        });
    }
    let mut holders: HashMap<Arc<str>, Vec<Arc<str>>> = HashMap::new();
    // Ascending badge ids: a user's badges come out in one stable order.
    let mut by_badge: Vec<(&String, &Vec<Value>)> = doc.users.iter().collect();
    by_badge.sort_by_key(|(id, _)| id.parse::<u64>().unwrap_or(u64::MAX));
    for (version, logins) in by_badge {
        if !known.contains(version) {
            continue;
        }
        let version: Arc<str> = Arc::from(version.as_str());
        for user in logins {
            // Ids are numbers (strings are accepted too).
            let id = match user {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => continue,
            };
            holders
                .entry(Arc::from(id))
                .or_default()
                .push(version.clone());
        }
    }
    Ok(BadgeHolders { badges, holders })
}

#[derive(Deserialize)]
struct ChatterinoBadge {
    #[serde(default, deserialize_with = "null_default")]
    tooltip: String,
    image1: Option<String>,
    image2: Option<String>,
    image3: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    users: Vec<String>,
}

#[derive(Deserialize)]
struct ChatterinoDocument {
    #[serde(default, deserialize_with = "null_default")]
    badges: Vec<Value>,
}

pub(crate) fn parse_chatterino(body: &[u8]) -> Result<BadgeHolders, EmoteError> {
    let doc: ChatterinoDocument = serde_json::from_slice(body).map_err(EmoteError::parse)?;
    let mut out = BadgeHolders::default();
    for (index, raw) in parse_entries::<ChatterinoBadge>("Chatterino badge", doc.badges)
        .into_iter()
        .enumerate()
    {
        let mut urls = ImageUrls::new();
        // Chatterino's third image is 3× (like BTTV's), the closest of our 4× slot.
        for (url, scale) in [
            (&raw.image1, ImageScale::X1),
            (&raw.image2, ImageScale::X2),
            (&raw.image3, ImageScale::X4),
        ] {
            if let Some(url) = url.as_ref().filter(|url| crate::trusted_image_url(url)) {
                urls = urls.with(scale, url.clone());
            }
        }
        if urls.is_empty() {
            continue;
        }
        // The tooltip already names the provider ("Chatterino Supporter").
        let title = if raw.tooltip.starts_with("Chatterino") {
            raw.tooltip.clone()
        } else {
            format!("Chatterino: {}", raw.tooltip)
        };
        // Badges are told apart by position in the document; it changes only when badges do.
        let version: Arc<str> = Arc::from(index.to_string());
        out.badges.push(Badge {
            id: BadgeId::new(sets::CHATTERINO, version.clone()),
            title,
            urls,
        });
        for user in raw.users {
            out.holders
                .entry(Arc::from(user))
                .or_default()
                .push(version.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeHttp, fixture};

    #[test]
    fn ffz_badges_are_keyed_by_twitch_id_and_titled_with_the_provider() {
        let parsed = parse_ffz(&fixture("ffz-badges.json")).unwrap();
        let bot = parsed
            .badges
            .iter()
            .find(|b| &*b.id.version == "2")
            .expect("the bot badge");
        assert_eq!(bot.id.set.as_ref(), "ffz");
        assert_eq!(bot.title, "FFZ: Bot");
        assert_eq!(
            bot.urls.get(ImageScale::X4),
            Some("https://cdn.frankerfacez.com/badge/2/4")
        );
        let holder = parsed.holders.get("73227142").expect("a bot account");
        assert_eq!(holder.iter().map(|v| &**v).collect::<Vec<_>>(), ["2"]);
        // Every holder's badges are in ascending badge-id order.
        for versions in parsed.holders.values() {
            let ids: Vec<u64> = versions.iter().map(|v| v.parse().unwrap()).collect();
            assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
        }
    }

    #[test]
    fn ffz_holders_of_a_badge_without_a_definition_are_dropped() {
        let body =
            br#"{"badges":[{"id":2,"title":"Bot","urls":{"1":"//cdn.frankerfacez.com/badge/2/1"}}],
            "users":{"2":[11, "12"],"9":[13]}}"#;
        let parsed = parse_ffz(body).unwrap();
        assert_eq!(parsed.badges.len(), 1);
        assert!(parsed.holders.contains_key("11") && parsed.holders.contains_key("12"));
        assert!(!parsed.holders.contains_key("13"));
        assert_eq!(
            parsed.badges[0].urls.get(ImageScale::X1),
            Some("https://cdn.frankerfacez.com/badge/2/1")
        );
    }

    #[test]
    fn chatterino_badges_keep_their_own_name_and_map_users_by_id() {
        let parsed = parse_chatterino(&fixture("chatterino-badges.json")).unwrap();
        let titles: Vec<_> = parsed.badges.iter().map(|b| b.title.as_str()).collect();
        assert!(titles.contains(&"Chatterino Supporter"), "{titles:?}");
        let (index, supporter) = parsed
            .badges
            .iter()
            .enumerate()
            .find(|(_, b)| b.title == "Chatterino Supporter")
            .unwrap();
        assert_eq!(supporter.id.set.as_ref(), "chatterino");
        assert_eq!(
            supporter.urls.get(ImageScale::X4).map(|u| u.contains("3x")),
            Some(true)
        );
        let holder = parsed
            .holders
            .values()
            .find(|v| v.iter().any(|v| **v == *index.to_string()));
        assert!(holder.is_some());
        // A tooltip that does not say who it is from gets the provider prefix.
        let body = br#"{"badges":[{"tooltip":"Pepe","image1":"https://fourtf.com/a/1.png","users":["1"]}]}"#;
        assert_eq!(
            parse_chatterino(body).unwrap().badges[0].title,
            "Chatterino: Pepe"
        );
    }

    #[test]
    fn a_failure_is_an_error_and_garbage_is_a_parse_error() {
        assert!(parse_ffz(b"nope").is_err());
        assert!(parse_chatterino(b"nope").is_err());
        let http = FakeHttp::new().status(FFZ_URL, 500, "");
        let result = futures_util::FutureExt::now_or_never(load_ffz(&http));
        assert_eq!(result.unwrap(), Err(EmoteError::Status(500)));
    }
}
