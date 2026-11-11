# Architecture

Seeded from the Architecture section of [`PLAN.md`](../PLAN.md). This document is
the living version: update it in the PR that changes the design. `PLAN.md` stays
the roadmap.

`gpui-chat` is a native Twitch chat client (in the spirit of Chatterino) written
in Rust on GPUI, consumed through GPUI Kit (`gpui-kit`, exact-pinned).

## Workspace

```
crates/
  app/         bin `gpui-chat` — the only crate that depends on GPUI. Views, actions, keymap, theme, platform/
  core/        domain types, message store, tokenizer. No I/O
  twitch/      IRC transport, Helix client, auth (device flow), later EventSub
  emotes/      providers: Twitch, BTTV, FFZ, 7TV; emote sets; resolution order; live updates
  media/       image fetch, disk cache, decode (static + animated), memory budget
  settings/    typed settings, load/save/watch, paths
  streamlink/  locate binary, launch, track child processes
assets/        fonts, icons, bundled themes, bundled global-badge snapshot   (added by later PRs)
fixtures/      recorded raw IRC lines, API JSON samples                      (added by later PRs)
docs/          ARCHITECTURE.md, BACKLOG.md
```

Package names are `gpui-chat-<dir>` for libraries (`gpui-chat-core`, …); the
binary package is `gpui-chat`. Crates other than `app` have no GPUI dependency:
they are unit-testable on any OS and let PR sessions work in parallel without
touching the same files.

Dependency direction: `app` → everything; `twitch`, `emotes`, `media`,
`settings`, `streamlink` → `core` at most; `core` depends on nothing but `std`
(plus pure-data crates such as `serde`). No cycles.

Third-party versions are pinned once in the root `Cargo.toml`
(`[workspace.dependencies]`); a crate opts in with `dep = { workspace = true }`
only when it actually uses the dependency.

### Application identity

`APP_NAME` and `APP_ID` live in `crates/core/src/lib.rs`. Everything that shows
the name or derives an identifier (window title, config dir, keychain service,
bundle ID, `User-Agent`) reads them from there, so a rename is one small PR.

## Data flow

```
twitch-irc / Helix / emote APIs   (tokio threads)
        │  ChatEvent over a bounded channel
        ▼
ChannelStore (one GPUI entity per joined channel)   ← tokenizer + EmoteResolver
        │  drained at most once per frame, one notify per batch
        ▼
ChatView → list() → MessageElement (custom Element)  ← ImageCache
```

- **`core::Message`** is immutable behind `Arc`: id, timestamp, author (login,
  display name, colour, badges), `fragments`, kind (chat, action, notice,
  user-notice, system), flags (mention, first message, highlighted, deleted,
  history), optional reply reference.
- **`core::Fragment`**: `Text`, `Emote { emote, zero_width_overlays }`, `Emoji`,
  `Mention`, `Link`, `Cheer`.
- **`MessageStore`**: ring buffer per channel, default 1 000 messages (setting),
  O(1) push and evict.
- **Emote resolution:** Twitch emotes come from IRC tag ranges (ranges count
  Unicode code points, not bytes). Remaining words are looked up: channel 7TV →
  channel BTTV → channel FFZ → global 7TV → global BTTV → global FFZ. Zero-width
  emotes stack on the preceding emote.
- Async I/O runs on a `tokio` runtime on background threads. GPUI has its own
  executor; backends talk to the UI over channels.

## Rendering (the performance-critical part)

- **`MessageElement`** implements GPUI's `Element` directly. It shapes text runs
  with the window text system, flows fragments into lines itself, and paints
  emote and badge images at computed positions. One element per message — not
  one element per word.
- **Layout cache** per message keyed by (width, font generation, scale factor);
  invalidated on resize or settings change.
- **Virtualisation** with GPUI `list()` / `ListState` in bottom-follow mode.
  Scrolling up pauses following and shows a "new messages" pill.
- **Ingest batching:** events are drained once per frame; a burst of 1 000
  messages causes one layout pass, not 1 000.
- **Images:** fetched and decoded off the UI thread; de-duplicated in flight;
  disk cache keyed by URL hash; in-memory LRU with a byte budget (default
  256 MB); GPU textures released on eviction.
- **Animation:** one global clock so identical emotes stay in sync. Animation
  frames are requested only while an animated emote is visible and the window is
  active. Setting to disable animation.
- **Fallback** if the custom element is blocked by an API gap: wrap-flex of
  per-word elements to keep the phase moving, with a backlog item to return.

Chat rendering uses core GPUI primitives only, so GPUI Kit churn cannot break
it; Kit components (sidebar, dock, menus, dialogs, …) stay in `crates/app`.

## Performance budgets

Measured with the replay harness (PR 1.3); enforced in PR 6.1.

| Metric | Target on Apple Silicon |
|---|---|
| Frame time while scrolling, one pane at 100 msg/s | p99 under 8.3 ms |
| Burst ingest | 1 000 msg/s without input lag |
| Idle CPU, no animated emote visible | under 1 % |
| Memory, 10 channels × 1 000 messages, default image budget | under 300 MB |
| Launch to first window | under 300 ms |

## Cross-platform guardrails

Platform-specific code lives only in `crates/app/src/platform/`. Paths, keychain
and process spawning go through `dirs`, `keyring` and the `streamlink` crate. CI
runs non-GPUI crate tests on Ubuntu from PR 0.1 (job `linux`).

## Technology choices

| Area | Choice | Note |
|---|---|---|
| UI | `gpui-kit = "=0.9.1"` (exact pin) | Kit usage stays inside `crates/app`. Version bumps are their own PRs |
| Async I/O | `tokio` | Background threads; channels to the UI |
| Chat read | `twitch-irc` over TLS (rustls) | Anonymous by default; same parser handles history lines |
| Helix + auth | `twitch_api` (helix) + `twitch_oauth2` device flow | Send, badges, emotes, streams, moderation |
| HTTP | `reqwest` (rustls) | `User-Agent: gpui-chat/<version>` |
| Images | `image` crate, WebP preferred, GIF/PNG fallback | Avoid AVIF (native decoder dependency) |
| Secrets | `keyring` | macOS Keychain now; Secret Service / Credential Manager later |
| Config | TOML via `serde` + `toml`, `notify` for hot reload | `~/.config/gpui-chat/{settings,keymap}.toml`, `themes/*.toml` |
| Paths | `dirs` | Cache in the platform cache dir |
| Font | Bundled upstream JetBrains Mono (OFL-1.1) | Default chain: `JetBrainsMono Nerd Font` → bundled `JetBrains Mono` → system monospace |
