//! Reconnect backoff and the JOIN rate limiter. Pure arithmetic; the supervisor owns
//! the clock and the randomness.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

/// Exponential reconnect backoff with jitter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Backoff {
    /// Delay before the first retry.
    pub initial: Duration,
    /// Upper bound, jitter included.
    pub max: Duration,
    /// Multiplier per further attempt.
    pub factor: f64,
    /// Fraction of the delay randomised in both directions (`0.2` = ±20%).
    pub jitter: f64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            factor: 2.0,
            jitter: 0.2,
        }
    }
}

impl Backoff {
    /// Delay before reconnect `attempt` (1 = first retry). `sample` is a uniform random
    /// number in `0.0..1.0`; pass `0.5` for no jitter.
    pub fn delay(&self, attempt: u32, sample: f64) -> Duration {
        let exponent = i32::try_from(attempt.saturating_sub(1)).unwrap_or(i32::MAX);
        let base = self.initial.as_secs_f64() * self.factor.powi(exponent);
        let spread = 1.0 + self.jitter * (sample.clamp(0.0, 1.0) * 2.0 - 1.0);
        let secs = (base * spread).min(self.max.as_secs_f64()).max(0.0);
        Duration::from_secs_f64(secs)
    }
}

/// How fast JOINs may be sent. `twitch-irc` 6.1.3 only paces new *connections*, not
/// JOINs, so the client does it here. Twitch allows 20 JOINs per 10 s for ordinary
/// accounts; the default stays under that so a fixed server-side window cannot be
/// exceeded by a sliding one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinRateLimit {
    pub max_joins: usize,
    pub per: Duration,
}

impl Default for JoinRateLimit {
    fn default() -> Self {
        Self {
            max_joins: 15,
            per: Duration::from_secs(10),
        }
    }
}

/// Sliding-window limiter for JOINs.
#[derive(Debug)]
pub(crate) struct JoinLimiter {
    limit: JoinRateLimit,
    sent: VecDeque<Instant>,
}

impl JoinLimiter {
    pub fn new(limit: JoinRateLimit) -> Self {
        Self {
            limit,
            sent: VecDeque::new(),
        }
    }

    /// The earliest instant (not before `now`) at which another JOIN may be sent.
    pub fn ready_at(&mut self, now: Instant) -> Instant {
        while self
            .sent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= self.limit.per)
        {
            self.sent.pop_front();
        }
        let max = self.limit.max_joins.max(1);
        if self.sent.len() < max {
            now
        } else {
            // The oldest JOIN that still counts leaves the window `per` after it was sent.
            self.sent[self.sent.len() - max] + self.limit.per
        }
    }

    pub fn record(&mut self, at: Instant) {
        self.sent.push_back(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let b = Backoff {
            jitter: 0.0,
            ..Backoff::default()
        };
        let d = |n| b.delay(n, 0.5).as_secs();
        assert_eq!([d(1), d(2), d(3), d(4)], [1, 2, 4, 8]);
        assert_eq!(d(7), 60);
        assert_eq!(d(500), 60, "no overflow for absurd attempt counts");
    }

    #[test]
    fn backoff_jitter_stays_within_bounds() {
        let b = Backoff::default();
        assert_eq!(b.delay(3, 0.0), Duration::from_secs_f64(4.0 * 0.8));
        assert_eq!(b.delay(3, 1.0), Duration::from_secs_f64(4.0 * 1.2));
        assert!(b.delay(20, 1.0) <= b.max);
    }

    #[tokio::test(start_paused = true)]
    async fn join_limiter_waits_for_the_window() {
        let mut l = JoinLimiter::new(JoinRateLimit {
            max_joins: 2,
            per: Duration::from_secs(10),
        });
        let t0 = Instant::now();
        assert_eq!(l.ready_at(t0), t0);
        l.record(t0);
        l.record(t0 + Duration::from_secs(1));
        assert_eq!(
            l.ready_at(t0 + Duration::from_secs(2)),
            t0 + Duration::from_secs(10)
        );
        // Once the first one expired, one slot is free again.
        let t = t0 + Duration::from_secs(10);
        assert_eq!(l.ready_at(t), t);
    }
}
