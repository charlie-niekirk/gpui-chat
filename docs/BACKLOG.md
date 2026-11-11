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
- **Newer-schema files are downgraded on save** (found in PR 0.3) — a file with `schema_version` higher than supported is loaded read-tolerantly, but saving writes the current version and drops unknown keys; decide on a refuse-to-save or backup policy.
