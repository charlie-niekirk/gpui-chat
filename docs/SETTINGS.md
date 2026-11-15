# Settings

`crates/settings` (`gpui-chat-settings`) owns `settings.toml`: schema, load,
atomic save, hot reload and paths. No GPUI, no async runtime.

## Locations

| What | Where |
|---|---|
| Config dir | `$GPUI_CHAT_CONFIG_DIR`, else `$XDG_CONFIG_HOME/<APP_NAME>` (absolute only), else `~/.config/<APP_NAME>/` (macOS and Linux alike) |
| Settings | `<config>/settings.toml` |
| Keymap | `<config>/keymap.toml` (PR 4.3) |
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

[emotes]
scale = 1.0                                # 0.5 to 4.0; a Twitch emote is 28 px at 1.0
animate = true                             # false: animated emotes show their first frame
seventv = true                             # 7TV emotes (channel + global, zero-width included)
bttv = true                                # BetterTTV emotes (channel + global)
ffz = true                                 # FrankerFaceZ emotes (channel + global)

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
applies the events to the store on the UI thread, once per frame, with one notify per batch. The
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

`appearance.line_height` and `chat.timestamps` also apply live (the chat view re-lays out its
messages). `chat.scrollback` is read once at startup.

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
