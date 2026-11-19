//! The automatic check as a long-running task: ask when due, report, sleep, repeat.

use std::time::Duration;

use tokio::sync::Notify;

use crate::checker::{Outcome, UpdateChecker};

/// Runs the automatic check for as long as the task lives.
///
/// While `enabled()` is false **no request is made at all**: the task waits on `wake` and nothing
/// else. Call `wake.notify_one()` when the setting changes; switching it on checks at once (if the
/// last answer is a day old), switching it off lets the pending sleep run out unused. Every time it
/// is due the task asks GitHub through [`UpdateChecker::check_if_due`] and hands the [`Outcome`] to
/// `report`; failures are logged and retried by the checker's own pause.
pub async fn run_automatic(
    checker: &UpdateChecker,
    enabled: impl Fn() -> bool,
    wake: &Notify,
    mut report: impl FnMut(Outcome),
) {
    loop {
        if !enabled() {
            wake.notified().await;
            continue;
        }
        match checker.check_if_due().await {
            Ok(outcome) => report(outcome),
            Err(error) => tracing::debug!(%error, "automatic update check gave no answer"),
        }
        let wait: Duration = checker.next_check_in();
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = wake.notified() => {}
        }
    }
}
