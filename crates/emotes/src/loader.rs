use std::sync::Arc;

use gpui_chat_core::EmoteProvider;

use crate::bttv::BttvClient;
use crate::error::EmoteError;
use crate::ffz::FfzClient;
use crate::http::{HttpClient, HttpError, ReqwestHttp};
use crate::resolver::Providers;
use crate::set::{EmoteScope, EmoteSet};
use crate::seventv::SevenTvClient;

/// One result per provider for one scope. Each is independent: a failing provider only
/// shows up as an `Err` in its own field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResults {
    pub seven_tv: Result<EmoteSet, EmoteError>,
    pub bttv: Result<EmoteSet, EmoteError>,
    pub ffz: Result<EmoteSet, EmoteError>,
}

impl ProviderResults {
    /// The three results in resolution-precedence order: 7TV, BTTV, FFZ.
    pub fn iter(&self) -> impl Iterator<Item = (EmoteProvider, &Result<EmoteSet, EmoteError>)> {
        [
            (EmoteProvider::SevenTv, &self.seven_tv),
            (EmoteProvider::Bttv, &self.bttv),
            (EmoteProvider::Ffz, &self.ffz),
        ]
        .into_iter()
    }

    /// The sets that loaded (empty ones included), in precedence order.
    pub fn sets(&self) -> impl Iterator<Item = &EmoteSet> {
        self.iter().filter_map(|(_, r)| r.as_ref().ok())
    }

    /// The providers that failed, in precedence order.
    pub fn errors(&self) -> impl Iterator<Item = (EmoteProvider, &EmoteError)> {
        self.iter()
            .filter_map(|(p, r)| r.as_ref().err().map(|e| (p, e)))
    }

    /// `true` if every provider loaded.
    pub fn all_ok(&self) -> bool {
        self.errors().next().is_none()
    }
}

/// Loads emote sets from all three providers concurrently.
#[derive(Clone)]
pub struct EmoteLoader {
    pub seven_tv: SevenTvClient,
    pub bttv: BttvClient,
    pub ffz: FfzClient,
}

impl EmoteLoader {
    /// Clients with the default endpoints sharing one HTTP client.
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self {
            seven_tv: SevenTvClient::new(http.clone()),
            bttv: BttvClient::new(http.clone()),
            ffz: FfzClient::new(http),
        }
    }

    /// [`EmoteLoader::new`] over a default [`ReqwestHttp`] (`<APP_NAME>/<version>`
    /// user agent, timeouts).
    pub fn with_default_http() -> Result<Self, HttpError> {
        Ok(Self::new(Arc::new(ReqwestHttp::new()?)))
    }

    /// Global sets of all providers.
    pub async fn load_global(&self) -> ProviderResults {
        self.load_global_with(Providers::ALL).await
    }

    /// Global sets of the `providers` that are on; the others are not requested and come back
    /// as empty sets.
    pub async fn load_global_with(&self, providers: Providers) -> ProviderResults {
        let scope = EmoteScope::Global;
        let (seven_tv, bttv, ffz) = tokio::join!(
            gated(
                providers.seven_tv,
                EmoteProvider::SevenTv,
                scope,
                self.seven_tv.global()
            ),
            gated(
                providers.bttv,
                EmoteProvider::Bttv,
                scope,
                self.bttv.global()
            ),
            gated(providers.ffz, EmoteProvider::Ffz, scope, self.ffz.global())
        );
        log_failures(
            "global",
            &ProviderResults {
                seven_tv,
                bttv,
                ffz,
            },
        )
    }

    /// Channel sets for a Twitch user id (the `room-id` of ROOMSTATE). A provider the
    /// channel has no account at yields an empty set.
    pub async fn load_channel(&self, twitch_user_id: &str) -> ProviderResults {
        self.load_channel_with(twitch_user_id, Providers::ALL).await
    }

    /// [`EmoteLoader::load_channel`] for the `providers` that are on only; the others are not
    /// requested and come back as empty sets.
    pub async fn load_channel_with(
        &self,
        twitch_user_id: &str,
        providers: Providers,
    ) -> ProviderResults {
        let scope = EmoteScope::Channel;
        let (seven_tv, bttv, ffz) = tokio::join!(
            gated(
                providers.seven_tv,
                EmoteProvider::SevenTv,
                scope,
                self.seven_tv.channel(twitch_user_id)
            ),
            gated(
                providers.bttv,
                EmoteProvider::Bttv,
                scope,
                self.bttv.channel(twitch_user_id)
            ),
            gated(
                providers.ffz,
                EmoteProvider::Ffz,
                scope,
                self.ffz.room(twitch_user_id)
            )
        );
        log_failures(
            "channel",
            &ProviderResults {
                seven_tv,
                bttv,
                ffz,
            },
        )
    }
}

/// Runs `request` only if `enabled`; a provider that is off is an empty set, not an error.
async fn gated(
    enabled: bool,
    provider: EmoteProvider,
    scope: EmoteScope,
    request: impl std::future::Future<Output = Result<EmoteSet, EmoteError>>,
) -> Result<EmoteSet, EmoteError> {
    if enabled {
        request.await
    } else {
        Ok(EmoteSet::empty(provider, scope))
    }
}

fn log_failures(scope: &str, results: &ProviderResults) -> ProviderResults {
    for (provider, error) in results.errors() {
        tracing::warn!(provider = provider.name(), scope, %error, "emote provider failed");
    }
    results.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpError;
    use crate::set::EmoteScope;
    use crate::testutil::FakeHttp;

    const ID: &str = "11148817";

    fn all_fixtures() -> FakeHttp {
        FakeHttp::new()
            .json("https://7tv.io/v3/emote-sets/global", "7tv-global.json")
            .json(
                "https://api.betterttv.net/3/cached/emotes/global",
                "bttv-global.json",
            )
            .json(
                "https://api.frankerfacez.com/v1/set/global",
                "ffz-global.json",
            )
            .json(
                "https://7tv.io/v3/users/twitch/11148817",
                "7tv-user-pajlada.json",
            )
            .json(
                "https://api.betterttv.net/3/cached/users/twitch/11148817",
                "bttv-channel-pajlada.json",
            )
            .json(
                "https://api.frankerfacez.com/v1/room/id/11148817",
                "ffz-room-pajlada.json",
            )
    }

    #[tokio::test]
    async fn loads_everything_in_precedence_order() {
        let loader = EmoteLoader::new(Arc::new(all_fixtures()));
        let global = loader.load_global().await;
        assert!(global.all_ok());
        let channel = loader.load_channel(ID).await;
        assert!(channel.all_ok());
        let order: Vec<_> = channel.sets().map(|s| (s.provider, s.scope)).collect();
        assert_eq!(
            order,
            [
                (EmoteProvider::SevenTv, EmoteScope::Channel),
                (EmoteProvider::Bttv, EmoteScope::Channel),
                (EmoteProvider::Ffz, EmoteScope::Channel),
            ]
        );
    }

    #[tokio::test]
    async fn one_provider_failing_does_not_block_the_others() {
        let fake = FakeHttp::new()
            .fail(
                "https://7tv.io/v3/users/twitch/11148817",
                HttpError::Timeout,
            )
            .status(
                "https://api.betterttv.net/3/cached/users/twitch/11148817",
                500,
                "",
            )
            .json(
                "https://api.frankerfacez.com/v1/room/id/11148817",
                "ffz-room-pajlada.json",
            );
        let results = EmoteLoader::new(Arc::new(fake)).load_channel(ID).await;
        assert_eq!(results.seven_tv, Err(EmoteError::Http(HttpError::Timeout)));
        assert_eq!(results.bttv, Err(EmoteError::Status(500)));
        assert_eq!(results.ffz.as_ref().unwrap().len(), 6);
        assert!(!results.all_ok());
        let failed: Vec<_> = results.errors().map(|(p, _)| p).collect();
        assert_eq!(failed, [EmoteProvider::SevenTv, EmoteProvider::Bttv]);
        assert_eq!(results.sets().count(), 1);
    }

    #[tokio::test]
    async fn channel_without_accounts_is_all_empty_not_errors() {
        let fake = FakeHttp::new()
            .status("https://7tv.io/v3/users/twitch/1", 404, "{}")
            .status(
                "https://api.betterttv.net/3/cached/users/twitch/1",
                404,
                "{}",
            )
            .status("https://api.frankerfacez.com/v1/room/id/1", 404, "{}");
        let results = EmoteLoader::new(Arc::new(fake)).load_channel("1").await;
        assert!(results.all_ok());
        assert!(results.sets().all(EmoteSet::is_empty));
        assert_eq!(results.sets().count(), 3);
    }

    #[tokio::test]
    async fn providers_that_are_off_are_not_requested() {
        let fake = Arc::new(all_fixtures());
        let loader = EmoteLoader::new(fake.clone());
        let only_bttv = Providers {
            seven_tv: false,
            bttv: true,
            ffz: false,
        };
        let global = loader.load_global_with(only_bttv).await;
        let channel = loader.load_channel_with(ID, only_bttv).await;
        for results in [&global, &channel] {
            assert!(results.all_ok());
            assert!(results.seven_tv.as_ref().unwrap().is_empty());
            assert!(results.ffz.as_ref().unwrap().is_empty());
            assert!(!results.bttv.as_ref().unwrap().is_empty());
        }
        let requested = fake.requested();
        assert_eq!(requested.len(), 2, "{requested:?}");
        assert!(requested.iter().all(|url| url.contains("betterttv")));
    }

    #[tokio::test]
    async fn invalid_id_fails_every_provider_without_network() {
        let fake = Arc::new(FakeHttp::new());
        let results = EmoteLoader::new(fake.clone()).load_channel("nope").await;
        assert_eq!(results.errors().count(), 3);
        assert!(fake.requested().is_empty());
    }
}
