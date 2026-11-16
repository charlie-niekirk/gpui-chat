//! A reconnecting websocket session: connect, run the protocol's [`Session`], and on any failure
//! wait (exponential backoff with jitter) and connect again, resubscribing from scratch.
//!
//! The runner owns the connection only while the session [`wants a
//! connection`](Session::wants_connection) (it has subscriptions): with none, the socket is closed
//! and the task just waits for the next change, so a channel-less app holds no connections.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{sleep_until, timeout};
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// A change to what the session listens to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change<S> {
    Subscribe(S),
    Unsubscribe(S),
    /// Drop every subscription (the provider was switched off).
    Clear,
}

/// What a frame makes the runner do.
#[derive(Debug, Default)]
pub struct Reaction {
    /// Text frames to send.
    pub send: Vec<String>,
    /// Drop the connection and reconnect (the server asked, or said it is going away).
    pub reconnect: bool,
    /// With `reconnect`: do not reconnect before this has passed (the server refused us; asking
    /// again at once would only be refused again).
    pub hold_off: Duration,
}

/// One provider's protocol and state, driven by [`run`].
pub trait Session: Send + 'static {
    type Sub: Send + 'static;

    /// Whether a connection is worth having (there is something to listen to).
    fn wants_connection(&self) -> bool;
    /// Frames to send as soon as the connection is open (it is a fresh one: nothing is
    /// subscribed on it yet).
    fn on_open(&mut self) -> Vec<String>;
    /// A text frame arrived.
    fn on_text(&mut self, text: &str, now: Instant) -> Reaction;
    /// Applies a change; returns the frames that make it so on a live connection (ignored when
    /// there is none: [`Session::on_open`] sends everything).
    fn apply(&mut self, change: Change<Self::Sub>) -> Vec<String>;
    /// Reconnect if nothing at all arrived for this long.
    fn idle_limit(&self) -> Option<Duration>;
    /// Send a websocket ping this often (the server has no heartbeat of its own).
    fn ping_interval(&self) -> Option<Duration>;
    /// The next time [`Session::on_tick`] wants to run (flushing coalesced notices).
    fn next_tick(&self) -> Option<Instant>;
    fn on_tick(&mut self, now: Instant);
}

/// Reconnect timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    /// A connection that stayed up this long counts as healthy: the next wait starts over.
    pub healthy_after: Duration,
    pub connect_timeout: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            healthy_after: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

impl Backoff {
    /// The wait after `failures` consecutive failures (1-based): doubles from [`Self::initial`]
    /// up to [`Self::max`]; `jitter` in `0.0..1.0` spreads it over 75–100 %.
    pub fn delay(&self, failures: u32, jitter: f64) -> Duration {
        let exponent = failures.saturating_sub(1).min(16);
        let base = self.initial.saturating_mul(1 << exponent).min(self.max);
        base.mul_f64(0.75 + 0.25 * jitter.clamp(0.0, 1.0))
    }
}

/// Jitter from the clock: no random-number dependency for a reconnect delay.
fn jitter() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    f64::from(nanos % 1000) / 1000.0
}

enum Served {
    /// The connection dropped (or was dropped on purpose): connect again, but not before
    /// `hold_off` (the server told us to go away).
    Lost { hold_off: Duration },
    /// No subscriptions left: wait for one.
    Idle,
    /// The command channel closed: the owner is gone.
    Shutdown,
}

const LOST: Served = Served::Lost {
    hold_off: Duration::ZERO,
};

/// The largest websocket message accepted (provider frames are a few KB; this is a guard against
/// a hostile or broken server).
pub const MAX_MESSAGE: usize = 1024 * 1024;

fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

/// Waits until `until` (or forever when `None`) while applying subscription changes and flushing
/// the session's due notices. `false`: the command channel closed.
async fn wait<S: Session>(
    session: &mut S,
    commands: &mut mpsc::UnboundedReceiver<Change<S::Sub>>,
    until: Option<tokio::time::Instant>,
    stop_when_wanted: bool,
) -> bool {
    let never = tokio::time::Instant::now() + Duration::from_secs(86_400);
    loop {
        if stop_when_wanted && session.wants_connection() {
            return true;
        }
        let tick_at = session.next_tick().map(tokio::time::Instant::from_std);
        tokio::select! {
            () = sleep_until(until.unwrap_or(never)), if until.is_some() => return true,
            () = sleep_until(tick_at.unwrap_or(never)), if tick_at.is_some() => {
                session.on_tick(Instant::now());
            }
            change = commands.recv() => match change {
                Some(change) => { session.apply(change); }
                None => return false,
            },
        }
    }
}

/// Runs `session` against `url` until the command channel closes.
pub async fn run<S: Session>(
    name: &'static str,
    url: String,
    mut session: S,
    mut commands: mpsc::UnboundedReceiver<Change<S::Sub>>,
    backoff: Backoff,
) {
    let mut failures = 0u32;
    loop {
        // Nothing to listen to: no connection (notices still flush).
        if !session.wants_connection() && !wait(&mut session, &mut commands, None, true).await {
            return;
        }
        let started = Instant::now();
        let connecting = connect_async_with_config(url.as_str(), Some(config()), false);
        let served = match timeout(backoff.connect_timeout, connecting).await {
            Ok(Ok((socket, _))) => {
                tracing::debug!(name, "live emote socket connected");
                serve(name, socket, &mut session, &mut commands).await
            }
            Ok(Err(error)) => {
                tracing::debug!(name, %error, "live emote socket: connect failed");
                LOST
            }
            Err(_) => {
                tracing::debug!(name, "live emote socket: connect timed out");
                LOST
            }
        };
        match served {
            Served::Shutdown => return,
            Served::Idle => failures = 0,
            Served::Lost { hold_off } => {
                // A connection is healthy only once it has stayed up for a while: the server
                // greeting us and then ending the stream does not make it so.
                if started.elapsed() >= backoff.healthy_after {
                    failures = 0;
                }
                failures = failures.saturating_add(1);
                let wait_for = backoff.delay(failures, jitter()).max(hold_off);
                tracing::debug!(name, ?wait_for, failures, "live emote socket: reconnecting");
                let until = tokio::time::Instant::now() + wait_for;
                if !wait(&mut session, &mut commands, Some(until), false).await {
                    return;
                }
            }
        }
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn serve<S: Session>(
    name: &'static str,
    socket: Socket,
    session: &mut S,
    commands: &mut mpsc::UnboundedReceiver<Change<S::Sub>>,
) -> Served {
    let (mut write, mut read) = socket.split();
    macro_rules! send_all {
        ($frames:expr) => {
            for frame in $frames {
                if write.send(Message::text(frame)).await.is_err() {
                    return LOST;
                }
            }
        };
    }
    send_all!(session.on_open());
    let mut last_frame = tokio::time::Instant::now();
    let mut next_ping = session
        .ping_interval()
        .map(|every| tokio::time::Instant::now() + every);
    loop {
        let idle_at = session.idle_limit().map(|limit| last_frame + limit);
        let tick_at = session.next_tick().map(tokio::time::Instant::from_std);
        // A far-away instant stands in for "never": the branches are guarded by `if`.
        let never = tokio::time::Instant::now() + Duration::from_secs(86_400);
        tokio::select! {
            message = read.next() => {
                last_frame = tokio::time::Instant::now();
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let reaction = session.on_text(text.as_str(), Instant::now());
                        send_all!(reaction.send);
                        if reaction.reconnect {
                            return Served::Lost { hold_off: reaction.hold_off };
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if write.send(Message::Pong(payload)).await.is_err() {
                            return LOST;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return LOST,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::debug!(name, %error, "live emote socket: read failed");
                        return LOST;
                    }
                }
            }
            change = commands.recv() => {
                let Some(change) = change else {
                    let _ = write.send(Message::Close(None)).await;
                    return Served::Shutdown;
                };
                let frames = session.apply(change);
                send_all!(frames);
                if !session.wants_connection() {
                    let _ = write.send(Message::Close(None)).await;
                    return Served::Idle;
                }
            }
            () = sleep_until(tick_at.unwrap_or(never)), if tick_at.is_some() => {
                session.on_tick(Instant::now());
            }
            () = sleep_until(idle_at.unwrap_or(never)), if idle_at.is_some() => {
                tracing::debug!(name, "live emote socket: silent for too long");
                return LOST;
            }
            () = sleep_until(next_ping.unwrap_or(never)), if next_ping.is_some() => {
                next_ping = session.ping_interval().map(|every| tokio::time::Instant::now() + every);
                if write.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return LOST;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
