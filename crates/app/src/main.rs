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
use crate::state::DisplayInfo;

/// Where to open the main window: the saved bounds on the saved display when that display is
/// still connected and the rect fits on it, else centred on the primary display.
///
/// GPUI reports/accepts window bounds relative to the display the window is on, so the
/// display is restored through `WindowOptions::display_id` rather than by global coordinates.
fn initial_placement(cx: &App) -> (WindowBounds, Option<DisplayId>) {
    let centred = || {
        (
            WindowBounds::centered(size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx),
            None,
        )
    };
    let Some(saved) = state::load().window else {
        return centred();
    };
    let primary = cx.primary_display().map(|display| display.id());
    let displays = cx.displays();
    let infos: Vec<DisplayInfo> = displays
        .iter()
        .map(|display| {
            let bounds = display.bounds();
            DisplayInfo {
                uuid: display.uuid().ok().map(|uuid| uuid.to_string()),
                is_primary: Some(display.id()) == primary,
                width: bounds.size.width.as_f32(),
                height: bounds.size.height.as_f32(),
            }
        })
        .collect();
    match saved.restore_display(&infos) {
        Some(index) => (
            shell::bounds_from_placement(&saved),
            Some(displays[index].id()),
        ),
        None => centred(),
    }
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

            let (window_bounds, display_id) = initial_placement(cx);
            let options = WindowOptions {
                window_bounds: Some(window_bounds),
                display_id,
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
