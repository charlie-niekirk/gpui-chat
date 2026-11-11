//! Application actions. Key bindings and menus are platform specific and live in
//! [`crate::platform`]; handlers live with the views that own the state they change.

use gpui_kit::actions;

actions!(
    gpui_chat,
    [About, Quit, CloseWindow, MinimizeWindow, ToggleSidebar]
);

/// Key context set on the root shell view.
pub const SHELL_CONTEXT: &str = "AppShell";
