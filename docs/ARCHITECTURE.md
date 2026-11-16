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
  emotes/      providers: Twitch, BTTV, FFZ, 7TV; emote sets; resolution order; live updates (7TV EventAPI, BTTV socket); badges (`BadgeSource`, registry); cosmetics (third-party badges, 7TV paints)
  media/       image fetch, disk cache, decode (static + animated), memory budget
  settings/    typed settings, load/save/watch, paths
  streamlink/  locate binary, launch, track child processes
assets/        fonts, icons, bundled themes (`themes/*.toml`); badges/global.json = bundled global-badge snapshot (PR 2.5)
fixtures/      recorded raw IRC lines, API JSON samples                      (added by later PRs)
docs/          ARCHITECTURE.md, BACKLOG.md, SETTINGS.md, THEMES.md
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
  (split panes, PR 4.1). Owns the `ChannelList`, the split `Layout` and the live source; every list
  or layout edit ends in `list_changed` (every shown channel has a pane, the sidebar selection is the
  focused pane's channel, schedule the `state.toml` write). Handles the window actions
  (`ToggleSidebar`, `CloseWindow`, `MinimizeWindow`, `About`, the channel actions below and the pane
  actions); `Quit` is global.
- `splits.rs` (PR 4.1) — `Layout`, plain data: a tree of `Split { axis: Row | Column, children,
  sizes }` whose leaves are panes (`PaneId`, `Option<ChannelId>`), plus the focused pane. A channel
  is shown in at most one pane (showing it elsewhere swaps the two). Split right/down inserts into the
  parent when it runs the same way (halving the split pane's share), else wraps the pane in a new
  two-way split; close gives the space to the sibling that takes its place and collapses splits of
  one and same-axis nesting, and focuses the pane of the heir nearest the closed one;
  `move_pane` / `drop_channel` take a `DropZone` (centre: swap/show, edge: put beside) and
  `accepts_pane` / `accepts_channel` say beforehand whether a drop would do anything (not onto the
  dragged pane itself, no new pane past 16); `resize` moves one line keeping 220 px per pane lined
  up on each side (`extent`), sharing half of the pair when that does not fit so a line never
  freezes; `neighbour` finds the pane on a side from the unit-square rects (keyboard focus);
  `zone_in` / `divider_fractions` are the pixel maths. New panes show the next open channel no pane
  shows. `splits/view.rs` draws it: flex rows/columns sized by `flex_grow(size)` with no gaps,
  Kit's `resize_handle` (themed hairline and pill, straddling the leading edge of every child after
  the first) whose `on_drag` payload the split follows with `on_drag_move`, an accent outline on
  the focused pane when split, and drop zones from `on_drag_move` / `on_drop` for `DraggedPane` (from the channel name in a pane header) and `DraggedChannel` (sidebar
  rows: `ChannelRows`, our `SidebarItem` that wraps each Kit `SidebarMenuItem` in a draggable `div`).
  A press anywhere in a pane focuses it (`capture_any_mouse_down`); keyboard focus follows on the
  next frame (`on_next_frame`, after a new pane's input exists): the pane's input, or the pane's
  own focus handle when it cannot type. Keys (macOS): ⌘D split right, ⇧⌥⌘D split down (⌥⌘D is the
  system's Dock toggle), ⇧⌘W close pane, ⌃⌘←/→/↑/↓ move focus; also in the View menu and as
  buttons in each pane header.
- `theme.rs` — our own `Tokens`, filled from a theme palette (`gpui-chat-settings`:
  `ThemeRegistry::resolve` picks theme + variant from `appearance.theme` / `theme_name` /
  `theme_name_light` and the OS appearance, live via `observe_window_appearance`) and mirrored into
  Kit's `Theme` colours so Kit components match. The user's `themes/` directory is watched and
  re-applied (`attach_reloads`). See `docs/THEMES.md`.
- `settings_window.rs` (PR 4.2) — the settings window (⌘, / the app menu's "Settings…", `OpenSettings`;
  one window, a second ⌘, focuses it, and the app quits when the last *chat* window closes, not when
  only settings remain). A Kit `Settings` (sidebar with search, pages Appearance / Chat / Emote
  providers / Highlights / Streamlink / Account) over `settings.toml`: every control writes through
  `config::edit`, which calls `Settings::update_file` (`gpui-chat-settings`, `toml_edit`: only the
  edited keys change, comments and unknown keys stay, nothing is written unless the *result* is valid
  settings) and then puts the result in the `SettingsStore` at once; the file watcher reports the same
  file and changes nothing. Controls read `config::current`, so a hand edit of the file (watcher →
  store) updates the open window, and a file that fails to load is shown in a banner while the controls
  keep the last good values. `settings_window/fields.rs` has text, number and one-per-line fields that
  commit on Enter / blur / stepping (Kit's own fields write per keystroke) and show a refusal under the
  field; switches and menus show it in the banner. The page's reset button removes the keys (switches
  and menus write the default). The Account page shows `AccountHub`'s state (`account::current`, a
  global set by `AccountHub::start`) and dispatches `LogIn` / `LogOut`, which the window handles with
  the same `sign_in::open` / `log_out` as the main window.
- `fonts.rs` / `assets.rs` — bundled JetBrains Mono TTFs (`assets/fonts/`, OFL in `OFL.txt`) are
  embedded, registered with the text system at startup and the family is resolved against the
  installed list (GPUI panics on unknown families, so there is no blind fallback).
- `actions.rs` + `platform/` — action definitions; key bindings and the native menu bar are
  per-platform and live only under `platform/`. The Kit `native_menu` is a popup context menu;
  the application menu bar uses GPUI `cx.set_menus`.
- `keymap.rs` / `palette.rs` (PR 4.3) — the user's `keymap.toml` and the ⌘K / ⌘T pickers, see `docs/KEYMAP.md`.
  `crates/settings` parses and checks the file (`parse_keymap` / `load_keymap`, GPUI-free; `watch_keymap`, the
  debounced directory watch, like the theme one). The app keeps `KeymapState` (a global): `base` = what Kit
  registered at startup, `defaults` = `platform::default_bindings()`, the user's entries and the problems. Every
  change rebuilds the keymap from scratch (`clear_key_bindings`, base, then `effective(defaults, entries)`) and
  re-sets the menu bar, because macOS reads the shortcuts a menu item shows when the menus are set. An entry
  replaces all default shortcuts of its action (context defaulting to the one the action's first default has) and
  displaces another action's default on the identical keystroke and context (the same keystroke in another
  context is kept and reported). **`keymap` is the only place that registers key bindings**: the rebuild drops
  whatever was bound with `cx.bind_keys` after `keymap::init` (which must run right after `gpui_kit::init`), so
  a feature hands its bindings to `platform::default_bindings` (as `completion` / `emote_picker` do) and its
  actions to the catalog; `apply` asserts the keymap holds exactly Kit's bindings plus the effective ones and
  logs an error if it finds extras. `keymap::catalog()` names every
  bindable action (and whether the palette lists it); a test checks it against `all_action_names`. Problems are a
  strip in the shell (`keymap_notice_banner`, same pattern as the theme one); the UI's shortcut hints (welcome text,
  empty sidebar, emote button) read the live binding through `keymap::shortcut_of`, there are no hard-coded labels.
  `palette.rs` is one Kit `Dialog` holding a Kit `Command` with its own filter switched off (`filterable(false)`):
  rows are ranked with `core::complete::rank` (label matches first, then matches through the group / action id;
  cached per query) and rebuilt on each query change. The palette's rows are the catalog's
  with the shortcut read from the keymap; choosing one closes the dialog (focus goes back to where it was) and
  dispatches the action on the window, as the key would. ⌘T lists `ChannelList::channels()`, and the shell shows
  the choice in the focused pane (`Layout::show`, so a channel in another pane swaps places).
- `channel_list.rs` / `login.rs` / `add_channel.rs` (PR 1.5) — `ChannelList` is the plain-data list of
  open channels plus the selection (add appends and selects, remove selects the next/previous, move
  swaps with the neighbour, restore drops invalid or duplicate logins); `login::normalise_login`
  turns `Forsen`, `#forsen`, `@forsen` or any `twitch.tv/…` URL form (popout, moderator, embed,
  query strings) into a channel, or says why not (3–25 chars of `a-z0-9_`); `add_channel` is the ⌘N
  dialog (Kit `Dialog` + `Input`, the rejection reason under the field). Reorder is by actions
  (⌘⌥↑/↓ and the row's context menu): Kit's `SidebarMenuItem` has no drag hooks.
- `live.rs` (PR 1.5) — `LiveSource`: one `TwitchClient` (anonymous until login, PR 3.2) started inside the backend runtime
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
  subs-only, unique). A pane belongs to its channel: created when the channel is first shown and
  kept (scroll position survives switching and moving between splits) until the channel is closed;
  closing a split only hides it. The header measures its width (a `canvas`) and below 480 px goes
  compact: no room-mode chips, the connection state and LIVE marker as dots (label in a tooltip),
  the play button as its icon; the buttons on the right never shrink and the name truncates.
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
  (`ChatView`: list, follow-bottom, "new messages" pill, live settings), `row` (reply button and
  context menu around a message, PR 3.4; Copy message / username, Open / Copy link, PR 5.6),
  `thread` (the thread popover, PR 3.4), `selection` (selecting text across messages and what
  copying gives, pure, PR 5.6), `selection_layer` (the window side of selection and links: one
  `Surface` per list / popover, PR 5.6), `events` (header line, accent bar and shared-chat label of
  event rows, PR 5.1).
- `streams.rs` (PR 1.6) — `StreamHub`, a GPUI entity created by the shell: owns the `Streams` tracker from
  `crates/streamlink`, turns its events (from worker threads, through a bridge channel) into a per-channel
  `StreamStates` (`Starting` → `Running` → `Stopping`, or `Ended` with the error until dismissed; events
  of an old session are ignored) and exposes `toggle` / `play(quality)` / `stop` / `dismiss`. The shell
  calls `forget` when a channel is closed; an `on_app_quit` hook calls `stop_all`. Settings are read at
  play time (`launch_spec`), so edits apply on the next press. The button is hidden for the offline sources.
- `emote_service.rs` (PR 2.4) — `EmoteService`: the live source's third-party emotes. The global BTTV /
  FFZ / 7TV sets load at startup, a channel's when its ROOMSTATE supplies the room id (once per id;
  a load with a provider error is retried: globals on `Connected`, a channel when ROOMSTATE repeats).
  Every event passes `process` on the way to the bridge (on the backend thread): chat messages get
  their text runs re-scanned with the channel's `EmoteResolver`. `Reload Emotes` (Channel menu)
  refetches everything; `emotes.seventv` / `bttv` / `ffz` switch providers live (a provider that is
  off is not requested; turning one on reloads). `--firehose` / `--replay` have none: no network.
- `badges.rs` (PR 2.5) — `Badges`, a GPUI global holding the `BadgeRegistry` (installed in `main` for every
  source, with the bundled global snapshot already stored, so `--firehose` / `--replay` show moderator,
  VIP, ... without a network), and `BadgeService`, the live source's badge loader. At start it asks its
  source for the global badges once (`BadgeSource::global`, off the UI thread) and stores the result over the
  snapshot, keeping the snapshot with a warn if that fails (the anonymous source returns the snapshot itself;
  Helix, PR 3.1, will fetch the real set). Channel badges follow
  `EmoteService`: a channel's badges load when its ROOMSTATE supplies the room id (once per id; a failed load
  is requested again when ROOMSTATE repeats; a load in flight is never duplicated), `forget` on part drops
  the channel's set and discards a load still running. Unlike emotes, nothing is rewritten in the message:
  views look badges up at shape time. After a store the loader pokes a `Notify`; a UI task bumps an epoch on
  the global (`observe_global`), and a `ChatView` lays its messages out again only when
  `BadgeRegistry::version(channel)` moved, so subscriber badges that arrive a few hundred ms after a join
  appear without a new message having to push them. A source failing is a `warn` and no badge (global ones
  still show). Not wired to `Reload Emotes`.
- `history_service.rs` (PR 5.5) — `HistoryService`, the live source's message history. Every event passes
  `observe` on its way to the UI (next to `BadgeService`): a channel's join confirmation
  (`Connection { channel: Some(_), Connected }`, repeated for every channel after a reconnect) starts a fetch on the
  backend runtime, never delaying the join. *What* to ask for is `gpui_chat_twitch::HistoryTracker` (pure, tested
  there): the first join asks for the newest 100 messages, every later one for the gap since the newest message seen
  (live or fetched, minus a 10 s overlap; the whole history again if no earlier fetch ever succeeded), only the latest
  fetch of a channel is still wanted when it finishes, and `chat.history = false` plans nothing so the channel name is
  never sent. `HistoryClient` (`crates/twitch/src/history.rs`) calls `recent-messages.robotty.de/api/v2/recent-messages/<login>`
  (`limit`, `after`; 8 s timeout, no account or token sent; the name must be a login, `[a-z0-9_]{1,25}`, or nothing is
  requested) and `parse_history` maps the raw IRC lines with the live parser (`parse_line_with`), flags them `history`,
  skips lines with no timestamp, returns `CLEARMSG` / `CLEARCHAT` lines as `Moderation`s with their time (not applied to the
  answer alone) and drops `ROOMSTATE` / `NOTICE` lines (they describe the room as it was). `channel_not_joined`
  (the service's normal answer for a channel it does not record yet; it starts when asked) is an empty history, not a failure.
  The answer is one `ChatEvent::History { messages, moderation }`; third-party emotes are resolved as for live messages. A
  failure is a `warn` plus one quiet system line in the channel; a full gap answer that starts after what was seen
  (`History::may_be_missing_messages`) adds a "Some messages may be missing here." line, stamped to land in the hole.
  `ChannelStore::apply` merges the batch with `MessageStore::merge_history` (by timestamp, de-duplicated by message id, the
  live copy wins; nothing at all happens when nothing is new), then applies the answer's moderation *and* the store's short
  memory of live `CLEARMSG` / `CLEARCHAT` (`live_moderation`) to everything shown: a clear reaches only messages sent at or
  before its time, so what a timed-out user said afterwards survives. Any live message with an id already shown (chat, action
  or user notice) is dropped. When history has to go *before* retained messages the store is renumbered and records a `Rebuild`
  (rows dropped at the front, runs of inserted rows, `map_seq` old to new): `ChatView::sync` replays it as list splices
  (`apply_rebuild`), so measured rows, the reader's scroll position (GPUI moves its anchor for splices above it) and the
  "new messages" count survive, and the layout caches are re-keyed (`LayoutCache::remap`). More merges than the store
  remembers (8) fall back to replacing every row. `apply_rebuild` also moves everything of PR 5.6 that is keyed by a sequence number through `Rebuild::map_seq`
  (`Surface::remap`: the selection and its two end texts, the press's link, the context-menu target, the popover's rows;
  a selection whose end message is gone is cleared) and clears the hovered row; when the view falls back to replacing every row
  `Surface::reset` drops the selection and the target. Any new state keyed by a sequence number must be remapped there too. The renumbering itself is O(scrollback) in the store, once per merge that adds messages before
  others. History rows are drawn at `Tokens::history_opacity` (`chat/row.rs`), derived per theme from how far its muted text
  sits between background and text, and `flags.history` stays on the message for notifications (PR 5.3) to skip. The
  setting is read when a channel is joined, so a change applies to the next join or reconnect.
- `moderation.rs` (PR 5.2) — moderation in the app, plain data and functions (tested without a window). `Moderator` runs a
  `ModRequest` (channel, room id, `core::ModCommand`, the target's user id when a message supplies it): `Live` goes through
  `LiveSource::moderate` (Helix as the logged-in user, a failure is the sentence shown, and the few commands chat does not
  echo, like `/mod`, add a system line), `Loopback` (`--replay` only, `ShellParts::loopback`) turns the command into the events
  the server would send (`loopback_events`: `CLEARCHAT` / `CLEARMSG` / `ROOMSTATE` / a confirmation line) so the controls can
  be tried and screenshotted offline, nothing leaves the machine. `RowAction` (Delete, Timeout(secs), Ban, Unban) is a button
  or menu item on a message and builds the same `ModCommand` typing would; `confirmation` says which ask first (only Ban);
  `is_collapsed` decides the `chat.deleted_messages = "collapse"` row. **Who sees what:** `ChannelStore::role` comes from
  `USERSTATE` (`ChannelRole`: viewer / moderator / broadcaster; reset by any non-connected channel state, a rejoin asks the
  server again), and the controls need that *and* a pane with a `Moderator`. Viewers (and every anonymous run) get none of
  it: no hover tracking, no menu items, static room chips, completion offers only `/me`. `ChatView` draws them
  (`chat/row.rs`: hover buttons beside Reply: four fixed slots on every row, Delete / 10m timeout / Ban / Unban, the ones that do
  not apply disabled so a click always lands on what was aimed at; while the pointer is on the strip the list stands still
  (`ChatView::hold_for_strip`, `Surface::keep_frozen`: held again every frame like a selection, resumed on leaving); ids are
  namespaced with the row's prefix because the list and the thread popover share sequence numbers; the right-click menu's
  Delete message / Timeout… (a submenu of lengths) / Ban… / Unban) and emits `ChatViewEvent::Moderate`; `ChannelPane` subscribes with
  window access (`subscribe_in`, first render), asks first for a Ban (confirmation dialog), runs it and shows a failure in a
  dismissible strip under the header. A 403 means Twitch does not think you moderate here: the role is dropped
  (`ChannelStore::drop_role`) and the controls go until the next join. A moderator's room chips are Kit buttons with a caret (the
  caret is what makes `Button::dropdown_menu` open) listing every mode, with checks and lengths (`pane::room_menu`,
  `core::RoomMode::toggled`); with no mode on, a "Chat modes" button. The composer's `plan_submit` checks the role (*"You need
  to be a moderator of #chan to use /ban"*), shows the command's usage under the box once its name is typed
  (`core::command_hint`) and the completion list only offers what the role may use. **Moderation scopes are opt-in:** a normal
  login asks only for `auth::SCOPES`; the eight `auth::MODERATION_SCOPES` are requested by *Enable moderation tools…* (account
  menu, Settings → Account, or the button on the failure strip) through the same device-code flow with them as extra scopes
  (`Auth::request_device_code_with`, `AccountHub::begin_sign_in(moderation)`); a later normal login keeps them if they were
  enabled. `Account::missing_moderation_scopes` says what a login lacks and is **not** part of `needs_relogin()`, so neither the
  account banner, the title-bar warning icon nor Settings → Account nags anyone who never moderates; using a tool without them says
  "Log in again to enable moderation (grants …)". Own messages (`account::own_login`), the broadcaster's, other moderators' and
  event cards (a `USERNOTICE`) get no controls.
- **Removed messages** (PR 5.2). `CLEARCHAT` / `CLEARMSG` strike messages out as before (`flags.deleted`; the chat view
  draws them dimmed and struck through, or, with `chat.deleted_messages = "collapse"`, as `name: <message deleted>` that a
  click reveals. The collapsed line is the same `MessageElement` with another display text (`display::build_collapsed`, key
  `LayoutKey::collapsed`), so selecting, copying and the right-click menu work on it. Collapsing is another row height:
  `ChannelStore::struck_since` (a bounded log of the sequence numbers a delete / timeout / ban / clear changed) makes the view
  measure just those rows again, and `revealed` is pruned to messages the store still holds). `MessageFlags::removal` says why
  a message was removed (`Deleted`, `Cleared`, `TimedOut`, `Banned`; a stronger one replaces a weaker): Unban is offered only for
  the last two, a timeout stays available after a delete. Event cards (`USERNOTICE`) are not chat lines: no collapse, no strike.
  `core::clear_line` makes the system line ("alice was timed out for 10m.", "alice was banned.", "Chat was cleared by a
  moderator.") from an event that has its `tmi-sent-ts`; its id is built from what happened and when, so the live copy and
  recent-messages history (`ChannelStore::with_clear_lines`, lines older than anything shown are left out) are one line, and
  it is flagged `history` like the messages around it. `mark_all_deleted` and a history channel clear only touch chat lines,
  never system lines (the line a clear made must not be struck by it).
- **Highlights, unread dots, Mentions, notifications** (PR 5.3). `core::highlight` is the pure part (tested there):
  `HighlightConfig` (the `[highlights]` section, embedded as is in `Settings`), `Highlights` (compiled once: phrases are
  escaped and compiled as case-insensitive `regex`es with a manual whole-word check, regexes are the user's own; the `regex`
  crate never backtracks, patterns get a size limit and one that matches the empty text is refused) and
  `Highlights::evaluate(&Message) -> Option<Hit>` (colour of the first match: own name, then rules in order, then users;
  `notify` if any match says so; never the user's own messages). `MessageFlags::highlight` carries the colour,
  `flags.mention` is set from the own-name rule (so it also covers history, whose parser did not know the login).
  `attention.rs`: `Core` (no GPUI: engine + `activity::Activity` + `notifications::Coalescer`, all tested) and the
  `Attention` entity, which `ChannelHub::apply_batch` calls with each channel's events **after routing** (only for a channel that
  has a store, so a closed or never-opened channel counts for nothing) and before its store sees them: it stamps messages
  (cloning only the ones a rule hit), counts unread / unseen mentions for channels that are not *watched*, logs hits for the
  Mentions panel and returns notifications to post and banners to dismiss. The hub also tells it which messages are the *copy*
  of a shared-chat line another open channel shows (`ChannelHub::open_rooms`): drawn, never counted. Matching runs all regexes in
  one `RegexSet` pass and looks users up in a map; unseen mentions are kept per channel (`Activity::unseen`, O(1) for the
  sidebar) and the log has a `generation` the panel's cached rows follow. Own login: the account (`account::own_login`), `viewer` for the
  offline sources; the display name is learned from the user's own messages. History is stamped but never counted;
  deletes / timeouts / clears take hits out of the log. `activity.rs` is the state: a channel is watched while a pane shows it
  and the window is active (`AppShell::push_view` tells `Attention::set_view` from `sync_layout` and window-activation
  changes); showing it in the *focused* pane clears its unread count and unseen mentions. The sidebar row's suffix draws the
  mention pill or unread dot (`shell::Dots`), `mentions.rs` the panel beside the panes (a `uniform_list` of fixed-height rows) (`View ▸ Show Mentions`, ⇧⌘M; click =
  `AppShell::show_message`: show the channel, `ChatView::scroll_to_message`, mark seen; the view holds still (`jump_hold`, ended by
  scrolling or the pill), flashes the row and the shell says so when the message left the scrollback). `notifications.rs` builds the text and
  the tag (`hit:<channel>:<message id>` / `summary`, parsed back on a click) and rate-limits (one per 5 s, the rest one summary
  after a timer); `platform/notify.rs` is the only place that calls GPUI's `show_system_notification` /
  `on_system_notification_response` (macOS `UNUserNotificationCenter`; authorisation is requested by GPUI on the first
  notification, nothing happens outside an app bundle). A click emits `AttentionEvent::Show`, which the shell turns into
  `show_message` / opening the panel. The settings window's Highlights page (`settings_window/highlights.rs`) edits the lists as
  one-per-line text (`{green, notify} pattern`) and writes `[[highlights.rules]]` / `[[highlights.users]]` through
  `EditValue::Tables`.
- `account.rs` / `sign_in.rs` / `platform/keychain.rs` (PR 3.1) — the Twitch account. `AccountHub` (GPUI
  entity, live source only: offline runs never touch the keychain) mirrors `gpui_chat_twitch::Auth`'s
  `AuthState` (a `watch` channel) into the UI: on start the backend restores the stored login and then
  validates it hourly. The title bar's right end shows "Log in" or the user's name with a menu (log in
  again, log out); a strip above the content says why the user was logged out (refresh rejected,
  unreadable keychain item) or that the login lacks scopes the app now asks for. `sign_in.rs` is the
  login sheet (Kit `Dialog`): it starts the device flow on open, shows the user code (copy button) and
  "Open browser" (Twitch's activation link, which carries the code; only an `https` link on `twitch.tv` or a
  subdomain is opened, else `https://www.twitch.tv/activate`), closes itself on success, and
  cancels the backend task whenever the sheet goes away (`on_release`). `KeychainTokenStore` is the
  `TokenStore` over `keyring`: one generic-password item, service `APP_ID` (or
  `$GPUI_CHAT_KEYCHAIN_SERVICE`), account `twitch`, holding the whole login as one JSON record. App menu: *Log In to Twitch…*, *Log Out* (`LogIn` / `LogOut` actions).
- `composer.rs` (PR 3.2) — `Composer`, the input box under each live pane's chat (the offline sources have none). A
  single-line Kit `Input`: Enter sends, so a paste can never send several messages (a multi-line paste is
  flattened to one line by the `Input::on_paste` hook, line breaks → spaces). ↑/↓ recall what was sent
  (`core::InputHistory`, 100 lines per pane; actions `ComposerHistoryPrevious/Next`, bound in
  `platform::default_bindings` to `ChatComposer > Input` so they beat the input's own cursor keys). `plan_submit`
  (pure) turns the text into *nothing / refuse with a reason / send to room id* using `core::parse_input`
  (`/me text` = action, `//text` = a literal leading slash, the moderation commands of PR 5.2 are `Plan::Moderate`, any other `/command` refused, 500-character limit) and
  the channel's `ROOMSTATE` room id. The box clears at once; if the send fails the reason shows in a strip above
  the box and the text comes back (unless the user typed something new; ↑ finds it anyway). `Mode` (from the
  `AuthState`) disables the box with its placeholder and a *Log in* button when logged out, still restoring, or
  when the login lacks `user:write:chat`.
  PR 3.4 adds *reply mode* (see `reply.rs`, `chat/row.rs` and *Replies and threads* below).
- `live.rs` (PR 3.2 additions) — with an account, the client gets a `CredentialsProvider` (below) and
  `follow_login` calls `TwitchClient::reconnect` whenever the account IRC should use changes (login,
  logout, another user), after the stored login was read. `LiveSource::send` posts through
  `HelixClient::send_chat_message` on the backend runtime, returns the outcome on a `oneshot`, and on success feeds
  the *echo* (`gpui_chat_twitch::echo_message`: the text tokenized, the id Twitch returned, `EmoteService::process`
  for third-party emotes) into the same bridge as every incoming event. The echo is flagged `MessageFlags::echo`: `ChannelStore::apply` lets the IRC copy (same id, real emotes,
  colour, badges) **replace** it in place (`MessageStore::replace`; the view remeasures the row through
  `ChannelStore::replaced_since`), and drops any other chat or action line whose id is already in the store, so the
  message shows once whichever arrives first. `follow_login` compares the account IRC should use with the login
  the provider *actually connected as* (`ConnectedAs`), so a first connection that went anonymous because the
  keychain was slow is corrected when the login arrives.
- `completion.rs` / `composer/emotes.rs` / `emote_picker.rs` / `emote_thumb.rs` / `emote_sources.rs` (PR 3.3) —
  autocomplete and the emote picker over the composer. **What a word asks for and how candidates rank is in
  `core::complete`** (unit-tested there): `find_trigger` reads the token ending at the caret (`@name` users,
  `/name` as the first word commands, `:name` with two characters or more emotes, any other word as emotes on
  Tab), `rank` orders by tier (case-sensitive prefix, case-insensitive prefix, camelCase/digit/`_` "hump",
  substring, subsequence) then recency, shorter name, alphabetical; `apply` makes the edit (token replaced,
  sigil added for users and commands, a space after). `Autocomplete` (`completion.rs`) holds the open list
  (candidates, selection, an 8-row scroll window) and draws it; `Composer` routes keys to it through actions
  bound to `ChatComposer > Input` (Tab / Shift-Tab / Esc act only while there is something to complete and otherwise
  `cx.propagate()`, so the input's own Escape and a parent's, e.g. cancelling a reply, still see the key; ↑/↓ go to the
  list while it is open, else to history; Enter accepts only once the user has moved in the list or asked for it
  with Tab, otherwise it sends the text as typed). A press outside the list, or the input losing focus, closes it. Tab opens the list for a plain word (a lone candidate is
  taken at once); Esc keeps it shut for that word. The caret is re-checked at render (arrow keys and clicks emit
  no change event). Candidates: the channel's `EmoteCatalog` (`crates/emotes`: the user's own Twitch emotes, then
  channel 7TV → BTTV → FFZ, then global, each name once; plus per-provider lists), `core::recent_chatters` over the
  pane's `MessageStore` (newest speaker first; the display name is inserted, the login is matched), and
  `core::COMMANDS` (a test keeps it in step with `parse_input`). The picker (`emote_picker.rs`, ⌘E / Ctrl-E or the
  🙂 button) is its own entity in a `deferred(anchored(..))` above the box: tabs Recent · Twitch · 7TV · BTTV · FFZ
  (empty third-party tabs hidden, Twitch always, to say why it is empty), a Kit `Input` for search (a search spans
  every provider, best match first), a `uniform_list` grid of 9 emotes per row so only visible rows build and
  request images. Keys, bound to `EmotePicker > Input` after the composer's so they win in the search field:
  arrows move, Tab / Shift-Tab change tab, Enter inserts and closes, Esc closes, a click inserts; a press outside it
  closes it (not on its 🙂 button, whose bounds the composer tracks, so that toggles); clicking a tab ends a search. Thumbnails
  (`emote_thumb.rs`) go through the chat's `Images` store (de-duplicated, cached, pinned per view in a
  `FrameImages`), the first frame of an animated emote. `EmoteSources` owns the user's own Twitch emotes (asked from
  Helix `Get User Emotes` once per user and room as soon as the room id is known and the login has
  `user:read:emotes`; follower emotes of other channels are dropped (`UserEmote::usable_in`); a failure shows on the Twitch tab and is asked again when the picker opens) and rebuilds the
  catalog only when a set's identity or those emotes changed. Recently used emotes (40, by name, shared by every
  pane) are kept in `state.toml` under `[emotes] recent`, so the Recent tab shows those the current channel can
  still use. Nothing here applies to the offline sources (they have no composer).
- `stream_info.rs` (PR 3.5) — `StreamInfoHub` (GPUI entity, live source only, created by the shell next to the
  `AccountHub`) mirrors the snapshots of `gpui_chat_twitch::streams::StreamPoller` (see *Stream state* below)
  and answers `liveness(channel)` (`Unknown` / `Offline` / `Live(info)`) and `followed_live()`. The shell calls
  `set_channels` with the open channels on every list edit. The pane header shows a LIVE / Offline marker after
  the channel name and, while live, a second line: title (tooltip: full title and category), category, viewers
  and uptime; sidebar rows of live channels get a red dot and a compact viewer count (tooltip: title, category,
  viewers, uptime). A second sidebar group, *Followed — live*, lists the followed channels that are live
  (category, dot, viewers); clicking one adds (if needed) and selects the channel, like ⌘N. The group is
  hidden while the list is unknown (logged out, login without `user:read:follows`). Uptime and viewers refresh
  with each poll (every minute). Logged out, nothing is polled and the header/sidebar show no stream state.
  `GPUI_CHAT_DEMO_STREAMS=1` with `--firehose` / `--replay` fakes the data (no network, no login) so the UI can
  be looked at and screenshotted offline.
- `state.rs` — `state.toml` in `<config dir>/<APP_NAME>/` with the last window placement
  (validated against connected displays on restore), `[channels]` (`open` in sidebar order,
  `selected`), `[emotes]` (`recent`, PR 3.3: the emotes used last, newest first) and `[layout]`
  (PR 4.1: `focused`, the focused pane's index in reading order, and `root`, a nested
  `channel` / `axis` + `sizes` + `panes` tree; checked against the open channels on restore, broken
  sizes (anything not a normal positive `f32`, or a total that overflows) evened out, at most 16
  panes / 8 levels; a file without it opens one pane on `selected`). The shell writes `[channels]`
  and `[layout]` together in one read-modify-write. Separate from user settings. Sections load independently (a malformed `[channels]`
  leaves `[window]` intact and vice versa) and each writer replaces only its own top-level key in the
  file's TOML table under one lock, so other sections (even ones this build cannot read) survive; a
  file that is not TOML at all is moved to `state.toml.bad` before a new one is started. Restore
  normalises every saved login (`Forsen`, `#forsen` still restore) and drops invalid or duplicate ones
  and a stale selection.

### Onboarding, problems and accessibility (PR 6.3)

- **First run.** `ShellParts::first_run` is true when the live source starts with no `[channels]` section in
  `state.toml` (the first channel added writes it, so removing every channel later is *not* a first run). While it is
  true and no channel is open the main pane shows `empty::welcome`: what the app is, then *Add a channel* (opens the
  ⌘N dialog; this button has keyboard focus, `apply_focus`), *Log in to Twitch* (only while logged out) and *Choose a
  theme* (opens Settings, which lands on Appearance). `AppShell::add_channel` clears the flag, so it is never shown
  again. There are no suggested channels: "who is live" needs a Twitch token and the first run has none.
- **Empty states** are `empty::empty_state` (title + one hint line, `text_muted`, `Role::Status`): no channels open, an
  empty pane, a channel with no messages yet ("Waiting for chat…", or "Joining the channel…" before the join, or "No
  messages match the filters"), Mentions. A single muted line in a list stays a plain line (sidebar, find bar count,
  user card header).
- **Problems.** `problems.rs` is the one place a failure the user can act on is reported: `Problem` (severity, title,
  lines, optional `Fix` button, raw `details` for *Copy details*) filed under a **cause key** in the `Problems` global.
  The same key replaces the problem (no duplicate strips); a dismissed problem stays hidden while its wording is
  unchanged and comes back when the wording changes or the cause is cleared and returns. `problems::strip` draws one
  (the only strip component: panes' moderation and streamlink strips, the composer's send error, the settings window
  banner and the shell's strips all use it). The shell files what it can see itself in `sync_problems` (settings /
  keymap / theme files, the login, the chat connection) every render, idempotently; background sources report through
  `problems::report_from_anywhere` / `clear_from_anywhere` (an unbounded channel drained by a UI task started in
  `problems::init`; no-ops before `init`, so tests and offline runs are unaffected): the emote providers and the
  FFZ / Chatterino badge lists (`emotes`), badge loads (`badges`), the stream poll (`streams`), keychain writes
  (`keychain`), a missing settings folder (`location`), file watchers that could not start (`watch`), the image cache
  (`images`) and a corrupt `state.toml` (`state`). Raw error text goes behind *Copy details*; the strip says what it
  means and what to do (`problems::plain` cuts absolute paths to file names).
- **Accessibility.** GPUI exposes roles and labels through AccessKit (`.role(Role::…)`, `.aria_label`,
  `.aria_selected`, `Button::accessibility_label`); every icon-only button has a label and the clickable rows, cells and
  tabs have a role (`Role::Alert` for strips, `Status` for empty states, `Dialog` for the user card, `Complementary` for
  Mentions, `ListItem` / `Button` / `Tab` for rows). Keyboard: the Mentions panel takes focus when it opens (Up / Down /
  Home / End / Page keys move a ringed row, Enter shows it, Esc closes and gives focus back to the pane); the user card
  takes focus when it opens (Tab walks its buttons with Kit's focus ring, Esc closes and focus returns to the chat); the
  find bar's buttons are Tab stops. **Reduce motion:** `gpui_kit::init` reads the macOS setting into
  `App::reduce_motion`; `config::animate_emotes` is `emotes.animate && !reduce_motion` (everything that animates reads
  it), and `GPUI_CHAT_REDUCE_MOTION=1` forces it for a run. **Text:** the settings window uses the chat font size like
  the main window (and scales its sidebar with it); chrome text is `text_sm` / `text_xs` (rem-relative, rem = font
  size). **Contrast:** `crates/settings` pulls a theme's `text_muted` and `accent_text` to 4.5:1 on every surface they sit
  on, and a test over every bundled theme and variant asserts text, muted text, the text on an accent fill, `danger_text`
  and `accent_link` all reach 4.5:1 on all five surfaces. *Increase contrast* is not exposed by GPUI and is not honoured
  (BACKLOG).

### Event messages (PR 5.1)

- **Model** (`core::special`, plain data; the parser in `crates/twitch/src/parse.rs` fills it, every tag also stays in
  `raw_tags`). `Message::event: Option<EventKind>` for a `USERNOTICE` — `Sub` / `Resub` (plan, months, streak),
  `Gift` (recipient, months, the gifter's total, anonymous, `community` = one of a mystery gift's lines),
  `MysteryGift`, `GiftPaidUpgrade`, `PrimePaidUpgrade`, `Raid`, `Announcement { color }`, `NewChatter`
  (`ritual`), `BitsBadgeTier` and `Other { msg_id }`: a notice whose tags are too incomplete for its variant
  (a subgift without recipient, a raid without a count) and any `msg-id` this build does not know are `Other`,
  drawn from Twitch's own `system-msg`. `Message::reward_id` (`custom-reward-id`; Twitch sends no name),
  `flags.highlighted` (`msg-id=highlighted-message`), `flags.first_message`, `bits`, and
  `Message::shared: Option<SharedChat>` (`source-room-id`, `source-id`, `source-badges`; `source_login` /
  `source_name` are filled in later by the app). In shared chat a notice of the *other* channel is
  `msg-id=sharedchatnotice` with the real kind in `source-msg-id` (and that kind's `msg-param-*`): the parser
  dispatches on that, and the row is labelled with its source like a chat line.
- **Rows** (`chat/events.rs`, pure except `Look::paint`). The *header* is a line of text in the row's display text
  (`Role::Header`) followed by a hard break and the user's message: `22:15 Alice resubscribed at Tier 1 for 12
  months, 12 month streak⏎Alice: a year Kappa`. A `USERNOTICE` carries the timestamp on the header; a chat line that
  is only marked (`First message`, `Highlighted message`, `Redeemed a reward`, `Cheered 100 bits`, joined by ` · `) has
  the marker above and the timestamp on the message. The wording is ours where the tags give enough, Twitch's
  `system-msg` for `Other`. Being text, headers are selected and copied like any other (`selection::message_line`
  uses `events::notice_line`; the menu's *Copy message* adds the event line, `events::copy_text`; the marker of a
  chat line is not copied). `Look` is the accent (`Accent`: support, raid, bits, points, first message, announcement
  colour; mapped to theme tokens by `Colors`, only announcement blue and purple are fixed hues) drawn as a 3 px bar in
  the row's left padding plus a 8 % tint, by `MessageElement::paint` before the text (a mention keeps its own tint;
  a per-recipient line whose gift bomb row is not there is `quiet`: no bar, muted). The look is decided once, when the
  message is shaped (`ShapedMessage::look`), not at every paint, and nothing about it enters the layout cache key.
  A message sent in another channel of a shared chat session starts with `[OtherChan]` (`[shared chat]` while the
  name is not known; `Role::Source`, muted; on the header line for a notice) and shows the badges the author has
  *there* (`source-badges`, looked up in the source channel's sets when it is open, else the generic ones). A whole-row
  copy keeps the label and the marker, like a partial selection. History rows are the same, dimmed by the existing
  history opacity.
- **Gift bombs.** `submysterygift` is one row (`EventKind::MysteryGift { origin_id, recipients, .. }`); Twitch then sends
  a `subgift` line per recipient (`community_gift_id` = that `origin_id`, running total 0). `ChannelStore` does not
  show those lines: it appends the recipient to the row (`EventKind::with_recipient`, each name once) and replaces
  the row in place (the same `replaced` log as an echo, so the view remeasures it), and the header reads
  `Bomber is gifting 12 Tier 1 subs to the community (512 gifted in total): Ann, Ben, Cleo and 9 others`. History
  batches are folded before they are merged (`fold_gifts`). A line whose row is not retained (evicted, not in the
  history answer) is shown on its own, quietly. Not handled: history lines folded into a row that was already shown live
  are not added to it.
- **Hard breaks.** `DisplayText::breaks` lists byte offsets of a one-space character that `ShapedMessage::from_layout`
  turns into an `AtomKind::Break` atom (zero width); `flow` ends the line after it (no empty line after a trailing
  break) and `index_at` never puts a caret after it. Everything else (glyph painting, hit-testing, selection)
  treats it as a space.
- **Cheermotes.** `Fragment::Cheer { prefix, bits, image }`; `image` is the tier's image (an `Emote` named by the
  prefix, 28 px at 1x) once the channel's `CheermoteSet` (`core::cheer`; `resolve` also finds a channel's own
  prefix in a message with bits) has been applied. `HelixClient::get_cheermotes` (`crates/twitch/src/cheermotes.rs`,
  `GET /helix/bits/cheermotes?broadcaster_id=`, needs a login, no scope) builds the set: the dark image of each
  tier, animated when there is one (`themed_url` swaps `/dark/` for `/light/` on a light theme). The word is drawn
  as an image box through the emote path (so animation, tooltip, placeholders and size handling are the emotes'), the
  amount follows as bold text. Without a set (logged out, not loaded yet, failed) the text stays and the marker line
  says how many bits. `special_service.rs` (`SpecialService`, live source) loads a channel's set when its `ROOMSTATE`
  supplies the room id and the user is logged in (and for every channel on login), once per room id, again after a
  failure when `ROOMSTATE` repeats; `process` runs on every live event and in `HistoryService`. Messages that arrive
  before the set keep their text. `GPUI_CHAT_DEMO_CHEERMOTES=1` with `--replay` applies a built-in `Cheer` / `Kappa`
  set (public CDN, no login) to look at the images offline.
- **Shared chat.** Twitch delivers a shared-chat message to every channel of the session, each copy with an id of its
  own and the room it was sent in as `source-room-id`. Every channel **keeps its copy** (replies, threads and
  mentions resolve inside one channel; what is shown never depends on which other channels happen to be open).
  The source is named so the row can say where it came from: `ChannelHub::name_shared_sources` fills `source_login`
  (and `source_name`) from the other open channels' `ROOMSTATE` room ids (also those announced in the same batch), no
  lookup; a source that is not open is looked up once per room id by `SpecialService`
  (`gpui_chat_twitch::RoomNames`, `api.ivr.fi/v2/twitch/user?id=`, so it works logged out; only the public room id is
  sent) and named on the messages that arrive after the answer; until then `[shared chat]`.

### Replies and threads (PR 3.4)

- **Data.** `parse_line` already turned the `reply-parent-*` / `reply-thread-parent-*` tags into `Message::reply`
  (`ReplyRef`: parent id, login, display name, body, thread root). PR 3.4 adds `ReplyRef::root_id()` (the thread
  parent, or the parent when no root is tagged), `ReplyRef::to(&parent)` (the reference a reply to `parent` carries,
  used for the echo), `Message::thread_root()` and `MessageStore::thread(root_id)` (the root if retained, then every
  retained message whose reply names that root; a scan, called only for the thread being looked at).
- **Context line.** A reply is drawn with a smaller, muted line above it, `↳ Replying to Name: parent text`,
  cut with `…` to the width. It is **not** part of the flowed display text: `ShapedMessage::reply` holds a second,
  single-line shaping (`ReplyLine`: atoms cut by the same `cut_atoms` as the message, one trailing `…` atom), made
  with the message in `shape` (so it follows font/size changes through the layout key). Cutting depends on the width
  and is done at paint time (`ReplyLine::fit`), so the cache's flow level (a resize) never re-shapes it. The row's
  height is `MessageLayout::height()` = reply line + flowed text; `MessageElement` measures with that, offsets the
  text origin by `reply_height()` (emote boxes, tooltips and underlines follow) and gives the line its own hit area:
  pointing hand, text colour on hover, click → `MessageStyle::on_open_thread` with the thread root.
  The thread popover's own style sets `reply_line: false` (and no click handler).
  **The leading mention is not drawn.** Twitch's copy of a reply starts with `@<parent login> `; the context line
  already says who is replied to, so `display::body_fragments` leaves out one leading `Mention` of the reply's parent
  login (any capitalisation) and one space after it, when something else remains (a reply that is only the mention
  keeps it). Display only: `Message::fragments`, `plain_text()` and the raw tags are untouched, so copy and other text
  actions (PR 5.6) can still have the full text. The local echo carries no mention, so it and Twitch's copy that
  replaces it draw the same.
- **Reply action.** `chat/row.rs` wraps rows that can be replied to (`reply::can_reply_to`: chat or `/me` lines by a
  user, not deleted; only when the pane has a composer): rows report whether the pointer is over them
  (`MessageStyle::hovered_row`), and only that row is wrapped in a `div` with a Reply button; one right-click menu
  around the list (and one around the thread popover) resolves the hovered row when it opens (*Reply*, *View thread*,
  disabled when the message is neither a reply nor replied to). `ChatView` emits
  `ChatViewEvent::ReplyRequested(message)`; `ChannelPane` hands it to `Composer::queue_reply`, which the next render
  turns into `start_reply` (it needs the window).
- **Reply mode** (`reply::ReplyTarget`, pure; `composer.rs`). A strip above the box names the parent (`Replying to
  Name: text`, cancel button, Esc); the box is left as it is, **without** an `@name`: Twitch puts `@parent ` in front
  of a reply sent with `reply_parent_message_id` itself (a prefilled mention came out doubled in the live check).
  Esc is one chain with the completion list (close the list, else cancel the reply, else propagate). Enter sends with `SendRequest::reply`: `LiveSource::post` passes its parent id to
  `HelixClient::send_chat_message` (`reply_parent_message_id`) and puts the same `ReplyRef` on the echo, so the
  context line shows at once and Twitch's own copy replaces it as before. Reply mode ends with the send; a failed send
  restores it with the text. Cancelling leaves the text alone.
- **Thread popover** (`chat/thread.rs`). Opened by a context line or *View thread*; a panel at the top right of the
  chat (`ChatView::thread`) with the root message and the replies the store still holds (newest 200), read from the
  store on every render so new replies appear. Its rows are ordinary `MessageElement`s with a `LayoutCache` and hover
  state of their own (same message, other width); the caches follow evictions, generation changes and image
  invalidations together (`ChatView::caches`). A root that left the store is introduced by the reply tags (author and,
  for a direct reply, the text).

### User card (PR 5.4)

A card over the chat with a user's avatar, names, badges, account age, this session's messages and actions
(`crates/app/src/chat/user_card.rs`; pure parts in `core::user_card`, Helix in `twitch::users`).

- **Opening.** Click an author's name or an `@mention` (`DisplayText::users`, hit-tested by `selection_layer::Surface` like links:
  pointing hand, opens on release, not after a drag; the surface calls the callback the view installs with `set_on_user`), *View user*
  in the right-click menu (the name/mention under the pointer, else the author; `MenuTarget::user`), or the `OpenUserCard` action
  (palette, bindable: the first selected message's author). `ChatView::open_user_card` takes focus, so Esc reaches `ClearSelection`
  there first: it closes the card, then (as for a selection) asks the pane to refocus the input box. With no card open Esc is exactly
  what it was (selection, else the composer chain: completion list, reply, propagate). A press outside the card (`on_mouse_down_out`)
  closes it (the press inside it keeps the chat's selection: the card tells the surfaces where it is, `Surface::set_overlay`). One Esc
  closes one thing: the card, else the selection. It is a `deferred(anchored())` element at the pointer, opening **upward** below the
  middle of the window (the newest lines, the ones people click, are at the bottom, and a card over the pointer would swallow the second
  press of a double-click, which must still select the word); Kit's `Popover` needs a trigger element and has no point anchor, which a
  click on painted text does not have. Names are also clickable on collapsed removed lines (a moderator unbans from there) and as the
  parent's name on a reply's context line (`ReplyLine::user_span`).
- **Content** (`CardModel`, built each render from `CardMessages`, which remembers the user's sequence numbers and only looks at what
  arrived since the last frame, so a busy channel with a 100k scrollback costs nothing extra; it starts over when history renumbers the
  store): the user's messages (newest first, 200 at most, a `uniform_list` of fixed-height rows; history marked, removed ones struck
  through or collapsed per `chat.deleted_messages`, each a click that scrolls the chat to it and closes the card), the badges and name of their newest message, and the account part: avatar (`profile_image_url` through the image
  store like an emote), display name + login, `Account created 14 Mar 2021 (5 years old)`, `Following since …`. Logged out (or an
  offline run) there is no Helix: the card says "Log in to see account details" (with a Log in button when there is a login owner)
  and the rest still shows.
- **Account lookups** (`user_directory::UserDirectory`, one entity over the pure, tested `Cache` state machine): `GET users?login=`
  on the backend runtime through a oneshot, once per login (loading / found / missing are kept; a failure is retried by the card's
  Retry or by opening the card again). `GET channels/followers` needs `moderator:read:followers` (never requested at login): it is
  only tried when the login was granted that scope (`Account::scopes`) and is a moderator there, and any refusal is "unavailable":
  the line is left out. The caches are dropped when the logged-in account changes (an answer still in flight for the old one is
  ignored). Nothing blocks the UI thread.
- **Whose account.** Moderation is aimed at the Twitch id of the account that *sent* the message the card was opened from (else their
  newest message's id), not at whoever owns the login now: after a rename another account may have the old name. If Helix's id for
  the login differs, the card says so and offers no moderation. A user known only from a mention (no message in the store, so no
  badges: they could be a moderator or the broadcaster) gets none either.
- **Avatar.** The image goes through the media cache like an emote; the workspace's `image` build now decodes JPEG too (Twitch avatars are very often `…-600x600.jpeg`). While it loads the box is a placeholder circle; logged out, with no avatar, or when it failed for good (`Images::failed_for_good`: not found, not an image, undecodable) it is the circle with the name's initial (`avatar_show`). A transient error keeps the placeholder and is retried.
- **Actions.** Copy username, Mention (`ChatViewEvent::Mention`: `Composer::insert_mention` puts `@login ` at the caret and focuses
  the box), Reply to their newest replyable message (`ReplyRequested`, the existing path), Profile (`core::profile_url`: always
  `https://www.twitch.tv/<login>`). For a moderator: Timeout presets, Ban… (asks first) and Unban, as `ChatViewEvent::ModerateUser`
  → `ChannelPane::user_action` → the same `confirm_and_run` / `run_command` as the buttons on a message, so Helix, the live
  moderator and the replay loopback, the missing-scope "Enable moderation tools" strip and the 403 handling are shared. `core::card_actions`
  decides what is offered: nothing to viewers, to oneself, to the broadcaster, to another moderator unless the user is the broadcaster,
  to a user not seen in a message, or to a user known only from shared-chat copies of another room; Unban is always offered to a moderator (the card has no ban to
  read it off). Every action closes the card.

### Search, filters and ignores (PR 5.7)

In-channel search (⌘F), the find bar's filters, ignored users / phrases and the Twitch block list. The logic is pure and
tested in `gpui-chat-core`; the app only draws it.

- **`core::search`.** `Needle` (a trimmed, lowercased query) and `find_ranges` (byte ranges into the *original* text, so the
  row can highlight them even where lowercasing changes a character's length); `search_text(&Message)` is what a message offers
  (display name, login, system line, body with emotes by code, lowercased, newline-separated so a one-line query never matches
  across them); `parse_query` takes a `from:login` token out of what was typed. **`SearchIndex`** keeps that text for every
  message of a `MessageStore`, in step with it (`sync`: the first call and any history merge that renumbered the store, i.e.
  `rebuild_serial` moved, index everything; otherwise only the evicted ones drop and the new ones are added). **`Search`** holds
  the matches of one query and never rescans the scrollback per keystroke: a query that extends the last one narrows the previous
  matches (so only they are looked at), going back to a shorter one restores its saved matches and scans only what arrived since
  (`Stage`s), a new message is checked once when it arrives, and a scan of the whole index happens only for an unrelated query
  (the first character, a paste) or a changed `allowed` set (a filter). A test pins the numbers at 100 000 messages. `step`
  walks the matches with wrap-around.
- **`core::filter`.** `Filters` (`from`, `mentions`, `links`, `hide_bots`) with `matches(message, &BotList)`;
  `visibility(message, &Ignores, &Filters, &BotList) -> Shown | Collapsed | Hidden` is the one answer to "does this message
  get a row, and which"; `FilterConfig` is `[filters] bots`.
- **`core::ignore`.** `IgnoreConfig` (`[ignores]`) compiled into `Ignores` with the conventions and code of
  `core::highlight` (`Matcher`, `check_regex`, the size / nesting limits; phrases one by one, regular expressions in one
  `RegexSet`), plus the block list (`set_blocked`: ids and logins) and the local user (`set_own`: never ignored). `is_ignored`:
  chat / `/me` by user or text, event cards by user only, notices and system lines never.
- **`ignores.rs` (`IgnoreHub`).** One GPUI entity per app (created at startup after the account hub, `ignores::init`) that owns the
  compiled `Ignores` (an `Arc`), the bot list, the block list and a `generation`. It observes the settings (recompiling only when
  `[ignores]` / `[filters]` changed) and the account hub (the user's login: own messages; the block list). Observers: every
  `ChatView`, `Attention` and the settings page. `ignore_user` / `unignore_user` write `ignores.users` through `config::edit` (the
  hub then sees its own write like any settings change). **Block list** (`crates/twitch/src/blocks.rs`): `HelixClient::get_blocked_users`
  (`GET users/blocks?broadcaster_id=<own id>`, paged, 100 at a time), `block_user` (`PUT users/blocks?target_user_id=`) and
  `unblock_user` (`DELETE`), each checking the login's scope first (`BlockError::MissingScope`, nothing sent) and turning
  Twitch's answers into sentences. The scopes (`auth::BLOCK_LIST_SCOPES`) are opt-in exactly like the moderation ones:
  `account::Features { moderation, block_list }` is what a sign-in asks for on top of the normal scopes, and a later login keeps
  what was enabled; `Account::block_list_enabled` is not part of `needs_relogin`. The hub fetches the list when a login with the
  scopes is there and `ignores.sync_block_list` is on (a stale answer, from another account or an older fetch, is dropped by
  an epoch), and has `block` / `unblock` (the id the message carried, else a `GET users?login=`) returning the result to the
  pane, which asks for confirmation first (`ChannelPane::confirm_block`) and shows a failure in the same dismissible strip the
  moderation tools use, with an *Enable block list sync* button for `FailureKind::BlockListScope`.
- **Rows (`chat/list_sync.rs`, `Rows`).** The chat list's items used to be every message of the store (item `i` is
  `first_seq + i`). `Rows` is that, *or*, when something may be hidden (`!engine.is_empty() || filters.is_active()`), an explicit
  ascending list of the sequence numbers that have a row. `sync` (evictions at the front, appended messages at the back, each
  asked whether it gets a row), `show_only` (the filter changed: the new list is diffed against the old one, `diff_rows`) and
  `renumber` (a history merge: old rows renumbered through `Rebuild::map_seq`, the new store range filtered, diffed) all return
  `Splice`s (and a row can be a *placeholder*, `Rows::is_placeholder`: the `1 ignored message` line of an ignored message
  with `show_collapsed`; it is a row, but the message is not on screen, so it is not searched, not copied and has no
  right-click menu, until a click opens it), so the list is still only ever *spliced*: measured rows and the scroll position of what stays survive every change.
  Everything that mapped a message to an item through `first_seq` goes through `Rows` now (`index_of` / `seq_at`): the render
  closure, `scroll_to_message`, `remeasure_seqs`, the pill's count (`rows_from`), and the selection surface (`set_rows`: scrolling
  to hold the list, the autoscroll's first / last row, and **copying skips hidden rows** between the two ends of a selection).
- **Visibility is cheap per keystroke.** `core::visibility_with` checks the filters (a few comparisons) *before* it asks whether
  a message is ignored, and the ignore verdict (text read, phrases, `RegexSet`) is cached per message
  (`core::IgnoreCache`, forgotten when the ignores change, the store is renumbered or a message is replaced in place). Typing
  `from:` therefore never runs the ignore rules again over the scrollback, and a `from:` token that may still be being typed is
  not applied until it is finished (a space after it, Enter, or 250 ms without a key), so each prefix of a login is not a filter
  of its own.
- **The view (`chat/view/find.rs`).** `ChatView` has the `Rows`, the engine / bots it last got from the hub (its `generation`
  says when to recompute), the `Filters` and, while the bar is open, a `Find` (the Kit `Input`, the `Search`, the current match).
  `sync` takes the store's changes into the rows first and then the search (`allowed` = has a row). The bar is a `h_flex` above
  the list with key context `FindBar`; its Esc / ↑ / ↓ are bound to `FindClose` / `FindPrevious` / `FindNext` in
  `FindBar > Input` (registered in `platform::default_bindings`, like the picker's), Enter / Shift-Enter arrive as
  `InputEvent::PressEnter { shift }`. Stepping and typing scroll with the Mentions view's path (`scroll_to_row`: a few rows of
  context, following paused, the row flashed); closing emits `ChatViewEvent::SelectionDismissed`, which the pane already turns into
  "focus the input box", so the box's own Esc chain (completion list, then reply mode, then propagate) is untouched: it only runs
  while the box is focused. ⌘F / ⌘G / ⇧⌘G are shell-level bindings that act on the focused pane's chat (`AppShell::with_chat`).
  The `Search` (index and matches) lives on the `ChatView`, not on the bar: ⌘F again only catches the index up (new messages,
  evictions, a renumbering) instead of reading every message; while the bar is closed it is not kept current. A query only
  matches messages that are shown as themselves (`Rows::is_shown`: not hidden by a filter, not an ignored placeholder); a message
  replaced in place (an echo swapped for Twitch's copy) is read again (`Search::replaced`, `IgnoreCache::invalidate`) and
  its visibility re-checked; after a history merge the saved stages are dropped and the current match follows
  `Rebuild::map_seq`. Matches are painted by `MessageElement::paint_search` from `MessageStyle::search` (`SearchHighlight`: the needle and the current
  seq): `find_ranges` over the row's display text, skipping the timestamp and badge spans, theme `warning` at 38 % (70 % for the
  current match), behind the text and after the selection. The strip below the bar ("Showing N of M messages · from alice ·
  mentions", **Clear filter**) is drawn whenever a filter is on.
- **Collapsed ignored rows.** With `ignores.show_collapsed` and no filter, `Visibility::Collapsed` rows stay in the list and
  `message_row` draws them with `MessageElement::ignored(true)`: another display text (`display::build_ignored`, `1 ignored
  message`, no name), another `LayoutKey` (`ignored`), no tints or reply line; a click reveals the message through the same
  `revealed` set as a collapsed removed message.
- **Block list upkeep.** The fetch is keyed by the account's user id (another account's list is cleared the moment a fetch for
  this one starts and never applies to it), a failure is retried with a 30 s → 15 min backoff, and while sync is on the list is
  fetched again every 15 minutes (so blocks made on twitch.tv are picked up) or on **Refresh** (Settings → Ignores). A block or
  unblock from the app only edits the list when sync is on and a list is loaded. The hub rebuilds (and every chat view
  re-checks its rows) only when the user's own login, the account id or the block-list scope state actually changed, not on every
  `AccountHub` notification. The user can never be ignored or blocked (`IgnoreHub::can_target`, checked in `ignore_user`,
  `block`, the pane's confirmation and the key-bound actions).
- **Attention.** `Core::set_ignores` takes the engine; an ignored message is stamped as usual but never counted (no dot, no
  Mentions entry, no notification), and `Activity::drop_ignored` takes an ignored author's earlier entries out of the log when the
  engine changes, `Coalescer::forget` takes their held notifications out of the summary and their banners are dismissed.
  A jump (Mentions, a notification, the user card) to a message hidden by an ignore says *That message is hidden (ignored user).*

### Domain model (`crates/core`)

Dependency-free (std only), no I/O, no clock. Modules: `ids` (`ChannelId` = lowercase login,
`MessageId`, `EmoteId`), `emote` (`Emote`, `EmoteProvider`, `ImageUrls` by scale, `BadgeId` /
`Badge`), `message` (`Message`, `Author`, `Fragment`, `MessageKind`, `MessageFlags`, `ReplyRef`),
`store` (`MessageStore`), `event` (`ChatEvent`, `RoomState`), `tokenizer`, `search` / `filter` / `ignore` (PR 5.7, above), `compose` (PR 3.2: `parse_input`,
`sanitize_line`, `InputHistory`), `complete` (PR 3.3: `find_trigger`, `rank`, `apply`, `recent_chatters`, `Recents`).

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
  emotes stack on the preceding emote. IRC lines are tokenized (in `crates/twitch`) before a
  channel's sets are known, so the app resolves third-party emotes in a second pass
  (`core::resolve_fragments`, called by `EmoteService`), on whole words only, with the same
  stacking rules as the tokenizer. Messages shown before the sets arrive keep plain text.
- **Emote providers (`emotes` crate):** `EmoteLoader::load_global()` / `load_channel(twitch_user_id)`
  query 7TV, BTTV and FFZ concurrently and return one `Result<EmoteSet, EmoteError>` per
  provider (`ProviderResults`, iterated in precedence order 7TV → BTTV → FFZ); a failing provider
  never blocks the others and a channel with no account (404) is an empty set. FFZ uses the
  official API with the BTTV mirror as fallback. All HTTP goes through the `HttpClient` trait so
  tests run offline against `fixtures/emotes/`.
- **Emote catalog (`emotes` crate, `catalog` module, PR 3.3):** `EmoteCatalog::build(own_twitch, sets)` merges what
  one channel's input box can offer: every name once in the precedence order above (Twitch's own first, as the IRC
  tags win in chat), plus per-provider lists (channel before global) for the picker's tabs.
  `EmoteResolver::sets()` exposes the sets it resolves against.
- **Badges (`emotes` crate, `badges` module):** `BadgeSource` (`global()` / `channel(room_id)` → `BadgeSet`, boxed
  futures like `HttpClient`) is the seam; `BadgeSet` is `set → version → Arc<Badge>` so a lookup is two hash probes
  with no allocation, and `BadgeRegistry` layers a channel's set over the global one and numbers each store.
  `AnonymousBadgeSource` is the logged-out source: global = `assets/badges/global.json` (`include_str!`,
  574 definitions, 47 KB), channel = `GET api.ivr.fi/v2/twitch/badges/channel?id=<room id>` (404 = no badges of
  its own). The snapshot stores set, version, title and the Twitch CDN image id (`…/badges/v1/<id>/{1,2,3}`
  = 1x/2x/4x; an entry whose URLs do not fit is refused when encoding); regenerate with
  `cargo run -p gpui-chat-emotes --example badges_snapshot` (`-- --check` says whether it is stale). Only
  metadata is bundled: the images always load from Twitch's CDN through the image store. The Helix source
  (PR 3.1) implements the same trait; in the app, `AccountBadgeSource` asks Helix while logged in (falling back
  to the anonymous source when a Helix request fails), and `BadgeService::follow_login` reloads the global and
  every open channel's set when the user logs in or out.
- **Live emote updates (`emotes` crate, `live` module, PR 6.2):** `LiveEmotes` keeps at most two websocket
  connections, one per provider, shared by every open channel, and only while something is subscribed
  (`live::socket::run`: connect, run the provider's `Session`, on any failure back off 1 s → 60 s with jitter and
  resubscribe from scratch; a connection that stays up resets the backoff). 7TV (`wss://events.7tv.io/v3`,
  `live::seventv`): per channel `emote_set.update` of its active set and `user.update` of the owning 7TV user
  (a switch of the active set fetches the new set, `SevenTvClient::set`, and re-subscribes), plus — when paints
  or 7TV badges are on — `cosmetic.create` / `entitlement.create` / `entitlement.delete` with the channel as
  context. BTTV (`wss://sockets.betterttv.net/ws`, `live::bttv`): `join_channel twitch:<room id>`, then
  `emote_create` / `emote_update` / `emote_delete`. Frames are parsed by pure functions (fixtures
  `fixtures/emotes/7tv-events-*.jsonl`, `bttv-events.jsonl`: the 7TV ones were recorded, except the pulled /
  updated / `user.update` ones, written from the documented shape; the BTTV ones are written from the protocol),
  applied to the channel's set in the `EmoteRegistry` (`EmoteSet::with_changes`: add, rename, remove by emote id; the
  next message resolves against the new set, messages already shown keep what they had) and reported as one
  coalesced quiet system line per channel and provider (`live::notices`: "7TV: added PogU, KEKW · removed Old",
  written out 1.5 s after the first change, also while the socket is idle or waiting to reconnect; names are cut to 32
  characters). Reconnecting: a connection counts as healthy only after it stayed up 30 s (a server that says
  `Hello` and ends the stream does not reset the backoff); the EventAPI's end-of-stream codes ask for a pause (a
  restart comes back at once, a refusal waits 2–5 minutes); subscriptions are sent once, after `Hello`. Frames are
  limited to 1 MiB and the heartbeat interval the server asks for is clamped to 5–120 s. An overlapping set switch is
  superseded by the later one (each route keeps the set it is waiting for; late answers are dropped), and a channel
  whose owner has an account but no active set still listens for `user.update`. A BTTV `emote_update` is a rename of
  the emote the channel has (it often carries just `{id, code}`), never a replacement. A provider switched off in the settings holds no connection; a channel
  that closes unsubscribes. `EmoteService` opens a channel once its sets loaded (the load records the 7TV ids in
  `EmoteSet::origin`), `GPUI_CHAT_7TV_EVENTS_URL` / `GPUI_CHAT_BTTV_SOCKET_URL` point the connections elsewhere and
  `cargo run -p gpui-chat-emotes --example live_replay -- fixtures/emotes/live-demo.jsonl` is a local server that
  replays a script of frames, to watch an add / rename / remove in the app. Not recovered: changes made while a
  connection was down (see BACKLOG).
- **Cosmetics (`emotes` crate, `cosmetics` module, PR 6.2):** `Cosmetics` holds what is worn next to and on a
  name. *7TV*: `StyleLookup` asks for chatters it does not know (`userByConnection(platform: TWITCH)`, up to 50 per
  GraphQL `GET`, then `cosmetics(list: ...)` for the definitions that are missing; the query complexity limit rules
  out one request), the EventAPI keeps styles current; a style is trusted for 30 minutes. *FFZ*
  (`api.frankerfacez.com/v1/badges/ids`) and *Chatterino* (`api.chatterino.com/badges`) publish their holders once
  (`cosmetics::bulk`). BTTV's badges are SVG, which the image pipeline cannot draw, so there are none. Badge
  definitions live in the `BadgeRegistry`'s third-party layer (sets `seventv` / `ffz` / `chatterino`; a switched-off
  provider hides its set without forgetting it, and the registry version moves so views lay out again), and a badge
  reaches a message as one more `BadgeId` after Twitch's own, added where emotes are resolved
  (`EmoteService::with_cosmetics`, live and history alike), so rendering, tooltips (title "7TV: …", line "7TV
  badge") and copy are the ones every badge has. A chatter's *first* message may precede the answer of the lookup
  and then has none (BACKLOG). *Paints* (`cosmetics::paint`): `Paint` is gradient / flat colour / image data plus
  `color_at(x, y, w, h)` (CSS linear and radial gradient maths, `repeat`) and the luminance of every stop and
  midpoint for readability (a translucent colour is judged as it looks over the background); paints are looked up by
  Twitch user id at paint time. Everything from the network is limited: at most 32 stops, 3 shadows offset by at most
  8 px, 20 000 styles (30-minute TTL, oldest dropped first), 4 096 paint and badge definitions, and image URLs are
  accepted only as `https` on the providers' hosts (`emotes::TRUSTED_IMAGE_HOSTS`: 7tv.app, 7tv.io, betterttv.net,
  frankerfacez.com, chatterino.com, fourtf.com (where Chatterino's badge list really serves from), jtvnw.net).
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
  PR 5.6 hit-tests with `Flow::line_at` / `index_at` / `atom_at` (point ↔ byte offset in the
  display text).
- **Name paints** (PR 6.2, `MessageElement::paint`). GPUI paints a glyph in one `Hsla` (`Window::paint_glyph`; a
  gradient `Background` exists for quads only), so a paint is drawn glyph by glyph: each glyph of the author's name
  gets `Paint::color_at` at its middle within the name's box (`ShapedMessage::name_extent`, worked out when the
  message is shaped; the paint itself is looked up per painted row, a read lock and a hash probe, with no
  allocation), and a shadow with an offset is the glyph again behind it in the shadow's colour (glows, which have
  no offset, and image paints are not drawn: the plain colour is used). `colors::paint_readable` falls back to the
  plain colour whenever any colour of the paint (stops and their midpoints) has under 3:1 contrast against the
  theme's background, so a paint is judged per theme. Removed, collapsed and ignored lines, event headers, reply
  lines and the user card keep plain colours; the readability check is against the row's real background (the
  event card's or highlight's tint included); a gradient is sampled once per glyph at the middle of the line, so a
  vertical one (angle 0° / 180°) shows as a single colour, and a radial one follows the paint's `shape` (circle or
  ellipse); selection, copy, search and highlights work on the display text,
  which paints do not touch.
- **Selection and links** (PR 5.6, `chat/selection.rs` pure, `chat/selection_layer.rs` GPUI). A
  selection is two units (anchor, head) of `(store seq, byte offset in the display text)`, so it
  spans messages; a click picks a caret, a double-click a word (a run of non-spaces inside one
  span: a name, a mention, a whole link), a triple-click the message, and dragging extends by the
  same unit. Each surface (the list, the thread popover) has one `Surface`: an empty canvas put
  before the rows registers the mouse handling once per frame, and each `MessageElement` only
  reports its bounds and layout in prepaint and paints its own part of the highlight (theme
  accent at 30 %, behind the text) — so a mouse move that changes the selection re-renders the
  view once from the layout cache; nothing is shaped or flowed again. Dragging past the top or
  bottom autoscrolls (until the first / last message is in view). From a left press until the
  selection is gone the list stops following (held where it is, the "new messages" pill shows), so
  a double / triple click lands on the text first clicked; a click that selects nothing lets it
  follow again after the double-click interval (500 ms), Esc or a press outside the chat at once,
  and only if the selection is what stopped it. Only a change of display text (timestamps, badge
  definitions) drops the selection; a font, line-height or theme change keeps it. Copy (⌘C in the
  `ChatView` key context, or the palette) writes one line per message: a whole message as
  `HH:MM name: text` from the stored message (original text, emote codes), a partly selected one
  as the selected text without badge titles, from the display text of the two end messages as it
  was when the pointer was on them (snapshotted, since their layouts may be dropped or rebuilt).
  The menu's *Copy message* is different on purpose: just the message's text, no time or name.
  Links (`DisplayText::links`, from the tokenizer's `Fragment::Link`) get a pointing-hand hit area
  per line; a click that starts and ends on the same link opens it with `cx.open_url` (http/https
  only) once the double-click interval has passed, so a double-click selects it instead. Kit's own `TextSelection` layer (gpui-base) is
  not used: it works on GPUI `TextLayout` runs, and these rows lay out their own atoms.
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
  An image that fails for good (404/410, undecodable) falls back to the emote name as text, and one
  that finishes at another size than its placeholder (BTTV often omits sizes) gets a new box: either
  way the image store appends its URL to a bounded change log (`Images::changes_since`), and each view
  drops only the cached layouts that draw that URL and remeasures only those rows
  (`LayoutCache::invalidate_urls`, `ListState::remeasure_items`); a view that fell further behind than
  the log reaches re-shapes everything. A transient failure (timeout, 5xx) keeps the
  placeholder and is retried.
  **Zero-width stacks** (7TV overlays) are one inline atom covering the base's name, each overlay's
  name and the spaces between; the box is as wide and tall as the biggest layer, layers without an
  image are left out (a stack with none stays text) and the painter draws each layer centred in the
  box, base first. Hovering an emote shows a tooltip (1:1 preview of the densest variant, name,
  provider, "animated"; one column per emote of a stack, base first) via `Window::set_tooltip` from the message's prepaint; its hitbox is
  non-blocking so wheel events still reach the list. GPUI re-checks it every frame against the
  pointer and the emote's current box, so it hides when rows move under a still pointer;
  leaving the window hides it via `MouseExitEvent`.
- **Badges** are drawn before the name (after the timestamp) as inline boxes through the same path as emotes: the
  display text has one `Role::Badge` span per drawable badge (its title, never painted as text) followed by a
  space, and shaping turns each into one `Inline` atom of a fixed square size (`1.3 × font size`, 18 px at the
  default 14 px; 1x–4x variant chosen from display scale × box size). `shape` looks each `BadgeId` of the
  author up in the channel's registry (O(1)) and asks `Images::badge_box`; a badge with no definition, no URLs
  or an image that failed for good gets no slot, so there is nothing to draw and no gap. The box does not
  depend on the image's own size, so a finished load never re-lays out a message; a permanent failure after
  shaping goes through the per-URL change log like an emote's. The placeholder is the faint quad. Hovering one
  shows the tooltip (preview, title, "Twitch badge"; `chat/tooltip.rs`, shared with emotes via
  `InlineContent`).
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
| Chat read | `twitch-irc` TLS transport and IRC types (rustls); own connection supervisor | Anonymous until login, then the user's token (PR 3.2). `gpui_chat_twitch::parse_line` maps a raw line to a `ChatEvent` with no network, and also handles recent-messages history lines. `twitch-irc`'s pooled client is not used: it reports no connection state, has no reconnect backoff and no JOIN pacing |
| Helix + auth | Own `reqwest` client in `crates/twitch` (`auth`, `helix`), see *Login and Helix* | Device flow, single refresh owner, badges, send chat; emotes, streams, moderation later |
| HTTP | `reqwest` (rustls) | `User-Agent: gpui-chat/<version>` |
| Images | `image` crate, WebP preferred, GIF/PNG fallback | Avoid AVIF (native decoder dependency) |
| Secrets | `keyring` | macOS Keychain now; Secret Service / Credential Manager later |
| Config | TOML via `serde` + `toml`, `notify` for hot reload | `~/.config/gpui-chat/{settings,keymap}.toml`, `themes/*.toml` |
| Paths | `dirs` | Cache in the platform cache dir |
| Font | Bundled upstream JetBrains Mono (OFL-1.1) | Default chain: `JetBrainsMono Nerd Font` → bundled `JetBrains Mono` → system monospace |

### Login and Helix (`crates/twitch`, PR 3.1)

GPUI-free, tested on Linux against a local mock server (`wiremock`, dev-only).

- **`auth::OAuthClient`** — `id.twitch.tv` (base injectable): device code, device-grant poll (Twitch's
  `{"status", "message"}` errors mapped to pending / slow down / expired / denied), refresh, validate
  (`Authorization: OAuth`), revoke. Public client: only the client id is sent, never a secret.
  `auth::client_id()` is `DEFAULT_CLIENT_ID` unless `$GPUI_CHAT_TWITCH_CLIENT_ID` is set; `auth::SCOPES` is the
  one scope list.
- **`auth::TokenStore`** — blocking `load` / `save` / `clear` of one `StoredToken` record (both tokens, expiry,
  scopes, login, user id): access and refresh token are only ever written together. `MemoryTokenStore` for
  tests and Linux; the app's keychain store lives in `crates/app/src/platform/keychain.rs`.
- **`auth::Auth`** — the single owner of the tokens. Tokens sit behind one async mutex with a *generation*;
  a caller needing a refresh (about to expire, or a 401) passes the generation it saw, the refresh runs under
  the mutex in a task of its own (a cancelled caller cannot abort it after Twitch rotated the one-use refresh
  token), and later callers that find the generation moved simply take the new token: N concurrent failures
  make one refresh. The new pair is saved before the mutex is released. A rejected refresh (400/401) logs out,
  clears the store and publishes `AuthState::LoggedOut { notice }`; a network error keeps the login.
  `restore` + `validate_now` (start) and `run_validation` (hourly): a 401 refreshes, a refused refresh logs out
  with a notice. `complete_device_login` polls at `interval`, adds 5 s on `slow_down`, stops at `expires_in`.
  Cancelling the device login only stops the polling: once Twitch issued a token, validating and installing it
  runs in a task of its own (an unvalidatable token is revoked), so the keychain, memory and published state
  agree. `restore` ignores what it read if the login changed meanwhile (a device login finished first).
  `logout` clears the store and revokes the token. `Secret` (every token) prints as `[redacted]`.
- **`helix::HelixClient`** — GET below a base URL (injectable) with `Client-Id` and `Bearer`; a 401 whose body
  says the token is invalid/expired refreshes through `Auth::refresh_after_unauthorized` and retries once (a
  401 for a missing scope or Client-Id mismatch is returned as it is); `Ratelimit-*` headers are remembered, an empty
  bucket waits for the reset (at most 60 s) and a 429 is retried once after it.
- The original plan named `twitch_api` / `twitch_oauth2`; they are not used: the flows above are a few
  requests each, and owning them gives the injectable base URLs and the refresh guarantee directly.

### Chat sending and the authenticated connection (`crates/twitch`, PR 3.2)

- **`HelixClient::post_json`** — like `get` (token from `Auth`, one retry after a refresh on an invalid-token 401,
  one retry after the rate-limit reset on a 429) with a JSON body; `send_request` is the shared request.
- **`HelixClient::send_chat_message(broadcaster_id, text, reply_parent)`** — `POST /helix/chat/messages` as the
  logged-in user (`sender_id` = the token's user id; scope `user:write:chat`, already in `auth::SCOPES`). Every
  failure is one `SendError` whose `Display` is the sentence shown under the input: `Dropped { code, message }`
  (HTTP 200, `is_sent: false`; Twitch's `drop_reason.message`), `Rejected { status, message }` (400/403/422 with
  Twitch's text; a 401 that survived the refresh, i.e. a missing scope, asks for a new login), `RateLimited`,
  `Helix(..)` (not logged in, session ended, network). `/me text` is sent as `/me text`.
- **Authenticated IRC** — `ClientConfig::credentials_provider` (a `CredentialsProvider`: an async closure) is asked
  before **every** connection attempt, so a reconnect hours after login presents a refreshed token. The app's is
  `Credentials::from_auth(&Auth)`: waits (≤ 5 s) for the stored login to be read, then the user's login with a fresh
  access token, or anonymous when logged out, when the login lacks `chat:read`, or when no token can be had.
  The provider is told how many connections in a row ended with `NOTICE * :Login authentication failed` (a token
  revoked before its local expiry): at 1 `from_auth` refreshes the token although it has not expired, at 2+ the
  connection stays anonymous; an accepted connection (`001`) resets the count.
  `TwitchClient::reconnect()` drops the connection and reconnects at once (no backoff), rejoining the wanted
  channels; `set_credentials` still works for a fixed login but a provider overrides it. Tests: the supervisor
  against the in-memory server (provider consulted per attempt, reconnect with login / logout, reconnect while
  connecting) and `chat_tests.rs` against a wiremock Helix / `id.twitch.tv`.
- **Echo and de-duplication** — Twitch may not send a user's own message back over IRC, so on `is_sent` the app
  shows the message itself (flagged `echo`), as the account whose token made the request (`SentMessage` carries
  its login and user id), under the id Twitch returned; Twitch's own copy, if it comes, replaces it, and any other
  second chat line with that id is ignored.

### Moderation (`crates/twitch` and `crates/core`, PR 5.2)

Twitch removed the IRC chat commands, so every moderation action is a Helix request as the logged-in user
(`crates/twitch/src/moderation.rs`, tested against wiremock and as pure request building).

- **`core::moderation`** — pure: `parse_mod_command` (`/timeout user [duration] [reason]`, `/ban`, `/unban`, `/delete id`,
  `/clear`, `/slow [s]`, `/slowoff`, `/followers [duration]`, `/followersoff`, `/subscribers[off]`, `/emoteonly[off]`,
  `/uniquechat[off]`, `/mod`, `/unmod`, `/vip`, `/unvip`, `/announce`, `/raid`, `/unraid`, `/shoutout`; durations are `90`, `10m`,
  `1h30m`, `1w`, `3mo`; ranges are Twitch's: timeout 1 s to 14 d, slow 3 to 120 s, followers up to 3 months in whole minutes) into
  `ModCommand`; `COMMANDS` (usage, summary, who may use it) for completion and the hint under the box; `RoomMode` toggling;
  `row_actions(role, channel, message)` (none for viewers, for lines nobody wrote, for the broadcaster's own messages and for
  another moderator unless the user is the broadcaster); `clear_line`. `parse_input` returns `Input::Command` for these; anything
  else starting with `/` is still "Unknown command".
- **`build_request(command, Ids)`** — the method, path (query values percent-encoded) and JSON body of each endpoint: `POST
  moderation/bans` (timeout with `duration`, ban without), `DELETE moderation/bans` (unban), `DELETE moderation/chat` (one
  message with `message_id`, else the whole chat), `PATCH chat/settings` (only the changed mode), `POST chat/announcements`,
  `POST chat/shoutouts`, `POST` / `DELETE moderation/moderators`, `channels/vips`, `raids`. `required_scope` names the OAuth scope
  each needs; `auth::MODERATION_SCOPES` are those eight, deliberately **not** in `auth::SCOPES` (see the app section: opt-in).
- **`HelixClient::moderate(broadcaster_id, command, target_id)`** — checks the login's moderation scopes first (a login that did
  not enable them gets `ModError::MissingScope`: *"Log in again to enable moderation (grants moderator:manage:banned_users)"*,
  nothing is sent; `ModError::is_forbidden` is a 403), resolves
  the target's login with `GET users?login=` unless the caller knows the id (a message's author), sends through
  `request_json` (any method; same refresh-on-401 and rate-limit handling as `post_json`) and maps Twitch's answer: 2xx is
  done, a 401 naming a scope is `MissingScope`, other refusals carry Twitch's own message, 429 is `RateLimited`.
- **`USERSTATE`** is parsed (`ChatEvent::UserState { channel, role }`; the broadcaster has `broadcaster/1` and no `mod=1`; staff / admin / global-mod badges are not moderator status in the channel).
- A ban or timeout without a reason omits `reason` from the body.

### User emotes (`crates/twitch`, PR 3.3)

- **`HelixClient::get_user_emotes(user_id, broadcaster_id)`** — `GET /helix/chat/emotes/user` (scope
  `user:read:emotes`, already in `auth::SCOPES`) through `get_all` (up to 100 pages). With a broadcaster the answer
  also holds that channel's follower emotes, so the app asks per channel (a superset of the user's emotes
  elsewhere). Each entry becomes a `UserEmote { emote, owner_id, kind }` with `Emote::twitch`'s standard CDN URLs
  (`animated` when Twitch lists the animated format); entries without id or name are dropped and an emote in two
  sets is kept once. A 401 about the scope is `HelixListError::is_missing_scope`. Tested against a wiremock Helix
  (paging with an escaped cursor, user and broadcaster in the query, scope error, Twitch's message).

### Stream state (`crates/twitch`, PR 3.5)

- **`helix::HelixClient::get_all`** — GET a list endpoint and follow `pagination.cursor` (`after=`, percent-encoded)
  until it ends (a repeated or empty cursor also ends it; `max_pages` bounds it). Non-2xx is
  `HelixListError::Status` with Twitch's `message` (`is_missing_scope()` for a 401 about a scope).
- **`streams::get_streams`** — `Get Streams` for any number of logins, `user_login=` ×100 per request (Helix's
  limit), duplicates dropped; offline channels are simply absent, other types are ignored (an empty `type` counts as live); the followed list is de-duplicated by channel.
  **`get_followed_streams`** — `Get Followed Streams` for the logged-in user, 100 per page, up to 10 pages.
- **`streams::StreamPoller`** — one task on the backend runtime. Each cycle polls the watched channels (the open
  ones) and, when the login has `user:read:follows`, the followed list, and publishes a `StreamSnapshot` (`live`,
  `checked` — to tell "offline" from "not polled yet" —, `followed`, `error`) on a `watch` channel. Interval 60 s
  (Helix caches stream data about that long); each of the two calls has its own schedule, so one that keeps
  failing doubles to a 5 min cap alone (the last good data stays, `error` set) while the other keeps its interval; a change of the watched set (compared as a set: reordering is none) polls the open channels after a 500 ms settle, a login change polls at once,
  logged out it publishes an empty snapshot and sleeps. A 401 refreshes through `Auth` (the HelixClient's
  retry), so an expired token costs no poll. Dropping the poller stops it.

### Streamlink (`crates/streamlink`, PR 1.6)

GPUI-free, std only (threads and `std::process`). `resolve` finds the binary through an injectable
`Environment` (settings override that must be valid → login-shell `command -v` with a 3 s limit →
`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`), `args::build_args` makes
`[--player P] [extra…] twitch.tv/<channel> <quality>`, `classify::classify_line` turns its output into a
`StreamError` (offline, no player, other; plus not found / spawn failure from the launcher).
`Streams` keeps one session per channel: `launch` returns at once and a worker thread resolves
(cached until the binary disappears or the override changes), spawns streamlink (stdout and stderr
share one pipe, because streamlink prints its `error:` lines on stdout) in **its own process group**,
reads that output continuously (bytes, capped lines, so it never blocks the child) and waits, sending `StreamEvent { channel, session,
Started | Failed(error) | Exited { stopped } }` to the caller's sink. Stop signals the *group*
(`SIGTERM`, `SIGKILL` after 3 s) with `/bin/kill`, so the player streamlink started goes down too;
`stop_all(timeout)` is the blocking variant for quit, and `Drop` does the same. Launching a channel that
is running restarts it under a new session id.
