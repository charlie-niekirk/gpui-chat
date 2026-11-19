//! Application actions. Key bindings and menus are platform specific and live in
//! [`crate::platform`]; handlers live with the views that own the state they change.

use gpui_kit::actions;

actions!(
    gpui_chat,
    [
        About,
        /// Asks GitHub whether a newer release is published (PR 7.2).
        CheckForUpdates,
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
        OpenSettings,
        ComposerHistoryPrevious,
        ComposerHistoryNext,
        SplitRight,
        SplitDown,
        ClosePane,
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        OpenPalette,
        OpenChannelSwitcher,
        /// Opens the user card of the author of the selected chat message.
        OpenUserCard,
        ShowMentions,
        /// Opens the find bar of the focused pane (PR 5.7).
        Find,
        /// Goes to the next (newer) match of the find bar.
        FindNext,
        /// Goes to the previous (older) match of the find bar.
        FindPrevious,
        /// Closes the find bar (Esc inside it).
        FindClose,
        /// Switches the focused pane's filters off.
        ClearFilter,
        /// Shows only mentions and highlights in the focused pane.
        ToggleMentionsFilter,
        /// Shows only messages with links in the focused pane.
        ToggleLinksFilter,
        /// Hides the messages of bots in the focused pane.
        ToggleBotsFilter,
        /// Ignores the author of the selected chat message.
        IgnoreUser,
        /// Stops ignoring the author of the selected chat message.
        UnignoreUser,
        /// Asks for the Twitch permissions that sync the block list.
        EnableBlockListSync,
        /// Blocks the author of the selected chat message on Twitch (asks first).
        BlockOnTwitch
    ]
);

/// Key context set on the root shell view.
pub const SHELL_CONTEXT: &str = "AppShell";

/// Key context of a chat view (the list of messages), focused by a press in it: where ⌘C copies
/// the selection and Esc clears it (PR 5.6).
pub const CHAT_CONTEXT: &str = "ChatView";

/// Key context of a pane's input box; its ↑/↓ bindings (see `platform::default_bindings`) are
/// scoped to the text input inside it so they win over the input's own cursor movement.
pub const COMPOSER_CONTEXT: &str = "ChatComposer";
