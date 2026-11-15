//! The Twitch emotes the logged-in user may use (PR 3.3): `Get User Emotes`.
//!
//! `GET /helix/chat/emotes/user?user_id=<id>[&broadcaster_id=<room id>]`, scope `user:read:emotes`.
//! Without a broadcaster it answers with every emote the user can use anywhere (global, subscriber,
//! bits and follower emotes of the channels they are part of); with one it adds that channel's
//! follower emotes. The input box asks per channel, so the list is a superset for that room.
//! Paged by `pagination.cursor` through [`HelixClient::get_all`].

use std::collections::HashSet;

use gpui_chat_core::Emote;
use serde::Deserialize;

use crate::helix::{HelixClient, HelixListError, encode_query_value};

/// The scope `Get User Emotes` needs.
pub const SCOPE: &str = "user:read:emotes";

/// Upper bound on pages read (a heavily subscribed user has a few thousand emotes).
const MAX_PAGES: usize = 100;

/// One emote the user can use, with where Twitch says it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEmote {
    /// The emote with Twitch's standard image URLs (`animated` when Twitch lists the animated
    /// format).
    pub emote: Emote,
    /// The channel it belongs to (`0` or empty for Twitch's global emotes).
    pub owner_id: String,
    /// `globals`, `subscriptions`, `bitstier`, `follower`, ... as Twitch says it.
    pub kind: String,
}

#[derive(Debug, Deserialize)]
struct Raw {
    id: String,
    name: String,
    #[serde(default)]
    emote_type: String,
    #[serde(default)]
    owner_id: String,
    #[serde(default)]
    format: Vec<String>,
}

impl Raw {
    fn into_emote(self) -> Option<UserEmote> {
        if self.id.is_empty() || self.name.is_empty() {
            return None;
        }
        let mut emote = Emote::twitch(self.id, self.name);
        emote.animated = self.format.iter().any(|format| format == "animated");
        Some(UserEmote {
            emote,
            owner_id: self.owner_id,
            kind: self.emote_type,
        })
    }
}

impl UserEmote {
    /// Whether the emote can be used in the room `room_id`. Twitch lists the follower emotes of
    /// every channel the user follows, but a follower emote only works in its own channel;
    /// subscriber emotes work everywhere.
    pub fn usable_in(&self, room_id: &str) -> bool {
        self.kind != "follower" || self.owner_id == room_id
    }
}

impl HelixClient {
    /// The emotes `user_id` can use, in Twitch's order, each once. With `broadcaster_id`, that
    /// channel's follower emotes are included. Needs [`SCOPE`]; a login without it fails with
    /// [`HelixListError::is_missing_scope`].
    pub async fn get_user_emotes(
        &self,
        user_id: &str,
        broadcaster_id: Option<&str>,
    ) -> Result<Vec<UserEmote>, HelixListError> {
        let mut path = format!("chat/emotes/user?user_id={}", encode_query_value(user_id));
        if let Some(broadcaster_id) = broadcaster_id {
            path.push_str("&broadcaster_id=");
            path.push_str(&encode_query_value(broadcaster_id));
        }
        let raw: Vec<Raw> = self.get_all(&path, MAX_PAGES).await?;
        let mut seen = HashSet::new();
        Ok(raw
            .into_iter()
            .filter_map(Raw::into_emote)
            .filter(|entry| seen.insert(entry.emote.id.clone()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_chat_core::{EmoteProvider, ImageScale};
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::{Auth, MemoryTokenStore, OAuthClient, Secret, StoredToken, unix_now};

    const CLIENT: &str = "test-client-id";

    async fn client(server: &MockServer, scopes: &[&str]) -> HelixClient {
        let store = Arc::new(MemoryTokenStore::with(StoredToken {
            access_token: Secret::new("access"),
            refresh_token: Secret::new("refresh"),
            expires_at: unix_now() + 4 * 3600,
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
            login: "viewer".to_owned(),
            user_id: "42".to_owned(),
        }));
        let oauth = OAuthClient::with_base(CLIENT, server.uri()).unwrap();
        let auth = Auth::with_scopes(oauth, store, &[SCOPE]);
        let _validate = Mock::given(method("GET"))
            .and(path("/oauth2/validate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "client_id": CLIENT, "login": "viewer", "scopes": scopes, "user_id": "42",
                "expires_in": 0,
            })))
            .mount_as_scoped(server)
            .await;
        auth.restore().await;
        HelixClient::with_base(auth, format!("{}/helix", server.uri())).unwrap()
    }

    fn emote(id: &str, name: &str, kind: &str, owner: &str, animated: bool) -> Value {
        json!({
            "id": id, "name": name, "emote_type": kind, "emote_set_id": "0", "owner_id": owner,
            "format": if animated { json!(["static", "animated"]) } else { json!(["static"]) },
            "scale": ["1.0", "2.0", "3.0"], "theme_mode": ["light", "dark"],
        })
    }

    fn page(emotes: &[Value], cursor: Option<&str>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "data": emotes,
            "template": "https://static-cdn.jtvnw.net/emoticons/v2/{{id}}/{{format}}/{{theme_mode}}/{{scale}}",
            "pagination": cursor.map_or(json!({}), |cursor| json!({ "cursor": cursor })),
        }))
    }

    #[tokio::test]
    async fn reads_every_page_with_the_user_and_the_room() {
        let server = MockServer::start().await;
        let helix = client(&server, &[SCOPE]).await;
        // Page 1 has no `after`; page 2 is asked for by the cursor page 1 gave (with characters
        // that need escaping).
        Mock::given(method("GET"))
            .and(path("/helix/chat/emotes/user"))
            .and(query_param("user_id", "42"))
            .and(query_param("broadcaster_id", "7"))
            .and(header("Client-Id", CLIENT))
            .and(header("Authorization", "Bearer access"))
            .and(wiremock::matchers::query_param_is_missing("after"))
            .respond_with(page(
                &[
                    emote("25", "Kappa", "globals", "0", false),
                    emote("303", "ChannelSub", "subscriptions", "7", true),
                ],
                Some("a+b/c="),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/helix/chat/emotes/user"))
            .and(query_param("after", "a+b/c="))
            .respond_with(page(
                &[
                    // Twice, as Twitch does for an emote in two sets.
                    emote("303", "ChannelSub", "subscriptions", "7", true),
                    emote("9", "Follower", "follower", "7", false),
                ],
                None,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let emotes = helix.get_user_emotes("42", Some("7")).await.unwrap();
        let names: Vec<_> = emotes.iter().map(|e| &*e.emote.name).collect();
        assert_eq!(names, ["Kappa", "ChannelSub", "Follower"]);
        assert_eq!(emotes[0].kind, "globals");
        assert_eq!(emotes[1].owner_id, "7");
        assert!(emotes[1].emote.animated && !emotes[0].emote.animated);
        assert_eq!(emotes[0].emote.provider, EmoteProvider::Twitch);
        assert_eq!(
            emotes[0].emote.urls.get(ImageScale::X1),
            Some("https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/1.0")
        );
    }

    #[test]
    fn a_follower_emote_is_only_usable_in_its_own_channel() {
        let entry = |kind: &str, owner: &str| UserEmote {
            emote: Emote::twitch("1", "Emote"),
            owner_id: owner.into(),
            kind: kind.into(),
        };
        assert!(entry("follower", "7").usable_in("7"));
        assert!(!entry("follower", "8").usable_in("7"));
        // Subscriber and global emotes work in every room.
        assert!(entry("subscriptions", "8").usable_in("7"));
        assert!(entry("globals", "0").usable_in("7"));
    }

    #[tokio::test]
    async fn without_a_room_no_broadcaster_is_sent() {
        let server = MockServer::start().await;
        let helix = client(&server, &[SCOPE]).await;
        Mock::given(method("GET"))
            .and(path("/helix/chat/emotes/user"))
            .and(query_param("user_id", "42"))
            .and(wiremock::matchers::query_param_is_missing("broadcaster_id"))
            .respond_with(page(&[emote("25", "Kappa", "globals", "0", false)], None))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(helix.get_user_emotes("42", None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn broken_entries_are_skipped() {
        let server = MockServer::start().await;
        let helix = client(&server, &[SCOPE]).await;
        Mock::given(method("GET"))
            .and(path("/helix/chat/emotes/user"))
            .respond_with(page(
                &[
                    emote("", "NoId", "globals", "0", false),
                    emote("1", "", "globals", "0", false),
                    emote("2", "Fine", "globals", "0", false),
                ],
                None,
            ))
            .mount(&server)
            .await;
        let emotes = helix.get_user_emotes("42", None).await.unwrap();
        assert_eq!(emotes.len(), 1);
        assert_eq!(&*emotes[0].emote.name, "Fine");
    }

    #[tokio::test]
    async fn a_login_without_the_scope_is_told_so() {
        let server = MockServer::start().await;
        let helix = client(&server, &[]).await;
        Mock::given(method("GET"))
            .and(path("/helix/chat/emotes/user"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "Unauthorized", "status": 401,
                "message": "Missing scope: user:read:emotes",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let error = helix.get_user_emotes("42", None).await.unwrap_err();
        assert!(error.is_missing_scope(), "{error}");
    }

    #[tokio::test]
    async fn another_failure_keeps_twitchs_message() {
        let server = MockServer::start().await;
        let helix = client(&server, &[SCOPE]).await;
        Mock::given(method("GET"))
            .and(path("/helix/chat/emotes/user"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "Bad Request", "status": 400, "message": "The ID in user_id is not valid",
            })))
            .mount(&server)
            .await;
        let error = helix.get_user_emotes("x", None).await.unwrap_err();
        assert!(!error.is_missing_scope());
        assert!(error.to_string().contains("not valid"), "{error}");
    }
}
