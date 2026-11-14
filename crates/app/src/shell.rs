//! The root view: custom title bar, collapsible channel sidebar and the main area (one pane).
//!
//! The shell owns the [`ChannelList`] (what is open, in which order, which is selected). Every
//! change to it goes through [`AppShell::list_changed`], which makes sure the selected channel
//! has a pane, joins/parts on the live client and schedules a write of `state.toml`.
//!
//! The title bar's right end is the account control (PR 3.1): "Log in" when logged out, the user's
//! name with a menu (log in again, log out) when logged in. A strip above the content says why the
//! user was logged out, or that the login needs new permissions.

use std::collections::HashMap;
use std::time::Duration;

use gpui_chat_core::{APP_ID, APP_NAME, ChannelId};
use gpui_chat_settings::SettingsError;
use gpui_chat_twitch::auth::AuthState;
use gpui_chat_twitch::streams::StreamInfo;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
    SidebarToggleButton,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Anchor, AnyElement, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable,
    FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, WindowBounds, div,
    px, size,
};

use crate::account::AccountHub;
use crate::actions::{
    About, AddChannel, CloseWindow, LogIn, LogOut, MinimizeWindow, MoveChannelDown, MoveChannelUp,
    ReloadEmotes, RemoveChannel, SHELL_CONTEXT, SelectNextChannel, SelectPreviousChannel,
    ToggleDebugHud, ToggleSidebar,
};
use crate::add_channel;
use crate::channel_list::{ChannelList, Move};
use crate::channels::ChannelHub;
use crate::config;
use crate::diagnostics::{Diagnostics, frame_probe};
use crate::emote_service;
use crate::fonts::FontConfig;
use crate::hud::DebugHud;
use crate::live::LiveSource;
use crate::pane::ChannelPane;
use crate::platform;
use crate::sign_in;
use crate::state::{self, Mode, SavedChannels, WindowPlacement};
use crate::stream_info::{self, Liveness, StreamInfoHub};
use crate::streams::StreamHub;
use crate::theme::{self, Tokens};

const SIDEBAR_WIDTH: Pixels = px(240.0);
/// Disk writes while dragging/resizing are coalesced into one write after this pause.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(750);
/// Channel list edits are rare, so they are written almost at once.
const CHANNELS_SAVE_DEBOUNCE: Duration = Duration::from_millis(200);

pub const DEFAULT_SIZE: (f32, f32) = (1100.0, 700.0);
pub const MIN_SIZE: (f32, f32) = (640.0, 400.0);

/// Latest window placement seen by the shell, flushed to disk on quit.
#[derive(Clone)]
struct LatestPlacement(WindowPlacement);
impl gpui_kit::Global for LatestPlacement {}

pub fn placement_from_bounds(bounds: WindowBounds, display: Option<String>) -> WindowPlacement {
    let (rect, mode) = match bounds {
        WindowBounds::Windowed(rect) => (rect, Mode::Windowed),
        WindowBounds::Maximized(rect) => (rect, Mode::Maximized),
        WindowBounds::Fullscreen(rect) => (rect, Mode::Fullscreen),
    };
    WindowPlacement {
        x: rect.origin.x.as_f32(),
        y: rect.origin.y.as_f32(),
        width: rect.size.width.as_f32(),
        height: rect.size.height.as_f32(),
        mode,
        display,
    }
}

/// Bounds are relative to the placement's display; the caller selects that display.
pub fn bounds_from_placement(placement: &WindowPlacement) -> WindowBounds {
    let rect = Bounds::new(
        gpui_kit::point(px(placement.x), px(placement.y)),
        size(
            px(placement.width.max(MIN_SIZE.0)),
            px(placement.height.max(MIN_SIZE.1)),
        ),
    );
    match placement.mode {
        Mode::Windowed => WindowBounds::Windowed(rect),
        Mode::Maximized => WindowBounds::Maximized(rect),
        Mode::Fullscreen => WindowBounds::Fullscreen(rect),
    }
}

/// Latest channel list, flushed to disk on quit. Only set when the list is persisted.
#[derive(Clone)]
struct LatestChannels(SavedChannels);
impl gpui_kit::Global for LatestChannels {}

/// Writes the most recent placement and channel list to disk, if there are any.
pub fn flush_state(cx: &App) {
    if let Some(LatestPlacement(placement)) = cx.try_global::<LatestPlacement>() {
        state::save_window(placement.clone());
    }
    if let Some(LatestChannels(channels)) = cx.try_global::<LatestChannels>() {
        state::save_channels(channels.clone());
    }
}

/// What the shell shows besides its own chrome: the channel stores (fed by the bridge), the
/// source that feeds them and the measurements behind the debug HUD.
pub struct ShellParts {
    pub hub: Entity<ChannelHub>,
    /// The channels to open at launch (from `state.toml`); empty for the offline sources.
    pub channels: ChannelList,
    /// The live client; `None` with `--firehose` / `--replay`, which then also keep the channel
    /// list out of `state.toml`.
    pub live: Option<LiveSource>,
    /// The Twitch account; `None` with the offline sources (they never touch the keychain).
    pub account: Option<Entity<AccountHub>>,
    pub diagnostics: Diagnostics,
    pub hud_visible: bool,
}

pub struct AppShell {
    focus_handle: FocusHandle,
    sidebar_collapsed: bool,
    diagnostics: Diagnostics,
    hub: Entity<ChannelHub>,
    hud: Entity<DebugHud>,
    list: ChannelList,
    live: Option<LiveSource>,
    account: Option<Entity<AccountHub>>,
    /// Live state of the open and the followed channels (PR 3.5); live source only, polls while
    /// logged in.
    stream_info: Option<Entity<StreamInfoHub>>,
    /// Streamlink processes and their play state; ended on quit and when a channel is closed.
    streams: Entity<StreamHub>,
    /// Panes are created when a channel is first selected and kept (with their scroll position)
    /// until it is closed.
    panes: HashMap<ChannelId, Entity<ChannelPane>>,
    save_task: Option<Task<()>>,
    channels_save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AppShell {
    pub fn new(window: &mut Window, parts: ShellParts, cx: &mut Context<Self>) -> Self {
        let ShellParts {
            hub,
            channels,
            live,
            account,
            diagnostics,
            hud_visible,
        } = parts;
        let hud = cx.new(|cx| DebugHud::new(diagnostics.clone(), hub.clone(), hud_visible, cx));
        let mut subscriptions = vec![
            // With an offline source, channels appear through their events.
            cx.observe(&hub, |this, _, cx| this.hub_changed(cx)),
            // Follow the system light/dark setting live, but only when `theme = "system"`.
            cx.observe_window_appearance(window, |_, window, cx| {
                theme::system_appearance_changed(window, cx);
            }),
            // A settings change (or a load error appearing/clearing) applies live: font,
            // size and theme are pushed into the global font config and Kit theme, which
            // refreshes every window, but only when the appearance section actually changed;
            // the notify re-renders the banner.
            cx.observe_in(&config::store(cx), window, |this, store, window, cx| {
                let settings = store.read(cx).settings();
                let (appearance, emotes) = (settings.appearance.clone(), settings.emotes.clone());
                theme::apply_settings_if_changed(&appearance, Some(window), cx);
                // The provider switches are idempotent: nothing happens unless one changed.
                if let Some(live) = &this.live {
                    live.set_emote_providers(emote_service::providers(&emotes));
                }
                cx.notify();
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.placement_changed(window, cx);
            }),
        ];
        let stream_info = match &account {
            Some(account) => {
                let helix = account.read(cx).helix().clone();
                Some(StreamInfoHub::start(helix, cx))
            }
            // Offline sources have no login; the demo variable fakes the data to look at.
            None if std::env::var_os(stream_info::DEMO_ENV).is_some() => {
                Some(StreamInfoHub::demo(cx))
            }
            None => None,
        };
        if let Some(account) = &account {
            subscriptions.push(cx.observe(account, |_, _, cx| cx.notify()));
        }
        if let Some(stream_info) = &stream_info {
            subscriptions.push(cx.observe(stream_info, |_, _, cx| cx.notify()));
        }
        window.set_window_title(APP_NAME);
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let mut shell = Self {
            focus_handle,
            sidebar_collapsed: false,
            diagnostics,
            hub,
            hud,
            list: channels,
            live,
            account,
            stream_info,
            streams: StreamHub::start(window, cx),
            panes: HashMap::new(),
            save_task: None,
            channels_save_task: None,
            _subscriptions: subscriptions,
        };
        // Restored channels get their stores and are joined in sidebar order.
        for channel in shell.list.channels().to_vec() {
            shell.hub.update(cx, |hub, cx| hub.ensure(&channel, cx));
            if let Some(live) = &shell.live {
                live.join(&channel);
            }
        }
        shell.hub_changed(cx);
        shell.ensure_selected_pane(cx);
        shell.watch_open_channels(cx);
        shell
    }

    /// Tells the stream poller which channels are open.
    fn watch_open_channels(&self, cx: &mut App) {
        if let Some(stream_info) = &self.stream_info {
            let channels = self.list.channels().to_vec();
            stream_info.update(cx, |hub, cx| hub.set_channels(channels, cx));
        }
    }

    /// Whether the channel list is the user's (live source) and so persisted.
    fn persists(&self) -> bool {
        self.live.is_some()
    }

    /// Offline sources create channels through their events: list each as it appears.
    fn hub_changed(&mut self, cx: &mut Context<Self>) {
        if self.live.is_some() {
            return;
        }
        let appeared: Vec<ChannelId> = self
            .hub
            .read(cx)
            .channels()
            .iter()
            .filter(|channel| !self.list.contains(channel))
            .cloned()
            .collect();
        if appeared.is_empty() {
            return;
        }
        for channel in appeared {
            self.list.append(channel);
        }
        if self.list.selected().is_none() {
            self.list.select_first();
        }
        self.ensure_selected_pane(cx);
        self.watch_open_channels(cx);
        cx.notify();
    }

    /// Creates the pane of the selected channel if it has none yet.
    fn ensure_selected_pane(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.list.selected().cloned() else {
            return;
        };
        if self.panes.contains_key(&channel) {
            return;
        }
        let hub = self.hub.clone();
        let store = hub.update(cx, |hub, cx| hub.ensure(&channel, cx));
        let live = self.live.is_some();
        let streams = self.streams.clone();
        let stream_info = self.stream_info.clone();
        let pane = cx.new(|cx| ChannelPane::new(hub, store, streams, stream_info, live, cx));
        self.panes.insert(channel, pane);
    }

    /// Every edit of the channel list ends here.
    fn list_changed(&mut self, cx: &mut Context<Self>) {
        self.ensure_selected_pane(cx);
        self.watch_open_channels(cx);
        if self.persists() {
            let saved = self.list.to_saved();
            cx.set_global(LatestChannels(saved.clone()));
            // Replacing the task drops (cancels) a write that has not started yet.
            self.channels_save_task = Some(cx.spawn(async move |_, cx| {
                cx.background_executor().timer(CHANNELS_SAVE_DEBOUNCE).await;
                cx.background_executor()
                    .spawn(async move { state::save_channels(saved) })
                    .await;
            }));
        }
        cx.notify();
    }

    fn add_channel(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        let newly_added = self.list.add(channel.clone());
        if newly_added {
            self.hub.update(cx, |hub, cx| hub.ensure(&channel, cx));
            if let Some(live) = &self.live {
                live.join(&channel);
            }
        }
        self.list_changed(cx);
    }

    fn remove_channel(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if !self.list.remove(channel) {
            return;
        }
        self.panes.remove(channel);
        self.streams
            .update(cx, |streams, cx| streams.forget(channel, cx));
        if let Some(live) = &self.live {
            live.part(channel);
        }
        self.hub.update(cx, |hub, cx| hub.remove(channel, cx));
        self.list_changed(cx);
    }

    fn select_channel(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if self.list.selected() != Some(channel) && self.list.select(channel) {
            self.list_changed(cx);
        }
    }

    fn move_channel(&mut self, channel: &ChannelId, direction: Move, cx: &mut Context<Self>) {
        if self.list.move_channel(channel, direction) {
            self.list_changed(cx);
        }
    }

    fn select_neighbour(&mut self, steps: isize, cx: &mut Context<Self>) {
        if let Some(channel) = self.list.neighbour(steps).cloned() {
            self.select_channel(&channel, cx);
        }
    }

    fn open_add_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shell = cx.entity().downgrade();
        add_channel::open(
            move |channel, _, cx| {
                shell
                    .update(cx, |shell, cx| shell.add_channel(channel, cx))
                    .ok();
            },
            window,
            cx,
        );
    }

    fn log_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(account) = self.account.clone() {
            sign_in::open(account, window, cx);
        }
    }

    fn log_out(&mut self, cx: &mut Context<Self>) {
        if let Some(account) = &self.account {
            account.update(cx, |account, cx| account.log_out(cx));
        }
    }

    /// The right end of the title bar: "Log in", or the account's name with its menu.
    fn account_control(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let account = self.account.as_ref()?;
        let shell = cx.entity().downgrade();
        let control = match account.read(cx).state() {
            AuthState::Restoring => return None,
            AuthState::LoggedOut { .. } => Button::new("account-log-in")
                .ghost()
                .small()
                .icon(Icon::new(IconName::CircleUser))
                .label("Log in")
                .tooltip("Log in to Twitch")
                .on_click(cx.listener(|this, _, window, cx| this.log_in(window, cx)))
                .into_any_element(),
            AuthState::LoggedIn(user) => {
                let login = user.login.clone();
                let needs_relogin = user.needs_relogin();
                Button::new("account-menu")
                    .ghost()
                    .small()
                    .icon(Icon::new(if needs_relogin {
                        IconName::TriangleAlert
                    } else {
                        IconName::CircleUser
                    }))
                    .label(login.clone())
                    .dropdown_caret(true)
                    .accessibility_label("Account")
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu: PopupMenu, _, _| {
                        let (again, out) = (shell.clone(), shell.clone());
                        menu.label(format!("Logged in to Twitch as {login}"))
                            .separator()
                            .item(PopupMenuItem::new("Log In Again…").on_click(
                                move |_, window, cx| {
                                    again.update(cx, |shell, cx| shell.log_in(window, cx)).ok();
                                },
                            ))
                            .item(PopupMenuItem::new("Log Out").on_click(move |_, _, cx| {
                                out.update(cx, |shell, cx| shell.log_out(cx)).ok();
                            }))
                    })
                    .into_any_element()
            }
        };
        Some(div().pr_2().child(control).into_any_element())
    }

    /// A strip above the content when the user was logged out without asking (with why), or the
    /// login lacks scopes the app now asks for.
    fn account_banner(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<AnyElement> {
        let account = self.account.as_ref()?.read(cx);
        let (message, action, colour) = if let Some(notice) = account.notice() {
            (notice.to_owned(), "Log in", tokens.danger)
        } else if let AuthState::LoggedIn(user) = account.state()
            && user.needs_relogin()
        {
            (
                format!(
                    "Log in again to grant the new Twitch permissions ({}).",
                    user.missing_scopes.join(", ")
                ),
                "Log in again",
                tokens.accent,
            )
        } else {
            return None;
        };
        let dismissable = account.notice().is_some();
        Some(
            h_flex()
                .id("account-banner")
                .w_full()
                .flex_none()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .bg(colour.opacity(0.14))
                .border_b_1()
                .border_color(colour)
                .child(div().flex_1().min_w_0().text_sm().child(message))
                .child(
                    Button::new("account-banner-log-in")
                        .small()
                        .label(action)
                        .on_click(cx.listener(|this, _, window, cx| this.log_in(window, cx))),
                )
                .children(dismissable.then(|| {
                    Button::new("account-banner-dismiss")
                        .small()
                        .ghost()
                        .label("Dismiss")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(account) = &this.account {
                                account.update(cx, |account, cx| account.dismiss_notice(cx));
                            }
                        }))
                }))
                .into_any_element(),
        )
    }

    fn placement_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let display = window
            .display(cx)
            .and_then(|display| display.uuid().ok())
            .map(|uuid| uuid.to_string());
        cx.set_global(LatestPlacement(placement_from_bounds(
            window.window_bounds(),
            display,
        )));
        // Coalesce bursts of move/resize events and write off the main thread. Replacing the
        // task drops (cancels) the previous pending write. The quit flush stays synchronous.
        self.save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let latest = cx.update(|cx| cx.try_global::<LatestPlacement>().cloned());
            if let Some(LatestPlacement(placement)) = latest {
                cx.background_executor()
                    .spawn(async move { state::save_window(placement) })
                    .await;
            }
        }));
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    fn show_about(window: &mut Window, cx: &mut App) {
        window.open_dialog(cx, |dialog, _, cx| {
            dialog.title(format!("About {APP_NAME}")).child(
                v_flex()
                    .gap_1()
                    .child(div().child(format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"))))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(APP_ID),
                    ),
            )
        });
    }

    fn sidebar(&self, tokens: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let shell = cx.entity().downgrade();
        let count = self.list.channels().len();
        // Copied out of the hub so the rows own their data.
        let (live, followed): (HashMap<ChannelId, StreamInfo>, Option<Vec<StreamInfo>>) =
            match &self.stream_info {
                Some(hub) => {
                    let hub = hub.read(cx);
                    let live = self
                        .list
                        .channels()
                        .iter()
                        .filter_map(|channel| match hub.liveness(channel) {
                            Liveness::Live(info) => Some((channel.clone(), info.clone())),
                            _ => None,
                        })
                        .collect();
                    (live, hub.followed_live().map(<[StreamInfo]>::to_vec))
                }
                None => (HashMap::new(), None),
            };
        let items: Vec<SidebarMenuItem> = if self.list.is_empty() {
            vec![
                SidebarMenuItem::new(format!("No channels yet: {}", platform::ADD_CHANNEL_LABEL))
                    .disable(true),
            ]
        } else {
            self.list
                .channels()
                .iter()
                .enumerate()
                .map(|(index, channel)| {
                    channel_item(
                        channel,
                        self.list.selected() == Some(channel),
                        index > 0,
                        index + 1 < count,
                        live.get(channel),
                        &shell,
                    )
                })
                .collect()
        };
        let mut groups = vec![SidebarGroup::new("Open").child(SidebarMenu::new().children(items))];
        if let Some(followed) = followed {
            let rows: Vec<SidebarMenuItem> = if followed.is_empty() {
                vec![SidebarMenuItem::new("No one you follow is live").disable(true)]
            } else {
                followed
                    .iter()
                    .map(|info| {
                        followed_item(info, self.list.selected() == Some(&info.channel), &shell)
                    })
                    .collect()
            };
            groups.push(
                SidebarGroup::new("Followed — live").child(SidebarMenu::new().children(rows)),
            );
        }
        Sidebar::<SidebarGroup<SidebarMenu>>::new("channel-sidebar")
            .collapsible(SidebarCollapsible::Offcanvas)
            .collapsed(self.sidebar_collapsed)
            .w(SIDEBAR_WIDTH)
            .header(
                SidebarHeader::new().child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .text_color(tokens.text_muted)
                                .child("Channels"),
                        )
                        .child(
                            Button::new("add-channel")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Plus).size_4())
                                .accessibility_label("Add channel")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_add_dialog(window, cx);
                                })),
                        ),
                ),
            )
            .children(groups)
    }

    /// A non-blocking strip above the content while `settings.toml` fails to load. The
    /// previous settings stay in effect; it disappears when the file loads again.
    fn settings_error_banner(error: &SettingsError, tokens: &Tokens) -> impl IntoElement {
        let danger = tokens.danger;
        v_flex()
            .id("settings-error-banner")
            .w_full()
            .flex_none()
            .px_3()
            .py_2()
            .gap_1()
            .bg(danger.opacity(0.14))
            .border_b_1()
            .border_color(danger)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(danger)
                    .child("Settings not applied: keeping the previous settings"),
            )
            .child(div().text_sm().child(error.to_string()))
    }

    fn main_area(&self, tokens: &Tokens) -> impl IntoElement {
        let area = div().flex_1().min_h_0().w_full().bg(tokens.main_bg);
        let pane = self
            .list
            .selected()
            .and_then(|channel| self.panes.get(channel));
        match pane {
            Some(pane) => area.child(pane.clone()),
            None => area.flex().items_center().justify_center().child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xl().child(APP_NAME))
                    .child(div().text_sm().text_color(tokens.text_muted).child(format!(
                        "Press {} to add a channel, {} to toggle the sidebar, {} for the debug HUD.",
                        platform::ADD_CHANNEL_LABEL,
                        platform::TOGGLE_SIDEBAR_LABEL,
                        platform::TOGGLE_HUD_LABEL
                    ))),
            ),
        }
    }
}

/// One sidebar row: click selects, the context menu removes and reorders.
fn channel_item(
    channel: &ChannelId,
    selected: bool,
    can_move_up: bool,
    can_move_down: bool,
    live: Option<&StreamInfo>,
    shell: &WeakEntity<AppShell>,
) -> SidebarMenuItem {
    let act = |shell: &WeakEntity<AppShell>,
               channel: &ChannelId,
               f: fn(&mut AppShell, &ChannelId, &mut Context<AppShell>)| {
        let (shell, channel) = (shell.clone(), channel.clone());
        move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
            shell.update(cx, |shell, cx| f(shell, &channel, cx)).ok();
        }
    };
    let (click_shell, menu_shell, menu_channel) = (shell.clone(), shell.clone(), channel.clone());
    let click_channel = channel.clone();
    let mut item = SidebarMenuItem::new(channel.login().to_owned())
        .active(selected)
        .on_click(act(&click_shell, &click_channel, |shell, channel, cx| {
            shell.select_channel(channel, cx);
        }));
    if let Some(info) = live {
        let info = info.clone();
        item = item
            .accessibility_label(format!(
                "{}, live, {} viewers",
                channel.login(),
                stream_info::format_viewers(info.viewers)
            ))
            .suffix(move |_, cx| live_suffix(&info, false, cx));
    }
    item.context_menu(move |menu: PopupMenu, _, _| {
        menu.item(
            PopupMenuItem::new("Move Up")
                .disabled(!can_move_up)
                .on_click(act(&menu_shell, &menu_channel, |shell, channel, cx| {
                    shell.move_channel(channel, Move::Up, cx);
                })),
        )
        .item(
            PopupMenuItem::new("Move Down")
                .disabled(!can_move_down)
                .on_click(act(&menu_shell, &menu_channel, |shell, channel, cx| {
                    shell.move_channel(channel, Move::Down, cx);
                })),
        )
        .separator()
        .item(PopupMenuItem::new("Remove").on_click(act(
            &menu_shell,
            &menu_channel,
            |shell, channel, cx| {
                shell.remove_channel(channel, cx);
            },
        )))
    })
}

/// The right end of a live row: the live dot, the category (followed rows) and the viewers. The
/// tooltip carries the title, which has no room in the row.
fn live_suffix(info: &StreamInfo, with_category: bool, cx: &App) -> AnyElement {
    let uptime = info
        .uptime(stream_info::unix_now())
        .map(stream_info::format_uptime);
    let tooltip = {
        let mut lines = vec![info.title.clone()];
        let mut facts = Vec::new();
        if !info.category.is_empty() {
            facts.push(info.category.clone());
        }
        facts.push(format!(
            "{} viewers",
            stream_info::format_viewers(info.viewers)
        ));
        if let Some(uptime) = uptime {
            facts.push(format!("live for {uptime}"));
        }
        lines.push(facts.join(" · "));
        lines.retain(|line| !line.is_empty());
        lines.join("\n")
    };
    let category = (with_category && !info.category.is_empty()).then(|| info.category.clone());
    h_flex()
        .id(SharedString::from(format!("live-{}", info.channel)))
        .flex_none()
        .gap_1p5()
        .items_center()
        .text_xs()
        .opacity(0.8)
        .children(category.map(|category| {
            div()
                .max_w(px(60.0))
                .truncate()
                .text_color(cx.theme().muted_foreground)
                .child(category)
        }))
        .child(
            div()
                .size(px(7.0))
                .rounded_full()
                .bg(stream_info::live_color()),
        )
        .child(stream_info::compact_viewers(info.viewers))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .into_any_element()
}

/// One "Followed — live" row: click opens (adds and selects) the channel.
fn followed_item(
    info: &StreamInfo,
    selected: bool,
    shell: &WeakEntity<AppShell>,
) -> SidebarMenuItem {
    let (shell, channel) = (shell.clone(), info.channel.clone());
    let shown = info.clone();
    SidebarMenuItem::new(info.display_name.clone())
        .active(selected)
        .accessibility_label(format!(
            "{}, live{}{}, {} viewers",
            info.display_name,
            if info.category.is_empty() {
                ""
            } else {
                " playing "
            },
            info.category,
            stream_info::format_viewers(info.viewers)
        ))
        .suffix(move |_, cx| live_suffix(&shown, true, cx))
        .on_click(move |_, _, cx| {
            shell
                .update(cx, |shell, cx| shell.add_channel(channel.clone(), cx))
                .ok();
        })
}

impl Focusable for AppShell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AppShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        let font = cx.global::<FontConfig>().clone();
        let settings_error = config::store(cx).read(cx).error().cloned();

        let root = v_flex()
            .key_context(SHELL_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _| window.minimize_window()))
            .on_action(cx.listener(|_, _: &About, window, cx| Self::show_about(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleDebugHud, _, cx| {
                this.hud.update(cx, |hud, cx| hud.toggle(cx));
            }))
            .on_action(cx.listener(|this, _: &AddChannel, window, cx| {
                this.open_add_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RemoveChannel, _, cx| {
                if let Some(channel) = this.list.selected().cloned() {
                    this.remove_channel(&channel, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &MoveChannelUp, _, cx| {
                if let Some(channel) = this.list.selected().cloned() {
                    this.move_channel(&channel, Move::Up, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &MoveChannelDown, _, cx| {
                if let Some(channel) = this.list.selected().cloned() {
                    this.move_channel(&channel, Move::Down, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SelectNextChannel, _, cx| {
                this.select_neighbour(1, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousChannel, _, cx| {
                this.select_neighbour(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &LogIn, window, cx| this.log_in(window, cx)))
            .on_action(cx.listener(|this, _: &LogOut, _, cx| this.log_out(cx)))
            .on_action(cx.listener(|this, _: &ReloadEmotes, _, _| {
                if let Some(live) = &this.live {
                    live.reload_emotes();
                }
            }))
            .size_full()
            .bg(tokens.window_bg)
            .text_color(tokens.text)
            .font_family(font.family)
            .text_size(font.size)
            .child(
                TitleBar::new()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                SidebarToggleButton::new()
                                    .collapsed(self.sidebar_collapsed)
                                    .accessibility_label("Toggle sidebar")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
                                    ),
                            )
                            .child(div().text_sm().child(APP_NAME)),
                    )
                    .children(self.account_control(cx)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.sidebar(&tokens, cx))
                    .child(
                        v_flex()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .children(
                                settings_error
                                    .as_ref()
                                    .map(|error| Self::settings_error_banner(error, &tokens)),
                            )
                            .children(self.account_banner(&tokens, cx))
                            .child(self.main_area(&tokens))
                            .child(self.hud.clone()),
                    ),
            );
        // Times the draw of everything above for the HUD.
        frame_probe(self.diagnostics.clone(), root)
    }
}
