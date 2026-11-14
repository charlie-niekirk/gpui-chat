//! Stream state from Helix (PR 3.5): who is live, with what title, category and viewers.
//!
//! * [`HelixClient::get_streams`] — `Get Streams` for any number of logins, batched 100 at a time
//!   (Helix's limit per call).
//! * [`HelixClient::get_followed_streams`] — `Get Followed Streams` for the logged-in user, paged
//!   by `pagination.cursor`. Needs the `user:read:follows` scope.
//! * [`StreamPoller`] — the polling loop. It owns no UI: it polls the *watched* channels (the
//!   open ones) and the followed-live list on one interval, backs off exponentially while
//!   requests fail, wakes early when the watched set or the login changes, and publishes a
//!   [`StreamSnapshot`] on a `watch` channel. Logged out, it publishes an empty snapshot and
//!   sleeps until the login changes.
//!
//! Only live streams come back from Helix; a watched channel that is absent from a successful
//! poll is offline, which [`StreamSnapshot::checked`] distinguishes from "not polled yet".

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui_chat_core::ChannelId;
use serde::Deserialize;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::helix::{HelixClient, HelixListError};

/// Helix accepts at most 100 `user_login` values per `Get Streams` call.
pub const MAX_LOGINS_PER_REQUEST: usize = 100;
/// `Get Followed Streams` returns at most 100 per page; 10 pages is 1 000 live followed channels.
const MAX_FOLLOWED_PAGES: usize = 10;
/// The scope `Get Followed Streams` needs.
const FOLLOWS_SCOPE: &str = "user:read:follows";

/// One live stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub user_id: String,
    pub channel: ChannelId,
    pub display_name: String,
    pub title: String,
    /// Category (game) name; empty when the stream has none.
    pub category: String,
    pub viewers: u64,
    /// When the stream started, in Unix seconds; 0 when Twitch did not say.
    pub started_at: u64,
}

impl StreamInfo {
    /// How long the stream has been live at `now` (Unix seconds); zero when the start is unknown.
    pub fn uptime(&self, now: u64) -> Duration {
        if self.started_at == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(now.saturating_sub(self.started_at))
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawStream {
    #[serde(default)]
    user_id: String,
    user_login: String,
    #[serde(default)]
    user_name: String,
    #[serde(default)]
    game_name: String,
    /// `live`, or empty when Twitch could not tell.
    #[serde(default = "live")]
    r#type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    viewer_count: u64,
    #[serde(default)]
    started_at: String,
}

fn live() -> String {
    "live".to_owned()
}

impl RawStream {
    fn into_info(self) -> Option<StreamInfo> {
        if self.r#type != "live" {
            return None;
        }
        let channel = ChannelId::new(&self.user_login);
        Some(StreamInfo {
            user_id: self.user_id,
            display_name: if self.user_name.is_empty() {
                channel.login().to_owned()
            } else {
                self.user_name
            },
            channel,
            title: self.title.trim().to_owned(),
            category: self.game_name,
            viewers: self.viewer_count,
            started_at: parse_rfc3339(&self.started_at).unwrap_or(0),
        })
    }
}

/// Parses Twitch's `2021-03-10T15:04:05Z` into Unix seconds (UTC only, which is all Helix sends;
/// fractional seconds are ignored).
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let text = text.trim().strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let mut time = time.splitn(3, ':');
    let hour: u64 = time.next()?.parse().ok()?;
    let minute: u64 = time.next()?.parse().ok()?;
    let second: u64 = time.next()?.split('.').next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let days = u64::try_from(days).ok()?;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second.min(60))
}

impl HelixClient {
    /// The live streams among `channels`, in no particular order. Channels are sent
    /// [`MAX_LOGINS_PER_REQUEST`] at a time (duplicates removed); offline channels are simply
    /// absent. Fails as a whole if any batch fails.
    pub async fn get_streams(
        &self,
        channels: &[ChannelId],
    ) -> Result<Vec<StreamInfo>, HelixListError> {
        let mut seen = HashSet::new();
        let logins: Vec<&str> = channels
            .iter()
            .map(ChannelId::login)
            .filter(|login| !login.is_empty() && seen.insert(*login))
            .collect();
        let mut live = Vec::new();
        for batch in logins.chunks(MAX_LOGINS_PER_REQUEST) {
            let mut path = format!("streams?first={MAX_LOGINS_PER_REQUEST}");
            for login in batch {
                path.push_str("&user_login=");
                path.push_str(login);
            }
            let raw: Vec<RawStream> = self.get_all(&path, 1).await?;
            live.extend(raw.into_iter().filter_map(RawStream::into_info));
        }
        Ok(live)
    }

    /// The live streams `user_id` follows, most viewers first (Twitch's order). Needs the
    /// `user:read:follows` scope.
    pub async fn get_followed_streams(
        &self,
        user_id: &str,
    ) -> Result<Vec<StreamInfo>, HelixListError> {
        let path = format!("streams/followed?first=100&user_id={user_id}");
        let raw: Vec<RawStream> = self.get_all(&path, MAX_FOLLOWED_PAGES).await?;
        Ok(raw.into_iter().filter_map(RawStream::into_info).collect())
    }
}

/// What the last polls found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamSnapshot {
    /// The watched channels that are live.
    pub live: HashMap<ChannelId, StreamInfo>,
    /// The watched channels a successful poll has covered: one of these absent from `live` is
    /// offline, one absent from both is not known yet.
    pub checked: HashSet<ChannelId>,
    /// The followed channels that are live, most viewers first. `None` while unknown: logged out,
    /// the login lacks `user:read:follows`, or no poll has succeeded yet.
    pub followed: Option<Vec<StreamInfo>>,
    /// Why the latest poll failed (the data above is from before); `None` after a clean one.
    pub error: Option<String>,
}

impl StreamSnapshot {
    /// The live state of `channel`: `Some(Some(_))` live, `Some(None)` offline, `None` unknown.
    pub fn state(&self, channel: &ChannelId) -> Option<Option<&StreamInfo>> {
        match self.live.get(channel) {
            Some(info) => Some(Some(info)),
            None => self.checked.contains(channel).then_some(None),
        }
    }
}

/// How the poller paces itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollConfig {
    /// Between polls while everything works. Helix caches stream data for about a minute, so
    /// polling faster finds nothing new.
    pub interval: Duration,
    /// The longest wait while polls keep failing (the wait doubles from `interval`).
    pub max_backoff: Duration,
    /// After the watched set changes, wait this long for more changes before polling (opening
    /// ten restored channels is one poll, not ten).
    pub settle: Duration,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            max_backoff: Duration::from_secs(5 * 60),
            settle: Duration::from_millis(500),
        }
    }
}

impl PollConfig {
    /// The wait before the next poll after `failures` polls in a row failed (0: the normal
    /// interval).
    pub fn delay(&self, failures: u32) -> Duration {
        if failures == 0 {
            return self.interval;
        }
        let factor = 1u32.checked_shl(failures.min(16)).unwrap_or(u32::MAX);
        self.interval
            .saturating_mul(factor)
            .min(self.max_backoff.max(self.interval))
    }
}

/// The polling loop, running on the tokio runtime it was spawned in. Dropping it stops the loop.
pub struct StreamPoller {
    watched: watch::Sender<Vec<ChannelId>>,
    refresh: std::sync::Arc<Notify>,
    snapshot: watch::Receiver<StreamSnapshot>,
    task: JoinHandle<()>,
}

impl Drop for StreamPoller {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl StreamPoller {
    /// Starts polling as `helix`'s user. Must be called inside a tokio runtime.
    pub fn spawn(helix: HelixClient, config: PollConfig) -> Self {
        let (watched, watched_rx) = watch::channel(Vec::new());
        let (snapshot_tx, snapshot) = watch::channel(StreamSnapshot::default());
        let refresh = std::sync::Arc::new(Notify::new());
        let task = tokio::spawn(run(helix, config, watched_rx, refresh.clone(), snapshot_tx));
        Self {
            watched,
            refresh,
            snapshot,
            task,
        }
    }

    /// Sets the channels whose live state is polled (the open ones). A change polls soon.
    pub fn set_channels(&self, channels: Vec<ChannelId>) {
        self.watched.send_if_modified(|current| {
            if *current == channels {
                false
            } else {
                *current = channels;
                true
            }
        });
    }

    /// Polls now instead of at the next interval.
    pub fn refresh_now(&self) {
        self.refresh.notify_one();
    }

    /// The snapshots; `changed()` resolves after every poll.
    pub fn subscribe(&self) -> watch::Receiver<StreamSnapshot> {
        self.snapshot.clone()
    }
}

async fn run(
    helix: HelixClient,
    config: PollConfig,
    mut watched: watch::Receiver<Vec<ChannelId>>,
    refresh: std::sync::Arc<Notify>,
    snapshot: watch::Sender<StreamSnapshot>,
) {
    let mut auth = helix.auth().subscribe();
    let mut failures = 0u32;
    loop {
        let account = auth.borrow_and_update().account().cloned();
        let Some(account) = account else {
            snapshot.send_if_modified(|current| {
                let changed = *current != StreamSnapshot::default();
                *current = StreamSnapshot::default();
                changed
            });
            failures = 0;
            tokio::select! {
                result = auth.changed() => if result.is_err() { return },
                () = refresh.notified() => {}
            }
            continue;
        };
        let channels = watched.borrow_and_update().clone();
        let can_follow = !account.missing_scopes.iter().any(|s| s == FOLLOWS_SCOPE);
        let current = snapshot.borrow().clone();
        let next = poll(&helix, &account.user_id, &channels, can_follow, current).await;
        failures = if next.error.is_some() {
            failures.saturating_add(1)
        } else {
            0
        };
        if let Some(error) = &next.error {
            tracing::warn!(%error, failures, "stream state poll failed");
        }
        snapshot.send_replace(next);

        let delay = config.delay(failures);
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = refresh.notified() => {}
            result = auth.changed() => if result.is_err() { return },
            result = watched.changed() => {
                if result.is_err() { return }
                // Let a burst of changes finish.
                loop {
                    tokio::select! {
                        () = tokio::time::sleep(config.settle) => break,
                        result = watched.changed() => if result.is_err() { return },
                    }
                }
            }
        }
    }
}

/// One poll: the watched channels and the followed list, each independently. What failed keeps
/// its old value; `error` says the first failure.
async fn poll(
    helix: &HelixClient,
    user_id: &str,
    channels: &[ChannelId],
    can_follow: bool,
    previous: StreamSnapshot,
) -> StreamSnapshot {
    let mut next = StreamSnapshot {
        error: None,
        ..previous
    };
    // Channels that were closed since stop being tracked straight away.
    let watching: HashSet<&ChannelId> = channels.iter().collect();
    next.live.retain(|channel, _| watching.contains(channel));
    next.checked.retain(|channel| watching.contains(channel));

    if !channels.is_empty() {
        match helix.get_streams(channels).await {
            Ok(streams) => {
                next.live = streams
                    .into_iter()
                    .filter(|info| watching.contains(&info.channel))
                    .map(|info| (info.channel.clone(), info))
                    .collect();
                next.checked = channels.iter().cloned().collect();
            }
            Err(error) => next.error = Some(error.to_string()),
        }
    }
    if can_follow {
        match helix.get_followed_streams(user_id).await {
            Ok(mut streams) => {
                streams.sort_by_key(|stream| std::cmp::Reverse(stream.viewers));
                next.followed = Some(streams);
            }
            // The login predates the scope (the banner already asks for a new one): not an error
            // worth retrying with backoff.
            Err(error) if error.is_missing_scope() => next.followed = None,
            Err(error) => {
                next.error.get_or_insert_with(|| error.to_string());
            }
        }
    } else {
        next.followed = None;
    }
    next
}

#[cfg(test)]
mod tests;
