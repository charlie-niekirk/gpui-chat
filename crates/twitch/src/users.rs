//! User lookups for the user card (PR 5.4): `Get Users` and `Get Channel Followers`.
//!
//! * `GET /helix/users?login=<login>` (or `id=`): the account's id, login, display name, avatar
//!   (`profile_image_url`) and creation time (`created_at`). Any user or app token works, no
//!   scope; it is only asked for while logged in, since Helix has no anonymous access. A login
//!   no account has is `Ok(None)`, not an error.
//! * `GET /helix/channels/followers?broadcaster_id=&user_id=`: when the user followed the
//!   channel (`followed_at`). Twitch requires the scope `moderator:read:followers` **and** that
//!   the logged-in user is the broadcaster or a moderator of the channel, and the app does not
//!   ask for that scope at login (see [`crate::auth::SCOPES`]): the card tries it only for a
//!   moderator and treats a refusal as "unknown", so the follow age shows up for a login that
//!   happens to hold the scope and is left out for every other.

use gpui_chat_core::parse_rfc3339;
use serde::Deserialize;

use crate::helix::{HelixClient, HelixListError, encode_query_value};

/// The scope `Get Channel Followers` needs to read one user's follow.
pub const FOLLOWERS_SCOPE: &str = "moderator:read:followers";

/// Which user to look up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserQuery<'a> {
    Login(&'a str),
    Id(&'a str),
}

/// What `Get Users` says about an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserProfile {
    pub id: String,
    /// Lowercase login.
    pub login: String,
    pub display_name: String,
    /// The avatar's URL (a 300×300 image), when the account has one.
    pub avatar_url: Option<String>,
    /// When the account was created, in Unix seconds; `None` if Twitch sent no usable date.
    pub created_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Users {
    #[serde(default)]
    data: Vec<RawUser>,
}

#[derive(Debug, Deserialize)]
struct RawUser {
    id: String,
    #[serde(default)]
    login: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    profile_image_url: String,
    #[serde(default)]
    created_at: String,
}

impl RawUser {
    fn into_profile(self) -> UserProfile {
        UserProfile {
            display_name: if self.display_name.trim().is_empty() {
                self.login.clone()
            } else {
                self.display_name
            },
            avatar_url: Some(self.profile_image_url).filter(|url| is_image_url(url)),
            created_at: parse_rfc3339(&self.created_at),
            id: self.id,
            login: self.login,
        }
    }
}

/// Only an `https` URL is fetched as an avatar.
fn is_image_url(url: &str) -> bool {
    url.starts_with("https://")
}

#[derive(Debug, Deserialize)]
struct Followers {
    #[serde(default)]
    data: Vec<RawFollow>,
}

#[derive(Debug, Deserialize)]
struct RawFollow {
    #[serde(default)]
    followed_at: String,
}

fn status_error(status: u16, body: &[u8]) -> HelixListError {
    #[derive(Deserialize)]
    struct Body {
        message: Option<String>,
    }
    HelixListError::Status {
        status,
        message: serde_json::from_slice::<Body>(body)
            .ok()
            .and_then(|body| body.message)
            .unwrap_or_default(),
    }
}

impl HelixClient {
    /// The account `query` names; `Ok(None)` when Twitch has no such user. Needs a login, no
    /// scope.
    pub async fn get_user(
        &self,
        query: UserQuery<'_>,
    ) -> Result<Option<UserProfile>, HelixListError> {
        let (key, value) = match query {
            UserQuery::Login(login) => ("login", login),
            UserQuery::Id(id) => ("id", id),
        };
        let response = self
            .get(&format!("users?{key}={}", encode_query_value(value)))
            .await?;
        // A malformed login is a 400 from Twitch: nobody has it.
        if response.status == 400 {
            return Ok(None);
        }
        if !(200..300).contains(&response.status) {
            return Err(status_error(response.status, &response.body));
        }
        let users: Users = serde_json::from_slice(&response.body)
            .map_err(|error| HelixListError::Parse(error.to_string()))?;
        Ok(users.data.into_iter().next().map(RawUser::into_profile))
    }

    /// When `user_id` followed the channel `broadcaster_id`, in Unix seconds; `Ok(None)` when
    /// they do not follow it (or Twitch sent no usable date). Needs [`FOLLOWERS_SCOPE`] and a
    /// logged-in broadcaster or moderator of that channel: anything else is an error
    /// ([`HelixListError::is_missing_scope`] for the scope).
    pub async fn get_follow_age(
        &self,
        broadcaster_id: &str,
        user_id: &str,
    ) -> Result<Option<i64>, HelixListError> {
        let response = self
            .get(&format!(
                "channels/followers?broadcaster_id={}&user_id={}",
                encode_query_value(broadcaster_id),
                encode_query_value(user_id)
            ))
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(status_error(response.status, &response.body));
        }
        let followers: Followers = serde_json::from_slice(&response.body)
            .map_err(|error| HelixListError::Parse(error.to_string()))?;
        Ok(followers
            .data
            .first()
            .and_then(|follow| parse_rfc3339(&follow.followed_at)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
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

    fn user_body() -> serde_json::Value {
        json!({ "data": [{
            "id": "141981764", "login": "twitchdev", "display_name": "TwitchDev",
            "type": "", "broadcaster_type": "partner", "description": "Supporting third-party developers.",
            "profile_image_url": "https://static-cdn.jtvnw.net/jtv_user_pictures/8a6381c7-profile_image-300x300.png",
            "offline_image_url": "", "view_count": 5980557,
            "created_at": "2016-12-14T20:32:28Z",
        }] })
    }

    #[tokio::test]
    async fn a_user_is_looked_up_by_login_with_the_token_and_client_id() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/users"))
            .and(query_param("login", "TwitchDev"))
            .and(header("Client-Id", CLIENT))
            .and(header("Authorization", "Bearer access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(user_body()))
            .expect(1)
            .mount(&server)
            .await;
        let profile = helix
            .get_user(UserQuery::Login("TwitchDev"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            profile,
            UserProfile {
                id: "141981764".into(),
                login: "twitchdev".into(),
                display_name: "TwitchDev".into(),
                avatar_url: Some(
                    "https://static-cdn.jtvnw.net/jtv_user_pictures/8a6381c7-profile_image-300x300.png"
                        .into()
                ),
                created_at: Some(1_481_747_548),
            }
        );
    }

    #[tokio::test]
    async fn a_user_can_be_looked_up_by_id() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/users"))
            .and(query_param("id", "141981764"))
            .respond_with(ResponseTemplate::new(200).set_body_json(user_body()))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            helix
                .get_user(UserQuery::Id("141981764"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_unknown_login_is_none_not_an_error() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/users"))
            .and(query_param("login", "nobody"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/helix/users"))
            .and(query_param("login", "bad login"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                json!({ "error": "Bad Request", "status": 400, "message": "Invalid login" }),
            ))
            .mount(&server)
            .await;
        assert_eq!(
            helix.get_user(UserQuery::Login("nobody")).await.unwrap(),
            None
        );
        assert_eq!(
            helix.get_user(UserQuery::Login("bad login")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn odd_fields_degrade_instead_of_failing() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/users"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [{
                "id": "1", "login": "plain", "display_name": " ",
                // Not https, and a date Twitch should not send.
                "profile_image_url": "http://example.com/a.png", "created_at": "long ago",
            }] })))
            .mount(&server)
            .await;
        let profile = helix
            .get_user(UserQuery::Login("plain"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(profile.display_name, "plain");
        assert_eq!(profile.avatar_url, None);
        assert_eq!(profile.created_at, None);
    }

    #[tokio::test]
    async fn twitch_refusals_and_garbage_are_errors() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/users"))
            .and(query_param("login", "a"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "message": "boom" })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/helix/users"))
            .and(query_param("login", "b"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        assert!(matches!(
            helix.get_user(UserQuery::Login("a")).await,
            Err(HelixListError::Status { status: 500, ref message }) if message == "boom"
        ));
        assert!(matches!(
            helix.get_user(UserQuery::Login("b")).await,
            Err(HelixListError::Parse(_))
        ));
    }

    #[tokio::test]
    async fn the_follow_age_is_the_followed_at_of_the_one_follow() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/channels/followers"))
            .and(query_param("broadcaster_id", "1000"))
            .and(query_param("user_id", "77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "data": [{ "user_id": "77", "user_login": "bob", "followed_at": "2022-05-24T22:22:08Z" }],
                "pagination": {},
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            helix.get_follow_age("1000", "77").await.unwrap(),
            Some(1_653_430_928)
        );
    }

    #[tokio::test]
    async fn a_user_who_does_not_follow_has_no_follow_age() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/channels/followers"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "total": 0, "data": [] })),
            )
            .mount(&server)
            .await;
        assert_eq!(helix.get_follow_age("1000", "77").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_login_without_the_followers_scope_is_a_missing_scope_error() {
        let server = MockServer::start().await;
        let helix = client(&server).await;
        Mock::given(method("GET"))
            .and(path("/helix/channels/followers"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "Unauthorized", "status": 401,
                "message": "Missing scope: moderator:read:followers",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let error = helix.get_follow_age("1000", "77").await.unwrap_err();
        assert!(error.is_missing_scope(), "{error}");
    }
}
