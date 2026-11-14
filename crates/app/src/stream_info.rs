//! Stream state in the app (PR 3.5): which open channels are live (with title, category, viewers
//! and uptime) and which followed channels are live.
//!
//! The polling is `gpui_chat_twitch::streams::StreamPoller` on the backend runtime; [`StreamInfoHub`]
//! mirrors its snapshots into a GPUI entity that the pane header and the sidebar read. The shell
//! tells it which channels are open ([`StreamInfoHub::set_channels`]). Only the live source has one
//! (it needs a login): without it the header and sidebar simply show no stream state.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_chat_core::ChannelId;
use gpui_chat_twitch::HelixClient;
use gpui_chat_twitch::streams::{PollConfig, StreamInfo, StreamPoller, StreamSnapshot};
use gpui_kit::{App, AppContext as _, Context, Entity, Hsla, Task, rgb};

use crate::runtime;

/// What the UI knows about one channel's live state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness<'a> {
    /// Not polled yet (or logged out).
    Unknown,
    Offline,
    Live(&'a StreamInfo),
}

pub struct StreamInfoHub {
    /// `None` in demo mode.
    poller: Option<StreamPoller>,
    snapshot: StreamSnapshot,
    _task: Option<Task<()>>,
}

/// Environment variable that makes the *offline* sources (`--firehose` / `--replay`) show made-up
/// stream state, so the header and sidebar can be looked at without a Twitch login.
pub const DEMO_ENV: &str = "GPUI_CHAT_DEMO_STREAMS";

impl StreamInfoHub {
    /// Starts polling as `helix`'s user on the backend runtime.
    pub fn start(helix: HelixClient, cx: &mut App) -> Entity<Self> {
        let poller = runtime::enter(cx, || StreamPoller::spawn(helix, PollConfig::default()));
        let mut snapshots = poller.subscribe();
        cx.new(|cx: &mut Context<Self>| {
            let task = cx.spawn(async move |this, cx| {
                while snapshots.changed().await.is_ok() {
                    let snapshot = snapshots.borrow_and_update().clone();
                    // Every poll notifies, even an unchanged one: uptime and viewers move.
                    let alive = this.update(cx, |hub, cx| {
                        hub.snapshot = snapshot;
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            });
            Self {
                poller: Some(poller),
                snapshot: StreamSnapshot::default(),
                _task: Some(task),
            }
        })
    }

    /// A hub with made-up data and no network ([`DEMO_ENV`]): every other open channel is live,
    /// the rest offline, and four followed channels are live.
    pub fn demo(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            poller: None,
            snapshot: StreamSnapshot::default(),
            _task: None,
        })
    }

    /// The open channels, whose live state is polled.
    pub fn set_channels(&mut self, channels: Vec<ChannelId>, cx: &mut Context<Self>) {
        match &self.poller {
            Some(poller) => poller.set_channels(channels),
            None => {
                self.snapshot = demo_snapshot(&channels);
                cx.notify();
            }
        }
    }

    pub fn liveness(&self, channel: &ChannelId) -> Liveness<'_> {
        match self.snapshot.state(channel) {
            None => Liveness::Unknown,
            Some(None) => Liveness::Offline,
            Some(Some(info)) => Liveness::Live(info),
        }
    }

    /// The followed channels that are live, most viewers first; `None` when that is not known
    /// (logged out, the login lacks `user:read:follows`, or not loaded yet).
    pub fn followed_live(&self) -> Option<&[StreamInfo]> {
        self.snapshot.followed.as_deref()
    }
}

fn demo_info(login: &str, viewers: u64, category: &str) -> StreamInfo {
    StreamInfo {
        user_id: login.to_owned(),
        channel: ChannelId::new(login),
        display_name: login.to_owned(),
        title: format!("Demo stream: {login} plays for the screenshot, no network involved"),
        category: category.to_owned(),
        viewers,
        started_at: unix_now().saturating_sub(2 * 3600 + 14 * 60),
    }
}

fn demo_snapshot(channels: &[ChannelId]) -> StreamSnapshot {
    let mut snapshot = StreamSnapshot::default();
    for (index, channel) in channels.iter().enumerate() {
        snapshot.checked.insert(channel.clone());
        if index % 2 == 0 {
            let viewers = 1_234 * (index as u64 + 1);
            snapshot.live.insert(
                channel.clone(),
                demo_info(channel.login(), viewers, "Just Chatting"),
            );
        }
    }
    snapshot.followed = Some(vec![
        demo_info("demo_alpha", 48_210, "Elden Ring"),
        demo_info("demo_bravo", 12_034, "Just Chatting"),
        demo_info("demo_charlie", 940, "Software and Game Development"),
        demo_info("demo_delta", 12, "Music"),
    ]);
    snapshot
}

/// The red of the live dot.
pub fn live_color() -> Hsla {
    rgb(0xe91916).into()
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `1234` → `1,234`.
pub fn format_viewers(viewers: u64) -> String {
    let digits = viewers.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Short form for the sidebar: `999`, `1.2K`, `12K`, `1.4M`.
pub fn compact_viewers(viewers: u64) -> String {
    let scaled = |value: u64, unit: u64, suffix: &str| {
        let tenths = value * 10 / unit;
        if tenths >= 100 {
            format!("{}{suffix}", tenths / 10)
        } else {
            format!("{}.{}{suffix}", tenths / 10, tenths % 10)
        }
    };
    match viewers {
        0..=999 => viewers.to_string(),
        1_000..=999_999 => {
            // 999 999 would read "1000K": roll it over.
            if viewers >= 999_950 {
                "1M".to_owned()
            } else {
                scaled(viewers, 1_000, "K").replace(".0K", "K")
            }
        }
        _ => scaled(viewers, 1_000_000, "M").replace(".0M", "M"),
    }
}

/// `3 725 s` → `1h 2m`, `45 s` → `<1m`, `90 000 s` → `1d 1h`: the two largest units.
pub fn format_uptime(uptime: Duration) -> String {
    let minutes = uptime.as_secs() / 60;
    if minutes == 0 {
        return "<1m".to_owned();
    }
    let (days, hours, mins) = (minutes / 1440, minutes % 1440 / 60, minutes % 60);
    let parts: Vec<String> = [(days, 'd'), (hours, 'h'), (mins, 'm')]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .take(2)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect();
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_alternates_live_and_offline_and_lists_followed() {
        let channels: Vec<ChannelId> = ["a", "b", "c"].map(ChannelId::new).to_vec();
        let snapshot = demo_snapshot(&channels);
        assert!(matches!(snapshot.state(&channels[0]), Some(Some(_))));
        assert_eq!(snapshot.state(&channels[1]), Some(None));
        assert!(matches!(snapshot.state(&channels[2]), Some(Some(_))));
        assert_eq!(snapshot.followed.as_ref().map(Vec::len), Some(4));
        assert_eq!(snapshot.state(&ChannelId::new("other")), None);
    }

    #[test]
    fn viewers_get_thousands_separators() {
        assert_eq!(format_viewers(0), "0");
        assert_eq!(format_viewers(999), "999");
        assert_eq!(format_viewers(1_000), "1,000");
        assert_eq!(format_viewers(1_234_567), "1,234,567");
    }

    #[test]
    fn compact_viewers_stay_short() {
        assert_eq!(compact_viewers(0), "0");
        assert_eq!(compact_viewers(999), "999");
        assert_eq!(compact_viewers(1_000), "1K");
        assert_eq!(compact_viewers(1_234), "1.2K");
        assert_eq!(compact_viewers(12_345), "12K");
        assert_eq!(compact_viewers(999_949), "999K");
        assert_eq!(compact_viewers(999_999), "1M");
        assert_eq!(compact_viewers(1_400_000), "1.4M");
        assert_eq!(compact_viewers(25_000_000), "25M");
    }

    #[test]
    fn uptime_shows_two_units_at_most() {
        let secs = Duration::from_secs;
        assert_eq!(format_uptime(secs(0)), "<1m");
        assert_eq!(format_uptime(secs(59)), "<1m");
        assert_eq!(format_uptime(secs(60)), "1m");
        assert_eq!(format_uptime(secs(3_725)), "1h 2m");
        assert_eq!(format_uptime(secs(7_200)), "2h");
        assert_eq!(format_uptime(secs(90_000)), "1d 1h");
        assert_eq!(format_uptime(secs(90_000 + 61)), "1d 1h");
    }
}
