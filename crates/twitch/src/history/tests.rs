//! The recent-messages client and parser against recorded response bodies (`fixtures/history/`)
//! and a local mock of the service (wiremock). No test touches the network.

use std::time::Duration;

use wiremock::matchers::{header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../../fixtures/history/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn ids(messages: &[Arc<Message>]) -> Vec<String> {
    messages
        .iter()
        .map(|m| {
            m.id.as_str()
                .rsplit('-')
                .next()
                .unwrap()
                .trim_start_matches('0')
                .to_owned()
        })
        .collect()
}

#[test]
fn a_recorded_response_parses_like_live_chat() {
    let messages = parse_history(&fixture("recent-messages.json")).unwrap();
    // The line with no id and the malformed one are skipped; ROOMSTATE, NOTICE and the
    // moderation lines are not messages. Order is the service's (oldest first).
    assert_eq!(ids(&messages), ["1", "2", "3", "4", "5", "6", "7"]);
    assert!(messages.iter().all(|m| m.flags.history), "all marked");

    // Badges, emotes (as the tag's ranges) and the author come through the live parser.
    let alice = &messages[0];
    let author = alice.author.as_ref().unwrap();
    assert_eq!(
        (author.login.as_str(), author.display_name.as_str()),
        ("alice", "Alice")
    );
    assert_eq!(author.badges.len(), 2);
    assert!(
        alice
            .fragments
            .iter()
            .any(|f| matches!(f, gpui_chat_core::Fragment::Emote { .. })),
        "{:?}",
        alice.fragments
    );
    assert_eq!(messages[1].kind, gpui_chat_core::MessageKind::Action);
    // A reply keeps its reference.
    let reply = messages[2].reply.as_ref().unwrap();
    assert_eq!(reply.parent_login, "alice");
    assert_eq!(reply.parent_body, "hello chat Kappa");
    // A sub notice is kept, as a user notice.
    assert_eq!(messages[3].kind, gpui_chat_core::MessageKind::UserNotice);
    assert_eq!(messages[3].notice_id.as_deref(), Some("sub"));
    // Timestamps are Twitch's, not the service's.
    assert_eq!(alice.timestamp, Timestamp(1_794_826_001_000));
}

#[test]
fn moderation_since_is_applied_to_the_messages_before_it() {
    let messages = parse_history(&fixture("recent-messages.json")).unwrap();
    let deleted: Vec<bool> = messages.iter().map(|m| m.flags.deleted).collect();
    // m5 by CLEARMSG, m6 by the CLEARCHAT of its author; nothing else.
    assert_eq!(deleted, [false, false, false, false, true, true, false]);
}

#[test]
fn a_clear_of_the_whole_chat_deletes_what_came_before_only() {
    let body = serde_json::json!({
        "messages": [
            "@id=a;tmi-sent-ts=1 :u!u@u.tmi.twitch.tv PRIVMSG #c :one",
            "@tmi-sent-ts=2 :tmi.twitch.tv CLEARCHAT #c",
            "@id=b;tmi-sent-ts=3 :u!u@u.tmi.twitch.tv PRIVMSG #c :two",
        ]
    })
    .to_string();
    let messages = parse_history(&body).unwrap();
    assert_eq!(
        messages.iter().map(|m| m.flags.deleted).collect::<Vec<_>>(),
        [true, false]
    );
}

#[test]
fn a_line_without_the_historical_tag_is_still_marked() {
    let body = r##"{"messages":["@id=a;tmi-sent-ts=1 :u!u@u.tmi.twitch.tv PRIVMSG #c :hi"]}"##;
    assert!(parse_history(body).unwrap()[0].flags.history);
}

#[test]
fn the_services_own_refusal_is_an_error_but_only_without_messages() {
    let error = parse_history(&fixture("channel-not-joined.json")).unwrap_err();
    assert!(
        matches!(&error, HistoryError::Service { code, .. } if code == "channel_not_joined"),
        "{error:?}"
    );
    let partial = r##"{"messages":["@id=a;tmi-sent-ts=1 :u!u@u.tmi.twitch.tv PRIVMSG #c :hi"],"error":"half","error_code":"x"}"##;
    assert_eq!(parse_history(partial).unwrap().len(), 1);
    // An empty channel is fine too.
    assert_eq!(
        parse_history(r#"{"messages":[],"error":null,"error_code":null}"#).unwrap(),
        []
    );
}

#[test]
fn garbage_is_malformed_not_a_panic() {
    for body in ["", "<html>502</html>", "null", r#"{"messages": 3}"#] {
        assert!(
            matches!(parse_history(body), Err(HistoryError::Malformed(_))),
            "{body:?}"
        );
    }
}

#[test]
fn a_reconnect_asks_from_a_little_before_the_newest_message_seen() {
    assert_eq!(gap_start(Timestamp(100_000)), Timestamp(90_000));
    assert_eq!(gap_start(Timestamp(3)), Timestamp(0), "never negative");
    let request = HistoryRequest::gap_after(Timestamp(1_000_000));
    assert_eq!(request.after, Some(Timestamp(990_000)));
    assert_eq!(request.limit, MAX_LIMIT);
    assert_eq!(HistoryRequest::on_join().after, None);
}

async fn server_with(body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/recent-messages/forsen"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn fetch_asks_for_the_channel_with_a_limit_and_parses_the_answer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/recent-messages/forsen"))
        .and(query_param("limit", "100"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture("recent-messages.json")))
        .expect(1)
        .mount(&server)
        .await;
    let client = HistoryClient::with_base(server.uri()).unwrap();
    let messages = client
        .fetch(&ChannelId::new("#Forsen"), HistoryRequest::on_join())
        .await
        .unwrap();
    assert_eq!(messages.len(), 7);
    // No account, token or cookie is sent: only the request line and the user agent.
    let received = &server.received_requests().await.unwrap()[0];
    assert!(received.headers.get("authorization").is_none());
    assert!(received.headers.get("cookie").is_none());
    assert_eq!(received.url.query(), Some("limit=100"));
}

#[tokio::test]
async fn a_gap_request_carries_after_and_a_large_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/recent-messages/forsen"))
        .and(query_param("after", "990000"))
        .and(query_param("limit", "800"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"messages":[]}"#))
        .expect(1)
        .mount(&server)
        .await;
    let client = HistoryClient::with_base(server.uri()).unwrap();
    let messages = client
        .fetch(
            &ChannelId::new("forsen"),
            HistoryRequest::gap_after(Timestamp(1_000_000)),
        )
        .await
        .unwrap();
    assert!(messages.is_empty());
}

#[tokio::test]
async fn the_limit_is_clamped_to_what_the_service_allows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("limit", "800"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"messages":[]}"#))
        .expect(1)
        .mount(&server)
        .await;
    let client = HistoryClient::with_base(server.uri()).unwrap();
    let request = HistoryRequest {
        limit: 5000,
        after: None,
    };
    client
        .fetch(&ChannelId::new("forsen"), request)
        .await
        .unwrap();
}

#[tokio::test]
async fn http_errors_are_errors_not_panics() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let client = HistoryClient::with_base(server.uri()).unwrap();
    let error = client
        .fetch(&ChannelId::new("forsen"), HistoryRequest::on_join())
        .await
        .unwrap_err();
    assert_eq!(error, HistoryError::Status(503));
}

#[tokio::test]
async fn a_slow_service_times_out() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"messages":[]}"#)
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server)
        .await;
    let client =
        HistoryClient::with_base_and_timeout(server.uri(), Duration::from_millis(100)).unwrap();
    let error = client
        .fetch(&ChannelId::new("forsen"), HistoryRequest::on_join())
        .await
        .unwrap_err();
    assert_eq!(error, HistoryError::Timeout);
}

#[tokio::test]
async fn an_unreachable_service_is_a_network_error_without_the_channel_in_it() {
    // Nothing listens on this port.
    let client = HistoryClient::with_base("http://127.0.0.1:9").unwrap();
    let error = client
        .fetch(&ChannelId::new("secretchannel"), HistoryRequest::on_join())
        .await
        .unwrap_err();
    assert!(matches!(error, HistoryError::Network(_)), "{error:?}");
    assert!(!error.to_string().contains("secretchannel"), "{error}");
}

#[tokio::test]
async fn the_services_refusal_comes_back_as_an_error() {
    let server = server_with(&fixture("channel-not-joined.json")).await;
    let client = HistoryClient::with_base(server.uri()).unwrap();
    let error = client
        .fetch(&ChannelId::new("forsen"), HistoryRequest::on_join())
        .await
        .unwrap_err();
    assert!(matches!(error, HistoryError::Service { .. }), "{error:?}");
}

// ---- the tracker: when to fetch, and what to ask for ---------------------------------------

fn joined(channel: &str) -> ChatEvent {
    ChatEvent::Connection {
        channel: Some(ChannelId::new(channel)),
        state: gpui_chat_core::ConnectionState::Connected,
    }
}

fn live(channel: &str, id: &str, ts: i64) -> ChatEvent {
    ChatEvent::Message {
        channel: ChannelId::new(channel),
        message: Arc::new(Message::chat(
            id,
            Timestamp(ts),
            gpui_chat_core::Author::new("u", "u"),
            vec![],
        )),
    }
}

#[test]
fn the_first_join_asks_for_the_recent_history() {
    let mut tracker = HistoryTracker::new(true);
    let fetch = tracker.observe(&joined("Forsen")).expect("a fetch");
    assert_eq!(fetch.channel, ChannelId::new("forsen"));
    assert_eq!(fetch.request, HistoryRequest::on_join());
}

#[test]
fn the_connection_as_a_whole_and_other_events_fetch_nothing() {
    let mut tracker = HistoryTracker::new(true);
    for event in [
        ChatEvent::Connection {
            channel: None,
            state: gpui_chat_core::ConnectionState::Connected,
        },
        ChatEvent::Connection {
            channel: Some(ChannelId::new("a")),
            state: gpui_chat_core::ConnectionState::Connecting,
        },
        live("a", "1", 5),
    ] {
        assert_eq!(tracker.observe(&event), None, "{event:?}");
    }
}

#[test]
fn a_rejoin_after_a_reconnect_asks_only_for_the_gap() {
    let mut tracker = HistoryTracker::new(true);
    let first = tracker.observe(&joined("a")).unwrap();
    let fetched = [Arc::new(Message::chat(
        "h1",
        Timestamp(500_000),
        gpui_chat_core::Author::new("u", "u"),
        vec![],
    ))];
    assert!(tracker.complete(&first.channel, first.token, Some(&fetched)));
    // Live chat goes on, then the connection drops and the channel is joined again.
    tracker.observe(&live("a", "1", 600_000));
    tracker.observe(&live("a", "2", 700_000));
    tracker.observe(&live("a", "old", 650_000)); // out of order: the newest still counts
    let rejoin = tracker.observe(&joined("a")).expect("a fetch");
    assert_eq!(
        rejoin.request,
        HistoryRequest::gap_after(Timestamp(700_000))
    );
    assert_eq!(rejoin.request.after, Some(Timestamp(690_000)));
    assert_ne!(rejoin.token, first.token);
}

#[test]
fn the_gap_starts_after_the_newest_of_live_and_fetched_messages() {
    let mut tracker = HistoryTracker::new(true);
    let first = tracker.observe(&joined("a")).unwrap();
    // Nothing live arrived before the drop; the fetch itself held the newest message.
    let fetched = [
        Arc::new(Message::chat(
            "h1",
            Timestamp(40_000),
            gpui_chat_core::Author::new("u", "u"),
            vec![],
        )),
        Arc::new(Message::chat(
            "h2",
            Timestamp(90_000),
            gpui_chat_core::Author::new("u", "u"),
            vec![],
        )),
    ];
    tracker.complete(&first.channel, first.token, Some(&fetched));
    let rejoin = tracker.observe(&joined("a")).unwrap();
    assert_eq!(rejoin.request.after, Some(Timestamp(80_000)));
}

#[test]
fn an_echo_or_a_system_line_does_not_move_the_gap() {
    let mut tracker = HistoryTracker::new(true);
    let first = tracker.observe(&joined("a")).unwrap();
    tracker.complete(&first.channel, first.token, Some(&[]));
    tracker.observe(&live("a", "1", 100_000));
    // The user's own echo carries the local clock (here far ahead); a system line has no time.
    let mut echo = Message::chat(
        "e",
        Timestamp(9_000_000),
        gpui_chat_core::Author::new("me", "me"),
        vec![],
    );
    echo.flags.echo = true;
    tracker.observe(&ChatEvent::Message {
        channel: ChannelId::new("a"),
        message: Arc::new(echo),
    });
    tracker.observe(&ChatEvent::Message {
        channel: ChannelId::new("a"),
        message: Arc::new(Message::plain(
            "s",
            Timestamp(8_000_000),
            gpui_chat_core::MessageKind::System,
            "x",
        )),
    });
    let rejoin = tracker.observe(&joined("a")).unwrap();
    assert_eq!(rejoin.request.after, Some(Timestamp(90_000)));
}

#[test]
fn a_rejoin_after_a_failed_first_fetch_asks_for_everything_again() {
    let mut tracker = HistoryTracker::new(true);
    let first = tracker.observe(&joined("a")).unwrap();
    assert!(
        tracker.complete(&first.channel, first.token, None),
        "failed, but current"
    );
    tracker.observe(&live("a", "1", 100_000));
    let rejoin = tracker.observe(&joined("a")).unwrap();
    assert_eq!(rejoin.request, HistoryRequest::on_join());
}

#[test]
fn an_empty_channel_that_answered_is_asked_in_full_next_time() {
    // `filled`, but nothing to anchor a gap on.
    let mut tracker = HistoryTracker::new(true);
    let first = tracker.observe(&joined("a")).unwrap();
    tracker.complete(&first.channel, first.token, Some(&[]));
    let rejoin = tracker.observe(&joined("a")).unwrap();
    assert_eq!(rejoin.request, HistoryRequest::on_join());
}

#[test]
fn history_off_fetches_nothing_and_on_again_applies_to_the_next_join() {
    let mut tracker = HistoryTracker::new(false);
    assert_eq!(tracker.observe(&joined("a")), None);
    assert_eq!(tracker.observe(&joined("a")), None, "a re-join neither");
    tracker.set_enabled(true);
    assert!(tracker.enabled());
    assert!(tracker.observe(&joined("a")).is_some());
    tracker.set_enabled(false);
    assert_eq!(tracker.observe(&joined("a")), None);
}

#[test]
fn only_the_latest_fetch_of_a_channel_is_wanted() {
    let mut tracker = HistoryTracker::new(true);
    let first = tracker.observe(&joined("a")).unwrap();
    let second = tracker.observe(&joined("a")).unwrap(); // re-joined before the first answered
    assert!(
        !tracker.complete(&first.channel, first.token, Some(&[])),
        "stale"
    );
    assert!(tracker.complete(&second.channel, second.token, Some(&[])));
    // Channels do not affect each other.
    let other = tracker.observe(&joined("b")).unwrap();
    assert!(tracker.complete(&other.channel, other.token, None));
}

#[test]
fn a_closed_channel_discards_its_running_fetch() {
    let mut tracker = HistoryTracker::new(true);
    let fetch = tracker.observe(&joined("a")).unwrap();
    tracker.forget(&fetch.channel);
    assert!(!tracker.complete(&fetch.channel, fetch.token, Some(&[])));
    // Opened again later: a fresh first join.
    let again = tracker.observe(&joined("a")).unwrap();
    assert_eq!(again.request, HistoryRequest::on_join());
}

// ---- join and reconnect end to end: tracker → client (mock service) → store ---------------------

/// A service that, like the real one, answers `after` with the messages after it and otherwise
/// the newest `limit`, over a fixed log of `(id, tmi-sent-ts)`.
async fn service_with_log(log: &[(&str, i64)]) -> MockServer {
    use wiremock::{Request, Respond};
    struct Log(Vec<(String, i64)>);
    impl Respond for Log {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let param = |name: &str| {
                request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == name)
                    .and_then(|(_, value)| value.parse::<i64>().ok())
            };
            let limit = param("limit").unwrap_or(100) as usize;
            let after = param("after").unwrap_or(i64::MIN);
            let mut lines: Vec<String> = self
                .0
                .iter()
                .filter(|(_, ts)| *ts > after)
                .map(|(id, ts)| {
                    format!(
                        "@id={id};historical=1;tmi-sent-ts={ts};rm-received-ts={ts} \
                         :u!u@u.tmi.twitch.tv PRIVMSG #forsen :text of {id}"
                    )
                })
                .collect();
            let skip = lines.len().saturating_sub(limit);
            lines.drain(..skip);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "messages": lines }))
        }
    }
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/recent-messages/forsen"))
        .respond_with(Log(log
            .iter()
            .map(|(id, ts)| ((*id).to_owned(), *ts))
            .collect()))
        .mount(&server)
        .await;
    server
}

fn shown(store: &gpui_chat_core::MessageStore) -> Vec<String> {
    store.iter().map(|m| m.id.to_string()).collect()
}

#[tokio::test]
async fn join_then_reconnect_shows_every_message_once_in_order() {
    use gpui_chat_core::MessageStore;
    // 25 messages, 1 s apart.
    let log: Vec<(String, i64)> = (1..=25).map(|n| (format!("m{n:02}"), n * 1000)).collect();
    let log_refs: Vec<(&str, i64)> = log.iter().map(|(id, ts)| (id.as_str(), *ts)).collect();
    let server = service_with_log(&log_refs).await;
    let client = HistoryClient::with_base(server.uri()).unwrap();
    let mut tracker = HistoryTracker::new(true);
    let mut store = MessageStore::new();
    let channel = ChannelId::new("forsen");

    // Join: the first 20 messages exist; one live message (m21) arrives while the fetch runs.
    let join = tracker.observe(&joined("forsen")).unwrap();
    let live_m21 = live("forsen", "m21", 21_000);
    tracker.observe(&live_m21);
    if let ChatEvent::Message { message, .. } = live_m21 {
        store.push(message);
    }
    let fetched = client
        .fetch(
            &channel,
            HistoryRequest {
                limit: 10,
                ..join.request
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fetched.len(),
        10,
        "the newest ten of what the service had (m16..m25)"
    );
    assert!(tracker.complete(&channel, join.token, Some(&fetched)));
    store.merge_history(fetched);
    // m16..m20 from history, then the live m21 (also in the answer: dropped), then m22..m25 that
    // history already knew about (it was fetched after they were said).
    assert_eq!(
        shown(&store),
        [
            "m16", "m17", "m18", "m19", "m20", "m21", "m22", "m23", "m24", "m25"
        ]
    );
    assert!(
        !store.get("m21").unwrap().flags.history,
        "the live copy stays"
    );

    // Connection lost after m25; chat said m26..m30 meanwhile; the channel is joined again and
    // live messages resume with m31.
    let rejoin = tracker.observe(&joined("forsen")).unwrap();
    assert_eq!(rejoin.request.after, Some(Timestamp(25_000 - 10_000)));
    let live_m31 = live("forsen", "m31", 31_000);
    tracker.observe(&live_m31);
    if let ChatEvent::Message { message, .. } = live_m31 {
        store.push(message);
    }
    let mut grown = log_refs.clone();
    grown.extend((26..=31).map(|n| {
        (
            ["m26", "m27", "m28", "m29", "m30", "m31"][n - 26],
            n as i64 * 1000,
        )
    }));
    let server = service_with_log(&grown).await;
    let client = HistoryClient::with_base(server.uri()).unwrap();
    let gap = client.fetch(&channel, rejoin.request).await.unwrap();
    // Only the gap (plus the overlap), not the whole backlog again.
    assert_eq!(
        gap.len(),
        16,
        "m16..m31 after the 10 s overlap: {:?}",
        ids(&gap)
    );
    assert!(tracker.complete(&channel, rejoin.token, Some(&gap)));
    let added = store.merge_history(gap);
    assert_eq!(added, 5, "m26..m30 are the only new ones");
    assert_eq!(
        shown(&store),
        [
            "m16", "m17", "m18", "m19", "m20", "m21", "m22", "m23", "m24", "m25", "m26", "m27",
            "m28", "m29", "m30", "m31"
        ]
    );
}
