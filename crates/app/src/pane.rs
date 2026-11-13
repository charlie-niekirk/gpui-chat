//! One chat pane: a header (channel name, connection state, room-mode chips) over a chat view.
//!
//! The formatting rules live in plain functions ([`link_status`], [`room_chips`]) so they are
//! unit-tested; [`ChannelPane`] is the thin view on top. Splits arrive in PR 4.1; until then the
//! shell shows exactly one pane.

use std::time::Duration;

use gpui_chat_core::{ConnectionState, FollowersOnly, RoomState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, px, rgb,
};

use crate::channels::{ChannelHub, ChannelStore};
use crate::chat::ChatView;
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

/// The header and chat of one channel.
pub struct ChannelPane {
    hub: Entity<ChannelHub>,
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
        live: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let chat = cx.new(|cx| ChatView::new(store.clone(), cx));
        // Room and connection state arrive as store / hub changes; redraw the header with them.
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe(&hub, |_, _, cx| cx.notify()),
        ];
        Self {
            hub,
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

        h_flex()
            .id("pane-header")
            .w_full()
            .flex_none()
            .h(px(36.0))
            .px_3()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(tokens.border)
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::BOLD)
                    .child(store.channel().login().to_owned()),
            )
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
            // Reserved for the streamlink play button and quality menu (PR 1.6).
            .child(h_flex().id("pane-actions").flex_none().gap_1())
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
