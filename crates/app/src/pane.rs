//! One chat pane: a header (channel name, connection state, room-mode chips) over a chat view.
//!
//! The formatting rules live in plain functions ([`link_status`], [`room_chips`]) so they are
//! unit-tested; [`ChannelPane`] is the thin view on top. Splits arrive in PR 4.1; until then the
//! shell shows exactly one pane.

use std::time::Duration;

use gpui_chat_core::{ChannelId, ConnectionState, FollowersOnly, RoomState};
use gpui_chat_streamlink::QUALITIES;
use gpui_chat_twitch::streams::StreamInfo;
use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement as _, Styled, Subscription, Window, div, px,
    rgb,
};

use crate::channels::{ChannelHub, ChannelStore};
use crate::chat::ChatView;
use crate::stream_info::{self, Liveness, StreamInfoHub};
use crate::streams::{Phase, StreamHub, StreamState};
use crate::theme::Tokens;

/// What the header says about the connection to one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkStatus {
    Connecting,
    Connected,
    Reconnecting {
        attempt: u32,
    },
    /// The connection or the join ended; `reason` is the client's, when it gave one.
    Error {
        reason: Option<String>,
    },
}

impl LinkStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "Connecting".to_owned(),
            Self::Connected => "Connected".to_owned(),
            Self::Reconnecting { attempt } => format!("Reconnecting (attempt {attempt})"),
            Self::Error { reason: None } => "Disconnected".to_owned(),
            Self::Error {
                reason: Some(reason),
            } => format!("Error: {reason}"),
        }
    }
}

/// Combines the state of the whole connection with the join state of one channel: a connection
/// that is down (or still being set up) says so for every channel; once it is up, the channel's
/// own join decides.
pub fn link_status(
    connection: Option<&ConnectionState>,
    channel: Option<&ConnectionState>,
) -> LinkStatus {
    let from = |state: &ConnectionState| match state {
        ConnectionState::Connecting => LinkStatus::Connecting,
        ConnectionState::Connected => LinkStatus::Connected,
        ConnectionState::Reconnecting { attempt } => LinkStatus::Reconnecting { attempt: *attempt },
        ConnectionState::Disconnected { reason } => LinkStatus::Error {
            reason: reason.clone(),
        },
    };
    match connection {
        None | Some(ConnectionState::Connecting) => LinkStatus::Connecting,
        Some(ConnectionState::Connected) => channel.map_or(LinkStatus::Connecting, from),
        Some(other) => from(other),
    }
}

/// `90` → `1h 30m`, `1440` → `1d`: at most the two largest non-zero units.
pub fn format_minutes(minutes: u32) -> String {
    let (days, hours, mins) = (minutes / 1440, minutes % 1440 / 60, minutes % 60);
    let parts: Vec<String> = [(days, 'd'), (hours, 'h'), (mins, 'm')]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .take(2)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect();
    if parts.is_empty() {
        "0m".to_owned()
    } else {
        parts.join(" ")
    }
}

/// One chip per room mode that is on, in a fixed order.
pub fn room_chips(room: &RoomState) -> Vec<String> {
    let mut chips = Vec::new();
    if room.emote_only {
        chips.push("Emote-only".to_owned());
    }
    match room.followers_only {
        FollowersOnly::Off => {}
        FollowersOnly::All | FollowersOnly::Minutes(0) => chips.push("Followers-only".to_owned()),
        FollowersOnly::Minutes(minutes) => {
            chips.push(format!("Followers-only {}", format_minutes(minutes)));
        }
    }
    if room.slow_mode > Duration::ZERO {
        chips.push(format!("Slow {}s", room.slow_mode.as_secs()));
    }
    if room.subscribers_only {
        chips.push("Subs-only".to_owned());
    }
    if room.unique_chat {
        chips.push("Unique".to_owned());
    }
    chips
}

const OK: u32 = 0x2fbf71;
const WARN: u32 = 0xe0a030;

fn status_color(status: &LinkStatus, tokens: &Tokens) -> Hsla {
    match status {
        LinkStatus::Connected => rgb(OK).into(),
        LinkStatus::Connecting | LinkStatus::Reconnecting { .. } => rgb(WARN).into(),
        LinkStatus::Error { .. } => tokens.danger,
    }
}

/// The "LIVE" / "Offline" marker next to the channel name.
fn live_badge(live: bool, tokens: &Tokens) -> impl IntoElement {
    let (color, label) = if live {
        (stream_info::live_color(), "LIVE")
    } else {
        (tokens.text_muted.opacity(0.6), "Offline")
    };
    h_flex()
        .id("pane-live")
        .flex_none()
        .gap_1p5()
        .items_center()
        .child(div().size(px(8.0)).rounded_full().bg(color))
        .child(
            div()
                .text_xs()
                .font_weight(if live {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if live { color } else { tokens.text_muted })
                .child(label),
        )
}

/// The second header line of a live channel: title, category, viewers and uptime.
fn stream_row(info: &StreamInfo, tokens: &Tokens) -> impl IntoElement {
    let uptime = stream_info::format_uptime(info.uptime(stream_info::unix_now()));
    let viewers = stream_info::format_viewers(info.viewers);
    let title = if info.title.is_empty() {
        "(no title)".to_owned()
    } else {
        info.title.clone()
    };
    let full = if info.category.is_empty() {
        title.clone()
    } else {
        format!("{title}\n{}", info.category)
    };
    let fact = |text: String| {
        div()
            .flex_none()
            .text_xs()
            .text_color(tokens.text_muted)
            .child(text)
    };
    h_flex()
        .id("pane-stream-info")
        .w_full()
        .flex_none()
        .h(px(26.0))
        .px_3()
        .gap_3()
        .items_center()
        .child(
            div()
                .id("pane-stream-title")
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .child(title)
                .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx)),
        )
        .children((!info.category.is_empty()).then(|| {
            div()
                .flex_none()
                .max_w(px(220.0))
                .truncate()
                .text_xs()
                .text_color(tokens.accent)
                .child(info.category.clone())
        }))
        .child(fact(format!("{viewers} viewers")))
        .child(fact(format!("up {uptime}")))
}

/// The quality menu: the standard list, plus the channel's current choice when it is not in it
/// (a custom `default_quality` such as `720p60,720p,best`).
pub fn quality_choices(current: &str) -> Vec<String> {
    let mut choices: Vec<String> = QUALITIES.iter().map(|q| (*q).to_owned()).collect();
    if !choices.iter().any(|q| q == current) {
        choices.insert(0, current.to_owned());
    }
    choices
}

/// What the play button says for a stream state (`None`: idle).
pub fn play_label(state: Option<&StreamState>, quality: &str) -> String {
    match state.map(|s| s.phase) {
        Some(Phase::Starting) => "Starting…".to_owned(),
        Some(Phase::Running) => "Stop".to_owned(),
        Some(Phase::Stopping) => "Stopping…".to_owned(),
        Some(Phase::Ended) | None => format!("Play {quality}"),
    }
}

/// The header and chat of one channel.
pub struct ChannelPane {
    hub: Entity<ChannelHub>,
    streams: Entity<StreamHub>,
    /// Live state from Helix; `None` for the offline sources.
    stream_info: Option<Entity<StreamInfoHub>>,
    store: Entity<ChannelStore>,
    chat: Entity<ChatView>,
    /// `false` for the offline sources, which have no connection to report.
    live: bool,
    _subscriptions: Vec<Subscription>,
}

impl ChannelPane {
    pub fn new(
        hub: Entity<ChannelHub>,
        store: Entity<ChannelStore>,
        streams: Entity<StreamHub>,
        stream_info: Option<Entity<StreamInfoHub>>,
        live: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let chat = cx.new(|cx| ChatView::new(store.clone(), cx));
        // Room and connection state arrive as store / hub changes; redraw the header with them.
        let mut subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe(&hub, |_, _, cx| cx.notify()),
            cx.observe(&streams, |_, _, cx| cx.notify()),
        ];
        if let Some(stream_info) = &stream_info {
            subscriptions.push(cx.observe(stream_info, |_, _, cx| cx.notify()));
        }
        Self {
            hub,
            streams,
            stream_info,
            store,
            chat,
            live,
            _subscriptions: subscriptions,
        }
    }

    fn header(&self, tokens: &Tokens, cx: &App) -> impl IntoElement {
        let store = self.store.read(cx);
        let status = self
            .live
            .then(|| link_status(self.hub.read(cx).connection(), store.connection()));
        let chips = room_chips(store.room());

        let status_view = match status {
            Some(status) => {
                let color = status_color(&status, tokens);
                h_flex()
                    .id("pane-status")
                    .gap_1p5()
                    .items_center()
                    .flex_none()
                    .child(div().size(px(8.0)).rounded_full().bg(color))
                    .child(
                        div()
                            .text_xs()
                            .text_color(tokens.text_muted)
                            .child(status.label()),
                    )
                    .into_any_element()
            }
            None => div()
                .id("pane-status")
                .text_xs()
                .text_color(tokens.text_muted)
                .child("Offline source")
                .into_any_element(),
        };

        let liveness = self
            .stream_info
            .as_ref()
            .map(|hub| hub.read(cx).liveness(store.channel()));
        let live_badge = match liveness {
            Some(Liveness::Live(_)) => Some(live_badge(true, tokens)),
            Some(Liveness::Offline) => Some(live_badge(false, tokens)),
            _ => None,
        };
        let stream_row = match liveness {
            Some(Liveness::Live(info)) => Some(stream_row(info, tokens)),
            _ => None,
        };

        let main_row = h_flex()
            .id("pane-header-main")
            .w_full()
            .flex_none()
            .h(px(36.0))
            .px_3()
            .gap_3()
            .items_center()
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::BOLD)
                    .child(store.channel().login().to_owned()),
            )
            .children(live_badge)
            .child(status_view)
            .child(
                h_flex()
                    .id("pane-chips")
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .gap_1p5()
                    .children(chips.into_iter().map(|chip| {
                        div()
                            .flex_none()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(tokens.border)
                            .text_xs()
                            .text_color(tokens.text_muted)
                            .child(chip)
                    })),
            )
            .child(self.stream_actions(tokens, cx));
        v_flex()
            .id("pane-header")
            .w_full()
            .flex_none()
            .border_b_1()
            .border_color(tokens.border)
            .child(main_row)
            .children(stream_row)
    }

    /// Streamlink error (when there is one) and the play / stop button with its quality menu.
    /// Offline sources have no real channels, so they get nothing.
    fn stream_actions(&self, tokens: &Tokens, cx: &App) -> impl IntoElement {
        let actions = h_flex()
            .id("pane-actions")
            .flex_none()
            .min_w_0()
            .gap_1p5()
            .items_center();
        if !self.live {
            return actions;
        }
        let channel = self.store.read(cx).channel().clone();
        let hub = self.streams.read(cx);
        let state = hub.state(&channel);
        let quality = hub.quality(&channel, cx);
        let active = state.is_some_and(StreamState::is_active);
        let stopping = state.is_some_and(|s| s.phase == Phase::Stopping);

        let notice = state.and_then(|s| s.error.as_ref()).map(|error| {
            let message = error.to_string();
            let streams = self.streams.clone();
            let target = channel.clone();
            h_flex()
                .id("stream-error")
                .gap_1()
                .items_center()
                .min_w_0()
                .max_w(px(420.0))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(tokens.danger)
                        .child(message),
                )
                .child(
                    Button::new("stream-error-dismiss")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Close))
                        .accessibility_label("Dismiss streamlink error")
                        .on_click(move |_, _, cx| {
                            streams.update(cx, |hub, cx| hub.dismiss(&target, cx));
                        }),
                )
        });

        let toggle = {
            let streams = self.streams.clone();
            let target = channel.clone();
            Button::new("stream-toggle")
                .small()
                .icon(Icon::new(if active {
                    IconName::Square
                } else {
                    IconName::Play
                }))
                .label(play_label(state, &quality))
                .tooltip(if active {
                    "Stop the stream"
                } else {
                    "Watch with streamlink"
                })
                .disabled(stopping)
                .on_click(move |_, _, cx| {
                    streams.update(cx, |hub, cx| hub.toggle(&target, cx));
                })
        };
        let menu = {
            let streams = self.streams.clone();
            let target = channel;
            let current = quality;
            move |mut menu: gpui_kit::component::menu::PopupMenu,
                  _: &mut Window,
                  _: &mut Context<_>| {
                for choice in quality_choices(&current) {
                    let streams = streams.clone();
                    let target: ChannelId = target.clone();
                    let picked = choice.clone();
                    menu = menu.item(
                        PopupMenuItem::new(choice.clone())
                            .checked(choice == current)
                            .on_click(move |_, _, cx| {
                                streams.update(cx, |hub, cx| hub.play(&target, &picked, cx));
                            }),
                    );
                }
                menu
            }
        };
        actions.children(notice).child(
            DropdownButton::new("stream-play")
                .button(toggle)
                .dropdown_menu(menu)
                .menu_accessibility_label("Stream quality"),
        )
    }
}

impl Render for ChannelPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        v_flex()
            .size_full()
            .child(self.header(&tokens, cx))
            .child(div().flex_1().min_h_0().w_full().child(self.chat.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(f: impl FnOnce(&mut RoomState)) -> RoomState {
        let mut room = RoomState::default();
        f(&mut room);
        room
    }

    #[test]
    fn no_modes_no_chips() {
        assert!(room_chips(&RoomState::default()).is_empty());
        // The room id alone is not a mode.
        assert!(room_chips(&room(|r| r.room_id = Some("1".into()))).is_empty());
    }

    #[test]
    fn every_mode_has_a_chip_in_a_fixed_order() {
        let all = room(|r| {
            r.unique_chat = true;
            r.subscribers_only = true;
            r.slow_mode = Duration::from_secs(30);
            r.followers_only = FollowersOnly::Minutes(10);
            r.emote_only = true;
        });
        assert_eq!(
            room_chips(&all),
            [
                "Emote-only",
                "Followers-only 10m",
                "Slow 30s",
                "Subs-only",
                "Unique"
            ]
        );
    }

    #[test]
    fn followers_only_forms() {
        let chip = |f| room_chips(&room(|r| r.followers_only = f));
        assert!(chip(FollowersOnly::Off).is_empty());
        assert_eq!(chip(FollowersOnly::All), ["Followers-only"]);
        assert_eq!(chip(FollowersOnly::Minutes(0)), ["Followers-only"]);
        assert_eq!(chip(FollowersOnly::Minutes(30)), ["Followers-only 30m"]);
        assert_eq!(chip(FollowersOnly::Minutes(60)), ["Followers-only 1h"]);
    }

    #[test]
    fn slow_mode_zero_is_off() {
        assert!(room_chips(&room(|r| r.slow_mode = Duration::ZERO)).is_empty());
        assert_eq!(
            room_chips(&room(|r| r.slow_mode = Duration::from_secs(120))),
            ["Slow 120s"]
        );
    }

    #[test]
    fn durations_show_two_units_at_most() {
        assert_eq!(format_minutes(0), "0m");
        assert_eq!(format_minutes(1), "1m");
        assert_eq!(format_minutes(59), "59m");
        assert_eq!(format_minutes(90), "1h 30m");
        assert_eq!(format_minutes(1440), "1d");
        assert_eq!(format_minutes(1500), "1d 1h");
        assert_eq!(format_minutes(1441), "1d 1m");
        assert_eq!(format_minutes(129_600), "90d");
    }

    #[test]
    fn a_connection_that_is_not_up_decides_for_every_channel() {
        let joined = Some(ConnectionState::Connected);
        assert_eq!(link_status(None, joined.as_ref()), LinkStatus::Connecting);
        assert_eq!(
            link_status(Some(&ConnectionState::Connecting), joined.as_ref()),
            LinkStatus::Connecting
        );
        assert_eq!(
            link_status(
                Some(&ConnectionState::Reconnecting { attempt: 3 }),
                joined.as_ref()
            ),
            LinkStatus::Reconnecting { attempt: 3 }
        );
        assert_eq!(
            link_status(
                Some(&ConnectionState::Disconnected {
                    reason: Some("boom".into())
                }),
                joined.as_ref()
            ),
            LinkStatus::Error {
                reason: Some("boom".into())
            }
        );
    }

    #[test]
    fn once_connected_the_channel_join_decides() {
        let up = ConnectionState::Connected;
        assert_eq!(link_status(Some(&up), None), LinkStatus::Connecting);
        assert_eq!(
            link_status(Some(&up), Some(&ConnectionState::Connecting)),
            LinkStatus::Connecting
        );
        assert_eq!(
            link_status(Some(&up), Some(&ConnectionState::Connected)),
            LinkStatus::Connected
        );
        assert_eq!(
            link_status(
                Some(&up),
                Some(&ConnectionState::Disconnected { reason: None })
            ),
            LinkStatus::Error { reason: None }
        );
    }

    #[test]
    fn the_quality_menu_lists_a_custom_default_first() {
        let standard = quality_choices("720p60");
        assert_eq!(standard.first().map(String::as_str), Some("best"));
        assert_eq!(standard.len(), QUALITIES.len());
        let custom = quality_choices("720p60,720p,best");
        assert_eq!(custom[0], "720p60,720p,best");
        assert_eq!(custom.len(), QUALITIES.len() + 1);
    }

    #[test]
    fn the_play_button_follows_the_stream_phase() {
        let state = |phase| StreamState {
            session: 1,
            phase,
            error: None,
        };
        assert_eq!(play_label(None, "best"), "Play best");
        assert_eq!(
            play_label(Some(&state(Phase::Starting)), "best"),
            "Starting…"
        );
        assert_eq!(play_label(Some(&state(Phase::Running)), "best"), "Stop");
        assert_eq!(
            play_label(Some(&state(Phase::Stopping)), "best"),
            "Stopping…"
        );
        // An ended stream (error showing) can be played again.
        assert_eq!(play_label(Some(&state(Phase::Ended)), "720p"), "Play 720p");
    }

    #[test]
    fn labels() {
        assert_eq!(LinkStatus::Connecting.label(), "Connecting");
        assert_eq!(LinkStatus::Connected.label(), "Connected");
        assert_eq!(
            LinkStatus::Reconnecting { attempt: 2 }.label(),
            "Reconnecting (attempt 2)"
        );
        assert_eq!(LinkStatus::Error { reason: None }.label(), "Disconnected");
        assert_eq!(
            LinkStatus::Error {
                reason: Some("tls".into())
            }
            .label(),
            "Error: tls"
        );
    }
}
