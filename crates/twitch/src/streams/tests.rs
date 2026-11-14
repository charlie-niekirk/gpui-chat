//! Stream state against a local mock Helix (wiremock): batching past 100 logins, cursor paging,
//! the offline → live transition through the poller, a 401 that refreshes, backoff and the
//! logged-out and missing-scope cases.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::auth::{Auth, MemoryTokenStore, OAuthClient, Secret, StoredToken, unix_now};

const CLIENT: &str = "test-client-id";
const FOLLOWS: &str = "user:read:follows";

fn token(access: &str, refresh: &str, scopes: &[&str]) -> StoredToken {
    StoredToken {
        access_token: Secret::new(access),
        refresh_token: Secret::new(refresh),
        expires_at: unix_now() + 4 * 3600,
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        login: "viewer".to_owned(),
        user_id: "42".to_owned(),
    }
}

/// A logged-in `Auth` (wanting [`FOLLOWS`]) and a `HelixClient` on `server`.
async fn client(server: &MockServer, stored: Option<StoredToken>) -> HelixClient {
    let scopes = stored
        .as_ref()
        .map(|t| t.scopes.clone())
        .unwrap_or_default();
    let store = Arc::new(match stored {
        Some(token) => MemoryTokenStore::with(token),
        None => MemoryTokenStore::default(),
    });
    let oauth = OAuthClient::with_base(CLIENT, server.uri()).unwrap();
    let auth = Auth::with_scopes(oauth, store, &[FOLLOWS]);
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

fn raw(login: &str, viewers: u64) -> Value {
    json!({
        "id": format!("s-{login}"), "user_id": format!("id-{login}"), "user_login": login,
        "user_name": login.to_uppercase(), "game_id": "1", "game_name": "Just Chatting",
        "type": "live", "title": format!("  {login} is live "), "viewer_count": viewers,
        "started_at": "2021-03-10T15:04:05Z",
    })
}

fn data(streams: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "data": streams, "pagination": {} }))
}

fn logins_of(request: &Request) -> Vec<String> {
    request
        .url
        .query_pairs()
        .filter(|(key, _)| key == "user_login")
        .map(|(_, value)| value.into_owned())
        .collect()
}

fn channels(logins: &[&str]) -> Vec<ChannelId> {
    logins.iter().map(|login| ChannelId::new(login)).collect()
}

fn fast() -> PollConfig {
    PollConfig {
        interval: Duration::from_millis(40),
        max_backoff: Duration::from_millis(160),
        settle: Duration::from_millis(10),
    }
}

/// Waits for a snapshot satisfying `done` (the receiver's current value counts).
async fn wait_for(
    rx: &mut watch::Receiver<StreamSnapshot>,
    done: impl Fn(&StreamSnapshot) -> bool,
) -> StreamSnapshot {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            {
                let current = rx.borrow_and_update();
                if done(&current) {
                    return current.clone();
                }
            }
            rx.changed().await.expect("poller ended");
        }
    })
    .await
    .expect("timed out waiting for a snapshot")
}

#[test]
fn rfc3339_to_unix_seconds() {
    assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339("2021-03-10T15:04:05Z"), Some(1_615_388_645));
    // A leap day and a fractional second.
    assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
    assert_eq!(parse_rfc3339("2024-03-01T00:00:00.5Z"), Some(1_709_251_200));
    for bad in [
        "",
        "2021-03-10",
        "2021-13-10T00:00:00Z",
        "2021-03-10T25:00:00Z",
        "nonsense",
    ] {
        assert_eq!(parse_rfc3339(bad), None, "{bad}");
    }
}

#[test]
fn backoff_doubles_up_to_the_cap() {
    let config = PollConfig::default();
    assert_eq!(config.delay(0), Duration::from_secs(60));
    assert_eq!(config.delay(1), Duration::from_secs(120));
    assert_eq!(config.delay(2), Duration::from_secs(240));
    assert_eq!(config.delay(3), Duration::from_secs(300));
    assert_eq!(config.delay(40), Duration::from_secs(300));
}

#[test]
fn uptime_needs_a_known_start() {
    let mut info = RawStream {
        user_id: "1".into(),
        user_login: "A".into(),
        user_name: String::new(),
        game_name: String::new(),
        r#type: "live".into(),
        title: " t ".into(),
        viewer_count: 3,
        started_at: "1970-01-01T00:01:40Z".into(),
    }
    .into_info()
    .unwrap();
    assert_eq!(info.channel, ChannelId::new("a"));
    assert_eq!(info.display_name, "a");
    assert_eq!(info.title, "t");
    assert_eq!(info.uptime(160), Duration::from_secs(60));
    assert_eq!(info.uptime(10), Duration::ZERO);
    info.started_at = 0;
    assert_eq!(info.uptime(160), Duration::ZERO);
}

#[tokio::test]
async fn get_streams_sends_100_logins_per_request() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("tok", "ref", &[FOLLOWS]))).await;
    // Every request answers with its first login live.
    Mock::given(method("GET"))
        .and(path("/helix/streams"))
        .respond_with(|request: &Request| data(&[raw(&logins_of(request)[0], 5)]))
        .mount(&server)
        .await;

    let all: Vec<String> = (0..250).map(|i| format!("chan{i}")).collect();
    let mut wanted: Vec<ChannelId> = all.iter().map(|login| ChannelId::new(login)).collect();
    wanted.push(ChannelId::new("CHAN3")); // a duplicate once lower-cased
    let live = helix.get_streams(&wanted).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    let sent: Vec<usize> = requests
        .iter()
        .filter(|r| r.url.path() == "/helix/streams")
        .map(|r| logins_of(r).len())
        .collect();
    assert_eq!(sent, [100, 100, 50]);
    let mut seen: Vec<String> = requests
        .iter()
        .filter(|r| r.url.path() == "/helix/streams")
        .flat_map(logins_of)
        .collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 250, "every login sent once");
    let mut live: Vec<&str> = live.iter().map(|s| s.channel.login()).collect();
    live.sort_unstable();
    assert_eq!(live, ["chan0", "chan100", "chan200"]);
}

#[tokio::test]
async fn get_streams_with_nothing_to_ask_makes_no_request() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("tok", "ref", &[FOLLOWS]))).await;
    assert!(helix.get_streams(&[]).await.unwrap().is_empty());
    let streams_calls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().contains("streams"))
        .count();
    assert_eq!(streams_calls, 0);
}

#[tokio::test]
async fn followed_streams_follow_the_cursor() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("tok", "ref", &[FOLLOWS]))).await;
    // The more specific mock first: wiremock answers with the first one that matches.
    Mock::given(method("GET"))
        .and(path("/helix/streams/followed"))
        .and(query_param("after", "cur+1=="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [raw("c", 30)], "pagination": { "cursor": "cur2" }
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/helix/streams/followed"))
        .and(query_param("after", "cur2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [raw("d", 5)], "pagination": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/helix/streams/followed"))
        .and(query_param("user_id", "42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [raw("a", 100), raw("b", 50)], "pagination": { "cursor": "cur+1==" }
        })))
        .mount(&server)
        .await;

    let followed = helix.get_followed_streams("42").await.unwrap();
    let names: Vec<&str> = followed.iter().map(|s| s.channel.login()).collect();
    assert_eq!(names, ["a", "b", "c", "d"]);
    assert_eq!(followed[0].viewers, 100);
    assert_eq!(followed[0].category, "Just Chatting");
}

#[tokio::test]
async fn a_repeating_cursor_ends_the_walk() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("tok", "ref", &[FOLLOWS]))).await;
    Mock::given(method("GET"))
        .and(path("/helix/streams/followed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [raw("a", 1)], "pagination": { "cursor": "same" }
        })))
        .mount(&server)
        .await;
    let followed = helix.get_followed_streams("42").await.unwrap();
    // First page, then the page for "same", which repeats it.
    assert_eq!(followed.len(), 2);
}

#[tokio::test]
async fn reruns_and_errors_are_not_live() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("tok", "ref", &[FOLLOWS]))).await;
    let mut rerun = raw("rerun", 1);
    rerun["type"] = json!("");
    Mock::given(method("GET"))
        .and(path("/helix/streams"))
        .respond_with(data(&[raw("on", 1), rerun]))
        .mount(&server)
        .await;
    let live = helix
        .get_streams(&channels(&["on", "rerun"]))
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
}

#[tokio::test]
async fn a_failed_status_is_an_error_with_twitch_s_message() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("tok", "ref", &[FOLLOWS]))).await;
    Mock::given(method("GET"))
        .and(path("/helix/streams/followed"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(
                json!({ "status": 401, "message": "Missing scope: user:read:follows" }),
            ),
        )
        .mount(&server)
        .await;
    let error = helix.get_followed_streams("42").await.unwrap_err();
    assert!(error.is_missing_scope(), "{error}");
    assert!(error.to_string().contains("Missing scope"));
}

#[tokio::test]
async fn the_poller_sees_a_channel_go_live() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("tok", "ref", &[FOLLOWS]))).await;
    // Offline for the first two polls, then live; the followed list is steady.
    Mock::given(method("GET"))
        .and(path("/helix/streams"))
        .respond_with(data(&[]))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/helix/streams"))
        .respond_with(data(&[raw("pepe", 1234)]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/helix/streams/followed"))
        .respond_with(data(&[raw("small", 3), raw("big", 900)]))
        .mount(&server)
        .await;

    let poller = StreamPoller::spawn(helix, fast());
    let mut rx = poller.subscribe();
    let pepe = ChannelId::new("pepe");
    poller.set_channels(vec![pepe.clone()]);

    // Unknown at first, then known offline...
    assert_eq!(rx.borrow().state(&pepe), None);
    let offline = wait_for(&mut rx, |s| s.state(&pepe) == Some(None)).await;
    assert_eq!(offline.error, None);
    // ...then live, with the details the header shows.
    let live = wait_for(&mut rx, |s| matches!(s.state(&pepe), Some(Some(_)))).await;
    let info = live.state(&pepe).unwrap().unwrap();
    assert_eq!(info.viewers, 1234);
    assert_eq!(info.title, "pepe is live");
    assert_eq!(info.category, "Just Chatting");
    assert_eq!(info.started_at, 1_615_388_645);
    // The followed list is most viewers first.
    let followed: Vec<&str> = live
        .followed
        .as_ref()
        .unwrap()
        .iter()
        .map(|s| s.channel.login())
        .collect();
    assert_eq!(followed, ["big", "small"]);

    // Closing the channel drops it from the snapshot on the next poll.
    poller.set_channels(vec![]);
    let gone = wait_for(&mut rx, |s| s.state(&pepe).is_none()).await;
    assert!(gone.live.is_empty());
}

#[tokio::test]
async fn the_poller_batches_a_large_open_set() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("tok", "ref", &[FOLLOWS]))).await;
    Mock::given(method("GET"))
        .and(path("/helix/streams"))
        .respond_with(data(&[]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/helix/streams/followed"))
        .respond_with(data(&[]))
        .mount(&server)
        .await;
    let poller = StreamPoller::spawn(
        helix,
        PollConfig {
            interval: Duration::from_secs(3600),
            ..fast()
        },
    );
    let mut rx = poller.subscribe();
    let open: Vec<ChannelId> = (0..230).map(|i| ChannelId::new(&format!("c{i}"))).collect();
    poller.set_channels(open.clone());
    let snapshot = wait_for(&mut rx, |s| s.checked.len() == 230).await;
    assert_eq!(snapshot.checked.len(), 230);
    let sizes: Vec<usize> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/helix/streams")
        .map(|r| logins_of(r).len())
        .collect();
    assert_eq!(sizes, [100, 100, 30]);
}

#[tokio::test]
async fn a_401_refreshes_the_token_and_the_poll_goes_through() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("old", "old-refresh", &[FOLLOWS]))).await;
    let auth = helix.auth().clone();
    for (bearer, response) in [
        (
            "old",
            ResponseTemplate::new(401)
                .set_body_json(json!({ "status": 401, "message": "Invalid OAuth token" })),
        ),
        ("new", data(&[raw("pepe", 7)])),
    ] {
        for endpoint in ["/helix/streams", "/helix/streams/followed"] {
            Mock::given(method("GET"))
                .and(path(endpoint))
                .and(wiremock::matchers::header(
                    "Authorization",
                    format!("Bearer {bearer}").as_str(),
                ))
                .respond_with(response.clone())
                .mount(&server)
                .await;
        }
    }
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=old-refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new", "refresh_token": "new-refresh", "expires_in": 14_000,
            "scope": [FOLLOWS], "token_type": "bearer",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let poller = StreamPoller::spawn(helix, fast());
    let mut rx = poller.subscribe();
    let pepe = ChannelId::new("pepe");
    poller.set_channels(vec![pepe.clone()]);
    let snapshot = wait_for(&mut rx, |s| matches!(s.state(&pepe), Some(Some(_)))).await;
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.followed.as_ref().map(Vec::len), Some(1));
    assert_eq!(auth.refresh_count(), 1, "one refresh for both calls");
}

#[tokio::test]
async fn failures_back_off_and_keep_the_last_good_data() {
    let server = MockServer::start().await;
    let helix = client(&server, Some(token("tok", "ref", &[FOLLOWS]))).await;
    Mock::given(method("GET"))
        .and(path("/helix/streams"))
        .respond_with(data(&[raw("pepe", 9)]))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/helix/streams"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/helix/streams/followed"))
        .respond_with(data(&[]))
        .mount(&server)
        .await;

    let poller = StreamPoller::spawn(helix, fast());
    let mut rx = poller.subscribe();
    let pepe = ChannelId::new("pepe");
    poller.set_channels(vec![pepe.clone()]);
    wait_for(&mut rx, |s| matches!(s.state(&pepe), Some(Some(_)))).await;
    let failed = wait_for(&mut rx, |s| s.error.is_some()).await;
    assert!(failed.error.unwrap().contains("500"));
    // The data from the good poll is still there.
    assert!(matches!(rx.borrow().state(&pepe), Some(Some(_))));

    // Wait out a few failing polls: with doubling waits (40, 80, 160, 160 ms) there are far fewer
    // requests than a fixed 40 ms interval would make.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let polls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/helix/streams")
        .count();
    assert!(polls <= 8, "{polls} polls: no backoff?");
}

#[tokio::test]
async fn logged_out_nothing_is_requested() {
    let server = MockServer::start().await;
    let helix = client(&server, None).await;
    let poller = StreamPoller::spawn(helix, fast());
    poller.set_channels(channels(&["pepe"]));
    poller.refresh_now();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*poller.subscribe().borrow(), StreamSnapshot::default());
    let helix_calls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/helix"))
        .count();
    assert_eq!(helix_calls, 0);
}

#[tokio::test]
async fn a_login_without_the_follows_scope_still_polls_open_channels() {
    let server = MockServer::start().await;
    // Granted only chat:read; the app wants FOLLOWS too.
    let helix = client(&server, Some(token("tok", "ref", &["chat:read"]))).await;
    Mock::given(method("GET"))
        .and(path("/helix/streams"))
        .respond_with(data(&[raw("pepe", 2)]))
        .mount(&server)
        .await;
    let poller = StreamPoller::spawn(helix, fast());
    let mut rx = poller.subscribe();
    let pepe = ChannelId::new("pepe");
    poller.set_channels(vec![pepe.clone()]);
    let snapshot = wait_for(&mut rx, |s| matches!(s.state(&pepe), Some(Some(_)))).await;
    assert_eq!(snapshot.followed, None);
    assert_eq!(snapshot.error, None);
    let followed_calls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/helix/streams/followed")
        .count();
    assert_eq!(followed_calls, 0);
}
