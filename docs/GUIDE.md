# User guide

How to use gpui-chat. For installing it see the [README](../README.md); the reference for every setting is
[`SETTINGS.md`](SETTINGS.md), for the keymap file [`KEYMAP.md`](KEYMAP.md), for themes [`THEMES.md`](THEMES.md).
Shortcuts below are the macOS ones (⌘ is Command); everything also lives in the menus and in the command
palette (⌘K).

- [First steps](#first-steps)
- [Channels and the sidebar](#channels-and-the-sidebar)
- [Splits](#splits)
- [Logging in](#logging-in)
- [Writing messages](#writing-messages)
- [Emotes and badges](#emotes-and-badges)
- [Mentions and highlights](#mentions-and-highlights)
- [Finding, filtering and ignoring](#finding-filtering-and-ignoring)
- [Moderation](#moderation)
- [Watching the stream: streamlink](#watching-the-stream-streamlink)
- [Appearance](#appearance)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Settings and files](#settings-and-files)
- [Updates](#updates)
- [Troubleshooting](#troubleshooting)

## First steps

Open the app and press **⌘N** (or the **+** next to *Channels*). Type a channel name, `#forsen`, `@forsen`
or paste any twitch.tv link to it (a pop-out, moderator or embed link works too) and press Enter. Chat starts
at once, **without logging in**: anonymous mode can read every chat and cannot write. Log in when you want to
send messages (see [Logging in](#logging-in)).

The channels you have open, the window position and the split layout are remembered between launches.

## Channels and the sidebar

- **Add / remove:** ⌘N adds a channel; ⌘⌫ removes the selected one (also in the row's right-click menu and the
  *Channel* menu).
- **Switch:** click a row, ⇧⌘] / ⇧⌘[ for next / previous, or **⌘T** to type part of a name and press Enter.
- **Reorder:** ⌥⌘↑ / ⌥⌘↓ or the row's right-click menu.
- **Unread markers:** a channel that is not on screen gets a dot when someone writes in it, and a number when a
  message mentions you or matches a highlight rule. Showing the channel clears them.
- **Live state:** an open channel that is live shows a LIVE marker in its header (title, category, viewers and
  uptime); once you are logged in the sidebar also lists the followed channels that are live, and dragging one
  into a pane opens it.
- **Hide the sidebar** with **⌘B**.

## Splits

Several channels can be on screen at once, side by side or stacked.

- **⌘D** splits the focused pane to the right, **⇧⌥⌘D** splits it downwards, **⇧⌘W** closes the pane. The same
  three buttons are in every pane header and in the *View* menu. At most 16 panes.
- **Focus:** click a pane, or **⌃⌘←/→/↑/↓** to move to the neighbour. The focused pane has an accent outline.
- **Resize** by dragging the line between two panes; every pane keeps a minimum width.
- **Rearrange:** drag a channel from the sidebar into a pane (the middle swaps or shows it, an edge puts it
  beside), or drag a pane by its channel name in the header.
- A channel is shown in at most one pane. Closing a pane only hides it: the channel stays open in the sidebar and
  keeps its scrollback.

## Logging in

Press **Log In to Twitch…** in the app menu (or *Settings → Account*). The app shows a code and opens
twitch.tv's activation page: enter the code there and approve. This is Twitch's *device code* flow: **no
password and no client secret ever reaches the app**. The login is kept in the macOS keychain; *Log Out* deletes
it and revokes the token.

A normal login asks only for what chatting needs:

| Permission | Used for |
|---|---|
| `chat:read`, `chat:edit`, `user:read:chat`, `user:write:chat` | reading and sending chat |
| `user:read:emotes` | your own subscriber and follower emotes in the emote picker and completion |
| `user:read:follows` | the followed-live list in the sidebar |

Two groups of permissions are **opt-in**. A normal login does not ask for them, nobody is asked to log in again
for them, and nothing breaks without them:

- **Moderation tools** (`moderator:manage:*`, `channel:manage:moderators`, `channel:manage:vips`,
  `channel:manage:raids`): timeouts, bans, deleting messages, room modes, announcements and the slash commands.
  Enable them with **Enable moderation tools…** (the account menu, *Settings → Account*, or the button on the
  strip that appears the first time you use a tool in a channel you moderate).
- **Block list sync** (`user:read:blocked_users`, `user:manage:blocked_users`): ignore the users on your Twitch
  block list here too, and block someone from a message's menu. Enable it with **Enable block list sync…**
  (the account menu, *Settings → Account* or *Ignores*).

Either one repeats the code step with the extra permissions; logging in again later keeps them. You can revoke
any of it at twitch.tv → Settings → Connections.

## Writing messages

Once logged in, the box under the chat sends on Enter. Messages go through Twitch's API like on twitch.tv, so
slow mode, follower-only mode and the other room rules apply, and a refusal is shown under the box.

- **Replies:** the *reply* action on a message (hover it, or right-click) puts the box in reply mode; Esc cancels.
- **Completion:** type `:` and part of an emote name, `@` and part of a chatter's name, or `/` for a command; ↑/↓
  choose, **Tab** or Enter accepts, Esc closes. **Tab** on a plain word offers emotes for it.
- **Emote picker:** **⌘E** (or the button next to the box) opens a panel with a tab per provider, a search
  field and the emotes you used last. Enter inserts the highlighted one.
- **History:** **↑ / ↓** in the box bring back what you sent before.

## Emotes and badges

Twitch's own emotes always work. Third-party emotes come from **7TV** (including zero-width emotes and animated
ones), **BetterTTV** and **FrankerFaceZ**, global and per channel; each provider has a switch in *Settings →
Emote providers*. 7TV and BetterTTV changes to a channel's set arrive live, without reloading. Badges: Twitch's
own, plus the optional 7TV, FrankerFaceZ and Chatterino badges and 7TV name paints (*Settings → Badges and
paints*).

- *Emote size* and *Animate emotes* are in the settings; animation also stops when macOS has **Reduce motion**
  on.
- Emotes missing in a channel? **Channel → Reload Emotes** fetches the sets again.
- Privacy: the providers see your IP address and, for the live updates, which channels you have open. Turning a
  provider off stops those requests. Details in [`SETTINGS.md`](SETTINGS.md#how-the-app-uses-it-pr-04).

## Mentions and highlights

- Your login (and your display name once you have written something) is highlighted wherever it appears, with
  or without `@`.
- **Highlight rules:** *Settings → Highlights* takes phrases and regular expressions with a colour each, and
  users whose every message you want to notice. A rule can also send a **system notification**.
- **Show Mentions** (**⇧⌘M**) opens a panel beside the chat with the last hits of every channel; clicking one
  jumps to the message in its channel.
- Notifications are rate limited (one every five seconds, the rest arrive as a summary), only fire when the
  window is in the background or the channel is not on screen, and can hide the sender and text
  (*notification previews*). macOS delivers them only to the installed `.app`, not to a build run from a
  terminal, and asks for permission the first time one is shown.

## Finding, filtering and ignoring

- **⌘F** opens the find bar over the focused pane: type to search the channel's messages, Enter / ⌘G for the next
  match, ⇧⌘G for the previous one, Esc to close. `from:name` shows one user's messages. **Mentions**, **Links**
  and **No bots** buttons filter further, and a strip says how many messages are shown, with *Clear filter*.
- **Ignoring:** right-click a message → *Ignore user*, or add users, phrases and regular expressions under
  *Settings → Ignores*. Ignored messages disappear everywhere: the chat, the mentions panel, unread dots and
  notifications (optionally as a one-line placeholder). Your own messages are never ignored.
- **Block list sync** (opt-in, see [Logging in](#logging-in)) adds your Twitch block list.
- **User cards:** click a name or an `@mention` for the card: names, badges, account age, follow date, what they
  said here, and *Copy username*, *Mention*, *Reply* and *Open profile*.

## Moderation

If you are the broadcaster or a moderator of a channel, and have enabled the moderation tools, messages get
timeout, ban and delete buttons, the user card offers timeout presets, ban and unban, the pane header gets a
room-mode menu (emote-only, followers-only, slow, subscribers-only, unique chat), and the `/timeout`, `/ban`,
`/unban`, `/delete`, `/clear` and similar commands work. Without the role, none of these is shown.

What moderators remove (a deleted message, a timeout, a ban, cleared chat) stays in the scrollback, struck
through and dimmed, or collapsed to `<message deleted>` (*Settings → Chat → Removed messages*), with a line
saying what happened.

## Watching the stream: streamlink

Each pane header has a play button that runs [streamlink](https://streamlink.github.io) for the channel in your
player. The menu next to it picks the quality for that pane.

1. Install both: `brew install streamlink` and a player such as mpv, IINA or VLC.
2. The app finds `streamlink` through your login shell and in `/opt/homebrew/bin`, `/usr/local/bin` and
   `~/.local/bin`. If yours is elsewhere (or set up only in `.zshrc`), set the path under *Settings →
   Streamlink*, or `streamlink.path` in `settings.toml`.
3. The same page sets the player, extra arguments (one per line, e.g. `--twitch-low-latency`) and the default
   quality.

Streams started from the app end when you press stop or quit the app. If streamlink fails, the header shows
its message with a dismiss button.

## Appearance

*Settings → Appearance* sets the font (default: JetBrainsMono Nerd Font if installed, else the bundled
JetBrains Mono), size, line height, whether the theme follows macOS or is light or dark, and the colour theme.
Bundled themes include Catppuccin, Tokyo Night, Rosé Pine, Gruvbox, Nord, Everforest, Kanagawa and more; your own
go in `~/.config/gpui-chat/themes/*.toml` and apply as soon as you save ([`THEMES.md`](THEMES.md)). *Chat* has
timestamps and scrollback.

## Keyboard shortcuts

Every action can be rebound in `~/.config/gpui-chat/keymap.toml` ([`KEYMAP.md`](KEYMAP.md)); this is what ships.
**⌘K** opens the command palette, which lists the actions with their current shortcut and runs them on Enter.

<!-- shortcuts:start (generated by the test the_guides_shortcut_table_matches_the_catalog; UPDATE_GUIDE=1 rewrites it) -->
| Group | Action | Shortcut |
|---|---|---|
| Go | Command Palette… | ⌘K |
| Go | Go to Channel… | ⌘T |
| Channel | Add Channel… | ⌘N |
| Channel | Remove Channel | ⌘⌫ |
| Channel | Move Channel Up | ⌥⌘↑ |
| Channel | Move Channel Down | ⌥⌘↓ |
| Channel | Next Channel | ⇧⌘] |
| Channel | Previous Channel | ⇧⌘[ |
| Channel | Reload Emotes | none |
| Pane | Split Right | ⌘D |
| Pane | Split Down | ⌥⇧⌘D |
| Pane | Close Pane | ⇧⌘W |
| Pane | Focus Pane Left | ⌃⌘← |
| Pane | Focus Pane Right | ⌃⌘→ |
| Pane | Focus Pane Above | ⌃⌘↑ |
| Pane | Focus Pane Below | ⌃⌘↓ |
| Chat | Copy Selection | ⌘C |
| Chat | Clear Selection | ⎋ |
| Chat | Open User Card for Selected Message | none |
| Chat | Find in Chat… | ⌘F |
| Chat | Find Next | ⌘G, ↓ |
| Chat | Find Previous | ⇧⌘G, ↑ |
| Chat | Close Find Bar | ⎋ |
| Chat | Clear Chat Filter | none |
| Chat | Show Only Mentions and Highlights | none |
| Chat | Show Only Messages with Links | none |
| Chat | Hide Bots | none |
| Chat | Ignore Author of Selected Message | none |
| Chat | Unignore Author of Selected Message | none |
| Chat | Block Author of Selected Message on Twitch… | none |
| Account | Enable Block List Sync… | none |
| View | Toggle Sidebar | ⌘B, ⌘\ |
| View | Show Mentions | ⇧⌘M |
| View | Toggle Debug HUD | ⇧⌘D |
| Account | Log In to Twitch… | none |
| Account | Log Out | none |
| App | Settings… | ⌘, |
| App | About | none |
| App | Check for Updates… | none |
| App | Minimize Window | ⌘M |
| App | Close Window | ⌘W |
| App | Quit | ⌘Q |
| Input | Previous Sent Message | ↑ |
| Input | Next Sent Message | ↓ |
| Input | Complete | Tab |
| Input | Complete Backwards | ⇧Tab |
| Input | Dismiss Completion | ⎋ |
| Input | Emote Picker | ⌘E |
| Emote Picker | Emote Left | ← |
| Emote Picker | Emote Right | → |
| Emote Picker | Emote Up | ↑ |
| Emote Picker | Emote Down | ↓ |
| Emote Picker | Next Emote Tab | Tab |
| Emote Picker | Previous Emote Tab | ⇧Tab |
| Emote Picker | Close Emote Picker | ⎋ |
<!-- shortcuts:end -->

*None* means the action has no default shortcut: use the menu or the palette, or bind one yourself.

## Settings and files

*Settings* (**⌘,**) writes `settings.toml` for you and follows hand edits of the file at once; a file with a
mistake keeps the last good values and says what is wrong in a banner.

| What | Where |
|---|---|
| Settings | `~/.config/gpui-chat/settings.toml` |
| Keymap | `~/.config/gpui-chat/keymap.toml` |
| Your themes | `~/.config/gpui-chat/themes/*.toml` |
| Window, open channels, layout | `~/Library/Application Support/gpui-chat/state.toml` |
| Cache (emote images, update check) | `~/Library/Caches/gpui-chat/` |
| Twitch login | the macOS keychain, service `dev.gpui-chat.app` |

`GPUI_CHAT_CONFIG_DIR` and `GPUI_CHAT_CACHE_DIR` point the app at other folders. Deleting a file resets what it
holds; deleting `~/.config/gpui-chat` gives you the defaults.

## Updates

The app asks GitHub, **at most once a day**, whether a newer release is published and shows a notice with a link
to its page. It never downloads or installs anything: you get the new version the way you got this one
(the DMG, or `brew upgrade --cask gpui-chat`). *Settings → Updates → Check for updates automatically* turns
the daily request off, and with it off the app makes no such request on its own. **gpui-chat → Check for
Updates…** checks when you ask, whatever that setting says. Dismissing a notice keeps it quiet for that release.

## Troubleshooting

**macOS says the app is from an unidentified developer / cannot be opened.** Releases are signed ad hoc until a
Developer ID certificate exists. Right-click the app → *Open* once (on macOS 15 and later: try to open it, then
*System Settings → Privacy & Security → Open Anyway*), or run
`xattr -dr com.apple.quarantine /Applications/gpui-chat.app`.

**No messages arrive / "You appear to be offline".** The strip above the chat means chat, emotes and badges
cannot reach their servers; check your connection or VPN. The app keeps retrying by itself. A channel that
shows nothing may simply be quiet: pick a busy one to check.

**Chat starts empty.** On join the app asks a third-party service (recent-messages.robotty.de) for the last
messages; it only knows channels it records. Switch it off, with the privacy trade-off, under *Settings → Chat →
Load recent messages*.

**Logging in fails or the login is forgotten.** The code is valid for a few minutes: start again. "Could not read
the saved Twitch login" means the keychain refused access; allow it when macOS asks, or log in again (a login
only lasts for the run if the keychain is unavailable). Ad-hoc signed builds change identity with each version,
so macOS may ask again after an update.

**"Log in again to enable moderation".** The tool needs the opt-in permissions: use *Enable moderation tools…*.
Likewise *Enable block list sync…* for the block list.

**Emotes are missing or show as text.** *Channel → Reload Emotes*. Check that the provider is on under *Settings
→ Emote providers*. A strip says when a provider could not be reached.

**The play button says streamlink was not found.** Install it (`brew install streamlink`) or set its path under
*Settings → Streamlink*. A path you set is used as is; the app never falls back to another binary.

**No notifications.** They work only from the installed `.app` (not `cargo run`), need permission in *System
Settings → Notifications*, and are off while the channel is in front of you in the active window.

**A strip says a settings, keymap or theme file has a problem.** It names the file and the line. The app keeps
what worked before; fix the file and the strip goes by itself.

**Where are the logs?** The app logs to standard error. Run it from a terminal to see them:

```bash
/Applications/gpui-chat.app/Contents/MacOS/gpui-chat
RUST_LOG=gpui_chat=debug /Applications/gpui-chat.app/Contents/MacOS/gpui-chat   # more detail
```

**Check for Updates… says it could not check.** GitHub could not be reached, or it is limiting requests from your
network (the message says when to try again). Nothing else is affected.

**Starting over.** Quit the app and remove `~/.config/gpui-chat`, `~/Library/Application Support/gpui-chat` and
`~/Library/Caches/gpui-chat`; *Log Out* first if you also want the login revoked. `brew uninstall --zap --cask
gpui-chat` removes the same folders for a Homebrew install.

Found a bug? Open an issue at <https://github.com/charlie-niekirk/gpui-chat/issues>; the *Copy details* button
on a strip puts the text for a report on the clipboard.
