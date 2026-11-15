//! macOS: ⌘ bindings and the standard application menu.

use gpui_chat_core::APP_NAME;
use gpui_kit::{KeyBinding, Menu, MenuItem, SystemMenuType};

use super::{
    about_item, account_items, channel_menu, quit_item, settings_item, view_menu, window_menu,
};
use crate::actions::{
    AddChannel, COMPOSER_CONTEXT, ClosePane, CloseWindow, FocusPaneDown, FocusPaneLeft,
    FocusPaneRight, FocusPaneUp, MinimizeWindow, MoveChannelDown, MoveChannelUp,
    OpenChannelSwitcher, OpenPalette, OpenSettings, Quit, RemoveChannel, SHELL_CONTEXT,
    SelectNextChannel, SelectPreviousChannel, SplitDown, SplitRight, ToggleDebugHud, ToggleSidebar,
};
use crate::completion::ComposerToggleEmotePicker;

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-\\", ToggleSidebar, None),
        KeyBinding::new("cmd-shift-d", ToggleDebugHud, None),
        KeyBinding::new("cmd-n", AddChannel, None),
        KeyBinding::new("cmd-k", OpenPalette, None),
        KeyBinding::new("cmd-t", OpenChannelSwitcher, None),
        KeyBinding::new("cmd-backspace", RemoveChannel, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-alt-up", MoveChannelUp, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-alt-down", MoveChannelDown, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-shift-]", SelectNextChannel, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-shift-[", SelectPreviousChannel, Some(SHELL_CONTEXT)),
        // Split panes (PR 4.1). Not ⌥⌘D: macOS takes it for "Turn Dock hiding on/off".
        KeyBinding::new("cmd-d", SplitRight, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-alt-shift-d", SplitDown, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-shift-w", ClosePane, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-cmd-left", FocusPaneLeft, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-cmd-right", FocusPaneRight, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-cmd-up", FocusPaneUp, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-cmd-down", FocusPaneDown, Some(SHELL_CONTEXT)),
        KeyBinding::new("cmd-e", ComposerToggleEmotePicker, Some(COMPOSER_CONTEXT)),
    ]
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu::new(APP_NAME).items(
            [about_item(), MenuItem::separator(), settings_item()]
                .into_iter()
                .chain([MenuItem::separator()])
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
