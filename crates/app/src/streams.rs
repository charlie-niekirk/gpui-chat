//! Streamlink in the app: the per-channel play state ([`StreamStates`], plain data, unit-tested)
//! and the GPUI entity ([`StreamHub`]) that owns the [`Streams`] tracker, feeds its events into
//! the states through the bridge and ends every child when the app quits.
//!
//! The pane header reads [`StreamHub::state`] and calls [`StreamHub::toggle`] / [`play`] /
//! [`stop`]; the shell calls [`StreamHub::forget`] when a channel is closed.
//!
//! [`play`]: StreamHub::play
//! [`stop`]: StreamHub::stop

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::ChannelId;
use gpui_chat_settings::StreamlinkSettings;
use gpui_chat_streamlink::{
    DEFAULT_QUALITY, LaunchSpec, SessionId, StreamError, StreamEvent, StreamEventKind, Streams,
    valid_quality,
};
use gpui_kit::{App, AppContext as _, Context, Entity, Window};

use crate::bridge;
use crate::config;

/// Events the worker threads may have queued before one blocks (a handful per stream).
const EVENT_QUEUE: usize = 64;
/// How long quit waits for streamlink and its players to exit before they are killed.
const QUIT_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Launched; streamlink is being found or has not reported yet.
    Starting,
    Running,
    /// Stop requested, waiting for the process to go.
    Stopping,
    /// Exited on its own; `error` says why, when it did.
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamState {
    pub session: SessionId,
    pub phase: Phase,
    pub error: Option<StreamError>,
}

impl StreamState {
    /// The play button shows "stop" (a stream that is starting can be cancelled too).
    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            Phase::Starting | Phase::Running | Phase::Stopping
        )
    }
}

/// The play state of every channel that has one; no entry means idle.
#[derive(Debug, Default)]
pub struct StreamStates {
    states: HashMap<String, StreamState>,
}

impl StreamStates {
    pub fn get(&self, channel: &str) -> Option<&StreamState> {
        self.states.get(channel)
    }

    /// A new launch replaces whatever the channel showed (including an old error).
    pub fn begin(&mut self, channel: &str, session: SessionId) {
        self.states.insert(
            channel.to_owned(),
            StreamState {
                session,
                phase: Phase::Starting,
                error: None,
            },
        );
    }

    pub fn stopping(&mut self, channel: &str) {
        if let Some(state) = self.states.get_mut(channel).filter(|s| s.is_active()) {
            state.phase = Phase::Stopping;
        }
    }

    /// Hides an error (and the idle `Ended` state under it).
    pub fn dismiss(&mut self, channel: &str) {
        if self
            .states
            .get(channel)
            .is_some_and(|state| !state.is_active())
        {
            self.states.remove(channel);
        }
    }

    pub fn remove(&mut self, channel: &str) {
        self.states.remove(channel);
    }

    /// Applies a tracker event; `true` if anything the UI shows changed. Events of a session
    /// that is no longer the channel's (a restart, a closed channel) are ignored.
    pub fn apply(&mut self, event: &StreamEvent) -> bool {
        let Some(state) = self.states.get_mut(&event.channel) else {
            return false;
        };
        if state.session != event.session {
            return false;
        }
        match &event.kind {
            StreamEventKind::Started => {
                if state.phase == Phase::Starting {
                    state.phase = Phase::Running;
                    return true;
                }
                false
            }
            StreamEventKind::Failed(error) => {
                if state.error.as_ref() == Some(error) {
                    return false;
                }
                state.error = Some(error.clone());
                true
            }
            StreamEventKind::Exited { stopped } => {
                if *stopped || state.error.is_none() {
                    // Asked for, or ended cleanly (the stream finished): back to idle.
                    self.states.remove(&event.channel);
                } else {
                    state.phase = Phase::Ended;
                }
                true
            }
        }
    }
}

/// `settings.toml` → the command to run for `channel`.
pub fn launch_spec(channel: &str, quality: &str, settings: &StreamlinkSettings) -> LaunchSpec {
    let text = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    LaunchSpec {
        channel: channel.to_owned(),
        quality: quality.to_owned(),
        binary_override: text(&settings.path).map(Into::into),
        player: text(&settings.player),
        extra_args: settings.extra_args.clone(),
    }
}

/// The quality a channel's menu starts at: the configured default, or `best` if that is unusable
/// (it is validated on load, so this only guards hand-built settings).
fn default_quality(settings: &StreamlinkSettings) -> String {
    if valid_quality(&settings.default_quality) {
        settings.default_quality.clone()
    } else {
        DEFAULT_QUALITY.to_owned()
    }
}

/// Owns the tracker and the states; one per app.
pub struct StreamHub {
    streams: Arc<Streams>,
    states: StreamStates,
    /// The quality chosen in a channel's menu (this run only).
    qualities: HashMap<String, String>,
}

impl StreamHub {
    /// Creates the hub, routes tracker events into it (applied once per frame by the bridge) and
    /// ends every child on quit.
    pub fn start(window: &Window, cx: &mut App) -> Entity<Self> {
        let (sender, receiver) = bridge::channel::<StreamEvent>(EVENT_QUEUE, EVENT_QUEUE);
        let streams = Arc::new(Streams::new(Arc::new(move |event| {
            // Worker threads; a closed bridge means the app is going away.
            let _ = sender.blocking_send(event);
        })));
        let hub = cx.new(|cx| {
            cx.on_app_quit(|this: &mut Self, _| {
                this.streams.stop_all(QUIT_GRACE);
                async {}
            })
            .detach();
            Self {
                streams,
                states: StreamStates::default(),
                qualities: HashMap::new(),
            }
        });
        bridge::attach(
            &hub,
            window.window_handle(),
            receiver,
            cx,
            |hub, events, _| {
                let mut changed = false;
                for event in &events {
                    changed |= hub.states.apply(event);
                }
                changed
            },
        )
        .detach();
        hub
    }

    pub fn state(&self, channel: &ChannelId) -> Option<&StreamState> {
        self.states.get(channel.login())
    }

    /// The quality the menu shows as chosen for `channel`.
    pub fn quality(&self, channel: &ChannelId, cx: &App) -> String {
        self.qualities
            .get(channel.login())
            .cloned()
            .unwrap_or_else(|| default_quality(&config::current(cx).streamlink))
    }

    /// Play, or stop when something is already running.
    pub fn toggle(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if self.state(channel).is_some_and(StreamState::is_active) {
            self.stop(channel, cx);
        } else {
            let quality = self.quality(channel, cx);
            self.play(channel, &quality, cx);
        }
    }

    /// Starts `channel` at `quality` (restarting it if it is running) and remembers the quality.
    pub fn play(&mut self, channel: &ChannelId, quality: &str, cx: &mut Context<Self>) {
        self.qualities
            .insert(channel.login().to_owned(), quality.to_owned());
        let spec = launch_spec(channel.login(), quality, &config::current(cx).streamlink);
        let session = self.streams.launch(spec);
        self.states.begin(channel.login(), session);
        cx.notify();
    }

    pub fn stop(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        self.streams.stop(channel.login());
        self.states.stopping(channel.login());
        cx.notify();
    }

    pub fn dismiss(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        self.states.dismiss(channel.login());
        cx.notify();
    }

    /// The channel was closed: end its stream and drop everything known about it.
    pub fn forget(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        self.streams.stop(channel.login());
        self.states.remove(channel.login());
        self.qualities.remove(channel.login());
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(channel: &str, session: SessionId, kind: StreamEventKind) -> StreamEvent {
        StreamEvent {
            channel: channel.to_owned(),
            session,
            kind,
        }
    }

    fn running(states: &mut StreamStates) {
        states.begin("a", 1);
        assert!(states.apply(&event("a", 1, StreamEventKind::Started)));
    }

    #[test]
    fn a_launch_goes_starting_then_running() {
        let mut states = StreamStates::default();
        assert!(states.get("a").is_none());
        states.begin("a", 1);
        let state = states.get("a").unwrap();
        assert_eq!(state.phase, Phase::Starting);
        assert!(state.is_active());
        running(&mut states);
        assert_eq!(states.get("a").unwrap().phase, Phase::Running);
    }

    #[test]
    fn a_stop_ends_in_idle_without_an_error() {
        let mut states = StreamStates::default();
        running(&mut states);
        states.stopping("a");
        assert_eq!(states.get("a").unwrap().phase, Phase::Stopping);
        // A late `Started` must not flip a stopping stream back.
        assert!(!states.apply(&event("a", 1, StreamEventKind::Started)));
        assert!(states.apply(&event("a", 1, StreamEventKind::Exited { stopped: true })));
        assert!(states.get("a").is_none());
    }

    #[test]
    fn an_error_stays_after_the_process_exits_until_dismissed() {
        let mut states = StreamStates::default();
        running(&mut states);
        assert!(states.apply(&event(
            "a",
            1,
            StreamEventKind::Failed(StreamError::ChannelOffline)
        )));
        assert!(states.apply(&event("a", 1, StreamEventKind::Exited { stopped: false })));
        let state = states.get("a").unwrap();
        assert_eq!(state.phase, Phase::Ended);
        assert!(!state.is_active());
        assert_eq!(state.error, Some(StreamError::ChannelOffline));
        states.dismiss("a");
        assert!(states.get("a").is_none());
    }

    #[test]
    fn dismiss_leaves_an_active_stream_alone() {
        let mut states = StreamStates::default();
        running(&mut states);
        states.dismiss("a");
        assert!(states.get("a").is_some());
    }

    #[test]
    fn a_clean_exit_is_idle_again() {
        let mut states = StreamStates::default();
        running(&mut states);
        assert!(states.apply(&event("a", 1, StreamEventKind::Exited { stopped: false })));
        assert!(states.get("a").is_none());
    }

    #[test]
    fn events_of_an_old_session_or_a_closed_channel_are_ignored() {
        let mut states = StreamStates::default();
        states.begin("a", 2);
        let stale = event("a", 1, StreamEventKind::Exited { stopped: true });
        assert!(!states.apply(&stale));
        assert_eq!(states.get("a").unwrap().session, 2);
        states.remove("a");
        assert!(!states.apply(&event("a", 2, StreamEventKind::Started)));
    }

    #[test]
    fn a_new_launch_clears_the_previous_error() {
        let mut states = StreamStates::default();
        running(&mut states);
        states.apply(&event(
            "a",
            1,
            StreamEventKind::Failed(StreamError::NotFound { configured: None }),
        ));
        states.apply(&event("a", 1, StreamEventKind::Exited { stopped: false }));
        states.begin("a", 2);
        assert_eq!(states.get("a").unwrap().error, None);
    }

    #[test]
    fn the_same_error_twice_changes_nothing() {
        let mut states = StreamStates::default();
        states.begin("a", 1);
        let failed = event("a", 1, StreamEventKind::Failed(StreamError::ChannelOffline));
        assert!(states.apply(&failed));
        assert!(!states.apply(&failed));
    }

    #[test]
    fn settings_become_a_launch_spec() {
        let settings = StreamlinkSettings {
            path: Some(" /opt/x/streamlink ".into()),
            player: Some("mpv".into()),
            extra_args: vec!["--twitch-low-latency".into()],
            default_quality: "720p".into(),
        };
        let spec = launch_spec("forsen", "480p", &settings);
        assert_eq!(spec.channel, "forsen");
        assert_eq!(spec.quality, "480p");
        assert_eq!(
            spec.binary_override.as_deref(),
            Some(std::path::Path::new("/opt/x/streamlink"))
        );
        assert_eq!(spec.player.as_deref(), Some("mpv"));
        assert_eq!(spec.extra_args, ["--twitch-low-latency"]);

        let blank = StreamlinkSettings {
            path: Some("  ".into()),
            player: Some(String::new()),
            ..StreamlinkSettings::default()
        };
        let spec = launch_spec("forsen", "best", &blank);
        assert_eq!((spec.binary_override, spec.player), (None, None));
    }

    #[test]
    fn an_unusable_default_quality_falls_back_to_best() {
        let mut settings = StreamlinkSettings::default();
        assert_eq!(default_quality(&settings), "best");
        settings.default_quality = "720p60".into();
        assert_eq!(default_quality(&settings), "720p60");
        settings.default_quality = "--x".into();
        assert_eq!(default_quality(&settings), "best");
    }
}
