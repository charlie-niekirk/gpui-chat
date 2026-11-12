//! `--replay <file> [--replay-speed <x>]`: feed a recorded raw IRC log through the real parser
//! ([`gpui_chat_twitch::parse_line`]) and into the app, offline.
//!
//! The file has one IRC line per line, as written by the Twitch crate's `dump --raw` example (see
//! `fixtures/irc/`); blank lines and `#` comments are ignored by the parser.
//!
//! **Timing.**
//! * Speed `1` follows the log: an event goes out `(tmi_sent_ts - first_ts) / speed` after the
//!   start. A gap between two lines is capped at [`MAX_GAP`] so a log with a long pause does not
//!   look like a hang, and timestamps that run backwards do not (the event goes out with the
//!   previous one). Events without a timestamp (a `NOTICE`, say) go out with the one before them.
//! * A log in which **no** line has `tmi-sent-ts` is replayed at a fixed [`FIXED_RATE`] events per
//!   second (times the speed).
//! * Speed `0` sends as fast as the UI takes events.
//!
//! Like the firehose, each wake-up sends everything that is due and the next sleep is at least
//! [`MIN_TICK`], so a busy log costs one timer wake-up per tick, not one per line. Unlike the
//! firehose, replay is lossless: it waits (`send().await`) when the bridge queue is full, and the
//! schedule catches up afterwards. Lines the parser rejects are counted (`parse` on the HUD) and
//! skipped.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui_chat_core::ChatEvent;
use gpui_chat_twitch::parse_line;
use tokio::time::{Instant, sleep_until};

use crate::bridge::BridgeSender;
use crate::diagnostics::IngestCounters;
use crate::firehose::MIN_TICK;

/// Events per second at speed 1 for a log without timestamps.
pub const FIXED_RATE: f64 = 100.0;

/// Longest pause between two events at speed 1 (before dividing by the speed).
pub const MAX_GAP: Duration = Duration::from_secs(5);

/// Events sent between two explicit yields in as-fast-as-possible mode, so an unbounded replay
/// cannot hold a worker while the bridge queue has room.
const YIELD_EVERY: u64 = 256;

/// How fast to replay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Speed {
    /// No waiting.
    Max,
    /// `1.0` is the log's own pace.
    Factor(f64),
}

impl Speed {
    /// `0` is [`Speed::Max`]. Callers validate that `factor` is finite and not negative.
    pub fn from_factor(factor: f64) -> Self {
        if factor <= 0.0 {
            Self::Max
        } else {
            Self::Factor(factor)
        }
    }
}

/// Milliseconds since the epoch from the `tmi-sent-ts` tag of a raw line, if it has one.
pub fn sent_ts(line: &str) -> Option<i64> {
    let tags = line.strip_prefix('@')?.split(' ').next()?;
    tags.split(';')
        .find_map(|tag| tag.strip_prefix("tmi-sent-ts="))?
        .parse()
        .ok()
}

/// Turns the events of a log into offsets from the start of the replay.
#[derive(Debug)]
pub struct Pacer {
    speed: Speed,
    /// Whether any line of the log carries a timestamp.
    timed: bool,
    last_ts: Option<i64>,
    /// Milliseconds of (gap-capped) log time elapsed.
    log_ms: f64,
    events: u64,
}

impl Pacer {
    pub fn new(speed: Speed, timed: bool) -> Self {
        Self {
            speed,
            timed,
            last_ts: None,
            log_ms: 0.0,
            events: 0,
        }
    }

    /// When the next event goes out, relative to the start. `ts` is its `tmi-sent-ts`.
    pub fn offset(&mut self, ts: Option<i64>) -> Duration {
        let index = self.events;
        self.events += 1;
        let Speed::Factor(speed) = self.speed else {
            return Duration::ZERO;
        };
        if !self.timed {
            return Duration::from_secs_f64(index as f64 / (FIXED_RATE * speed));
        }
        if let Some(ts) = ts {
            if let Some(last) = self.last_ts {
                let gap = (ts - last).clamp(0, MAX_GAP.as_millis() as i64);
                self.log_ms += gap as f64;
            }
            // Backwards timestamps do not move the clock back.
            self.last_ts = Some(self.last_ts.map_or(ts, |last| last.max(ts)));
        }
        Duration::from_secs_f64(self.log_ms / 1000.0 / speed)
    }
}

/// Replays `log` into `sender` and returns when it is done or the UI is gone.
pub async fn run(
    sender: BridgeSender<ChatEvent>,
    log: Arc<str>,
    speed: Speed,
    counters: Arc<IngestCounters>,
) {
    let timed = log.lines().any(|line| sent_ts(line).is_some());
    let mut pacer = Pacer::new(speed, timed);
    let start = Instant::now();
    let mut sent = 0u64;
    for line in log.lines() {
        let event = match parse_line(line) {
            Ok(Some(event)) => event,
            Ok(None) => continue,
            Err(error) => {
                counters.parse_errors.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(%error, line, "replay: skipping a line that did not parse");
                continue;
            }
        };
        let due = start + pacer.offset(sent_ts(line));
        let now = Instant::now();
        if due > now {
            sleep_until(due.max(now + MIN_TICK)).await;
        }
        if sender.send(event).await.is_err() {
            return;
        }
        counters.sent.fetch_add(1, Ordering::Relaxed);
        sent += 1;
        if speed == Speed::Max && sent.is_multiple_of(YIELD_EVERY) {
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge;
    use gpui_chat_core::{ChannelId, ClearTarget};

    fn line(id: &str, ts: Option<i64>) -> String {
        match ts {
            Some(ts) => format!(
                "@id={id};tmi-sent-ts={ts};display-name=A :a!a@a.tmi.twitch.tv PRIVMSG #chan :msg {id}"
            ),
            None => format!("@id={id};display-name=A :a!a@a.tmi.twitch.tv PRIVMSG #chan :msg {id}"),
        }
    }

    fn id_of(event: &ChatEvent) -> String {
        match event {
            ChatEvent::Message { message, .. } => message.id.as_str().to_owned(),
            other => format!("{other:?}"),
        }
    }

    /// Replays `log` under virtual time and returns `(arrival offset, event)` for each event.
    async fn replay(log: &str, speed: Speed) -> (Vec<(Duration, ChatEvent)>, Arc<IngestCounters>) {
        let (tx, mut rx) = bridge::channel::<ChatEvent>(1024, usize::MAX);
        let counters = Arc::new(IngestCounters::default());
        let start = Instant::now();
        let source = tokio::spawn(run(tx, Arc::from(log), speed, counters.clone()));
        let mut out = Vec::new();
        while let Some(first) = rx.first().await {
            let at = Instant::now() - start;
            out.extend(rx.drain(first).into_iter().map(|event| (at, event)));
        }
        source.await.unwrap();
        (out, counters)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn sent_ts_reads_the_tag_and_only_the_tag() {
        assert_eq!(
            sent_ts(&line("1", Some(1_794_510_204_310))),
            Some(1_794_510_204_310)
        );
        assert_eq!(sent_ts(&line("1", None)), None);
        assert_eq!(sent_ts("PING :tmi.twitch.tv"), None);
        assert_eq!(sent_ts("@tmi-sent-ts=oops :a PRIVMSG #c :x"), None);
        // A message that merely contains the text is not a tag.
        assert_eq!(sent_ts("@id=1 :a!a@a PRIVMSG #c :tmi-sent-ts=5"), None);
        // Another tag that ends the same way is not it.
        assert_eq!(sent_ts("@x-tmi-sent-ts=5;id=1 :a PRIVMSG #c :x"), None);
    }

    #[test]
    fn pacer_follows_the_log_scales_by_speed_and_caps_gaps() {
        let times = |speed, stamps: &[Option<i64>]| {
            let mut pacer = Pacer::new(speed, true);
            stamps
                .iter()
                .map(|ts| pacer.offset(*ts))
                .collect::<Vec<_>>()
        };
        let stamps = [Some(10_000), Some(10_500), None, Some(10_500), Some(12_000)];
        assert_eq!(
            times(Speed::Factor(1.0), &stamps),
            [ms(0), ms(500), ms(500), ms(500), ms(2000)]
        );
        assert_eq!(
            times(Speed::Factor(2.0), &stamps),
            [ms(0), ms(250), ms(250), ms(250), ms(1000)]
        );
        assert_eq!(times(Speed::Factor(0.5), &stamps)[4], ms(4000));
        assert_eq!(times(Speed::Max, &stamps), [Duration::ZERO; 5]);

        // A long pause is capped, a timestamp running backwards does not rewind, and the
        // next one is measured from the highest seen.
        assert_eq!(
            times(
                Speed::Factor(1.0),
                &[Some(0), Some(3_600_000), Some(3_000_000), Some(3_600_100)]
            ),
            [ms(0), ms(5000), ms(5000), ms(5100)]
        );
        // Lines before the first timestamp go out at the start.
        assert_eq!(
            times(Speed::Factor(1.0), &[None, Some(100), Some(200)]),
            [ms(0), ms(0), ms(100)]
        );
    }

    #[test]
    fn untimed_logs_use_the_fixed_rate() {
        let mut pacer = Pacer::new(Speed::Factor(1.0), false);
        let offsets: Vec<_> = (0..4).map(|_| pacer.offset(None)).collect();
        assert_eq!(offsets, [ms(0), ms(10), ms(20), ms(30)]);
        let mut fast = Pacer::new(Speed::Factor(10.0), false);
        assert_eq!(fast.offset(None), ms(0));
        assert_eq!(fast.offset(None), ms(1));
    }

    #[tokio::test(start_paused = true)]
    async fn events_arrive_in_order_at_the_logged_times() {
        let log = [
            line("a", Some(1_000)),
            "PING :tmi.twitch.tv".to_owned(),
            line("b", Some(1_500)),
            "# a comment".to_owned(),
            String::new(),
            line("c", Some(1_500)),
            line("d", Some(3_000)),
        ]
        .join("\n");
        let (events, counters) = replay(&log, Speed::Factor(1.0)).await;
        let ids: Vec<_> = events.iter().map(|(_, e)| id_of(e)).collect();
        assert_eq!(
            ids,
            ["a", "b", "c", "d"],
            "order preserved, non-chat lines skipped"
        );
        assert_eq!(counters.parse_errors(), 0);
        assert_eq!(counters.sent(), 4);

        // Never early; late by at most the tick plus the timer's 1 ms rounding.
        let due = [ms(0), ms(500), ms(500), ms(2000)];
        for ((at, event), due) in events.iter().zip(due) {
            assert!(*at >= due, "{} early: {at:?} < {due:?}", id_of(event));
            assert!(
                *at <= due + MIN_TICK + ms(1),
                "{} late: {at:?} vs {due:?}",
                id_of(event)
            );
        }
        // b and c are due together, so they go out in one wake-up.
        assert_eq!(events[1].0, events[2].0);
    }

    #[tokio::test(start_paused = true)]
    async fn speed_scales_and_zero_is_immediate() {
        let log = [
            line("a", Some(0)),
            line("b", Some(4_000)),
            line("c", Some(8_000)),
        ]
        .join("\n");

        let (events, _) = replay(&log, Speed::Factor(4.0)).await;
        let at: Vec<_> = events.iter().map(|(at, _)| *at).collect();
        assert!(at[1] >= ms(1000) && at[1] <= ms(1003), "{at:?}");
        assert!(at[2] >= ms(2000) && at[2] <= ms(2003), "{at:?}");

        let (events, _) = replay(&log, Speed::Max).await;
        assert_eq!(events.len(), 3);
        assert!(
            events.iter().all(|(at, _)| *at == Duration::ZERO),
            "as fast as possible does not wait"
        );
        assert_eq!(
            events.iter().map(|(_, e)| id_of(e)).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_log_without_timestamps_runs_at_the_fixed_rate() {
        let log: Vec<String> = (0..5).map(|n| line(&n.to_string(), None)).collect();
        let (events, _) = replay(&log.join("\n"), Speed::Factor(1.0)).await;
        assert_eq!(events.len(), 5);
        let last = events.last().unwrap().0;
        assert!(
            last >= ms(40) && last <= ms(44),
            "5 events at 100/s end near 40 ms: {last:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn parse_errors_are_counted_and_do_not_stop_the_replay() {
        let log = [
            line("a", Some(0)),
            "@id=x :a!a@a PRIVMSG".to_owned(), // PRIVMSG without a channel or text
            "@broken tags only".to_owned(),
            line("b", Some(10)),
        ]
        .join("\n");
        let (events, counters) = replay(&log, Speed::Max).await;
        assert_eq!(
            events.iter().map(|(_, e)| id_of(e)).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(counters.parse_errors() >= 1, "{}", counters.parse_errors());
        assert_eq!(counters.sent(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_replay_stops_when_the_ui_is_gone() {
        let log: String = (0..100)
            .map(|n| line(&n.to_string(), Some(n * 10)) + "\n")
            .collect();
        let (tx, rx) = bridge::channel::<ChatEvent>(4, 4);
        let counters = Arc::new(IngestCounters::default());
        let source = tokio::spawn(run(
            tx,
            Arc::from(log.as_str()),
            Speed::Max,
            counters.clone(),
        ));
        drop(rx);
        tokio::time::timeout(ms(100), source)
            .await
            .expect("replay ends")
            .unwrap();
        assert!(counters.sent() < 100);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_queue_makes_replay_wait_not_drop() {
        let log: String = (0..50)
            .map(|n| line(&n.to_string(), Some(n)) + "\n")
            .collect();
        let (tx, mut rx) = bridge::channel::<ChatEvent>(4, 4);
        let counters = Arc::new(IngestCounters::default());
        let source = tokio::spawn(run(
            tx,
            Arc::from(log.as_str()),
            Speed::Max,
            counters.clone(),
        ));
        let mut ids = Vec::new();
        while let Some(first) = rx.first().await {
            ids.extend(rx.drain(first).iter().map(id_of));
        }
        source.await.unwrap();
        let expected: Vec<String> = (0..50).map(|n| n.to_string()).collect();
        assert_eq!(
            ids, expected,
            "lossless and in order through a 4-event queue"
        );
        assert_eq!(counters.dropped(), 0);
    }

    // ---- the recorded fixtures ---------------------------------------------------------------

    fn fixture(name: &str) -> String {
        let path = format!("{}/../../fixtures/irc/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    #[tokio::test(start_paused = true)]
    async fn the_recorded_privmsg_fixture_replays_as_the_parser_reads_it() {
        let log = fixture("privmsg.irc");
        let expected: Vec<ChatEvent> = log
            .lines()
            .filter_map(|line| parse_line(line).ok().flatten())
            .collect();
        assert!(
            expected.len() >= 15,
            "{} events in the fixture",
            expected.len()
        );

        let (events, counters) = replay(&log, Speed::Max).await;
        let got: Vec<ChatEvent> = events.into_iter().map(|(_, event)| event).collect();
        assert_eq!(got, expected, "same events, same order");
        assert_eq!(counters.parse_errors(), 0);

        // Paced at 1x the recorded timestamps, the whole fixture takes as long as its span
        // (gaps capped), and the order is still the file's.
        let (timed, _) = replay(&log, Speed::Factor(1.0)).await;
        assert_eq!(timed.len(), expected.len());
        assert!(
            timed.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "arrivals never go back"
        );
        assert_eq!(
            timed.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(),
            expected
        );
    }

    #[tokio::test(start_paused = true)]
    async fn moderation_fixture_reaches_the_events_the_stores_expect() {
        let (events, _) = replay(&fixture("clearchat.irc"), Speed::Max).await;
        assert!(events.iter().any(|(_, e)| matches!(
            e,
            ChatEvent::ClearChat {
                target: ClearTarget::Channel,
                ..
            }
        )));
        assert!(
            events
                .iter()
                .all(|(_, e)| e.channel() != Some(&ChannelId::new("")))
        );
    }
}
