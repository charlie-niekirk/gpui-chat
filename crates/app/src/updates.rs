//! The update notice (PR 7.2): is a newer release published on GitHub?
//!
//! The check itself (HTTP, once a day, `ETag`, rate limits, semver) is `gpui-chat-update`, which
//! runs on the backend runtime. This module only starts it and shows what it found, as one strip
//! under the cause key `update` (see [`crate::problems`]):
//!
//! * the **automatic** check runs after startup and then daily while `updates.check_automatically`
//!   is on; with the setting off nothing is requested at all. It speaks up only for a newer release
//!   the user has not dismissed; "up to date", "no release yet" and failures stay silent;
//! * **Check for Updates…** (`CheckForUpdates`) asks whatever the setting says and always answers:
//!   a newer release, up to date, no release yet, or why it could not tell.
//!
//! The notice has one button, "View release", which opens the release page in the browser.
//! Nothing is downloaded or installed. Offline runs (`--firehose`, `--replay`, `--bench`) never
//! check on their own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_chat_core::APP_NAME;
use gpui_chat_settings::SettingsPaths;
use gpui_chat_update::{
    CACHE_FILE_NAME, CheckError, Outcome, Release, UpdateChecker, run_automatic,
};
use gpui_kit::App;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::problems::{self, Fix, Problem};
use crate::{config, runtime};

/// The cause key of the strip.
pub const KEY: &str = "update";

/// The first automatic check waits this long after launch: startup has better things to do with
/// the network.
const STARTUP_DELAY: Duration = Duration::from_secs(20);

/// What the check reports to the UI thread.
struct Event {
    /// The user asked ("Check for Updates…"), as opposed to the daily check.
    manual: bool,
    result: Result<Outcome, CheckError>,
}

/// The update check as a GPUI global.
struct Updates {
    checker: Arc<UpdateChecker>,
    sender: UnboundedSender<Event>,
    /// A manual check is running: another click waits for it.
    busy: Arc<AtomicBool>,
    /// The release the strip on screen announces, if it announces one.
    showing: Option<Release>,
}

impl gpui_kit::Global for Updates {}

/// Whether the daily check may ask GitHub. It **fails closed**: the setting must be on *and* the
/// settings file must have loaded. A file that does not load (at startup the defaults are used, and
/// the defaults say "on") might hold `check_automatically = false`, so nothing is asked until the
/// file is fixed.
fn automatic_wanted(store: &config::SettingsStore) -> bool {
    store.error().is_none() && store.settings().updates.check_automatically
}

/// What the strip should do about a result.
#[derive(Debug, PartialEq)]
enum Notice {
    Show {
        problem: Problem,
        /// The release the strip announces (what "View release" opens).
        release: Option<Release>,
    },
    Clear,
}

/// Sets the check up and, unless this is an offline run, starts the daily one. Call once at startup,
/// after the settings and the backend runtime exist.
pub fn init(offline: bool, cx: &mut App) {
    let cache_file = SettingsPaths::from_env()
        .ok()
        .map(|paths| paths.cache_dir().join(CACHE_FILE_NAME));
    let checker = match runtime::enter(cx, || {
        UpdateChecker::new(env!("CARGO_PKG_VERSION"), cache_file)
    }) {
        Ok(checker) => Arc::new(checker),
        Err(error) => {
            tracing::warn!(%error, "the update check is unavailable");
            return;
        }
    };
    let (sender, mut receiver) = unbounded_channel::<Event>();
    cx.set_global(Updates {
        checker: checker.clone(),
        sender: sender.clone(),
        busy: Arc::new(AtomicBool::new(false)),
        showing: None,
    });
    cx.spawn(async move |cx| {
        while let Some(event) = receiver.recv().await {
            cx.update(|cx| apply(event, cx));
        }
    })
    .detach();

    if offline {
        return;
    }
    let enabled = Arc::new(AtomicBool::new(automatic_wanted(
        config::store(cx).read(cx),
    )));
    let wake = Arc::new(Notify::new());
    // Switching the setting on or off wakes the task: on checks at once (if a day is up), off
    // lets it sleep until it is switched on again.
    cx.observe(&config::store(cx), {
        let (enabled, wake) = (enabled.clone(), wake.clone());
        move |store, cx| {
            let on = automatic_wanted(store.read(cx));
            if enabled.swap(on, Ordering::SeqCst) != on {
                wake.notify_one();
            }
        }
    })
    .detach();
    drop(runtime::spawn_backend(cx, async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        run_automatic(
            &checker,
            || enabled.load(Ordering::SeqCst),
            &wake,
            |outcome| {
                sender
                    .send(Event {
                        manual: false,
                        result: Ok(outcome),
                    })
                    .ok();
            },
        )
        .await;
    }));
}

/// "Check for Updates…": asks GitHub now and shows the answer.
pub fn check_now(cx: &mut App) {
    let Some(updates) = cx.try_global::<Updates>() else {
        return;
    };
    if updates.busy.swap(true, Ordering::SeqCst) {
        return;
    }
    let (checker, sender, busy) = (
        updates.checker.clone(),
        updates.sender.clone(),
        updates.busy.clone(),
    );
    problems::report(KEY, Problem::info("Checking for updates…"), cx);
    cx.global_mut::<Updates>().showing = None;
    drop(runtime::spawn_backend(cx, async move {
        let result = checker.check_now().await;
        busy.store(false, Ordering::SeqCst);
        sender
            .send(Event {
                manual: true,
                result,
            })
            .ok();
    }));
}

/// The strip's "View release": opens the release page in the browser.
pub fn open_release(cx: &mut App) {
    let url = cx
        .try_global::<Updates>()
        .and_then(|updates| updates.showing.as_ref())
        .map(|release| release.url.clone());
    if let Some(url) = url {
        cx.open_url(&url);
    }
}

/// The user dismissed the strip: if it announced a release, its automatic notice stays quiet from
/// now on (a newer release brings it back).
pub fn dismissed(cx: &mut App) {
    if let Some(updates) = cx.try_global::<Updates>()
        && let Some(release) = &updates.showing
    {
        updates.checker.dismiss(release);
    }
}

fn apply(event: Event, cx: &mut App) {
    let Some(updates) = cx.try_global::<Updates>() else {
        return;
    };
    let dismissed = match &event.result {
        Ok(Outcome::UpdateAvailable(release)) => updates.checker.is_dismissed(release),
        _ => false,
    };
    let notice = notice(
        &event.result,
        env!("CARGO_PKG_VERSION"),
        event.manual,
        dismissed,
        unix_now(),
    );
    match notice {
        Some(Notice::Show { problem, release }) => {
            cx.global_mut::<Updates>().showing = release;
            problems::report(KEY, problem, cx);
        }
        Some(Notice::Clear) => {
            cx.global_mut::<Updates>().showing = None;
            problems::clear(KEY, cx);
        }
        None => {}
    }
}

/// What to show for `result`, or `None` to leave things as they are. `dismissed` says the user
/// already dismissed the notice for this release; only the daily check honours that.
fn notice(
    result: &Result<Outcome, CheckError>,
    current: &str,
    manual: bool,
    dismissed: bool,
    now: u64,
) -> Option<Notice> {
    match result {
        Ok(Outcome::UpdateAvailable(release)) => {
            if dismissed && !manual {
                return Some(Notice::Clear);
            }
            Some(Notice::Show {
                problem: Problem::info(format!("{APP_NAME} {} is available", release.version))
                    .line(format!(
                        "You have {current}. The release page has the notes and the download; \
                         nothing is installed for you."
                    ))
                    .fix("View release", Fix::ViewRelease)
                    .details(release.url.clone()),
                release: Some(release.clone()),
            })
        }
        Ok(Outcome::UpToDate { latest }) => manual.then(|| Notice::Show {
            problem: Problem::info(format!("{APP_NAME} is up to date")).line(format!(
                "You have {current}; the newest release is {latest}."
            )),
            release: None,
        }),
        Ok(Outcome::NoRelease) => manual.then(|| Notice::Show {
            problem: Problem::info("No release to update to").line(format!(
                "You have {current}; no release has been published yet."
            )),
            release: None,
        }),
        Err(error) => manual.then(|| Notice::Show {
            problem: Problem::warning("Could not check for updates")
                .line(match error {
                    CheckError::RateLimited { until } => {
                        let minutes = until.saturating_sub(now).div_ceil(60).max(1);
                        format!(
                            "GitHub is limiting requests from your network. Try again in about \
                             {minutes} minute{}.",
                            if minutes == 1 { "" } else { "s" }
                        )
                    }
                    CheckError::Network(_) => {
                        "GitHub could not be reached. Check your internet connection and try \
                         again."
                            .to_owned()
                    }
                    _ => "GitHub's answer was not usable. Try again later.".to_owned(),
                })
                .details(error.to_string()),
            release: None,
        }),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_from(text: &str) -> config::SettingsStore {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.toml");
        std::fs::write(&file, text).unwrap();
        config::SettingsStore::new(gpui_chat_settings::Settings::load_or_default(&file))
    }

    #[test]
    fn the_daily_check_follows_the_setting() {
        assert!(automatic_wanted(&store_from("")));
        assert!(automatic_wanted(&store_from(
            "[updates]\ncheck_automatically = true\n"
        )));
        assert!(!automatic_wanted(&store_from(
            "[updates]\ncheck_automatically = false\n"
        )));
    }

    #[test]
    fn a_settings_file_that_does_not_load_switches_the_daily_check_off() {
        // `check_automatically = false` next to an unrelated bad value: the whole file is
        // rejected and the defaults (check on) would apply; the check must stay off.
        let broken = "[updates]\ncheck_automatically = false\n[chat]\nscrollback = \"lots\"\n";
        let store = store_from(broken);
        assert!(store.error().is_some());
        assert!(
            store.settings().updates.check_automatically,
            "the defaults are in effect"
        );
        assert!(!automatic_wanted(&store));
        // Not valid TOML at all, and a value out of range, likewise.
        assert!(!automatic_wanted(&store_from("[updates\n")));
        assert!(!automatic_wanted(&store_from("[chat]\nscrollback = 1\n")));
    }

    #[test]
    fn a_reload_that_fails_switches_it_off_and_a_good_one_back_on() {
        use gpui_chat_settings::{Settings, SettingsEvent};
        let mut store = store_from("");
        assert!(automatic_wanted(&store));
        let error =
            Settings::parse("[chat]\nscrollback = \"x\"\n", std::path::Path::new("x")).unwrap_err();
        store.apply_events([SettingsEvent::Failed(error)]);
        assert!(!automatic_wanted(&store));
        store.apply_events([SettingsEvent::Reloaded(Settings::default())]);
        assert!(automatic_wanted(&store));
    }

    fn release(tag: &str) -> Release {
        gpui_chat_update::release::release_for_tag("charlie-niekirk/gpui-chat", tag).unwrap()
    }

    fn shown(notice: Option<Notice>) -> (Problem, Option<Release>) {
        match notice {
            Some(Notice::Show { problem, release }) => (problem, release),
            other => panic!("expected a strip, got {other:?}"),
        }
    }

    #[test]
    fn a_newer_release_is_announced_with_a_button_that_opens_it() {
        let result = Ok(Outcome::UpdateAvailable(release("v0.2.0")));
        let (problem, announced) = shown(notice(&result, "0.1.0", false, false, 0));
        assert!(problem.title.contains("0.2.0"), "{}", problem.title);
        assert_eq!(problem.fix, Some(("View release", Fix::ViewRelease)));
        assert_eq!(announced.unwrap().tag, "v0.2.0");
        // The link is in the details too, so "Copy details" gives it away.
        assert!(problem.details.unwrap().ends_with("/releases/tag/v0.2.0"));
    }

    #[test]
    fn a_dismissed_release_stays_quiet_for_the_daily_check_but_not_for_the_menu_item() {
        let result = Ok(Outcome::UpdateAvailable(release("v0.2.0")));
        assert_eq!(
            notice(&result, "0.1.0", false, true, 0),
            Some(Notice::Clear)
        );
        assert!(matches!(
            notice(&result, "0.1.0", true, true, 0),
            Some(Notice::Show { .. })
        ));
    }

    #[test]
    fn the_daily_check_is_silent_unless_there_is_an_update() {
        for result in [
            Ok(Outcome::UpToDate {
                latest: gpui_chat_update::Version::new(0, 1, 0),
            }),
            Ok(Outcome::NoRelease),
        ] {
            assert_eq!(notice(&result, "0.1.0", false, false, 0), None);
        }
        let failed = Err(CheckError::Network("down".into()));
        assert_eq!(notice(&failed, "0.1.0", false, false, 0), None);
    }

    #[test]
    fn the_menu_item_always_answers() {
        let up_to_date = Ok(Outcome::UpToDate {
            latest: gpui_chat_update::Version::new(0, 1, 0),
        });
        let (problem, _) = shown(notice(&up_to_date, "0.1.0", true, false, 0));
        assert!(problem.title.contains("up to date"));
        assert!(problem.fix.is_none());
        let (problem, _) = shown(notice(&Ok(Outcome::NoRelease), "0.0.0", true, false, 0));
        assert!(problem.lines[0].contains("no release has been published yet"));
        let (problem, _) = shown(notice(
            &Err(CheckError::Network("refused".into())),
            "0.1.0",
            true,
            false,
            0,
        ));
        assert!(problem.title.contains("Could not check"));
        assert!(problem.lines[0].contains("internet"));
        assert_eq!(
            problem.details.as_deref(),
            Some("could not reach GitHub: refused")
        );
    }

    #[test]
    fn a_rate_limit_says_when_to_try_again() {
        let error = Err(CheckError::RateLimited {
            until: 1_000 + 20 * 60,
        });
        let (problem, _) = shown(notice(&error, "0.1.0", true, false, 1_000));
        assert!(
            problem.lines[0].contains("about 20 minutes"),
            "{:?}",
            problem.lines
        );
    }
}
