# Settings

`crates/settings` (`gpui-chat-settings`) owns `settings.toml`: schema, load,
atomic save, hot reload and paths. No GPUI, no async runtime.

## Locations

| What | Where |
|---|---|
| Config dir | `$GPUI_CHAT_CONFIG_DIR`, else `$XDG_CONFIG_HOME/<APP_NAME>` (absolute only), else `~/.config/<APP_NAME>/` (macOS and Linux alike) |
| Settings | `<config>/settings.toml` |
| Keymap | `<config>/keymap.toml`, watched (see `docs/KEYMAP.md`) |
| Themes | `<config>/themes/*.toml`, watched (see `docs/THEMES.md`) |
| Cache | `$GPUI_CHAT_CACHE_DIR`, else `dirs::cache_dir()/<APP_NAME>` |

`<APP_NAME>` is `gpui_chat_core::APP_NAME`. Tests build a `SettingsPaths` with
`SettingsPaths::new` / `with_base_dir` instead of touching the environment.

## File format

```toml
schema_version = 1

[appearance]
font_family = "JetBrainsMono Nerd Font"   # head of the font chain
font_fallbacks = ["JetBrains Mono", "monospace"]
font_size = 14.0                           # 6 to 72
line_height = 1.4                          # 1.0 to 3.0
theme = "system"                           # system | light | dark
theme_name = "default"                     # colour theme: bundled id or themes/<name>.toml (docs/THEMES.md)
# theme_name_light = "rose-pine"           # optional: theme for the light variant (follow-system pairing)

[chat]
scrollback = 1000                          # 10 to 100000
timestamps = true
history = true                             # load recent messages on join / reconnect (sends the channel name to a third party)
deleted_messages = "strikethrough"         # how removed messages show: strikethrough (dimmed, in place) | collapse ("<message deleted>", click to show)

[emotes]
scale = 1.0                                # 0.5 to 4.0; a Twitch emote is 28 px at 1.0
animate = true                             # false: animated emotes show their first frame
seventv = true                             # 7TV emotes (channel + global, zero-width included)
bttv = true                                # BetterTTV emotes (channel + global)
ffz = true                                 # FrankerFaceZ emotes (channel + global)

[badges]                                   # PR 6.2: besides Twitch's own badges
seventv = true                             # 7TV badges
ffz = true                                 # FrankerFaceZ badges (bot, supporter, ...)
chatterino = true                          # Chatterino badges
paints = true                              # 7TV name paints (gradient / shadow usernames)

[highlights]                               # PR 5.3, see "Highlights and notifications" below
mentions = true                            # highlight messages that say your login / display name
mention_color = "accent"                   # accent | red | green | yellow (theme colours)
mention_notify = true                      # system notification for a mention
notifications = true                       # master switch for system notifications
notification_previews = true               # false: "New highlight in #channel", no name or text

[[highlights.rules]]                       # any number; phrases and regular expressions
kind = "phrase"                            # phrase (plain text, any case) | regex
pattern = "nice play"
whole_word = true                          # phrases only: whole words (default) or anywhere
color = "green"
notify = false

[[highlights.users]]                       # everything they say is highlighted
login = "streamer"
color = "yellow"
notify = true

[ignores]                                  # PR 5.7, see "Ignores, filters and the block list" below
users = ["some_spammer"]                   # logins whose messages are hidden everywhere
show_collapsed = false                     # true: an ignored message is one line, "1 ignored message", that a click opens
sync_block_list = true                     # also ignore the users on your Twitch block list (needs "Enable block list sync")

[[ignores.rules]]                          # any number; phrases and regular expressions
kind = "phrase"                            # phrase (plain text, any case) | regex
pattern = "buy followers"
whole_word = true                          # phrases only: whole words (default) or anywhere

[filters]
bots = ["nightbot", "streamelements"]      # what the find bar's "No bots" filter hides (the default list has seven usual chat bots)

[updates]                                  # PR 7.2, see "Update check" below
check_automatically = true                 # ask GitHub once a day whether a newer release exists (a notice only; nothing is downloaded)

[streamlink]
# path = "/opt/homebrew/bin/streamlink"   # unset: found automatically (see below)
# player = "mpv"                          # --player; unset: streamlink's default player
extra_args = []                            # one argument per entry, e.g. ["--twitch-low-latency"]
default_quality = "best"                   # best, 1080p60, 720p, audio_only, "720p60,720p,best", ...
```

## Compatibility rules

- **Every key is optional.** All structs are `#[serde(default)]`; an empty or
  absent file means defaults. Adding a field is never a breaking change.
- **Unknown keys are ignored** on load, so a file written by a newer build still
  loads in an older one. The settings window keeps them (and comments) when it
  changes a value (see "Editing from the settings window"); only
  `Settings::save`, which rewrites the whole file from the typed struct, drops them.
- `schema_version` is bumped only for changes a default cannot express (rename,
  changed meaning). A file with a *newer* version still loads (with a
  `tracing` warning).
- **Unknown enum values fall back.** `appearance.theme` accepts `system`,
  `light`, `dark`; any other string reads as `system`, so a value from a newer
  build does not reject the file.
- A file with no `schema_version` reads as version 1; `save` always writes the
  current version.
- **Bad files never win.** Invalid TOML, a wrong type
  or an out-of-range number gives a `SettingsError` (with line and column where
  the parser knows them). The caller keeps defaults at startup and the last
  good value on reload. The file is never modified by loading. All-or-nothing:
  one bad value rejects the whole file, so the error message is the single
  thing to fix.
- A missing file is not an error; deleting the file resets to defaults.

## Saving

`Settings::save` writes `.<name>.<pid>.<n>.tmp` in the same directory, fsyncs
it, renames it over the target, then fsyncs the directory. A symlinked
`settings.toml` is written through, not replaced. **Do not save over a file that
failed to load** unless the user asked for it; see `docs/BACKLOG.md`.

## Editing from the settings window (PR 4.2)

`Settings::update_file(path, &[SettingEdit])` (`edit.rs`) is how the UI changes the file. It parses
the file with `toml_edit`, sets or removes just the named keys (`"appearance.font_size"`), and writes
the result with the same atomic write as `save`. Everything else stays byte for byte: comments,
blank lines, key order, unknown keys and sections, and the comment after an edited value. A section
that does not exist is added; a missing file is created.

Nothing is written unless the **result** is valid settings, so:

- a file that is not valid TOML is never touched (the error has line and column);
- a file with one out-of-range value only accepts an edit that fixes that value; any other edit is
  refused with the reason, and the file is left alone;
- a value outside its range (`font_size = 500`) is refused before it reaches the file.

The settings window shows a file that does not load in a banner (the controls keep the last good
values) and shows a refused change next to the control or in the banner. Edits made while the file
is open elsewhere are a read-modify-write: an editor saving in the few milliseconds between our
read and rename is overwritten by our (otherwise identical) text.

Known limits of the in-place edit:

- A value is rewritten as a whole, so comments **inside** a multi-line array
  (`streamlink.extra_args = [ ... ]`) are lost when that array is changed. A
  comment after the closing `]` on the same line stays.
- A comment at the very end of the file belongs to the last table; when an edit
  adds a new section (`[emotes]`, say) it is written after the existing text, so
  that trailing comment ends up above the new section instead of at the end.
- A whole number replacing an integer literal (or a new key) is written as an
  integer (`font_size = 16`), a float literal stays a float (`14.0`).
- Text typed into a field of the settings window is committed when the field loses
  focus or Enter is pressed (stepping a number commits at once); closing the
  window (button or ⌘W) and quitting commit what is still being typed. A refused
  value (out of range, or the file is invalid) is not saved and not kept.

`appearance.theme`, `theme_name` and `theme_name_light` are written as plain strings; the theme list
is `theme::choices` (bundled pack plus `themes/`), and "Same as Theme" for the light theme removes
`theme_name_light`.

## Watching

`watch` / `watch_channel` watch the config *directory* (so rename-over saves
from editors are seen), filter to `settings.toml`, debounce (default 200 ms),
reload, and emit `SettingsEvent::Reloaded(Settings)` only if the result differs
from the last one reported (except the first successful load after a `Failed`,
which is always reported so the UI can clear its error), or `SettingsEvent::Failed(SettingsError)` (repeats of
the same error are suppressed). The worker is a plain thread; it ends when the
`SettingsWatcher` is dropped. The app's own saves also produce a `Reloaded`
event when the content differs from the baseline; observers should treat equal
settings as a no-op.

## How the app uses it (PR 0.4)

`crates/app/src/config.rs` loads `settings.toml` at startup into a `SettingsStore` entity
(settings in effect + the last load error), exposed as a GPUI global:

```rust
let store = config::store(cx);          // Entity<SettingsStore>, for cx.observe / observe_in
let settings = config::current(cx);     // &Settings in effect
```

The watcher's callback pushes each `SettingsEvent` into a bounded `bridge` channel; the bridge
applies the events to the store on the UI thread (a foreground task, not a frame callback, so it works while the main window is minimised), with one notify per batch. The
app shell observes the store and calls `theme::apply_settings`, so **font family, font size and
theme (`system` / `light` / `dark`) change live** when the file is edited. A file that fails to
load keeps the previous settings in effect and shows a banner with the file path and the parse
message; it disappears when the file loads again. The OS light/dark appearance only drives the
theme while `appearance.theme = "system"`.

`appearance.font_family` and `font_fallbacks` are validated against the installed font families
(first installed entry wins, then the built-in chain) so a missing family never reaches GPUI.
`emotes.seventv`, `emotes.bttv` and `emotes.ffz` (PR 2.4) switch a third-party provider off or on
without a restart: a provider that is off is not requested, and its emotes are not resolved in
messages that arrive afterwards (messages already on screen keep what they were resolved with).
Turning a provider on reloads the emote sets, since it may never have been fetched; Twitch's
own emotes are always on. Files without these keys read as all on.

`badges.seventv`, `badges.ffz`, `badges.chatterino` and `badges.paints` (PR 6.2) apply live too: a
provider that is off hides its badges at once (on messages already on screen as well), `paints`
draws every name in its plain colour. BetterTTV's badges are not offered: they are SVG images and
the image pipeline draws bitmaps only. A paint that would be hard to read on the current theme is
drawn in the plain colour as well. With `emotes.seventv` (live emote updates) and the 7TV
cosmetics both off, no connection to 7TV's EventAPI is made (and with `badges.seventv` and `badges.paints` both off no chatter id is sent to 7tv.io; with either on, the Twitch ids of the chatters you see are looked up there in batches). The sockets also tell the providers which channels you have open: 7TV's EventAPI and BTTV's socket receive the Twitch room id of every open channel (and, for 7TV, the ids of its emote set and owner), along with your IP address, for as long as the provider is on; likewise none to BTTV's socket with
`emotes.bttv` off.

`appearance.line_height` and `chat.timestamps` also apply live (the chat view re-lays out its
messages). `chat.scrollback` is read once at startup.

### Message history (`chat.history`, PR 5.5)

With `chat.history = true` (the default) a channel shows its recent messages as soon as it is joined,
and after a reconnect the messages missed meanwhile are filled in. They come from the public
[recent-messages service](https://recent-messages.robotty.de/) (`recent-messages.robotty.de`, run by a
third party, not by Twitch or by this app), which keeps the last few hundred messages of the channels
it has been asked about. They are shown dimmed so they are not mistaken for live chat, and never
raise a notification.

**Privacy:** to get them the app sends *the name of the channel you joined* (and your IP address, as
any web request does) to that service. It sends no account, token or message of yours. The service
only has history for channels it records, and it starts recording a channel the first time anyone asks
for it: joining a small channel with this on can make that third-party bot start logging the
channel's chat from then on (its first answer is "not tracked yet", so that first join shows no
history). Set `chat.history = false` (or switch off *Chat → Load recent messages*) to never contact it; chat then
starts empty at the moment you join, as on twitch.tv. The setting is read when a channel is joined or
re-joined, so changing it affects the next join or reconnect, not channels already open. If the service
is slow, down or refuses (it is a volunteer-run service), joining is not delayed and the channel shows a
single quiet line saying history could not be loaded.

### Removed messages (`chat.deleted_messages`, PR 5.2)

What a moderator removing a message (deleting it, timing out or banning its author, clearing the chat) looks like:

- `"strikethrough"` (the default): the message stays where it is, dimmed and struck through.
- `"collapse"`: it becomes one muted line, `name: <message deleted>`, until you click it. A moderator's own view uses the same
  setting. *Chat → Moderation → Removed messages* in the settings window.

Either way a system line says what happened ("alice was timed out for 10m.", "alice was banned.", "Chat was cleared by a
moderator."), for live chat and for recent-messages history alike. A value this build does not know falls back to
`"strikethrough"`.

Moderation tools (buttons, menu items, the room-mode menu, `/timeout` and the other commands) appear only for a moderator or the
broadcaster of the channel. They need Twitch permissions a normal login does not ask for (least privilege, and nobody who only
watches chat is asked to log in again): choose **Enable moderation tools…** (account menu, *Settings → Account*, or the button on
the strip that appears when you first use a tool) and approve the extra permissions. Logging in again later keeps them.

## Highlights and notifications (PR 5.3)

`[highlights]` says which messages to notice. The matching is `gpui_chat_core::highlight` (pure code, tested
there); the app stamps the verdict on each message **as it arrives** (`MessageFlags::highlight`, and `mention`
for your own name), so editing the rules recolours what comes next, not what is already on screen.

- **Mentions** (`mentions`, on by default): your login, and your display name once the app has seen one of your
  own messages (the login is all the account says), as a whole word in any case, with or without `@`. History
  lines are checked too (their parser does not know you), so a mention in recent messages is drawn as one, but
  history never counts or notifies.
- **Rules** (`[[highlights.rules]]`): `kind = "phrase"` is plain text compared ignoring Unicode case (`É` = `é`);
  with `whole_word = true` (the default) a letter, digit or `_` may not touch either end, except where the phrase
  itself starts or ends with a symbol (`c++`, `!hype`). `kind = "regex"` is the Rust `regex` syntax, searched
  unanchored and ignoring case (write `(?-i)` to match case). **A regular expression cannot stall chat:** the
  engine's search time is linear in the message however the pattern is written (no backtracking), patterns are
  compiled with a size limit, and one that matches the empty text (so every message, e.g. `.*`) is refused. Text is
  matched as typed: an emote is its name, so `pog` as a whole word does not match the emote `PogChamp`.
  **Limits:** a pattern is at most 500 characters *as typed* (phrases and regexes alike); one rule's compiled size is capped
  (256 KiB) and all regular expressions together share a 4 MiB budget (they are searched in one pass with a `RegexSet`), so
  200 rules cannot take hundreds of megabytes; over the budget none of the regular expressions is used and the problem says
  so.
- **Unicode:** case is ignored by *simple* Unicode case folding, exactly what the `regex` crate does: `É` = `é`, `Σ` = `ς`,
  `ß` = `ẞ`. It is not full folding: `STRASSE` does **not** match `straße`, and the Turkish letters are their own: `istanbul`
  does not match `İSTANBUL` or `ıstanbul`, while `İstanbul` matches `İSTANBUL`. Write a second rule if you need both.
- **Users** (`[[highlights.users]]`): the login, case and a leading `@` ignored.
- **Colours** are the theme's own (`accent`, `red` = `danger`, `green` = `success`, `yellow` = `warning`), so every
  theme and variant gets a suitable tint; a value this build does not know reads as `accent`. A hit draws a tint and
  a left bar in that colour (a removed message is not tinted).
- **Priority:** your name, then rules in file order, then users. The first match picks the colour; the message
  notifies if *any* matching rule says `notify`. Your own messages never match.
- **Validation:** a rule that cannot be used makes the file invalid like any other bad value (the previous settings
  stay, the banner says `rule 2: `(`: unclosed group`). At most 200 rules and 500 users. The settings window's
  Highlights page edits these through `update_file` (`EditValue::Tables` writes the `[[...]]` blocks; replacing a list
  replaces all its blocks, so comments inside them are lost).

**What counts:** only events for a channel that is open. A line that arrives for a channel just closed (the part is still on
its way) or never opened is ignored, and in shared chat (PR 5.1) the *copy* of a line that another open channel shows too is
drawn but not counted, so one line is one dot, one Mentions row and one notification, not one per channel. A line a moderator
removes (a delete, a timeout, a ban, a clear) leaves the Mentions log, is taken out of a held summary and its banner in
Notification Center is dismissed.

**Unread and mention dots** (sidebar): a channel is *watched* while a pane shows it and the window is in front. A live
message (not history, not yours, not removed) in any other channel makes its dot; a highlight hit among them becomes a
count pill (`9+` above nine). Showing the channel in the **focused** pane while the window is in front clears both
(switching to it, focusing its pane, coming back to the window); a channel in an unfocused pane stops counting new
messages but keeps its backlog until focused. **Show Mentions** (View menu, ⇧⌘M, ⌘K) opens a panel beside the chat with
the last 500 hits of every channel, newest first (the 200 newest are listed; the list is virtualised and rebuilt only when the
log changes); clicking a row shows the message in its channel: the list pauses on it (new messages wait behind the "new
messages" pill until you scroll or use it), the row flashes briefly, and a message that has left the scrollback says so.

**Notifications** go through GPUI (`App::show_system_notification`, `UNUserNotificationCenter` on macOS) and are raised
for a hit whose rule says `notify` (mentions: `mention_notify`) when the window is in the background *or* the channel is
not on screen, unless `notifications = false`. At most one every 5 s: hits in between are counted and arrive as one
"N more highlights in #a, #b" (nothing held fires after `notifications` is switched off). `notification_previews = false`
leaves the author and the text out: the notification only says "New highlight in #channel", for a locked screen. Clicking one brings the app forward and shows the channel and message (the summary opens
the Mentions panel). macOS is asked for permission the first time a notification is shown, never at startup. macOS only
delivers notifications to an app **inside a `.app` bundle**: from `cargo run` GPUI logs
`system notifications disabled: not running from an app bundle` (it shows in the log, below) and does nothing; highlights, dots and the Mentions
panel work either way. Offline runs (`--firehose`, `--replay`) act as the login `viewer`.

## Ignores, filters and the block list (PR 5.7)

`[ignores]` says which messages the user does not want to see. The matching is `gpui_chat_core::ignore` (pure code,
tested there) and shares its conventions with the highlight rules above: phrases are plain text compared ignoring
Unicode case (whole words unless `whole_word = false`), `kind = "regex"` is the Rust `regex` syntax (linear-time search,
size limits, a pattern that matches the empty text is refused, 500 characters, 200 rules, 500 users), emotes count by their
name. A bad rule makes the file invalid like any other bad value (`ignores.rules` / `ignores.users` in the banner).

- **What is ignored.** A chat message or `/me` line from an ignored user, or whose text matches a phrase or regex. An
  event card (a sub, a raid) only when its *user* is ignored: phrases are for what people type. Notices and system lines
  never. **Your own messages never**, whatever the rules say.
- **Hidden everywhere.** No row in the chat, no Mentions entry, no unread or mention dot, no notification. The ignore is
  asked about a message whenever it is drawn or counted, not stamped when it arrives, so ignoring a user hides what they
  already said and unignoring brings it back (it is still in the scrollback); an entry already in the Mentions log leaves it
  when its author is ignored. A message that was already counted as unread before the ignore is not un-counted.
- **Collapsed.** `show_collapsed = true` draws each ignored message as one muted line, `1 ignored message` (no name, no text),
  that a click opens. The line is only a placeholder: it is not searched, not copied with a selection and has no menu until it is
  opened. A chat with a find-bar filter on shows no placeholders.
- **Where to ignore.** The settings window's *Ignores* page (one entry per line; phrases take `{anywhere}`), the right-click
  menu of a message (*Ignore user* / *Unignore user*), the user card, and the `IgnoreUser` / `UnignoreUser` actions (the author
  of the selected message; ⌘K).
- **Twitch block list.** With `sync_block_list` and a login that was granted `user:read:blocked_users`, the app reads the block
  list (`GET /helix/users/blocks`, once per login and after a block or unblock) and ignores those users too (by Twitch id and
  login), so someone who renamed is still ignored. *Block on Twitch…* (message menu, user card, `BlockOnTwitch`; asks first)
  calls `PUT /helix/users/blocks` (`user:manage:blocked_users`), *Unblock on Twitch* the `DELETE` of the same path. **Both
  scopes are opt-in**, like the moderation ones: a normal login does not ask for them, nobody is told to log in again, and
  they are requested together by **Enable block list sync…** (account menu, *Settings → Account* or *Ignores*, or the button on
  the strip a failed block shows). Logging in again later keeps them. Without them nothing is requested and nothing fails: the
  page says the list is not synced, and a block attempt says how to enable it. The list is fetched again every 15 minutes while sync is on, after a failure with a growing
  pause (30 s up to 15 min) and by the **Refresh** button on the Ignores page. You can never ignore or block yourself. Offline runs
  never use an account.

`[filters] bots` is the list the find bar's **No bots** filter hides (the default is nightbot, streamelements, streamlabs,
moobot, fossabot, wizebot, sery_bot); a bot's *chat* lines are hidden, its event cards are not.

**Find and filters.** ⌘F (Ctrl-F elsewhere) opens a find bar over the focused pane's chat: type to search the channel's
messages (case-insensitive, the author's name and login, the body with emotes by code), the bar shows `3 of 12`, Enter / ↓ goes to
the next (newer) match, Shift-Enter / ↑ to the previous (older), wrapping; matches are drawn behind the text in the theme's warning
colour and the current one stronger; Esc closes the bar and gives the input box focus back. `from:login` in the text is a filter
(only that user's messages), not a search term; it applies once the token is finished (a space, Enter, or a short pause), not on each
letter. **Mentions**, **Links** and **No bots** toggle the other filters (mentions and
highlights only, messages with a link only, bots hidden). Filtering hides the rows that do not match and a strip says
`Showing 12 of 1000 messages · from alice · mentions` (`(incl. ignored)` after the count while anything is ignored) with **Clear filter**, whether the bar is open or not (Esc closes the
bar, not the filters). Clicking a Mentions row for a message a filter hides clears the filter first. The search keeps an index of
the channel's text and updates it incrementally (typing looks only at what matched before, a new message is checked once),
so it stays smooth at the largest scrollback (100 000) and at `--firehose 1000`.

## Logging

The app logs to stderr: `tracing` events and, because `tracing-subscriber` bridges the `log` crate, GPUI's own `log` records
(why a notification could not be shown, a denied authorisation). Default filter `warn,gpui_chat=info,gpui_macos=info`;
`RUST_LOG` overrides it (`RUST_LOG=gpui_macos=debug,gpui_chat=debug cargo run`).

## Twitch account (PR 3.1)

The login is **not** in `settings.toml` (no settings change in this PR). It lives in the system credential
store, through `keyring`:

| What | Where |
|---|---|
| Keychain item (macOS) | generic password, service `dev.gpui-chat.app` (`APP_ID`), account `twitch` |
| Item content | one JSON record: access token, refresh token, expiry (Unix s), granted scopes, login, user id |
| `$GPUI_CHAT_KEYCHAIN_SERVICE` | use another service name (test runs that must not touch the real login) |
| `$GPUI_CHAT_TWITCH_CLIENT_ID` | use another Twitch application's Client ID (public client, no secret) |

Logging out deletes the item and revokes the token. Remove a test item by hand with
`security delete-generic-password -s <service> -a twitch`. With a scratch `$HOME` macOS finds no default
keychain: the app then shows "Could not read the saved Twitch login" and a login lasts only for that run.
`--firehose` / `--replay` never read the keychain.

## Streamlink (PR 1.6)

The play button in a pane header runs
`streamlink [--player <player>] <extra_args…> twitch.tv/<channel> <quality>`.

- **Finding the binary.** `streamlink.path` if set (`~/` is expanded). A set path that is not
  an executable file is an error shown in the pane header; it never falls back to another
  binary. Unset: `command -v streamlink` in the login shell (`$SHELL -l -c`, 3 s limit; a GUI
  app does not inherit the shell's `PATH`), then `/opt/homebrew/bin`, `/usr/local/bin`,
  `~/.local/bin`. Nothing found shows the hint `brew install streamlink`. The login shell is
  non-interactive, so it reads `.zprofile` / `.profile` but **not `.zshrc`**: an install whose `PATH`
  is set up only there (pyenv, pipx or asdf shims, say) is not found; set `streamlink.path` for
  those. The result is
  remembered until the binary disappears or `path` changes, so installing streamlink needs no
  restart.
- **`player`** goes to `--player` as one argument (spaces are fine); it is a program, not a
  command line. Player arguments go in `extra_args` as `--player-args` plus its value.
- **`extra_args`** is a list, so no shell quoting is involved. Blank entries are dropped.
- **`default_quality`** is the quality a pane starts with. The quality menu in the header
  chooses another one for that pane (not saved); the value must not be empty, contain
  whitespace or start with `-`, otherwise the whole file is rejected like any other bad value.
- Changes apply the next time play is pressed; a running stream is not restarted.
- The `[streamlink]` section is new: a file without it loads with the defaults above.

## Update check (PR 7.2)

`updates.check_automatically` (default `true`; a file without the section means on) lets the app ask
GitHub, **at most once a day**, whether a newer release of this repository is published. The request is
`GET https://api.github.com/repos/<owner>/<repo>/releases` (unauthenticated; GitHub sees your IP address
and a `User-Agent` of `<APP_NAME>/<version>`, nothing else). A newer *published, non-draft,
non-prerelease* release with a semver tag (`v0.2.0`) shows a notice with a link to its page; nothing is
downloaded or installed.

- **Off** makes the app send **no** such request on its own, ever; switching it on checks at once if the
  last answer is a day old. The *Check for Updates…* menu item (app menu, and the command palette) checks
  when you choose it, whatever this setting says.
- The automatic check runs after startup and then once a day while the app stays open. Offline runs
  (`--firehose`, `--replay`, `--bench`) never check on their own.
- The answer, GitHub's `ETag` and the release whose notice you dismissed are remembered in
  `update-check.json` in the cache directory (see Locations); delete it to forget them. A `304 Not
  Modified` costs no body. When GitHub says the rate limit is used up (`403` / `429`), the app waits
  for the time it names (at most a day). After a failed request (no network, an error status, an answer
  that is not a list of releases) the *automatic* check waits an hour.
- A dismissed notice is not shown again for that release (it comes back for a newer one);
  *Check for Updates…* always reports.
- `GPUI_CHAT_UPDATE_API` points the check at another server speaking the same API (for trying the notice
  against a local copy of `fixtures/updates/`); nothing else reads it.
