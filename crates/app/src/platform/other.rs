//! Linux / Windows: Ctrl bindings and a plain menu bar.

use gpui_chat_core::APP_NAME;
use gpui_kit::{KeyBinding, Menu};

use super::{about_item, channel_menu, quit_item, view_menu, window_menu};
use crate::actions::{
    AddChannel, CloseWindow, MinimizeWindow, MoveChannelDown, MoveChannelUp, Quit, RemoveChannel,
    SHELL_CONTEXT, SelectNextChannel, SelectPreviousChannel, ToggleDebugHud, ToggleSidebar,
};

pub const TOGGLE_SIDEBAR_LABEL: &str = "Ctrl+B";
pub const TOGGLE_HUD_LABEL: &str = "Ctrl+Shift+D";
pub const ADD_CHANNEL_LABEL: &str = "Ctrl+N";

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("ctrl-w", CloseWindow, None),
        KeyBinding::new("ctrl-m", MinimizeWindow, None),
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        KeyBinding::new("ctrl-\\", ToggleSidebar, None),
        KeyBinding::new("ctrl-shift-d", ToggleDebugHud, None),
        KeyBinding::new("ctrl-n", AddChannel, None),
        KeyBinding::new("ctrl-backspace", RemoveChannel, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-alt-up", MoveChannelUp, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-alt-down", MoveChannelDown, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-shift-]", SelectNextChannel, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-shift-[", SelectPreviousChannel, Some(SHELL_CONTEXT)),
    ]
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu::new(APP_NAME).items([about_item(), quit_item()]),
        channel_menu(),
        view_menu(),
        window_menu(),
    ]
}
