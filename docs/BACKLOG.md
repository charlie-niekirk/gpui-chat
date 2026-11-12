# Backlog

Ideas and follow-ups that are out of scope for the PR that surfaced them. Append
an entry here instead of expanding a PR. Format:

```
- **Short title** (found in PR x.y) — one or two sentences of context.
```

<!-- Entries below -->
- **Preserve comments and unknown keys on save** (found in PR 0.3) — `Settings::save` rewrites the whole file; switch to `toml_edit` for the settings UI (PR 4.2) so user comments and keys from newer versions survive.
- **Protect a broken settings file from being overwritten** (found in PR 0.3) — loading never touches it, but a later `save` (settings UI) would clobber it; back it up to `settings.toml.bak` or require confirmation when the last load had an error.
- **Watch through symlinked settings.toml** (found in PR 0.3) — the watcher watches the config dir, so edits to a symlink target in another directory (dotfile managers) are not seen; also watch the canonical target's parent.
- **Per-field tolerance on load** (found in PR 0.3) — one wrong value currently rejects the whole file; could fall back per field/section and report each problem.
- **Newer-schema files lose unknown keys on save** (found in PR 0.3) — `save` stamps the current `SCHEMA_VERSION` (so a file from a newer build is downgraded) and drops keys this build does not know; decide on a refuse-to-save or backup policy.
- **Watcher debounce has no max-wait** (found in PR 0.3) — a steady stream of events, including sibling-file events in the config dir, keeps resetting the debounce, so a reload can be postponed indefinitely; add a max-wait and filter before resetting.
- **Generalise the watcher** (found in PR 0.3) — it is hard-wired to `Settings::reload` and uses two threads per watcher (notify's plus the worker); make it generic over a loader so keymap (4.3) and themes (4.4) can share one watcher.
- **Join the watcher worker on drop** (found in PR 0.3) — dropping `SettingsWatcher` ends the worker but does not wait for it, so an in-flight reload/callback can still run briefly afterwards.
- **Preserve file mode on atomic save** (found in PR 0.3) — the temp file gets default permissions, so a user's chmod on `settings.toml` is lost on save.
- **Re-check for edits between load and `watch()`** (found in PR 0.3) — a change made after the initial load but before the watch starts is not noticed until the next event; reload once right after the watch is established.
- **Persist sidebar collapsed state and width** (found in PR 0.2) — `state.toml` only stores window placement; sidebar collapse resets on launch.
- **Set up a `tracing` subscriber** (found in PR 0.2) — the app logs via `tracing` (e.g. resolved font family) but nothing installs a subscriber yet, so output is dropped.
- **Per-glyph font fallback cascade** (found in PR 0.2) — the font chain picks one installed family; add `FontFallbacks` (e.g. Nerd Font → JetBrains Mono → emoji/CJK) once chat text is shaped by the custom element.
- **Custom About window** (found in PR 0.2) — About is a Kit dialog showing name/version/ID; a proper panel with licences (incl. OFL notice for JetBrains Mono) can come later.
- **Maximized state is never saved on macOS** (found in PR 0.2) — gpui-pre 0.3.12's macOS `window_bounds()` reports only `Windowed`/`Fullscreen`, so `Mode::Maximized` in `state.toml` is never written; revisit if upstream reports zoomed windows.
- **Verify restore on a secondary display with a non-zero screen origin** (found in PR 0.2) — GPUI's macOS `bounds()` mixes the screen's y origin into the display-relative y; round trip is verified on the primary display only.
- **Badge info in messages** (found in PR 1.1) — `BadgeId` carries only set and version; the IRC `badge-info` tag (e.g. subscriber months for the tooltip) is not modelled.
- **Replace hand-rolled emoji and TLD tables** (found in PR 1.1) — `tokenizer/emoji.rs` and `tokenizer/links.rs` embed range/TLD tables to keep core dependency-free; consider `unicode-segmentation`/emoji data and a public-suffix list if they drift (new gTLDs and IDN TLDs are not recognised for bare domains).
- **Links: IPv6, userinfo, non-ASCII TLDs** (found in PR 1.1) — only hostnames and IPv4, optional port; `http://[::1]/` and `user@host` URLs are left as text.
- **Mentions by display name** (found in PR 1.1) — own-login detection matches the login only (ASCII case-insensitive), not a differently spelled non-ASCII display name.
- **Strip reply `@user` prefix** (found in PR 1.1) — Twitch prepends `@parent ` to reply text; mapping (PR 1.2) should drop it before tokenizing, or the UI hides it when `reply` is set.
- **Animated Twitch emotes** (found in PR 1.1) — `Emote::twitch` sets `animated: false` because the IRC tag does not say; PR 2.2 should detect via the `default` CDN format or the emote metadata.
- **Bare-domain false positives for short TLDs** (found in PR 1.1) — `ok.so`, `main.rs`, `readme.md`, `1.2.3.4` are linkified; consider requiring a path/port, a minimum label length, or an allow-list for ccTLDs that collide with file extensions and words.
- **`mark_all_deleted` deep-clones every message** (found in PR 1.1) — CLEARCHAT of a full 1 000-message store clones each `Message` (fragments included); consider a shared body behind `Arc` or a per-store deleted-set instead of replacing the message.
- **Emote name cache keyed by id only** (found in PR 1.1) — `twitch_emote_ranges` caches one `Arc<Emote>` per id within a message, so if the same id appears with different covered text (should not happen on Twitch) the first name wins.
- **Seed the settings watcher with the startup load error** (found in PR 0.4) — the watcher starts with no memory of a failure at startup, so the app passes a sentinel baseline (`schema_version = 0`) to make the first good reload always report; a `watch(..)` option taking the initial error would be cleaner.
- **Bridge relies on window frames** (found in PR 0.4) — `bridge::attach` drains in a next-frame callback of one window, so a hidden/occluded window stalls delivery (senders back up) and closing that window ends the bridge; add a timer fallback and per-app (not per-window) attachment before multi-window support.
- **Apply `line_height` live** (found in PR 0.4) — `appearance.line_height` is loaded and watched but nothing reads it yet; wire it in with the chat view (PR 1.4).
- **`gpui-kit` `test-support` does not resolve** (found in PR 0.4) — enabling it pulls `gpui-fast`, which needs a `proptest` version missing from the index, so tests use `gpui-pre` (same pinned version) with its `test-support` feature as a dev-dependency; revisit on the next Kit bump.
- **Bridge keeps the receiver after the entity is released** (found in PR 0.4) — the attached task only notices a released entity (or closed window) when the next event arrives, so until then senders do not see `Closed`; have the task also wait on the entity's release.
- **Quit can stall on runtime shutdown** (found in PR 0.4) — `Backend::shutdown` calls `shutdown_timeout(500 ms)` on the main thread from the quit observer, so blocking backend work can delay quit by up to that long; move it off the main thread or shorten it.
- **Settings banner may never clear if the watcher failed to start** (found in PR 0.4) — when the file was broken at startup and `watch` could not start, nothing reports a later fix, so the banner stays until restart; retry starting the watcher or re-read the file on window focus.
- **Apply `chat.scrollback` live** (found in PR 1.3) — `ChannelHub` reads the setting once at startup; a changed value should call `MessageStore::set_capacity` on every store.
- **Draw time excludes GPU present** (found in PR 1.3) — the HUD's frame probe times layout/prepaint/paint only; `Window::present` has no public hook. Revisit with GPUI's `profiler` feature (`FrameTiming`) if present time ever matters.
- **A frame per event at low rates** (found in PR 1.3) — the bridge wakes a frame for every event that arrives while idle, so 200 msg/s means ~200 draws/s (about 30 % of a core with the placeholder list); batching only kicks in once the rate exceeds the display's frame rate. PR 1.4 should coalesce ingest-driven redraws (e.g. a minimum frame interval) once the real view is measured.
- **Firehose keeps generating while the window is occluded** (found in PR 1.3) — the source only learns the UI is behind when the bridge queue is full (then it drops and counts); it never pauses for a hidden or occluded window, so a long hidden run just sheds events.
- **`due_count` floors, so message *i* goes out at *(i+1)/rate*** (found in PR 1.3) — the first event is sent one interval after start, not at start; message timestamps use `i / rate`. Harmless for a load generator, but note it if exact phase ever matters.
- **Firehose drops instead of backpressuring** (found in PR 1.3) — it uses `try_send` so a slow UI shows up as `dropped` on the HUD; a mode that waits (`send().await`) would give lossless runs for correctness checks.
- **HUD lists only the first four stores** (found in PR 1.3) — fine for the firehose; per-channel detail belongs to the channel list once PR 1.5 exists.
- **Per-window frame stats** (found in PR 1.3) — `Diagnostics` is one set for the whole app; with several windows the probe would mix their draws.
- **Strip the reply mention from the body** (found in PR 1.2) — a reply's text starts with `@parent `; `parse_line` leaves it in the fragments (as a `Mention`). Chatterino hides it and shows the reply header instead; decide in the chat view (PR 1.4/5.x), the data is in `Message::reply`.
- **Join confirmation timeout** (found in PR 1.2) — a JOIN Twitch silently ignores (suspended or renamed channel) leaves the channel in `Connecting` forever; add a timeout that reports the channel as failed.
- **Stop reconnecting on a rejected login** (found in PR 1.2) — with a bad token Twitch sends `NOTICE * :Login authentication failed` and closes; the client keeps retrying with backoff. PR 3.2 should surface it as `Disconnected { reason }` and stop until new credentials arrive.
- **Multiple connections for many channels** (found in PR 1.2) — one connection carries every channel (JOINs paced at 15 per 10 s); `twitch-irc`'s pool splits at 90 channels per connection. Revisit if someone really joins 100+ channels.
- **Preserve tag order in `RawTags`** (found in PR 1.2) — `twitch-irc` parses tags into a `HashMap`, so `RawTags` from the Twitch backend are sorted by key, not in wire order. Only matters if something wants to re-serialise a line byte for byte.
- **Recent-messages ordering/dedupe** (found in PR 1.2) — `parse_line` flags `historical=1` lines as `history`; PR 5.5 still has to merge them with live messages by id.
- **Offline emits can block commands** (found in PR 1.2) — while no connection is open (connecting, backing off) `Connecting`/`Reconnecting`/part events are sent with an awaited `send`, so a completely full event channel delays join/part/shutdown until the UI drains it. Likewise a dropped receiver is only noticed at the next emit while offline. A live session is not affected (outbox, and it watches for the receiver closing).
