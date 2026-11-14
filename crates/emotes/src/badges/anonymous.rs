use std::sync::Arc;

use super::{BadgeError, BadgeFuture, BadgeSet, BadgeSource, ivr, snapshot};
use crate::http::HttpClient;
use crate::validate_user_id;

/// The badge source that needs no login: the global badges are the bundled snapshot
/// ([`snapshot::bundled`], no request), a channel's come from `api.ivr.fi`.
///
/// A failing request is the caller's to log and retry; nothing here falls back to anything else,
/// since there is no other anonymous source: the badges are then simply missing.
#[derive(Clone)]
pub struct AnonymousBadgeSource {
    http: Arc<dyn HttpClient>,
    base: String,
}

impl AnonymousBadgeSource {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self::with_base(http, ivr::BASE_URL)
    }

    /// A source talking to `base` instead of `https://api.ivr.fi` (tests, mirrors).
    pub fn with_base(http: Arc<dyn HttpClient>, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into(),
        }
    }

    async fn channel_set(&self, room_id: &str) -> Result<BadgeSet, BadgeError> {
        let id =
            validate_user_id(room_id).map_err(|_| BadgeError::InvalidUserId(room_id.to_owned()))?;
        let response = self.http.get(&ivr::channel_url(&self.base, id)).await?;
        match response.status {
            200..=299 => ivr::parse(&response.body),
            // A channel the API does not know has no badges of its own.
            404 => Ok(BadgeSet::new()),
            status => Err(BadgeError::Status(status)),
        }
    }
}

impl BadgeSource for AnonymousBadgeSource {
    fn global(&self) -> BadgeFuture<'_> {
        Box::pin(std::future::ready(snapshot::bundled()))
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

    const URL: &str = "https://api.ivr.fi/v2/twitch/badges/channel?id=71092938";

    fn anonymous(fake: FakeHttp) -> (AnonymousBadgeSource, Arc<FakeHttp>) {
        let fake = Arc::new(fake);
        (AnonymousBadgeSource::new(fake.clone()), fake)
    }

    #[tokio::test]
    async fn the_global_badges_are_bundled_and_need_no_request() {
        let (source, fake) = anonymous(FakeHttp::new());
        let set = source.global().await.unwrap();
        assert!(set.get("moderator", "1").is_some());
        assert!(fake.requested().is_empty());
    }

    #[tokio::test]
    async fn a_channels_badges_come_from_ivr() {
        let (source, fake) =
            anonymous(FakeHttp::new().body(URL, 200, badge_fixture("ivr-channel-sample.json")));
        let set = source.channel("71092938").await.unwrap();
        assert!(set.get("subscriber", "0").is_some());
        assert_eq!(fake.requested(), [URL]);
    }

    #[tokio::test]
    async fn an_unknown_channel_is_empty_and_other_failures_are_errors() {
        let (source, _) = anonymous(FakeHttp::new().status(URL, 404, "{}"));
        assert!(source.channel("71092938").await.unwrap().is_empty());

        let (source, _) = anonymous(FakeHttp::new().status(URL, 503, ""));
        assert_eq!(
            source.channel("71092938").await,
            Err(BadgeError::Status(503))
        );

        let (source, _) = anonymous(FakeHttp::new().fail(URL, HttpError::Timeout));
        assert_eq!(
            source.channel("71092938").await,
            Err(BadgeError::Http(HttpError::Timeout))
        );

        let (source, _) = anonymous(FakeHttp::new().status(URL, 200, "<html>"));
        assert!(matches!(
            source.channel("71092938").await,
            Err(BadgeError::Parse(_))
        ));
    }

    #[tokio::test]
    async fn a_bad_room_id_is_refused_without_a_request() {
        let (source, fake) = anonymous(FakeHttp::new());
        for bad in ["", "abc", "1&x=2", "1/2"] {
            assert!(
                matches!(source.channel(bad).await, Err(BadgeError::InvalidUserId(_))),
                "{bad:?}"
            );
        }
        assert!(fake.requested().is_empty());
    }
}
