//! Linux / Windows: Ctrl bindings and a plain menu bar.

use gpui_chat_core::APP_NAME;
use gpui_kit::{KeyBinding, Menu};

use super::{
    about_item, account_items, channel_menu, quit_item, settings_item, view_menu, window_menu,
};
use crate::actions::{
    AddChannel, CHAT_CONTEXT, COMPOSER_CONTEXT, ClosePane, CloseWindow, FocusPaneDown,
    FocusPaneLeft, FocusPaneRight, FocusPaneUp, MinimizeWindow, MoveChannelDown, MoveChannelUp,
    OpenChannelSwitcher, OpenPalette, OpenSettings, Quit, RemoveChannel, SHELL_CONTEXT,
    SelectNextChannel, SelectPreviousChannel, SplitDown, SplitRight, ToggleDebugHud, ToggleSidebar,
};
use crate::chat::selection_layer::CopySelection;
use crate::completion::ComposerToggleEmotePicker;

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("ctrl-,", OpenSettings, None),
        KeyBinding::new("ctrl-w", CloseWindow, None),
        KeyBinding::new("ctrl-m", MinimizeWindow, None),
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        KeyBinding::new("ctrl-\\", ToggleSidebar, None),
        KeyBinding::new("ctrl-shift-d", ToggleDebugHud, None),
        KeyBinding::new("ctrl-n", AddChannel, None),
        KeyBinding::new("ctrl-k", OpenPalette, None),
        KeyBinding::new("ctrl-t", OpenChannelSwitcher, None),
        KeyBinding::new("ctrl-backspace", RemoveChannel, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-alt-up", MoveChannelUp, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-alt-down", MoveChannelDown, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-shift-]", SelectNextChannel, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-shift-[", SelectPreviousChannel, Some(SHELL_CONTEXT)),
        // Split panes (PR 4.1).
        KeyBinding::new("ctrl-d", SplitRight, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-alt-shift-d", SplitDown, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-shift-w", ClosePane, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-alt-shift-left", FocusPaneLeft, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-alt-shift-right", FocusPaneRight, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-alt-shift-up", FocusPaneUp, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-alt-shift-down", FocusPaneDown, Some(SHELL_CONTEXT)),
        KeyBinding::new("ctrl-e", ComposerToggleEmotePicker, Some(COMPOSER_CONTEXT)),
        KeyBinding::new("ctrl-c", CopySelection, Some(CHAT_CONTEXT)),
    ]
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu::new(APP_NAME).items(
            [about_item(), settings_item()]
                .into_iter()
                .chain(account_items())
                .chain([quit_item()]),
        ),
        channel_menu(),
        view_menu(),
        window_menu(),
    ]
}
