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

### App shell (`crates/app`)

- `shell.rs` — root view: Kit `TitleBar`, collapsible Kit `Sidebar` (the channel list), main area
  (one `ChannelPane`, PR 1.5; splits come in 4.1). Owns the `ChannelList` and the live source; every
  list edit ends in `list_changed` (make sure the selected channel has a pane, schedule the
  `state.toml` write). Handles the window actions (`ToggleSidebar`, `CloseWindow`, `MinimizeWindow`,
  `About`, and the channel actions below); `Quit` is global.
- `theme.rs` — our own `Tokens` (dark/light), chosen from the system appearance (live, via
  `observe_window_appearance`) and mirrored into Kit's `Theme` so Kit components match.
  Theme packs (PR 4.4) extend `Tokens`.
- `fonts.rs` / `assets.rs` — bundled JetBrains Mono TTFs (`assets/fonts/`, OFL in `OFL.txt`) are
  embedded, registered with the text system at startup and the family is resolved against the
  installed list (GPUI panics on unknown families, so there is no blind fallback).
- `actions.rs` + `platform/` — action definitions; key bindings and the native menu bar are
  per-platform and live only under `platform/`. The Kit `native_menu` is a popup context menu;
  the application menu bar uses GPUI `cx.set_menus`.
- `channel_list.rs` / `login.rs` / `add_channel.rs` (PR 1.5) — `ChannelList` is the plain-data list of
  open channels plus the selection (add appends and selects, remove selects the next/previous, move
  swaps with the neighbour, restore drops invalid or duplicate logins); `login::normalise_login`
  turns `Forsen`, `#forsen`, `@forsen` or any `twitch.tv/…` URL form (popout, moderator, embed,
  query strings) into a channel, or says why not (3–25 chars of `a-z0-9_`); `add_channel` is the ⌘N
  dialog (Kit `Dialog` + `Input`, the rejection reason under the field). Reorder is by actions
  (⌘⌥↑/↓ and the row's context menu): Kit's `SidebarMenuItem` has no drag hooks.
- `live.rs` (PR 1.5) — `LiveSource`: one anonymous `TwitchClient` started inside the backend runtime
  (`runtime::enter`), its events forwarded with `send().await` into the bridge. The channel list is the
  only thing that decides what is joined: the shell calls `join`/`part` on add/remove and for every
  restored channel at launch (the client paces the JOINs). **`--firehose` / `--replay` replace it**: no
  client, no joins, and the channel list is neither read from nor written to `state.toml`; the sidebar
  just lists channels as the offline source creates them.
- `pane.rs` (PR 1.5) — `ChannelPane`: pane header (channel name, connection state, room-mode chips, the
  streamlink play/stop split button with its quality menu and, when streamlink failed, the error with a
  dismiss button; PR 1.6) over the `ChatView`. `link_status` combines the state of
  the whole connection (`Connection { channel: None }`, kept by the hub) with the channel's own join
  state; `room_chips` formats `RoomState` (emote-only, followers-only with duration, slow N s,
  subs-only, unique). Panes are created on first selection and kept (scroll position survives
  switching) until the channel is closed.
- `channels.rs` — `ChannelStore` (GPUI entity: core `MessageStore` + room/connection state, one per
  channel) and `ChannelHub` (owns the stores; its `apply_batch` is the bridge's `apply`: it splits a
  frame's batch by channel, applies each run in order and notifies each touched store once; it also
  keeps the whole-connection state). With the live client the hub does **not** create stores from
  events (`set_auto_create(false)`): the shell creates one per open channel (`ensure`) and drops it on
  close (`remove`), so the client's `Disconnected` event after a part cannot resurrect a closed channel.
- `firehose.rs` / `cli.rs` — `--firehose <msgs/sec>`: a seeded synthetic `ChatEvent` source on the
  tokio runtime (messages are due at `i / rate`; each wake-up sends everything due in one batch;
  `try_send`, so a UI that falls behind makes it drop and count rather than stall). `cli.rs` is a
  std-only flag parser.
- `replay.rs` — `--replay <file> [--replay-speed <x>]`: feeds a recorded raw IRC log through
  `gpui_chat_twitch::parse_line`. At speed 1 events follow `tmi-sent-ts` (gaps capped at 5 s,
  backwards stamps never rewind, untimestamped events go with the previous one; a log with no
  stamps runs at 100 events/s); speed 0 is as fast as the UI takes events. Lossless (`send().await`),
  unlike the firehose; rejected lines are counted as parse errors on the HUD.
- `diagnostics.rs` / `hud.rs` — debug HUD (⌘⇧D, View menu). `FrameProbe` wraps the window content
  and times the draw (layout request → paint end) with `Instant`s; ingest is timestamped in the
  bridge `apply`. The HUD re-renders only when something happened (ingest, drops, foreign draws)
  plus one settle refresh, from a 4 Hz timer that exists only while it is visible, so an idle window
  never redraws. `present` (GPU hand-off) is not included: GPUI has no hook for it outside its
  `profiler` feature.
- `chat/` — the chat view (PR 1.4), see *Rendering* below: `display` (a message as one line of
  text with span roles), `flow` (line wrapping and hit-testing, pure), `layout` (shaping into
  grapheme atoms, per-message layout cache), `colors` (fallback and readability-adjusted username
  colours), `element` (`MessageElement`), `list_sync` (incremental list splices), `view`
  (`ChatView`: list, follow-bottom, "new messages" pill, live settings).
- `streams.rs` (PR 1.6) — `StreamHub`, a GPUI entity created by the shell: owns the `Streams` tracker from
  `crates/streamlink`, turns its events (from worker threads, through a bridge channel) into a per-channel
  `StreamStates` (`Starting` → `Running` → `Stopping`, or `Ended` with the error until dismissed; events
  of an old session are ignored) and exposes `toggle` / `play(quality)` / `stop` / `dismiss`. The shell
  calls `forget` when a channel is closed; an `on_app_quit` hook calls `stop_all`. Settings are read at
  play time (`launch_spec`), so edits apply on the next press. The button is hidden for the offline sources.
- `state.rs` — `state.toml` in `<config dir>/<APP_NAME>/` with the last window placement
  (validated against connected displays on restore) and `[channels]` (`open` in sidebar order,
  `selected`). Separate from user settings. Sections load independently (a malformed `[channels]`
  leaves `[window]` intact and vice versa) and each writer replaces only its own top-level key in the
  file's TOML table under one lock, so other sections (even ones this build cannot read) survive; a
  file that is not TOML at all is moved to `state.toml.bad` before a new one is started. Restore
  normalises every saved login (`Forsen`, `#forsen` still restore) and drops invalid or duplicate ones
  and a stale selection.

### Domain model (`crates/core`)

Dependency-free (std only), no I/O, no clock. Modules: `ids` (`ChannelId` = lowercase login,
`MessageId`, `EmoteId`), `emote` (`Emote`, `EmoteProvider`, `ImageUrls` by scale, `BadgeId` /
`Badge`), `message` (`Message`, `Author`, `Fragment`, `MessageKind`, `MessageFlags`, `ReplyRef`),
`store` (`MessageStore`), `event` (`ChatEvent`, `RoomState`), `tokenizer`.

- **`MessageStore`** hands out a monotonically increasing *sequence number* per pushed message
  (never reused; `first_seq`/`next_seq`/`iter_since` let the UI see what is new or scrolled out) and a
  *generation* that bumps on every change, including deletes and resizes. Deleting replaces the stored
  `Arc<Message>` with a copy flagged `deleted`; the old `Arc` is untouched.
- **Tokenizer** (`Tokenizer::tokenize(text, twitch_ranges)`): Twitch emote ranges first (code-point
  indices, invalid or overlapping ranges dropped), then per whitespace-delimited word: `WordResolver`
  (third-party emotes, whole word) → cheermote → emoji split → mention / link on the pieces. Zero-width
  emotes stack onto a directly preceding emote. Plain words and whitespace are merged into `Text`
  runs, so fragments concatenate back to the message text.
- Emoji detection uses small built-in Unicode range tables (no data crate); the TLD allow-list for
  bare domains is likewise built in.

### Media pipeline (`crates/media`)

GPUI-free; takes a tokio `Handle` and the cache directory from the caller. `ImageCache<V>`
looks up memory LRU → in-flight load (joined, so N requests for a URL make one fetch) →
remembered failure → new load task: disk cache → network (`reqwest`/rustls, `User-Agent:
<APP_NAME>/<version>`, bounded concurrency, per-attempt timeout, exponential retry on
timeouts/network errors/408/429/5xx) → decode on the blocking pool (bounded) → disk write →
memory insert. 404/410 and undecodable images are remembered for 10 min, other failures 30 s.

- **Pixels:** every frame is **BGRA8, straight (non-premultiplied) alpha**, tightly packed,
  full canvas size — what `gpui-pre 0.3.12`'s `RenderImage` stores (its own decoders only swap
  R/B on `image`'s straight RGBA). `DecodedFrame::into_pixels` hands the buffer over without a
  copy for `image::RgbaImage::from_raw` → `image::Frame::from_parts` → `RenderImage::new`.
- **Decode:** PNG/APNG, GIF, WebP (still + animated, composited on a transparent canvas like
  browsers). Delays of 10 ms or less become 100 ms. `DecodeLimits` (default 2048 px per side,
  1024 frames, 64 MB decoded) reject oversized canvases and truncate long animations.
  Decode cost is benchmarked in `crates/media/benches/decode.rs`.
- **Memory:** `ByteLru` with a byte budget (default 256 MB). `V: CacheValue` lets the app store
  its own value (e.g. `Arc<RenderImage>` plus a `Timeline`) so pixels are held once.
  `on_evict` fires for every entry leaving memory (to release GPU textures), `on_ready` when a
  load finishes (to repaint); both run off the UI thread with no locks held.
- **Render path:** `get_if_ready(url)` (memory only, never blocks) or `get_or_request(url)`;
  `Timeline::frame_at(elapsed)` maps one global animation clock to a frame index and the time
  to the next frame change.
- **Disk:** `<dir>/v1/<aa>/<sha256(url)[..16] hex>`, each entry with a header holding the full
  URL and body length; written to a temp file and renamed; size cap (default 256 MB) with LRU
  eviction by mtime; truncated or corrupt entries are deleted and refetched.
- **Scale:** `scale::select(urls, display_scale, emote_scale)` takes the smallest variant with
  at least `display_scale × emote_scale` density, else the largest below;
  `logical_size` gives the draw size independent of the variant.

## Data flow

```
twitch-irc / Helix / emote APIs   (tokio threads)
        │  ChatEvent over a bounded channel
        ▼
ChannelHub → ChannelStore (one GPUI entity per joined channel)   ← tokenizer + EmoteResolver
        │  drained at most once per frame, one notify per touched store per batch
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
- **Emote providers (`emotes` crate):** `EmoteLoader::load_global()` / `load_channel(twitch_user_id)`
  query 7TV, BTTV and FFZ concurrently and return one `Result<EmoteSet, EmoteError>` per
  provider (`ProviderResults`, iterated in precedence order 7TV → BTTV → FFZ); a failing provider
  never blocks the others and a channel with no account (404) is an empty set. FFZ uses the
  official API with the BTTV mirror as fallback. All HTTP goes through the `HttpClient` trait so
  tests run offline against `fixtures/emotes/`.
- Async I/O runs on a `tokio` runtime on background threads. GPUI has its own
  executor; backends talk to the UI over channels.

## Rendering (the performance-critical part)

- **`MessageElement`** (`chat/element.rs`) implements GPUI's `Element` directly, one element
  per message — not one per word. Its display text (`[HH:MM] name: body`, spans tagged with a
  role: timestamp, name, text, mention, self-mention, link, emote, …) is shaped **once as a
  single unwrapped line** with `WindowTextSystem::layout_line` (bold runs for names/mentions),
  then cut into **atoms** at extended grapheme-cluster boundaries with their advance widths.
  `flow` places atoms into lines itself: break before a word that does not fit, trailing spaces
  hang, a word wider than the line is hard-broken between clusters (an emoji ZWJ sequence or a
  letter with combining marks never splits). Paint draws each glyph at its atom's flowed
  position (`paint_glyph` / `paint_emoji`), plus link underlines, the self-mention/highlight
  background and a strikethrough for deleted messages. Colours are resolved at paint time from
  the theme, so a theme switch re-lays out nothing.
  *Seams:* PR 2.2 adds `AtomKind::Inline` atoms (emote images with a known width) to the same
  stream — flow already treats them as unbreakable boxes and the painter skips their glyphs;
  PR 5.6 hit-tests with `Flow::index_at` / `position_of` (point ↔ byte offset in the display
  text).
- **Usernames** use the author's colour, or a Twitch default colour picked by a hash of the
  login, then **readability-adjusted** against the current background: hue and saturation are
  kept and HSL lightness moves away from the background just enough to reach a WCAG contrast of
  4.5:1 (unchanged when it already passes), in light and dark themes alike.
- **Layout cache** per message (by store sequence number) keyed by (width, text-config
  generation, scale factor). Two levels: the shaped atoms survive a width change, so a resize
  only re-flows (no shaping); a font family/size, line-height or timestamp change bumps the
  generation and re-shapes on next use; a replaced message (`Arc` differs, e.g. deleted)
  re-shapes. Evicted messages' layouts are dropped on the next sync.
- **Virtualisation** with GPUI `list()` / `ListState` (`ListAlignment::Bottom`,
  `FollowMode::Tail`). The view keeps a `ListMirror` of the store's sequence range and turns
  each store notification into at most two `splice`s (evicted at the front, appended at the
  back) — never a `reset` per batch. Scrolling up stops following (GPUI does it on an upward
  wheel delta) and shows a "N new messages" pill that jumps back; scrolling to the bottom
  resumes following. GPUI re-engages following during list layout, so a zero-size probe after
  the list requests one extra frame when the pill is stale. Settings changes call
  `ListState::remeasure`; width changes are re-measured by the list itself.
- **Ingest batching:** events are drained once per frame; a burst of 1 000
  messages causes one layout pass, not 1 000. Chat ingest is additionally applied at most
  once per 16 ms (`BridgeReceiver::with_min_interval`): under steady traffic (200 msg/s) that is
  ~60 ingest-driven draws per second, each taking everything that arrived meanwhile, instead of
  a draw per message; an event after a quiet spell is applied on the very next frame. The added
  latency is at most 16 ms plus one frame — far below the network latency of chat itself and
  below what reads as a delay — while scrolling still draws at the display's full rate.
- **Images (`crates/app/src/images.rs`):** fetched and decoded off the UI thread by
  `gpui-chat-media` (de-duplicated in flight, disk cache keyed by URL hash, LRU with a byte budget,
  default 256 MB). The cached value is an `EmoteImage`: the decoded BGRA straight-alpha frames
  moved (no copy) into one GPUI `RenderImage` on the blocking pool, plus the frame `Timeline`;
  its atlas textures are uploaded lazily by the first `paint_image` of each frame. Each chat view
  keeps the images it painted last frame (`FrameImages`) and paints from there first, so an image
  the budget evicts while on screen keeps drawing instead of thrashing. Evicted images wait until
  no view holds them, then `App::drop_image` releases their textures. Finished loads wake every
  view that painted a placeholder, once per batch.
- **Inline emotes:** shaped as one `Inline` atom per emote with its final logical size (the
  provider's base size, or 28 px for Twitch, × `emotes.scale`; the image's own size once known),
  so a placeholder box holds the space while loading and lines never jump. Each flowed line is as
  tall as its tallest emote; text and emotes are centred in it. The variant is picked by
  `scale::select(display scale, emotes.scale)`; Twitch URLs switch to `light` on light themes.
  An image that fails for good (404/410, undecodable) falls back to the emote name as text (the
  store's epoch moves and views re-shape); a transient failure (timeout, 5xx) keeps the
  placeholder and is retried. Hovering an emote shows a tooltip (1:1 preview of the densest variant, name,
  provider, "animated") via `Window::set_tooltip` from the message's prepaint; its hitbox is
  non-blocking so wheel events still reach the list. GPUI re-checks it every frame against the
  pointer and the emote's current box, so it hides when rows move under a still pointer;
  leaving the window hides it via `MouseExitEvent`.
- **Emoji** are drawn by text shaping with the system colour-emoji font (`paint_emoji`), not an
  image set: CoreText falls back to Apple Color Emoji, so ZWJ sequences, flags and skin tones
  shape correctly and align with the text baseline, with nothing to download or license.
- **Animation:** one global clock (time since the image store started) picks every animated
  image's frame, so identical emotes stay in sync. After the rows paint, the view schedules one
  timer for the soonest frame change, only if an animated emote was painted, `emotes.animate`
  is on and the window is active; otherwise no timers run (idle 0 % CPU). GPUI refreshes the
  window on (de)activation, which stops and resumes animation.
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
| Chat read | `twitch-irc` TLS transport and IRC types (rustls); own connection supervisor | Anonymous by default. `gpui_chat_twitch::parse_line` maps a raw line to a `ChatEvent` with no network, and also handles recent-messages history lines. `twitch-irc`'s pooled client is not used: it reports no connection state, has no reconnect backoff and no JOIN pacing |
| Helix + auth | `twitch_api` (helix) + `twitch_oauth2` device flow | Send, badges, emotes, streams, moderation |
| HTTP | `reqwest` (rustls) | `User-Agent: gpui-chat/<version>` |
| Images | `image` crate, WebP preferred, GIF/PNG fallback | Avoid AVIF (native decoder dependency) |
| Secrets | `keyring` | macOS Keychain now; Secret Service / Credential Manager later |
| Config | TOML via `serde` + `toml`, `notify` for hot reload | `~/.config/gpui-chat/{settings,keymap}.toml`, `themes/*.toml` |
| Paths | `dirs` | Cache in the platform cache dir |
| Font | Bundled upstream JetBrains Mono (OFL-1.1) | Default chain: `JetBrainsMono Nerd Font` → bundled `JetBrains Mono` → system monospace |

### Streamlink (`crates/streamlink`, PR 1.6)

GPUI-free, std only (threads and `std::process`). `resolve` finds the binary through an injectable
`Environment` (settings override that must be valid → login-shell `command -v` with a 3 s limit →
`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`), `args::build_args` makes
`[--player P] [extra…] twitch.tv/<channel> <quality>`, `classify::classify_line` turns stderr into a
`StreamError` (offline, no player, other; plus not found / spawn failure from the launcher).
`Streams` keeps one session per channel: `launch` returns at once and a worker thread resolves
(cached until the binary disappears or the override changes), spawns streamlink (stdout null, stderr
piped) in **its own process group**, reads stderr and waits, sending `StreamEvent { channel, session,
Started | Failed(error) | Exited { stopped } }` to the caller's sink. Stop signals the *group*
(`SIGTERM`, `SIGKILL` after 3 s) with the system `kill`, so the player streamlink started goes down too;
`stop_all(timeout)` is the blocking variant for quit, and `Drop` does the same. Launching a channel that
is running restarts it under a new session id.
