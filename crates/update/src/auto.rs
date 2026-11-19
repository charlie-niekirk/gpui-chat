//! The automatic check as a long-running task: ask when due, report, sleep, repeat.

use std::time::Duration;

use tokio::sync::Notify;

use crate::checker::{Outcome, UpdateChecker};

/// The longest one sleep lasts. A tokio sleep does not advance while the machine is asleep, so a
/// day-long one could overshoot by the whole night; waking hourly lets the checker, which looks
/// at the wall clock, decide whether a check is due.
const MAX_SLEEP: Duration = Duration::from_secs(60 * 60);

/// Runs the automatic check for as long as the task lives.
///
/// While `enabled()` is false **no request is made at all**: the task waits on `wake` and nothing
/// else. Call `wake.notify_one()` when the setting changes; switching it on checks at once (if the
/// last answer is a day old), switching it off lets the pending sleep run out unused. Every time it
/// is due the task asks GitHub through [`UpdateChecker::check_if_due`] and hands the [`Outcome`] to
/// `report` (when it differs from the last one reported); failures are logged and retried by the checker's own pause.
pub async fn run_automatic(
    checker: &UpdateChecker,
    enabled: impl Fn() -> bool,
    wake: &Notify,
    mut report: impl FnMut(Outcome),
) {
    // Only a changed answer is reported: the hourly wake-ups must not repeat it.
    let mut last: Option<Outcome> = None;
    loop {
        if !enabled() {
            wake.notified().await;
            continue;
        }
        match checker.check_if_due().await {
            Ok(outcome) => {
                if last.as_ref() != Some(&outcome) {
                    last = Some(outcome.clone());
                    report(outcome);
                }
            }
            Err(error) => tracing::debug!(%error, "automatic update check gave no answer"),
        }
        let wait: Duration = checker.next_check_in().min(MAX_SLEEP);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = wake.notified() => {}
        }
    }
}
