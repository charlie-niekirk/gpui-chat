//! The root view: custom title bar, collapsible channel sidebar and the main area (one pane).
//!
//! The shell owns the [`ChannelList`] (what is open, in which order, which is selected). Every
//! change to it goes through [`AppShell::list_changed`], which makes sure the selected channel
//! has a pane, joins/parts on the live client and schedules a write of `state.toml`.

use std::collections::HashMap;
use std::time::Duration;

use gpui_chat_core::{APP_ID, APP_NAME, ChannelId};
use gpui_chat_settings::SettingsError;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarHeader, SidebarMenu, SidebarMenuItem, SidebarToggleButton,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, Styled, Subscription, Task,
    WeakEntity, Window, WindowBounds, div, px, size,
};

use crate::actions::{
    About, AddChannel, CloseWindow, MinimizeWindow, MoveChannelDown, MoveChannelUp, ReloadEmotes,
    RemoveChannel, SHELL_CONTEXT, SelectNextChannel, SelectPreviousChannel, ToggleDebugHud,
    ToggleSidebar,
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
use crate::state::{self, Mode, SavedChannels, WindowPlacement};
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
            diagnostics,
            hud_visible,
        } = parts;
        let hud = cx.new(|cx| DebugHud::new(diagnostics.clone(), hub.clone(), hud_visible, cx));
        let subscriptions = vec![
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
        shell
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
        let pane = cx.new(|cx| ChannelPane::new(hub, store, streams, live, cx));
        self.panes.insert(channel, pane);
    }

    /// Every edit of the channel list ends here.
    fn list_changed(&mut self, cx: &mut Context<Self>) {
        self.ensure_selected_pane(cx);
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
                        &shell,
                    )
                })
                .collect()
        };
        Sidebar::<SidebarMenu>::new("channel-sidebar")
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
            .child(SidebarMenu::new().children(items))
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
    SidebarMenuItem::new(channel.login().to_owned())
        .active(selected)
        .on_click(act(&click_shell, &click_channel, |shell, channel, cx| {
            shell.select_channel(channel, cx);
        }))
        .context_menu(move |menu: PopupMenu, _, _| {
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
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            SidebarToggleButton::new()
                                .collapsed(self.sidebar_collapsed)
                                .accessibility_label("Toggle sidebar")
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                        )
                        .child(div().text_sm().child(APP_NAME)),
                ),
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
                            .child(self.main_area(&tokens))
                            .child(self.hud.clone()),
                    ),
            );
        // Times the draw of everything above for the HUD.
        frame_probe(self.diagnostics.clone(), root)
    }
}
