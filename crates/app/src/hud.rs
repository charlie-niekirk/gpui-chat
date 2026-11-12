//! The debug HUD: a small overlay with frame time, ingest rate, batch size, store sizes and
//! dropped events. Toggled by [`crate::actions::ToggleDebugHud`].
//!
//! **Idle cost.** The HUD never asks for continuous frames. While hidden it does nothing at all.
//! While visible, a 4 Hz timer wakes it; it re-renders only if something happened since the last
//! tick (an ingest batch, a dropped event, or a draw it did not cause itself), plus one final
//! refresh after activity stops so the numbers settle. An idle window with the HUD open therefore
//! draws nothing; the timer wake-ups themselves are a few microseconds each.

use std::time::{Duration, Instant};

use gpui_kit::component::v_flex;
use gpui_kit::{
    Context, Entity, FontWeight, Hsla, IntoElement, ParentElement, Render, Styled, Task, Window,
    div, px,
};

use crate::channels::ChannelHub;
use crate::diagnostics::{Diagnostics, FRAME_BUDGET};
use crate::theme::Tokens;

const REFRESH: Duration = Duration::from_millis(250);

/// How many channels are listed individually.
const LISTED_STORES: usize = 4;

pub struct DebugHud {
    diagnostics: Diagnostics,
    hub: Entity<ChannelHub>,
    visible: bool,
    ticker: Option<Task<()>>,
    // What the last tick saw, to tell whether anything happened since.
    seen_draws: u64,
    seen_batches: u64,
    seen_dropped: u64,
    /// Re-renders this view asked for since the last tick; their draws are not "activity".
    own_refreshes: u64,
    /// Refresh once more after activity stops.
    settle: bool,
}

impl DebugHud {
    pub fn new(
        diagnostics: Diagnostics,
        hub: Entity<ChannelHub>,
        visible: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut hud = Self {
            seen_draws: diagnostics.total_draws(),
            seen_batches: diagnostics.total_batches(),
            seen_dropped: diagnostics.counters().dropped(),
            diagnostics,
            hub,
            visible: false,
            ticker: None,
            own_refreshes: 0,
            settle: false,
        };
        if visible {
            hud.toggle(cx);
        }
        hud
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.visible = !self.visible;
        if self.visible {
            self.seen_draws = self.diagnostics.total_draws();
            self.seen_batches = self.diagnostics.total_batches();
            self.seen_dropped = self.diagnostics.counters().dropped();
            self.own_refreshes = 0;
            self.settle = false;
            self.ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(REFRESH).await;
                    if this.update(cx, |hud, cx| hud.tick(cx)).is_err() {
                        break;
                    }
                }
            }));
        } else {
            // Dropping the task cancels the timer: a hidden HUD costs nothing.
            self.ticker = None;
        }
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.own_refreshes += 1;
        cx.notify();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let draws = self.diagnostics.total_draws();
        let batches = self.diagnostics.total_batches();
        let dropped = self.diagnostics.counters().dropped();
        // Each refresh we asked for costs at most one draw; anything beyond that was somebody
        // else's (scrolling, resizing, ingest) and counts as activity.
        let foreign_draws = (draws - self.seen_draws).saturating_sub(self.own_refreshes);
        let active =
            foreign_draws > 0 || batches != self.seen_batches || dropped != self.seen_dropped;
        self.seen_draws = draws;
        self.seen_batches = batches;
        self.seen_dropped = dropped;
        self.own_refreshes = 0;
        if active || self.settle {
            self.settle = active;
            self.refresh(cx);
        }
    }
}

fn ms(duration: Duration) -> String {
    format!("{:.1}", duration.as_secs_f64() * 1000.0)
}

fn line(label: &'static str, value: String, colour: Hsla) -> impl IntoElement {
    div()
        .flex()
        .gap_2()
        .child(div().w(px(64.0)).flex_none().child(label))
        .child(div().text_color(colour).child(value))
}

impl Render for DebugHud {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div().into_any_element();
        }
        let tokens = *cx.global::<Tokens>();
        let now = Instant::now();
        let frames = self.diagnostics.frame_summary(now);
        let ingest = self.diagnostics.ingest_summary(now);
        let counters = self.diagnostics.counters();
        let sizes = self.hub.read(cx).sizes(cx);
        let total: usize = sizes.iter().map(|s| s.len).sum();

        let budget_colour = |value: Duration| {
            if value > FRAME_BUDGET {
                tokens.danger
            } else {
                tokens.text
            }
        };
        let dropped = counters.dropped();

        let mut stores = v_flex();
        for size in sizes.iter().take(LISTED_STORES) {
            stores = stores.child(line(
                "",
                format!("{} {}/{}", size.channel, size.len, size.capacity),
                tokens.text_muted,
            ));
        }
        if sizes.len() > LISTED_STORES {
            stores = stores.child(line(
                "",
                format!("+{} more", sizes.len() - LISTED_STORES),
                tokens.text_muted,
            ));
        }

        v_flex()
            .absolute()
            .top(px(8.0))
            .right(px(8.0))
            .gap_0p5()
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .bg(tokens.window_bg)
            .text_xs()
            .text_color(tokens.text)
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .child(format!("Debug HUD · budget {} ms", ms(FRAME_BUDGET))),
            )
            .child(line(
                "draw",
                format!(
                    "{} ms  p50 {}  p99 {}  max {}",
                    ms(frames.last),
                    ms(frames.p50),
                    ms(frames.p99),
                    ms(frames.max)
                ),
                budget_colour(frames.p99),
            ))
            .child(line(
                "over",
                format!("{}/{} frames", frames.over_budget, frames.window_draws),
                if frames.over_budget > 0 {
                    tokens.danger
                } else {
                    tokens.text
                },
            ))
            .child(line(
                "interval",
                format!(
                    "p50 {}  p99 {} ms  {} draws/s",
                    ms(frames.interval_p50),
                    ms(frames.interval_p99),
                    frames.draws_last_second
                ),
                tokens.text,
            ))
            .child(line(
                "ingest",
                format!("{:.0} msg/s", ingest.per_second),
                tokens.text,
            ))
            .child(line(
                "batch",
                format!("{} (max {})", ingest.last_batch, ingest.max_batch),
                tokens.text,
            ))
            .child(line(
                "stores",
                format!("{} channels, {total} msgs", sizes.len()),
                tokens.text,
            ))
            .child(stores)
            .child(line(
                "dropped",
                format!("{dropped} of {}", counters.sent() + dropped),
                if dropped > 0 {
                    tokens.danger
                } else {
                    tokens.text
                },
            ))
            .into_any_element()
    }
}
