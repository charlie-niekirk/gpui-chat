//! Auth and Helix against a local mock of `id.twitch.tv` and Helix (wiremock): device flow,
//! forced refresh, the single-refresh guarantee, validation 401, logout, and redaction.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::helix::{HelixClient, HelixError};

const CLIENT: &str = "test-client-id";
const WANTED: &[&str] = &["chat:read", "user:read:chat"];

fn stored(access: &str, refresh: &str, expires_at: u64) -> StoredToken {
    StoredToken {
        access_token: Secret::new(access),
        refresh_token: Secret::new(refresh),
        expires_at,
        scopes: WANTED.iter().map(|s| (*s).to_owned()).collect(),
        login: "viewer".to_owned(),
        user_id: "42".to_owned(),
    }
}

fn far_future() -> u64 {
    unix_now() + 3600 * 4
}

fn auth_with(server: &MockServer, store: Arc<MemoryTokenStore>) -> Auth {
    let oauth = OAuthClient::with_base(CLIENT, server.uri()).unwrap();
    Auth::with_scopes(oauth, store, WANTED)
}

/// An `Auth` already holding `token` (restored without a validation round trip).
async fn logged_in(server: &MockServer, token: StoredToken) -> (Auth, Arc<MemoryTokenStore>) {
    let store = Arc::new(MemoryTokenStore::with(token));
    let auth = auth_with(server, store.clone());
    // `restore` validates; answer that with a pass for the stored token that leaves its expiry
    // alone (`expires_in: 0`), so a test can start from an expired access token.
    let mut pass = validation("viewer", WANTED);
    pass["expires_in"] = json!(0);
    let _guard = Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pass))
        .mount_as_scoped(server)
        .await;
    assert!(auth.restore().await.account().is_some());
    (auth, store)
}

fn validation(login: &str, scopes: &[&str]) -> serde_json::Value {
    json!({
        "client_id": CLIENT,
        "login": login,
        "scopes": scopes,
        "user_id": "42",
        "expires_in": 14_000,
    })
}

fn token_body(access: &str, refresh: &str) -> serde_json::Value {
    json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": 14_000,
        "scope": WANTED,
        "token_type": "bearer",
    })
}

/// A public client: the body names the client id and never carries a secret.
fn public_client(request: &Request) -> bool {
    let body = String::from_utf8_lossy(&request.body);
    body.contains(&format!("client_id={CLIENT}")) && !body.contains("client_secret")
}

fn twitch_error(status: u16, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "status": status, "message": message }))
}

async fn mount_refresh(server: &MockServer, old: &str, response: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains(format!("refresh_token={old}")))
        .and(public_client)
        .respond_with(response)
        .expect(times)
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------------------------
// Device flow
// ---------------------------------------------------------------------------------------------

fn polling() -> DevicePolling {
    DevicePolling {
        unit: Duration::from_millis(20),
        slow_down_step: 5,
    }
}

async fn mount_device_code(server: &MockServer, expires_in: u64) {
    Mock::given(method("POST"))
        .and(path("/oauth2/device"))
        .and(public_client)
        .and(body_string_contains(
            "scopes=chat%3Aread+user%3Aread%3Achat",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "secret-device-code",
            "expires_in": expires_in,
            "interval": 1,
            "user_code": "ABCDEFGH",
            "verification_uri": "https://www.twitch.tv/activate?public=true&device-code=ABCDEFGH",
        })))
        .expect(1)
        .mount(server)
        .await;
}

/// One answer of the device-code grant, used `times` times, tried in `priority` order.
async fn mount_poll(server: &MockServer, response: ResponseTemplate, times: u64, priority: u8) {
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(body_string_contains("device_code=secret-device-code"))
        .and(public_client)
        .and(body_string_contains(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
        ))
        .respond_with(response)
        .up_to_n_times(times)
        .with_priority(priority)
        .expect(times)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_flow_waits_out_pending_and_slow_down_then_stores_the_login() {
    let server = MockServer::start().await;
    mount_device_code(&server, 1800).await;
    mount_poll(&server, twitch_error(400, "authorization_pending"), 1, 1).await;
    mount_poll(&server, twitch_error(400, "slow_down"), 1, 2).await;
    mount_poll(&server, twitch_error(400, "authorization_pending"), 1, 3).await;
    mount_poll(
        &server,
        ResponseTemplate::new(200).set_body_json(token_body("first-access", "first-refresh")),
        1,
        4,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .and(header("Authorization", "OAuth first-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(validation("viewer", WANTED)))
        .expect(1)
        .mount(&server)
        .await;

    let store = Arc::new(MemoryTokenStore::new());
    let auth = auth_with(&server, store.clone());
    assert_eq!(auth.restore().await, AuthState::LoggedOut { notice: None });

    let code = auth.request_device_code().await.unwrap();
    assert_eq!(code.user_code, "ABCDEFGH");
    assert_eq!(code.interval, 1);
    let started = Instant::now();
    let account = auth.complete_device_login(&code, polling()).await.unwrap();
    // Interval 1 twice, then 1 + 5 after `slow_down`, twice: 1 + 1 + 6 + 6 units.
    assert!(
        started.elapsed() >= Duration::from_millis(20 * 14),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(account.login, "viewer");
    assert_eq!(account.user_id, "42");
    assert!(!account.needs_relogin());
    assert_eq!(auth.state(), AuthState::LoggedIn(account));

    let saved = store.get().unwrap();
    assert_eq!(saved.access_token.expose(), "first-access");
    assert_eq!(saved.refresh_token.expose(), "first-refresh");
    assert!(saved.expires_at > unix_now());
    assert_eq!(
        auth.access_token().await.unwrap().token.expose(),
        "first-access"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_after_twitch_issued_the_token_still_installs_it_consistently() {
    let server = MockServer::start().await;
    mount_device_code(&server, 1800).await;
    mount_poll(
        &server,
        ResponseTemplate::new(200).set_body_json(token_body("first-access", "first-refresh")),
        1,
        1,
    )
    .await;
    // Validation (and the install after it) is still running when the caller gives up.
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(validation("viewer", WANTED))
                .set_delay(Duration::from_millis(300)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let store = Arc::new(MemoryTokenStore::new());
    let auth = auth_with(&server, store.clone());
    auth.restore().await;
    let code = auth.request_device_code().await.unwrap();
    let flow = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.complete_device_login(&code, polling()).await })
    };
    // One 20 ms interval, then the token; abort while the validation is in flight.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    flow.abort();
    assert!(flow.await.unwrap_err().is_cancelled());

    let mut states = auth.subscribe();
    tokio::time::timeout(
        Duration::from_secs(2),
        states.wait_for(|state| state.account().is_some()),
    )
    .await
    .expect("the login is installed")
    .unwrap();
    // Keychain, memory and published state agree.
    assert_eq!(store.get().unwrap().access_token.expose(), "first-access");
    assert_eq!(
        auth.access_token().await.unwrap().token.expose(),
        "first-access"
    );
    assert_eq!(auth.state().account().unwrap().login, "viewer");
}

#[tokio::test]
async fn a_device_token_that_cannot_be_validated_is_revoked() {
    let server = MockServer::start().await;
    mount_device_code(&server, 1800).await;
    mount_poll(
        &server,
        ResponseTemplate::new(200).set_body_json(token_body("orphan-access", "orphan-refresh")),
        1,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth2/revoke"))
        .and(body_string_contains("token=orphan-access"))
        .and(public_client)
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let store = Arc::new(MemoryTokenStore::new());
    let auth = auth_with(&server, store.clone());
    let code = auth.request_device_code().await.unwrap();
    assert!(auth.complete_device_login(&code, polling()).await.is_err());
    assert!(store.get().is_none());
    // The revoke runs in the background; give it a moment before the server checks `expect(1)`.
    for _ in 0..100 {
        let revoked = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path() == "/oauth2/revoke");
        if revoked {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A store whose `load` is slow and returns a stale login.
struct SlowStore {
    stale: StoredToken,
    inner: MemoryTokenStore,
}

impl TokenStore for SlowStore {
    fn load(&self) -> Result<Option<StoredToken>, StoreError> {
        std::thread::sleep(Duration::from_millis(400));
        Ok(Some(self.stale.clone()))
    }
    fn save(&self, token: &StoredToken) -> Result<(), StoreError> {
        self.inner.save(token)
    }
    fn clear(&self) -> Result<(), StoreError> {
        self.inner.clear()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_restore_does_not_overwrite_a_login_that_finished_first() {
    let server = MockServer::start().await;
    mount_device_code(&server, 1800).await;
    mount_poll(
        &server,
        ResponseTemplate::new(200).set_body_json(token_body("new-access", "new-refresh")),
        1,
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .and(header("Authorization", "OAuth new-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(validation("newer", WANTED)))
        .mount(&server)
        .await;
    let store = Arc::new(SlowStore {
        stale: stored("stale-access", "stale-refresh", far_future()),
        inner: MemoryTokenStore::new(),
    });
    let oauth = OAuthClient::with_base(CLIENT, server.uri()).unwrap();
    let auth = Auth::with_scopes(oauth, store.clone(), WANTED);
    let restoring = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.restore().await })
    };
    let code = auth.request_device_code().await.unwrap();
    let account = auth.complete_device_login(&code, polling()).await.unwrap();
    assert_eq!(account.login, "newer");
    // The keychain read finishes only now, with the stale login: it must be ignored.
    let state = restoring.await.unwrap();
    assert_eq!(state.account().unwrap().login, "newer");
    assert_eq!(
        auth.access_token().await.unwrap().token.expose(),
        "new-access"
    );
    assert_eq!(
        store.inner.get().unwrap().access_token.expose(),
        "new-access"
    );
}

#[tokio::test]
async fn device_flow_reports_expiry_and_denial() {
    for (message, expected) in [
        ("expired_token", AuthError::DeviceCodeExpired),
        ("invalid device code", AuthError::DeviceCodeExpired),
        ("access_denied", AuthError::AccessDenied),
    ] {
        let server = MockServer::start().await;
        mount_device_code(&server, 1800).await;
        mount_poll(&server, twitch_error(400, message), 1, 1).await;
        let store = Arc::new(MemoryTokenStore::new());
        let auth = auth_with(&server, store.clone());
        let code = auth.request_device_code().await.unwrap();
        assert_eq!(
            auth.complete_device_login(&code, polling()).await,
            Err(expected),
            "{message}"
        );
        assert!(store.get().is_none());
    }
}

#[tokio::test]
async fn device_flow_gives_up_when_the_code_expires_while_pending() {
    let server = MockServer::start().await;
    // Expires after 3 units; the interval is 1 unit, so it polls twice and then gives up.
    mount_device_code(&server, 3).await;
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .respond_with(twitch_error(400, "authorization_pending"))
        .mount(&server)
        .await;
    let auth = auth_with(&server, Arc::new(MemoryTokenStore::new()));
    let code = auth.request_device_code().await.unwrap();
    assert_eq!(
        auth.complete_device_login(&code, polling()).await,
        Err(AuthError::DeviceCodeExpired)
    );
}

// ---------------------------------------------------------------------------------------------
// Refresh: forced, single owner, atomic persistence
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_during_a_forced_refresh_share_one_refresh() {
    let server = MockServer::start().await;
    // The stored access token has expired: the first `access_token` must refresh.
    let (auth, store) = logged_in(&server, stored("old-access", "old-refresh", 1)).await;
    let saves_before = store.saves();
    mount_refresh(
        &server,
        "old-refresh",
        ResponseTemplate::new(200)
            .set_body_json(token_body("new-access", "new-refresh"))
            // Long enough for every caller below to queue up behind the refresh.
            .set_delay(Duration::from_millis(300)),
        1,
    )
    .await;

    let callers: Vec<_> = (0..16)
        .map(|_| {
            let auth = auth.clone();
            tokio::spawn(async move { auth.access_token().await })
        })
        .collect();
    for caller in callers {
        let lease = caller.await.unwrap().unwrap();
        assert_eq!(lease.token.expose(), "new-access");
    }
    assert_eq!(auth.refresh_count(), 1, "exactly one refresh request");

    // Both new tokens were stored together, in one write.
    let saved = store.get().unwrap();
    assert_eq!(saved.access_token.expose(), "new-access");
    assert_eq!(saved.refresh_token.expose(), "new-refresh");
    assert_eq!(store.saves(), saves_before + 1);
    // `expect(1)` on the refresh mock is checked when the server drops.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caller_dropped_mid_refresh_does_not_lose_the_rotated_tokens() {
    let server = MockServer::start().await;
    let (auth, store) = logged_in(&server, stored("old-access", "old-refresh", 1)).await;
    mount_refresh(
        &server,
        "old-refresh",
        ResponseTemplate::new(200)
            .set_body_json(token_body("new-access", "new-refresh"))
            .set_delay(Duration::from_millis(200)),
        1,
    )
    .await;
    // Give up long before the refresh answers.
    let quick = tokio::time::timeout(Duration::from_millis(50), auth.access_token()).await;
    assert!(quick.is_err());
    // The refresh still finishes and is stored; the next caller does not refresh again.
    let lease = auth.access_token().await.unwrap();
    assert_eq!(lease.token.expose(), "new-access");
    assert_eq!(store.get().unwrap().refresh_token.expose(), "new-refresh");
    assert_eq!(auth.refresh_count(), 1);
}

#[tokio::test]
async fn a_rejected_refresh_logs_out_with_a_notice() {
    let server = MockServer::start().await;
    let (auth, store) = logged_in(&server, stored("old-access", "old-refresh", 1)).await;
    let mut states = auth.subscribe();
    mount_refresh(
        &server,
        "old-refresh",
        twitch_error(400, "Invalid refresh token"),
        1,
    )
    .await;
    assert_eq!(auth.access_token().await, Err(AuthError::SessionEnded));
    assert!(store.get().is_none(), "the store is cleared");
    assert!(states.has_changed().unwrap());
    assert_eq!(
        *states.borrow_and_update(),
        AuthState::LoggedOut {
            notice: Some(SESSION_EXPIRED_NOTICE.to_owned())
        }
    );
    assert_eq!(auth.access_token().await, Err(AuthError::NotLoggedIn));
}

#[tokio::test]
async fn a_failed_early_refresh_keeps_the_still_valid_token() {
    let server = MockServer::start().await;
    // Expires in two minutes: inside the refresh margin, but still valid.
    let (auth, store) = logged_in(
        &server,
        stored("old-access", "old-refresh", unix_now() + 120),
    )
    .await;
    mount_refresh(&server, "old-refresh", ResponseTemplate::new(503), 1).await;
    let lease = auth.access_token().await.unwrap();
    assert_eq!(lease.token.expose(), "old-access");
    assert!(auth.is_logged_in());
    assert_eq!(
        store.get().unwrap().refresh_token.expose(),
        "old-refresh",
        "a transient failure keeps the login"
    );
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn restore_validates_and_a_401_refreshes() {
    let server = MockServer::start().await;
    let store = Arc::new(MemoryTokenStore::with(stored(
        "old-access",
        "old-refresh",
        far_future(),
    )));
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .and(header("Authorization", "OAuth old-access"))
        .respond_with(twitch_error(401, "invalid access token"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .and(header("Authorization", "OAuth new-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(validation("renamed", WANTED)))
        .expect(1)
        .mount(&server)
        .await;
    mount_refresh(
        &server,
        "old-refresh",
        ResponseTemplate::new(200).set_body_json(token_body("new-access", "new-refresh")),
        1,
    )
    .await;
    let auth = auth_with(&server, store.clone());
    let state = auth.restore().await;
    let account = state.account().unwrap();
    assert_eq!(account.login, "renamed", "validation updates the login");
    let saved = store.get().unwrap();
    assert_eq!(saved.access_token.expose(), "new-access");
    assert_eq!(saved.refresh_token.expose(), "new-refresh");
    assert_eq!(saved.login, "renamed");
    assert_eq!(auth.refresh_count(), 1);
}

#[tokio::test]
async fn a_401_whose_refresh_fails_logs_out_with_a_visible_notice() {
    let server = MockServer::start().await;
    let store = Arc::new(MemoryTokenStore::with(stored(
        "old-access",
        "old-refresh",
        far_future(),
    )));
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .respond_with(twitch_error(401, "invalid access token"))
        .mount(&server)
        .await;
    mount_refresh(
        &server,
        "old-refresh",
        twitch_error(400, "Invalid refresh token"),
        1,
    )
    .await;
    let auth = auth_with(&server, store.clone());
    assert_eq!(
        auth.restore().await,
        AuthState::LoggedOut {
            notice: Some(SESSION_EXPIRED_NOTICE.to_owned())
        }
    );
    assert!(store.get().is_none());
}

#[tokio::test]
async fn hourly_validation_catches_a_revoked_token() {
    let server = MockServer::start().await;
    let (auth, store) = logged_in(&server, stored("old-access", "old-refresh", far_future())).await;
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .respond_with(twitch_error(401, "invalid access token"))
        .mount(&server)
        .await;
    mount_refresh(
        &server,
        "old-refresh",
        twitch_error(400, "Invalid refresh token"),
        1,
    )
    .await;
    let mut states = auth.subscribe();
    let validator = tokio::spawn(auth.clone().run_validation(Duration::from_millis(20)));
    states.changed().await.unwrap();
    assert!(matches!(
        *states.borrow(),
        AuthState::LoggedOut { notice: Some(_) }
    ));
    assert!(store.get().is_none());
    validator.abort();
}

#[tokio::test]
async fn an_offline_start_keeps_the_stored_login() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let store = Arc::new(MemoryTokenStore::with(stored("a", "r", far_future())));
    let auth = auth_with(&server, store.clone());
    assert!(auth.restore().await.account().is_some());
    assert!(store.get().is_some());
}

#[tokio::test]
async fn a_login_with_fewer_scopes_than_wanted_asks_to_log_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/oauth2/validate"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(validation("viewer", &["chat:read"])),
        )
        .mount(&server)
        .await;
    let mut token = stored("a", "r", far_future());
    token.scopes = vec!["chat:read".to_owned()];
    let auth = auth_with(&server, Arc::new(MemoryTokenStore::with(token)));
    let state = auth.restore().await;
    let account = state.account().unwrap();
    assert!(account.needs_relogin());
    assert_eq!(account.missing_scopes, ["user:read:chat"]);
}

struct CorruptStore;

impl TokenStore for CorruptStore {
    fn load(&self) -> Result<Option<StoredToken>, StoreError> {
        Err(StoreError::Corrupt("not json".to_owned()))
    }
    fn save(&self, _: &StoredToken) -> Result<(), StoreError> {
        Ok(())
    }
    fn clear(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[tokio::test]
async fn an_unreadable_stored_login_logs_out_with_a_notice() {
    let server = MockServer::start().await;
    let oauth = OAuthClient::with_base(CLIENT, server.uri()).unwrap();
    let auth = Auth::with_scopes(oauth, Arc::new(CorruptStore), WANTED);
    assert!(matches!(
        auth.restore().await,
        AuthState::LoggedOut { notice: Some(_) }
    ));
}

// ---------------------------------------------------------------------------------------------
// Logout
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn logout_revokes_and_clears_the_store() {
    let server = MockServer::start().await;
    let (auth, store) = logged_in(&server, stored("old-access", "old-refresh", far_future())).await;
    Mock::given(method("POST"))
        .and(path("/oauth2/revoke"))
        .and(body_string_contains(format!("client_id={CLIENT}")))
        .and(body_string_contains("token=old-access"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    auth.logout().await;
    assert_eq!(auth.state(), AuthState::LoggedOut { notice: None });
    assert!(store.get().is_none());
    assert_eq!(auth.access_token().await, Err(AuthError::NotLoggedIn));
}

// ---------------------------------------------------------------------------------------------
// Helix
// ---------------------------------------------------------------------------------------------

async fn mount_helix(server: &MockServer, bearer: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/helix/chat/badges/global"))
        .and(header("Client-Id", CLIENT))
        .and(header("Authorization", format!("Bearer {bearer}").as_str()))
        .respond_with(response)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn helix_401s_refresh_once_and_every_request_retries_with_the_new_token() {
    let server = MockServer::start().await;
    let (auth, store) = logged_in(&server, stored("old-access", "old-refresh", far_future())).await;
    mount_helix(
        &server,
        "old-access",
        twitch_error(401, "Invalid OAuth token"),
    )
    .await;
    mount_helix(
        &server,
        "new-access",
        ResponseTemplate::new(200).set_body_json(json!({ "data": [] })),
    )
    .await;
    mount_refresh(
        &server,
        "old-refresh",
        ResponseTemplate::new(200)
            .set_body_json(token_body("new-access", "new-refresh"))
            .set_delay(Duration::from_millis(200)),
        1,
    )
    .await;
    let helix = HelixClient::with_base(auth.clone(), format!("{}/helix", server.uri())).unwrap();
    let requests: Vec<_> = (0..8)
        .map(|_| {
            let helix = helix.clone();
            tokio::spawn(async move { helix.get("chat/badges/global").await })
        })
        .collect();
    for request in requests {
        let response = request.await.unwrap().unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, br#"{"data":[]}"#);
    }
    assert_eq!(auth.refresh_count(), 1);
    assert_eq!(store.get().unwrap().refresh_token.expose(), "new-refresh");
}

#[tokio::test]
async fn a_helix_401_for_a_missing_scope_does_not_refresh() {
    let server = MockServer::start().await;
    let (auth, store) = logged_in(&server, stored("old-access", "old-refresh", far_future())).await;
    mount_helix(
        &server,
        "old-access",
        twitch_error(401, "Missing scope: moderator:read:chatters"),
    )
    .await;
    // No refresh may happen.
    mount_refresh(&server, "old-refresh", ResponseTemplate::new(500), 0).await;
    let helix = HelixClient::with_base(auth.clone(), format!("{}/helix", server.uri())).unwrap();
    let response = helix.get("chat/badges/global").await.unwrap();
    assert_eq!(response.status, 401);
    assert_eq!(auth.refresh_count(), 0);
    assert_eq!(store.get().unwrap().refresh_token.expose(), "old-refresh");
    assert!(auth.is_logged_in());
}

#[tokio::test]
async fn helix_gives_up_when_the_refresh_is_rejected() {
    let server = MockServer::start().await;
    let (auth, _) = logged_in(&server, stored("old-access", "old-refresh", far_future())).await;
    mount_helix(
        &server,
        "old-access",
        twitch_error(401, "Invalid OAuth token"),
    )
    .await;
    mount_refresh(
        &server,
        "old-refresh",
        twitch_error(400, "Invalid refresh token"),
        1,
    )
    .await;
    let helix = HelixClient::with_base(auth.clone(), format!("{}/helix", server.uri())).unwrap();
    assert_eq!(
        helix.get("chat/badges/global").await,
        Err(HelixError::Auth(AuthError::SessionEnded))
    );
    assert!(!auth.is_logged_in());
}

#[tokio::test]
async fn helix_retries_a_429_after_the_reset_and_remembers_the_limit() {
    let server = MockServer::start().await;
    let (auth, _) = logged_in(&server, stored("access", "refresh", far_future())).await;
    let reset = unix_now().to_string();
    Mock::given(method("GET"))
        .and(path("/helix/chat/badges"))
        .and(query_param("broadcaster_id", "1"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Ratelimit-Limit", "800")
                .insert_header("Ratelimit-Remaining", "0")
                .insert_header("Ratelimit-Reset", reset.as_str()),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/helix/chat/badges"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Ratelimit-Limit", "800")
                .insert_header("Ratelimit-Remaining", "799")
                .set_body_json(json!({ "data": [] })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let helix = HelixClient::with_base(auth, format!("{}/helix/", server.uri())).unwrap();
    let response = helix.get("chat/badges?broadcaster_id=1").await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(helix.rate_limit().remaining, Some(799));
    assert_eq!(helix.rate_limit().limit, Some(800));
}

#[tokio::test]
async fn helix_needs_a_login() {
    let server = MockServer::start().await;
    let auth = auth_with(&server, Arc::new(MemoryTokenStore::new()));
    auth.restore().await;
    let helix = HelixClient::with_base(auth, server.uri()).unwrap();
    assert_eq!(
        helix.get("chat/badges/global").await,
        Err(HelixError::Auth(AuthError::NotLoggedIn))
    );
}

// ---------------------------------------------------------------------------------------------
// Pure parts
// ---------------------------------------------------------------------------------------------

#[test]
fn tokens_never_show_in_debug_output() {
    let token = stored("very-secret-access", "very-secret-refresh", 1);
    let response = TokenResponse {
        access_token: Secret::new("very-secret-access"),
        refresh_token: Secret::new("very-secret-refresh"),
        expires_in: 1,
        scopes: vec![],
    };
    let code = DeviceCode {
        user_code: "ABCDEFGH".to_owned(),
        verification_uri: "https://www.twitch.tv/activate".to_owned(),
        device_code: Secret::new("very-secret-device"),
        interval: 5,
        expires_in: 1800,
    };
    let lease = TokenLease {
        token: Secret::new("very-secret-access"),
        user_id: "42".to_owned(),
        login: "viewer".to_owned(),
        generation: 1,
    };
    let outputs = [
        format!("{:?}", Secret::new("very-secret-access")),
        format!("{token:?}"),
        format!("{token:#?}"),
        format!("{response:?}"),
        format!("{:?}", PollOutcome::Token(response.clone())),
        format!("{code:?}"),
        format!("{lease:?}"),
        format!("{:?}", Ok::<_, AuthError>(lease.clone())),
    ];
    for output in outputs {
        assert!(!output.contains("very-secret"), "{output}");
        assert!(output.contains("[redacted]"), "{output}");
    }
    // The user code is not a secret: the user types it.
    assert!(format!("{code:?}").contains("ABCDEFGH"));
}

#[test]
fn the_stored_record_round_trips_and_garbage_is_corrupt() {
    let token = stored("a", "r", 7);
    let encoded = encode_token(&token);
    assert_eq!(decode_token(&encoded).unwrap(), token);
    assert!(matches!(decode_token("{"), Err(StoreError::Corrupt(_))));
    // A record without the optional fields (an older build) still loads.
    let minimal = r#"{"access_token":"a","refresh_token":"r","login":"x","user_id":"1"}"#;
    let token = decode_token(minimal).unwrap();
    assert_eq!(token.expires_at, 0);
    assert!(token.scopes.is_empty());
}

#[test]
fn missing_scopes_lists_what_the_constant_adds() {
    let granted = vec!["chat:read".to_owned(), "extra".to_owned()];
    assert_eq!(
        missing_scopes(&granted, &["chat:read", "chat:edit"]),
        ["chat:edit"]
    );
    assert!(missing_scopes(&granted, &["chat:read"]).is_empty());
    assert!(SCOPES.contains(&"chat:read") && SCOPES.contains(&"chat:edit"));
}

#[test]
fn expiry_margin() {
    let token = stored("a", "r", 1000);
    assert!(token.expires_within(1000, 0));
    assert!(token.expires_within(800, 300));
    assert!(!token.expires_within(600, 300));
    let unknown = stored("a", "r", 0);
    assert!(!unknown.expires_within(u64::MAX, 300));
}

#[test]
fn the_client_id_can_be_overridden() {
    assert_eq!(client_id_from(|_| None), DEFAULT_CLIENT_ID);
    assert_eq!(client_id_from(|_| Some("  ".to_owned())), DEFAULT_CLIENT_ID);
    assert_eq!(
        client_id_from(|name| (name == CLIENT_ID_ENV).then(|| " fork-id ".to_owned())),
        "fork-id"
    );
    assert_eq!(CLIENT_ID_ENV, "GPUI_CHAT_TWITCH_CLIENT_ID");
}
