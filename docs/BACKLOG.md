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
