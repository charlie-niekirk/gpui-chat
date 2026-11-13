# gpui-chat — build plan

> **For the orchestrator:** this file is the only thing in the folder. There is no git repository and no code yet — begin at **Step 0** in the Orchestration protocol, then work through the phases, ticking the Status list at the bottom as PRs merge.

## Context

`gpui-chat` is a native Twitch chat client in the spirit of Chatterino, written in Rust on GPUI. Goals, in the owner's words: modern UI, performant, macOS first (Linux/Windows later), open source on GitHub, Twitch + BTTV + 7TV emotes (plus other common providers), a per-chat button that launches streamlink, and a monospace default font matching Omarchy, customisable.

The folder `/Users/cniekirk/Projects/gpui-chat` is empty and is not yet a git repository. This plan is executed by an **orchestrator session (Opus)** that starts **one separate Claude session per PR**.

Decisions already made by the owner:

| Decision | Choice |
|---|---|
| Name (binary, repo, bundle ID suffix, config dir) | `gpui-chat` — working title, kept in one constant (`APP_NAME` / `APP_ID`) so a rename is one small PR |
| License | MIT |
| Merging | Orchestrator reviews, waits for green CI, squash-merges. Owner reviews at each phase milestone |
| Window layout | Collapsible channel sidebar + split panes (no top tab bar) |

## Facts checked on 2026-11-11

Implementation sessions must re-check anything marked ⚠ before relying on it.

- **GPUI:** `gpui` on crates.io is still `0.2.2`. The practical route is **GPUI Kit** (`gpui-kit = 0.9.1`, repo `longbridge/gpui-kit`, Apache-2.0): one dependency that re-exports GPUI, `gpui-base`, `gpui-component` (75+ components) and assets. It pins `gpui-pre =0.3.12`, a published snapshot of Zed's GPUI (published 2026-11-09). Entry points: `gpui_kit::application().run(..)`, `gpui_kit::init(cx)`, `gpui_kit::open_window(..)`.
- **Kit components we use:** `sidebar`, `dock`, `resizable`, `title_bar`, `native_menu`, `input`, `popover`, `tooltip`, `hover_card`, `menu`, `dialog`, `sheet`, `notification`, `command` (palette), `setting`, `virtual_list`, `theme`, `avatar`, `badge`, `kbd`, `switch`, `slider`, `select`.
- **GPUI primitives present in the snapshot:** `Element`, `list` / `ListState` / `ListAlignment` / `FollowMode`, `uniform_list`, `img`, `RenderImage` (BGRA), `Hitbox`, `WindowTextSystem`, `AssetSource`, `TestAppContext`. ⚠ Exact signatures for painting images, dropping image textures, requesting animation frames and adding fonts were not visible on the docs index — read the source in `~/.cargo/registry` for the pinned version.
- **Omarchy font:** the Omarchy manual states the default terminal and system font is **`JetBrainsMono Nerd Font`**.
- **Twitch auth:** Device Code Flow works for public clients (no secret): `POST id.twitch.tv/oauth2/device`, then poll `/oauth2/token`. Access tokens last 4 h; refresh tokens are **single use** and expire after 30 days idle.
- **Twitch chat:** IRC is not deprecated (Twitch recommends EventSub for bots). EventSub cannot read chat anonymously; IRC can. Slash commands and sending are done through Helix (`user:write:chat`).
- **Crates:** `twitch-irc 6.1.3` (ws + rustls transports, anonymous login), `twitch_api 0.8.0`, `twitch_oauth2 0.17.1`, `tokio-tungstenite 0.30.0`. Local toolchain: rustc 1.99.1, `gh` logged in. `streamlink` is **not installed** on this Mac.
- **Emote endpoints (live, HTTP 200):** `7tv.io/v3/emote-sets/global` (files `1x–4x` in `.webp`, `.avif`, `.gif`; `data.animated`, `flags`), `api.betterttv.net/3/cached/emotes/global`, `cdn.7tv.app/emote/{id}/2x.webp`, `api.ivr.fi/v2/twitch/badges/global`, `recent-messages.robotty.de/api/v2/recent-messages/{channel}`.
- ⚠ `api.frankerfacez.com/v1/set/global` timed out in the probe — re-test in PR 2.3; BTTV mirrors FFZ data at `api.betterttv.net/3/cached/frankerfacez/...` as a fallback.

## What the owner must supply

| When | What |
|---|---|
| Step 0 | Confirm creating the **public** GitHub repo `gpui-chat` (orchestrator asks once, then runs `gh repo create`) |
| Before PR 1.6 testing | `brew install streamlink` and a player (mpv, IINA or VLC) |
| Before PR 3.1 | Register an app at dev.twitch.tv/console — **Client Type: Public**, redirect `http://localhost` — and give the Client ID to the orchestrator |
| Before PR 7.1 (optional) | Apple Developer ID certificate + notarization credentials as GitHub secrets; without them the release is unsigned with documented Gatekeeper steps |

## Technology

| Area | Choice | Note |
|---|---|---|
| UI | `gpui-kit = "=0.9.1"` (exact pin) | Kit usage stays inside `crates/app`. Chat rendering uses core GPUI primitives only, so kit churn cannot break it. Version bumps are their own PRs |
| Async I/O | `tokio` runtime on background threads | GPUI has its own executor; backends talk to the UI over channels |
| Chat read | `twitch-irc` over TLS (rustls) | Anonymous by default; same parser handles history lines |
| Helix + auth | `twitch_api` (helix feature) + `twitch_oauth2` device flow | Send, badges, emotes, streams, moderation |
| HTTP | `reqwest` (rustls) | Descriptive `User-Agent: gpui-chat/<version>` |
| Images | `image` crate, WebP preferred, GIF/PNG fallback | Avoid AVIF (native decoder dependency) |
| Secrets | `keyring` | macOS Keychain now; Secret Service / Credential Manager later |
| Config | TOML via `serde` + `toml`, `notify` for hot reload | `~/.config/gpui-chat/{settings,keymap}.toml`, `themes/*.toml` |
| Paths | `dirs` | Cache in the platform cache dir |
| Font | Bundle upstream **JetBrains Mono** (OFL-1.1; Regular, Bold, Italic, Bold Italic) | Default family chain: `JetBrainsMono Nerd Font` (if installed) → bundled `JetBrains Mono` → system monospace. Same glyph design as Omarchy either way |

## Architecture

### Workspace

```
crates/
  app/         bin `gpui-chat` — the only crate that depends on GPUI. Views, actions, keymap, theme, platform/
  core/        domain types, message store, tokenizer. No I/O
  twitch/      IRC transport, Helix client, auth (device flow), later EventSub
  emotes/      providers: Twitch, BTTV, FFZ, 7TV; emote sets; resolution order; live updates
  media/       image fetch, disk cache, decode (static + animated), memory budget
  settings/    typed settings, load/save/watch, paths
  streamlink/  locate binary, launch, track child processes
assets/        fonts, icons, bundled themes, bundled global-badge snapshot
fixtures/      recorded raw IRC lines, API JSON samples
docs/          ARCHITECTURE.md, BACKLOG.md
```

Crates other than `app` have no GPUI dependency: they are unit-testable on any OS and let PR sessions work in parallel without touching the same files.

### Data flow

```
twitch-irc / Helix / emote APIs   (tokio threads)
        │  ChatEvent over a bounded channel
        ▼
ChannelStore (one GPUI entity per joined channel)   ← tokenizer + EmoteResolver
        │  drained at most once per frame, one notify per batch
        ▼
ChatView → list() → MessageElement (custom Element)  ← ImageCache
```

- **`core::Message`** is immutable behind `Arc`: id, timestamp, author (login, display name, colour, badges), `fragments`, kind (chat, action, notice, user-notice, system), flags (mention, first message, highlighted, deleted, history), optional reply reference.
- **`core::Fragment`**: `Text`, `Emote { emote, zero_width_overlays }`, `Emoji`, `Mention`, `Link`, `Cheer`.
- **`MessageStore`**: ring buffer per channel, default 1 000 messages (setting), O(1) push and evict.
- **Emote resolution:** Twitch emotes come from IRC tag ranges (⚠ ranges count Unicode code points, not bytes). Remaining words are looked up: channel 7TV → channel BTTV → channel FFZ → global 7TV → global BTTV → global FFZ. Zero-width emotes stack on the preceding emote.

### Rendering (the performance-critical part)

- **`MessageElement`** implements GPUI's `Element` directly. It shapes text runs with the window text system, flows fragments into lines itself, and paints emote and badge images at computed positions. One element per message — not one element per word.
- **Layout cache** per message keyed by (width, font generation, scale factor); invalidated on resize or settings change.
- **Virtualisation** with GPUI `list()` / `ListState` in bottom-follow mode. Scrolling up pauses following and shows a "new messages" pill.
- **Ingest batching:** events are drained once per frame; a burst of 1 000 messages causes one layout pass, not 1 000.
- **Images:** fetched and decoded off the UI thread; de-duplicated in flight; disk cache keyed by URL hash; in-memory LRU with a byte budget (default 256 MB); GPU textures released on eviction.
- **Animation:** one global clock so identical emotes stay in sync. Animation frames are requested only while an animated emote is visible and the window is active. Setting to disable animation.
- **Fallback** if the custom element is blocked by an API gap: wrap-flex of per-word elements to keep the phase moving, with a backlog item to return.

### Performance budgets (measured with the replay harness, PR 1.3; enforced in PR 6.1)

| Metric | Target on Apple Silicon |
|---|---|
| Frame time while scrolling, one pane at 100 msg/s | p99 under 8.3 ms |
| Burst ingest | 1 000 msg/s without input lag |
| Idle CPU, no animated emote visible | under 1 % |
| Memory, 10 channels × 1 000 messages, default image budget | under 300 MB |
| Launch to first window | under 300 ms |

### Cross-platform guardrails (from day one)

Platform-specific code lives only in `crates/app/src/platform/`. Paths, keychain and process spawning go through `dirs`, `keyring` and the `streamlink` crate. CI runs non-GPUI crate tests on Ubuntu from PR 0.1.

## Orchestration protocol

**Step 0 (orchestrator, no PR).** Ask the owner to confirm the public repo. Then: `git init`, commit `PLAN.md`, MIT `LICENSE` and `.gitignore`, `gh repo create gpui-chat --public --source . --push`, enable squash merging and required CI on `main`.

**Loop.**
1. Pick PRs whose dependencies are merged. Run at most **two sessions at once**, three only when they touch disjoint crates (the tables mark what can run in parallel).
2. Start a **fresh Claude session per PR in its own git worktree**, branch `pr/<id>-<slug>`, model as marked: **S** = Sonnet 5.5, **O** = Opus 5.5.
3. When the session reports: read the diff, run a code review, confirm CI is green, check the acceptance criteria yourself (run tests; launch the app for UI PRs). A session report is not proof.
4. Squash-merge, delete the branch, tick the PR in the status list below with a direct docs-only commit to `main`.
5. At the end of each phase: run the milestone check, tag `m<N>`, post a short summary to the owner, and continue. Stop only for the owner-supplied items listed above.

**Rules.**
- Never merge with red CI. If a PR fails acceptance twice, split it or move it up a model.
- Scope creep goes to `docs/BACKLOG.md`, not into the PR.
- Feature PRs do not bump `gpui-kit`.
- GPUI Kit is newer than any model's training data: sessions read docs.rs or the source in `~/.cargo/registry` for the pinned version instead of writing from memory.
- Shared files (`Cargo.toml`, `crates/app/src/main.rs`, settings schema) are the conflict hot spots: later-merging PR rebases.

**Session brief template.**

```
You are implementing PR <id> "<title>" for gpui-chat. Work only in this worktree.
1. Read CLAUDE.md, docs/ARCHITECTURE.md and the PR <id> section of PLAN.md.
2. Implement exactly that scope. Out-of-scope ideas: append to docs/BACKLOG.md.
3. GPUI / GPUI Kit APIs: verify against the pinned version's docs or source.
4. Run: cargo fmt --check, cargo clippy --workspace -- -D warnings, cargo test --workspace,
   plus the PR's acceptance checks. For UI changes, launch the app and attach a screenshot.
5. Open a PR with gh: summary, acceptance checklist with results, anything not verified.
Done = PR open, CI green, acceptance checklist filled in honestly.
```

## Phases and PRs

Legend: **Dep** = must be merged first. **∥** = may run alongside. **S/O** = suggested model.

### Phase 0 — Foundations

| ID | PR | Dep | ∥ | Model |
|---|---|---|---|---|
| 0.1 | Workspace, CI, hello window | Step 0 | — | S |
| 0.2 | App shell, theme tokens, bundled font | 0.1 | 0.3 | S |
| 0.3 | Settings crate | 0.1 | 0.2 | S |
| 0.4 | Runtime bridge and settings wiring | 0.2, 0.3 | — | S |

- **0.1** Cargo workspace with the seven crates (stub lib + one test each); pinned workspace dependencies including the ones later PRs will need; `rust-toolchain.toml`; rustfmt, clippy, `cargo-deny` configs; GitHub Actions (macOS: fmt, clippy, test, build; Ubuntu: test non-GPUI crates); `CLAUDE.md` (conventions, commands, the API-verification rule); `README`, `CONTRIBUTING`, PR template, `docs/ARCHITECTURE.md` seeded from this plan. Hello-world window through `gpui-kit`. *Accept:* `cargo run -p gpui-chat` opens a window; CI green.
- **0.2** Root window with custom title bar, collapsible sidebar placeholder and main area; dark and light theme tokens that follow the system; bundled JetBrains Mono registered at start with the fallback chain; actions, default keymap, native macOS menu; window size and position restored. *Accept:* screenshot in both appearances; text renders in JetBrains Mono.
- **0.3** `Settings` struct (serde, defaults, schema version); load, save, atomic write; tolerant of bad files (keeps defaults, reports the error); file watcher; path helpers. No GPUI. *Accept:* round-trip and bad-input tests.
- **0.4** Tokio runtime owned by the app; `spawn_backend` helper and a channel-to-entity bridge with per-frame draining; settings as a GPUI global with observers; editing `settings.toml` changes font family, size and theme live. *Accept:* live font change demonstrated; bridge unit test.

### Phase 1 — Anonymous read-only chat → **M1**

| ID | PR | Dep | ∥ | Model |
|---|---|---|---|---|
| 1.1 | Core domain model and tokenizer | 0.1 | 0.2–0.4 | S |
| 1.2 | Twitch IRC transport | 1.1 | 1.3 | S |
| 1.3 | Firehose/replay source and frame HUD | 1.1, 0.4 | 1.2 | S |
| 1.4 | Chat view v1 (`MessageElement`, text only) | 1.3 | 1.2 | **O** |
| 1.5 | Channels: sidebar, panes, session restore | 1.2, 1.4 | — | S |
| 1.6 | Streamlink launch button | 1.5 | 2.x | S |

- **1.1** Types from the Architecture section; `MessageStore`; tokenizer for words, links and @mentions; `ChatEvent` enum (message, clear chat, delete message, room state, notice, connection state). *Accept:* unit tests incl. emoji and code-point offsets.
- **1.2** `TwitchClient`: anonymous connect, join/part, reconnect with backoff, join rate limiting (from `twitch-irc`), mapping of PRIVMSG, USERNOTICE, CLEARCHAT, CLEARMSG, ROOMSTATE, NOTICE into `ChatEvent`; raw tags kept for later PRs. Fixture tests from recorded lines; `--example dump -- <channel>` prints live chat. *Accept:* fixtures pass; dump example shows a live channel.
- **1.3** `--firehose <msgs/sec>` synthetic source and `--replay <file>` for recorded IRC logs; debug HUD with frame time, messages/sec, store sizes (toggle by action). *Accept:* app runs offline against the firehose.
- **1.4** `MessageElement` with timestamp, coloured username (readability-adjusted against the background), wrapped text, mention and link styling; layout cache; `list()` virtualisation; follow-bottom, pause on scroll-up, "new messages" pill; scrollback cap. *Accept:* firehose at 200 msg/s holds the frame budget on the HUD; resize re-wraps correctly.
- **1.5** Sidebar channel list (add with ⌘N dialog, remove, reorder), one pane showing the selected channel, pane header (name, connection state, room-mode chips), open channels and selection persisted. *Accept:* join three real channels anonymously, restart, they return.
- **1.6** `streamlink` crate: resolve binary (setting → login-shell `command -v` → `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`; GUI apps do not inherit the shell `PATH`), spawn `streamlink twitch.tv/<channel> <quality>` with configurable player and extra args, track the child, surface stderr errors (offline, no player). Pane-header button with running state (click again to stop), quality menu, and a "streamlink not found — `brew install streamlink`" hint. *Accept:* unit tests for argument building and path resolution; manual launch on a live channel.

**M1 check:** fresh launch, no login → add channels → live chat scrolls smoothly → play button opens the stream.

### Phase 2 — Emotes and badges → **M2**

| ID | PR | Dep | ∥ | Model |
|---|---|---|---|---|
| 2.1 | Media pipeline | 0.1 | Phase 1 | **O** |
| 2.2 | Inline images: Twitch emotes, emoji, tooltip | 1.4, 2.1 | 2.3 | **O** |
| 2.3 | BTTV, FFZ, 7TV provider clients | 1.1 | 2.1, 2.2 | S |
| 2.4 | Third-party emote integration, zero-width | 2.2, 2.3, 1.5 | 2.5 | S |
| 2.5 | Twitch badges | 2.2, 1.5 | 2.4 | S |

- **2.1** `media` crate: fetch with in-flight de-duplication, disk cache, off-thread decode of PNG, GIF and animated WebP into frames with delays, LRU with byte budget, size-aware scale selection (1x–4x from display scale × emote-scale setting). *Accept:* tests with fixture images; benchmark of decode cost.
- **2.2** App-side `ImageCache` adapter (frames → `RenderImage`, texture release on eviction), global animation clock, `MessageElement` paints `Emote` and `Emoji` fragments, Twitch emotes from IRC tags, hover tooltip (large preview, name, provider), placeholder box while loading so lines do not jump. *Accept:* animated and static Twitch emotes render in a live channel; idle CPU budget holds when none are visible.
- **2.3** `emotes` crate: typed clients and `EmoteSet` for BTTV (global, channel + shared), FFZ (global, room) and 7TV (global, channel by Twitch user ID), with fixtures and error handling per provider; one provider failing never blocks the others. *Accept:* fixture tests; example binary lists a channel's emotes.
- **2.4** Load channel sets when ROOMSTATE supplies the room ID; `EmoteResolver` with the precedence above; zero-width stacking; `/reload-emotes` action; per-provider enable switches in settings. *Accept:* a channel using 7TV zero-width emotes renders them stacked.
- **2.5** `BadgeSource` trait; anonymous implementation (bundled global snapshot + `api.ivr.fi` for channel badges, degrade silently); badges painted before the username with tooltip. Helix implementation arrives in 3.1. *Accept:* subscriber and moderator badges visible while logged out.

**M2 check:** popular channel shows Twitch, BTTV, FFZ and 7TV emotes (animated included) and badges; scroll remains smooth.

### Phase 3 — Account and sending → **M3**

| ID | PR | Dep | ∥ | Model |
|---|---|---|---|---|
| 3.1 | Login (device flow), Keychain, Helix client | M2, Client ID | — | **O** |
| 3.2 | Authenticated IRC, input box, sending | 3.1 | 3.5 | S |
| 3.3 | Autocomplete and emote picker | 3.2 | 3.4, 3.5 | S |
| 3.4 | Replies and threads | 3.2 | 3.3, 3.5 | S |
| 3.5 | Stream info and followed-live sidebar | 3.1 | 3.2–3.4 | S |

- **3.1** Device-code login sheet (code, copy button, open browser), tokens in Keychain, a **single** refresh owner (refresh tokens are one-use) with atomic persistence, validation at start and hourly, logout, scope list in one constant (re-login prompt when it grows). Client ID constant overridable by `GPUI_CHAT_TWITCH_CLIENT_ID`. Helix client plumbing; Helix badge source replaces the anonymous one when logged in. *Accept:* login, restart, still logged in; forced refresh test against a mock server.
- **3.2** IRC reconnects with the user token; per-pane input (multi-line paste safe, history with ↑); send through Helix Send Chat Message with drop-reason errors shown inline; own-message echo; `/me` and basic slash-command parsing. *Accept:* message sent and visible in a test channel.
- **3.3** Tab/`:` emote completion, `@` user completion from recent chatters, command completion; emote picker popover with provider tabs, search and recents; the user's own Twitch emotes via Helix. *Accept:* keyboard-only emote insertion works.
- **3.4** Reply context line on messages, reply action, thread popover. *Accept:* reply round-trip in a test channel.
- **3.5** Batched Helix polling of stream state: live dot, title, category, viewers, uptime in the pane header and sidebar; "Followed — live" sidebar section; click to open. *Accept:* header updates when a channel goes live.

### Phase 4 — Workspace UX → **M4**

| ID | PR | Dep | ∥ | Model |
|---|---|---|---|---|
| 4.1 | Split panes | M3 | 4.2–4.4 | **O** |
| 4.2 | Settings window | M3 | 4.1, 4.3, 4.4 | S |
| 4.3 | Command palette, quick switcher, `keymap.toml` | M3 | 4.1, 4.2, 4.4 | S |
| 4.4 | Theme pack and custom themes | M3 | 4.1–4.3 | S |

- **4.1** Split right/down, close, drag to rearrange, resize, keyboard focus movement, drag a sidebar channel into a split; layout persisted. Each pane keeps its own streamlink button.
- **4.2** Settings UI over the same `settings.toml` (file stays the source of truth): appearance (font family, size, line height, emote scale, timestamps, animation), chat, emote providers, streamlink, account.
- **4.3** ⌘K palette of all actions, ⌘T fuzzy channel switcher, user-editable keymap with hot reload.
- **4.4** Bundled themes matching the stock Omarchy set (Tokyo Night, Catppuccin, Gruvbox, Nord, Everforest, Kanagawa, Rosé Pine and others), user themes in `themes/*.toml`, follow-system option.

### Phase 5 — Chat power features → **M5**

All depend on M4; any two may run in parallel except 5.6 with 5.1/5.2 (all three edit `MessageElement`).

| ID | PR | Model |
|---|---|---|
| 5.1 | Event messages: subs, gifts, raids, announcements, bits/cheermotes, channel-point highlights, first-time chatter, shared chat | S |
| 5.2 | Moderation: deleted/timed-out rendering, mod buttons, slash commands via Helix, room-mode toggles | S |
| 5.3 | Highlights and notifications: mentions, custom phrases/regex, users; unread and mention dots; macOS notifications; Mentions view | S |
| 5.4 | User card: avatar, account age, this-session messages, mod actions | S |
| 5.5 | Message history on join and after reconnect (recent-messages service; opt-out with a privacy note) | S |
| 5.6 | Text selection across messages, copy, context menus, link handling | **O** |
| 5.7 | In-channel search (⌘F), filters, ignored users/phrases, Twitch block list | S |

### Phase 6 — Polish and performance → **M6**

| ID | PR | Dep | Model |
|---|---|---|---|
| 6.1 | Performance pass against the budgets; fix what misses | M5 | **O** |
| 6.2 | Live emote updates (7TV EventAPI, BTTV socket), third-party badges, 7TV name paints | M5 | S |
| 6.3 | First-run onboarding, empty and error states, accessibility pass | M5 | S |

### Phase 7 — macOS release → **v0.1.0**

| ID | PR | Dep | Model |
|---|---|---|---|
| 7.1 | `.app` bundle, icon, universal binary, DMG, tag-driven release workflow, signing + notarization when secrets exist | M6 | S |
| 7.2 | Update check against GitHub Releases, Homebrew cask, README with screenshots, user guide | 7.1 | S |

### Phase 8 — Other platforms

| ID | PR | Dep | Model |
|---|---|---|---|
| 8.1 | Linux (Wayland/X11): build, Secret Service, AppImage + AUR package | 7.2 | S |
| 8.2 | Windows: build, Credential Manager, installer + winget | 7.2 | S |

## Suggested features

Scheduled above beyond the stated requirements: FrankerFaceZ emotes, anonymous mode, emote picker and autocomplete, replies, stream info, followed-live list, command palette, quick switcher, Omarchy theme pack, highlights and notifications, user cards, moderation tools, message history, search and filters, 7TV paints, live emote updates.

Left in `docs/BACKLOG.md` for the owner to pull in: whispers, polls and predictions, AutoMod queue (EventSub), multiple accounts, opt-in chat logging, clip creation, embedded video pane, YouTube/Kick chat sources (the `ChatEvent` boundary allows it), plugin scripting, spell check, localisation.

## Verification

- **Every PR:** `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, plus its *Accept* line. UI PRs attach a screenshot.
- **Offline UI checks:** `cargo run -p gpui-chat -- --firehose 200` (PR 1.3 onward) exercises rendering without a network or login; the HUD shows frame time against the budgets.
- **Backend checks:** fixture-driven tests for IRC mapping, provider JSON and auth (mock server); example binaries for live smoke tests.
- **Milestones:** the M-check scripts under each phase, run by the orchestrator on the merged `main`, then tagged.
- **Release:** download the DMG from the draft release on a clean user account, launch, log in, join, play via streamlink.

## Status

- [x] Step 0
- [x] Phase 0: ~~0.1~~ ✅ · ~~0.2~~ ✅ · ~~0.3~~ ✅ · ~~0.4~~ ✅
- [ ] Phase 1: ~~1.1~~ ✅ · ~~1.2~~ ✅ · ~~1.3~~ ✅ · ~~1.4~~ ✅ · ~~1.5~~ ✅ · 1.6 → M1
- [ ] Phase 2: ~~2.1~~ ✅ · ~~2.2~~ ✅ · ~~2.3~~ ✅ · 2.4 · 2.5 → M2
- [ ] Phase 3: 3.1 · 3.2 · 3.3 · 3.4 · 3.5 → M3
- [ ] Phase 4: 4.1 · 4.2 · 4.3 · 4.4 → M4
- [ ] Phase 5: 5.1 · 5.2 · 5.3 · 5.4 · 5.5 · 5.6 · 5.7 → M5
- [ ] Phase 6: 6.1 · 6.2 · 6.3 → M6
- [ ] Phase 7: 7.1 · 7.2 → v0.1.0
- [ ] Phase 8: 8.1 · 8.2
