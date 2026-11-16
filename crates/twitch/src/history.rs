//! Recent messages (PR 5.5): the public recent-messages service
//! (<https://recent-messages.robotty.de/>, run by a third party) keeps the last few hundred raw IRC
//! lines of the channels it has been asked about. [`HistoryClient`] fetches them for a channel and
//! [`parse_history`] turns them into messages with the same parser the live connection uses
//! ([`crate::parse_line_with`]), so badges, emotes and replies come out exactly as in live chat.
//!
//! * `GET {base}/api/v2/recent-messages/<login>?limit=N[&after=<unix ms>]` answers
//!   `{"messages": ["<raw IRC line>", ...], "error": null | "text", "error_code": null | "code"}`.
//!   `after` keeps only messages received after that time; it is how a reconnect asks for the gap
//!   (the service's boundary is not exact, so callers ask from a little earlier and de-duplicate by
//!   message id, see [`gap_start`]). Moderation lines are requested too, so a message that was
//!   deleted or timed out since is shown as deleted, not as if nothing happened.
//! * Nothing here touches the store or the clock; merging (order, de-duplication) is
//!   `MessageStore::merge_history` in core. Nothing is fetched unless the caller asks, which is
//!   how the `chat.history` opt-out works: the channel name goes to a third party only from here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::{
    APP_NAME, ChannelId, ChatEvent, ConnectionState, Message, MessageKind, Moderation, Timestamp,
};
use serde::Deserialize;
use thiserror::Error;

use crate::parse::{ParseOptions, parse_line_with};

/// The service's address.
pub const DEFAULT_BASE_URL: &str = "https://recent-messages.robotty.de";

/// Messages asked for when a channel is joined. (The service keeps at most 800.)
pub const JOIN_LIMIT: u32 = 100;

/// The most the service returns for one request.
pub const MAX_LIMIT: u32 = 800;

/// How much earlier than the newest message already seen a reconnect asks from, because the
/// service's `after` is judged by when *it* received a message, not when Twitch stamped it.
pub const GAP_OVERLAP: Duration = Duration::from_secs(10);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Why recent messages could not be had. None of these is worth more than a log line and a quiet
/// notice: chat works without them.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HistoryError {
    /// The service did not answer in time.
    #[error("the recent-messages service did not answer in time")]
    Timeout,
    /// DNS, connection or TLS trouble.
    #[error("could not reach the recent-messages service: {0}")]
    Network(String),
    /// A non-2xx answer (the service is down, overloaded or gone).
    #[error("the recent-messages service answered HTTP {0}")]
    Status(u16),
    /// A 2xx answer that is not the documented JSON.
    #[error("the recent-messages service sent an unreadable answer: {0}")]
    Malformed(String),
    /// The channel name is not a Twitch login, so nothing is requested.
    #[error("not a channel name: {0:?}")]
    InvalidChannel(String),
    /// The service itself says no (a disabled channel, ...). `channel_not_joined` is *not* one of
    /// these: it is the service's normal answer for a channel it is not tracking yet, and
    /// reads as an empty history.
    #[error("the recent-messages service says: {message} ({code})")]
    Service { code: String, message: String },
}

/// What to ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryRequest {
    /// At most this many messages (clamped to [`MAX_LIMIT`]).
    pub limit: u32,
    /// Only messages received after this time; `None` for the newest `limit` ones.
    pub after: Option<Timestamp>,
}

impl HistoryRequest {
    /// The newest [`JOIN_LIMIT`] messages: what a channel shows when it is first joined.
    pub fn on_join() -> Self {
        Self {
            limit: JOIN_LIMIT,
            after: None,
        }
    }

    /// Everything since shortly before `newest_seen` (see [`gap_start`]), as many as the service
    /// has: what fills the hole a reconnect left.
    pub fn gap_after(newest_seen: Timestamp) -> Self {
        Self {
            limit: MAX_LIMIT,
            after: Some(gap_start(newest_seen)),
        }
    }
}

/// Where a reconnect asks from: [`GAP_OVERLAP`] before the newest message already shown. Messages
/// in the overlap come back again and are dropped by id when merged.
pub fn gap_start(newest_seen: Timestamp) -> Timestamp {
    let overlap = i64::try_from(GAP_OVERLAP.as_millis()).unwrap_or(i64::MAX);
    Timestamp::from_unix_millis(newest_seen.unix_millis().saturating_sub(overlap).max(0))
}

/// The recent-messages endpoint. Cheap to clone.
#[derive(Clone)]
pub struct HistoryClient {
    http: reqwest::Client,
    base: String,
}

impl std::fmt::Debug for HistoryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl HistoryClient {
    /// A client for [`DEFAULT_BASE_URL`].
    pub fn new() -> Result<Self, HistoryError> {
        Self::with_base(DEFAULT_BASE_URL)
    }

    /// A client talking to `base` instead (tests).
    pub fn with_base(base: impl Into<String>) -> Result<Self, HistoryError> {
        Self::with_base_and_timeout(base, REQUEST_TIMEOUT)
    }

    /// [`HistoryClient::with_base`] with another whole-request timeout (tests).
    pub fn with_base_and_timeout(
        base: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, HistoryError> {
        let http = reqwest::Client::builder()
            .user_agent(format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION")))
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|error| HistoryError::Network(describe(error)))?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
        })
    }

    /// Fetches and parses the recent messages of `channel`, oldest first, each flagged
    /// `history`. The channel name is the only thing sent besides the request's own parameters;
    /// a name that is not a Twitch login is refused without a request.
    pub async fn fetch(
        &self,
        channel: &ChannelId,
        request: HistoryRequest,
    ) -> Result<History, HistoryError> {
        let url = self.url(channel, request)?;
        let response = self.http.get(url).send().await.map_err(|error| {
            if error.is_timeout() {
                HistoryError::Timeout
            } else {
                HistoryError::Network(describe(error))
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(HistoryError::Status(status.as_u16()));
        }
        let body = response.text().await.map_err(|error| {
            if error.is_timeout() {
                HistoryError::Timeout
            } else {
                HistoryError::Network(describe(error))
            }
        })?;
        parse_history(&body)
    }

    fn url(
        &self,
        channel: &ChannelId,
        request: HistoryRequest,
    ) -> Result<reqwest::Url, HistoryError> {
        if !is_login(channel.login()) {
            return Err(HistoryError::InvalidChannel(channel.login().to_owned()));
        }
        let mut url = reqwest::Url::parse(&self.base)
            .map_err(|error| HistoryError::Malformed(format!("bad service address: {error}")))?;
        url.path_segments_mut()
            .map_err(|()| HistoryError::Malformed("bad service address".to_owned()))?
            .pop_if_empty()
            .extend(["api", "v2", "recent-messages", channel.login()]);
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("limit", &request.limit.clamp(1, MAX_LIMIT).to_string());
            if let Some(after) = request.after {
                query.append_pair("after", &after.unix_millis().to_string());
            }
        }
        Ok(url)
    }
}

/// A transport error without the URL (which holds the channel name).
fn describe(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    messages: Vec<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
}

/// A Twitch login: `[a-z0-9_]{1,25}`. Anything else (`..`, a slash, an empty name) never reaches
/// the URL.
fn is_login(name: &str) -> bool {
    (1..=25).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// What one answer of the service held.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// Chat lines, actions and user notices, oldest first, each flagged `history`.
    pub messages: Vec<Arc<Message>>,
    /// What moderators removed, in order (see [`Moderation`]); not yet applied to anything.
    pub moderation: Vec<Moderation>,
    /// How many raw lines the answer had (the service caps an answer at [`MAX_LIMIT`]).
    pub lines: usize,
}

impl History {
    /// Whether a reconnect's answer may have left a hole: it asked for everything after
    /// [`gap_start`] and got a full answer (`limit` lines) whose oldest message is newer than the
    /// newest one already seen, so the service had more than it could return and the earlier part
    /// of the gap is missing. Never true for a join (no gap to speak of).
    pub fn may_be_missing_messages(&self, request: &HistoryRequest) -> bool {
        let Some(after) = request.after else {
            return false;
        };
        // `after` is `newest_seen` less the overlap, see `gap_start`.
        let newest_seen = after
            .unix_millis()
            .saturating_add(i64::try_from(GAP_OVERLAP.as_millis()).unwrap_or(i64::MAX));
        self.lines >= request.limit as usize
            && self
                .messages
                .first()
                .is_some_and(|oldest| oldest.timestamp.unix_millis() > newest_seen)
    }
}

/// Turns the service's JSON answer into a [`History`].
///
/// * Chat lines, actions and `USERNOTICE`s become messages flagged `history` (also when the line
///   lacks the service's `historical=1` tag). A line with no timestamp at all is skipped: it
///   could not be put in order.
/// * `CLEARMSG` / `CLEARCHAT` lines become [`Moderation`]s carrying the time they happened; they
///   are for the caller to apply to *everything* it shows, not only to this answer's messages.
///   `ROOMSTATE` and `NOTICE` lines are dropped: they describe the room as it was, and would
///   overwrite what is true now.
/// * A line that does not parse is skipped (logged at debug level), never an error: one bad line
///   must not cost the other 99.
/// * `channel_not_joined` (the service is not tracking the channel yet and starts doing so when
///   asked: most small channels the first time, every channel after the service restarted) is an
///   empty history. Any other `error` with no messages is an error ([`HistoryError::Service`]);
///   with messages it is ignored.
pub fn parse_history(body: &str) -> Result<History, HistoryError> {
    let response: Response =
        serde_json::from_str(body).map_err(|error| HistoryError::Malformed(error.to_string()))?;
    if response.messages.is_empty()
        && let Some(message) = response.error
    {
        let code = response.error_code.unwrap_or_default();
        if code == "channel_not_joined" {
            tracing::debug!("the recent-messages service was not tracking this channel yet");
            return Ok(History::default());
        }
        return Err(HistoryError::Service { code, message });
    }
    // A line with neither timestamp tag gets this and is dropped.
    const NO_TIME: Timestamp = Timestamp(i64::MIN);
    let options = ParseOptions {
        fallback_timestamp: NO_TIME,
        ..ParseOptions::default()
    };
    let mut history = History {
        lines: response.messages.len(),
        ..History::default()
    };
    let mut skipped = 0usize;
    for line in &response.messages {
        match parse_line_with(line, &options) {
            Ok(Some(ChatEvent::Message { mut message, .. })) => {
                if message.timestamp == NO_TIME {
                    skipped += 1;
                    continue;
                }
                Arc::make_mut(&mut message).flags.history = true;
                history.messages.push(message);
            }
            Ok(Some(ChatEvent::DeleteMessage { message_id, .. })) => {
                history.moderation.push(Moderation::Delete { message_id });
            }
            Ok(Some(ChatEvent::ClearChat {
                target,
                at: Some(at),
                ..
            })) => history.moderation.push(Moderation::Clear { target, at }),
            Ok(Some(ChatEvent::ClearChat { at: None, .. })) => skipped += 1,
            Ok(_) => {}
            Err(error) => {
                skipped += 1;
                tracing::debug!(%error, "skipped an unreadable recent-messages line");
            }
        }
    }
    if skipped > 0 {
        tracing::debug!(skipped, "recent messages: lines skipped");
    }
    Ok(history)
}

/// One fetch the tracker wants made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetch {
    pub channel: ChannelId,
    /// Hand this back to [`HistoryTracker::complete`] with the outcome.
    pub token: u64,
    pub request: HistoryRequest,
}

#[derive(Debug, Default)]
struct Tracked {
    /// The newest message time seen live or fetched (`tmi-sent-ts`, never the local clock).
    newest: Option<Timestamp>,
    /// Whether a fetch for this channel has succeeded: until then a (re)join asks for the whole
    /// recent history again, not just the gap, because nothing earlier was ever shown.
    filled: bool,
    /// The fetch whose answer is still wanted; an older one is stale.
    token: u64,
}

/// Decides *when* to fetch history and *what* to ask for, from the stream of chat events. Pure
/// (no network, no clock), so the join / reconnect logic is tested without either.
///
/// * The first time a channel's join is confirmed (`Connection { channel: Some(_), Connected }`)
///   the newest [`JOIN_LIMIT`] messages are asked for.
/// * Every later confirmation is a re-join after a reconnect: the answer is limited to what came
///   after the newest message seen so far ([`HistoryRequest::gap_after`]), so only the gap is
///   filled. If the earlier fetch never succeeded (the service was down), the whole recent history
///   is asked for instead.
/// * While disabled ([`HistoryTracker::set_enabled`], the `chat.history` opt-out) it plans
///   nothing, so nothing is requested and the channel name goes nowhere.
/// * Only the latest fetch of a channel counts: [`HistoryTracker::complete`] says whether an
///   answer is still wanted (a newer join, or a closed channel, made it stale).
///
/// Messages are never *removed* or reordered here; merging, ordering and de-duplication by message
/// id are `MessageStore::merge_history`'s job.
#[derive(Debug)]
pub struct HistoryTracker {
    enabled: bool,
    next_token: u64,
    channels: HashMap<ChannelId, Tracked>,
}

impl HistoryTracker {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            next_token: 0,
            channels: HashMap::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Switches fetching on or off for the joins that follow; fetches already running are left
    /// alone (their answers are still shown).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Looks at an event on its way to the UI. Returns the fetch to make when it confirms a join.
    pub fn observe(&mut self, event: &ChatEvent) -> Option<Fetch> {
        match event {
            ChatEvent::Message { channel, message } => {
                // Only what Twitch stamped counts: the echo of the user's own message carries the
                // local clock, and a system line has no meaningful time.
                if matches!(
                    message.kind,
                    MessageKind::Chat | MessageKind::Action | MessageKind::UserNotice
                ) && !message.flags.echo
                {
                    self.channels
                        .entry(channel.clone())
                        .or_default()
                        .see(message.timestamp);
                }
                None
            }
            ChatEvent::Connection {
                channel: Some(channel),
                state: ConnectionState::Connected,
            } => self.plan(channel),
            _ => None,
        }
    }

    fn plan(&mut self, channel: &ChannelId) -> Option<Fetch> {
        let tracked = self.channels.entry(channel.clone()).or_default();
        if !self.enabled {
            return None;
        }
        self.next_token += 1;
        tracked.token = self.next_token;
        let request = match tracked.newest {
            Some(newest) if tracked.filled => HistoryRequest::gap_after(newest),
            _ => HistoryRequest::on_join(),
        };
        Some(Fetch {
            channel: channel.clone(),
            token: self.next_token,
            request,
        })
    }

    /// Records the end of a fetch. `fetched` is what it returned (`None` if it failed). Returns
    /// `false` when the answer is stale and must be dropped: the channel was closed or joined
    /// again since, and the newer fetch covers it.
    pub fn complete(
        &mut self,
        channel: &ChannelId,
        token: u64,
        fetched: Option<&[Arc<Message>]>,
    ) -> bool {
        let Some(tracked) = self.channels.get_mut(channel) else {
            return false;
        };
        if tracked.token != token {
            return false;
        }
        if let Some(messages) = fetched {
            tracked.filled = true;
            for message in messages {
                tracked.see(message.timestamp);
            }
        }
        true
    }

    /// Forgets a closed channel; a fetch still running for it becomes stale.
    pub fn forget(&mut self, channel: &ChannelId) {
        self.channels.remove(channel);
    }
}

impl Tracked {
    fn see(&mut self, timestamp: Timestamp) {
        self.newest = Some(
            self.newest
                .map_or(timestamp, |newest| newest.max(timestamp)),
        );
    }
}

#[cfg(test)]
mod tests;
