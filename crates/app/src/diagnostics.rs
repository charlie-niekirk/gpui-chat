//! Frame and ingest measurements behind the debug HUD.
//!
//! * **Draw time** is measured by [`FrameProbe`], an element wrapped around the whole window
//!   content: it stamps `Instant::now()` when layout is requested (the start of the draw, which
//!   is when the shell's views are rendered into elements) and again when its subtree has been
//!   painted. That covers render, layout, prepaint and paint; it does not cover handing the scene
//!   to the GPU (`Window::present`), which GPUI does after `draw` and offers no hook for outside
//!   its `profiler` feature.
//! * **Frame interval** is the time between the starts of consecutive draws. Gaps over
//!   [`IDLE_GAP`] are idle time, not slow frames, and are left out.
//! * **Ingest** is recorded by the bridge `apply` callback, which runs at the start of a frame, so
//!   its timestamps are frame timestamps.
//!
//! Recording never schedules a frame: the HUD reads these numbers when it renders.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui_kit::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};

/// One frame at 120 Hz, the draw budget of the performance table in `docs/ARCHITECTURE.md`.
pub const FRAME_BUDGET: Duration = Duration::from_micros(8_333);

/// Draws kept for the percentiles (two seconds at 120 Hz).
const FRAME_WINDOW: usize = 240;

/// Gap between draws above which the app was idle rather than slow.
pub const IDLE_GAP: Duration = Duration::from_millis(100);

/// Trailing window of the messages-per-second figure.
const INGEST_WINDOW: Duration = Duration::from_secs(1);

/// Nearest-rank percentile of `sorted` (ascending), `q` in `0.0..=1.0`.
fn percentile(sorted: &[Duration], q: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn sorted(values: &VecDeque<Duration>) -> Vec<Duration> {
    let mut out: Vec<Duration> = values.iter().copied().collect();
    out.sort_unstable();
    out
}

fn push_capped<T>(queue: &mut VecDeque<T>, value: T, cap: usize) {
    if queue.len() >= cap {
        queue.pop_front();
    }
    queue.push_back(value);
}

/// What the HUD shows about frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameSummary {
    /// Draws seen since startup.
    pub total_draws: u64,
    /// Draws in the rolling window.
    pub window_draws: usize,
    pub last: Duration,
    pub p50: Duration,
    pub p99: Duration,
    pub max: Duration,
    /// Draws in the window slower than [`FRAME_BUDGET`].
    pub over_budget: usize,
    /// Median / 99th percentile time between consecutive draws while busy.
    pub interval_p50: Duration,
    pub interval_p99: Duration,
    /// Draws that started within the last second.
    pub draws_last_second: usize,
}

/// Rolling draw times and intervals.
#[derive(Debug, Default)]
pub struct FrameStats {
    draws: VecDeque<Duration>,
    intervals: VecDeque<Duration>,
    last_start: Option<Instant>,
    /// Start times of the draws in the last second (for the draws-per-second figure).
    recent_starts: VecDeque<Instant>,
    total_draws: u64,
}

impl FrameStats {
    /// Records one draw that started at `start` and ended at `end`.
    pub fn record_draw(&mut self, start: Instant, end: Instant) {
        self.total_draws += 1;
        push_capped(
            &mut self.draws,
            end.saturating_duration_since(start),
            FRAME_WINDOW,
        );
        if let Some(previous) = self.last_start {
            let gap = start.saturating_duration_since(previous);
            if gap <= IDLE_GAP {
                push_capped(&mut self.intervals, gap, FRAME_WINDOW);
            }
        }
        self.last_start = Some(start);
        self.recent_starts.push_back(start);
        while self
            .recent_starts
            .front()
            .is_some_and(|t| start.saturating_duration_since(*t) > Duration::from_secs(1))
        {
            self.recent_starts.pop_front();
        }
    }

    pub fn total_draws(&self) -> u64 {
        self.total_draws
    }

    /// `now` anchors the draws-per-second figure, so an idle app reads 0 rather than the rate
    /// of its last burst.
    pub fn summary(&self, now: Instant) -> FrameSummary {
        let draws = sorted(&self.draws);
        let intervals = sorted(&self.intervals);
        FrameSummary {
            total_draws: self.total_draws,
            window_draws: draws.len(),
            last: self.draws.back().copied().unwrap_or_default(),
            p50: percentile(&draws, 0.50),
            p99: percentile(&draws, 0.99),
            max: draws.last().copied().unwrap_or_default(),
            over_budget: draws.iter().filter(|d| **d > FRAME_BUDGET).count(),
            interval_p50: percentile(&intervals, 0.50),
            interval_p99: percentile(&intervals, 0.99),
            draws_last_second: self
                .recent_starts
                .iter()
                .filter(|t| now.saturating_duration_since(**t) <= Duration::from_secs(1))
                .count(),
        }
    }
}

/// What the HUD shows about ingest.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct IngestSummary {
    /// Events applied over the trailing second.
    pub per_second: f64,
    /// Size of the most recent batch and the largest one in the trailing second.
    pub last_batch: usize,
    pub max_batch: usize,
    pub total_events: u64,
    pub total_batches: u64,
}

/// Events applied per frame batch.
#[derive(Debug, Default)]
pub struct IngestStats {
    batches: VecDeque<(Instant, usize)>,
    last_batch: usize,
    total_events: u64,
    total_batches: u64,
}

impl IngestStats {
    /// Records a batch of `events` applied at `at` (a frame timestamp).
    pub fn record_batch(&mut self, at: Instant, events: usize) {
        self.total_events += events as u64;
        self.total_batches += 1;
        self.last_batch = events;
        self.batches.push_back((at, events));
        // Anything older than the window can never count again; keep the queue short.
        while self
            .batches
            .front()
            .is_some_and(|(t, _)| at.saturating_duration_since(*t) > INGEST_WINDOW)
        {
            self.batches.pop_front();
        }
    }

    pub fn total_batches(&self) -> u64 {
        self.total_batches
    }

    pub fn summary(&self, now: Instant) -> IngestSummary {
        let recent = || {
            self.batches
                .iter()
                .filter(|(t, _)| now.saturating_duration_since(*t) <= INGEST_WINDOW)
        };
        let events: usize = recent().map(|(_, n)| n).sum();
        IngestSummary {
            per_second: events as f64 / INGEST_WINDOW.as_secs_f64(),
            last_batch: self.last_batch,
            max_batch: recent().map(|(_, n)| *n).max().unwrap_or(0),
            total_events: self.total_events,
            total_batches: self.total_batches,
        }
    }
}

/// Counters a producer thread updates and the HUD reads.
#[derive(Debug, Default)]
pub struct IngestCounters {
    /// Events handed to the bridge.
    pub sent: AtomicU64,
    /// Events a producer discarded because the bridge queue was full (the UI was behind).
    pub dropped: AtomicU64,
}

impl IngestCounters {
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

struct Inner {
    frames: FrameStats,
    ingest: IngestStats,
}

/// Handle to the measurements; cheap to clone, main thread only (except [`Self::counters`]).
#[derive(Clone)]
pub struct Diagnostics {
    inner: Rc<RefCell<Inner>>,
    counters: Arc<IngestCounters>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new()
    }
}

impl Diagnostics {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(Inner {
                frames: FrameStats::default(),
                ingest: IngestStats::default(),
            })),
            counters: Arc::default(),
        }
    }

    /// Shared with producer threads (the firehose) to report sent and dropped events.
    pub fn counters(&self) -> Arc<IngestCounters> {
        self.counters.clone()
    }

    pub fn record_draw(&self, start: Instant, end: Instant) {
        self.inner.borrow_mut().frames.record_draw(start, end);
    }

    pub fn record_batch(&self, at: Instant, events: usize) {
        self.inner.borrow_mut().ingest.record_batch(at, events);
    }

    pub fn frame_summary(&self, now: Instant) -> FrameSummary {
        self.inner.borrow().frames.summary(now)
    }

    pub fn ingest_summary(&self, now: Instant) -> IngestSummary {
        self.inner.borrow().ingest.summary(now)
    }

    pub fn total_draws(&self) -> u64 {
        self.inner.borrow().frames.total_draws()
    }

    pub fn total_batches(&self) -> u64 {
        self.inner.borrow().ingest.total_batches()
    }
}

/// Wraps `child` and records how long drawing it took (see the module docs).
pub fn frame_probe(diagnostics: Diagnostics, child: impl IntoElement) -> FrameProbe {
    FrameProbe {
        diagnostics,
        child: child.into_any_element(),
    }
}

/// Element that times the draw of its subtree. It adds no layout of its own.
pub struct FrameProbe {
    diagnostics: Diagnostics,
    child: AnyElement,
}

impl IntoElement for FrameProbe {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for FrameProbe {
    /// When the draw reached this element.
    type RequestLayoutState = Instant;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Instant) {
        let start = Instant::now();
        (self.child.request_layout(window, cx), start)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _start: &mut Instant,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        start: &mut Instant,
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        self.diagnostics.record_draw(*start, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let values: Vec<Duration> = (1..=100).map(ms).collect();
        assert_eq!(percentile(&values, 0.50), ms(50));
        assert_eq!(percentile(&values, 0.99), ms(99));
        assert_eq!(percentile(&values, 1.0), ms(100));
        assert_eq!(percentile(&values, 0.0), ms(1));
        assert_eq!(percentile(&[], 0.5), Duration::ZERO);
        assert_eq!(percentile(&[ms(7)], 0.99), ms(7));
    }

    #[test]
    fn frame_stats_track_draw_time_budget_and_interval() {
        let t0 = Instant::now();
        let mut stats = FrameStats::default();
        // Ten draws, 8 ms apart, each taking 2 ms; the last one overruns the budget.
        for i in 0..10u64 {
            let start = t0 + ms(i * 8);
            let took = if i == 9 { ms(12) } else { ms(2) };
            stats.record_draw(start, start + took);
        }
        let s = stats.summary(t0 + ms(100));
        assert_eq!(s.total_draws, 10);
        assert_eq!(s.window_draws, 10);
        assert_eq!(s.last, ms(12));
        assert_eq!(s.p50, ms(2));
        assert_eq!(s.p99, ms(12));
        assert_eq!(s.max, ms(12));
        assert_eq!(s.over_budget, 1);
        assert_eq!(s.interval_p50, ms(8));
        assert_eq!(s.draws_last_second, 10);
        // A while later, with nothing drawn since, the rate reads zero.
        assert_eq!(stats.summary(t0 + ms(5_000)).draws_last_second, 0);
    }

    #[test]
    fn idle_gaps_are_not_frame_intervals() {
        let t0 = Instant::now();
        let mut stats = FrameStats::default();
        stats.record_draw(t0, t0 + ms(1));
        stats.record_draw(t0 + ms(8), t0 + ms(9));
        stats.record_draw(t0 + ms(5_000), t0 + ms(5_001)); // user came back after five seconds
        let s = stats.summary(Instant::now());
        assert_eq!(s.total_draws, 3);
        assert_eq!(
            s.interval_p99,
            ms(8),
            "the 5 s gap is idle, not a slow frame"
        );
    }

    #[test]
    fn frame_window_is_bounded() {
        let t0 = Instant::now();
        let mut stats = FrameStats::default();
        for i in 0..(FRAME_WINDOW as u64 * 3) {
            let start = t0 + ms(i);
            stats.record_draw(start, start + ms(1));
        }
        let s = stats.summary(Instant::now());
        assert_eq!(s.window_draws, FRAME_WINDOW);
        assert_eq!(s.total_draws, FRAME_WINDOW as u64 * 3);
    }

    #[test]
    fn ingest_rate_covers_the_trailing_second() {
        let t0 = Instant::now();
        let mut stats = IngestStats::default();
        // 8 events per 8 ms frame for 2 seconds = 1000/s.
        for i in 0..250u64 {
            stats.record_batch(t0 + ms(i * 8), 8);
        }
        let now = t0 + ms(249 * 8);
        let s = stats.summary(now);
        assert!((s.per_second - 1000.0).abs() <= 16.0, "{}", s.per_second);
        assert_eq!(s.last_batch, 8);
        assert_eq!(s.max_batch, 8);
        assert_eq!(s.total_events, 2000);
        assert_eq!(s.total_batches, 250);
        assert!(stats.batches.len() <= 130, "old batches are pruned");

        // Later, with nothing new, the rate decays to zero but the totals stay.
        let later = stats.summary(now + ms(1500));
        assert_eq!(later.per_second, 0.0);
        assert_eq!(later.max_batch, 0);
        assert_eq!(later.total_events, 2000);
    }

    #[test]
    fn counters_are_shared() {
        let diagnostics = Diagnostics::new();
        let counters = diagnostics.counters();
        counters.dropped.fetch_add(3, Ordering::Relaxed);
        counters.sent.fetch_add(10, Ordering::Relaxed);
        assert_eq!(diagnostics.counters().dropped(), 3);
        assert_eq!(diagnostics.counters().sent(), 10);
    }
}
