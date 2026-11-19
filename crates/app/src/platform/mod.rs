//! Platform-specific behaviour: native menu layout and default key bindings.
//!
//! This is the only module allowed to contain `cfg(target_os)` branches for app behaviour.

use gpui_kit::{KeyBinding, Menu, MenuItem};

use crate::actions::{
    About, AddChannel, CheckForUpdates, ClearFilter, ClosePane, CloseWindow, ComposerHistoryNext,
    ComposerHistoryPrevious, Find, FindNext, FindPrevious, FocusPaneDown, FocusPaneLeft,
    FocusPaneRight, FocusPaneUp, LogIn, LogOut, MinimizeWindow, MoveChannelDown, MoveChannelUp,
    OpenChannelSwitcher, OpenPalette, OpenSettings, Quit, ReloadEmotes, RemoveChannel,
    SelectNextChannel, SelectPreviousChannel, ShowMentions, SplitDown, SplitRight, ToggleDebugHud,
    ToggleSidebar,
};

use crate::chat::selection_layer::ClearSelection;

pub mod keychain;
#[cfg(target_os = "macos")]
mod macos;
pub mod notify;
#[cfg(not(target_os = "macos"))]
mod other;

#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(not(target_os = "macos"))]
use other as imp;

/// Default key bindings for the current platform.
pub fn default_bindings() -> Vec<KeyBinding> {
    let mut bindings = imp::bindings();
    // Registered after the input's own bindings and matching at the same depth, so ↑/↓ recall the
    // sent lines instead of moving the cursor of the one-line input.
    bindings.extend([
        KeyBinding::new("up", ComposerHistoryPrevious, Some(COMPOSER_INPUT)),
        KeyBinding::new("down", ComposerHistoryNext, Some(COMPOSER_INPUT)),
    ]);
    // Completion (Tab, Esc) and the emote picker: after the above, so the picker's ↑/↓ win in its
    // own search field.
    bindings.extend(crate::completion::bindings());
    // The find bar's Esc / ↑ / ↓ (PR 5.7), scoped to its text field.
    bindings.extend(crate::chat::view::find_bindings());
    // Esc in the chat clears its selection (⌘C / Ctrl-C to copy it is per platform).
    bindings.push(KeyBinding::new(
        "escape",
        ClearSelection,
        Some(crate::actions::CHAT_CONTEXT),
    ));
    bindings
}

/// The text input inside a composer (see `actions::COMPOSER_CONTEXT`).
const COMPOSER_INPUT: &str = "ChatComposer > Input";

/// The application menu bar (the native menu on macOS).
pub fn menus() -> Vec<Menu> {
    imp::menus()
}

/// Items shared by every platform's menu bar.
fn view_menu() -> Menu {
    Menu::new("View").items([
        MenuItem::action("Command Palette…", OpenPalette),
        MenuItem::separator(),
        MenuItem::action("Find in Chat…", Find),
        MenuItem::action("Find Next", FindNext),
        MenuItem::action("Find Previous", FindPrevious),
        MenuItem::action("Clear Chat Filter", ClearFilter),
        MenuItem::separator(),
        MenuItem::action("Toggle Sidebar", ToggleSidebar),
        MenuItem::action("Show Mentions", ShowMentions),
        MenuItem::action("Toggle Debug HUD", ToggleDebugHud),
        MenuItem::separator(),
        MenuItem::action("Split Right", SplitRight),
        MenuItem::action("Split Down", SplitDown),
        MenuItem::action("Close Pane", ClosePane),
        MenuItem::separator(),
        MenuItem::action("Focus Pane Left", FocusPaneLeft),
        MenuItem::action("Focus Pane Right", FocusPaneRight),
        MenuItem::action("Focus Pane Above", FocusPaneUp),
        MenuItem::action("Focus Pane Below", FocusPaneDown),
    ])
}

fn channel_menu() -> Menu {
    Menu::new("Channel").items([
        MenuItem::action("Go to Channel…", OpenChannelSwitcher),
        MenuItem::separator(),
        MenuItem::action("Add Channel…", AddChannel),
        MenuItem::action("Remove Channel", RemoveChannel),
        MenuItem::separator(),
        MenuItem::action("Move Up", MoveChannelUp),
        MenuItem::action("Move Down", MoveChannelDown),
        MenuItem::separator(),
        MenuItem::action("Next Channel", SelectNextChannel),
        MenuItem::action("Previous Channel", SelectPreviousChannel),
        MenuItem::separator(),
        MenuItem::action("Reload Emotes", ReloadEmotes),
    ])
}

fn window_menu() -> Menu {
    Menu::new("Window").items([
        MenuItem::action("Minimize", MinimizeWindow),
        MenuItem::action("Close Window", CloseWindow),
    ])
}

/// Log in / log out (the application menu).
fn account_items() -> [MenuItem; 2] {
    [
        MenuItem::action("Log In to Twitch…", LogIn),
        MenuItem::action("Log Out", LogOut),
    ]
}

/// Opens the settings window (the application menu).
fn settings_item() -> MenuItem {
    MenuItem::action("Settings…", OpenSettings)
}

/// Asks GitHub for a newer release (the application menu).
fn check_updates_item() -> MenuItem {
    MenuItem::action("Check for Updates…", CheckForUpdates)
}

fn about_item() -> MenuItem {
    MenuItem::action("About", About)
}

fn quit_item() -> MenuItem {
    MenuItem::action("Quit", Quit)
}
