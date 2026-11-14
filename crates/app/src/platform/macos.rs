//! macOS: ⌘ bindings and the standard application menu.

use gpui_chat_core::APP_NAME;
use gpui_kit::{KeyBinding, Menu, MenuItem, SystemMenuType};

use super::{about_item, account_items, channel_menu, quit_item, view_menu, window_menu};
use crate::actions::{
    AddChannel, CloseWindow, MinimizeWindow, MoveChannelDown, MoveChannelUp, Quit, RemoveChannel,
    SHELL_CONTEXT, SelectNextChannel, SelectPreviousChannel, ToggleDebugHud, ToggleSidebar,
};

pub const TOGGLE_SIDEBAR_LABEL: &str = "⌘B";
pub const TOGGLE_HUD_LABEL: &str = "⌘⇧D";
pub const ADD_CHANNEL_LABEL: &str = "⌘N";

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-\\", ToggleSidebar, None),
        KeyBinding::new("cmd-shift-d", ToggleDebugHud, None),
        KeyBinding::new("cmd-n", AddChannel, None),
        KeyBinding::new("cmd-backspace", RemoveChannel, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-alt-up", MoveChannelUp, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-alt-down", MoveChannelDown, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-shift-]", SelectNextChannel, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-shift-[", SelectPreviousChannel, Some(SHELL_CONTEXT)),
    ]
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu::new(APP_NAME).items(
            [about_item(), MenuItem::separator()]
                .into_iter()
                .chain(account_items())
                .chain([
                    MenuItem::separator(),
                    MenuItem::os_submenu("Services", SystemMenuType::Services),
                    MenuItem::separator(),
                    quit_item(),
                ]),
        ),
        channel_menu(),
        view_menu(),
        window_menu(),
    ]
}
