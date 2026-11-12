# CLAUDE.md

Guidance for Claude Code sessions (and humans) working on `gpui-chat`, a native
Twitch chat client in Rust on GPUI. Read this, `docs/ARCHITECTURE.md` and the
section of `PLAN.md` for your PR before changing anything.

## Commands

```bash
cargo run -p gpui-chat                                # launch the app
cargo run -p gpui-chat --release -- --firehose 1000 --hud  # offline synthetic chat + debug HUD (⌘⇧D)
cargo run -p gpui-chat --release -- --replay fixtures/irc/privmsg.irc --hud   # replay a recorded IRC log
cargo fmt --all --check                               # formatting
cargo clippy --workspace --all-targets -- -D warnings # lints (must be warning-free)
cargo test --workspace                                # all tests
cargo test --workspace --exclude gpui-chat            # what the Linux CI job runs (no GPUI)
cargo deny check                                      # licences / advisories / sources
```

Before opening a PR: fmt, clippy, test must pass, plus the PR's *Accept* line in
`PLAN.md`. UI changes: launch the app and attach a screenshot
(`screencapture -x -R<x>,<y>,<w>,<h> out.png`).

CI (`.github/workflows/ci.yml`) has stable job names `macos` (fmt, clippy, test,
build), `linux` (tests for every crate except the app) and `deny`. Never merge
or ask for merge with red CI.

## Crate boundaries

| Crate (dir) | Package | Role |
|---|---|---|
| `crates/app` | `gpui-chat` | The binary. **Only** crate allowed to depend on GPUI / `gpui-kit`. Platform-specific code only under `crates/app/src/platform/` |
| `crates/core` | `gpui-chat-core` | Domain types, message store, tokenizer, `APP_NAME` / `APP_ID`. No I/O |
| `crates/twitch` | `gpui-chat-twitch` | IRC transport, Helix, auth |
| `crates/emotes` | `gpui-chat-emotes` | Emote providers and resolution |
| `crates/media` | `gpui-chat-media` | Image fetch, cache, decode |
| `crates/settings` | `gpui-chat-settings` | Typed settings, load/save/watch |
| `crates/streamlink` | `gpui-chat-streamlink` | Locate/launch/track streamlink |

Rules:
- Non-app crates must build and test on Linux without GPUI. Never add a GPUI dependency to them.
- Third-party deps are pinned once in the root `[workspace.dependencies]`; member crates use
  `dep = { workspace = true }` and only add what the PR actually uses.
- The app name and bundle/config identifier come from `APP_NAME` / `APP_ID` in `gpui-chat-core`.
  Do not hardcode `"gpui-chat"` strings elsewhere.

## Conventions

- Rust 2024 edition, toolchain pinned in `rust-toolchain.toml`. `rustfmt.toml` and workspace
  lints (root `Cargo.toml`) are authoritative; `unsafe_code` is forbidden.
- Errors: `thiserror` for library crates, `anyhow` only in the app's `main`/glue.
- Logging via `tracing`; no `println!`/`dbg!` in committed code (except example/CLI output).
- Tests live next to the code (`#[cfg(test)] mod tests`); fixtures in `fixtures/`.
- Keep PRs to the scope in `PLAN.md`. Branch names `pr/<id>-<slug>`; squash-merge.
- `Cargo.toml`, `crates/app/src/main.rs` and the settings schema are conflict hot spots: keep
  edits there minimal and rebase on `main` before opening the PR.

## GPUI / GPUI Kit APIs: verify, never recall

GPUI Kit (`gpui-kit`, pinned `=0.9.1`) wraps a `gpui-pre` snapshot that is **newer
than any model's training data**. Never write GPUI / Kit API calls from memory.
Before using an API, read its real definition in the pinned version:

- Source: `~/.cargo/registry/src/*/gpui-kit-0.9.1/`, `gpui-pre-0.3.12/`, `gpui-component-0.9.1/`,
  `gpui-base-0.9.1/` (run `cargo fetch` first if missing), or
- Docs: https://docs.rs/gpui-kit/0.9.1

Say in the PR which signatures you checked. Feature PRs do not bump `gpui-kit`;
a bump is its own PR.

## BACKLOG rule

Anything out of scope — an idea, a bug you noticed, a refactor, a nice-to-have —
goes to `docs/BACKLOG.md` as a one- or two-line entry. It does **not** go into the
current PR. If something blocks your PR, say so in the PR description instead of
widening scope.

## Writing PRs

Use `.github/pull_request_template.md`: summary, acceptance checklist with real
results, anything not verified. Be honest about what you did not run. Do not
merge; the orchestrator reviews and squash-merges after green CI.
