//! The root view: custom title bar, collapsible channel sidebar and the main area.
//!
//! Everything inside is a placeholder until the chat phases land; this PR owns the frame.

use std::time::Duration;

use gpui_chat_core::{APP_ID, APP_NAME};
use gpui_chat_settings::SettingsError;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarHeader, SidebarMenu, SidebarMenuItem, SidebarToggleButton,
};
use gpui_kit::component::{ActiveTheme as _, TitleBar, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, Styled, Subscription, Task,
    Window, WindowBounds, div, px, size,
};

use crate::actions::{
    About, CloseWindow, MinimizeWindow, SHELL_CONTEXT, ToggleDebugHud, ToggleSidebar,
};
use crate::channels::ChannelHub;
use crate::config;
use crate::diagnostics::{Diagnostics, frame_probe};
use crate::fonts::FontConfig;
use crate::hud::DebugHud;
use crate::placeholder_chat::PlaceholderChat;
use crate::platform;
use crate::state::{self, Mode, WindowPlacement};
use crate::theme::{self, Tokens};

const SIDEBAR_WIDTH: Pixels = px(240.0);
/// Disk writes while dragging/resizing are coalesced into one write after this pause.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(750);

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

/// Writes the most recent placement to disk, if there is one.
pub fn flush_placement(cx: &App) {
    if let Some(LatestPlacement(placement)) = cx.try_global::<LatestPlacement>() {
        state::save_window(placement.clone());
    }
}

/// What the shell shows besides its own chrome: the channel stores (fed by the bridge) and the
/// measurements behind the debug HUD.
pub struct ShellParts {
    pub hub: Entity<ChannelHub>,
    pub diagnostics: Diagnostics,
    pub hud_visible: bool,
}

pub struct AppShell {
    focus_handle: FocusHandle,
    sidebar_collapsed: bool,
    diagnostics: Diagnostics,
    hub: Entity<ChannelHub>,
    hud: Entity<DebugHud>,
    /// The placeholder list over the first channel that has appeared (PR 1.5 adds selection).
    chat: Option<Entity<PlaceholderChat>>,
    save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AppShell {
    pub fn new(window: &mut Window, parts: ShellParts, cx: &mut Context<Self>) -> Self {
        let ShellParts {
            hub,
            diagnostics,
            hud_visible,
        } = parts;
        let hud = cx.new(|cx| DebugHud::new(diagnostics.clone(), hub.clone(), hud_visible, cx));
        let subscriptions = vec![
            // A channel appearing in the hub gives the main area something to show.
            cx.observe(&hub, |this, _, cx| this.hub_changed(cx)),
            // Follow the system light/dark setting live, but only when `theme = "system"`.
            cx.observe_window_appearance(window, |_, window, cx| {
                theme::system_appearance_changed(window, cx);
            }),
            // A settings change (or a load error appearing/clearing) applies live: font,
            // size and theme are pushed into the global font config and Kit theme, which
            // refreshes every window, but only when the appearance section actually changed;
            // the notify re-renders the banner.
            cx.observe_in(&config::store(cx), window, |_, store, window, cx| {
                let appearance = store.read(cx).settings().appearance.clone();
                theme::apply_settings_if_changed(&appearance, Some(window), cx);
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
            chat: None,
            save_task: None,
            _subscriptions: subscriptions,
        };
        shell.hub_changed(cx);
        shell
    }

    /// Shows the first channel's store in the main area once there is one.
    fn hub_changed(&mut self, cx: &mut Context<Self>) {
        if self.chat.is_some() {
            return;
        }
        let first = {
            let hub = self.hub.read(cx);
            hub.channels()
                .first()
                .and_then(|channel| hub.store(channel))
                .cloned()
        };
        if let Some(store) = first {
            self.chat = Some(cx.new(|cx| PlaceholderChat::new(store, cx)));
            cx.notify();
        }
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

    fn sidebar(&self, tokens: &Tokens) -> impl IntoElement {
        Sidebar::<SidebarMenu>::new("channel-sidebar")
            .collapsible(SidebarCollapsible::Offcanvas)
            .collapsed(self.sidebar_collapsed)
            .w(SIDEBAR_WIDTH)
            .header(
                SidebarHeader::new().child(
                    div()
                        .text_sm()
                        .text_color(tokens.text_muted)
                        .child("Channels"),
                ),
            )
            .child(SidebarMenu::new().child(SidebarMenuItem::new("No channels yet")))
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
        match &self.chat {
            Some(chat) => area.child(chat.clone()),
            None => area.flex().items_center().justify_center().child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xl().child(APP_NAME))
                    .child(div().text_sm().text_color(tokens.text_muted).child(format!(
                        "Chat will appear here. Press {} to toggle the sidebar, {} for the debug HUD.",
                        platform::TOGGLE_SIDEBAR_LABEL,
                        platform::TOGGLE_HUD_LABEL
                    ))),
            ),
        }
    }
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
                    .child(self.sidebar(&tokens))
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
