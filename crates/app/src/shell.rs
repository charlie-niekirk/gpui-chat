//! The root view: custom title bar, collapsible channel sidebar and the main area (split panes).
//!
//! The shell owns the [`ChannelList`] (what is open, in which order, which is selected) and the
//! split [`Layout`] (which pane shows which channel, which pane has focus). The sidebar selection
//! is the focused pane's channel. Every change to either goes through [`AppShell::list_changed`],
//! which makes sure every shown channel has a pane, keeps the selection on the focused pane and
//! schedules a write of `state.toml`; joins/parts on the live client happen where channels are
//! opened and closed.
//!
//! The title bar's right end is the account control (PR 3.1): "Log in" when logged out, the user's
//! name with a menu (log in again, log out) when logged in. A strip above the content says why the
//! user was logged out, or that the login needs new permissions.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui_chat_core::{APP_ID, APP_NAME, ChannelId, ChatEvent, MessageId};
use gpui_chat_settings::SettingsError;
use gpui_chat_twitch::auth::AuthState;
use gpui_chat_twitch::streams::StreamInfo;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarHeader, SidebarMenuItem, SidebarToggleButton,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Anchor, AnyElement, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable,
    FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, UniformListScrollHandle, WeakEntity,
    Window, WindowBounds, div, px, size,
};

use crate::account::AccountHub;
use crate::actions::{
    About, AddChannel, BlockOnTwitch, ClearFilter, ClosePane, CloseWindow, EnableBlockListSync,
    Find, FindNext, FindPrevious, FocusPaneDown, FocusPaneLeft, FocusPaneRight, FocusPaneUp,
    IgnoreUser, LogIn, LogOut, MinimizeWindow, MoveChannelDown, MoveChannelUp, OpenChannelSwitcher,
    OpenPalette, ReloadEmotes, RemoveChannel, SHELL_CONTEXT, SelectNextChannel,
    SelectPreviousChannel, ShowMentions, SplitDown, SplitRight, ToggleBotsFilter, ToggleDebugHud,
    ToggleLinksFilter, ToggleMentionsFilter, ToggleSidebar, UnignoreUser,
};
use crate::add_channel;
use crate::attention::{Attention, AttentionEvent};
use crate::bridge::BridgeSender;
use crate::channel_list::{ChannelList, Move};
use crate::channels::ChannelHub;
use crate::chat::ChatView;
use crate::composer::Composer;
use crate::config;
use crate::diagnostics::{Diagnostics, frame_probe};
use crate::emote_service;
use crate::fonts::FontConfig;
use crate::hud::DebugHud;
use crate::keymap::{self, KeymapState};
use crate::live::LiveSource;
use crate::mentions;
use crate::moderation::Moderator;
use crate::notifications::Target;
use crate::palette;
use crate::pane::{ChannelPane, PaneTools};
use crate::sign_in;
use crate::splits::view::{ChannelRows, Splits};
use crate::splits::{Direction, DropZone, Layout, PaneId};
use crate::state::{self, Mode, SavedChannels, SavedLayout, WindowPlacement};
use crate::stream_info::{self, Liveness, StreamInfoHub};
use crate::streams::StreamHub;
use crate::theme::{self, Tokens};

const SIDEBAR_WIDTH: Pixels = px(240.0);
/// How long a one-line notice stays above the panes.
const NOTICE_TIME: Duration = Duration::from_secs(4);
/// Disk writes while dragging/resizing are coalesced into one write after this pause.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(750);
/// Channel list and layout edits are rare, so they are written almost at once (a divider drag
/// writes once it pauses).
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

/// Latest split layout, flushed to disk on quit. Only set when the list is persisted.
#[derive(Clone)]
struct LatestLayout(SavedLayout);
impl gpui_kit::Global for LatestLayout {}

/// Writes the most recent placement, channel list and layout to disk, if there are any.
pub fn flush_state(cx: &App) {
    if let Some(LatestPlacement(placement)) = cx.try_global::<LatestPlacement>() {
        state::save_window(placement.clone());
    }
    match (
        cx.try_global::<LatestChannels>(),
        cx.try_global::<LatestLayout>(),
    ) {
        (Some(LatestChannels(channels)), Some(LatestLayout(layout))) => {
            state::save_channels_and_layout(channels.clone(), layout.clone());
        }
        (Some(LatestChannels(channels)), None) => state::save_channels(channels.clone()),
        (None, Some(LatestLayout(layout))) => state::save_layout(layout.clone()),
        (None, None) => {}
    }
}

/// What the shell shows besides its own chrome: the channel stores (fed by the bridge), the
/// source that feeds them and the measurements behind the debug HUD.
pub struct ShellParts {
    pub hub: Entity<ChannelHub>,
    /// Highlights, unread dots, the Mentions log (PR 5.3); every batch passes through it before
    /// the hub's stores see it.
    pub attention: Entity<Attention>,
    /// The channels to open at launch (from `state.toml`); empty for the offline sources.
    pub channels: ChannelList,
    /// The live client; `None` with `--firehose` / `--replay`, which then also keep the channel
    /// list out of `state.toml`.
    pub live: Option<LiveSource>,
    /// The Twitch account; `None` with the offline sources (they never touch the keychain).
    pub account: Option<Entity<AccountHub>>,
    /// With `--replay`: where moderation commands go instead of Helix, so a recorded log that
    /// says the user is a moderator can show (and try) the moderation controls offline.
    pub loopback: Option<BridgeSender<ChatEvent>>,
    pub diagnostics: Diagnostics,
    pub hud_visible: bool,
}

pub struct AppShell {
    focus_handle: FocusHandle,
    sidebar_collapsed: bool,
    diagnostics: Diagnostics,
    hub: Entity<ChannelHub>,
    attention: Entity<Attention>,
    /// The Mentions panel is open beside the panes.
    mentions_open: bool,
    /// A line shown for a few seconds above the panes (a jump to a message that is gone).
    notice: Option<&'static str>,
    notice_task: Option<Task<()>>,
    /// The panel's rows (held still while the pointer is on it).
    mentions_feed: RefCell<mentions::Feed>,
    /// The pointer is on the Mentions panel.
    mentions_hover: Rc<Cell<bool>>,
    mentions_scroll: UniformListScrollHandle,
    /// Whether this window is the active one (a channel is only being watched while it is).
    window_active: bool,
    hud: Entity<DebugHud>,
    list: ChannelList,
    live: Option<LiveSource>,
    account: Option<Entity<AccountHub>>,
    loopback: Option<BridgeSender<ChatEvent>>,
    /// Live state of the open and the followed channels (PR 3.5); live source only, polls while
    /// logged in.
    stream_info: Option<Entity<StreamInfoHub>>,
    /// Streamlink processes and their play state; ended on quit and when a channel is closed.
    streams: Entity<StreamHub>,
    /// Panes are created when a channel is first shown and kept (with their scroll position)
    /// until it is closed, also while no split shows it.
    panes: HashMap<ChannelId, Entity<ChannelPane>>,
    /// Which pane shows which channel, and which one has focus (PR 4.1).
    layout: Layout,
    /// The pane and zone under the pointer while a pane or a channel is dragged.
    drop_target: Option<(PaneId, DropZone)>,
    /// The focused pane (or its channel) changed: give it keyboard focus after the next frame,
    /// once a new pane's input exists.
    focus_pending: bool,
    /// What [`Self::focus_pending`] compares against.
    last_focus: (PaneId, Option<ChannelId>),
    save_task: Option<Task<()>>,
    channels_save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AppShell {
    pub fn new(window: &mut Window, parts: ShellParts, cx: &mut Context<Self>) -> Self {
        let ShellParts {
            hub,
            attention,
            channels,
            live,
            account,
            loopback,
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
                let history = settings.chat.history;
                let badges = settings.badges.clone();
                crate::badges::Badges::apply_settings(&badges, cx);
                theme::apply_settings_if_changed(&appearance, Some(window), cx);
                // The provider switches are idempotent: nothing happens unless one changed.
                if let Some(live) = &this.live {
                    live.set_emote_providers(emote_service::providers(&emotes));
                    live.set_history(history);
                    live.set_badges(&badges);
                }
                cx.notify();
            }),
            // A keymap reload (or a problem with the file appearing or clearing) re-renders the
            // strip and every shortcut hint.
            cx.observe_global::<KeymapState>(|_, cx| cx.notify()),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.placement_changed(window, cx);
            }),
            // Dots and the Mentions list follow the attention state.
            cx.observe(&attention, |_, _, cx| cx.notify()),
            // A clicked notification shows its message (or the Mentions view).
            cx.subscribe_in(&attention, window, |this, _, event, window, cx| {
                let AttentionEvent::Show(target) = event;
                this.show_target(target, window, cx);
            }),
            // What a channel's dots mean depends on whether the window is the one in front.
            cx.observe_window_activation(window, |this, window, cx| {
                this.window_active = window.is_window_active();
                this.push_view(cx);
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
        // Read here rather than passed in like the channel list: only the live source keeps one.
        let saved_layout = live.as_ref().and_then(|_| state::load().layout);
        let layout = Layout::from_saved(saved_layout.as_ref(), &channels);
        let last_focus = (layout.focused(), None);
        let mut shell = Self {
            focus_handle,
            sidebar_collapsed: false,
            diagnostics,
            hub,
            attention,
            mentions_open: false,
            notice: None,
            notice_task: None,
            mentions_feed: RefCell::default(),
            mentions_hover: Rc::default(),
            mentions_scroll: UniformListScrollHandle::new(),
            window_active: window.is_window_active(),
            hud,
            list: channels,
            live,
            account,
            loopback,
            stream_info,
            streams: StreamHub::start(window, cx),
            panes: HashMap::new(),
            layout,
            drop_target: None,
            focus_pending: true,
            last_focus,
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
        shell.sync_layout(cx);
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
        // An empty focused pane shows the first channel to appear.
        if self.layout.focused_channel().is_none()
            && let Some(channel) = self.layout.next_unshown(self.list.channels())
        {
            let focused = self.layout.focused();
            self.layout.show(focused, Some(channel));
        }
        self.sync_layout(cx);
        self.watch_open_channels(cx);
        cx.notify();
    }

    /// Creates the pane of `channel` if it has none yet.
    fn ensure_pane(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if self.panes.contains_key(channel) {
            return;
        }
        let channel = channel.clone();
        let hub = self.hub.clone();
        let store = hub.update(cx, |hub, cx| hub.ensure(&channel, cx));
        let live = self.live.is_some();
        let streams = self.streams.clone();
        // Only the live source can send, and only when there is a login owner to send as.
        let composer = self
            .live
            .clone()
            .filter(LiveSource::can_send)
            .zip(self.account.clone())
            .map(|(live, account)| {
                let store = store.clone();
                cx.new(|cx| Composer::new(store, live, account, cx))
            });
        let stream_info = self.stream_info.clone();
        // Moderation commands: Helix for the live source (with a login owner), the loopback for
        // `--replay`.
        let moderator = self
            .live
            .clone()
            .filter(LiveSource::can_send)
            .map(Moderator::Live)
            .or_else(|| self.loopback.clone().map(Moderator::Loopback));
        let tools = PaneTools {
            composer,
            moderator,
        };
        let pane = cx.new(|cx| ChannelPane::new(hub, store, streams, stream_info, live, tools, cx));
        self.panes.insert(channel, pane);
    }

    /// Every shown channel has a pane, the sidebar selection is the focused pane's channel, and
    /// panes know whether the window is split.
    fn sync_layout(&mut self, cx: &mut Context<Self>) {
        let shown: Vec<ChannelId> = self
            .layout
            .leaves()
            .iter()
            .filter_map(|leaf| leaf.channel.clone())
            .collect();
        for channel in &shown {
            self.ensure_pane(channel, cx);
        }
        match self.layout.focused_channel().cloned() {
            Some(channel) => {
                self.list.select(&channel);
            }
            None => self.list.deselect(),
        }
        let in_split = self.layout.is_split();
        for channel in &shown {
            if let Some(pane) = self.panes.get(channel) {
                pane.update(cx, |pane, cx| pane.set_in_split(in_split, cx));
            }
        }
        self.push_view(cx);
        let focus = (
            self.layout.focused(),
            self.layout.focused_channel().cloned(),
        );
        if focus != self.last_focus {
            self.last_focus = focus;
            self.focus_pending = true;
        }
    }

    /// Tells the attention state what is on screen: which channels the panes show, which one has
    /// focus and whether the window is in front. This is what clears a channel's dots.
    fn push_view(&self, cx: &mut Context<Self>) {
        let visible: Vec<ChannelId> = self
            .layout
            .leaves()
            .iter()
            .filter_map(|leaf| leaf.channel.clone())
            .collect();
        let focused = self.layout.focused_channel().cloned();
        let active = self.window_active;
        self.attention.update(cx, |attention, cx| {
            attention.set_view(visible, focused, active, cx);
        });
    }

    /// Shows `message` of `channel` (the Mentions view, a notification): the channel in the focused
    /// pane (or its own pane, focused), scrolled to the message when it is still there.
    fn show_message(
        &mut self,
        channel: &ChannelId,
        message: Option<&MessageId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.list.contains(channel) {
            return;
        }
        self.show_channel(channel);
        self.list_changed(cx);
        self.focus_pending = true;
        if let (Some(id), Some(pane)) = (message, self.panes.get(channel).cloned()) {
            let found = pane.update(cx, |pane, cx| pane.show_message(id, cx));
            if !found {
                // It left the scrollback while the hit sat in the log (or is hidden because its
                // author is ignored): say so instead of showing the channel as if nothing had
                // been asked.
                let ignored = pane.read(cx).chat().read(cx).is_hidden_by_ignore(id, cx);
                self.show_notice(
                    if ignored {
                        "That message is hidden (ignored user)."
                    } else {
                        "That message is no longer in the scrollback."
                    },
                    cx,
                );
            }
        }
        self.attention
            .update(cx, |attention, cx| attention.mark_seen(channel, cx));
        window.activate_window();
        cx.notify();
    }

    /// Shows `text` above the panes for a few seconds.
    fn show_notice(&mut self, text: &'static str, cx: &mut Context<Self>) {
        self.notice = Some(text);
        self.notice_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_TIME).await;
            this.update(cx, |this, cx| {
                this.notice = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// What a clicked notification asked for.
    fn show_target(&mut self, target: &Target, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            Target::Message(channel, id) => self.show_message(channel, Some(id), window, cx),
            Target::Mentions => {
                self.mentions_open = true;
                window.activate_window();
                cx.notify();
            }
        }
    }

    /// Opens or closes the Mentions panel.
    fn toggle_mentions(&mut self, cx: &mut Context<Self>) {
        self.mentions_open = !self.mentions_open;
        self.mentions_hover.set(false);
        cx.notify();
    }

    /// Every edit of the channel list or the layout ends here.
    fn list_changed(&mut self, cx: &mut Context<Self>) {
        self.sync_layout(cx);
        self.watch_open_channels(cx);
        self.persist(cx);
        cx.notify();
    }

    /// Schedules a write of the channel list and the layout (live source only).
    fn persist(&mut self, cx: &mut Context<Self>) {
        if !self.persists() {
            return;
        }
        let channels = self.list.to_saved();
        let layout = self.layout.to_saved();
        cx.set_global(LatestChannels(channels.clone()));
        cx.set_global(LatestLayout(layout.clone()));
        // Replacing the task drops (cancels) a write that has not started yet.
        self.channels_save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(CHANNELS_SAVE_DEBOUNCE).await;
            cx.background_executor()
                .spawn(async move { state::save_channels_and_layout(channels, layout) })
                .await;
        }));
    }

    /// Adds `channel` to the sidebar (joining it) if it is not open yet; `true` if it was not.
    fn open_channel(&mut self, channel: &ChannelId, cx: &mut Context<Self>) -> bool {
        let newly_added = self.list.add(channel.clone());
        if newly_added {
            self.hub.update(cx, |hub, cx| hub.ensure(channel, cx));
            if let Some(live) = &self.live {
                live.join(channel);
            }
        }
        newly_added
    }

    /// Focuses the pane showing `channel`, or shows it in the focused pane.
    fn show_channel(&mut self, channel: &ChannelId) {
        match self.layout.pane_showing(channel) {
            Some(pane) => {
                self.layout.focus(pane);
            }
            None => {
                let focused = self.layout.focused();
                self.layout.show(focused, Some(channel.clone()));
            }
        }
    }

    fn add_channel(&mut self, channel: ChannelId, cx: &mut Context<Self>) {
        self.open_channel(&channel, cx);
        self.show_channel(&channel);
        self.list_changed(cx);
    }

    fn remove_channel(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        let Some(index) = self.list.index_of(channel) else {
            return;
        };
        self.list.remove(channel);
        self.layout
            .channel_closed(channel, self.list.channels(), index);
        self.panes.remove(channel);
        self.streams
            .update(cx, |streams, cx| streams.forget(channel, cx));
        if let Some(live) = &self.live {
            live.part(channel);
        }
        self.hub.update(cx, |hub, cx| hub.remove(channel, cx));
        self.attention
            .update(cx, |attention, cx| attention.forget(channel, cx));
        self.list_changed(cx);
    }

    fn select_channel(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if self.list.contains(channel) && self.list.selected() != Some(channel) {
            self.show_channel(channel);
            self.list_changed(cx);
        }
    }

    fn split(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let channel = self.layout.next_unshown(self.list.channels());
        if self
            .layout
            .split(self.layout.focused(), direction, channel)
            .is_some()
        {
            self.list_changed(cx);
        }
    }

    pub(crate) fn close_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.layout.close(pane) {
            self.list_changed(cx);
        }
    }

    fn focus_neighbour(&mut self, direction: Direction, cx: &mut Context<Self>) {
        if let Some(pane) = self.layout.neighbour(self.layout.focused(), direction)
            && self.layout.focus(pane)
        {
            self.list_changed(cx);
        }
    }

    /// A press anywhere in a pane focuses it.
    pub(crate) fn pane_pressed(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.layout.focus(pane) {
            self.list_changed(cx);
        }
    }

    /// Shows (or, with `None`, hides) where a drop would land.
    fn set_drop_target(&mut self, target: Option<(PaneId, DropZone)>, cx: &mut Context<Self>) {
        if self.drop_target != target {
            self.drop_target = target;
            cx.notify();
        }
    }

    /// The pointer is over `zone` of `pane` while dragging the pane showing `channel`: nothing to
    /// show over the dragged pane itself.
    pub(crate) fn drag_pane_over(
        &mut self,
        channel: &ChannelId,
        pane: PaneId,
        zone: DropZone,
        cx: &mut Context<Self>,
    ) {
        let accepted = self
            .layout
            .pane_showing(channel)
            .is_some_and(|src| self.layout.accepts_pane(src, pane));
        self.set_drop_target(accepted.then_some((pane, zone)), cx);
    }

    /// The pointer is over `zone` of `pane` while dragging a sidebar channel: only zones where a
    /// drop would do something are shown (no new pane at the limit).
    pub(crate) fn drag_channel_over(
        &mut self,
        channel: &ChannelId,
        pane: PaneId,
        zone: DropZone,
        cx: &mut Context<Self>,
    ) {
        let accepted = self.layout.accepts_channel(channel, pane, zone);
        self.set_drop_target(accepted.then_some((pane, zone)), cx);
    }

    /// Where a drop on `pane` lands: the zone last seen under the pointer.
    fn take_drop_zone(&mut self, pane: PaneId) -> DropZone {
        match self.drop_target.take() {
            Some((target, zone)) if target == pane => zone,
            _ => DropZone::Center,
        }
    }

    /// The pane showing `channel` was dropped on `target`.
    pub(crate) fn drop_pane(
        &mut self,
        channel: &ChannelId,
        target: PaneId,
        cx: &mut Context<Self>,
    ) {
        let zone = self.take_drop_zone(target);
        if let Some(src) = self.layout.pane_showing(channel)
            && self.layout.move_pane(src, target, zone)
        {
            self.list_changed(cx);
        }
        cx.notify();
    }

    /// A sidebar channel was dropped on `target` (a followed one is opened first).
    pub(crate) fn drop_channel(
        &mut self,
        channel: ChannelId,
        target: PaneId,
        cx: &mut Context<Self>,
    ) {
        let zone = self.take_drop_zone(target);
        // A followed channel is opened only when the drop takes it (not at the pane limit).
        if self.layout.accepts_channel(&channel, target, zone) {
            self.open_channel(&channel, cx);
            self.layout.drop_channel(channel, target, zone);
            self.list_changed(cx);
        }
        cx.notify();
    }

    /// A divider was dragged; see [`Layout::resize`].
    pub(crate) fn resize_split(
        &mut self,
        path: &[usize],
        handle: usize,
        at: f32,
        min: f32,
        cx: &mut Context<Self>,
    ) {
        if self.layout.resize(path, handle, at, min) {
            self.persist(cx);
            cx.notify();
        }
    }

    /// Gives the focused pane keyboard focus (the shell itself when it shows nothing).
    fn apply_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self
            .layout
            .focused_channel()
            .and_then(|channel| self.panes.get(channel))
            .cloned();
        match pane {
            Some(pane) => pane.update(cx, |pane, cx| pane.focus(window, cx)),
            None => window.focus(&self.focus_handle, cx),
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

    /// ⌘K: every action with its shortcut, fuzzy filtered; the chosen one runs as its key would.
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (entries, actions) = palette::palette_entries(cx);
        palette::open(
            entries,
            "Type a command…",
            "No matching command",
            move |index, window, cx| {
                if let Some(info) = actions.get(index) {
                    window.dispatch_action(info.build(), cx);
                }
            },
            window,
            cx,
        );
    }

    /// ⌘T: the open channels, fuzzy filtered; the chosen one is shown in the focused pane.
    fn open_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let channels = self.list.channels().to_vec();
        let focused = self.layout.focused_channel().cloned();
        let entries = channels
            .iter()
            .map(|channel| palette::Entry {
                label: channel.to_string().into(),
                detail: if focused.as_ref() == Some(channel) {
                    Some("this pane".into())
                } else if self.layout.is_shown(channel) {
                    Some("in another pane".into())
                } else {
                    None
                },
                shortcut: None,
                keywords: String::new(),
            })
            .collect();
        let shell = cx.entity().downgrade();
        palette::open(
            entries,
            "Go to channel…",
            "No open channel matches",
            move |index, _, cx| {
                if let Some(channel) = channels.get(index) {
                    shell
                        .update(cx, |shell, cx| shell.show_in_focused_pane(channel, cx))
                        .ok();
                }
            },
            window,
            cx,
        );
    }

    /// Shows `channel` in the focused pane (swapping with the pane that has it, if any) and gives
    /// that pane keyboard focus.
    fn show_in_focused_pane(&mut self, channel: &ChannelId, cx: &mut Context<Self>) {
        if !self.list.contains(channel) {
            return;
        }
        let focused = self.layout.focused();
        self.layout.show(focused, Some(channel.clone()));
        self.list_changed(cx);
        // The pane may show the same channel as before: focus still goes back to its input.
        self.focus_pending = true;
        cx.notify();
    }

    fn log_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(account) = self.account.clone() {
            sign_in::open(account, window, cx);
        }
    }

    /// The pane that has the keyboard focus (what Find, the filters and the ignore actions act on).
    fn focused_pane(&self) -> Option<Entity<ChannelPane>> {
        self.layout
            .focused_channel()
            .and_then(|channel| self.panes.get(channel))
            .cloned()
    }

    /// Runs `f` on the focused pane's chat view; whether there was one.
    fn with_chat(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut ChatView, &mut Context<ChatView>),
    ) -> bool {
        let Some(chat) = self.focused_pane().map(|pane| pane.read(cx).chat()) else {
            return false;
        };
        chat.update(cx, f);
        true
    }

    /// *Enable block list sync…*: a login that also asks for the block list scopes (PR 5.7).
    fn enable_block_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(account) = self.account.clone() {
            sign_in::open_for_block_list(account, window, cx);
        }
    }

    /// *Enable moderation tools…*: a login that also asks for the moderation scopes.
    fn enable_moderation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(account) = self.account.clone() {
            sign_in::open_for_moderation(account, window, cx);
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
                let can_enable_moderation = !user.moderation_enabled();
                let can_enable_block_list = !user.block_list_enabled();
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
                        let (again, moderation, out, blocks) =
                            (shell.clone(), shell.clone(), shell.clone(), shell.clone());
                        let menu = menu
                            .label(format!("Logged in to Twitch as {login}"))
                            .separator()
                            .item(PopupMenuItem::new("Log In Again…").on_click(
                                move |_, window, cx| {
                                    again.update(cx, |shell, cx| shell.log_in(window, cx)).ok();
                                },
                            ));
                        // Moderation needs extra permissions, asked for only when wanted.
                        let menu = if can_enable_moderation {
                            menu.item(PopupMenuItem::new("Enable Moderation Tools…").on_click(
                                move |_, window, cx| {
                                    moderation
                                        .update(cx, |shell, cx| shell.enable_moderation(window, cx))
                                        .ok();
                                },
                            ))
                        } else {
                            menu
                        };
                        // The Twitch block list (PR 5.7): asked for only when wanted, too.
                        let menu = if can_enable_block_list {
                            menu.item(PopupMenuItem::new("Enable Block List Sync…").on_click(
                                move |_, window, cx| {
                                    blocks
                                        .update(cx, |shell, cx| shell.enable_block_list(window, cx))
                                        .ok();
                                },
                            ))
                        } else {
                            menu
                        };
                        menu.item(PopupMenuItem::new("Log Out").on_click(move |_, _, cx| {
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
        // What is unread in each open channel (PR 5.3), copied out like the live state.
        let activity: HashMap<ChannelId, Dots> = {
            let activity = self.attention.read(cx).activity();
            self.list
                .channels()
                .iter()
                .filter_map(|channel| {
                    let dots = Dots {
                        unread: activity.unread(channel),
                        mentions: activity.unseen_mentions(channel),
                    };
                    dots.any().then(|| (channel.clone(), dots))
                })
                .collect()
        };
        // Open and followed rows can be dragged into a split (PR 4.1).
        let items: Vec<(SidebarMenuItem, Option<ChannelId>)> = if self.list.is_empty() {
            vec![(
                SidebarMenuItem::new(match keymap::shortcut_of(AddChannel, cx) {
                    Some(keys) => format!("No channels yet: {keys}"),
                    None => "No channels yet".to_owned(),
                })
                .disable(true),
                None,
            )]
        } else {
            self.list
                .channels()
                .iter()
                .enumerate()
                .map(|(index, channel)| {
                    let item = channel_item(
                        channel,
                        self.list.selected() == Some(channel),
                        index > 0,
                        index + 1 < count,
                        live.get(channel),
                        activity.get(channel).copied().unwrap_or_default(),
                        &shell,
                    );
                    (item, Some(channel.clone()))
                })
                .collect()
        };
        let mut groups = vec![SidebarGroup::new("Open").child(ChannelRows::new(items))];
        if let Some(followed) = followed {
            let rows: Vec<(SidebarMenuItem, Option<ChannelId>)> = if followed.is_empty() {
                vec![(
                    SidebarMenuItem::new("No one you follow is live").disable(true),
                    None,
                )]
            } else {
                followed
                    .iter()
                    .map(|info| {
                        let item = followed_item(
                            info,
                            self.list.selected() == Some(&info.channel),
                            &shell,
                        );
                        (item, Some(info.channel.clone()))
                    })
                    .collect()
            };
            groups.push(SidebarGroup::new("Followed — live").child(ChannelRows::new(rows)));
        }
        Sidebar::<SidebarGroup<ChannelRows>>::new("channel-sidebar")
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
                    .text_color(tokens.danger_text)
                    .child("Settings not applied: keeping the previous settings"),
            )
            .child(div().text_sm().child(error.to_string()))
    }

    /// A strip listing problems with the user's themes: a theme file that does not parse, a
    /// duplicate name, unreadable colours, or a `theme_name` that matches no theme. The theme
    /// in effect keeps working; the strip goes when the problem is fixed.
    fn theme_notice_banner(notices: &[String], tokens: &Tokens) -> impl IntoElement {
        Self::notice_banner("theme-notice-banner", "Theme problem", notices, tokens)
    }

    /// The same strip for `keymap.toml` (PR 4.3): an entry naming no action, a keystroke that
    /// does not parse, two actions on one key, or a file that is not TOML (the previous bindings
    /// stay in effect). It goes when the file is fixed.
    fn keymap_notice_banner(notices: &[String], tokens: &Tokens) -> impl IntoElement {
        Self::notice_banner(
            "keymap-notice-banner",
            "Keymap problem (keymap.toml)",
            notices,
            tokens,
        )
    }

    fn notice_banner(
        id: &'static str,
        title: &'static str,
        notices: &[String],
        tokens: &Tokens,
    ) -> impl IntoElement {
        v_flex()
            .id(id)
            .w_full()
            .flex_none()
            .px_3()
            .py_2()
            .gap_1()
            .bg(tokens.warning.opacity(0.14))
            .border_b_1()
            .border_color(tokens.warning)
            .child(div().text_sm().font_weight(FontWeight::BOLD).child(title))
            .children(
                notices
                    .iter()
                    .map(|line| div().text_sm().child(line.clone())),
            )
    }

    /// The Mentions panel (PR 5.3), when it is open.
    fn mentions_panel(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.mentions_open {
            return None;
        }
        // Rebuilt only when the log changed (not on every frame or sidebar change), and not at all
        // while the pointer is on the panel: the entry under it must not slide away.
        let (rows, waiting) = {
            let activity = self.attention.read(cx).activity();
            self.mentions_feed.borrow_mut().update(
                self.mentions_hover.get(),
                activity.generation(),
                activity.added(),
                || mentions::rows(activity.mentions()),
            )
        };
        let shell = cx.entity().downgrade();
        let (close, hover, reveal) = (shell.clone(), shell.clone(), shell.clone());
        let on_pick: mentions::Pick = Rc::new(move |channel, id, window, cx| {
            let (channel, id) = (channel.clone(), id.clone());
            shell
                .update(cx, |shell, cx| {
                    shell.show_message(&channel, Some(&id), window, cx);
                })
                .ok();
        });
        let flag = self.mentions_hover.clone();
        Some(mentions::panel(
            rows,
            waiting,
            &self.mentions_scroll,
            tokens,
            mentions::PanelActions {
                on_pick,
                on_close: Rc::new(move |_, cx| {
                    close
                        .update(cx, |shell, cx| {
                            shell.mentions_open = false;
                            shell.mentions_hover.set(false);
                            cx.notify();
                        })
                        .ok();
                }),
                on_hover: Rc::new(move |hovered, _, cx| {
                    flag.set(hovered);
                    // Letting go (or taking hold) changes what the panel shows.
                    hover.update(cx, |_, cx| cx.notify()).ok();
                }),
                on_reveal: Rc::new(move |_, cx| {
                    reveal
                        .update(cx, |shell, cx| {
                            shell.mentions_feed.borrow_mut().reveal();
                            cx.notify();
                        })
                        .ok();
                }),
            },
        ))
    }

    fn main_area(&self, tokens: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let area = div().flex().flex_1().min_h_0().w_full().bg(tokens.main_bg);
        // Nothing open and nothing split: the welcome text instead of an empty pane.
        let welcome = self.list.is_empty() && !self.layout.is_split();
        if !welcome {
            let splits = Splits {
                layout: &self.layout,
                panes: &self.panes,
                drop: self.drop_target,
                tokens: *tokens,
            };
            return area
                .child(splits.render(cx))
                .children(self.mentions_panel(tokens, cx));
        }
        area.items_center().justify_center().child(
            v_flex()
                .items_center()
                .gap_2()
                .child(div().text_xl().child(APP_NAME))
                .child(
                    div()
                        .text_sm()
                        .text_color(tokens.text_muted)
                        .child(welcome_hint(cx)),
                ),
        )
    }
}

/// "Press ⌘N to add a channel, ⌘K for the command palette, …": the shortcuts as they are bound now
/// (an unbound action is left out).
fn welcome_hint(cx: &App) -> String {
    let hints: Vec<String> = [
        (keymap::shortcut_of(AddChannel, cx), "to add a channel"),
        (
            keymap::shortcut_of(OpenPalette, cx),
            "for the command palette",
        ),
        (
            keymap::shortcut_of(ToggleSidebar, cx),
            "to toggle the sidebar",
        ),
        (keymap::shortcut_of(ToggleDebugHud, cx), "for the debug HUD"),
    ]
    .into_iter()
    .filter_map(|(keys, what)| keys.map(|keys| format!("{keys} {what}")))
    .collect();
    if hints.is_empty() {
        String::new()
    } else {
        format!("Press {}.", hints.join(", "))
    }
}

/// One sidebar row: click selects, the context menu removes and reorders.
fn channel_item(
    channel: &ChannelId,
    selected: bool,
    can_move_up: bool,
    can_move_down: bool,
    live: Option<&StreamInfo>,
    dots: Dots,
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
    if live.is_some() || dots.any() {
        let info = live.cloned();
        let mut label = channel.login().to_owned();
        if let Some(info) = &info {
            label.push_str(&format!(
                ", live, {} viewers",
                stream_info::format_viewers(info.viewers)
            ));
        }
        if let Some(text) = dots.description() {
            label.push_str(&format!(", {text}"));
        }
        let id = SharedString::from(format!("activity-{channel}"));
        item = item.accessibility_label(label).suffix(move |_, cx| {
            h_flex()
                .flex_none()
                .gap_1p5()
                .items_center()
                .children(dots.element(id.clone(), cx))
                .children(info.as_ref().map(|info| live_suffix(info, false, cx)))
                .into_any_element()
        });
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

/// A channel's unread state for the sidebar (PR 5.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Dots {
    /// Messages since the channel was last looked at.
    unread: u32,
    /// Highlight hits among them nobody has looked at.
    mentions: usize,
}

impl Dots {
    fn any(self) -> bool {
        self.unread > 0 || self.mentions > 0
    }

    /// "3 unread, 1 mention", for the tooltip and the screen reader.
    fn description(self) -> Option<String> {
        let mut parts = Vec::new();
        if self.mentions > 0 {
            parts.push(format!(
                "{} {}",
                self.mentions,
                if self.mentions == 1 {
                    "mention"
                } else {
                    "mentions"
                }
            ));
        }
        if self.unread > 0 {
            parts.push(format!("{} unread", self.unread));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }

    /// A pill with the number of mentions (accent) and a dot for unread messages (muted), with the
    /// numbers in a tooltip.
    fn element(self, id: SharedString, cx: &App) -> Option<AnyElement> {
        let text = self.description()?;
        let tokens = *cx.global::<Tokens>();
        Some(
            h_flex()
                .id(id)
                .flex_none()
                .gap_1()
                .items_center()
                .children((self.mentions > 0).then(|| {
                    div()
                        .min_w(px(16.0))
                        .h(px(16.0))
                        .px_1()
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(tokens.accent)
                        .text_color(tokens.accent_text)
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .child(if self.mentions > 9 {
                            "9+".to_owned()
                        } else {
                            self.mentions.to_string()
                        })
                }))
                .children(
                    (self.unread > 0 && self.mentions == 0)
                        .then(|| div().size(px(7.0)).rounded_full().bg(tokens.text)),
                )
                .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
                .into_any_element(),
        )
    }
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
                .bg(cx.global::<Tokens>().danger),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        if self.focus_pending {
            self.focus_pending = false;
            cx.on_next_frame(window, |this, window, cx| this.apply_focus(window, cx));
        }
        // A drag that ended outside every pane leaves no drop target behind.
        if !cx.has_active_drag() {
            self.drop_target = None;
        }
        let font = cx.global::<FontConfig>().clone();
        let settings_error = config::store(cx).read(cx).error().cloned();
        let theme_notices = theme::notices(cx);
        let keymap_notices = keymap::notices(cx);

        let root = v_flex()
            .key_context(SHELL_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _| window.minimize_window()))
            .on_action(cx.listener(|_, _: &About, window, cx| Self::show_about(window, cx)))
            .on_action(cx.listener(|this, _: &ShowMentions, _, cx| this.toggle_mentions(cx)))
            .on_action(cx.listener(|this, _: &ToggleDebugHud, _, cx| {
                this.hud.update(cx, |hud, cx| hud.toggle(cx));
            }))
            .on_action(cx.listener(|this, _: &AddChannel, window, cx| {
                this.open_add_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.open_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChannelSwitcher, window, cx| {
                this.open_switcher(window, cx);
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
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| {
                this.split(Direction::Right, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| {
                this.split(Direction::Down, cx);
            }))
            .on_action(cx.listener(|this, _: &ClosePane, _, cx| {
                this.close_pane(this.layout.focused(), cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneLeft, _, cx| {
                this.focus_neighbour(Direction::Left, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneRight, _, cx| {
                this.focus_neighbour(Direction::Right, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneUp, _, cx| {
                this.focus_neighbour(Direction::Up, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneDown, _, cx| {
                this.focus_neighbour(Direction::Down, cx);
            }))
            // In-channel search, filters and ignoring (PR 5.7): all act on the focused pane.
            .on_action(cx.listener(|this, _: &Find, window, cx| {
                if let Some(pane) = this.focused_pane() {
                    let chat = pane.read(cx).chat();
                    chat.update(cx, |chat, cx| chat.open_find(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &FindNext, _, cx| {
                this.with_chat(cx, |chat, cx| chat.find_step(true, cx));
            }))
            .on_action(cx.listener(|this, _: &FindPrevious, _, cx| {
                this.with_chat(cx, |chat, cx| chat.find_step(false, cx));
            }))
            .on_action(cx.listener(|this, _: &ClearFilter, _, cx| {
                this.with_chat(cx, |chat, cx| chat.clear_filters(cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleMentionsFilter, _, cx| {
                this.with_chat(cx, |chat, cx| chat.toggle_mentions_filter(cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleLinksFilter, _, cx| {
                this.with_chat(cx, |chat, cx| chat.toggle_links_filter(cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleBotsFilter, _, cx| {
                this.with_chat(cx, |chat, cx| chat.toggle_bots_filter(cx));
            }))
            .on_action(cx.listener(|this, _: &IgnoreUser, _, cx| {
                this.with_chat(cx, |chat, cx| {
                    if let Some((login, _)) = chat.selected_author(cx) {
                        chat.ignore_user(&login, cx);
                    }
                });
            }))
            .on_action(cx.listener(|this, _: &UnignoreUser, _, cx| {
                this.with_chat(cx, |chat, cx| {
                    if let Some((login, _)) = chat.selected_author(cx) {
                        chat.unignore_user(&login, cx);
                    }
                });
            }))
            .on_action(cx.listener(|this, _: &BlockOnTwitch, window, cx| {
                let Some(pane) = this.focused_pane() else {
                    return;
                };
                let author = pane
                    .read(cx)
                    .chat()
                    .read_with(cx, |chat, cx| chat.selected_author(cx));
                if let Some((login, user_id)) = author {
                    pane.update(cx, |pane, cx| {
                        pane.confirm_block(&login, user_id, window, cx)
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &EnableBlockListSync, window, cx| {
                this.enable_block_list(window, cx);
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
                            .children(
                                (!theme_notices.is_empty())
                                    .then(|| Self::theme_notice_banner(&theme_notices, &tokens)),
                            )
                            .children(
                                (!keymap_notices.is_empty())
                                    .then(|| Self::keymap_notice_banner(&keymap_notices, &tokens)),
                            )
                            .children(self.account_banner(&tokens, cx))
                            .children(
                                self.notice.map(|text| {
                                    Self::notice_banner("jump-notice", text, &[], &tokens)
                                }),
                            )
                            .child(self.main_area(&tokens, cx))
                            .child(self.hud.clone()),
                    ),
            );
        // Times the draw of everything above for the HUD.
        frame_probe(self.diagnostics.clone(), root)
    }
}
