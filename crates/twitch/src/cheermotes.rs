//! A channel's cheermotes (PR 5.1): `Get Cheermotes`.
//!
//! `GET /helix/bits/cheermotes?broadcaster_id=<room id>` answers with Twitch's cheermotes and the
//! channel's own, each as tiers (`min_bits`) with `dark` / `light` × `animated` / `static` image
//! URLs per scale. It needs a user (or app) token and no scope, so it is only asked for when
//! logged in. The result is a [`CheermoteSet`]: for each tier the *dark* image, animated when
//! there is one (the app swaps `/dark/` for `/light/` on a light theme, like Twitch's emotes).

use std::collections::HashMap;
use std::sync::Arc;

use gpui_chat_core::{
    CheerTier, Cheermote, CheermoteSet, Emote, EmoteId, EmoteProvider, ImageScale, ImageUrls,
};
use serde::Deserialize;

use crate::helix::{HelixClient, HelixListError, encode_query_value};

/// Cheermotes are drawn this many logical pixels square at 1x.
const BASE_SIZE: u16 = 28;

#[derive(Debug, Deserialize)]
struct RawCheermote {
    prefix: String,
    #[serde(default)]
    tiers: Vec<RawTier>,
}

/// `format -> scale -> url` of one theme.
type Formats = HashMap<String, Option<HashMap<String, String>>>;

#[derive(Debug, Deserialize)]
struct RawTier {
    min_bits: u32,
    #[serde(default = "yes")]
    can_cheer: bool,
    /// `theme -> format -> scale -> url`; any level may be missing or `null` (one odd tier must not
    /// cost the whole set).
    #[serde(default)]
    images: Option<HashMap<String, Option<Formats>>>,
}

fn yes() -> bool {
    true
}

/// `scale` key of Twitch's image maps. 1.5x has no counterpart here.
fn scale_of(key: &str) -> Option<ImageScale> {
    match key {
        "1" => Some(ImageScale::X1),
        "2" => Some(ImageScale::X2),
        "3" => Some(ImageScale::X3),
        "4" => Some(ImageScale::X4),
        _ => None,
    }
}

impl RawTier {
    /// The dark images, animated if the tier has them, else static.
    fn emote(&self, prefix: &str) -> Option<Emote> {
        let dark = self.images.as_ref()?.get("dark")?.as_ref()?;
        let usable = |format: &str| {
            dark.get(format)
                .and_then(Option::as_ref)
                .filter(|urls| !urls.is_empty())
        };
        let (formats, animated) = match usable("animated") {
            Some(urls) => (urls, true),
            None => (usable("static")?, false),
        };
        let mut urls = ImageUrls::new();
        for (key, url) in formats {
            if let Some(scale) = scale_of(key) {
                urls = urls.with(scale, url.as_str());
            }
        }
        if urls.is_empty() {
            return None;
        }
        Some(Emote {
            id: EmoteId::from(format!(
                "cheer:{}:{}",
                prefix.to_ascii_lowercase(),
                self.min_bits
            )),
            name: Arc::from(prefix),
            provider: EmoteProvider::Twitch,
            urls,
            animated,
            zero_width: false,
            width: Some(BASE_SIZE),
            height: Some(BASE_SIZE),
        })
    }
}

/// Builds the set from the `data` of the answer. A tier that cannot be cheered, or has no usable
/// image, is left out; a cheermote left without tiers is dropped.
fn build(raw: Vec<RawCheermote>) -> CheermoteSet {
    CheermoteSet::new(raw.into_iter().filter_map(|cheermote| {
        let tiers: Vec<CheerTier> = cheermote
            .tiers
            .iter()
            .filter(|tier| tier.can_cheer)
            .filter_map(|tier| {
                Some(CheerTier {
                    min_bits: tier.min_bits,
                    image: Arc::new(tier.emote(&cheermote.prefix)?),
                })
            })
            .collect();
        (!tiers.is_empty()).then_some(Cheermote {
            prefix: cheermote.prefix,
            tiers,
        })
    }))
}

impl HelixClient {
    /// The cheermotes usable in the channel `broadcaster_id` (Twitch's own and the channel's).
    /// Needs a login, no scope.
    pub async fn get_cheermotes(
        &self,
        broadcaster_id: &str,
    ) -> Result<CheermoteSet, HelixListError> {
        let path = format!(
            "bits/cheermotes?broadcaster_id={}",
            encode_query_value(broadcaster_id)
        );
        // One answer, not paged.
        let raw: Vec<RawCheermote> = self.get_all(&path, 1).await?;
        Ok(build(raw))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::{Auth, MemoryTokenStore, OAuthClient, Secret, StoredToken, unix_now};

    const CLIENT: &str = "test-client-id";

    async fn client(server: &MockServer) -> HelixClient {
        let store = Arc::new(MemoryTokenStore::with(StoredToken {
            access_token: Secret::new("access"),
            refresh_token: Secret::new("refresh"),
            expires_at: unix_now() + 4 * 3600,
            scopes: Vec::new(),
            login: "viewer".to_owned(),
            user_id: "42".to_owned(),
        }));
        let oauth = OAuthClient::with_base(CLIENT, server.uri()).unwrap();
        let auth = Auth::with_scopes(oauth, store, &[]);
        let _validate = Mock::given(method("GET"))
            .and(path("/oauth2/validate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "client_id": CLIENT, "login": "viewer", "scopes": [], "user_id": "42",
                "expires_in": 0,
            })))
            .mount_as_scoped(server)
            .await;
        auth.restore().await;
        HelixClient::with_base(auth, format!("{}/helix", server.uri())).unwrap()
    }

    fn images(prefix: &str, min: u32, animated: bool) -> Value {
        let url = |theme: &str, kind: &str, scale: &str, ext: &str| {
            format!(
                "https://d3aqoihi2n8ty8.cloudfront.net/actions/{prefix}/{theme}/{kind}/{min}/{scale}.{ext}"
            )
        };
        let set = |theme: &str| {
            let mut formats = json!({ "static": {
                "1": url(theme, "static", "1", "png"), "1.5": url(theme, "static", "1.5", "png"),
                "2": url(theme, "static", "2", "png"), "4": url(theme, "static", "4", "png"),
            }});
            if animated {
                formats["animated"] = json!({
                    "1": url(theme, "animated", "1", "gif"), "2": url(theme, "animated", "2", "gif"),
                    "4": url(theme, "animated", "4", "gif"),
                });
            }
            formats
        };
        json!({ "dark": set("dark"), "light": set("light") })
    }

    fn tier(prefix: &str, min: u32, animated: bool, can_cheer: bool) -> Value {
        json!({ "min_bits": min, "id": min.to_string(), "color": "#979797",
            "images": images(prefix, min, animated), "can_cheer": can_cheer,
            "show_in_bits_card": true })
    }

    #[tokio::test]
    async fn the_set_has_a_dark_image_per_tier_animated_when_there_is_one() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/bits/cheermotes"))
            .and(query_param("broadcaster_id", "7"))
            .and(header("Client-Id", CLIENT))
            .and(header("Authorization", "Bearer access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [
                { "prefix": "Cheer", "type": "global_first_party", "tiers": [
                    tier("cheer", 1, true, true), tier("cheer", 100, true, true),
                    tier("cheer", 1000, false, true), tier("cheer", 5000, true, false),
                ]},
                { "prefix": "chanCheer", "type": "channel_custom", "tiers": [
                    tier("chancheer", 1, false, true),
                ]},
                { "prefix": "Broken", "tiers": [ { "min_bits": 1, "images": {} } ] },
                // `null` images, formats and scales on one tier: that tier is skipped, the
                // others (and the cheermote) stay.
                { "prefix": "Nully", "tiers": [
                    { "min_bits": 1, "images": null },
                    { "min_bits": 100, "images": { "dark": null } },
                    { "min_bits": 1000, "images": { "dark": { "animated": null, "static": null } } },
                    tier("nully", 5000, false, true),
                ]},
            ]})))
            .expect(1)
            .mount(&server)
            .await;

        let set = helix.get_cheermotes("7").await.unwrap();
        assert_eq!(set.len(), 3, "a cheermote with no usable image is dropped");
        // The tiers with `null` images are skipped, the good one of that cheermote is kept.
        assert_eq!(set.tier("Nully", 9999).unwrap().min_bits, 5000);
        assert!(set.tier("Nully", 20).is_none());

        let hundred = set.tier("Cheer", 250).unwrap();
        assert_eq!(hundred.min_bits, 100);
        assert!(hundred.image.animated);
        assert_eq!(
            hundred.image.urls.get(ImageScale::X1),
            Some("https://d3aqoihi2n8ty8.cloudfront.net/actions/cheer/dark/animated/100/1.gif")
        );
        assert_eq!(
            hundred
                .image
                .urls
                .get(ImageScale::X2)
                .map(|u| u.ends_with("/2.gif")),
            Some(true)
        );
        assert_eq!(hundred.image.urls.get(ImageScale::X3), None);
        assert_eq!(&*hundred.image.name, "Cheer");
        assert_eq!(hundred.image.provider, EmoteProvider::Twitch);
        assert_eq!(
            (hundred.image.width, hundred.image.height),
            (Some(28), Some(28))
        );

        // No animated images: the static ones.
        let thousand = set.tier("cheer", 1000).unwrap();
        assert!(!thousand.image.animated);
        assert!(
            thousand
                .image
                .urls
                .get(ImageScale::X1)
                .unwrap()
                .contains("/dark/static/1000/")
        );
        // A tier that cannot be cheered is never drawn: 5000 bits use the 1000 tier.
        assert_eq!(set.tier("Cheer", 9999).unwrap().min_bits, 1000);
        // The channel's own prefix.
        assert!(set.tier("CHANCHEER", 20).is_some());
        assert_ne!(
            set.tier("Cheer", 1).unwrap().image.id,
            set.tier("Cheer", 100).unwrap().image.id
        );
    }

    #[tokio::test]
    async fn an_error_answer_is_reported() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/bits/cheermotes"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({ "message": "Missing broadcaster" })),
            )
            .mount(&server)
            .await;
        let error = helix.get_cheermotes("").await.unwrap_err();
        assert!(
            matches!(error, HelixListError::Status { status: 400, .. }),
            "{error:?}"
        );
    }
}
