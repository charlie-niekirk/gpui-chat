//! Signalling process groups (shared by the launcher and the login-shell probe).
//!
//! No `unsafe` (the crate forbids it): signals are sent with the system `kill`, by absolute path
//! so a `PATH` that a GUI app does not control cannot change what runs.

use std::process::{Command, Stdio};

#[derive(Clone, Copy)]
pub(crate) enum Signal {
    Term,
    Kill,
}

/// Signals a whole process group whose leader is `pid` (streamlink and the player it started).
#[cfg(unix)]
pub(crate) fn signal_group(pid: u32, signal: Signal) {
    let name = match signal {
        Signal::Term => "TERM",
        Signal::Kill => "KILL",
    };
    let result = Command::new("/bin/kill")
        .args(["-s", name, "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !matches!(result, Ok(status) if status.success()) {
        tracing::debug!(pid, signal = name, "kill of a process group failed");
    }
}

#[cfg(not(unix))]
pub(crate) fn signal_group(pid: u32, _signal: Signal) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Writing a script and then executing it races on `ETXTBSY` with any other test that spawns a
/// process (a forked child inherits the still-open write handle), so every test that spawns takes
/// turns on this lock.
#[cfg(all(test, unix))]
pub(crate) static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
