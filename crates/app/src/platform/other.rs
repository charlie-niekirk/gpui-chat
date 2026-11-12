//! Linux / Windows: Ctrl bindings and a plain menu bar.

use gpui_chat_core::APP_NAME;
use gpui_kit::{KeyBinding, Menu};

use super::{about_item, quit_item, view_menu, window_menu};
use crate::actions::{CloseWindow, MinimizeWindow, Quit, ToggleDebugHud, ToggleSidebar};

pub const TOGGLE_SIDEBAR_LABEL: &str = "Ctrl+B";
pub const TOGGLE_HUD_LABEL: &str = "Ctrl+Shift+D";

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("ctrl-w", CloseWindow, None),
        KeyBinding::new("ctrl-m", MinimizeWindow, None),
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        KeyBinding::new("ctrl-\\", ToggleSidebar, None),
        KeyBinding::new("ctrl-shift-d", ToggleDebugHud, None),
    ]
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu::new(APP_NAME).items([about_item(), quit_item()]),
        view_menu(),
        window_menu(),
    ]
}
