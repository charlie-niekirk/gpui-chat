//! Platform-specific behaviour: native menu layout and default key bindings.
//!
//! This is the only module allowed to contain `cfg(target_os)` branches for app behaviour.

use gpui_kit::{KeyBinding, Menu, MenuItem};

use crate::actions::{About, CloseWindow, MinimizeWindow, Quit, ToggleSidebar};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod other;

#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(not(target_os = "macos"))]
use other as imp;

/// Human-readable label of the Toggle Sidebar shortcut (kept in sync with `bindings()`).
pub use imp::TOGGLE_SIDEBAR_LABEL;

/// Default key bindings for the current platform.
pub fn default_bindings() -> Vec<KeyBinding> {
    imp::bindings()
}

/// The application menu bar (the native menu on macOS).
pub fn menus() -> Vec<Menu> {
    imp::menus()
}

/// Items shared by every platform's menu bar.
fn view_menu() -> Menu {
    Menu::new("View").items([MenuItem::action("Toggle Sidebar", ToggleSidebar)])
}

fn window_menu() -> Menu {
    Menu::new("Window").items([
        MenuItem::action("Minimize", MinimizeWindow),
        MenuItem::action("Close Window", CloseWindow),
    ])
}

fn about_item() -> MenuItem {
    MenuItem::action("About", About)
}

fn quit_item() -> MenuItem {
    MenuItem::action("Quit", Quit)
}
