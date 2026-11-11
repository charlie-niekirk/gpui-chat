//! macOS: ⌘ bindings and the standard application menu.

use gpui_chat_core::APP_NAME;
use gpui_kit::{KeyBinding, Menu, MenuItem, SystemMenuType};

use super::{about_item, quit_item, view_menu, window_menu};
use crate::actions::{CloseWindow, MinimizeWindow, Quit, ToggleSidebar};

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-\\", ToggleSidebar, None),
    ]
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu::new(APP_NAME).items([
            about_item(),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            quit_item(),
        ]),
        view_menu(),
        window_menu(),
    ]
}
