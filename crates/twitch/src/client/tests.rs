//! Supervisor tests against an in-memory server. Time is paused, so backoff, join pacing
//! and ping timeouts run instantly but are measured exactly.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_chat_core::{ChannelId, ConnectionState};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

use super::*;

const LONG: Duration = Duration::from_secs(300);

// ---- fake network --------------------------------------------------------------------------

struct FakeConnector {
    next: mpsc::UnboundedReceiver<Result<FakeConn, String>>,
    attempts: Arc<Mutex<Vec<Instant>>>,
}

struct FakeConn {
    incoming: mpsc::UnboundedReceiver<Result<IRCMessage, String>>,
    outgoing: mpsc::UnboundedSender<IRCMessage>,
}

impl Connector for FakeConnector {
    type Conn = FakeConn;

    async fn connect(&mut self) -> Result<FakeConn, String> {
        self.attempts.lock().unwrap().push(Instant::now());
        self.next
            .recv()
            .await
            .unwrap_or_else(|| Err("test over".into()))
    }
}

impl Connection for FakeConn {
    async fn recv(&mut self) -> Result<Option<IRCMessage>, String> {
        self.incoming.recv().await.transpose()
    }

    async fn send(&mut self, message: IRCMessage) -> Result<(), String> {
        self.outgoing
            .send(message)
            .map_err(|_| "peer gone".to_owned())
    }
}

/// The server side of one connection. Dropping it closes the connection.
struct Server {
    to_client: mpsc::UnboundedSender<Result<IRCMessage, String>>,
    from_client: mpsc::UnboundedReceiver<IRCMessage>,
}

impl Server {
    fn say(&self, line: &str) {
        self.to_client
            .send(Ok(IRCMessage::parse(line).unwrap()))
            .unwrap();
    }

    /// Next line the client wrote, as raw IRC, with the time it arrived.
    async fn read(&mut self) -> (String, Instant) {
        let message = timeout(LONG, self.from_client.recv())
            .await
            .expect("client wrote nothing")
            .expect("client closed the connection");
        (message.as_raw_irc(), Instant::now())
    }

    async fn line(&mut self) -> String {
        self.read().await.0
    }

    /// `true` once the client dropped its end.
    async fn client_hung_up(&mut self) -> bool {
        timeout(LONG, self.from_client.recv())
            .await
            .expect("no hang-up")
            .is_none()
    }

    /// Reads and checks the login handshake; returns the NICK and whether PASS was sent.
    async fn handshake(&mut self) -> (String, Option<String>) {
        assert_eq!(
            self.line().await,
            "CAP REQ :twitch.tv/tags twitch.tv/commands"
        );
        let mut line = self.line().await;
        let pass = line.strip_prefix("PASS ").map(str::to_owned);
        if pass.is_some() {
            line = self.line().await;
        }
        let nick = line.strip_prefix("NICK ").expect("NICK").to_owned();
        (nick, pass)
    }

    fn welcome(&self, nick: &str) {
        self.say(&format!(":tmi.twitch.tv 001 {nick} :Welcome, GLHF!"));
    }

    fn confirm_join(&self, nick: &str, channel: &str) {
        self.say(&format!(
            ":{nick}!{nick}@{nick}.tmi.twitch.tv JOIN #{channel}"
        ));
    }
}

struct Net {
    connections: mpsc::UnboundedSender<Result<FakeConn, String>>,
    attempts: Arc<Mutex<Vec<Instant>>>,
}

impl Net {
    fn accept(&self) -> Server {
        let (to_client, incoming) = mpsc::unbounded_channel();
        let (outgoing, from_client) = mpsc::unbounded_channel();
        self.connections
            .send(Ok(FakeConn { incoming, outgoing }))
            .unwrap();
        Server {
            to_client,
            from_client,
        }
    }

    fn refuse(&self, why: &str) {
        self.connections.send(Err(why.to_owned())).unwrap();
    }

    fn attempts(&self) -> Vec<Instant> {
        self.attempts.lock().unwrap().clone()
    }
}

struct Rig {
    client: TwitchClient,
    events: mpsc::Receiver<ChatEvent>,
    net: Net,
}

impl Rig {
    async fn event(&mut self) -> ChatEvent {
        timeout(LONG, self.events.recv())
            .await
            .expect("no event")
            .expect("event channel closed")
    }

    async fn expect_connection(&mut self, channel: Option<&str>, state: ConnectionState) {
        assert_eq!(
            self.event().await,
            ChatEvent::Connection {
                channel: channel.map(ChannelId::new),
                state
            }
        );
    }
}

fn config() -> ClientConfig {
    ClientConfig {
        credentials: Credentials {
            login: "justinfan1".into(),
            token: None,
        },
        backoff: Backoff {
            jitter: 0.0,
            ..Backoff::default()
        },
        ..ClientConfig::default()
    }
}

fn rig_with(config: ClientConfig) -> Rig {
    let (connections, next) = mpsc::unbounded_channel();
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let connector = FakeConnector {
        next,
        attempts: Arc::clone(&attempts),
    };
    let (client, events) = TwitchClient::spawn_with(connector, config);
    Rig {
        client,
        events,
        net: Net {
            connections,
            attempts,
        },
    }
}

fn rig() -> Rig {
    rig_with(config())
}

fn chan(name: &str) -> ChannelId {
    ChannelId::new(name)
}

const NICK: &str = "justinfan1";

/// Connects, logs in and joins `#chan`, leaving the rig idle and connected.
async fn connected(rig: &mut Rig, channel: &str) -> Server {
    rig.client.join(&chan(channel));
    rig.expect_connection(None, ConnectionState::Connecting)
        .await;
    let channel = channel.to_ascii_lowercase();
    let channel = channel.as_str();
    let mut server = rig.net.accept();
    server.handshake().await;
    server.welcome(NICK);
    rig.expect_connection(None, ConnectionState::Connected)
        .await;
    assert_eq!(server.line().await, format!("JOIN #{channel}"));
    rig.expect_connection(Some(channel), ConnectionState::Connecting)
        .await;
    server.confirm_join(NICK, channel);
    rig.expect_connection(Some(channel), ConnectionState::Connected)
        .await;
    server
}

// ---- tests ---------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn logs_in_anonymously_joins_and_delivers_messages() {
    let mut rig = rig();
    let mut server = connected(&mut rig, "Chan").await;

    server.say("@id=m1;tmi-sent-ts=7 :a!a@a.tmi.twitch.tv PRIVMSG #chan :hello");
    match rig.event().await {
        ChatEvent::Message { channel, message } => {
            assert_eq!(channel, chan("chan"));
            assert_eq!(message.plain_text(), "hello");
            assert_eq!(message.timestamp.unix_millis(), 7);
        }
        other => panic!("{other:?}"),
    }

    server.say("@room-id=9;slow=5 :tmi.twitch.tv ROOMSTATE #chan");
    assert!(matches!(rig.event().await, ChatEvent::RoomState { .. }));

    // Server keep-alive is answered with the same token.
    server.say("PING :tmi.twitch.tv");
    assert_eq!(server.line().await, "PONG tmi.twitch.tv");
}

#[tokio::test(start_paused = true)]
async fn anonymous_login_has_no_password() {
    let mut rig = rig();
    rig.client.join(&chan("a"));
    rig.expect_connection(None, ConnectionState::Connecting)
        .await;
    let mut server = rig.net.accept();
    let (nick, pass) = server.handshake().await;
    assert_eq!(nick, NICK);
    assert_eq!(pass, None);
    assert!(Credentials::anonymous().login.starts_with("justinfan"));
}

#[tokio::test(start_paused = true)]
async fn bad_lines_are_skipped_and_the_connection_survives() {
    let mut rig = rig();
    let server = connected(&mut rig, "chan").await;
    // PRIVMSG without an id tag cannot become a message.
    server.say("@tmi-sent-ts=1 :a!a@a.tmi.twitch.tv PRIVMSG #chan :broken");
    server.say("@id=ok;tmi-sent-ts=2 :a!a@a.tmi.twitch.tv PRIVMSG #chan :fine");
    match rig.event().await {
        ChatEvent::Message { message, .. } => assert_eq!(message.plain_text(), "fine"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn reconnects_with_growing_backoff_and_rejoins() {
    let mut rig = rig();
    let server = connected(&mut rig, "chan").await;
    let dropped_at = Instant::now();
    drop(server);

    rig.expect_connection(None, ConnectionState::Reconnecting { attempt: 1 })
        .await;
    // The retry waits 1 s, then fails; the next waits 2 s, then succeeds.
    rig.net.refuse("no route");
    rig.expect_connection(None, ConnectionState::Reconnecting { attempt: 2 })
        .await;
    let mut server = rig.net.accept();
    server.handshake().await;
    server.welcome(NICK);
    rig.expect_connection(None, ConnectionState::Connected)
        .await;

    let attempts = rig.net.attempts();
    assert_eq!(attempts.len(), 3);
    assert_eq!(attempts[1] - dropped_at, Duration::from_secs(1));
    assert_eq!(attempts[2] - attempts[1], Duration::from_secs(2));

    // The wanted channel is joined again and goes through the states once more.
    assert_eq!(server.line().await, "JOIN #chan");
    rig.expect_connection(Some("chan"), ConnectionState::Connecting)
        .await;
    server.confirm_join(NICK, "chan");
    rig.expect_connection(Some("chan"), ConnectionState::Connected)
        .await;
}

#[tokio::test(start_paused = true)]
async fn backoff_resets_after_a_stable_session() {
    let mut rig = rig();
    let mut server = connected(&mut rig, "chan").await;
    // Healthy for longer than `stable_after` (30 s): answer two keep-alive pings.
    for _ in 0..2 {
        assert_eq!(server.line().await, "PING tmi.twitch.tv");
        server.say(":tmi.twitch.tv PONG tmi.twitch.tv :tmi.twitch.tv");
    }
    let dropped_at = Instant::now();
    drop(server);
    rig.expect_connection(None, ConnectionState::Reconnecting { attempt: 1 })
        .await;
    let _server = rig.net.accept();
    let _ = rig.event().await;
    let attempts = rig.net.attempts();
    assert_eq!(
        attempts[1] - dropped_at,
        Duration::from_secs(1),
        "attempt counter was reset"
    );
}

#[tokio::test(start_paused = true)]
async fn join_rate_is_limited() {
    let mut rig = rig_with(ClientConfig {
        join_limit: JoinRateLimit {
            max_joins: 3,
            per: Duration::from_secs(10),
        },
        ..config()
    });
    for i in 0..7 {
        rig.client.join(&chan(&format!("c{i}")));
    }
    rig.client.join(&chan("c0")); // duplicate: ignored
    rig.expect_connection(None, ConnectionState::Connecting)
        .await;
    let mut server = rig.net.accept();
    server.handshake().await;
    server.welcome(NICK);

    let mut joins = Vec::new();
    for _ in 0..7 {
        let (line, at) = server.read().await;
        joins.push((line, at));
    }
    let names: Vec<_> = joins.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        names,
        [
            "JOIN #c0", "JOIN #c1", "JOIN #c2", "JOIN #c3", "JOIN #c4", "JOIN #c5", "JOIN #c6"
        ]
    );
    let t0 = joins[0].1;
    let secs = |i: usize| (joins[i].1 - t0).as_secs();
    assert_eq!([secs(0), secs(1), secs(2)], [0, 0, 0]);
    assert_eq!([secs(3), secs(4), secs(5)], [10, 10, 10]);
    assert_eq!(secs(6), 20);
}

#[tokio::test(start_paused = true)]
async fn part_leaves_and_is_not_rejoined() {
    let mut rig = rig();
    let mut server = connected(&mut rig, "a").await;
    rig.client.join(&chan("b"));
    assert_eq!(server.line().await, "JOIN #b");
    rig.expect_connection(Some("b"), ConnectionState::Connecting)
        .await;
    server.confirm_join(NICK, "b");
    rig.expect_connection(Some("b"), ConnectionState::Connected)
        .await;

    rig.client.part(&chan("a"));
    assert_eq!(server.line().await, "PART #a");
    rig.expect_connection(Some("a"), ConnectionState::Disconnected { reason: None })
        .await;

    drop(server);
    rig.expect_connection(None, ConnectionState::Reconnecting { attempt: 1 })
        .await;
    let mut server = rig.net.accept();
    server.handshake().await;
    server.welcome(NICK);
    assert_eq!(server.line().await, "JOIN #b", "only b is wanted any more");
}

#[tokio::test(start_paused = true)]
async fn set_credentials_reconnects_immediately_with_the_token() {
    let mut rig = rig();
    let mut server = connected(&mut rig, "chan").await;
    let at = Instant::now();
    rig.client
        .set_credentials(Credentials::user("Me", "secret"));

    assert!(server.client_hung_up().await);
    rig.expect_connection(None, ConnectionState::Reconnecting { attempt: 1 })
        .await;
    let mut server = rig.net.accept();
    let (nick, pass) = server.handshake().await;
    assert_eq!(
        (nick.as_str(), pass.as_deref()),
        ("me", Some("oauth:secret"))
    );
    assert_eq!(
        rig.net.attempts()[1],
        at,
        "no backoff for a credentials change"
    );

    server.welcome("me");
    rig.expect_connection(None, ConnectionState::Connected)
        .await;
    assert_eq!(server.line().await, "JOIN #chan");

    // Mentions of the new login are flagged.
    server.say("@id=1;tmi-sent-ts=1 :a!a@a.tmi.twitch.tv PRIVMSG #chan :hi @me");
    rig.expect_connection(Some("chan"), ConnectionState::Connecting)
        .await;
    match rig.event().await {
        ChatEvent::Message { message, .. } => assert!(message.flags.mention),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn credentials_do_not_leak_into_debug_output() {
    let c = Credentials::user("me", "secret");
    assert!(!format!("{c:?}").contains("secret"));
    assert!(
        !format!(
            "{:?}",
            ClientConfig {
                credentials: c,
                ..config()
            }
        )
        .contains("secret")
    );
}

#[tokio::test(start_paused = true)]
async fn silent_server_is_dropped_after_a_missed_pong() {
    let mut rig = rig();
    let mut server = connected(&mut rig, "chan").await;
    let (line, at) = server.read().await;
    assert_eq!(line, "PING tmi.twitch.tv");
    // No PONG: the connection is dropped `ping_timeout` later.
    rig.expect_connection(None, ConnectionState::Reconnecting { attempt: 1 })
        .await;
    assert_eq!(
        Instant::now() - at,
        Duration::from_secs(10),
        "dropped `ping_timeout` after the PING"
    );
}

#[tokio::test(start_paused = true)]
async fn answered_ping_keeps_the_connection() {
    let mut rig = rig();
    let mut server = connected(&mut rig, "chan").await;
    for _ in 0..3 {
        assert_eq!(server.line().await, "PING tmi.twitch.tv");
        server.say(":tmi.twitch.tv PONG tmi.twitch.tv :tmi.twitch.tv");
    }
    server.say("@id=1;tmi-sent-ts=1 :a!a@a.tmi.twitch.tv PRIVMSG #chan :still here");
    assert!(matches!(rig.event().await, ChatEvent::Message { .. }));
    assert_eq!(rig.net.attempts().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn login_that_never_completes_times_out() {
    let mut rig = rig();
    rig.expect_connection(None, ConnectionState::Connecting)
        .await;
    let mut server = rig.net.accept();
    server.handshake().await;
    // No 001.
    rig.expect_connection(None, ConnectionState::Reconnecting { attempt: 1 })
        .await;
    drop(server);
}

#[tokio::test(start_paused = true)]
async fn server_reconnect_request_is_followed() {
    let mut rig = rig();
    let server = connected(&mut rig, "chan").await;
    server.say(":tmi.twitch.tv RECONNECT");
    rig.expect_connection(None, ConnectionState::Reconnecting { attempt: 1 })
        .await;
    let mut again = rig.net.accept();
    again.handshake().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_reports_disconnected_and_ends() {
    let mut rig = rig();
    let mut server = connected(&mut rig, "chan").await;
    rig.client.shutdown();
    rig.expect_connection(None, ConnectionState::Disconnected { reason: None })
        .await;
    assert!(rig.events.recv().await.is_none(), "channel closes");
    assert!(server.client_hung_up().await);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_receiver_stops_the_task() {
    let mut rig = rig();
    let mut server = connected(&mut rig, "chan").await;
    let Rig { events, .. } = rig;
    drop(events);
    // The next event finds nobody listening; the client closes the connection.
    server.say("@id=1;tmi-sent-ts=1 :a!a@a.tmi.twitch.tv PRIVMSG #chan :nobody listens");
    assert!(server.client_hung_up().await);
}

#[tokio::test(start_paused = true)]
async fn commands_are_applied_while_connecting_and_backing_off() {
    let mut rig = rig();
    rig.expect_connection(None, ConnectionState::Connecting)
        .await;
    // No connection offered yet: the connect call is pending.
    rig.client.join(&chan("early"));
    rig.client.join(&chan("dropped"));
    rig.client.part(&chan("dropped"));
    rig.expect_connection(
        Some("dropped"),
        ConnectionState::Disconnected { reason: None },
    )
    .await;
    let mut server = rig.net.accept();
    server.handshake().await;
    server.welcome(NICK);
    assert_eq!(server.line().await, "JOIN #early");
}
