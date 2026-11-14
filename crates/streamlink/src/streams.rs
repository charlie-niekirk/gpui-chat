//! Spawning and tracking streamlink children.
//!
//! One session per channel. [`Streams::launch`] returns at once; a worker thread resolves the
//! binary (which may run a login shell), spawns streamlink, reads its stderr and waits for it,
//! reporting [`StreamEvent`]s to the sink the caller gave [`Streams::new`]. The sink runs on
//! worker threads, so it must only hand the event over (the app pushes it into its bridge).
//!
//! On unix streamlink is started in its own process group, so stopping signals the group: the
//! player streamlink started goes down with it. Stop is `SIGTERM`, then `SIGKILL` after
//! [`STOP_GRACE`]. No `unsafe` is needed: signals are sent with the system `kill` command.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::args::{LaunchSpec, build_args, valid_quality};
use crate::classify::{StreamError, classify_line};
use crate::resolve::{Environment, ResolveError, SystemEnvironment, resolve};

/// How long a stopped stream gets to exit on `SIGTERM` before it is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(3);
/// After streamlink exits, how long to wait for the rest of its stderr (a player that inherited
/// the pipe can hold it open) before reporting the exit.
const STDERR_DRAIN: Duration = Duration::from_millis(500);

/// Identifies one launch; a restart of the same channel is a new session, so late events of the
/// old one can be told apart.
pub type SessionId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    pub channel: String,
    pub session: SessionId,
    pub kind: StreamEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEventKind {
    /// streamlink was spawned.
    Started,
    /// Something went wrong. May be followed by more of these, then `Exited`.
    Failed(StreamError),
    /// Always the last event of a session. `stopped`: we asked for it ([`Streams::stop`]).
    Exited { stopped: bool },
}

/// Where events go. Called from worker threads.
pub type EventSink = Arc<dyn Fn(StreamEvent) + Send + Sync>;

struct Shared {
    env: Arc<dyn Environment + Send + Sync>,
    sink: EventSink,
    /// The last successful resolution and the override it answered, so a click does not run the
    /// login shell again. Failures are not cached: installing streamlink works without restart.
    resolved: Mutex<Option<(Option<PathBuf>, PathBuf)>>,
    /// Live sessions by channel; a worker removes its own entry when it finishes.
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Shared {
    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
struct SessionState {
    pid: Option<u32>,
    stop_requested: bool,
    done: bool,
}

struct Session {
    id: SessionId,
    state: Mutex<SessionState>,
    changed: Condvar,
}

impl Session {
    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks the session stopping and sends `SIGTERM`. `true` if it was still alive.
    fn request_stop(&self) -> bool {
        let mut state = self.lock();
        if state.done {
            return false;
        }
        state.stop_requested = true;
        if let Some(pid) = state.pid {
            signal_group(pid, Signal::Term);
        }
        true
    }

    /// Waits until done or `deadline`; `true` if done.
    fn wait_done(&self, deadline: Instant) -> bool {
        let mut state = self.lock();
        while !state.done {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    /// `SIGKILL` unless it finished meanwhile (checked under the lock, so a recycled pid is
    /// never signalled).
    fn force_kill(&self) {
        let state = self.lock();
        if let (false, Some(pid)) = (state.done, state.pid) {
            signal_group(pid, Signal::Kill);
        }
    }
}

/// The streams of an app: at most one per channel.
pub struct Streams {
    shared: Arc<Shared>,
    next_id: AtomicU64,
}

impl Streams {
    /// Streams on the real machine.
    pub fn new(sink: EventSink) -> Self {
        Self::with_environment(Arc::new(SystemEnvironment), sink)
    }

    /// Streams with an injected [`Environment`] (tests).
    pub fn with_environment(env: Arc<dyn Environment + Send + Sync>, sink: EventSink) -> Self {
        Self {
            shared: Arc::new(Shared {
                env,
                sink,
                resolved: Mutex::new(None),
                sessions: Mutex::new(HashMap::new()),
            }),
            next_id: AtomicU64::new(1),
        }
    }

    /// Starts `spec`, first stopping this channel's current stream if it has one. Returns
    /// immediately; progress arrives as events.
    pub fn launch(&self, spec: LaunchSpec) -> SessionId {
        self.stop(&spec.channel);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session = Arc::new(Session {
            id,
            state: Mutex::new(SessionState::default()),
            changed: Condvar::new(),
        });
        let channel = spec.channel.clone();
        self.shared
            .sessions()
            .insert(channel.clone(), session.clone());
        let shared = self.shared.clone();
        let worker_session = session.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("streamlink-{channel}"))
            .spawn(move || run(&shared, &worker_session, spec));
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the streamlink worker thread");
            forget(&self.shared, &channel, &session);
            let message = StreamError::Spawn(error.to_string());
            for kind in [
                StreamEventKind::Failed(message),
                StreamEventKind::Exited { stopped: false },
            ] {
                emit(&self.shared, &channel, &session, kind);
            }
        }
        id
    }

    /// Asks the channel's stream to end (`SIGTERM`, then a kill after [`STOP_GRACE`]). Does not
    /// wait; the session's `Exited { stopped: true }` follows. No-op if nothing is running.
    pub fn stop(&self, channel: &str) {
        let Some(session) = self.shared.sessions().get(channel).cloned() else {
            return;
        };
        if session.request_stop() {
            std::thread::spawn(move || {
                if !session.wait_done(Instant::now() + STOP_GRACE) {
                    session.force_kill();
                }
            });
        }
    }

    /// Whether the channel has a stream that has not exited (starting counts).
    pub fn is_running(&self, channel: &str) -> bool {
        self.shared.sessions().contains_key(channel)
    }

    /// Channels with a live stream.
    pub fn running(&self) -> Vec<String> {
        self.shared.sessions().keys().cloned().collect()
    }

    /// Ends every stream and waits up to `timeout` for them (then kills what is left). For app
    /// quit; blocking.
    pub fn stop_all(&self, timeout: Duration) {
        let sessions: Vec<Arc<Session>> = self.shared.sessions().values().cloned().collect();
        let deadline = Instant::now() + timeout;
        let alive: Vec<_> = sessions.iter().filter(|s| s.request_stop()).collect();
        for session in alive {
            if !session.wait_done(deadline) {
                session.force_kill();
            }
        }
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        self.stop_all(Duration::from_secs(1));
    }
}

/// Drops the channel's entry if it still belongs to `session` (a restart may have replaced it).
fn forget(shared: &Shared, channel: &str, session: &Arc<Session>) {
    let mut sessions = shared.sessions();
    if sessions
        .get(channel)
        .is_some_and(|s| Arc::ptr_eq(s, session))
    {
        sessions.remove(channel);
    }
}

fn emit(shared: &Shared, channel: &str, session: &Session, kind: StreamEventKind) {
    (shared.sink)(StreamEvent {
        channel: channel.to_owned(),
        session: session.id,
        kind,
    });
}

fn resolve_cached(shared: &Shared, spec: &LaunchSpec) -> Result<PathBuf, ResolveError> {
    let mut cache = shared.resolved.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((key, path)) = cache.as_ref()
        && *key == spec.binary_override
        && shared.env.is_executable(path)
    {
        return Ok(path.clone());
    }
    let path = resolve(spec.binary_override.as_deref(), shared.env.as_ref())?;
    *cache = Some((spec.binary_override.clone(), path.clone()));
    Ok(path)
}

/// The worker: everything from resolving to the last event.
fn run(shared: &Arc<Shared>, session: &Arc<Session>, spec: LaunchSpec) {
    let channel = spec.channel.as_str();
    let finish = |stopped: bool| {
        let mut state = session.lock();
        state.done = true;
        session.changed.notify_all();
        drop(state);
        forget(shared, channel, session);
        emit(
            shared,
            channel,
            session,
            StreamEventKind::Exited { stopped },
        );
    };
    let fail = |error: StreamError| {
        emit(shared, channel, session, StreamEventKind::Failed(error));
        finish(false);
    };

    if !valid_quality(&spec.quality) {
        return fail(StreamError::Other(format!(
            "invalid quality {:?}",
            spec.quality
        )));
    }
    let binary = match resolve_cached(shared, &spec) {
        Ok(binary) => binary,
        Err(ResolveError::NotFound) => return fail(StreamError::NotFound { configured: None }),
        Err(ResolveError::OverrideInvalid(path)) => {
            return fail(StreamError::NotFound {
                configured: Some(path.display().to_string()),
            });
        }
    };
    if session.lock().stop_requested {
        return finish(true);
    }

    let mut command = Command::new(&binary);
    command
        .args(build_args(&spec))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let error = if error.kind() == std::io::ErrorKind::NotFound {
                StreamError::NotFound {
                    configured: spec
                        .binary_override
                        .as_ref()
                        .map(|path| path.display().to_string()),
                }
            } else {
                StreamError::Spawn(error.to_string())
            };
            return fail(error);
        }
    };
    {
        let mut state = session.lock();
        state.pid = Some(child.id());
        if state.stop_requested {
            // Stopped while resolving or spawning.
            signal_group(child.id(), Signal::Term);
        }
    }
    emit(shared, channel, session, StreamEventKind::Started);

    let saw_error = Arc::new(AtomicBool::new(false));
    let (drained_tx, drained_rx) = mpsc::channel::<()>();
    if let Some(stderr) = child.stderr.take() {
        let (shared, session, channel, saw_error) = (
            shared.clone(),
            session.clone(),
            channel.to_owned(),
            saw_error.clone(),
        );
        std::thread::spawn(move || {
            let mut last: Option<StreamError> = None;
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                tracing::debug!(channel = %channel, line = %line, "streamlink");
                if let Some(error) = classify_line(&line) {
                    saw_error.store(true, Ordering::Release);
                    if last.as_ref() != Some(&error) {
                        last = Some(error.clone());
                        // A stop we asked for is not an error worth showing.
                        if !session.lock().stop_requested {
                            emit(&shared, &channel, &session, StreamEventKind::Failed(error));
                        }
                    }
                }
            }
            let _ = drained_tx.send(());
        });
    } else {
        drop(drained_tx);
    }

    let status = child.wait();
    let _ = drained_rx.recv_timeout(STDERR_DRAIN);
    let stopped = session.lock().stop_requested;
    if !stopped && !saw_error.load(Ordering::Acquire) {
        match &status {
            Ok(status) if !status.success() => emit(
                shared,
                channel,
                session,
                StreamEventKind::Failed(StreamError::Other(format!(
                    "streamlink exited with {status}"
                ))),
            ),
            Err(error) => emit(
                shared,
                channel,
                session,
                StreamEventKind::Failed(StreamError::Other(format!(
                    "waiting for streamlink failed: {error}"
                ))),
            ),
            Ok(_) => {}
        }
    }
    finish(stopped);
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Signals a whole process group (streamlink and the player it started).
#[cfg(unix)]
fn signal_group(pid: u32, signal: Signal) {
    let name = match signal {
        Signal::Term => "TERM",
        Signal::Kill => "KILL",
    };
    let result = Command::new("kill")
        .args(["-s", name, "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !matches!(result, Ok(status) if status.success()) {
        tracing::debug!(
            pid,
            signal = name,
            "kill of streamlink's process group failed"
        );
    }
}

#[cfg(not(unix))]
fn signal_group(pid: u32, _signal: Signal) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::sync::mpsc::Receiver;

    use super::*;

    /// Writing then executing scripts from many test threads races on `ETXTBSY` (a forked child
    /// inherits the still-open write handle), so the tests that spawn take turns.
    static SERIAL: Mutex<()> = Mutex::new(());
    const WAIT: Duration = Duration::from_secs(10);

    struct Rig {
        _guard: MutexGuard<'static, ()>,
        dir: tempfile::TempDir,
        streams: Streams,
        events: Receiver<StreamEvent>,
    }

    fn rig() -> Rig {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, events) = mpsc::channel();
        let sink: EventSink = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        Rig {
            _guard: guard,
            dir: tempfile::tempdir().unwrap(),
            streams: Streams::new(sink),
            events,
        }
    }

    impl Rig {
        /// Writes an executable fake `streamlink` and returns a spec that runs it.
        fn script(&self, body: &str) -> LaunchSpec {
            let path = self.dir.path().join("streamlink");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut spec = LaunchSpec::new("forsen", "720p");
            spec.binary_override = Some(path);
            spec
        }

        fn next(&self) -> StreamEvent {
            self.events.recv_timeout(WAIT).expect("an event")
        }

        /// Events up to and including `Exited`.
        fn until_exit(&self) -> Vec<StreamEventKind> {
            let mut kinds = Vec::new();
            loop {
                let kind = self.next().kind;
                let last = matches!(kind, StreamEventKind::Exited { .. });
                kinds.push(kind);
                if last {
                    return kinds;
                }
            }
        }

        fn file(&self, name: &str) -> String {
            std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
        }
    }

    fn pid_alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + WAIT;
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn passes_the_built_arguments_and_reports_a_clean_exit() {
        let rig = rig();
        let dir = rig.dir.path().display();
        let mut spec = rig.script(&format!("echo \"$@\" > {dir}/args"));
        spec.player = Some("mpv".into());
        spec.extra_args = vec!["--twitch-low-latency".into()];
        let id = rig.streams.launch(spec);
        let events = rig.until_exit();
        assert_eq!(
            events,
            [
                StreamEventKind::Started,
                StreamEventKind::Exited { stopped: false }
            ]
        );
        assert_eq!(
            rig.file("args").trim(),
            "--player mpv --twitch-low-latency twitch.tv/forsen 720p"
        );
        assert!(!rig.streams.is_running("forsen"));
        assert_eq!(id, 1);
    }

    #[test]
    fn an_offline_channel_is_reported_as_such() {
        let rig = rig();
        let spec = rig.script(
            "echo 'error: No playable streams found on this URL: https://twitch.tv/forsen' >&2\nexit 1",
        );
        rig.streams.launch(spec);
        assert_eq!(
            rig.until_exit(),
            [
                StreamEventKind::Started,
                StreamEventKind::Failed(StreamError::ChannelOffline),
                StreamEventKind::Exited { stopped: false }
            ]
        );
    }

    #[test]
    fn a_missing_player_is_reported_as_such() {
        let rig = rig();
        let spec = rig.script(
            "echo 'error: The default player (VLC) does not seem to be installed.' >&2\nexit 1",
        );
        rig.streams.launch(spec);
        let events = rig.until_exit();
        assert!(matches!(
            events[1],
            StreamEventKind::Failed(StreamError::NoPlayer(_))
        ));
    }

    #[test]
    fn a_silent_failure_still_says_something() {
        let rig = rig();
        let spec = rig.script("exit 3");
        rig.streams.launch(spec);
        let events = rig.until_exit();
        assert!(matches!(
            &events[1],
            StreamEventKind::Failed(StreamError::Other(message)) if message.contains("exit")
        ));
    }

    #[test]
    fn a_missing_binary_is_not_found_and_never_started() {
        let rig = rig();
        let mut spec = LaunchSpec::new("forsen", "best");
        spec.binary_override = Some(rig.dir.path().join("nope"));
        rig.streams.launch(spec);
        let events = rig.until_exit();
        assert!(matches!(
            &events[0],
            StreamEventKind::Failed(StreamError::NotFound {
                configured: Some(_)
            })
        ));
        assert_eq!(events.len(), 2);
        assert!(!rig.streams.is_running("forsen"));
    }

    #[test]
    fn a_bad_quality_is_rejected_before_spawning() {
        let rig = rig();
        let mut spec = rig.script("exit 0");
        spec.quality = "--player=evil".into();
        rig.streams.launch(spec);
        let events = rig.until_exit();
        assert!(matches!(
            &events[0],
            StreamEventKind::Failed(StreamError::Other(_))
        ));
    }

    #[test]
    fn stop_ends_streamlink_and_the_player_it_started() {
        let rig = rig();
        let dir = rig.dir.path().display();
        // A stand-in for the player: a child of the script that outlives a plain kill of it.
        let spec = rig.script(&format!(
            "sleep 60 &\necho $! > {dir}/player.pid\necho $$ > {dir}/self.pid\nwait"
        ));
        rig.streams.launch(spec);
        assert_eq!(rig.next().kind, StreamEventKind::Started);
        wait_for(&rig.dir.path().join("player.pid"));
        wait_for(&rig.dir.path().join("self.pid"));
        std::thread::sleep(Duration::from_millis(100));
        assert!(rig.streams.is_running("forsen"));
        rig.streams.stop("forsen");
        assert_eq!(rig.next().kind, StreamEventKind::Exited { stopped: true });
        let (player, me) = (rig.file("player.pid"), rig.file("self.pid"));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!pid_alive(&me), "streamlink survived stop");
        assert!(!pid_alive(&player), "the player survived stop");
        assert!(!rig.streams.is_running("forsen"));
    }

    #[test]
    fn stop_escalates_to_kill_when_term_is_ignored() {
        let rig = rig();
        let dir = rig.dir.path().display();
        let spec = rig.script(&format!(
            "trap '' TERM\necho ready > {dir}/ready\nwhile true; do sleep 1; done"
        ));
        rig.streams.launch(spec);
        assert_eq!(rig.next().kind, StreamEventKind::Started);
        wait_for(&rig.dir.path().join("ready"));
        let began = Instant::now();
        rig.streams.stop("forsen");
        assert_eq!(rig.next().kind, StreamEventKind::Exited { stopped: true });
        assert!(began.elapsed() >= STOP_GRACE - Duration::from_millis(200));
    }

    #[test]
    fn stop_all_kills_every_child_and_waits() {
        let rig = rig();
        let dir = rig.dir.path().display();
        let mut first = rig.script(&format!("echo $$ >> {dir}/pids\nsleep 60"));
        first.channel = "a".into();
        let mut second = first.clone();
        second.channel = "b".into();
        rig.streams.launch(first);
        rig.streams.launch(second);
        assert_eq!(rig.streams.running().len(), 2);
        let deadline = Instant::now() + WAIT;
        while rig.file("pids").lines().count() < 2 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        rig.streams.stop_all(Duration::from_secs(5));
        for pid in rig.file("pids").lines() {
            assert!(!pid_alive(pid), "pid {pid} survived stop_all");
        }
    }

    #[test]
    fn relaunching_replaces_the_session() {
        let rig = rig();
        let spec = rig.script("sleep 60");
        let first = rig.streams.launch(spec.clone());
        let second = rig.streams.launch(spec);
        assert_ne!(first, second);
        let mut old_stopped = false;
        let mut new_started = false;
        while !(old_stopped && new_started) {
            let event = rig.next();
            match (event.session, event.kind) {
                (id, StreamEventKind::Exited { stopped: true }) if id == first => {
                    old_stopped = true;
                }
                (id, StreamEventKind::Started) if id == second => new_started = true,
                _ => {}
            }
        }
        assert!(rig.streams.is_running("forsen"));
        rig.streams.stop_all(Duration::from_secs(5));
    }

    #[test]
    fn stopping_something_that_is_not_running_is_a_no_op() {
        let rig = rig();
        rig.streams.stop("nobody");
        assert!(rig.streams.running().is_empty());
        assert!(rig.events.recv_timeout(Duration::from_millis(50)).is_err());
    }
}
