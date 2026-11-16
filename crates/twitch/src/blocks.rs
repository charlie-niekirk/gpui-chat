//! The Twitch block list through Helix (PR 5.7).
//!
//! * `GET /helix/users/blocks?broadcaster_id=<own id>` lists the users the logged-in account has
//!   blocked (scope [`READ_SCOPE`], `user:read:blocked_users`). The app treats them as ignored.
//! * `PUT /helix/users/blocks?target_user_id=<id>` blocks a user (scope [`MANAGE_SCOPE`],
//!   `user:manage:blocked_users`); `DELETE` on the same path unblocks.
//!
//! Neither scope is part of a normal login ([`crate::auth::SCOPES`]); they are requested together by
//! *Enable block list sync…* ([`crate::auth::BLOCK_LIST_SCOPES`]). A login without them gets
//! [`BlockError::MissingScope`] before any request is made, the same way a moderation tool does.

use reqwest::Method;
use serde::Deserialize;
use thiserror::Error;

use crate::auth::{AuthError, AuthState};
use crate::helix::{HelixClient, HelixError, HelixListError, HelixResponse, encode_query_value};

/// The scope that lets the app read the block list.
pub const READ_SCOPE: &str = "user:read:blocked_users";
/// The scope that lets the app block and unblock users.
pub const MANAGE_SCOPE: &str = "user:manage:blocked_users";

/// Pages of 100: 5 000 blocked users is far beyond what anyone has.
const MAX_PAGES: usize = 50;

/// One user on the block list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedUser {
    pub id: String,
    /// Lowercase login.
    pub login: String,
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
struct RawBlocked {
    user_id: String,
    #[serde(default)]
    user_login: String,
    #[serde(default)]
    display_name: String,
}

impl RawBlocked {
    fn into_user(self) -> BlockedUser {
        BlockedUser {
            login: self.user_login.to_lowercase(),
            display_name: if self.display_name.trim().is_empty() {
                self.user_login
            } else {
                self.display_name
            },
            id: self.user_id,
        }
    }
}

fn helix_message(error: &HelixError) -> String {
    match error {
        HelixError::Auth(AuthError::NotLoggedIn) => {
            "You are not logged in. Log in to use your Twitch block list.".to_owned()
        }
        HelixError::Auth(AuthError::SessionEnded) => {
            "Your Twitch session ended. Log in again.".to_owned()
        }
        HelixError::Auth(other) => format!("Could not authorise the request: {other}."),
        HelixError::Network(why) => format!("Could not reach Twitch ({why})."),
    }
}

/// Why a block list request did not go through. `Display` is written for the user.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BlockError {
    /// Not logged in, the login ended, or the request never completed.
    #[error("{}", helix_message(.0))]
    Helix(#[from] HelixError),
    /// The login was not granted the scope: block list sync is enabled by a login that asks for it
    /// (account menu → *Enable block list sync…*).
    #[error(
        "Log in again to enable block list sync (grants {scope}): account menu → Enable block list sync."
    )]
    MissingScope { scope: String },
    /// Twitch refused the request, with its own message.
    #[error("{message}")]
    Rejected { status: u16, message: String },
    /// Too many requests; Twitch asked to wait.
    #[error("Twitch is rate limiting you. Try again in a moment.")]
    RateLimited,
}

impl BlockError {
    /// A login that lacks the scope (before the request, or as Twitch's own 401 for it).
    pub fn is_missing_scope(&self) -> bool {
        matches!(self, Self::MissingScope { .. })
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
}

fn rejected(status: u16, body: &[u8]) -> BlockError {
    let message = serde_json::from_slice::<ErrorBody>(body)
        .map(|body| body.message.trim().to_owned())
        .unwrap_or_default();
    match status {
        429 => BlockError::RateLimited,
        401 if message.to_ascii_lowercase().contains("scope") => BlockError::MissingScope {
            scope: message
                .to_ascii_lowercase()
                .split("scope:")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .map_or_else(
                    || READ_SCOPE.to_owned(),
                    |scope| scope.trim_end_matches('.').to_owned(),
                ),
        },
        401 => BlockError::Rejected {
            status,
            message: "Twitch did not accept your login for the block list. Log in again from the account menu."
                .to_owned(),
        },
        status => BlockError::Rejected {
            status,
            message: if message.is_empty() {
                format!("Twitch answered {status}.")
            } else {
                message
            },
        },
    }
}

fn interpret(response: &HelixResponse) -> Result<(), BlockError> {
    if (200..300).contains(&response.status) {
        Ok(())
    } else {
        Err(rejected(response.status, &response.body))
    }
}

impl From<HelixListError> for BlockError {
    fn from(error: HelixListError) -> Self {
        match error {
            HelixListError::Helix(error) => Self::Helix(error),
            HelixListError::Status { status, message } => {
                // `rejected` reads a Twitch body; rebuild one from the message the list gave.
                let body = serde_json::json!({ "message": message }).to_string();
                rejected(status, body.as_bytes())
            }
            HelixListError::Parse(why) => Self::Rejected {
                status: 200,
                message: format!("Twitch sent a block list this app cannot read ({why})."),
            },
        }
    }
}

impl HelixClient {
    /// Fails with [`BlockError::MissingScope`] when the login lacks `scope` (nothing is sent).
    fn require_scope(&self, scope: &str) -> Result<(), BlockError> {
        if let AuthState::LoggedIn(account) = self.auth().state()
            && !account.has_scope(scope)
        {
            return Err(BlockError::MissingScope {
                scope: scope.to_owned(),
            });
        }
        Ok(())
    }

    /// Everyone the logged-in account has blocked. Needs [`READ_SCOPE`].
    pub async fn get_blocked_users(&self) -> Result<Vec<BlockedUser>, BlockError> {
        self.require_scope(READ_SCOPE)?;
        let lease = self.auth().access_token().await.map_err(HelixError::from)?;
        let raw: Vec<RawBlocked> = self
            .get_all(
                &format!(
                    "users/blocks?broadcaster_id={}&first=100",
                    encode_query_value(&lease.user_id)
                ),
                MAX_PAGES,
            )
            .await?;
        Ok(raw.into_iter().map(RawBlocked::into_user).collect())
    }

    /// Blocks the user with id `target_id` (`PUT users/blocks`). Needs [`MANAGE_SCOPE`].
    pub async fn block_user(&self, target_id: &str) -> Result<(), BlockError> {
        self.change_block(Method::PUT, target_id).await
    }

    /// Unblocks the user with id `target_id` (`DELETE users/blocks`). Needs [`MANAGE_SCOPE`].
    pub async fn unblock_user(&self, target_id: &str) -> Result<(), BlockError> {
        self.change_block(Method::DELETE, target_id).await
    }

    async fn change_block(&self, method: Method, target_id: &str) -> Result<(), BlockError> {
        self.require_scope(MANAGE_SCOPE)?;
        let mut path = format!(
            "users/blocks?target_user_id={}",
            encode_query_value(target_id)
        );
        if method == Method::PUT {
            path.push_str("&source_context=chat");
        }
        let response = self.request_json(method, &path, None).await?;
        interpret(&response)
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

    async fn client(server: &MockServer, scopes: &[&str]) -> HelixClient {
        let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
        let store = Arc::new(MemoryTokenStore::with(StoredToken {
            access_token: Secret::new("access"),
            refresh_token: Secret::new("refresh"),
            expires_at: unix_now() + 4 * 3600,
            scopes: scopes.clone(),
            login: "viewer".to_owned(),
            user_id: "42".to_owned(),
        }));
        let oauth = OAuthClient::with_base(CLIENT, server.uri()).unwrap();
        let auth = Auth::with_scopes(oauth, store, &[]);
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

    fn page(users: &[(&str, &str)], cursor: Option<&str>) -> serde_json::Value {
        json!({
            "data": users.iter().map(|(id, login)| json!({
                "user_id": id, "user_login": login, "display_name": login.to_uppercase(),
            })).collect::<Vec<_>>(),
            "pagination": cursor.map_or_else(|| json!({}), |c| json!({ "cursor": c })),
        })
    }

    #[tokio::test]
    async fn the_block_list_is_read_for_the_logged_in_user_across_pages() {
        let server = MockServer::start().await;
        let helix = client(&server, &[READ_SCOPE]).await;
        Mock::given(method("GET"))
            .and(path("/helix/users/blocks"))
            .and(query_param("broadcaster_id", "42"))
            .and(query_param("first", "100"))
            .and(header("Authorization", "Bearer access"))
            .and(header("Client-Id", CLIENT))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(page(&[("1", "Spammer")], Some("next"))),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/helix/users/blocks"))
            .and(query_param("after", "next"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(&[("2", "bot_b")], None)))
            .mount(&server)
            .await;
        let blocked = helix.get_blocked_users().await.unwrap();
        assert_eq!(
            blocked,
            vec![
                BlockedUser {
                    id: "1".into(),
                    login: "spammer".into(),
                    display_name: "SPAMMER".into()
                },
                BlockedUser {
                    id: "2".into(),
                    login: "bot_b".into(),
                    display_name: "BOT_B".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_login_without_the_scope_sends_nothing() {
        let server = MockServer::start().await;
        let helix = client(&server, &[]).await;
        // No mock for the block endpoints: a request would be a 404 and a different error.
        let error = helix.get_blocked_users().await.unwrap_err();
        assert_eq!(
            error,
            BlockError::MissingScope {
                scope: READ_SCOPE.into()
            }
        );
        assert!(error.to_string().contains("Enable block list sync"));
        let error = helix.block_user("7").await.unwrap_err();
        assert_eq!(
            error,
            BlockError::MissingScope {
                scope: MANAGE_SCOPE.into()
            }
        );
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.iter().all(|r| !r.url.path().contains("blocks")),
            "{requests:?}"
        );
    }

    #[tokio::test]
    async fn blocking_puts_the_target_id_and_unblocking_deletes_it() {
        let server = MockServer::start().await;
        let helix = client(&server, &[READ_SCOPE, MANAGE_SCOPE]).await;
        Mock::given(method("PUT"))
            .and(path("/helix/users/blocks"))
            .and(query_param("target_user_id", "7"))
            .and(query_param("source_context", "chat"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/helix/users/blocks"))
            .and(query_param("target_user_id", "7"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        helix.block_user("7").await.unwrap();
        helix.unblock_user("7").await.unwrap();
    }

    #[tokio::test]
    async fn twitchs_refusals_become_sentences() {
        let server = MockServer::start().await;
        let helix = client(&server, &[READ_SCOPE, MANAGE_SCOPE]).await;
        Mock::given(method("PUT"))
            .and(query_param("target_user_id", "1"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "Bad Request", "status": 400,
                "message": "The user in target_user_id is not allowed to be blocked"
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(query_param("target_user_id", "2"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "Unauthorized", "status": 401,
                "message": "Missing scope: user:manage:blocked_users"
            })))
            .mount(&server)
            .await;
        assert_eq!(
            helix.block_user("1").await.unwrap_err(),
            BlockError::Rejected {
                status: 400,
                message: "The user in target_user_id is not allowed to be blocked".into()
            }
        );
        assert_eq!(
            helix.block_user("2").await.unwrap_err(),
            BlockError::MissingScope {
                scope: "user:manage:blocked_users".into()
            }
        );
    }
}
