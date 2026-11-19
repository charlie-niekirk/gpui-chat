# Keymap and command palette

PR 4.3. The key bindings are the app's defaults (`crates/app/src/platform/`) with the user's
`<config>/keymap.toml` applied on top. The file is optional, watched, and applied the moment it is
saved: no restart. Menus, the palette and the hints in the UI show the bindings in force.

## Palette and switcher

| Key (macOS / elsewhere) | Action | What it does |
|---|---|---|
| ⌘K / Ctrl-K | `OpenPalette` | Every action with its current shortcut. Type to filter (fuzzy: `sr` finds *Split Right*, a group name such as `pane` or the action's id finds it too), ↑/↓ to move, Enter runs it, Esc closes (the first Esc clears the query). |
| ⌘T / Ctrl-T | `OpenChannelSwitcher` | The open channels, fuzzy filtered the same way. Enter shows the chosen one in the focused pane (if another pane has it, the two swap). |

Both are in the menus too (View ▸ Command Palette…, Channel ▸ Go to Channel…). Ranking is
`gpui_chat_core::complete::rank`, the tiers the input box's completion uses.

## `keymap.toml`

One line per action. The value **replaces** every default shortcut of that action:

```toml
SplitRight = "cmd-shift-r"                  # rebind
ToggleSidebar = ["cmd-b", "cmd-\\"]         # several shortcuts
ClosePane = []                              # unbind (also "none")
FocusPaneLeft = { keys = "alt-left", context = "AppShell" }   # choose the key context
OpenPalette = "secondary-p"                 # cmd on macOS, ctrl elsewhere
```

- **Action names** are the names in the table below (case does not matter). An unknown name is
  reported with a "did you mean".
- **Keystrokes** are `[ctrl-][alt-][shift-][cmd-][fn-]key`, modifiers in any order. `key` is one
  character (`-` is written `cmd--`) or one of `enter escape tab space backspace delete up down left
  right pageup pagedown home end insert f1`…`f35`. `cmd`, `super` and `win` are the same modifier;
  `secondary` is `cmd` on macOS and `ctrl` elsewhere, so one file can serve both. Space-separated
  keystrokes (`"cmd-k cmd-s"`) are a sequence. **A key that types a character** (a letter, digit,
  symbol or `space`) needs `ctrl`, `cmd`, `fn` or `secondary` in the shortcut: `r`, `shift-d` or
  `alt-e` would fire before text input and the input box could no longer type them (and on macOS
  `alt-` makes accents). Keys that type nothing (`escape`, arrows, `f1`…, `tab`, `enter`) may stand
  alone.
- **`context`** is a GPUI key-context expression (`AppShell`, `ChatView`, `ChatComposer > Input`,
  `EmotePicker > Input`). Left out, it is the context the action's own default shortcut has, so
  rebinding keeps working where the default did. A shortcut in a narrower context beats one in a
  wider context (the input box's Tab beats a global Tab).
- If the new shortcut is another action's *default* on the same keystroke and context, that default
  is dropped: the keystroke now runs the new action. On the same keystroke in a *different* context
  (say `SplitRight = "cmd-q"`: ⌘Q is Quit's everywhere) both stay, the narrower context wins, and the
  strip says so.
- **Problems show up in the window**, in a strip above the chat, and in the log: an unknown action,
  a keystroke that does not parse, a context that does not parse, two entries on one key (the
  first by action name keeps it), a value of the wrong type. The entry with the problem is skipped
  (its action keeps its defaults); the other entries apply. A file that is not valid TOML keeps the
  previous bindings and says where the syntax error is. The strip goes when the file is fixed.
- A missing file is the defaults. Editors that save by renaming are fine.

## Actions

| Name | Palette label | Default (macOS) |
|---|---|---|
| `OpenPalette` | Command Palette… (not listed in the palette itself) | ⌘K |
| `OpenChannelSwitcher` | Go to Channel… | ⌘T |
| `AddChannel` | Add Channel… | ⌘N |
| `RemoveChannel` | Remove Channel | ⌘⌫ |
| `MoveChannelUp` / `MoveChannelDown` | Move Channel Up / Down | ⌥⌘↑ / ⌥⌘↓ |
| `SelectNextChannel` / `SelectPreviousChannel` | Next / Previous Channel | ⇧⌘] / ⇧⌘[ |
| `ReloadEmotes` | Reload Emotes | |
| `SplitRight` / `SplitDown` | Split Right / Down | ⌘D / ⌥⇧⌘D |
| `ClosePane` | Close Pane | ⇧⌘W |
| `FocusPaneLeft` `FocusPaneRight` `FocusPaneUp` `FocusPaneDown` | Focus Pane … | ⌃⌘←→↑↓ |
| `OpenUserCard` | Open User Card for Selected Message (the author of the first selected chat message; no default key) | |
| `Find` | Find in Chat… (opens the find bar of the focused pane, PR 5.7; context `AppShell`) | ⌘F |
| `FindNext` / `FindPrevious` | Find Next / Previous (newer / older match, wrapping) | ⌘G / ⇧⌘G |
| `ClearFilter` | Clear Chat Filter (the find bar's filters of the focused pane) | |
| `ToggleMentionsFilter` `ToggleLinksFilter` `ToggleBotsFilter` | Show Only Mentions and Highlights / Messages with Links, Hide Bots | |
| `IgnoreUser` / `UnignoreUser` | Ignore / Unignore Author of Selected Message | |
| `BlockOnTwitch` | Block Author of Selected Message on Twitch… (asks first; needs block list sync) | |
| `EnableBlockListSync` | Enable Block List Sync… (account) | |
| `CopySelection` | Copy Selection (the selected chat text; context `ChatView`) | ⌘C |
| `ToggleSidebar` | Toggle Sidebar | ⌘B, ⌘\ |
| `ShowMentions` | Show Mentions (toggles the Mentions panel, PR 5.3) | ⌘⇧M |
| `ToggleDebugHud` | Toggle Debug HUD | ⌘⇧D |
| `LogIn` / `LogOut` | Log In to Twitch… / Log Out | |
| `CheckForUpdates` | Check for Updates… (asks GitHub for a newer release, PR 7.2) | |
| `About` `MinimizeWindow` `CloseWindow` `Quit` | | ⌘M ⌘W ⌘Q |

Bindable but not in the palette (the input box and the emote picker; contexts
`ChatComposer > Input` and `EmotePicker > Input`): `ComposerHistoryPrevious` (↑),
`ComposerHistoryNext` (↓), `ComposerComplete` (Tab), `ComposerCompletePrevious` (⇧Tab),
`ComposerCompletionDismiss` (Esc), `ComposerToggleEmotePicker` (⌘E), `EmotePickerLeft`
`EmotePickerRight` `EmotePickerUp` `EmotePickerDown` `EmotePickerNextTab` `EmotePickerPreviousTab`
`EmotePickerClose`; in the find bar's text field (context `FindBar > Input`) `FindClose` (Esc: closes the bar and
goes back to the input box), with ↑ / ↓ bound to `FindPrevious` / `FindNext` there (Enter and Shift-Enter step too); and in the chat (context `ChatView`, focused by a press in the messages)
`ClearSelection` (Esc: closes the user card, or clears the selection, and goes back to the input box; with neither
Esc goes on). `CopySelection` likewise goes on when nothing is selected; in the input box
⌘C is the box's own copy. They keep their defaults unless an entry names them.

The list lives in `crates/app/src/keymap.rs` (`catalog`); a test fails if an action is defined and
missing from it.

## Where things are

- `crates/settings/src/keymap.rs`: parse and check the file (no GPUI): TOML shape, action names,
  keystroke syntax (stricter than GPUI, which accepts any word as a key), `load_keymap` (a missing
  file is empty, a broken one keeps the previous entries). `keymap_watch.rs`: the directory watch,
  debounced, only for `keymap.toml`, a report only when it changed.
- `crates/app/src/keymap.rs`: the catalog, turning entries into `KeyBinding`s (`effective`),
  rebuilding the keymap and the menu bar on every change, the problem list, `shortcut()` for hints.
- `crates/app/src/palette.rs`: the palette and the switcher (Kit `Dialog` + `Command`).
