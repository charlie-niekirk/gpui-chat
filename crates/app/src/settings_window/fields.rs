//! Text-like fields for the settings window.
//!
//! Kit's own text and number fields write on every keystroke. A font size typed as `2`, `0`
//! would apply a 2 pt font on the way to 20 and write `settings.toml` twice, so these commit when
//! the user presses Enter, leaves the field or steps a number, and say why a value is refused
//! right under the field. The field shows what the file holds: when the file changes (by hand,
//! or another control) the text follows, unless the user's own edit is refused, in which case
//! their text stays beside the error.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_chat_settings::SettingEdit;
use gpui_kit::component::input::{
    Input, InputEvent, InputState, NumberInput, NumberStep, Textarea, TextareaState,
};
use gpui_kit::component::setting::{RenderOptions, SettingField};
use gpui_kit::component::v_flex;
use gpui_kit::{
    App, AppContext as _, Context, Entity, Global, IntoElement, ParentElement as _, SharedString,
    Styled as _, Subscription, Window, div, px,
};

use crate::theme::Tokens;

/// Writes a committed value; the error is shown under the field.
pub type Commit<T> = Rc<dyn Fn(T, &mut App) -> Result<(), String>>;

/// Width of the text fields. The window's content column is about 600 px wide at its smallest.
const TEXT_WIDTH: f32 = 210.0;
const NUMBER_WIDTH: f32 = 140.0;

/// Commits whatever is typed in a field but not yet committed. Every rendered field registers
/// one under its key (replacing the last frame's), so the window can flush them all before it
/// closes or the app quits, where a field's blur would never be delivered.
#[derive(Default)]
struct Flushers(HashMap<&'static str, Flush>);

type Flush = Rc<dyn Fn(&mut App)>;

impl Global for Flushers {}

fn register<S: 'static>(
    key: &'static str,
    state: &Entity<S>,
    flush: impl Fn(&mut S, &mut Context<S>) + 'static,
    cx: &mut App,
) {
    let state = state.downgrade();
    let flush: Flush = Rc::new(move |cx| {
        state.update(cx, |state, cx| flush(state, cx)).ok();
    });
    if cx.try_global::<Flushers>().is_none() {
        cx.set_global(Flushers::default());
    }
    cx.global_mut::<Flushers>().0.insert(key, flush);
}

/// Commits every field's uncommitted text (a refusal stays in the field, which is about to go
/// away; the file keeps its last good value).
pub fn flush_all(cx: &mut App) {
    let flushers: Vec<_> = cx
        .try_global::<Flushers>()
        .map(|f| f.0.values().cloned().collect())
        .unwrap_or_default();
    for flush in flushers {
        flush(cx);
    }
}

/// How the page's reset button treats a field: dirty when it differs from the default, and
/// resetting removes the key from the file so the default applies again.
pub struct Reset {
    pub field: &'static str,
    pub is_dirty: Rc<dyn Fn(&App) -> bool>,
}

fn with_reset(field: SettingField<SharedString>, reset: Reset) -> SettingField<SharedString> {
    let Reset {
        field: key,
        is_dirty,
    } = reset;
    field.on_reset(
        move |cx| is_dirty(cx),
        move |_, cx| {
            // A refusal (another key in the file is invalid) goes to the window's banner.
            crate::settings_window::write(vec![SettingEdit::remove(key)], cx);
        },
    )
}

fn error_line(
    error: Option<&SharedString>,
    width: f32,
    cx: &App,
) -> Option<impl IntoElement + use<>> {
    let error = error?.clone();
    let danger = cx.global::<Tokens>().danger_text;
    Some(
        div()
            .max_w(px(width))
            .text_xs()
            .text_color(danger)
            .child(error),
    )
}

// ---- single-line text ----

struct TextState {
    input: Entity<InputState>,
    /// What the file holds, as this field last showed it.
    shown: String,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl TextState {
    fn commit(&mut self, commit: &Commit<String>, cx: &mut App) {
        let text = self.input.read(cx).value().trim().to_owned();
        self.error = if text == self.shown {
            None
        } else {
            commit(text, cx).err().map(SharedString::from)
        };
    }
}

/// A single-line text field. `current` reads the value from the settings in effect; `commit`
/// receives the trimmed text (possibly empty) and writes it.
pub fn text(
    key: &'static str,
    title: &'static str,
    placeholder: &'static str,
    current: impl Fn(&App) -> String + 'static,
    commit: Commit<String>,
    reset: Reset,
) -> SettingField<SharedString> {
    let field = SettingField::render(
        move |_: &RenderOptions, window: &mut Window, cx: &mut App| {
            let value = current(cx);
            let state = window.use_keyed_state(key, cx, |window, cx| {
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .default_value(value.clone())
                });
                let commit = commit.clone();
                let subscription = cx.subscribe_in(
                    &input,
                    window,
                    move |state: &mut TextState,
                          _input,
                          event: &InputEvent,
                          _window,
                          cx: &mut Context<TextState>| {
                        if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                            state.commit(&commit, cx);
                            cx.notify();
                        }
                    },
                );
                TextState {
                    input,
                    shown: value.clone(),
                    error: None,
                    _subscription: subscription,
                }
            });
            register(
                key,
                &state,
                {
                    let commit = commit.clone();
                    move |state, cx| state.commit(&commit, cx)
                },
                cx,
            );
            state.update(cx, |state, cx| {
                if state.shown != value {
                    state.shown = value.clone();
                    state.error = None;
                    if state.input.read(cx).value() != value {
                        state
                            .input
                            .update(cx, |input, cx| input.set_value(value.clone(), window, cx));
                    }
                }
            });
            let state = state.read(cx);
            v_flex()
                .gap_1()
                .items_end()
                .child(Input::new(&state.input).aria_label(title).w(px(TEXT_WIDTH)))
                .children(error_line(state.error.as_ref(), TEXT_WIDTH, cx))
        },
    );
    with_reset(field, reset)
}

// ---- number ----

/// What a number field accepts and how its buttons step.
#[derive(Clone, Copy)]
pub struct NumberSpec {
    pub min: f64,
    pub max: f64,
    pub step: f64,
    /// Decimal places kept when stepping (`0.1` steps would otherwise show `1.4000000000000001`).
    pub decimals: i32,
}

impl NumberSpec {
    fn round(&self, value: f64) -> f64 {
        let scale = 10f64.powi(self.decimals);
        (value * scale).round() / scale
    }

    fn message(&self) -> String {
        let kind = if self.decimals == 0 {
            "whole number"
        } else {
            "number"
        };
        format!("Enter a {kind} from {} to {}.", self.min, self.max)
    }

    /// The number in `text`, rounded to the places the setting keeps, if it is one inside the
    /// range (and whole, for a setting that keeps no decimals).
    fn parse(&self, text: &str) -> Result<f64, String> {
        match text.trim().parse::<f64>() {
            Ok(v) if v.is_finite() && (self.decimals > 0 || v.fract() == 0.0) => {
                let v = self.round(v);
                (self.min..=self.max)
                    .contains(&v)
                    .then_some(v)
                    .ok_or_else(|| self.message())
            }
            _ => Err(self.message()),
        }
    }
}

struct NumberState {
    input: Entity<InputState>,
    shown: String,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl NumberState {
    fn commit(&mut self, text: &str, spec: NumberSpec, commit: &Commit<f64>, cx: &mut App) {
        self.error = if text.trim() == self.shown {
            None
        } else {
            spec.parse(text)
                .and_then(|value| commit(value, cx))
                .err()
                .map(SharedString::from)
        };
    }
}

/// A number field with − / + buttons and ↑ / ↓ (each step is committed at once). `current` is the value as text, formatted by the
/// caller (an `f32` setting must not show its `f64` widening).
pub fn number(
    key: &'static str,
    title: &'static str,
    spec: NumberSpec,
    current: impl Fn(&App) -> String + 'static,
    commit: Commit<f64>,
    reset: Reset,
) -> SettingField<SharedString> {
    let field = SettingField::render(
        move |_: &RenderOptions, window: &mut Window, cx: &mut App| {
            let value = current(cx);
            let state = window.use_keyed_state(key, cx, |window, cx| {
                // The engine steps the text itself (− / + and ↑ / ↓) and then reports a `Change`,
                // the same event as a keystroke. `by_value` runs only for a step, so it marks the
                // `Change` that follows as one to commit at once; typing waits for Enter or blur.
                let stepping = Rc::new(Cell::new(false));
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(value.clone())
                        .step(NumberStep::by_value({
                            let stepping = stepping.clone();
                            move |_, _, _| {
                                stepping.set(true);
                                spec.step
                            }
                        }))
                });
                let subscription = cx.subscribe_in(&input, window, {
                    let commit = commit.clone();
                    move |state: &mut NumberState,
                          input,
                          event: &InputEvent,
                          _window,
                          cx: &mut Context<NumberState>| {
                        let commit_now = match event {
                            InputEvent::PressEnter { .. } | InputEvent::Blur => {
                                stepping.set(false);
                                true
                            }
                            InputEvent::Change => stepping.take(),
                            InputEvent::Focus => false,
                        };
                        if commit_now {
                            let text = input.read(cx).value();
                            state.commit(&text, spec, &commit, cx);
                            cx.notify();
                        }
                    }
                });
                NumberState {
                    input,
                    shown: value.clone(),
                    error: None,
                    _subscription: subscription,
                }
            });
            register(
                key,
                &state,
                {
                    let commit = commit.clone();
                    move |state, cx| {
                        let text = state.input.read(cx).value();
                        state.commit(&text, spec, &commit, cx);
                    }
                },
                cx,
            );
            state.update(cx, |state, cx| {
                if state.shown != value {
                    state.shown = value.clone();
                    state.error = None;
                    if state.input.read(cx).value() != value {
                        state
                            .input
                            .update(cx, |input, cx| input.set_value(value.clone(), window, cx));
                    }
                }
            });
            let state = state.read(cx);
            v_flex()
                .gap_1()
                .items_end()
                .child(
                    NumberInput::new(&state.input)
                        .accessibility_label(title)
                        .w(px(NUMBER_WIDTH)),
                )
                .children(error_line(state.error.as_ref(), TEXT_WIDTH, cx))
        },
    );
    with_reset(field, reset)
}

// ---- one item per line ----

struct LinesState {
    input: Entity<TextareaState>,
    shown: String,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl LinesState {
    fn commit(&mut self, commit: &Commit<Vec<String>>, cx: &mut App) {
        let entries = clean_lines(&self.input.read(cx).value());
        self.error = if entries.join("\n") == self.shown {
            None
        } else {
            commit(entries, cx).err().map(SharedString::from)
        };
    }
}

/// A multi-line field holding a list, one entry per line; commits when it loses focus (Enter
/// starts a new line). `commit` receives the non-blank, trimmed lines.
pub fn lines(
    key: &'static str,
    title: &'static str,
    placeholder: &'static str,
    current: impl Fn(&App) -> Vec<String> + 'static,
    commit: Commit<Vec<String>>,
    reset: Reset,
) -> SettingField<SharedString> {
    lines_sized(
        key,
        title,
        placeholder,
        Size {
            width: TEXT_WIDTH,
            rows: 3,
        },
        current,
        commit,
        reset,
    )
}

/// How big a multi-line field is: its width in pixels and its height in rows.
#[derive(Clone, Copy)]
pub struct Size {
    pub width: f32,
    pub rows: usize,
}

/// [`lines`] with a size of its own, for lists whose entries are longer than a program argument
/// (highlight rules).
pub fn lines_sized(
    key: &'static str,
    title: &'static str,
    placeholder: &'static str,
    size: Size,
    current: impl Fn(&App) -> Vec<String> + 'static,
    commit: Commit<Vec<String>>,
    reset: Reset,
) -> SettingField<SharedString> {
    let field = SettingField::render(
        move |_: &RenderOptions, window: &mut Window, cx: &mut App| {
            let value = current(cx).join("\n");
            let state = window.use_keyed_state(key, cx, |window, cx| {
                let input = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .rows(size.rows)
                        .placeholder(placeholder)
                        .default_value(value.clone())
                });
                let commit = commit.clone();
                let subscription = cx.subscribe_in(
                    &input,
                    window,
                    move |state: &mut LinesState,
                          _input,
                          event: &InputEvent,
                          _window,
                          cx: &mut Context<LinesState>| {
                        if matches!(event, InputEvent::Blur) {
                            state.commit(&commit, cx);
                            cx.notify();
                        }
                    },
                );
                LinesState {
                    input,
                    shown: value.clone(),
                    error: None,
                    _subscription: subscription,
                }
            });
            register(
                key,
                &state,
                {
                    let commit = commit.clone();
                    move |state, cx| state.commit(&commit, cx)
                },
                cx,
            );
            state.update(cx, |state, cx| {
                if state.shown != value {
                    state.shown = value.clone();
                    state.error = None;
                    if clean_lines(&state.input.read(cx).value()).join("\n") != value {
                        state
                            .input
                            .update(cx, |input, cx| input.set_value(value.clone(), window, cx));
                    }
                }
            });
            let state = state.read(cx);
            v_flex()
                .gap_1()
                .items_end()
                .child(
                    Textarea::new(&state.input)
                        .aria_label(title)
                        .w(px(size.width)),
                )
                .children(error_line(state.error.as_ref(), size.width, cx))
        },
    );
    with_reset(field, reset)
}

/// The entries of a one-per-line list: trimmed, blank lines dropped.
pub fn clean_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: NumberSpec = NumberSpec {
        min: 6.0,
        max: 72.0,
        step: 1.0,
        decimals: 1,
    };

    #[test]
    fn numbers_inside_the_range_are_accepted() {
        assert_eq!(SIZE.parse("14"), Ok(14.0));
        assert_eq!(SIZE.parse(" 6.5 "), Ok(6.5));
        assert_eq!(SIZE.parse("72"), Ok(72.0));
    }

    #[test]
    fn anything_else_is_refused_with_the_range_in_the_message() {
        for bad in ["", "abc", "5", "73", "nan", "inf", "1e9"] {
            let error = SIZE.parse(bad).unwrap_err();
            assert_eq!(error, "Enter a number from 6 to 72.", "{bad:?}");
        }
    }

    #[test]
    fn a_setting_without_decimals_takes_whole_numbers_only() {
        let scrollback = NumberSpec {
            min: 10.0,
            max: 100_000.0,
            step: 100.0,
            decimals: 0,
        };
        assert_eq!(scrollback.parse("150"), Ok(150.0));
        assert_eq!(
            scrollback.parse("150.7").unwrap_err(),
            "Enter a whole number from 10 to 100000."
        );
        assert_eq!(scrollback.parse("150.0"), Ok(150.0));
    }

    #[test]
    fn a_value_is_rounded_to_the_places_the_setting_keeps() {
        let height = NumberSpec {
            min: 1.0,
            max: 3.0,
            step: 0.1,
            decimals: 2,
        };
        assert_eq!(height.parse("1.236"), Ok(1.24));
        assert_eq!(height.parse("1.5000000000000002"), Ok(1.5));
    }

    #[test]
    fn stepping_rounds_away_float_noise() {
        let spec = NumberSpec {
            min: 1.0,
            max: 3.0,
            step: 0.1,
            decimals: 1,
        };
        assert_eq!(spec.round(1.4 + 0.1).to_string(), "1.5");
        assert_eq!(spec.round(1.1 + 0.2).to_string(), "1.3");
    }

    #[test]
    fn list_lines_are_trimmed_and_blank_ones_dropped() {
        assert_eq!(
            clean_lines("  --twitch-low-latency \n\n--player-args\n  \n-fs"),
            ["--twitch-low-latency", "--player-args", "-fs"]
        );
        assert!(clean_lines("").is_empty());
    }
}
