//! `gpui-chat` binary: the only crate that depends on GPUI (via GPUI Kit).
//!
//! GPUI / GPUI Kit signatures used here were checked against the pinned sources
//! (`gpui-kit 0.9.1`, `gpui-component 0.9.1`, `gpui-pre 0.3.12`); see the PR 0.2 description.

mod account;
mod actions;
mod add_channel;
mod assets;
mod badges;
mod bridge;
mod channel_list;
mod channels;
mod chat;
mod cli;
mod config;
mod diagnostics;
mod emote_service;
mod firehose;
mod fonts;
mod hud;
mod images;
mod live;
mod login;
mod pane;
mod platform;
mod replay;
mod runtime;
mod shell;
mod sign_in;
mod state;
mod streams;
mod theme;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_chat_core::APP_NAME;
use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::account::AccountHub;
use crate::actions::Quit;
use crate::channel_list::ChannelList;
use crate::channels::ChannelHub;
use crate::cli::{Cli, Parsed};
use crate::diagnostics::Diagnostics;
use crate::firehose::FirehoseConfig;
use crate::live::LiveSource;
use crate::shell::{AppShell, DEFAULT_SIZE, MIN_SIZE, ShellParts};
use crate::state::DisplayInfo;

/// Events the bridge queue holds before a producer is slowed (or, for the firehose, sheds).
const INGEST_QUEUE: usize = 16_384;
/// Most events one frame applies; the rest stay queued for the next frame.
const INGEST_BATCH: usize = 4_096;
/// Chat ingest is applied at most this often (see `bridge::BridgeReceiver::with_min_interval`):
/// at most ~60 ingest-driven draws per second however fast messages arrive, while a message
/// arriving after a quiet spell is still applied on the very next frame.
const INGEST_MIN_INTERVAL: Duration = Duration::from_millis(16);

/// Where to open the main window: the saved bounds on the saved display when that display is
/// still connected and the rect fits on it, else centred on the primary display.
///
/// GPUI reports/accepts window bounds relative to the display the window is on, so the
/// display is restored through `WindowOptions::display_id` rather than by global coordinates.
fn initial_placement(cx: &App) -> (WindowBounds, Option<DisplayId>) {
    let centred = || {
        (
            WindowBounds::centered(size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx),
            None,
        )
    };
    let Some(saved) = state::load().window else {
        return centred();
    };
    let primary = cx.primary_display().map(|display| display.id());
    let displays = cx.displays();
    let infos: Vec<DisplayInfo> = displays
        .iter()
        .map(|display| {
            let bounds = display.bounds();
            DisplayInfo {
                uuid: display.uuid().ok().map(|uuid| uuid.to_string()),
                is_primary: Some(display.id()) == primary,
                width: bounds.size.width.as_f32(),
                height: bounds.size.height.as_f32(),
            }
        })
        .collect();
    match saved.restore_display(&infos) {
        Some(index) => (
            shell::bounds_from_placement(&saved),
            Some(displays[index].id()),
        ),
        None => centred(),
    }
}

/// Hooks the chat backends up to the UI: a hub of per-channel stores fed through the bridge, and
/// one source on the backend runtime. By default that is the live anonymous Twitch client, which
/// joins the channels saved in `state.toml`. `--firehose` and `--replay` *replace* it: nothing is
/// joined (and the channel list is neither read from nor written to `state.toml`), so an offline
/// run never touches the user's saved channels.
fn start_chat(
    cli: &Cli,
    replay_log: Option<Arc<str>>,
    window: &Window,
    cx: &mut App,
) -> ShellParts {
    let diagnostics = Diagnostics::new();
    let scrollback = config::current(cx).chat.scrollback;
    let offline = cli.firehose.is_some() || replay_log.is_some();
    let hub = cx.new(|_| {
        let mut hub = ChannelHub::new(scrollback, diagnostics.clone());
        // Live channels exist because the user opened them; offline ones through their events.
        hub.set_auto_create(offline);
        hub
    });
    let (sender, receiver) = bridge::channel(INGEST_QUEUE, INGEST_BATCH);
    bridge::attach(
        &hub,
        window.window_handle(),
        receiver.with_min_interval(INGEST_MIN_INTERVAL),
        cx,
        |hub, events, cx| hub.apply_batch(events, cx),
    )
    .detach();
    // The account (login, Helix) exists only with the live source: offline runs never touch the
    // keychain.
    let (channels, live, account) = if offline {
        (ChannelList::default(), None, None)
    } else {
        let saved = state::load().channels.unwrap_or_default();
        let account = AccountHub::create(cx);
        let live = LiveSource::start(sender.clone(), account.clone(), cx);
        let account = account.map(|(auth, helix)| AccountHub::start(auth, helix, cx));
        (ChannelList::from_saved(&saved), Some(live), account)
    };
    if let Some(rate) = cli.firehose {
        let start_ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        let config = FirehoseConfig {
            rate,
            seed: cli.seed,
            channels: firehose::channel_names(cli.firehose_channels),
            start_ts_ms,
        };
        // The handle is dropped on purpose: the task ends when the bridge closes or on quit.
        drop(runtime::spawn_backend(
            cx,
            firehose::run(sender.clone(), config, diagnostics.counters()),
        ));
    }
    if let Some(log) = replay_log {
        let speed = replay::Speed::from_factor(cli.replay_speed);
        drop(runtime::spawn_backend(
            cx,
            replay::run(sender, log, speed, diagnostics.counters()),
        ));
    }
    ShellParts {
        hub,
        channels,
        live,
        account,
        diagnostics,
        hud_visible: cli.hud,
    }
}

fn main() {
    let cli = match cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(cli)) => cli,
        Ok(Parsed::Help) => {
            print!("{}", cli::USAGE);
            return;
        }
        Err(error) => {
            eprintln!("{APP_NAME}: {error}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    // Read the log before any window exists, so a bad path fails with a message, not a blank app.
    let replay_log: Option<Arc<str>> = cli.replay.as_ref().map(|path| {
        std::fs::read_to_string(path)
            .map(Arc::from)
            .unwrap_or_else(|error| {
                eprintln!("{APP_NAME}: cannot read {}: {error}", path.display());
                std::process::exit(2);
            })
    });
    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            // The backend runtime exists before any window and stops when the app quits.
            runtime::init(cx)
                .unwrap_or_else(|error| panic!("failed to start the backend runtime: {error}"));
            images::init(cx);
            badges::Badges::install(cx);
            // Settings (a bad file falls back to defaults and is shown in the window), then
            // the font and theme they ask for.
            let settings_events = config::init(cx);
            let appearance = config::current(cx).appearance.clone();
            fonts::init(&appearance, cx);
            theme::apply_settings(&appearance, None, cx);

            cx.bind_keys(platform::default_bindings());
            cx.set_menus(platform::menus());
            cx.on_action(|_: &Quit, cx| cx.quit());

            // Persist the last window placement and channel list on the way out (⌘Q and last-window close).
            cx.on_app_quit(|cx| {
                shell::flush_state(cx);
                async {}
            })
            .detach();

            // Quit when the last window closes (single-window app for now).
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let (window_bounds, display_id) = initial_placement(cx);
            let options = WindowOptions {
                window_bounds: Some(window_bounds),
                display_id,
                window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
                ..TitleBar::window_options()
            };

            gpui_kit::open_window(options, cx, |window, cx| {
                // Re-apply once the window exists: its appearance is authoritative on Linux.
                let appearance = config::current(cx).appearance.clone();
                theme::apply_settings(&appearance, Some(window), cx);
                // Settings changes reach the UI through the bridge: the watcher thread sends,
                // and the store is updated once per frame on the main thread.
                if let Some(events) = settings_events {
                    bridge::attach(
                        &config::store(cx),
                        window.window_handle(),
                        events,
                        cx,
                        |store, events, _| store.apply_events(events),
                    )
                    .detach();
                }
                let parts = start_chat(&cli, replay_log.clone(), window, cx);
                cx.new(|cx| AppShell::new(window, parts, cx))
            })
            .unwrap_or_else(|error| panic!("failed to open the {APP_NAME} main window: {error}"));
            cx.activate(true);
        });
}
