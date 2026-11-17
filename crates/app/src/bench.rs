//! `--bench <name>`: the performance budgets of `docs/ARCHITECTURE.md` as repeatable, offline
//! scenarios (PR 6.1). Each one feeds the real app synthetic chat (the firehose generator, with
//! paints, third-party badges and 7TV / BTTV emotes resolved as the live `EmoteService` does),
//! serves every image from [`SyntheticFetcher`] (no network, no disk cache: the same bytes every
//! run), drives the window, prints one `BENCH {json}` line on stdout and quits.
//! `scripts/bench.sh` runs them all in an isolated `HOME` and checks the numbers against the
//! budgets.
//!
//! | name | what runs | what is measured |
//! |---|---|---|
//! | `scroll` | 1 channel at 100 msg/s after a full scrollback; a scroll event every 8 ms, direction flipping every second | apply + draw p99 (the HUD's frame total) |
//! | `burst` | 1 channel at 1 000 msg/s; a scroll event every 16 ms | input latency (event due → end of the draw showing it) p99, frame total p99, dropped events |
//! | `idle` | 3 channels × 1 000 messages, static images only, then nothing | CPU over 10 s, draws in that time |
//! | `memory` | 10 channels × 1 000 messages (the default scrollback), each channel shown while its messages arrive (2 000 msg/s) and until they are all applied, drawn and their images loaded, as in a watched chat | resident size and physical footprint |
//! | `images` | 1 channel whose messages are all distinct animated emotes, scrolled end to end | sanity check: decoded bytes within the cache's budget; footprint |
//! | `launch` | the offline startup path | process start (`GPUI_CHAT_BENCH_T0_NS`, else `main`) → end of the first draw |
//!
//! Numbers that come from outside the process (resident size, footprint, CPU time) are read with
//! `ps` and `footprint`, which every macOS has.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_chat_media::{FetchError, FetchFuture, Fetcher};
use gpui_kit::{
    AnyWindowHandle, App, AsyncApp, Entity, Modifiers, Pixels, PlatformInput, ScrollDelta,
    ScrollEvent, TouchPhase, point, px,
};

use crate::bridge::BridgeSender;
use crate::channels::ChannelHub;
use crate::diagnostics::{Diagnostics, IngestCounters};
use crate::firehose::{self, FirehoseConfig, Generator, ThirdPartyEmotes, WorkLimits};
use gpui_chat_core::ChatEvent;

/// A scenario of `--bench`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchKind {
    Scroll,
    Burst,
    Idle,
    Memory,
    Images,
    Launch,
}

impl BenchKind {
    pub const NAMES: [&'static str; 6] = ["scroll", "burst", "idle", "memory", "images", "launch"];

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "scroll" => Self::Scroll,
            "burst" => Self::Burst,
            "idle" => Self::Idle,
            "memory" => Self::Memory,
            "images" => Self::Images,
            "launch" => Self::Launch,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Scroll => "scroll",
            Self::Burst => "burst",
            Self::Idle => "idle",
            Self::Memory => "memory",
            Self::Images => "images",
            Self::Launch => "launch",
        }
    }

    /// The synthetic chat of the scenario.
    fn plan(self) -> Plan {
        let base = Plan {
            channels: 1,
            prefill_per_channel: 1_000,
            rate: None,
            animate: true,
            distinct_emotes: false,
            fed_by_driver: false,
        };
        match self {
            Self::Scroll => Plan {
                rate: Some(100.0),
                ..base
            },
            Self::Burst => Plan {
                rate: Some(1_000.0),
                ..base
            },
            Self::Idle => Plan {
                channels: 3,
                animate: false,
                ..base
            },
            Self::Memory => Plan {
                channels: 10,
                fed_by_driver: true,
                ..base
            },
            Self::Images => Plan {
                prefill_per_channel: 600,
                distinct_emotes: true,
                ..base
            },
            Self::Launch => Plan {
                prefill_per_channel: 0,
                ..base
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Plan {
    channels: usize,
    /// Messages put in each channel before anything is measured (`chat.scrollback` is 1 000 by
    /// default, so 1 000 is a full channel).
    prefill_per_channel: usize,
    /// Messages per second after the prefill; `None` stops there.
    rate: Option<f64>,
    /// Some images are animated (the synthetic fetcher serves stills only when `false`).
    animate: bool,
    /// Every message is a run of emotes no other message has (the `images` scenario).
    distinct_emotes: bool,
    /// The driver feeds one channel at a time while it is shown (the `memory` scenario) instead
    /// of the source filling every channel up front.
    fed_by_driver: bool,
}

// ---- process clock ------------------------------------------------------------------------

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Call first thing in `main`: the launch scenario counts from here when the script gives no
/// exec time.
pub fn mark_process_start() {
    PROCESS_START.get_or_init(Instant::now);
}

// ---- synthetic images ---------------------------------------------------------------------

/// Serves a deterministic image for any URL, sized like the provider's variant the URL names
/// (Twitch `…/2.0` is 56 px, 7TV / BTTV `…/2x.webp` 64 px, badges 36 px, …). A few shapes are
/// encoded once (PNG for stills, GIF for animations) and handed out by size, so every URL still
/// decodes into its own image, as in the real app.
pub struct SyntheticFetcher {
    animate: bool,
    encoded: Mutex<HashMap<Shape, Arc<Vec<u8>>>>,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct Shape {
    width: u32,
    height: u32,
    frames: u32,
}

impl SyntheticFetcher {
    pub fn new(animate: bool) -> Arc<Self> {
        Arc::new(Self {
            animate,
            encoded: Mutex::new(HashMap::new()),
        })
    }

    /// The image `url` gets: its pixel size from the variant in the URL, animation from the
    /// synthetic id (`…a…` in the bench's 7TV / BTTV ids, every eighth Twitch emote).
    fn shape(&self, url: &str) -> Shape {
        let last = url.rsplit('/').next().unwrap_or_default();
        let factor = match last.trim_end_matches(".webp").trim_end_matches(".png") {
            "1x" | "1.0" | "1" => 1,
            "3.0" | "4x" | "3" => 4,
            "3x" => 3,
            _ => 2,
        };
        let id = url.rsplit('/').nth(1).unwrap_or_default();
        let badge = url.contains("/badge");
        let base = if badge {
            18
        } else if url.contains("jtvnw.net/emoticons") {
            28
        } else {
            32
        };
        let wide = id.starts_with("bench") && id.ends_with('w');
        // The bench's ids end in `a`/`s` (animated / still) then `w`/`n` (wide / not).
        let animated = self.animate
            && !badge
            && if id.starts_with("bench") {
                id.chars().rev().nth(1) == Some('a')
            } else {
                hash(url).is_multiple_of(8)
            };
        let frames = if animated {
            if hash(url).is_multiple_of(2) { 24 } else { 48 }
        } else {
            1
        };
        Shape {
            width: base * factor * if wide { 2 } else { 1 },
            height: base * factor,
            frames,
        }
    }

    fn bytes(&self, shape: Shape) -> Result<Arc<Vec<u8>>, FetchError> {
        let mut encoded = self.encoded.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(bytes) = encoded.get(&shape) {
            return Ok(bytes.clone());
        }
        let bytes = Arc::new(encode(shape).map_err(|e| FetchError::Network(e.to_string().into()))?);
        encoded.insert(shape, bytes.clone());
        Ok(bytes)
    }
}

impl Fetcher for SyntheticFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        Box::pin(async move {
            let shape = self.shape(url);
            self.bytes(shape).map(|bytes| bytes.to_vec())
        })
    }
}

fn hash(text: &str) -> u64 {
    // FNV-1a: stable across runs and platforms.
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A still (PNG) or an animation (GIF, 50 ms frames) of `shape` with some structure in it.
fn encode(shape: Shape) -> image::ImageResult<Vec<u8>> {
    use image::ImageEncoder as _;
    let frame = |n: u32| {
        image::RgbaImage::from_fn(shape.width, shape.height, |x, y| {
            let v = ((x + y + n * 7) % 32) as u8 * 8;
            let alpha = if (x + y) % 11 == 0 { 0 } else { 255 };
            image::Rgba([v, 255 - v, (n * 13 % 255) as u8, alpha])
        })
    };
    let mut out = Vec::new();
    if shape.frames <= 1 {
        let still = frame(0);
        image::codecs::png::PngEncoder::new(&mut out).write_image(
            still.as_raw(),
            shape.width,
            shape.height,
            image::ExtendedColorType::Rgba8,
        )?;
    } else {
        let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(&mut out, 30);
        encoder.set_repeat(image::codecs::gif::Repeat::Infinite)?;
        for n in 0..shape.frames {
            encoder.encode_frame(image::Frame::from_parts(
                frame(n),
                0,
                0,
                image::Delay::from_numer_denom_ms(50, 1),
            ))?;
        }
        drop(encoder);
    }
    Ok(out)
}

/// The image fetcher for a scenario.
pub fn fetcher(kind: BenchKind) -> Arc<dyn Fetcher> {
    SyntheticFetcher::new(kind.plan().animate)
}

// ---- the source ---------------------------------------------------------------------------

/// Starts the scenario's synthetic chat on the backend runtime.
pub fn start_source(
    kind: BenchKind,
    sender: BridgeSender<ChatEvent>,
    counters: Arc<IngestCounters>,
    cx: &mut App,
) {
    let plan = kind.plan();
    if plan.fed_by_driver || (plan.prefill_per_channel == 0 && plan.rate.is_none()) {
        return;
    }
    let channels = firehose::channel_names(plan.channels);
    let cosmetics = cx.global::<crate::badges::Badges>().cosmetics().clone();
    firehose::seed_cosmetics(&cosmetics);
    let config = FirehoseConfig {
        rate: plan.rate.unwrap_or(1_000.0),
        seed: crate::cli::DEFAULT_SEED,
        third_party: Some(Arc::new(if plan.distinct_emotes {
            ThirdPartyEmotes::distinct(&channels)
        } else {
            ThirdPartyEmotes::synthetic(&channels)
        })),
        channels,
        start_ts_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64),
        cosmetics: Some(cosmetics),
    };
    let prefill = plan.prefill_per_channel * plan.channels;
    drop(crate::runtime::spawn_backend(cx, async move {
        let mut generator = Generator::new(&config);
        if plan.distinct_emotes {
            generator.set_emote_only(true);
        }
        // The skew that makes some channels busier would leave the quiet ones short of a full
        // scrollback, so the prefill deals messages out evenly.
        let mut per_channel = vec![0usize; plan.channels];
        while per_channel.iter().sum::<usize>() < prefill {
            let event = generator.next_event();
            let Some(index) = event
                .channel()
                .and_then(|c| config.channels.iter().position(|known| known == c))
            else {
                continue;
            };
            if per_channel[index] >= plan.prefill_per_channel {
                continue;
            }
            per_channel[index] += 1;
            if sender.send(event).await.is_err() {
                return;
            }
            counters.sent.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(rate) = plan.rate {
            firehose::drive(sender, generator, rate, counters, WorkLimits::default()).await;
        }
    }));
}

// ---- the driver ---------------------------------------------------------------------------

/// What the driver needs from the app.
pub struct Driver {
    pub kind: BenchKind,
    pub window: AnyWindowHandle,
    pub hub: Entity<ChannelHub>,
    pub diagnostics: Diagnostics,
    /// Where the driver's own messages go (the `memory` scenario).
    pub sender: BridgeSender<ChatEvent>,
}

/// One result line: `BENCH {"bench":"memory","rss_mb":123.4,...}`.
#[derive(Default)]
struct Report {
    fields: Vec<(&'static str, String)>,
}

impl Report {
    fn num(&mut self, key: &'static str, value: f64) -> &mut Self {
        let text = if value.is_finite() {
            format!("{value:.3}")
        } else {
            "null".to_owned()
        };
        self.fields.push((key, text));
        self
    }

    fn ms(&mut self, key: &'static str, value: Duration) -> &mut Self {
        self.num(key, value.as_secs_f64() * 1_000.0)
    }

    fn mb(&mut self, key: &'static str, bytes: Option<u64>) -> &mut Self {
        self.num(
            key,
            bytes.map_or(f64::NAN, |b| b as f64 / (1024.0 * 1024.0)),
        )
    }

    fn text(&mut self, key: &'static str, value: &str) -> &mut Self {
        let escaped: String = value
            .chars()
            .flat_map(|c| match c {
                '"' | '\\' => vec!['\\', c],
                c if c.is_control() => vec![' '],
                c => vec![c],
            })
            .collect();
        self.fields.push((key, format!("\"{escaped}\"")));
        self
    }

    fn line(&self) -> String {
        let mut out = String::from("BENCH {");
        for (i, (key, value)) in self.fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{key}\":{value}");
        }
        out.push('}');
        out
    }
}

impl Driver {
    /// Runs the scenario on the UI thread's executor; prints the report and quits.
    pub fn start(self, cx: &mut App) {
        cx.spawn(async move |cx| {
            let mut report = Report::default();
            report.text("bench", self.kind.name());
            if let Err(error) = self.run(&mut report, cx).await {
                report.text("error", &error);
            }
            let line = report.line();
            // The scenario's output, read by scripts/bench.sh.
            #[allow(clippy::print_stdout)]
            {
                println!("{line}");
            }
            // Keeps the process up after measuring, for `footprint`, `vmmap`, `heap` or
            // Instruments to look at the same state.
            if let Some(hold) = std::env::var("GPUI_CHAT_BENCH_HOLD_S")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
            {
                tracing::info!(pid = std::process::id(), hold, "bench: holding before quit");
                Self::sleep(cx, Duration::from_secs(hold)).await;
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    async fn run(&self, report: &mut Report, cx: &mut AsyncApp) -> Result<(), String> {
        let first_draw = self
            .until(cx, Duration::from_secs(20), |d, _| d.first_draw_end())
            .await?;
        if self.kind == BenchKind::Launch {
            return self.launch(report, first_draw);
        }
        let plan = self.kind.plan();
        if plan.fed_by_driver {
            self.feed_shown_channels(plan, cx).await?;
        }
        let expected = plan.prefill_per_channel * plan.channels;
        let hub = self.hub.clone();
        self.until(cx, Duration::from_secs(60), move |_, cx| {
            let held: usize = hub.read(cx).sizes(cx).iter().map(|s| s.len).sum();
            (held >= expected).then_some(())
        })
        .await?;
        self.settle_images(cx).await?;
        match self.kind {
            BenchKind::Scroll => self.scroll(report, cx, Duration::from_millis(8)).await,
            BenchKind::Burst => self.scroll(report, cx, Duration::from_millis(16)).await,
            BenchKind::Idle => self.idle(report, cx).await,
            BenchKind::Memory => self.memory(report, cx).await,
            BenchKind::Images => self.images(report, cx).await,
            BenchKind::Launch => Ok(()),
        }
    }

    /// Polls `check` every 10 ms until it gives a value, or fails after `limit`.
    async fn until<T>(
        &self,
        cx: &mut AsyncApp,
        limit: Duration,
        mut check: impl FnMut(&Diagnostics, &App) -> Option<T>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(value) = cx.update(|cx| check(&self.diagnostics, cx)) {
                return Ok(value);
            }
            if Instant::now() > deadline {
                let (draws, held, channels, images) = cx.update(|cx| {
                    let sizes = self.hub.read(cx).sizes(cx);
                    (
                        self.diagnostics.total_draws(),
                        sizes.iter().map(|s| s.len).sum::<usize>(),
                        sizes.len(),
                        image_stats(cx).map_or(0, |s| s.in_flight),
                    )
                });
                return Err(format!(
                    "timed out after {limit:?} ({draws} draws, {held} messages in \
                     {channels} channels, {images} image loads in flight)"
                ));
            }
            cx.background_executor()
                .timer(Duration::from_millis(10))
                .await;
        }
    }

    async fn sleep(cx: &AsyncApp, duration: Duration) {
        cx.background_executor().timer(duration).await;
    }

    /// Waits until no image load is in flight (with a little slack for views to repaint).
    async fn settle_images(&self, cx: &mut AsyncApp) -> Result<(), String> {
        let mut quiet_since: Option<Instant> = None;
        self.until(cx, Duration::from_secs(60), |_, cx| {
            let busy = image_stats(cx).is_some_and(|s| s.in_flight > 0);
            let now = Instant::now();
            if busy {
                quiet_since = None;
                return None;
            }
            let since = *quiet_since.get_or_insert(now);
            (now - since >= Duration::from_millis(300)).then_some(())
        })
        .await
    }

    fn launch(&self, report: &mut Report, first_draw: Instant) -> Result<(), String> {
        let start = PROCESS_START
            .get()
            .copied()
            .ok_or("process start not marked")?;
        report.ms("main_to_first_draw_ms", first_draw - start);
        if let Some(t0) = std::env::var("GPUI_CHAT_BENCH_T0_NS")
            .ok()
            .and_then(|v| v.parse::<u128>().ok())
        {
            // The wall-clock time of the first draw, from how long ago it was.
            let ago = Instant::now() - first_draw;
            let drawn = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .saturating_sub(ago)
                .as_nanos();
            let exec = drawn.saturating_sub(t0);
            report.num("exec_to_first_draw_ms", exec as f64 / 1e6);
        }
        Ok(())
    }

    /// Scroll events every `every` for ten seconds over the chat, flipping direction each
    /// second, while the source keeps its rate.
    async fn scroll(
        &self,
        report: &mut Report,
        cx: &mut AsyncApp,
        every: Duration,
    ) -> Result<(), String> {
        let counters = self.diagnostics.counters();
        let dropped_before = counters.dropped();
        // Let the rate settle in, then measure from a clean window.
        Self::sleep(cx, Duration::from_secs(2)).await;
        self.diagnostics.reset_frame_window();
        let start = Instant::now();
        let measure = Duration::from_secs(10);
        let mut due = start;
        let mut sent = 0usize;
        while due - start < measure {
            due += every;
            let now = Instant::now();
            if due > now {
                cx.background_executor().timer(due - now).await;
            }
            let up = (due - start).as_secs().is_multiple_of(2);
            let delta = if up { px(36.0) } else { px(-36.0) };
            self.window
                .update(cx, |_, window, cx| {
                    let size = window.viewport_size();
                    let event = PlatformInput::Scroll(ScrollEvent {
                        position: point(size.width * 0.6, size.height * 0.5),
                        delta: ScrollDelta::Pixels(point(Pixels::ZERO, delta)),
                        modifiers: Modifiers::default(),
                        touch_phase: TouchPhase::Moved,
                        ..ScrollEvent::default()
                    });
                    window.dispatch_event(event, cx);
                })
                .map_err(|e| e.to_string())?;
            self.diagnostics.record_input(due, Instant::now());
            sent += 1;
        }
        // The last event's draw.
        Self::sleep(cx, Duration::from_millis(100)).await;
        let frames = self.diagnostics.frame_summary(Instant::now());
        let ingest = self.diagnostics.ingest_summary(Instant::now());
        report
            .num("frames", frames.window_draws as f64)
            .ms("frame_total_p99_ms", frames.total_p99)
            .ms("draw_p50_ms", frames.p50)
            .ms("draw_p99_ms", frames.p99)
            .ms("apply_p99_ms", frames.apply_p99)
            .num("over_budget_frames", frames.over_budget as f64)
            .num("inputs_sent", sent as f64)
            .num("inputs_drawn", frames.inputs as f64)
            .ms("input_p50_ms", frames.input_p50)
            .ms("input_p99_ms", frames.input_p99)
            .ms("input_max_ms", frames.input_max)
            .num("ingest_per_s", ingest.per_second)
            .num("dropped", (counters.dropped() - dropped_before) as f64)
            .num("behind", counters.behind() as f64);
        Ok(())
    }

    async fn idle(&self, report: &mut Report, cx: &mut AsyncApp) -> Result<(), String> {
        Self::sleep(cx, Duration::from_secs(3)).await;
        let measure = Duration::from_secs(10);
        let draws_before = self.diagnostics.total_draws();
        let cpu_before = own_cpu_seconds().ok_or("ps failed")?;
        let start = Instant::now();
        Self::sleep(cx, measure).await;
        let cpu_after = own_cpu_seconds().ok_or("ps failed")?;
        let elapsed = start.elapsed().as_secs_f64();
        report
            .num("cpu_percent", (cpu_after - cpu_before) / elapsed * 100.0)
            .num(
                "draws",
                (self.diagnostics.total_draws() - draws_before) as f64,
            )
            .num("seconds", elapsed);
        Ok(())
    }

    /// Fills the channels one after the other, each shown (following the newest message) while
    /// its messages arrive, so every row is drawn once and every image in it loads.
    async fn feed_shown_channels(&self, plan: Plan, cx: &mut AsyncApp) -> Result<(), String> {
        const BATCH: usize = 16;
        let channels = firehose::channel_names(plan.channels);
        let third_party = Arc::new(ThirdPartyEmotes::synthetic(&channels));
        let cosmetics = cx.update(|cx| cx.global::<crate::badges::Badges>().cosmetics().clone());
        firehose::seed_cosmetics(&cosmetics);
        let start_ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        for (index, channel) in channels.iter().enumerate() {
            let mut generator = Generator::new(&FirehoseConfig {
                rate: 2_000.0,
                seed: crate::cli::DEFAULT_SEED + index as u64,
                channels: vec![channel.clone()],
                start_ts_ms,
                cosmetics: Some(cosmetics.clone()),
                third_party: Some(third_party.clone()),
            });
            let mut sent = 0;
            while sent < plan.prefill_per_channel {
                let n = BATCH.min(plan.prefill_per_channel - sent);
                for _ in 0..n {
                    self.sender
                        .send(generator.next_event())
                        .await
                        .map_err(|_| "bridge closed")?;
                }
                sent += n;
                if sent == n && index > 0 {
                    // The channel exists once its first batch is in: show it.
                    let hub = self.hub.clone();
                    self.until(cx, Duration::from_secs(60), move |_, cx| {
                        (hub.read(cx).channels().len() > index).then_some(())
                    })
                    .await
                    .map_err(|e| format!("channel {} never appeared: {e}", index + 1))?;
                    self.window
                        .update(cx, |_, window, cx| {
                            window.dispatch_action(Box::new(crate::actions::SelectNextChannel), cx);
                        })
                        .map_err(|e| e.to_string())?;
                }
                Self::sleep(cx, Duration::from_millis(8)).await;
            }
            // Fully loaded before the next channel: all its messages applied and drawn, its
            // images loaded and drawn (a slow machine must not move on early and understate).
            let (hub, want) = (self.hub.clone(), plan.prefill_per_channel);
            let shown = channel.clone();
            self.until(cx, Duration::from_secs(60), move |_, cx| {
                let held = hub
                    .read(cx)
                    .sizes(cx)
                    .into_iter()
                    .find(|s| s.channel == shown);
                held.is_some_and(|s| s.len >= want).then_some(())
            })
            .await
            .map_err(|e| format!("channel {} never held its messages: {e}", index + 1))?;
            self.drawn_after_now(cx).await?;
            self.settle_images(cx).await?;
            self.drawn_after_now(cx).await?;
        }
        Ok(())
    }

    /// Asks the window to draw and waits until a draw has finished after this point.
    async fn drawn_after_now(&self, cx: &mut AsyncApp) -> Result<(), String> {
        let before = self.diagnostics.total_draws();
        self.window
            .update(cx, |_, window, _| window.refresh())
            .map_err(|e| e.to_string())?;
        self.until(cx, Duration::from_secs(60), move |d, _| {
            (d.total_draws() > before).then_some(())
        })
        .await
    }

    async fn memory(&self, report: &mut Report, cx: &mut AsyncApp) -> Result<(), String> {
        Self::sleep(cx, Duration::from_secs(2)).await;
        self.memory_numbers(report, cx);
        Ok(())
    }

    /// Scrolls the whole channel of distinct animated emotes from the top to the bottom, so
    /// every image is decoded and painted (uploaded) once and most are evicted again.
    async fn images(&self, report: &mut Report, cx: &mut AsyncApp) -> Result<(), String> {
        let steps = 3_000;
        for step in 0..steps {
            // Up to the top first, then down to the end.
            let delta = if step < steps / 3 {
                px(400.0)
            } else {
                px(-200.0)
            };
            self.window
                .update(cx, |_, window, cx| {
                    let size = window.viewport_size();
                    window.dispatch_event(
                        PlatformInput::Scroll(ScrollEvent {
                            position: point(size.width * 0.6, size.height * 0.5),
                            delta: ScrollDelta::Pixels(point(Pixels::ZERO, delta)),
                            touch_phase: TouchPhase::Moved,
                            ..ScrollEvent::default()
                        }),
                        cx,
                    );
                })
                .map_err(|e| e.to_string())?;
            Self::sleep(cx, Duration::from_millis(8)).await;
            if step % 50 == 0 {
                self.settle_images(cx).await?;
            }
        }
        self.settle_images(cx).await?;
        Self::sleep(cx, Duration::from_secs(2)).await;
        self.memory_numbers(report, cx);
        Ok(())
    }

    fn memory_numbers(&self, report: &mut Report, cx: &mut AsyncApp) {
        let (stats, graveyard, messages) = cx.update(|cx| {
            (
                image_stats(cx),
                cx.try_global::<crate::images::Images>()
                    .map_or(0, crate::images::Images::graveyard_len),
                self.hub
                    .read(cx)
                    .sizes(cx)
                    .iter()
                    .map(|s| s.len)
                    .sum::<usize>(),
            )
        });
        report
            .mb("rss_mb", own_rss_bytes())
            .mb("footprint_mb", own_footprint_bytes())
            .num("messages", messages as f64)
            .mb("image_bytes_mb", stats.map(|s| s.memory_bytes as u64))
            .mb("image_budget_mb", stats.map(|s| s.memory_budget as u64))
            .num("images", stats.map_or(0.0, |s| s.memory_entries as f64))
            .num("evictions", stats.map_or(0.0, |s| s.evictions as f64))
            .num("graveyard", graveyard as f64);
    }
}

fn image_stats(cx: &App) -> Option<gpui_chat_media::CacheStats> {
    cx.try_global::<crate::images::Images>()?.stats()
}

// ---- process numbers ----------------------------------------------------------------------

fn ps(field: &str) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args([
            "-o",
            &format!("{field}="),
            "-p",
            &std::process::id().to_string(),
        ])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// CPU time used so far (user + system), from `ps -o time`: `[[dd-]hh:]mm:ss.cc`.
fn own_cpu_seconds() -> Option<f64> {
    parse_cpu_time(&ps("time")?)
}

fn parse_cpu_time(text: &str) -> Option<f64> {
    let (days, rest) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<f64>().ok()?, rest),
        None => (0.0, text),
    };
    let mut seconds = 0.0;
    for part in rest.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + seconds)
}

/// Resident set size, from `ps -o rss` (KiB).
fn own_rss_bytes() -> Option<u64> {
    ps("rss")?.parse::<u64>().ok().map(|kib| kib * 1024)
}

/// Physical footprint (what Activity Monitor calls Memory: includes GPU memory the process
/// owns), from the header line of `footprint <pid>`.
fn own_footprint_bytes() -> Option<u64> {
    let output = std::process::Command::new("footprint")
        .arg(std::process::id().to_string())
        .output()
        .ok()?;
    parse_footprint(&String::from_utf8_lossy(&output.stdout))
}

fn parse_footprint(text: &str) -> Option<u64> {
    let rest = text.split("Footprint:").nth(1)?.trim_start();
    let mut words = rest.split_whitespace();
    let value: f64 = words.next()?.parse().ok()?;
    let unit = match words.next()? {
        "B" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * unit) as u64)
}

/// The channels a scenario's source creates, in order (tests).
#[cfg(test)]
fn channels_of(kind: BenchKind) -> Vec<gpui_chat_core::ChannelId> {
    firehose::channel_names(kind.plan().channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for name in BenchKind::NAMES {
            assert_eq!(BenchKind::parse(name).map(BenchKind::name), Some(name));
        }
        assert_eq!(BenchKind::parse("nope"), None);
    }

    #[test]
    fn the_memory_scenario_is_ten_full_channels() {
        let plan = BenchKind::Memory.plan();
        assert_eq!((plan.channels, plan.prefill_per_channel), (10, 1_000));
        assert_eq!(channels_of(BenchKind::Memory).len(), 10);
        assert_eq!(
            gpui_chat_settings::Settings::default().chat.scrollback,
            1_000,
            "the budget is for the default scrollback"
        );
    }

    #[test]
    fn synthetic_images_follow_the_url() {
        let fetcher = SyntheticFetcher::new(true);
        let twitch = |scale: &str| {
            fetcher.shape(&format!(
                "https://static-cdn.jtvnw.net/emoticons/v2/25/default/dark/{scale}"
            ))
        };
        assert_eq!(
            (
                twitch("1.0").width,
                twitch("2.0").width,
                twitch("3.0").width
            ),
            (28, 56, 112)
        );
        let seventv = fetcher.shape("https://cdn.7tv.app/emote/benchc01an/2x.webp");
        assert_eq!((seventv.width, seventv.height), (64, 64));
        assert!(seventv.frames > 1);
        let wide = fetcher.shape("https://cdn.7tv.app/emote/benchc07sw/2x.webp");
        assert_eq!((wide.width, wide.height, wide.frames), (128, 64, 1));
        let badge = fetcher.shape("https://static-cdn.jtvnw.net/badges/v1/abc/2");
        assert_eq!((badge.width, badge.frames), (36, 1));
        let stills = SyntheticFetcher::new(false);
        assert_eq!(
            stills
                .shape("https://cdn.7tv.app/emote/benchc01an/2x.webp")
                .frames,
            1
        );
    }

    #[test]
    fn synthetic_images_decode() {
        let fetcher = SyntheticFetcher::new(true);
        for shape in [
            Shape {
                width: 64,
                height: 64,
                frames: 1,
            },
            Shape {
                width: 128,
                height: 64,
                frames: 24,
            },
        ] {
            let bytes = fetcher.bytes(shape).unwrap();
            let image =
                gpui_chat_media::decode(&bytes, &gpui_chat_media::DecodeLimits::default()).unwrap();
            assert_eq!((image.width(), image.height()), (shape.width, shape.height));
            assert_eq!(image.frames().len(), shape.frames as usize);
        }
    }

    #[test]
    fn process_numbers_parse() {
        assert_eq!(parse_cpu_time("0:01.78"), Some(1.78));
        assert_eq!(parse_cpu_time("1:02:03.50"), Some(3723.5));
        assert_eq!(parse_cpu_time("2-00:00:01.00"), Some(172_801.0));
        assert_eq!(parse_cpu_time(""), None);
        let header = "gpui-chat [66065]: 64-bit    Footprint: 223 MB (16384 bytes per page)";
        assert_eq!(parse_footprint(header), Some(223 * 1024 * 1024));
        assert_eq!(
            parse_footprint("x: 64-bit    Footprint: 1.5 GB (16384 bytes per page)"),
            Some(1_610_612_736)
        );
        assert!(own_rss_bytes().is_some_and(|b| b > 0));
        assert!(own_cpu_seconds().is_some());
    }

    #[test]
    fn report_is_one_json_line() {
        let mut report = Report::default();
        report
            .text("bench", "idle")
            .num("cpu_percent", 0.25)
            .ms("p99_ms", Duration::from_micros(1500))
            .mb("rss_mb", None)
            .text("error", "a \"quoted\"\nline");
        assert_eq!(
            report.line(),
            r#"BENCH {"bench":"idle","cpu_percent":0.250,"p99_ms":1.500,"rss_mb":null,"error":"a \"quoted\" line"}"#
        );
    }
}
