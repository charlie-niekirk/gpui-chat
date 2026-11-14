# Settings

`crates/settings` (`gpui-chat-settings`) owns `settings.toml`: schema, load,
atomic save, hot reload and paths. No GPUI, no async runtime.

## Locations

| What | Where |
|---|---|
| Config dir | `$GPUI_CHAT_CONFIG_DIR`, else `$XDG_CONFIG_HOME/<APP_NAME>` (absolute only), else `~/.config/<APP_NAME>/` (macOS and Linux alike) |
| Settings | `<config>/settings.toml` |
| Keymap | `<config>/keymap.toml` (PR 4.3) |
| Themes | `<config>/themes/*.toml` (PR 4.4) |
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
theme_name = "default"                     # colour theme (PR 4.4)

[chat]
scrollback = 1000                          # 10 to 100000
timestamps = true

[emotes]
scale = 1.0                                # 0.5 to 4.0; a Twitch emote is 28 px at 1.0
animate = true                             # false: animated emotes show their first frame
seventv = true                             # 7TV emotes (channel + global, zero-width included)
bttv = true                                # BetterTTV emotes (channel + global)
ffz = true                                 # FrankerFaceZ emotes (channel + global)
```

## Compatibility rules

- **Every key is optional.** All structs are `#[serde(default)]`; an empty or
  absent file means defaults. Adding a field is never a breaking change.
- **Unknown keys are ignored** on load, so a file written by a newer build still
  loads in an older one. They are *not* preserved when the app saves (saving
  rewrites the whole file from the typed struct, dropping comments too).
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
