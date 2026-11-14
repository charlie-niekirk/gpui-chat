//! The bundled snapshot of Twitch's global badges (`assets/badges/global.json`), so moderator,
//! VIP, broadcaster, staff and the many event badges resolve offline and before any request.
//!
//! The file is compact JSON, `{"version": 1, "badges": {"<set>": [["<version>", "<title>",
//! "<image id>"], ...]}}`. The image id is the part of Twitch's CDN URL that identifies the
//! image: the three sizes are `https://static-cdn.jtvnw.net/badges/v1/<id>/1`, `/2` and `/3`
//! (`1x`, `2x`, `4x`). A badge whose URLs do not follow that pattern cannot be encoded
//! ([`encode`] fails rather than guessing), so the file stays small and every URL in it is one
//! Twitch published.
//!
//! Regenerate it with `cargo run -p gpui-chat-emotes --example badges_snapshot` (see
//! `docs/ARCHITECTURE.md`); the test below checks that the committed file decodes and has the
//! badges chat needs.

use std::collections::BTreeMap;

use gpui_chat_core::{Badge, BadgeId, ImageScale, ImageUrls};
use serde::{Deserialize, Serialize};

use super::{BadgeError, BadgeSet};

/// The snapshot as committed in `assets/badges/global.json`.
pub const BUNDLED: &str = include_str!("../../../../assets/badges/global.json");

const FORMAT_VERSION: u32 = 1;
const CDN: &str = "https://static-cdn.jtvnw.net/badges/v1/";

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    badges: BTreeMap<String, Vec<(String, String, String)>>,
}

/// The bundled global badges.
pub fn bundled() -> Result<BadgeSet, BadgeError> {
    decode(BUNDLED)
}

/// Reads a snapshot.
pub fn decode(json: &str) -> Result<BadgeSet, BadgeError> {
    let file: File = serde_json::from_str(json).map_err(|e| BadgeError::Snapshot(e.to_string()))?;
    if file.version != FORMAT_VERSION {
        return Err(BadgeError::Snapshot(format!(
            "unsupported format version {}",
            file.version
        )));
    }
    let mut set = BadgeSet::new();
    for (name, versions) in file.badges {
        for (version, title, image) in versions {
            let url = |n: u8| format!("{CDN}{image}/{n}");
            set.insert(Badge {
                id: BadgeId::new(name.as_str(), version),
                title,
                urls: ImageUrls::new()
                    .with(ImageScale::X1, url(1))
                    .with(ImageScale::X2, url(2))
                    .with(ImageScale::X4, url(3)),
            });
        }
    }
    Ok(set)
}

/// Writes `set` as a snapshot: deterministic (sorted), one line.
pub fn encode(set: &BadgeSet) -> Result<String, BadgeError> {
    let mut badges: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    for badge in set.iter_sorted() {
        let image = image_id(badge).ok_or_else(|| {
            BadgeError::Snapshot(format!(
                "{}/{} has image URLs that are not Twitch's /1 /2 /3 CDN sizes",
                badge.id.set, badge.id.version
            ))
        })?;
        badges.entry(badge.id.set.to_string()).or_default().push((
            badge.id.version.to_string(),
            badge.title.clone(),
            image,
        ));
    }
    let file = File {
        version: FORMAT_VERSION,
        badges,
    };
    serde_json::to_string(&file).map_err(|e| BadgeError::Snapshot(e.to_string()))
}

/// The CDN image id of `badge`, if its URLs are exactly the three standard sizes of one image.
fn image_id(badge: &Badge) -> Option<String> {
    let one = badge.urls.get(ImageScale::X1)?;
    let id = one.strip_prefix(CDN)?.strip_suffix("/1")?;
    if id.is_empty() || id.contains(['/', '"', '\\']) {
        return None;
    }
    let sizes_follow = badge.urls.get(ImageScale::X2) == Some(&format!("{CDN}{id}/2"))
        && badge.urls.get(ImageScale::X4) == Some(&format!("{CDN}{id}/3"))
        && badge.urls.get(ImageScale::X3).is_none();
    sizes_follow.then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::badges::ivr;
    use crate::testutil::badge_fixture;

    #[test]
    fn the_committed_snapshot_decodes_and_has_what_chat_needs() {
        let set = bundled().unwrap();
        assert!(set.len() > 100, "{} badges", set.len());
        for (name, version, title) in [
            ("moderator", "1", "Moderator"),
            ("broadcaster", "1", "Broadcaster"),
            ("vip", "1", "VIP"),
            ("premium", "1", "Prime Gaming"),
        ] {
            let badge = set
                .get(name, version)
                .unwrap_or_else(|| panic!("{name}/{version}"));
            assert_eq!(badge.title, title, "{name}/{version}");
            for scale in [ImageScale::X1, ImageScale::X2, ImageScale::X4] {
                let url = badge.urls.get(scale).unwrap();
                assert!(url.starts_with(CDN), "{url}");
            }
        }
        // The snapshot is what the generator would write for what it contains.
        assert_eq!(encode(&set).unwrap(), BUNDLED.trim_end());
    }

    #[test]
    fn encode_then_decode_round_trips() {
        let set = ivr::parse(&badge_fixture("ivr-global-sample.json")).unwrap();
        let json = encode(&set).unwrap();
        assert!(!json.contains('\n'));
        assert_eq!(decode(&json).unwrap(), set);
        // Deterministic.
        assert_eq!(encode(&decode(&json).unwrap()).unwrap(), json);
    }

    #[test]
    fn urls_that_are_not_the_standard_sizes_are_refused() {
        let odd = |urls: ImageUrls| {
            let mut set = BadgeSet::new();
            set.insert(Badge {
                id: BadgeId::new("x", "1"),
                title: "X".into(),
                urls,
            });
            encode(&set)
        };
        assert!(matches!(
            odd(ImageUrls::new().with(ImageScale::X1, "https://example.com/a/1")),
            Err(BadgeError::Snapshot(_))
        ));
        let id = "6ae7ce40-99e6-4d83-8487-f8b990bf5f32";
        // A 2x that is not this image's 2x.
        assert!(
            odd(ImageUrls::new()
                .with(ImageScale::X1, format!("{CDN}{id}/1"))
                .with(ImageScale::X2, format!("{CDN}other/2")))
            .is_err()
        );
        // Only the 1x of an image: not representable (the decoder would invent the others).
        assert!(odd(ImageUrls::new().with(ImageScale::X1, format!("{CDN}{id}/1"))).is_err());
    }

    #[test]
    fn a_snapshot_of_another_version_or_garbage_is_refused() {
        assert!(decode("{\"version\":2,\"badges\":{}}").is_err());
        assert!(decode("nope").is_err());
        assert!(decode("{\"version\":1,\"badges\":{}}").unwrap().is_empty());
    }
}
