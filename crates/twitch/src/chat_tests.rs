//! Sending chat and logging in to IRC against a mock Helix / `id.twitch.tv` (wiremock): success,
//! every kind of drop, refusals, the 401 → refresh → retry path, 429, and the credentials the
//! IRC connection is given.

use std::sync::Arc;

use gpui_chat_core::Timestamp;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::auth::{Auth, AuthError, MemoryTokenStore, OAuthClient, Secret, StoredToken, unix_now};
use crate::chat_send::SendError;
use crate::client::Credentials;
use crate::helix::{HelixClient, HelixError};

const CLIENT: &str = "test-client-id";
const WANTED: &[&str] = &["chat:read", "user:write:chat"];

fn stored(access: &str, refresh: &str, scopes: &[&str]) -> StoredToken {
    StoredToken {
        access_token: Secret::new(access),
        refresh_token: Secret::new(refresh),
        expires_at: unix_now() + 3600 * 4,
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        login: "viewer".to_owned(),
        user_id: "42".to_owned(),
    }
}

/// `/oauth2/validate` as Twitch answers it for a token granted `scopes`.
async fn mount_validate(server: &MockServer, scopes: &[&str]) {
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": CLIENT, "login": "viewer", "scopes": scopes,
            "user_id": "42", "expires_in": 0,
        })))
        .mount(server)
        .await;
}

async fn logged_in_with(server: &MockServer, token: StoredToken) -> Auth {
    let scopes: Vec<&str> = token.scopes.iter().map(String::as_str).collect();
    mount_validate(server, &scopes).await;
    let store = Arc::new(MemoryTokenStore::with(token));
    let auth = Auth::with_scopes(
        OAuthClient::with_base(CLIENT, server.uri()).unwrap(),
        store,
        WANTED,
    );
    assert!(auth.restore().await.account().is_some());
    auth
}

async fn logged_in(server: &MockServer) -> (Auth, HelixClient) {
    let auth = logged_in_with(server, stored("access", "refresh", WANTED)).await;
    let helix = HelixClient::with_base(auth.clone(), format!("{}/helix", server.uri())).unwrap();
    (auth, helix)
}

fn send_body(message: &str) -> Value {
    json!({ "broadcaster_id": "1000", "sender_id": "42", "message": message })
}

fn accepted(id: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "data": [{ "message_id": id, "is_sent": true, "drop_reason": null }]
    }))
}

fn dropped(code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "data": [{
            "message_id": "", "is_sent": false,
            "drop_reason": { "code": code, "message": message }
        }]
    }))
}

fn twitch_error(status: u16, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "error": "x", "status": status, "message": message
    }))
}

async fn mount_send(server: &MockServer, bearer: &str, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .and(header("Client-Id", CLIENT))
        .and(header("Authorization", format!("Bearer {bearer}").as_str()))
        .and(header("Content-Type", "application/json"))
        .respond_with(response)
        .mount(server)
        .await;
}

// ---- sending -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_message_is_posted_as_the_logged_in_user_and_its_id_returned() {
    let server = MockServer::start().await;
    let (_, helix) = logged_in(&server).await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .and(header("Authorization", "Bearer access"))
        .and(body_json(send_body("hello chat")))
        .respond_with(accepted("msg-1"))
        .expect(1)
        .mount(&server)
        .await;
    let sent = helix
        .send_chat_message("1000", "hello chat", None)
        .await
        .unwrap();
    assert_eq!(sent.message_id, "msg-1");
    // Who it was sent as comes from the token that made the request.
    assert_eq!(
        (sent.user_id.as_str(), sent.login.as_str()),
        ("42", "viewer")
    );
}

#[tokio::test]
async fn an_action_goes_out_as_me_text_and_a_reply_names_its_parent() {
    let server = MockServer::start().await;
    let (_, helix) = logged_in(&server).await;
    let mut reply = send_body("/me waves");
    reply["reply_parent_message_id"] = json!("parent-9");
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .and(body_json(reply))
        .respond_with(accepted("msg-2"))
        .expect(1)
        .mount(&server)
        .await;
    helix
        .send_chat_message("1000", "/me waves", Some("parent-9"))
        .await
        .unwrap();
}

#[tokio::test]
async fn every_drop_reason_comes_back_with_twitchs_words() {
    let cases = [
        (
            "msg_duplicate",
            "Your message was not sent because it is identical to the previous one you sent, less than 30 seconds ago.",
        ),
        (
            "msg_ratelimit",
            "Your message was not sent because you are sending messages too quickly.",
        ),
        (
            "channel_settings",
            "Your message was not sent because it is not allowed by the channel's chat settings.",
        ),
        (
            "msg_rejected",
            "Your message wasn't posted due to conflicts with the channel's moderation settings.",
        ),
        (
            "automod_held",
            "Your message is being checked by mods and has not been sent.",
        ),
    ];
    for (code, message) in cases {
        let server = MockServer::start().await;
        let (_, helix) = logged_in(&server).await;
        mount_send(&server, "access", dropped(code, message)).await;
        let error = helix
            .send_chat_message("1000", "hi", None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            SendError::Dropped {
                code: code.into(),
                message: message.into()
            }
        );
        assert_eq!(error.to_string(), message, "the pane shows Twitch's text");
    }
}

#[tokio::test]
async fn refusals_come_back_as_rejections_with_twitchs_message() {
    for (status, message) in [
        (400, "The message is missing"),
        (403, "The user is banned from this channel."),
        (422, "The message is too large."),
    ] {
        let server = MockServer::start().await;
        let (auth, helix) = logged_in(&server).await;
        mount_send(&server, "access", twitch_error(status, message)).await;
        assert_eq!(
            helix.send_chat_message("1000", "hi", None).await,
            Err(SendError::Rejected {
                status,
                message: message.into()
            })
        );
        assert_eq!(auth.refresh_count(), 0, "a refusal is not a bad token");
    }
}

#[tokio::test]
async fn an_invalid_token_is_refreshed_and_the_message_sent_once_with_the_new_one() {
    let server = MockServer::start().await;
    let (auth, helix) = logged_in(&server).await;
    mount_send(&server, "access", twitch_error(401, "Invalid OAuth token")).await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .and(header("Authorization", "Bearer fresh-access"))
        .and(body_json(send_body("after refresh")))
        .respond_with(accepted("msg-3"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "fresh-access", "refresh_token": "fresh-refresh",
            "expires_in": 14_000, "scope": WANTED, "token_type": "bearer",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let sent = helix
        .send_chat_message("1000", "after refresh", None)
        .await
        .unwrap();
    assert_eq!(sent.message_id, "msg-3");
    assert_eq!(auth.refresh_count(), 1);
}

#[tokio::test]
async fn a_401_for_a_missing_scope_asks_for_a_new_login_without_refreshing() {
    let server = MockServer::start().await;
    let (auth, helix) = logged_in(&server).await;
    mount_send(
        &server,
        "access",
        twitch_error(401, "Missing scope: user:write:chat"),
    )
    .await;
    let error = helix
        .send_chat_message("1000", "hi", None)
        .await
        .unwrap_err();
    assert!(matches!(error, SendError::Rejected { status: 401, .. }));
    assert!(error.to_string().contains("Log in again"));
    assert_eq!(auth.refresh_count(), 0);
    assert!(auth.is_logged_in());
}

#[tokio::test]
async fn a_429_is_retried_after_the_reset_then_reported() {
    let server = MockServer::start().await;
    let (_, helix) = logged_in(&server).await;
    let reset = unix_now().to_string();
    let limited = || {
        ResponseTemplate::new(429)
            .insert_header("Ratelimit-Limit", "800")
            .insert_header("Ratelimit-Remaining", "0")
            .insert_header("Ratelimit-Reset", reset.as_str())
    };
    // Limited once, then fine.
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .respond_with(limited())
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/helix/chat/messages"))
        .respond_with(accepted("msg-4"))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        helix
            .send_chat_message("1000", "hi", None)
            .await
            .unwrap()
            .message_id,
        "msg-4"
    );

    // Limited every time: the error says so.
    let server = MockServer::start().await;
    let (_, helix) = logged_in(&server).await;
    mount_send(&server, "access", limited()).await;
    assert_eq!(
        helix.send_chat_message("1000", "hi", None).await,
        Err(SendError::RateLimited)
    );
}

#[tokio::test]
async fn sending_needs_a_login() {
    let server = MockServer::start().await;
    let auth = Auth::with_scopes(
        OAuthClient::with_base(CLIENT, server.uri()).unwrap(),
        Arc::new(MemoryTokenStore::new()),
        WANTED,
    );
    auth.restore().await;
    let helix = HelixClient::with_base(auth, server.uri()).unwrap();
    assert_eq!(
        helix.send_chat_message("1000", "hi", None).await,
        Err(SendError::Helix(HelixError::Auth(AuthError::NotLoggedIn)))
    );
}

#[tokio::test]
async fn an_unreachable_helix_is_a_network_error() {
    let server = MockServer::start().await;
    let (auth, _) = logged_in(&server).await;
    // Nothing listens on port 1.
    let helix = HelixClient::with_base(auth, "http://127.0.0.1:1/helix").unwrap();
    let result = helix.send_chat_message("1000", "hi", None).await;
    assert!(
        matches!(result, Err(SendError::Helix(HelixError::Network(_)))),
        "{result:?}"
    );
}

#[test]
fn the_echo_helper_is_exported_for_the_app() {
    let author = gpui_chat_core::Author::new("viewer", "viewer");
    let message = crate::echo_message("m", author, "hi", false, Timestamp(1));
    assert_eq!(message.id.as_str(), "m");
}

// ---- IRC credentials ---------------------------------------------------------------------------

#[tokio::test]
async fn a_logged_in_user_connects_with_their_login_and_token() {
    let server = MockServer::start().await;
    let (auth, _) = logged_in(&server).await;
    let credentials = Credentials::from_auth(&auth, 0).await;
    assert!(!credentials.is_anonymous());
    assert_eq!(credentials.login, "viewer");
    assert_eq!(credentials.token.as_deref(), Some("access"));
}

#[tokio::test]
async fn logged_out_connects_anonymously() {
    let server = MockServer::start().await;
    let auth = Auth::with_scopes(
        OAuthClient::with_base(CLIENT, server.uri()).unwrap(),
        Arc::new(MemoryTokenStore::new()),
        WANTED,
    );
    auth.restore().await;
    assert!(Credentials::from_auth(&auth, 0).await.is_anonymous());
}

#[tokio::test]
async fn a_login_without_chat_read_stays_anonymous() {
    let server = MockServer::start().await;
    // Granted only what the older build asked for.
    let auth = logged_in_with(&server, stored("access", "refresh", &["user:write:chat"])).await;
    let account = auth.state().account().cloned().unwrap();
    assert!(account.needs_relogin());
    assert!(Credentials::from_auth(&auth, 0).await.is_anonymous());
}

#[tokio::test]
async fn an_expired_access_token_is_refreshed_before_it_is_handed_to_irc() {
    let server = MockServer::start().await;
    let mut token = stored("stale", "refresh", WANTED);
    token.expires_at = unix_now() + 10; // inside the refresh margin
    let auth = logged_in_with(&server, token).await;
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "fresh", "refresh_token": "fresh-refresh",
            "expires_in": 14_000, "scope": WANTED, "token_type": "bearer",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let credentials = Credentials::from_auth(&auth, 0).await;
    assert_eq!(credentials.token.as_deref(), Some("fresh"));
}

#[tokio::test(start_paused = true)]
async fn the_first_connection_waits_for_the_stored_login_to_be_read() {
    let server = MockServer::start().await;
    mount_validate(&server, WANTED).await;
    let store = Arc::new(MemoryTokenStore::with(stored("access", "refresh", WANTED)));
    let auth = Auth::with_scopes(
        OAuthClient::with_base(CLIENT, server.uri()).unwrap(),
        store,
        WANTED,
    );
    // `restore` has not run yet: the state is still `Restoring`.
    let waiting = tokio::spawn({
        let auth = auth.clone();
        async move { Credentials::from_auth(&auth, 0).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "still waiting for the restore");
    auth.restore().await;
    let credentials = waiting.await.unwrap();
    assert_eq!(credentials.login, "viewer");
    assert!(!credentials.is_anonymous());
}

#[tokio::test]
async fn a_token_twitch_refused_is_refreshed_once_and_then_given_up_on() {
    let server = MockServer::start().await;
    // The token looks valid locally (expires in hours) but Twitch revoked it.
    let auth = logged_in_with(&server, stored("revoked", "refresh", WANTED)).await;
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "renewed", "refresh_token": "renewed-refresh",
            "expires_in": 14_000, "scope": WANTED, "token_type": "bearer",
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Refused once: the refresh happens although the token had not expired.
    let credentials = Credentials::from_auth(&auth, 1).await;
    assert_eq!(credentials.token.as_deref(), Some("renewed"));
    assert_eq!(auth.refresh_count(), 1);
    // Refused again: no further refresh, the connection stays anonymous.
    assert!(Credentials::from_auth(&auth, 2).await.is_anonymous());
    assert_eq!(auth.refresh_count(), 1);
}

#[tokio::test]
async fn a_rejected_refresh_after_a_refused_token_ends_the_login_and_goes_anonymous() {
    let server = MockServer::start().await;
    let auth = logged_in_with(&server, stored("revoked", "refresh", WANTED)).await;
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "status": 400, "message": "Invalid refresh token"
        })))
        .mount(&server)
        .await;
    assert!(Credentials::from_auth(&auth, 1).await.is_anonymous());
    assert!(
        !auth.is_logged_in(),
        "Twitch rejected the refresh: logged out"
    );
}
