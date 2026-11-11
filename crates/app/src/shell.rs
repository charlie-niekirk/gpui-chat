//! The root view: custom title bar, collapsible channel sidebar and the main area.
//!
//! Everything inside is a placeholder until the chat phases land; this PR owns the frame.

use std::time::Duration;

use gpui_chat_core::{APP_ID, APP_NAME};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarHeader, SidebarMenu, SidebarMenuItem, SidebarToggleButton,
};
use gpui_kit::component::{ActiveTheme as _, TitleBar, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, Bounds, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, Styled, Subscription, Task, Window, WindowBounds, div, px, size,
};

use crate::actions::{About, CloseWindow, MinimizeWindow, SHELL_CONTEXT, ToggleSidebar};
use crate::fonts::FontConfig;
use crate::state::{self, Mode, WindowPlacement};
use crate::theme::Tokens;

const SIDEBAR_WIDTH: Pixels = px(240.0);
/// Disk writes while dragging/resizing are coalesced into one write after this pause.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(750);

pub const DEFAULT_SIZE: (f32, f32) = (1100.0, 700.0);
pub const MIN_SIZE: (f32, f32) = (640.0, 400.0);

/// Latest window placement seen by the shell, flushed to disk on quit.
#[derive(Clone, Copy)]
struct LatestPlacement(WindowPlacement);
impl gpui_kit::Global for LatestPlacement {}

pub fn placement_from_bounds(bounds: WindowBounds) -> WindowPlacement {
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
    }
}

pub fn bounds_from_placement(placement: WindowPlacement) -> WindowBounds {
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
        state::save_window(*placement);
    }
}

pub struct AppShell {
    focus_handle: FocusHandle,
    sidebar_collapsed: bool,
    save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AppShell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            // Follow the system light/dark setting live.
            cx.observe_window_appearance(window, |_, window, cx| {
                crate::theme::sync_with_system(Some(window), cx);
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.placement_changed(window, cx);
            }),
        ];
        window.set_window_title(APP_NAME);
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            focus_handle,
            sidebar_collapsed: false,
            save_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn placement_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.set_global(LatestPlacement(placement_from_bounds(
            window.window_bounds(),
        )));
        self.save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            cx.update(|cx| flush_placement(cx));
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

    fn main_area(tokens: &Tokens) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(tokens.main_bg)
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xl().child(APP_NAME))
                    .child(
                        div()
                            .text_sm()
                            .text_color(tokens.text_muted)
                            .child("Chat will appear here. Press ⌘B to toggle the sidebar."),
                    ),
            )
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

        v_flex()
            .key_context(SHELL_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _| window.minimize_window()))
            .on_action(cx.listener(|_, _: &About, window, cx| Self::show_about(window, cx)))
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
                    .child(Self::main_area(&tokens)),
            )
    }
}
