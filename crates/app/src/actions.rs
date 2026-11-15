//! Application actions. Key bindings and menus are platform specific and live in
//! [`crate::platform`]; handlers live with the views that own the state they change.

use gpui_kit::actions;

actions!(
    gpui_chat,
    [
        About,
        Quit,
        CloseWindow,
        MinimizeWindow,
        ToggleSidebar,
        ToggleDebugHud,
        AddChannel,
        RemoveChannel,
        MoveChannelUp,
        MoveChannelDown,
        SelectNextChannel,
        SelectPreviousChannel,
        ReloadEmotes,
        LogIn,
        LogOut,
        ComposerHistoryPrevious,
        ComposerHistoryNext
    ]
);

/// Key context set on the root shell view.
pub const SHELL_CONTEXT: &str = "AppShell";

/// Key context of a pane's input box; its ↑/↓ bindings (see `platform::default_bindings`) are
/// scoped to the text input inside it so they win over the input's own cursor movement.
pub const COMPOSER_CONTEXT: &str = "ChatComposer";
