//! The logged-in badge source: Twitch's own Helix endpoints.
//!
//! * `GET <helix>/chat/badges/global` → the global badges (the live set, not the bundled snapshot);
//! * `GET <helix>/chat/badges?broadcaster_id=<room id>` → a channel's.
//!
//! Both answer `{"data": [...]}` with the items [`super::ivr::parse`] already reads. Helix needs a
//! `Client-Id` and a user token on every request: the [`HttpClient`] given here is expected to add
//! them (the app passes one backed by `gpui_chat_twitch::HelixClient`, which also refreshes the
//! token on a 401), so this crate never sees a token.

use std::sync::Arc;

use super::{BadgeError, BadgeFuture, BadgeSet, BadgeSource, ivr};
use crate::http::HttpClient;
use crate::validate_user_id;

/// URL of the global badges below the Helix `base` (e.g. `https://api.twitch.tv/helix`).
pub fn global_url(base: &str) -> String {
    format!("{}/chat/badges/global", base.trim_end_matches('/'))
}

/// URL of a channel's badges below the Helix `base`.
pub fn channel_url(base: &str, broadcaster_id: &str) -> String {
    format!(
        "{}/chat/badges?broadcaster_id={broadcaster_id}",
        base.trim_end_matches('/')
    )
}

/// Badges from Helix, through an authenticating [`HttpClient`].
#[derive(Clone)]
pub struct HelixBadgeSource {
    http: Arc<dyn HttpClient>,
    base: String,
}

impl HelixBadgeSource {
    /// A source GETting `<base>/chat/badges...` through `http`, which must authenticate the
    /// requests.
    pub fn new(http: Arc<dyn HttpClient>, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into(),
        }
    }

    async fn fetch(&self, url: String) -> Result<BadgeSet, BadgeError> {
        let response = self.http.get(&url).await?;
        match response.status {
            200..=299 => ivr::parse(&response.body),
            status => Err(BadgeError::Status(status)),
        }
    }

    async fn channel_set(&self, room_id: &str) -> Result<BadgeSet, BadgeError> {
        let id =
            validate_user_id(room_id).map_err(|_| BadgeError::InvalidUserId(room_id.to_owned()))?;
        self.fetch(channel_url(&self.base, id)).await
    }
}

impl BadgeSource for HelixBadgeSource {
    fn global(&self) -> BadgeFuture<'_> {
        Box::pin(self.fetch(global_url(&self.base)))
    }

    fn channel<'a>(&'a self, room_id: &'a str) -> BadgeFuture<'a> {
        Box::pin(self.channel_set(room_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpError;
    use crate::testutil::{FakeHttp, badge_fixture};

    const BASE: &str = "https://api.twitch.tv/helix";
    const GLOBAL: &str = "https://api.twitch.tv/helix/chat/badges/global";
    const CHANNEL: &str = "https://api.twitch.tv/helix/chat/badges?broadcaster_id=71092938";

    /// IVR's bare list wrapped the way Helix sends it.
    fn helix_body(fixture: &str) -> Vec<u8> {
        let list: serde_json::Value = serde_json::from_slice(&badge_fixture(fixture)).unwrap();
        serde_json::to_vec(&serde_json::json!({ "data": list })).unwrap()
    }

    fn helix(fake: FakeHttp) -> (HelixBadgeSource, Arc<FakeHttp>) {
        let fake = Arc::new(fake);
        (HelixBadgeSource::new(fake.clone(), BASE), fake)
    }

    #[test]
    fn urls() {
        assert_eq!(global_url(BASE), GLOBAL);
        assert_eq!(
            global_url("http://127.0.0.1:1/helix/"),
            "http://127.0.0.1:1/helix/chat/badges/global"
        );
        assert_eq!(channel_url(BASE, "71092938"), CHANNEL);
    }

    #[tokio::test]
    async fn global_and_channel_badges_come_from_helix() {
        let (source, fake) = helix(
            FakeHttp::new()
                .body(GLOBAL, 200, helix_body("ivr-global-sample.json"))
                .body(CHANNEL, 200, helix_body("ivr-channel-sample.json")),
        );
        let global = source.global().await.unwrap();
        assert_eq!(global.get("moderator", "1").unwrap().title, "Moderator");
        let channel = source.channel("71092938").await.unwrap();
        assert!(channel.get("subscriber", "0").is_some());
        assert_eq!(fake.requested(), [GLOBAL, CHANNEL]);
    }

    #[tokio::test]
    async fn failures_are_errors_and_bad_ids_are_refused_without_a_request() {
        let (source, _) = helix(FakeHttp::new().status(GLOBAL, 401, "{}"));
        assert_eq!(source.global().await, Err(BadgeError::Status(401)));

        let (source, _) = helix(FakeHttp::new().fail(CHANNEL, HttpError::Timeout));
        assert_eq!(
            source.channel("71092938").await,
            Err(BadgeError::Http(HttpError::Timeout))
        );

        let (source, _) = helix(FakeHttp::new().status(CHANNEL, 200, "[oops"));
        assert!(matches!(
            source.channel("71092938").await,
            Err(BadgeError::Parse(_))
        ));

        let (source, fake) = helix(FakeHttp::new());
        assert!(matches!(
            source.channel("1&x=2").await,
            Err(BadgeError::InvalidUserId(_))
        ));
        assert!(fake.requested().is_empty());
    }
}
