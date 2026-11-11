//! `gpui-chat` binary: the only crate that depends on GPUI (via GPUI Kit).
//!
//! GPUI / GPUI Kit signatures used here were checked against the pinned sources
//! (`gpui-kit 0.9.1`, `gpui-component 0.9.1`, `gpui-pre 0.3.12`); see the PR 0.2 description.

mod actions;
mod assets;
mod fonts;
mod platform;
mod shell;
mod state;
mod theme;

use gpui_chat_core::APP_NAME;
use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::actions::Quit;
use crate::shell::{AppShell, DEFAULT_SIZE, MIN_SIZE};
use crate::state::Rect;

/// Window bounds restored from `state.toml` when still on a connected display, else centred.
fn initial_bounds(cx: &App) -> WindowBounds {
    let displays: Vec<Rect> = cx
        .displays()
        .iter()
        .map(|display| {
            let bounds = display.bounds();
            Rect {
                x: bounds.origin.x.as_f32(),
                y: bounds.origin.y.as_f32(),
                width: bounds.size.width.as_f32(),
                height: bounds.size.height.as_f32(),
            }
        })
        .collect();
    state::load()
        .window
        .filter(|placement| placement.is_sane() && placement.is_visible_on(&displays))
        .map(shell::bounds_from_placement)
        .unwrap_or_else(|| WindowBounds::centered(size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx))
}

fn main() {
    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            fonts::init(cx);
            theme::sync_with_system(None, cx);

            cx.bind_keys(platform::default_bindings());
            cx.set_menus(platform::menus());
            cx.on_action(|_: &Quit, cx| cx.quit());

            // Persist the last window placement on the way out (⌘Q and last-window close).
            cx.on_app_quit(|cx| {
                shell::flush_placement(cx);
                async {}
            })
            .detach();

            // Quit when the last window closes (single-window app for now).
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                window_bounds: Some(initial_bounds(cx)),
                window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
                ..TitleBar::window_options()
            };

            gpui_kit::open_window(options, cx, |window, cx| {
                // Re-sync once the window exists: its appearance is authoritative on Linux.
                theme::sync_with_system(Some(window), cx);
                cx.new(|cx| AppShell::new(window, cx))
            })
            .unwrap_or_else(|error| panic!("failed to open the {APP_NAME} main window: {error}"));
            cx.activate(true);
        });
}
