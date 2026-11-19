//! The check against a local mock of GitHub's "list releases" endpoint serving the recorded JSON
//! in `fixtures/updates/` (wiremock; the live API is never contacted).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const RELEASES: &str = include_str!("../../../fixtures/updates/releases.json");
const EMPTY: &str = include_str!("../../../fixtures/updates/releases-empty.json");
const MALFORMED: &str = include_str!("../../../fixtures/updates/releases-malformed.json");
const NOT_FOUND: &str = include_str!("../../../fixtures/updates/not-found.json");
const NON_SEMVER: &str = include_str!("../../../fixtures/updates/releases-non-semver.json");
const RATE_LIMITED: &str = include_str!("../../../fixtures/updates/rate-limited.json");

const ENDPOINT: &str = "/repos/charlie-niekirk/gpui-chat/releases";
const DAY: u64 = CHECK_INTERVAL;
const START: u64 = 1_800_000_000;

/// A checker on `server` whose clock is the returned cell, starting at [`START`].
fn checker(server: &MockServer, current: &str) -> (UpdateChecker, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(START));
    let clock = now.clone();
    let checker = UpdateChecker::new(current, None)
        .unwrap()
        .with_api_base(server.uri())
        .with_clock(move || clock.load(Ordering::SeqCst));
    (checker, now)
}

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json; charset=utf-8")
        .set_body_string(body)
}

async fn serve(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(ENDPOINT))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn available(tag: &str) -> Outcome {
    Outcome::UpdateAvailable(Release {
        version: semver::Version::parse(tag.trim_start_matches('v')).unwrap(),
        tag: tag.to_owned(),
        url: format!("https://github.com/charlie-niekirk/gpui-chat/releases/tag/{tag}"),
    })
}

#[tokio::test]
async fn a_newer_published_release_is_an_update() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let (checker, _) = checker(&server, "0.3.0");
    // v0.10.0 beats v0.3.0 (numerically, not as text); the draft v0.9.0 and the prereleases do not count.
    assert_eq!(checker.check_now().await, Ok(available("v0.10.0")));
}

#[tokio::test]
async fn the_running_version_or_a_newer_one_is_up_to_date() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    for current in ["0.10.0", "1.0.0"] {
        let (checker, _) = checker(&server, current);
        assert_eq!(
            checker.check_now().await,
            Ok(Outcome::UpToDate {
                latest: semver::Version::new(0, 10, 0)
            }),
            "{current}"
        );
    }
}

#[tokio::test]
async fn a_pre_release_build_is_older_than_its_release() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let (checker, _) = checker(&server, "0.10.0-dev");
    assert_eq!(checker.check_now().await, Ok(available("v0.10.0")));
}

#[tokio::test]
async fn no_releases_yet_is_not_an_error() {
    let server = MockServer::start().await;
    serve(&server, json(EMPTY)).await;
    let (checker, _) = checker(&server, "0.0.0");
    assert_eq!(checker.check_now().await, Ok(Outcome::NoRelease));
    // It is an answer like any other: the automatic check does not ask again today.
    assert_eq!(checker.check_if_due().await, Ok(Outcome::NoRelease));
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn a_404_means_no_release_too() {
    let server = MockServer::start().await;
    serve(
        &server,
        ResponseTemplate::new(404).set_body_string(NOT_FOUND),
    )
    .await;
    let (checker, _) = checker(&server, "0.0.0");
    assert_eq!(checker.check_now().await, Ok(Outcome::NoRelease));
}

#[tokio::test]
async fn tags_that_are_not_semver_are_no_release() {
    let server = MockServer::start().await;
    serve(&server, json(NON_SEMVER)).await;
    let (checker, _) = checker(&server, "0.0.0");
    assert_eq!(checker.check_now().await, Ok(Outcome::NoRelease));
}

#[tokio::test]
async fn malformed_json_is_an_error_and_pauses_the_automatic_check() {
    let server = MockServer::start().await;
    serve(&server, json(MALFORMED)).await;
    let (checker, now) = checker(&server, "0.0.0");
    assert!(matches!(
        checker.check_now().await,
        Err(CheckError::Malformed(_))
    ));
    assert_eq!(
        checker.check_if_due().await,
        Err(CheckError::BackingOff {
            until: START + FAILURE_PAUSE
        })
    );
    assert_eq!(requests(&server).await, 1);
    // An hour later it tries again.
    now.store(START + FAILURE_PAUSE, Ordering::SeqCst);
    assert!(checker.check_if_due().await.is_err());
    assert_eq!(requests(&server).await, 2);
}

#[tokio::test]
async fn an_answer_that_is_not_a_list_is_an_error() {
    let server = MockServer::start().await;
    serve(&server, json(NOT_FOUND)).await;
    let (checker, _) = checker(&server, "0.0.0");
    assert!(matches!(
        checker.check_now().await,
        Err(CheckError::Malformed(_))
    ));
}

#[tokio::test]
async fn a_server_error_is_an_error() {
    let server = MockServer::start().await;
    serve(&server, ResponseTemplate::new(503)).await;
    let (checker, _) = checker(&server, "0.0.0");
    assert_eq!(checker.check_now().await, Err(CheckError::Status(503)));
}

#[tokio::test]
async fn an_oversized_answer_is_refused() {
    let server = MockServer::start().await;
    serve(&server, json(&" ".repeat(3 * 1024 * 1024))).await;
    let (checker, _) = checker(&server, "0.0.0");
    assert_eq!(checker.check_now().await, Err(CheckError::TooLarge));
}

#[tokio::test]
async fn no_network_is_an_error_not_a_panic() {
    // Nothing listens on port 1, so the connection is refused at once.
    let base = "http://127.0.0.1:1";
    let now = Arc::new(AtomicU64::new(START));
    let clock = now.clone();
    let checker = UpdateChecker::new("0.0.0", None)
        .unwrap()
        .with_api_base(base)
        .with_clock(move || clock.load(Ordering::SeqCst));
    assert!(matches!(
        checker.check_now().await,
        Err(CheckError::Network(_))
    ));
    // The failure pauses the automatic check, but not a manual one.
    assert!(matches!(
        checker.check_if_due().await,
        Err(CheckError::BackingOff { .. })
    ));
    assert!(matches!(
        checker.check_now().await,
        Err(CheckError::Network(_))
    ));
}

#[tokio::test]
async fn a_slow_answer_times_out() {
    let server = MockServer::start().await;
    serve(&server, json(EMPTY).set_delay(Duration::from_secs(5))).await;
    let (checker, _) = checker(&server, "0.0.0");
    let checker = checker.with_timeout(Duration::from_millis(100)).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(
        checker.check_now().await,
        Err(CheckError::Network(_))
    ));
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn the_request_names_the_app_and_the_api_version() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(ENDPOINT))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(header(
            "user-agent",
            format!("{}/{}", gpui_chat_core::APP_NAME, env!("CARGO_PKG_VERSION")).as_str(),
        ))
        .respond_with(json(EMPTY))
        .expect(1)
        .mount(&server)
        .await;
    let (checker, _) = checker(&server, "0.0.0");
    assert_eq!(checker.check_now().await, Ok(Outcome::NoRelease));
}

// --- once a day -------------------------------------------------------------------------------

#[tokio::test]
async fn the_automatic_check_asks_once_a_day() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let (checker, now) = checker(&server, "0.3.0");
    assert_eq!(checker.check_if_due().await, Ok(available("v0.10.0")));
    // The remembered answer, no request, for the rest of the day (and across a day's launches).
    for hours in [0, 1, 12, 23] {
        now.store(START + hours * 3600, Ordering::SeqCst);
        assert_eq!(checker.check_if_due().await, Ok(available("v0.10.0")));
    }
    assert_eq!(requests(&server).await, 1);
    now.store(START + DAY, Ordering::SeqCst);
    assert_eq!(checker.check_if_due().await, Ok(available("v0.10.0")));
    assert_eq!(requests(&server).await, 2);
}

#[tokio::test]
async fn the_manual_check_asks_whatever_the_last_answer_age() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let (checker, _) = checker(&server, "0.3.0");
    checker.check_if_due().await.unwrap();
    checker.check_now().await.unwrap();
    assert_eq!(requests(&server).await, 2);
}

#[tokio::test]
async fn the_remembered_answer_survives_a_restart_and_is_judged_against_the_running_version() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(CACHE_FILE_NAME);
    let make = |current: &str| {
        UpdateChecker::new(current, Some(file.clone()))
            .unwrap()
            .with_api_base(server.uri())
            .with_clock(|| START + 3600)
    };
    // Run 1 asks GitHub.
    assert_eq!(make("0.3.0").check_if_due().await, Ok(available("v0.10.0")));
    // Run 2, the same day, does not ask, and still knows the update.
    assert_eq!(make("0.3.0").check_if_due().await, Ok(available("v0.10.0")));
    // Run 3 is the new version itself: no request, no notice.
    assert_eq!(
        make("0.10.0").check_if_due().await,
        Ok(Outcome::UpToDate {
            latest: semver::Version::new(0, 10, 0)
        })
    );
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn a_corrupt_cache_file_is_ignored() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(CACHE_FILE_NAME);
    std::fs::write(&file, "\u{0}\u{0} not json").unwrap();
    let checker = UpdateChecker::new("0.3.0", Some(file.clone()))
        .unwrap()
        .with_api_base(server.uri());
    assert_eq!(checker.check_if_due().await, Ok(available("v0.10.0")));
    // And the file was replaced by a good one.
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("\"latest_tag\"")
    );
}

// --- ETag / 304 --------------------------------------------------------------------------------

#[tokio::test]
async fn the_etag_is_sent_back_and_a_304_keeps_the_answer() {
    let server = MockServer::start().await;
    // Requests that carry the ETag get 304 (mounted first: wiremock tries mocks in order).
    Mock::given(method("GET"))
        .and(path(ENDPOINT))
        .and(header("if-none-match", "\"v1-etag\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    serve(&server, json(RELEASES).insert_header("etag", "\"v1-etag\"")).await;
    let (checker, now) = checker(&server, "0.3.0");
    assert_eq!(checker.check_if_due().await, Ok(available("v0.10.0")));
    now.store(START + DAY, Ordering::SeqCst);
    assert_eq!(checker.check_if_due().await, Ok(available("v0.10.0")));
    assert_eq!(requests(&server).await, 2);
    // The 304 counted as an answer: a day passes before the next request.
    now.store(START + DAY + 3600, Ordering::SeqCst);
    checker.check_if_due().await.unwrap();
    assert_eq!(requests(&server).await, 2);
}

#[tokio::test]
async fn no_etag_is_sent_before_there_is_an_answer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(ENDPOINT))
        .and(header("if-none-match", "\"stale\""))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    serve(&server, json(EMPTY)).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(CACHE_FILE_NAME);
    // An ETag without a remembered answer (a half-written or edited file) is not used.
    std::fs::write(&file, r#"{"etag": "\"stale\""}"#).unwrap();
    let checker = UpdateChecker::new("0.0.0", Some(file))
        .unwrap()
        .with_api_base(server.uri());
    assert_eq!(checker.check_now().await, Ok(Outcome::NoRelease));
}

#[tokio::test]
async fn a_304_with_nothing_remembered_is_an_error() {
    let server = MockServer::start().await;
    serve(&server, ResponseTemplate::new(304)).await;
    let (checker, _) = checker(&server, "0.0.0");
    assert_eq!(checker.check_now().await, Err(CheckError::Status(304)));
}

// --- rate limits -------------------------------------------------------------------------------

fn limited(reset: u64) -> ResponseTemplate {
    ResponseTemplate::new(403)
        .insert_header("x-ratelimit-limit", "60")
        .insert_header("x-ratelimit-remaining", "0")
        .insert_header("x-ratelimit-reset", reset.to_string().as_str())
        .set_body_string(RATE_LIMITED)
}

#[tokio::test]
async fn a_used_up_rate_limit_is_honoured_by_both_checks_until_it_resets() {
    let server = MockServer::start().await;
    let reset = START + 1800;
    serve(&server, limited(reset)).await;
    let (checker, now) = checker(&server, "0.0.0");
    assert_eq!(
        checker.check_now().await,
        Err(CheckError::RateLimited { until: reset })
    );
    assert_eq!(
        checker.check_if_due().await,
        Err(CheckError::RateLimited { until: reset })
    );
    assert_eq!(
        checker.check_now().await,
        Err(CheckError::RateLimited { until: reset })
    );
    assert_eq!(requests(&server).await, 1);
    // After the reset it asks again.
    now.store(reset, Ordering::SeqCst);
    server.reset().await;
    serve(&server, json(EMPTY)).await;
    assert_eq!(checker.check_if_due().await, Ok(Outcome::NoRelease));
}

#[tokio::test]
async fn retry_after_and_429_are_honoured_and_capped_at_a_day() {
    let server = MockServer::start().await;
    serve(
        &server,
        ResponseTemplate::new(429).insert_header("retry-after", "120"),
    )
    .await;
    let (checker, now) = checker(&server, "0.0.0");
    assert_eq!(
        checker.check_now().await,
        Err(CheckError::RateLimited { until: START + 120 })
    );
    server.reset().await;
    serve(
        &server,
        ResponseTemplate::new(429).insert_header("retry-after", "9999999"),
    )
    .await;
    now.store(START + 200, Ordering::SeqCst);
    assert_eq!(
        checker.check_now().await,
        Err(CheckError::RateLimited {
            until: START + 200 + DAY
        })
    );
}

#[tokio::test]
async fn a_reset_in_the_past_still_waits_a_minute() {
    let server = MockServer::start().await;
    serve(&server, limited(START - 1000)).await;
    let (checker, _) = checker(&server, "0.0.0");
    assert_eq!(
        checker.check_now().await,
        Err(CheckError::RateLimited { until: START + 60 })
    );
}

#[tokio::test]
async fn a_403_that_is_not_a_rate_limit_is_a_plain_error() {
    let server = MockServer::start().await;
    serve(&server, ResponseTemplate::new(403)).await;
    let (checker, _) = checker(&server, "0.0.0");
    assert_eq!(checker.check_now().await, Err(CheckError::Status(403)));
}

#[tokio::test]
async fn a_remembered_answer_is_still_served_while_rate_limited() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let (checker, now) = checker(&server, "0.3.0");
    checker.check_if_due().await.unwrap();
    server.reset().await;
    serve(&server, limited(START + DAY + 7200)).await;
    now.store(START + DAY, Ordering::SeqCst);
    // Due, so it asks, and is limited ...
    assert_eq!(
        checker.check_if_due().await,
        Err(CheckError::RateLimited {
            until: START + DAY + 7200
        })
    );
    // ... but the next automatic check shows what it knew instead of an error.
    assert_eq!(checker.check_if_due().await, Ok(available("v0.10.0")));
    assert_eq!(requests(&server).await, 1);
}

// --- dismissal ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_dismissed_notice_stays_dismissed_across_runs_until_a_newer_release() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(CACHE_FILE_NAME);
    let make = || {
        UpdateChecker::new("0.3.0", Some(file.clone()))
            .unwrap()
            .with_api_base(server.uri())
            .with_clock(|| START)
    };
    let first = make();
    let Ok(Outcome::UpdateAvailable(release)) = first.check_if_due().await else {
        panic!("expected an update");
    };
    assert!(!first.is_dismissed(&release));
    first.dismiss(&release);
    assert!(make().is_dismissed(&release));
    let newer = Release {
        tag: "v0.11.0".into(),
        ..release
    };
    assert!(!make().is_dismissed(&newer));
}

// --- the automatic task ------------------------------------------------------------------------

/// Lets the spawned task run until it waits (time is paused, so sleeping costs nothing).
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn with_the_setting_off_no_request_is_made_at_all() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(RELEASES))
        .expect(0)
        .mount(&server)
        .await;
    let (checker, _) = checker(&server, "0.0.0");
    let checker = Arc::new(checker);
    let wake = Arc::new(Notify::new());
    let task = tokio::spawn({
        let (checker, wake) = (checker.clone(), wake.clone());
        async move { run_automatic(&checker, || false, &wake, |_| {}).await }
    });
    settle().await;
    // Time passes and the setting is poked, still nothing.
    wake.notify_one();
    tokio::time::sleep(Duration::from_secs(3 * DAY)).await;
    wake.notify_one();
    settle().await;
    assert_eq!(requests(&server).await, 0);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn switching_the_setting_on_checks_and_reports_then_waits_a_day() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let (checker, now) = checker(&server, "0.3.0");
    // Paused time jumps to the nearest timer whenever the runtime waits for the socket, so the
    // request timeout must be farther away than anything this test sleeps for.
    let checker = Arc::new(
        checker
            .with_timeout(Duration::from_secs(1000 * DAY))
            .unwrap(),
    );
    let wake = Arc::new(Notify::new());
    let on = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reports = Arc::new(std::sync::Mutex::new(Vec::new()));
    let task = tokio::spawn({
        let (checker, wake, on, reports) =
            (checker.clone(), wake.clone(), on.clone(), reports.clone());
        async move {
            run_automatic(
                &checker,
                || on.load(Ordering::SeqCst),
                &wake,
                |outcome| reports.lock().unwrap().push(outcome),
            )
            .await
        }
    });
    settle().await;
    assert_eq!(requests(&server).await, 0);
    on.store(true, Ordering::SeqCst);
    wake.notify_one();
    settle().await;
    assert_eq!(requests(&server).await, 1);
    assert_eq!(reports.lock().unwrap().as_slice(), [available("v0.10.0")]);
    // Twelve hours on: nothing. A day on: one more request.
    now.fetch_add(12 * 3600, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(12 * 3600)).await;
    settle().await;
    assert_eq!(requests(&server).await, 1);
    now.fetch_add(12 * 3600 + 60, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(12 * 3600 + 60)).await;
    settle().await;
    assert_eq!(requests(&server).await, 2);
    // Switched off again: the next due time passes without a request.
    on.store(false, Ordering::SeqCst);
    now.fetch_add(2 * DAY, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(2 * DAY)).await;
    settle().await;
    assert_eq!(requests(&server).await, 2);
    task.abort();
}

#[tokio::test]
async fn an_overridden_api_base_keeps_its_state_in_a_file_of_its_own() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join(CACHE_FILE_NAME);
    let checker = UpdateChecker::with_override("0.3.0", Some(real.clone()), Some(server.uri()))
        .unwrap()
        .with_clock(|| START);
    assert_eq!(checker.check_now().await, Ok(available("v0.10.0")));
    assert!(!real.exists(), "the real cache file must stay untouched");
    assert!(dir.path().join(OVERRIDE_CACHE_FILE).exists());
}

#[tokio::test]
async fn pauses_written_under_a_wrong_clock_are_ignored() {
    let server = MockServer::start().await;
    serve(&server, json(EMPTY)).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(CACHE_FILE_NAME);
    // A rate limit and a failure pause years ahead, and a check "from the future".
    let far = START + 5 * 365 * DAY;
    std::fs::write(
        &file,
        format!(r#"{{"rate_limited_until": {far}, "retry_after": {far}, "checked_at": {far}}}"#),
    )
    .unwrap();
    let checker = UpdateChecker::new("0.0.0", Some(file))
        .unwrap()
        .with_api_base(server.uri())
        .with_clock(|| START);
    assert!(checker.next_check_in() <= Duration::from_secs(DAY));
    assert_eq!(checker.check_if_due().await, Ok(Outcome::NoRelease));
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test(start_paused = true)]
async fn waking_from_sleep_is_noticed_within_the_hour_and_an_answer_is_reported_once() {
    let server = MockServer::start().await;
    serve(&server, json(RELEASES)).await;
    let (checker, now) = checker(&server, "0.3.0");
    let checker = Arc::new(
        checker
            .with_timeout(Duration::from_secs(1000 * DAY))
            .unwrap(),
    );
    let wake = Arc::new(Notify::new());
    let reports = Arc::new(std::sync::Mutex::new(Vec::new()));
    let task = tokio::spawn({
        let (checker, wake, reports) = (checker.clone(), wake.clone(), reports.clone());
        async move {
            run_automatic(
                &checker,
                || true,
                &wake,
                |outcome| reports.lock().unwrap().push(outcome),
            )
            .await
        }
    });
    settle().await;
    assert_eq!(requests(&server).await, 1);
    // Hourly wake-ups within the day neither ask nor repeat the answer.
    now.fetch_add(3 * 3600, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3 * 3600)).await;
    settle().await;
    assert_eq!(requests(&server).await, 1);
    assert_eq!(reports.lock().unwrap().len(), 1);
    // The machine slept two days: the wall clock jumped, tokio's clock moved one hour.
    now.fetch_add(2 * DAY, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3600 + 1)).await;
    settle().await;
    assert_eq!(requests(&server).await, 2);
    task.abort();
}

#[test]
fn the_repository_is_read_from_the_manifest() {
    assert_eq!(
        repo_from_url(env!("CARGO_PKG_REPOSITORY")),
        "charlie-niekirk/gpui-chat"
    );
    assert_eq!(repo_from_url("https://github.com/a/b.git"), "a/b");
    assert_eq!(repo_from_url("https://github.com/a/b/"), "a/b");
}

#[test]
fn a_bad_running_version_is_refused() {
    assert!(matches!(
        UpdateChecker::new("not-a-version", None),
        Err(CheckError::BadVersion(_))
    ));
}
