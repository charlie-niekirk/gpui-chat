//! One chat pane: a header (channel name, connection state, room-mode chips) over a chat view and,
//! with the live source, the input box ([`crate::composer`]).
//!
//! The formatting rules live in plain functions ([`link_status`], [`room_chips`]) so they are
//! unit-tested; [`ChannelPane`] is the thin view on top. The shell lays panes out in splits
//! ([`crate::splits`]); a pane belongs to its channel, so its chat, input and streamlink button go
//! wherever the channel is shown. The channel name in the header is the handle to drag the pane by.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_chat_core::{ChannelId, ConnectionState, FollowersOnly, RoomState};
use gpui_chat_streamlink::QUALITIES;
use gpui_chat_twitch::streams::StreamInfo;
use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Action, App, AppContext as _, Context, Entity, EntityId, FocusHandle, Focusable, FontWeight,
    Hsla, InteractiveElement, IntoElement, ParentElement, Render, StatefulInteractiveElement as _,
    Styled, Subscription, Window, canvas, div, px,
};

use crate::actions::{ClosePane, SplitDown, SplitRight};
use crate::channels::{ChannelHub, ChannelStore};
use crate::chat::ChatView;
use crate::chat::selection_layer::{ClearSelection, CopySelection};
use crate::chat::view::ChatViewEvent;
use crate::composer::Composer;
use crate::splits::view::{DragChip, DraggedPane};
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

/// Below this header width (a narrow split) the header goes compact: no room-mode chips, the
/// connection state and LIVE marker as dots, the play button as its icon. The buttons on the
/// right never shrink; the channel name truncates.
const COMPACT_BELOW: f32 = 480.0;

fn status_color(status: &LinkStatus, tokens: &Tokens) -> Hsla {
    match status {
        LinkStatus::Connected => tokens.success,
        LinkStatus::Connecting | LinkStatus::Reconnecting { .. } => tokens.warning,
        LinkStatus::Error { .. } => tokens.danger,
    }
}

/// The "LIVE" / "Offline" marker next to the channel name.
fn live_badge(live: bool, compact: bool, tokens: &Tokens) -> impl IntoElement {
    let (color, label) = if live {
        (tokens.danger, "LIVE")
    } else {
        (tokens.text_muted.opacity(0.6), "Offline")
    };
    h_flex()
        .id("pane-live")
        .flex_none()
        .gap_1p5()
        .items_center()
        .child(div().flex_none().size(px(8.0)).rounded_full().bg(color))
        .children((!compact).then(|| {
            div()
                .text_xs()
                .font_weight(if live {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if live {
                    tokens.danger_text
                } else {
                    tokens.text_muted
                })
                .child(label)
        }))
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
}

/// The second header line of a live channel: title, category, viewers and uptime.
fn stream_row(info: &StreamInfo, compact: bool, tokens: &Tokens) -> impl IntoElement {
    let uptime = info
        .uptime(stream_info::unix_now())
        .map(stream_info::format_uptime);
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
        .overflow_x_hidden()
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
        .children((!compact && !info.category.is_empty()).then(|| {
            div()
                .flex_none()
                .max_w(px(220.0))
                .truncate()
                .text_xs()
                .text_color(tokens.accent_link)
                .child(info.category.clone())
        }))
        .child(fact(format!("{viewers} viewers")))
        .children(uptime.map(|uptime| fact(format!("up {uptime}"))))
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
    /// The input box; `None` for the offline sources (and when there is no login owner).
    composer: Option<Entity<Composer>>,
    /// `false` for the offline sources, which have no connection to report.
    live: bool,
    /// Keyboard focus for a pane without an input box (the offline sources).
    focus_handle: FocusHandle,
    /// Whether the window is split, so the pane can be closed.
    in_split: bool,
    /// The header's width at the last paint (narrow splits get a compact header).
    header_width: Rc<Cell<f32>>,
    _subscriptions: Vec<Subscription>,
}

impl ChannelPane {
    pub fn new(
        hub: Entity<ChannelHub>,
        store: Entity<ChannelStore>,
        streams: Entity<StreamHub>,
        stream_info: Option<Entity<StreamInfoHub>>,
        live: bool,
        composer: Option<Entity<Composer>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let chat = cx.new(|cx| ChatView::new(store.clone(), cx));
        // Reply on a row puts this pane's composer (when it has one) in reply mode.
        chat.update(cx, |chat, cx| chat.set_can_reply(composer.is_some(), cx));
        // Room and connection state arrive as store / hub changes; redraw the header with them.
        let mut subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe(&hub, |_, _, cx| cx.notify()),
            cx.observe(&streams, |_, _, cx| cx.notify()),
        ];
        if let Some(stream_info) = &stream_info {
            subscriptions.push(cx.observe(stream_info, |_, _, cx| cx.notify()));
        }
        subscriptions.push(
            cx.subscribe(&chat, |this, _, event: &ChatViewEvent, cx| match event {
                ChatViewEvent::ReplyRequested(message) => {
                    if let Some(composer) = &this.composer {
                        composer
                            .update(cx, |composer, cx| composer.queue_reply(message.clone(), cx));
                    }
                }
            }),
        );
        Self {
            hub,
            streams,
            stream_info,
            store,
            chat,
            composer,
            live,
            focus_handle: cx.focus_handle(),
            in_split: false,
            header_width: Rc::new(Cell::new(f32::INFINITY)),
            _subscriptions: subscriptions,
        }
    }

    /// Gives the pane keyboard focus: its input box, or the pane itself when it has none.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self
            .composer
            .as_ref()
            .is_some_and(|composer| composer.update(cx, |composer, cx| composer.focus(window, cx)));
        if !typed {
            window.focus(&self.focus_handle, cx);
        }
    }

    pub fn set_in_split(&mut self, in_split: bool, cx: &mut Context<Self>) {
        if self.in_split != in_split {
            self.in_split = in_split;
            cx.notify();
        }
    }

    /// Split right / split down / close: they act on the focused pane, which a press in this
    /// pane has just made this one.
    fn pane_actions(&self, tokens: &Tokens) -> impl IntoElement {
        let button =
            |id: &'static str, icon: IconName, label: &'static str, action: Box<dyn Action>| {
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(icon))
                    .tooltip(label)
                    .accessibility_label(label)
                    .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
            };
        h_flex()
            .id("pane-split-actions")
            .flex_none()
            .gap_0p5()
            .items_center()
            .pl_1p5()
            .border_l_1()
            .border_color(tokens.border)
            .child(button(
                "pane-split-right",
                IconName::PanelRight,
                "Split right",
                Box::new(SplitRight),
            ))
            .child(button(
                "pane-split-down",
                IconName::PanelBottom,
                "Split down",
                Box::new(SplitDown),
            ))
            .children(self.in_split.then(|| {
                button(
                    "pane-close",
                    IconName::Close,
                    "Close pane",
                    Box::new(ClosePane),
                )
            }))
    }

    /// `compact` is what the last measured width asked for; the measuring canvas asks for a
    /// redraw of `pane` when the width crosses [`COMPACT_BELOW`].
    fn header(&self, tokens: &Tokens, compact: bool, pane: EntityId, cx: &App) -> impl IntoElement {
        let store = self.store.read(cx);
        let status = self
            .live
            .then(|| link_status(self.hub.read(cx).connection(), store.connection()));
        let chips = room_chips(store.room());

        let status_view = match status {
            Some(status) => {
                let color = status_color(&status, tokens);
                let label = status.label();
                h_flex()
                    .id("pane-status")
                    .gap_1p5()
                    .items_center()
                    .flex_shrink(1.0)
                    .min_w_0()
                    .child(div().flex_none().size(px(8.0)).rounded_full().bg(color))
                    .children((!compact).then(|| {
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(tokens.text_muted)
                            .child(label.clone())
                    }))
                    .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
                    .into_any_element()
            }
            None => div()
                .id("pane-status")
                .min_w_0()
                .truncate()
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
            Some(Liveness::Live(_)) => Some(live_badge(true, compact, tokens)),
            Some(Liveness::Offline) => Some(live_badge(false, compact, tokens)),
            _ => None,
        };
        let stream_row = match liveness {
            Some(Liveness::Live(info)) => Some(stream_row(info, compact, tokens)),
            _ => None,
        };

        // Measures the row; crossing the compact width redraws the pane after this frame.
        let measure = canvas(
            move |bounds, window, _| {
                if (bounds.size.width.as_f32() < COMPACT_BELOW) != compact {
                    window.on_next_frame(move |_, cx| cx.notify(pane));
                }
                bounds.size.width.as_f32()
            },
            {
                let width = self.header_width.clone();
                move |_, measured, _, _| width.set(measured)
            },
        )
        .absolute()
        .inset_0();

        // Name, LIVE, connection state and chips give way (truncate, then clip); the buttons
        // on the right never shrink.
        let left = h_flex()
            .id("pane-header-left")
            .flex_1()
            .min_w_0()
            .overflow_x_hidden()
            .gap_3()
            .items_center()
            .child({
                let login = store.channel().login().to_owned();
                div()
                    .id("pane-drag-handle")
                    .min_w_0()
                    .flex_shrink(1.0)
                    .truncate()
                    .font_weight(FontWeight::BOLD)
                    .cursor_grab()
                    .child(login.clone())
                    .on_drag(
                        DraggedPane {
                            channel: store.channel().clone(),
                        },
                        move |_, _, _, cx| DragChip::build(login.clone(), cx),
                    )
            })
            .children(live_badge)
            .child(status_view)
            .children((!compact).then(|| {
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
                    }))
            }));
        let right = h_flex()
            .id("pane-header-right")
            .flex_none()
            .gap_3()
            .items_center()
            .child(self.stream_actions(tokens, compact, cx))
            .child(self.pane_actions(tokens));
        let main_row = h_flex()
            .id("pane-header-main")
            .relative()
            .w_full()
            .flex_none()
            .overflow_x_hidden()
            .h(px(36.0))
            .px_3()
            .gap_3()
            .items_center()
            .child(measure)
            .child(left)
            .child(right);
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
    fn stream_actions(&self, tokens: &Tokens, compact: bool, cx: &App) -> impl IntoElement {
        let actions = h_flex()
            .id("pane-actions")
            .flex_none()
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
                .max_w(px(if compact { 120.0 } else { 420.0 }))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(tokens.danger_text)
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
            let label = play_label(state, &quality);
            // Compact: the icon alone, with the label in the tooltip.
            let tooltip = match (compact, active) {
                (true, _) => label.clone(),
                (false, true) => "Stop the stream".to_owned(),
                (false, false) => "Watch with streamlink".to_owned(),
            };
            let button = Button::new("stream-toggle")
                .small()
                .icon(Icon::new(if active {
                    IconName::Square
                } else {
                    IconName::Play
                }))
                .accessibility_label(label.clone());
            let button = if compact { button } else { button.label(label) };
            button
                .tooltip(tooltip)
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
            .id("channel-pane")
            .track_focus(&self.focus_handle)
            // The chat's selection (PR 5.6). Here rather than on the chat, so the command palette
            // reaches it with the input box focused too. Esc only acts when there is something to
            // clear (then focus goes back to the input box); otherwise it goes on.
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                if !this.chat.update(cx, |chat, cx| chat.copy_selection(cx)) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, window, cx| {
                if this.chat.update(cx, |chat, cx| chat.clear_selection(cx)) {
                    this.focus(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .size_full()
            .child(self.header(
                &tokens,
                self.header_width.get() < COMPACT_BELOW,
                cx.entity_id(),
                cx,
            ))
            .child(div().flex_1().min_h_0().w_full().child(self.chat.clone()))
            .children(self.composer.clone())
    }
}

impl Focusable for ChannelPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
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
