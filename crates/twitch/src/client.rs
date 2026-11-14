//! The live connection: connect, log in, join, keep alive, reconnect.
//!
//! `twitch-irc` supplies the TLS transport and the IRC types. Its pooled
//! `TwitchIRCClient` is not used: it neither reports connection state nor backs off
//! between reconnects, and has no JOIN pacing, all of which the UI needs. One
//! supervisor task owns one connection instead.
//!
//! # Events
//!
//! Everything leaves through one bounded [`mpsc::Receiver<ChatEvent>`]. If the UI falls
//! behind, events wait in an outbox of the same size while the connection keeps being
//! serviced (PINGs answered, join/part/shutdown handled); only if the UI stays behind
//! past both buffers are chat *messages* dropped (warned in the log), never state
//! changes. Dropping the receiver shuts the client down at once.
//!
//! * `Connection { channel: None, .. }` describes the whole connection and applies to
//!   every channel: `Connecting`, `Connected` (login accepted), `Reconnecting`,
//!   `Disconnected` (only on shutdown).
//! * `Connection { channel: Some(c), .. }` is the join state of one channel:
//!   `Connecting` when its JOIN is sent, `Connected` when the server confirms it,
//!   `Disconnected` after [`TwitchClient::part`].
//!
//! After a reconnect every wanted channel is joined again (paced by the join limiter)
//! and goes through `Connecting` → `Connected` once more.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use gpui_chat_core::{ChannelId, ChatEvent, ConnectionState, Timestamp};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout};
use twitch_irc::SecureTCPTransport;
use twitch_irc::irc;
use twitch_irc::message::{AsRawIRC, IRCMessage, IRCPrefix};
use twitch_irc::transport::Transport;

use crate::auth::{Auth, AuthState};
use crate::limits::{Backoff, JoinLimiter, JoinRateLimit};
use crate::parse::{ParseOptions, map_message};

/// The scope that lets the IRC connection read chat while logged in.
const READ_SCOPE: &str = "chat:read";

/// How long [`Credentials::from_auth`] waits for the stored login to be read.
const RESTORE_WAIT: Duration = Duration::from_secs(5);

/// The login state once the stored login has been read, waiting at most [`RESTORE_WAIT`] for it
/// (after that, whatever the state is then, `Restoring` included).
pub async fn settled_state(auth: &Auth) -> AuthState {
    let mut states = auth.subscribe();
    // Elapsed or closed: whatever the state is now decides.
    let _ = timeout(
        RESTORE_WAIT,
        states.wait_for(|state| !matches!(state, AuthState::Restoring)),
    )
    .await;
    auth.state()
}

/// What the client logs in as.
///
/// The login and OAuth token (no `oauth:` prefix, `chat:read` scope) the client logs in with.
/// Nothing here fetches or refreshes tokens: the app does, through a [`CredentialsProvider`]
/// (or, for a fixed login, [`TwitchClient::set_credentials`], which reconnects with them).
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub login: String,
    /// OAuth access token with the `chat:read` scope; `None` for anonymous.
    pub token: Option<String>,
}

impl Credentials {
    /// A random `justinfan<N>` login: read-only, no token.
    pub fn anonymous() -> Self {
        Self {
            login: anonymous_login(clock_seed()),
            token: None,
        }
    }

    /// A logged-in user.
    pub fn user(login: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            login: login.into().to_ascii_lowercase(),
            token: Some(token.into()),
        }
    }

    /// The credentials for `auth`'s current login: the user's login and a fresh access token
    /// (refreshed first when it is about to expire), or anonymous when nobody is logged in, the
    /// login lacks `chat:read` (it predates the scope: the UI asks the user to log in again) or the
    /// token cannot be had. Waits briefly for the stored login to be read at start-up
    /// ([`settled_state`]), so the first connection already uses it.
    ///
    /// `auth_failures` is how many connections in a row Twitch refused the token of
    /// (`NOTICE * :Login authentication failed`, see [`CredentialsProvider`]): the first time the
    /// token is refreshed even though it has not expired (it was revoked or invalidated
    /// server-side), after that the connection stays anonymous instead of presenting a token Twitch
    /// keeps refusing.
    pub async fn from_auth(auth: &Auth, auth_failures: u32) -> Self {
        let state = settled_state(auth).await;
        let Some(login) = irc_login(&state) else {
            if let AuthState::LoggedIn(_) = state {
                tracing::info!(
                    "the login lacks `{READ_SCOPE}`: chat stays anonymous until it is renewed"
                );
            }
            return Self::anonymous();
        };
        if auth_failures >= 2 {
            tracing::warn!("Twitch keeps refusing the chat token: connecting anonymously");
            return Self::anonymous();
        }
        let lease = if auth_failures == 1 {
            // The token Twitch refused is the one `access_token` would hand out again.
            match auth.access_token().await {
                Ok(refused) => auth.refresh_after_unauthorized(refused.generation).await,
                Err(error) => Err(error),
            }
        } else {
            auth.access_token().await
        };
        match lease {
            Ok(lease) => Self::user(login, lease.token.expose()),
            Err(error) => {
                tracing::warn!(%error, "no access token for chat: connecting anonymously");
                Self::anonymous()
            }
        }
    }

    /// `true` for `justinfan` logins without a token.
    pub fn is_anonymous(&self) -> bool {
        self.token.is_none()
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("login", &self.login)
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Supplies the credentials for **each** connection attempt (PR 3.2): the app asks its login owner,
/// which refreshes an access token that has expired in the meantime, so a reconnect hours after the
/// login never presents a stale token. Set it on [`ClientConfig::credentials_provider`]; while it is
/// set it decides the credentials of every attempt (and [`TwitchClient::set_credentials`] has no
/// lasting effect). Anything but a [`Credentials::user`] is a read-only anonymous connection.
///
/// The provider is called with the number of connections **in a row** that Twitch refused the token
/// of (`NOTICE * :Login authentication failed`, which a token revoked before its local expiry
/// gets): 0 normally. At 1 it should refresh the token even though it looks valid, at 2 and more
/// give up on it; the count resets when a connection is accepted.
#[derive(Clone)]
pub struct CredentialsProvider(Arc<ProviderFn>);

type ProviderFn = dyn Fn(u32) -> Pin<Box<dyn Future<Output = Credentials> + Send>> + Send + Sync;

impl CredentialsProvider {
    pub fn new<F, Fut>(provider: F) -> Self
    where
        F: Fn(u32) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Credentials> + Send + 'static,
    {
        Self(Arc::new(move |failures| Box::pin(provider(failures))))
    }

    async fn credentials(&self, auth_failures: u32) -> Credentials {
        (self.0)(auth_failures).await
    }
}

impl std::fmt::Debug for CredentialsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialsProvider")
    }
}

/// The login IRC authenticates as in `state`: the logged-in account, unless its login predates
/// `chat:read` (then chat stays anonymous until the user logs in again). `None`: anonymous.
pub fn irc_login(state: &AuthState) -> Option<&str> {
    let account = state.account()?;
    (!account
        .missing_scopes
        .iter()
        .any(|scope| scope == READ_SCOPE))
    .then_some(account.login.as_str())
}

fn anonymous_login(seed: u64) -> String {
    format!("justinfan{}", 10_000 + seed % 80_000)
}

fn clock_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()) ^ d.as_secs())
}

/// Tunables for [`TwitchClient::spawn`]. `ClientConfig::default()` is what the app uses.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub credentials: Credentials,
    /// Asked for the credentials before every connection attempt; see [`CredentialsProvider`].
    pub credentials_provider: Option<CredentialsProvider>,
    /// Capacity of the event channel.
    pub event_capacity: usize,
    /// Reconnect backoff.
    pub backoff: Backoff,
    /// A session that stayed connected at least this long resets the backoff.
    pub stable_after: Duration,
    pub join_limit: JoinRateLimit,
    /// Limit for TCP connect plus TLS handshake.
    pub connect_timeout: Duration,
    /// Limit for the server to accept the login (`001`) after the socket opened.
    pub login_timeout: Duration,
    /// How often an idle connection is probed with `PING`.
    pub ping_interval: Duration,
    /// How long after a `PING` any traffic must arrive before the connection is dropped.
    pub ping_timeout: Duration,
    /// Limit for writing one line to the socket.
    pub send_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            credentials: Credentials::anonymous(),
            credentials_provider: None,
            event_capacity: 4096,
            backoff: Backoff::default(),
            stable_after: Duration::from_secs(30),
            join_limit: JoinRateLimit::default(),
            connect_timeout: Duration::from_secs(20),
            login_timeout: Duration::from_secs(15),
            ping_interval: Duration::from_secs(30),
            ping_timeout: Duration::from_secs(10),
            send_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug)]
enum Command {
    Join(ChannelId),
    Part(ChannelId),
    SetCredentials(Credentials),
    Reconnect,
    Shutdown,
}

/// Handle to the connection task. Cheap to clone; all methods are non-blocking.
/// The task ends when [`shutdown`](Self::shutdown) is called, every handle is dropped,
/// or the event receiver is dropped.
#[derive(Debug, Clone)]
pub struct TwitchClient {
    commands: mpsc::UnboundedSender<Command>,
}

impl TwitchClient {
    /// Starts the connection task on the current tokio runtime and begins connecting.
    /// Must be called from within a runtime. Join channels with [`join`](Self::join).
    pub fn spawn(config: ClientConfig) -> (Self, mpsc::Receiver<ChatEvent>) {
        let connector = TcpConnector {
            connect_timeout: config.connect_timeout,
        };
        Self::spawn_with(connector, config)
    }

    fn spawn_with<C: Connector>(
        connector: C,
        config: ClientConfig,
    ) -> (Self, mpsc::Receiver<ChatEvent>) {
        let (events_tx, events_rx) = mpsc::channel(config.event_capacity.max(1));
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let supervisor = Supervisor {
            connector,
            limiter: JoinLimiter::new(config.join_limit),
            credentials: config.credentials.clone(),
            config,
            commands: commands_rx,
            events: events_tx,
            wanted: Vec::new(),
            rng: clock_seed() | 1,
            auth_failures: 0,
        };
        tokio::spawn(supervisor.run());
        (
            Self {
                commands: commands_tx,
            },
            events_rx,
        )
    }

    /// Joins `channel` and keeps it joined across reconnects. Joins are paced by
    /// [`ClientConfig::join_limit`]. No-op if already wanted.
    pub fn join(&self, channel: &ChannelId) {
        self.send(Command::Join(channel.clone()));
    }

    /// Leaves `channel` and stops rejoining it.
    pub fn part(&self, channel: &ChannelId) {
        self.send(Command::Part(channel.clone()));
    }

    /// Auth hook: replaces the login and reconnects right away (no backoff). Wanted
    /// channels are rejoined. Logging out is
    /// `set_credentials(Credentials::anonymous())`.
    pub fn set_credentials(&self, credentials: Credentials) {
        self.send(Command::SetCredentials(credentials));
    }

    /// Drops the connection and connects again at once (no backoff), asking the
    /// [`CredentialsProvider`] for fresh credentials; wanted channels are rejoined. This is what
    /// the app calls when the user logs in or out.
    pub fn reconnect(&self) {
        self.send(Command::Reconnect);
    }

    /// Closes the connection and ends the task. A final
    /// `Connection { channel: None, state: Disconnected }` is sent if there is room.
    pub fn shutdown(&self) {
        self.send(Command::Shutdown);
    }

    fn send(&self, command: Command) {
        // A closed channel means the task already ended; nothing to do.
        let _ = self.commands.send(command);
    }
}

// ---- connection abstraction (lets tests run without a network) -----------------------

trait Connector: Send + Sync + 'static {
    type Conn: Connection;
    fn connect(&mut self) -> impl Future<Output = Result<Self::Conn, String>> + Send;
}

trait Connection: Send + 'static {
    /// Next message; `Ok(None)` is end of stream. Must be cancel safe: it is polled in
    /// a `select!`.
    fn recv(&mut self) -> impl Future<Output = Result<Option<IRCMessage>, String>> + Send;
    fn send(&mut self, message: IRCMessage) -> impl Future<Output = Result<(), String>> + Send;
}

struct TcpConnector {
    connect_timeout: Duration,
}

struct TcpConnection {
    incoming: <SecureTCPTransport as Transport>::Incoming,
    outgoing: <SecureTCPTransport as Transport>::Outgoing,
}

impl Connector for TcpConnector {
    type Conn = TcpConnection;

    async fn connect(&mut self) -> Result<TcpConnection, String> {
        let transport = timeout(self.connect_timeout, SecureTCPTransport::new())
            .await
            .map_err(|_| "connect timed out".to_owned())?
            .map_err(|e| e.to_string())?;
        let (incoming, outgoing) = transport.split();
        Ok(TcpConnection { incoming, outgoing })
    }
}

impl Connection for TcpConnection {
    async fn recv(&mut self) -> Result<Option<IRCMessage>, String> {
        loop {
            match self.incoming.next().await {
                None => return Ok(None),
                Some(Ok(message)) => {
                    tracing::trace!(target: "gpui_chat_twitch::raw", "< {}", message.as_raw_irc());
                    return Ok(Some(message));
                }
                // One unparseable line must not cost the connection.
                Some(Err(e)) if e.is_right() => tracing::warn!("skipping malformed IRC line: {e}"),
                Some(Err(e)) => return Err(e.to_string()),
            }
        }
    }

    async fn send(&mut self, message: IRCMessage) -> Result<(), String> {
        self.outgoing.send(message).await.map_err(|e| e.to_string())
    }
}

// ---- supervisor ------------------------------------------------------------------------

enum ConnectOutcome<C> {
    Connected(C),
    Failed(String),
    /// Credentials changed while connecting.
    Restart,
    Shutdown,
}

enum Offline {
    Continue,
    Shutdown,
    Credentials,
}

enum SessionEnd {
    Shutdown,
    Lost {
        reason: String,
        /// How long the session was logged in, if it got that far.
        connected_for: Option<Duration>,
        /// Reconnect without waiting (credentials changed, server asked us to).
        immediate: bool,
    },
}

struct Supervisor<C: Connector> {
    connector: C,
    config: ClientConfig,
    credentials: Credentials,
    commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::Sender<ChatEvent>,
    /// Channels to be joined, in the order they were asked for.
    wanted: Vec<ChannelId>,
    limiter: JoinLimiter,
    rng: u64,
    /// Connections in a row that ended with Twitch refusing the token; see [`CredentialsProvider`].
    auth_failures: u32,
}

impl<C: Connector> Supervisor<C> {
    async fn run(mut self) {
        let mut attempt: u32 = 0;
        let mut skip_delay = false;
        if !self.emit_connection(ConnectionState::Connecting).await {
            return;
        }
        loop {
            if attempt > 0 {
                if !self
                    .emit_connection(ConnectionState::Reconnecting { attempt })
                    .await
                {
                    break;
                }
                let delay = if skip_delay {
                    Duration::ZERO
                } else {
                    {
                        let sample = self.sample();
                        self.config.backoff.delay(attempt, sample)
                    }
                };
                tracing::info!(attempt, ?delay, "reconnecting");
                if !self.idle(delay).await {
                    break;
                }
            }
            skip_delay = false;

            if let Some(provider) = self.config.credentials_provider.clone() {
                self.credentials = provider.credentials(self.auth_failures).await;
                tracing::debug!(
                    login = %self.credentials.login,
                    anonymous = self.credentials.is_anonymous(),
                    "connecting"
                );
            }
            let conn = match self.connect_while_serving_commands().await {
                ConnectOutcome::Connected(conn) => conn,
                ConnectOutcome::Failed(reason) => {
                    tracing::warn!("connect failed: {reason}");
                    attempt += 1;
                    continue;
                }
                // New credentials arrived mid-connect: start over with them, at once.
                ConnectOutcome::Restart => {
                    skip_delay = true;
                    attempt = attempt.max(1);
                    continue;
                }
                ConnectOutcome::Shutdown => break,
            };

            match self.session(conn).await {
                SessionEnd::Shutdown => break,
                SessionEnd::Lost {
                    reason,
                    connected_for,
                    immediate,
                } => {
                    tracing::info!("connection lost: {reason}");
                    let stable = connected_for.is_some_and(|d| d >= self.config.stable_after);
                    skip_delay = immediate;
                    if stable || immediate {
                        attempt = 0;
                    }
                    attempt += 1;
                }
            }
        }
        // Best effort: nobody may be listening any more.
        let _ = self.events.try_send(connection_event(
            None,
            ConnectionState::Disconnected { reason: None },
        ));
    }

    /// Connects, but keeps applying commands meanwhile.
    async fn connect_while_serving_commands(&mut self) -> ConnectOutcome<C::Conn> {
        let mut connect = pin!(self.connector.connect());
        loop {
            tokio::select! {
                result = &mut connect => return match result {
                    Ok(conn) => ConnectOutcome::Connected(conn),
                    Err(reason) => ConnectOutcome::Failed(reason),
                },
                command = self.commands.recv() => match offline_command(&mut self.wanted, &mut self.credentials, &self.events, command).await {
                    Offline::Continue => {}
                    Offline::Shutdown => return ConnectOutcome::Shutdown,
                    Offline::Credentials => return ConnectOutcome::Restart,
                },
            }
        }
    }

    /// Waits `delay`, applying commands meanwhile. `false` = shut down. A credentials
    /// change ends the wait early.
    async fn idle(&mut self, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        loop {
            tokio::select! {
                () = sleep_until(until) => return true,
                command = self.commands.recv() => match offline_command(&mut self.wanted, &mut self.credentials, &self.events, command).await {
                    Offline::Continue => {}
                    Offline::Shutdown => return false,
                    Offline::Credentials => return true,
                },
            }
        }
    }

    /// Runs one connection to its end. Events are queued in an outbox and delivered by a
    /// `select!` branch, so a full event channel never stops the loop from answering
    /// PINGs or handling commands (see [`SessionState::queue`]).
    async fn session(&mut self, mut conn: C::Conn) -> SessionEnd {
        let started = Instant::now();
        let mut options = ParseOptions {
            own_login: (!self.credentials.is_anonymous()).then(|| self.credentials.login.clone()),
            fallback_timestamp: Timestamp::default(),
        };
        let mut handshake = vec![irc!["CAP", "REQ", "twitch.tv/tags twitch.tv/commands"]];
        if let Some(token) = &self.credentials.token {
            handshake.push(irc!["PASS", format!("oauth:{token}")]);
        }
        handshake.push(irc!["NICK", self.credentials.login.clone()]);
        for message in handshake {
            if let Err(reason) = self.write(&mut conn, message).await {
                return lost(reason, None, false);
            }
        }

        let mut state = SessionState {
            login_deadline: Some(started + self.config.login_timeout),
            pong_deadline: None,
            next_ping: started + self.config.ping_interval,
            welcomed_at: None,
            pending: VecDeque::new(),
            sent: HashSet::new(),
            outbox: VecDeque::new(),
            outbox_limit: self.config.event_capacity.max(1),
            dropped: 0,
        };
        let end = self.session_loop(&mut conn, &mut state, &mut options).await;
        // Deliver what is still queued before the caller reports the reconnect, so
        // events stay in order. `Shutdown` is best effort.
        match end {
            SessionEnd::Shutdown => {
                for event in state.outbox.drain(..) {
                    let _ = self.events.try_send(event);
                }
                SessionEnd::Shutdown
            }
            end => {
                if self.flush(&mut state.outbox).await {
                    end
                } else {
                    SessionEnd::Shutdown
                }
            }
        }
    }

    /// Delivers `outbox`, still applying commands meanwhile. `false` = shut down.
    async fn flush(&mut self, outbox: &mut VecDeque<ChatEvent>) -> bool {
        while !outbox.is_empty() {
            tokio::select! {
                permit = self.events.reserve() => match permit {
                    Ok(permit) => {
                        if let Some(event) = outbox.pop_front() {
                            permit.send(event);
                        }
                    }
                    Err(_) => return false,
                },
                command = self.commands.recv() => match offline_command(&mut self.wanted, &mut self.credentials, &self.events, command).await {
                    Offline::Shutdown => return false,
                    // The new credentials are used by the next connection.
                    Offline::Continue | Offline::Credentials => {}
                },
            }
        }
        true
    }

    async fn session_loop(
        &mut self,
        conn: &mut C::Conn,
        state: &mut SessionState,
        options: &mut ParseOptions,
    ) -> SessionEnd {
        // A local handle: the select below borrows it (and the `Permit` it hands out)
        // while the branch handlers need `&mut self`.
        let events = self.events.clone();
        loop {
            let join_at = (state.welcomed_at.is_some() && !state.pending.is_empty())
                .then(|| self.limiter.ready_at(Instant::now()));
            let deadline = [state.login_deadline, state.pong_deadline]
                .into_iter()
                .flatten()
                .min();

            tokio::select! {
                command = self.commands.recv() => match command {
                    None | Some(Command::Shutdown) => return SessionEnd::Shutdown,
                    Some(Command::SetCredentials(credentials)) => {
                        self.credentials = credentials;
                        return lost("credentials changed".into(), state.connected_for(), true);
                    }
                    Some(Command::Reconnect) => {
                        return lost("reconnect requested".into(), state.connected_for(), true);
                    }
                    Some(Command::Join(channel)) => {
                        if !self.wanted.contains(&channel) {
                            self.wanted.push(channel.clone());
                            if state.welcomed_at.is_some() {
                                state.pending.push_back(channel);
                            }
                        }
                    }
                    Some(Command::Part(channel)) => self.part(conn, state, &channel).await,
                },

                received = conn.recv() => {
                    let message = match received {
                        Ok(Some(message)) => message,
                        Ok(None) => return lost("connection closed by server".into(), state.connected_for(), false),
                        Err(reason) => return lost(reason, state.connected_for(), false),
                    };
                    state.pong_deadline = None;
                    match self.handle(conn, state, message, options).await {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::Reconnect) => {
                            return lost("server asked to reconnect".into(), state.connected_for(), true);
                        }
                        Err(reason) => return lost(reason, state.connected_for(), false),
                    }
                }

                // Hand queued events to the UI as soon as the channel has room.
                permit = events.reserve(), if !state.outbox.is_empty() => match permit {
                    Ok(permit) => {
                        if let Some(event) = state.outbox.pop_front() {
                            permit.send(event);
                        }
                    }
                    Err(_) => return SessionEnd::Shutdown,
                },

                // The receiver was dropped: stop even if the connection is quiet.
                () = events.closed() => return SessionEnd::Shutdown,

                () = sleep_until(join_at.unwrap_or_else(Instant::now)), if join_at.is_some() => {
                    if let Err(reason) = self.send_next_join(conn, state).await {
                        return lost(reason, state.connected_for(), false);
                    }
                }

                () = sleep_until(state.next_ping), if state.welcomed_at.is_some() => {
                    state.next_ping = Instant::now() + self.config.ping_interval;
                    state.pong_deadline.get_or_insert(Instant::now() + self.config.ping_timeout);
                    if let Err(reason) = self.write(conn, irc!["PING", "tmi.twitch.tv"]).await {
                        return lost(reason, state.connected_for(), false);
                    }
                }

                () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    let reason = if state.welcomed_at.is_none() { "login timed out" } else { "ping timed out" };
                    return lost(reason.into(), state.connected_for(), false);
                }
            }
        }
    }

    async fn handle(
        &mut self,
        conn: &mut C::Conn,
        state: &mut SessionState,
        message: IRCMessage,
        options: &mut ParseOptions,
    ) -> Result<Flow, String> {
        match message.command.as_str() {
            "PING" => {
                let token = message.params.first().cloned().unwrap_or_default();
                self.write(conn, irc!["PONG", token]).await?;
                return Ok(Flow::Continue);
            }
            "RECONNECT" => return Ok(Flow::Reconnect),
            // RPL_WELCOME: the login was accepted.
            "001" => {
                self.auth_failures = 0;
                state.welcomed_at = Some(Instant::now());
                state.login_deadline = None;
                state.pending = self.wanted.iter().cloned().collect();
                state.queue(connection_event(None, ConnectionState::Connected));
                return Ok(Flow::Continue);
            }
            // Confirmation of our own JOIN (membership is not requested, so only ours arrive).
            "JOIN" => {
                if let (Some(IRCPrefix::Full { nick, .. }), Some(name)) =
                    (&message.prefix, message.params.first())
                    && nick.eq_ignore_ascii_case(&self.credentials.login)
                {
                    let channel = ChannelId::new(name);
                    if self.wanted.contains(&channel) {
                        state.queue(connection_event(Some(channel), ConnectionState::Connected));
                    }
                }
                return Ok(Flow::Continue);
            }
            // The login was refused (a token Twitch no longer accepts): Twitch closes the
            // connection next. Count it, so the next attempt asks for a refreshed token.
            "NOTICE" if self.refused_login(&message) => {
                if !self.credentials.is_anonymous() {
                    self.auth_failures += 1;
                }
                return Err("login authentication failed".to_owned());
            }
            _ => {}
        }

        options.fallback_timestamp = now_timestamp();
        match map_message(&message, options) {
            Ok(Some(event)) => state.queue(event),
            Ok(None) => {}
            Err(e) => tracing::warn!("ignoring {} line: {e}", message.command),
        }
        Ok(Flow::Continue)
    }

    /// `NOTICE * :Login authentication failed` (or `Login unsuccessful`): before the welcome, with no
    /// channel, saying the login was refused.
    fn refused_login(&self, message: &IRCMessage) -> bool {
        let Some(text) = message.params.get(1) else {
            return false;
        };
        let text = text.to_ascii_lowercase();
        message.params.first().is_some_and(|target| target == "*")
            && (text.contains("login authentication failed") || text.contains("login unsuccessful"))
    }

    async fn send_next_join(
        &mut self,
        conn: &mut C::Conn,
        state: &mut SessionState,
    ) -> Result<(), String> {
        let Some(channel) = state.pending.pop_front() else {
            return Ok(());
        };
        if !self.wanted.contains(&channel) || state.sent.contains(&channel) {
            return Ok(());
        }
        self.limiter.record(Instant::now());
        self.write(conn, irc!["JOIN", channel.irc_name()]).await?;
        state.sent.insert(channel.clone());
        state.queue(connection_event(Some(channel), ConnectionState::Connecting));
        Ok(())
    }

    /// Handles [`Command::Part`].
    async fn part(&mut self, conn: &mut C::Conn, state: &mut SessionState, channel: &ChannelId) {
        if !self.wanted.contains(channel) {
            return;
        }
        self.wanted.retain(|c| c != channel);
        state.pending.retain(|c| c != channel);
        if state.sent.remove(channel) {
            // A failed write surfaces on the next read or ping; the channel is no
            // longer wanted either way, so it is not rejoined.
            let _ = self.write(conn, irc!["PART", channel.irc_name()]).await;
        }
        state.queue(connection_event(
            Some(channel.clone()),
            ConnectionState::Disconnected { reason: None },
        ));
    }

    async fn write(&self, conn: &mut C::Conn, message: IRCMessage) -> Result<(), String> {
        tracing::trace!(target: "gpui_chat_twitch::raw", "> {}", redact(&message));
        timeout(self.config.send_timeout, conn.send(message))
            .await
            .map_err(|_| "write timed out".to_owned())?
    }

    /// `false` when the receiver is gone. Only for use while no connection is open;
    /// a session queues events instead (see [`SessionState::queue`]).
    async fn emit(&self, event: ChatEvent) -> bool {
        self.events.send(event).await.is_ok()
    }

    async fn emit_connection(&self, state: ConnectionState) -> bool {
        self.emit(connection_event(None, state)).await
    }

    /// Uniform random in `0.0..1.0` (xorshift64*; jitter needs nothing better).
    fn sample(&mut self) -> f64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let x = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

enum Flow {
    Continue,
    Reconnect,
}

struct SessionState {
    login_deadline: Option<Instant>,
    pong_deadline: Option<Instant>,
    next_ping: Instant,
    welcomed_at: Option<Instant>,
    /// Channels still to JOIN on this connection.
    pending: VecDeque<ChannelId>,
    /// Channels a JOIN was sent for on this connection.
    sent: HashSet<ChannelId>,
    /// Events waiting for room in the (bounded) event channel.
    outbox: VecDeque<ChatEvent>,
    outbox_limit: usize,
    /// Chat messages dropped because both the channel and the outbox were full.
    dropped: u64,
}

impl SessionState {
    /// Queues an event for the UI. The socket keeps being read while the UI is slow, so
    /// PINGs are answered and commands handled. The outbox holds up to one more channel
    /// capacity of events; past that, chat *messages* are dropped (with a warning) and
    /// everything else (state changes, moderation) is still kept, so a UI that is
    /// stalled for minutes costs messages rather than the connection.
    fn queue(&mut self, event: ChatEvent) {
        if self.outbox.len() >= self.outbox_limit && matches!(event, ChatEvent::Message { .. }) {
            if self.dropped == 0 {
                tracing::warn!("event channel is full; dropping chat messages until it drains");
            }
            self.dropped += 1;
            return;
        }
        if self.dropped > 0 && self.outbox.len() < self.outbox_limit {
            tracing::warn!(dropped = self.dropped, "event channel drained again");
            self.dropped = 0;
        }
        self.outbox.push_back(event);
    }

    fn connected_for(&self) -> Option<Duration> {
        self.welcomed_at.map(|t| t.elapsed())
    }
}

/// A command that arrives while there is no open connection. Takes the pieces of the
/// supervisor it needs so it can run while `connector.connect()` borrows another field.
async fn offline_command(
    wanted: &mut Vec<ChannelId>,
    credentials: &mut Credentials,
    events: &mpsc::Sender<ChatEvent>,
    command: Option<Command>,
) -> Offline {
    match command {
        None | Some(Command::Shutdown) => Offline::Shutdown,
        Some(Command::Join(channel)) => {
            add_wanted(wanted, channel);
            Offline::Continue
        }
        Some(Command::Part(channel)) => {
            if wanted.contains(&channel) {
                wanted.retain(|c| *c != channel);
                let gone = ConnectionState::Disconnected { reason: None };
                if events
                    .send(connection_event(Some(channel), gone))
                    .await
                    .is_err()
                {
                    return Offline::Shutdown;
                }
            }
            Offline::Continue
        }
        Some(Command::SetCredentials(new)) => {
            *credentials = new;
            Offline::Credentials
        }
        Some(Command::Reconnect) => Offline::Credentials,
    }
}

fn lost(reason: String, connected_for: Option<Duration>, immediate: bool) -> SessionEnd {
    SessionEnd::Lost {
        reason,
        connected_for,
        immediate,
    }
}

fn add_wanted(wanted: &mut Vec<ChannelId>, channel: ChannelId) {
    if !wanted.contains(&channel) {
        wanted.push(channel);
    }
}

fn connection_event(channel: Option<ChannelId>, state: ConnectionState) -> ChatEvent {
    ChatEvent::Connection { channel, state }
}

fn now_timestamp() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    Timestamp::from_unix_millis(ms)
}

/// Raw line for trace logging with the token masked.
fn redact(message: &IRCMessage) -> String {
    if message.command == "PASS" {
        "PASS oauth:[redacted]".to_owned()
    } else {
        message.as_raw_irc()
    }
}

#[cfg(test)]
mod tests;
