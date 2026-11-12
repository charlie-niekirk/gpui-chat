//! The tokio runtime that backends (IRC, Helix, emote and image fetches) run on.
//!
//! GPUI has its own single-threaded foreground executor; network I/O does not belong there.
//! The app starts one multi-thread tokio runtime before the window opens, keeps it in a
//! [`Backend`] global and shuts it down when the app quits. Backend code is started with
//! [`spawn_backend`] and talks back to the UI through [`crate::bridge`].

use std::future::Future;
use std::io;
use std::time::Duration;

use gpui_chat_core::APP_NAME;
use gpui_kit::{App, Global};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinHandle;

/// How long quit waits for running backend tasks before abandoning them.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Owns the runtime. Lives as a GPUI global for the lifetime of the app.
pub struct Backend {
    /// `None` once [`Backend::shutdown`] has run.
    runtime: Option<Runtime>,
    /// Stays usable after shutdown; tasks spawned through it are then dropped unrun.
    #[allow(dead_code)] // read by `spawn`, first used by the backends of PR 1.2 / 1.3
    handle: Handle,
}

impl Global for Backend {}

impl Backend {
    /// Starts a multi-thread runtime whose worker threads are named `<APP_NAME>-backend`.
    pub fn start() -> io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .thread_name(format!("{APP_NAME}-backend"))
            .build()?;
        let handle = runtime.handle().clone();
        Ok(Self {
            runtime: Some(runtime),
            handle,
        })
    }

    /// Runs `future` on the runtime. After shutdown the future is dropped without running and
    /// the returned handle reports a cancelled task.
    #[allow(dead_code)] // first used by the backends of PR 1.2 / 1.3
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }

    #[allow(dead_code)] // used by tests; handy for backends deciding whether to start work
    pub fn is_running(&self) -> bool {
        self.runtime.is_some()
    }

    /// Stops the runtime: running tasks are cancelled at their next `.await`, and blocking
    /// work gets [`SHUTDOWN_GRACE`] to finish. Idempotent. Must not be called from inside the
    /// runtime (the app calls it from the main thread on quit).
    pub fn shutdown(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(SHUTDOWN_GRACE);
        }
    }
}

/// Starts the runtime, stores it as a global and arranges for it to stop on quit.
pub fn init(cx: &mut App) -> io::Result<()> {
    cx.set_global(Backend::start()?);
    cx.on_app_quit(|cx| {
        cx.global_mut::<Backend>().shutdown();
        async {}
    })
    .detach();
    Ok(())
}

/// Runs async backend work on the app's tokio runtime. The future may use tokio I/O and
/// timers; send results to the UI with a [`crate::bridge::BridgeSender`], never by touching
/// GPUI state from the task.
#[allow(dead_code)] // first used by the backends of PR 1.2 / 1.3
pub fn spawn_backend<F>(cx: &App, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    cx.global::<Backend>().spawn(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_tasks_on_worker_threads_and_stops() {
        let mut backend = Backend::start().expect("runtime");
        assert!(backend.is_running());
        let main_thread = std::thread::current().id();
        let (tx, rx) = std::sync::mpsc::channel();
        let join = backend.spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            tx.send(std::thread::current().id()).unwrap();
            7
        });
        let worker = rx.recv_timeout(Duration::from_secs(5)).expect("task ran");
        assert_ne!(worker, main_thread);

        backend.shutdown();
        assert!(!backend.is_running());
        backend.shutdown(); // idempotent
        drop(join);
    }

    #[test]
    fn spawn_after_shutdown_does_not_run_or_panic() {
        let mut backend = Backend::start().expect("runtime");
        backend.shutdown();
        let (tx, rx) = std::sync::mpsc::channel();
        drop(backend.spawn(async move {
            let _ = tx.send(());
        }));
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn shutdown_cancels_pending_tasks() {
        let mut backend = Backend::start().expect("runtime");
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        // The task owns `tx` and never finishes; cancelling it drops `tx`, which unblocks `rx`.
        drop(backend.spawn(async move {
            let _keep = tx;
            std::future::pending::<()>().await;
        }));
        backend.shutdown();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
