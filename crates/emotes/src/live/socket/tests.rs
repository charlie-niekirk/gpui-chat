use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;

use super::*;
use crate::live::testserver::{server, server_greeting};

#[test]
fn the_wait_doubles_up_to_the_cap_and_jitter_spreads_it() {
    let backoff = Backoff::default();
    let secs = |failures, jitter| backoff.delay(failures, jitter).as_secs_f64();
    assert_eq!(secs(1, 1.0), 1.0);
    assert_eq!(secs(2, 1.0), 2.0);
    assert_eq!(secs(3, 1.0), 4.0);
    assert_eq!(secs(7, 1.0), 60.0);
    assert_eq!(secs(40, 1.0), 60.0, "no overflow, capped");
    assert_eq!(secs(1, 0.0), 0.75);
    assert!(secs(0, 0.5) > 0.0);
}

/// Subscribes to words; events are text frames "event:<word>".
struct Probe {
    subs: Vec<String>,
    seen: Arc<Mutex<Vec<String>>>,
    opened: Arc<Mutex<Vec<Vec<String>>>>,
    /// When `on_tick` should run next, and how often it did.
    tick: Arc<Mutex<Option<Instant>>>,
    ticks: Arc<Mutex<usize>>,
}

impl Session for Probe {
    type Sub = String;

    fn wants_connection(&self) -> bool {
        !self.subs.is_empty()
    }

    fn on_open(&mut self) -> Vec<String> {
        let frames: Vec<String> = self.subs.iter().map(|s| format!("sub:{s}")).collect();
        self.opened.lock().unwrap().push(frames.clone());
        frames
    }

    fn on_text(&mut self, text: &str, _: Instant) -> Reaction {
        self.seen.lock().unwrap().push(text.to_owned());
        Reaction {
            send: Vec::new(),
            reconnect: text == "bye" || text == "refused",
            hold_off: if text == "refused" {
                Duration::from_millis(400)
            } else {
                Duration::ZERO
            },
        }
    }

    fn apply(&mut self, change: Change<String>) -> Vec<String> {
        match change {
            Change::Subscribe(s) => {
                self.subs.push(s.clone());
                vec![format!("sub:{s}")]
            }
            Change::Unsubscribe(s) => {
                self.subs.retain(|x| *x != s);
                vec![format!("unsub:{s}")]
            }
            Change::Clear => {
                self.subs.clear();
                Vec::new()
            }
        }
    }

    fn idle_limit(&self) -> Option<Duration> {
        None
    }

    fn ping_interval(&self) -> Option<Duration> {
        None
    }

    fn next_tick(&self) -> Option<Instant> {
        *self.tick.lock().unwrap()
    }

    fn on_tick(&mut self, _: Instant) {
        *self.tick.lock().unwrap() = None;
        *self.ticks.lock().unwrap() += 1;
    }
}

fn fast() -> Backoff {
    Backoff {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(50),
        healthy_after: Duration::from_secs(30),
        connect_timeout: Duration::from_secs(2),
    }
}

async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..300 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn no_subscriptions_means_no_connection_until_there_is_one() {
    let server = server().await;
    let (tx, rx) = mpsc::unbounded_channel();
    let probe = Probe {
        subs: Vec::new(),
        seen: Arc::default(),
        opened: Arc::default(),
        tick: Arc::default(),
        ticks: Arc::default(),
    };
    tokio::spawn(run("probe", server.url.clone(), probe, rx, fast()));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(*server.accepted.lock().unwrap(), 0);

    tx.send(Change::Subscribe("a".into())).unwrap();
    eventually(|| *server.accepted.lock().unwrap() == 1).await;
    eventually(|| server.received.lock().unwrap()[0] == ["sub:a"]).await;
    // A live change goes out on the open connection.
    tx.send(Change::Subscribe("b".into())).unwrap();
    eventually(|| server.received.lock().unwrap()[0] == ["sub:a", "sub:b"]).await;
}

#[tokio::test]
async fn events_arrive_and_the_last_unsubscribe_closes_the_connection() {
    let server = server().await;
    let (tx, rx) = mpsc::unbounded_channel();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe {
        subs: vec!["a".into()],
        seen: seen.clone(),
        opened: Arc::default(),
        tick: Arc::default(),
        ticks: Arc::default(),
    };
    tokio::spawn(run("probe", server.url.clone(), probe, rx, fast()));
    eventually(|| *server.accepted.lock().unwrap() == 1).await;
    eventually(|| !server.received.lock().unwrap()[0].is_empty()).await;
    server.push.send("event:one".into()).unwrap();
    eventually(|| *seen.lock().unwrap() == ["event:one"]).await;

    tx.send(Change::Unsubscribe("a".into())).unwrap();
    eventually(|| {
        server.received.lock().unwrap()[0]
            .last()
            .is_some_and(|f| f == "unsub:a")
    })
    .await;
    // Closed, and not reconnected while nothing is wanted.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*server.accepted.lock().unwrap(), 1);
    // Wanted again: a new connection, subscribing from scratch.
    tx.send(Change::Subscribe("c".into())).unwrap();
    eventually(|| *server.accepted.lock().unwrap() == 2).await;
    eventually(|| server.received.lock().unwrap()[1] == ["sub:c"]).await;
}

#[tokio::test]
async fn a_dropped_connection_is_remade_and_everything_resubscribed() {
    let server = server().await;
    let (tx, rx) = mpsc::unbounded_channel();
    let opened = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe {
        subs: vec!["a".into()],
        seen: Arc::default(),
        opened: opened.clone(),
        tick: Arc::default(),
        ticks: Arc::default(),
    };
    tokio::spawn(run("probe", server.url.clone(), probe, rx, fast()));
    eventually(|| *server.accepted.lock().unwrap() == 1).await;
    tx.send(Change::Subscribe("b".into())).unwrap();
    eventually(|| server.received.lock().unwrap()[0].len() == 2).await;

    server.push.send("drop".into()).unwrap();
    eventually(|| *server.accepted.lock().unwrap() == 2).await;
    eventually(|| server.received.lock().unwrap()[1].len() == 2).await;
    let mut resent = server.received.lock().unwrap()[1].clone();
    resent.sort();
    assert_eq!(resent, ["sub:a", "sub:b"]);
    assert_eq!(opened.lock().unwrap().len(), 2);
    drop(tx);
}

#[tokio::test]
async fn a_session_that_asks_to_reconnect_gets_a_new_connection() {
    let server = server().await;
    let (_tx, rx) = mpsc::unbounded_channel();
    let probe = Probe {
        subs: vec!["a".into()],
        seen: Arc::default(),
        opened: Arc::default(),
        tick: Arc::default(),
        ticks: Arc::default(),
    };
    tokio::spawn(run("probe", server.url.clone(), probe, rx, fast()));
    eventually(|| *server.accepted.lock().unwrap() == 1).await;
    eventually(|| !server.received.lock().unwrap()[0].is_empty()).await;
    server.push.send("bye".into()).unwrap();
    eventually(|| *server.accepted.lock().unwrap() == 2).await;
}

#[tokio::test]
async fn an_unreachable_server_is_retried_without_giving_up() {
    // Nothing listens on this port: connect fails, the runner keeps trying until told to stop.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    drop(listener);
    let (tx, rx) = mpsc::unbounded_channel();
    let probe = Probe {
        subs: vec!["a".into()],
        seen: Arc::default(),
        opened: Arc::default(),
        tick: Arc::default(),
        ticks: Arc::default(),
    };
    let task = tokio::spawn(run("probe", url, probe, rx, fast()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!task.is_finished());
    drop(tx);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_server_that_greets_and_ends_the_stream_does_not_reset_the_backoff() {
    // Every connection is told "bye" by the session (like 7TV's Hello followed by an end of
    // stream): a frame arriving must not count as a healthy connection.
    let server = server_greeting(Some("bye".to_owned())).await;
    let (_tx, rx) = mpsc::unbounded_channel();
    let probe = Probe {
        subs: vec!["a".into()],
        seen: Arc::default(),
        opened: Arc::default(),
        tick: Arc::default(),
        ticks: Arc::default(),
    };
    let backoff = Backoff {
        initial: Duration::from_millis(100),
        max: Duration::from_secs(10),
        healthy_after: Duration::from_secs(30),
        connect_timeout: Duration::from_secs(2),
    };
    tokio::spawn(run("probe", server.url.clone(), probe, rx, backoff));
    tokio::time::sleep(Duration::from_millis(1300)).await;
    // Waits of ~100, 200, 400, 800 ms: at most five connections in 1.3 s (a reset after every
    // greeting would make it a dozen).
    let connections = *server.accepted.lock().unwrap();
    assert!((3..=5).contains(&connections), "{connections} connections");
}

#[tokio::test]
async fn a_refusal_holds_the_client_off_for_as_long_as_the_server_asked() {
    let server = server_greeting(Some("refused".to_owned())).await;
    let (_tx, rx) = mpsc::unbounded_channel();
    let probe = Probe {
        subs: vec!["a".into()],
        seen: Arc::default(),
        opened: Arc::default(),
        tick: Arc::default(),
        ticks: Arc::default(),
    };
    tokio::spawn(run("probe", server.url.clone(), probe, rx, fast()));
    eventually(|| *server.accepted.lock().unwrap() == 1).await;
    // The usual wait is 20 ms; the refusal asks for 400.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(*server.accepted.lock().unwrap(), 1);
    eventually(|| *server.accepted.lock().unwrap() == 2).await;
}

#[tokio::test]
async fn due_ticks_run_with_no_connection_and_while_waiting_to_reconnect() {
    // Idle (no subscriptions): a pending notice is still written out.
    let (_tx, rx) = mpsc::unbounded_channel();
    let probe = Probe {
        subs: Vec::new(),
        seen: Arc::default(),
        opened: Arc::default(),
        tick: Arc::new(Mutex::new(Some(Instant::now() + Duration::from_millis(30)))),
        ticks: Arc::default(),
    };
    let ticks = probe.ticks.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(run("probe", url.clone(), probe, rx, fast()));
    eventually(|| *ticks.lock().unwrap() == 1).await;

    // Waiting out a long backoff after a failed connect: the same.
    drop(listener);
    let (_tx, rx) = mpsc::unbounded_channel();
    let probe = Probe {
        subs: vec!["a".into()],
        seen: Arc::default(),
        opened: Arc::default(),
        tick: Arc::new(Mutex::new(Some(
            Instant::now() + Duration::from_millis(100),
        ))),
        ticks: Arc::default(),
    };
    let ticks = probe.ticks.clone();
    let slow = Backoff {
        initial: Duration::from_secs(30),
        ..fast()
    };
    tokio::spawn(run("probe", url, probe, rx, slow));
    eventually(|| *ticks.lock().unwrap() == 1).await;
}
