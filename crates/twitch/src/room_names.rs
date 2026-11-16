//! Names of Twitch rooms (PR 5.1). In shared chat a message says which room it came from only as a
//! number (`source-room-id`); [`RoomNames`] turns that into the channel's display name through
//! `api.ivr.fi` (`GET {base}/v2/twitch/user?id=<id>`, the same service the anonymous badge source
//! uses), once per id: the answer, or the failure, is remembered for the session. Nothing but the
//! public room id is sent, with no account or token.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gpui_chat_core::APP_NAME;
use serde::Deserialize;

/// The service's address.
pub const DEFAULT_BASE_URL: &str = "https://api.ivr.fi";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Deserialize)]
struct User {
    #[serde(default)]
    login: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
}

/// A room's channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomName {
    /// Lowercase login (`otherchan`).
    pub login: String,
    /// Display name (the login when Twitch has none).
    pub display_name: String,
}

/// The channel in an `api.ivr.fi` user answer (an array of users; empty for an unknown id). `None`
/// when there is no login to name it by.
pub fn parse_name(body: &[u8]) -> Option<RoomName> {
    let users: Vec<User> = serde_json::from_slice(body).ok()?;
    let user = users.into_iter().next()?;
    let login = user.login.trim().to_ascii_lowercase();
    if login.is_empty() {
        return None;
    }
    let display_name = match user.display_name.trim() {
        "" => login.clone(),
        name => name.to_owned(),
    };
    Some(RoomName {
        login,
        display_name,
    })
}

/// Looks room ids up and remembers what it learned. Cheap to clone; clones share the memory.
#[derive(Clone)]
pub struct RoomNames {
    http: reqwest::Client,
    base: Arc<str>,
    known: Arc<Mutex<HashMap<String, Option<Arc<RoomName>>>>>,
}

impl RoomNames {
    pub fn new() -> Result<Self, reqwest::Error> {
        Self::with_base(DEFAULT_BASE_URL)
    }

    pub fn with_base(base: &str) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .user_agent(format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            base: Arc::from(base.trim_end_matches('/')),
            known: Arc::default(),
        })
    }

    /// What is known about `room_id`: `None` when it has not been looked up, `Some(None)` when the
    /// lookup failed or found nobody (not asked again), `Some(Some(name))` otherwise.
    pub fn known(&self, room_id: &str) -> Option<Option<Arc<RoomName>>> {
        self.known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(room_id)
            .cloned()
    }

    /// Whether the caller should look `room_id` up now: true once per id (the first caller gets
    /// it), so two messages arriving together make one request.
    pub fn claim(&self, room_id: &str) -> bool {
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        if known.contains_key(room_id) {
            return false;
        }
        known.insert(room_id.to_owned(), None);
        true
    }

    /// Asks the service for the name of `room_id` and remembers the answer. A room id that is not
    /// digits is not sent (it names nobody). Returns the name.
    pub async fn lookup(&self, room_id: &str) -> Option<Arc<RoomName>> {
        let name = self.fetch(room_id).await.map(Arc::new);
        self.known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(room_id.to_owned(), name.clone());
        name
    }

    async fn fetch(&self, room_id: &str) -> Option<RoomName> {
        if room_id.is_empty() || !room_id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let url = format!("{}/v2/twitch/user?id={room_id}", self.base);
        let response = match self.http.get(&url).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::debug!(status = %response.status(), room_id, "no name for room");
                return None;
            }
            Err(error) => {
                tracing::debug!(%error, room_id, "room name lookup failed");
                return None;
            }
        };
        let body = response.bytes().await.ok()?;
        parse_name(&body)
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn the_display_name_wins_over_the_login() {
        let body = br#"[{"id":"1","login":"somechan","displayName":"SomeChan","logo":"x"}]"#;
        let name = parse_name(body).unwrap();
        assert_eq!(
            (name.login.as_str(), name.display_name.as_str()),
            ("somechan", "SomeChan")
        );
        let no_display = br#"[{"login":"somechan","displayName":""}]"#;
        assert_eq!(parse_name(no_display).unwrap().display_name, "somechan");
        assert_eq!(
            parse_name(br#"[{"displayName":"X"}]"#),
            None,
            "no login, no name"
        );
        assert_eq!(parse_name(b"[]"), None, "an unknown id");
        assert_eq!(parse_name(b"not json"), None);
        assert_eq!(parse_name(br#"[{"id":"1"}]"#), None);
    }

    #[tokio::test]
    async fn a_room_is_looked_up_once_and_remembered() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/twitch/user"))
            .and(query_param("id", "2000"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"id":"2000","login":"otherchan","displayName":"OtherChan"}]"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        let names = RoomNames::with_base(&server.uri()).unwrap();
        assert_eq!(names.known("2000"), None);
        assert!(names.claim("2000"));
        assert!(!names.claim("2000"), "only the first caller looks it up");
        let name = names.lookup("2000").await.unwrap();
        assert_eq!(
            (name.login.as_str(), name.display_name.as_str()),
            ("otherchan", "OtherChan")
        );
        assert_eq!(names.known("2000"), Some(Some(name)));
    }

    #[tokio::test]
    async fn a_failure_is_remembered_and_odd_ids_are_not_sent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        let names = RoomNames::with_base(&server.uri()).unwrap();
        assert_eq!(names.lookup("77").await, None);
        assert_eq!(names.known("77"), Some(None));
        assert!(!names.claim("77"), "a failed id is not asked again");
        // Never a request: the mock expects exactly the one above.
        assert_eq!(names.lookup("../x?y").await, None);
        assert_eq!(names.lookup("").await, None);
    }
}
