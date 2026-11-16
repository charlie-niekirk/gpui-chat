//! The moderation requests: what each command sends (pure), and the whole path against a mock
//! Helix (wiremock) including the scope check, the user lookup and Twitch's refusals.

use std::sync::Arc;

use gpui_chat_core::{ModCommand, RoomChange};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::auth::{
    Auth, MODERATION_SCOPES, MemoryTokenStore, OAuthClient, SCOPES, Secret, StoredToken, unix_now,
};

const CLIENT: &str = "test-client-id";

fn ids<'a>(target: Option<&'a str>) -> Ids<'a> {
    Ids {
        broadcaster: "1000",
        moderator: "42",
        target,
    }
}

fn user(login: &str) -> String {
    login.to_owned()
}

fn request(command: &ModCommand, target: Option<&str>) -> ModRequest {
    build_request(command, ids(target)).unwrap()
}

#[test]
fn a_timeout_posts_the_duration_and_reason_to_bans() {
    let command = ModCommand::Timeout {
        user: user("bob"),
        seconds: 600,
        reason: Some("spam".into()),
    };
    assert_eq!(
        request(&command, Some("7")),
        ModRequest {
            method: Method::POST,
            path: "moderation/bans?broadcaster_id=1000&moderator_id=42".into(),
            body: Some(json!({ "data": { "user_id": "7", "duration": 600, "reason": "spam" } })),
        }
    );
}

#[test]
fn an_empty_reason_is_left_out_of_the_body() {
    let ban = |reason: Option<&str>| {
        request(
            &ModCommand::Ban {
                user: user("bob"),
                reason: reason.map(str::to_owned),
            },
            Some("7"),
        )
        .body
        .unwrap()
    };
    assert_eq!(ban(Some("")), json!({ "data": { "user_id": "7" } }));
    assert_eq!(ban(None), json!({ "data": { "user_id": "7" } }));
    assert_eq!(
        ban(Some("spam")),
        json!({ "data": { "user_id": "7", "reason": "spam" } })
    );
}

#[test]
fn a_403_says_the_user_may_not_moderate_here() {
    let forbidden = ModError::Rejected {
        status: 403,
        message: "x".into(),
    };
    assert!(forbidden.is_forbidden());
    assert!(
        !ModError::Rejected {
            status: 400,
            message: "x".into()
        }
        .is_forbidden()
    );
    assert!(!ModError::RateLimited.is_forbidden());
}

#[test]
fn a_ban_has_no_duration() {
    let command = ModCommand::Ban {
        user: user("bob"),
        reason: None,
    };
    assert_eq!(
        request(&command, Some("7")).body,
        Some(json!({ "data": { "user_id": "7" } }))
    );
}

#[test]
fn unban_and_the_deletes_are_delete_requests_with_a_query() {
    let unban = request(&ModCommand::Unban { user: user("bob") }, Some("7"));
    assert_eq!(unban.method, Method::DELETE);
    assert_eq!(
        unban.path,
        "moderation/bans?broadcaster_id=1000&moderator_id=42&user_id=7"
    );
    assert_eq!(unban.body, None);
    let delete = request(
        &ModCommand::DeleteMessage {
            message_id: "abc-123".into(),
        },
        None,
    );
    assert_eq!(delete.method, Method::DELETE);
    assert_eq!(
        delete.path,
        "moderation/chat?broadcaster_id=1000&moderator_id=42&message_id=abc-123"
    );
    // A message id is never able to add parameters of its own.
    let sneaky = request(
        &ModCommand::DeleteMessage {
            message_id: "a&moderator_id=1".into(),
        },
        None,
    );
    assert!(
        sneaky.path.ends_with("message_id=a%26moderator_id%3D1"),
        "{}",
        sneaky.path
    );
    let clear = request(&ModCommand::ClearChat, None);
    assert_eq!(clear.method, Method::DELETE);
    assert_eq!(
        clear.path,
        "moderation/chat?broadcaster_id=1000&moderator_id=42"
    );
}

#[test]
fn room_modes_patch_only_the_changed_setting() {
    let body = |change| {
        let request = request(&ModCommand::Room(change), None);
        assert_eq!(request.method, Method::PATCH);
        assert_eq!(
            request.path,
            "chat/settings?broadcaster_id=1000&moderator_id=42"
        );
        request.body.unwrap()
    };
    assert_eq!(
        body(RoomChange::Slow(Some(30))),
        json!({ "slow_mode": true, "slow_mode_wait_time": 30 })
    );
    assert_eq!(body(RoomChange::Slow(None)), json!({ "slow_mode": false }));
    assert_eq!(
        body(RoomChange::Followers(Some(1440))),
        json!({ "follower_mode": true, "follower_mode_duration": 1440 })
    );
    assert_eq!(
        body(RoomChange::Followers(Some(0))),
        json!({ "follower_mode": true, "follower_mode_duration": 0 })
    );
    assert_eq!(
        body(RoomChange::Followers(None)),
        json!({ "follower_mode": false })
    );
    assert_eq!(
        body(RoomChange::Subscribers(true)),
        json!({ "subscriber_mode": true })
    );
    assert_eq!(
        body(RoomChange::EmoteOnly(false)),
        json!({ "emote_mode": false })
    );
    assert_eq!(
        body(RoomChange::Unique(true)),
        json!({ "unique_chat_mode": true })
    );
}

#[test]
fn people_and_channel_commands_use_their_own_endpoints() {
    let case = |command: ModCommand, method: Method, path: &str| {
        let built = request(&command, Some("7"));
        assert_eq!(built.method, method, "{command:?}");
        assert_eq!(built.path, path, "{command:?}");
        built
    };
    let u = || user("bob");
    case(
        ModCommand::AddMod { user: u() },
        Method::POST,
        "moderation/moderators?broadcaster_id=1000&user_id=7",
    );
    case(
        ModCommand::RemoveMod { user: u() },
        Method::DELETE,
        "moderation/moderators?broadcaster_id=1000&user_id=7",
    );
    case(
        ModCommand::AddVip { user: u() },
        Method::POST,
        "channels/vips?broadcaster_id=1000&user_id=7",
    );
    case(
        ModCommand::RemoveVip { user: u() },
        Method::DELETE,
        "channels/vips?broadcaster_id=1000&user_id=7",
    );
    case(
        ModCommand::Raid { user: u() },
        Method::POST,
        "raids?from_broadcaster_id=1000&to_broadcaster_id=7",
    );
    case(
        ModCommand::Unraid,
        Method::DELETE,
        "raids?broadcaster_id=1000",
    );
    case(
        ModCommand::Shoutout { user: u() },
        Method::POST,
        "chat/shoutouts?from_broadcaster_id=1000&to_broadcaster_id=7&moderator_id=42",
    );
    let announce = case(
        ModCommand::Announce {
            text: "hello & welcome".into(),
        },
        Method::POST,
        "chat/announcements?broadcaster_id=1000&moderator_id=42",
    );
    assert_eq!(
        announce.body,
        Some(json!({ "message": "hello & welcome", "color": "primary" }))
    );
}

#[test]
fn a_command_about_a_user_needs_the_users_id() {
    let command = ModCommand::Ban {
        user: user("ghost"),
        reason: None,
    };
    assert_eq!(
        build_request(&command, ids(None)),
        Err(ModError::UnknownUser("ghost".into()))
    );
}

#[test]
fn every_command_names_a_scope_the_login_asks_for() {
    let u = || user("bob");
    let commands = [
        ModCommand::Timeout {
            user: u(),
            seconds: 60,
            reason: None,
        },
        ModCommand::Ban {
            user: u(),
            reason: None,
        },
        ModCommand::Unban { user: u() },
        ModCommand::DeleteMessage {
            message_id: "m".into(),
        },
        ModCommand::ClearChat,
        ModCommand::Room(RoomChange::EmoteOnly(true)),
        ModCommand::AddMod { user: u() },
        ModCommand::RemoveMod { user: u() },
        ModCommand::AddVip { user: u() },
        ModCommand::RemoveVip { user: u() },
        ModCommand::Announce { text: "x".into() },
        ModCommand::Raid { user: u() },
        ModCommand::Unraid,
        ModCommand::Shoutout { user: u() },
    ];
    let mut seen = std::collections::BTreeSet::new();
    for command in &commands {
        let scope = required_scope(command);
        assert!(MODERATION_SCOPES.contains(&scope), "{command:?}: {scope}");
        seen.insert(scope);
    }
    assert_eq!(
        seen.len(),
        MODERATION_SCOPES.len(),
        "every scope is used by a command"
    );
}

#[test]
fn twitchs_answers_become_sentences() {
    let response = |status, message: &str| HelixResponse {
        status,
        body: serde_json::to_vec(&json!({ "status": status, "message": message })).unwrap(),
    };
    assert_eq!(interpret(&response(204, "")), Ok(()));
    assert_eq!(interpret(&response(200, "")), Ok(()));
    assert_eq!(
        interpret(&response(
            400,
            "The user specified in the user_id field may not be banned."
        )),
        Err(ModError::Rejected {
            status: 400,
            message: "The user specified in the user_id field may not be banned.".into()
        })
    );
    let error = interpret(&response(
        401,
        "Missing scope: moderator:manage:banned_users",
    ))
    .unwrap_err();
    assert_eq!(
        error,
        ModError::MissingScope {
            scope: "moderator:manage:banned_users".into()
        }
    );
    assert_eq!(
        error.to_string(),
        "Log in again to enable moderation (grants moderator:manage:banned_users): account menu → Enable moderation tools."
    );
    assert!(
        interpret(&response(401, "Invalid OAuth token"))
            .unwrap_err()
            .to_string()
            .contains("Log in again")
    );
    assert_eq!(interpret(&response(429, "")), Err(ModError::RateLimited));
    let empty = HelixResponse {
        status: 403,
        body: Vec::new(),
    };
    assert_eq!(
        interpret(&empty).unwrap_err().to_string(),
        "You are not allowed to do that in this channel."
    );
    let teapot = HelixResponse {
        status: 503,
        body: b"<html>".to_vec(),
    };
    assert_eq!(
        interpret(&teapot).unwrap_err().to_string(),
        "Twitch answered 503."
    );
    assert_eq!(
        ModError::Helix(HelixError::Auth(AuthError::NotLoggedIn)).to_string(),
        "You are not logged in. Log in to moderate."
    );
}

// ---- against a mock Helix --------------------------------------------------------------------

fn stored(scopes: &[&str]) -> StoredToken {
    StoredToken {
        access_token: Secret::new("access"),
        refresh_token: Secret::new("refresh"),
        expires_at: unix_now() + 3600 * 4,
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        login: "viewer".to_owned(),
        user_id: "42".to_owned(),
    }
}

/// Every scope: the login of a user who enabled moderation.
fn everything() -> Vec<&'static str> {
    SCOPES.iter().chain(MODERATION_SCOPES).copied().collect()
}

/// A logged-in client whose token was granted `granted`.
async fn client(server: &MockServer, granted: &[&str]) -> HelixClient {
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": CLIENT, "login": "viewer", "scopes": granted,
            "user_id": "42", "expires_in": 0,
        })))
        .mount(server)
        .await;
    let auth = Auth::with_scopes(
        OAuthClient::with_base(CLIENT, server.uri()).unwrap(),
        Arc::new(MemoryTokenStore::with(stored(granted))),
        SCOPES,
    );
    assert!(auth.restore().await.account().is_some());
    HelixClient::with_base(auth, format!("{}/helix", server.uri())).unwrap()
}

#[tokio::test]
async fn a_ban_by_login_looks_the_user_up_then_bans_as_the_moderator() {
    let server = MockServer::start().await;
    let helix = client(&server, &everything()).await;
    Mock::given(method("GET"))
        .and(path("/helix/users"))
        .and(query_param("login", "bob"))
        .and(header("Authorization", "Bearer access"))
        .and(header("Client-Id", CLIENT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "id": "7", "login": "bob" }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/helix/moderation/bans"))
        .and(query_param("broadcaster_id", "1000"))
        .and(query_param("moderator_id", "42"))
        .and(header("Authorization", "Bearer access"))
        .and(body_json(
            json!({ "data": { "user_id": "7", "duration": 600 } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [{}] })))
        .expect(1)
        .mount(&server)
        .await;
    let command = ModCommand::Timeout {
        user: "bob".into(),
        seconds: 600,
        reason: None,
    };
    assert_eq!(helix.moderate("1000", &command, None).await, Ok(()));
}

#[tokio::test]
async fn a_known_user_id_skips_the_lookup() {
    let server = MockServer::start().await;
    let helix = client(&server, &everything()).await;
    Mock::given(method("GET"))
        .and(path("/helix/users"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/helix/moderation/bans"))
        .and(query_param("user_id", "7"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let command = ModCommand::Unban { user: "bob".into() };
    assert_eq!(helix.moderate("1000", &command, Some("7")).await, Ok(()));
}

#[tokio::test]
async fn an_unknown_login_is_reported_without_sending_anything() {
    let server = MockServer::start().await;
    let helix = client(&server, &everything()).await;
    Mock::given(method("GET"))
        .and(path("/helix/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let command = ModCommand::Ban {
        user: "ghost".into(),
        reason: None,
    };
    let error = helix.moderate("1000", &command, None).await.unwrap_err();
    assert_eq!(error, ModError::UnknownUser("ghost".into()));
    assert_eq!(error.to_string(), "No Twitch user is called ghost.");
}

#[tokio::test]
async fn a_login_without_the_scope_is_told_to_log_in_again_and_nothing_is_sent() {
    let server = MockServer::start().await;
    // A normal login: moderation was never enabled.
    let old: Vec<&str> = SCOPES.to_vec();
    let helix = client(&server, &old).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let command = ModCommand::DeleteMessage {
        message_id: "m1".into(),
    };
    let error = helix.moderate("1000", &command, None).await.unwrap_err();
    assert_eq!(
        error,
        ModError::MissingScope {
            scope: "moderator:manage:chat_messages".into()
        }
    );
    assert!(
        error.to_string().starts_with(
            "Log in again to enable moderation (grants moderator:manage:chat_messages)"
        )
    );
}

#[tokio::test]
async fn a_scope_the_login_has_but_twitch_does_not_accept_is_still_a_clear_message() {
    let server = MockServer::start().await;
    let helix = client(&server, &everything()).await;
    Mock::given(method("PATCH"))
        .and(path("/helix/chat/settings"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "status": 401, "message": "Missing scope: moderator:manage:chat_settings"
        })))
        .mount(&server)
        .await;
    let command = ModCommand::Room(RoomChange::EmoteOnly(true));
    assert_eq!(
        helix.moderate("1000", &command, None).await,
        Err(ModError::MissingScope {
            scope: "moderator:manage:chat_settings".into()
        })
    );
}

#[tokio::test]
async fn a_room_mode_is_patched_with_its_body_and_a_refusal_carries_twitchs_text() {
    let server = MockServer::start().await;
    let helix = client(&server, &everything()).await;
    Mock::given(method("PATCH"))
        .and(path("/helix/chat/settings"))
        .and(query_param("broadcaster_id", "1000"))
        .and(query_param("moderator_id", "42"))
        .and(body_json(
            json!({ "slow_mode": true, "slow_mode_wait_time": 30 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [{}] })))
        .expect(1)
        .mount(&server)
        .await;
    let slow = ModCommand::Room(RoomChange::Slow(Some(30)));
    assert_eq!(helix.moderate("1000", &slow, None).await, Ok(()));

    Mock::given(method("DELETE"))
        .and(path("/helix/moderation/chat"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "status": 403,
            "message": "The user in moderator_id is not a moderator in the broadcaster's chat room."
        })))
        .mount(&server)
        .await;
    let error = helix
        .moderate("1000", &ModCommand::ClearChat, None)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "The user in moderator_id is not a moderator in the broadcaster's chat room."
    );
}

#[tokio::test]
async fn nothing_is_sent_without_a_login() {
    let server = MockServer::start().await;
    let auth = Auth::with_scopes(
        OAuthClient::with_base(CLIENT, server.uri()).unwrap(),
        Arc::new(MemoryTokenStore::default()),
        SCOPES,
    );
    let helix = HelixClient::with_base(auth, format!("{}/helix", server.uri())).unwrap();
    let error = helix
        .moderate("1000", &ModCommand::ClearChat, None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, ModError::Helix(HelixError::Auth(_))),
        "{error:?}"
    );
    assert!(error.to_string().contains("Log in"));
}
