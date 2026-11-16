//! What is wrong, once, in one place (PR 6.3): the strips above the panes.
//!
//! Every failure the user can act on (the connection, the emote and badge providers, Twitch's API,
//! the keychain, a bad `settings.toml` / `keymap.toml` / theme file, …) is a [`Problem`] filed under
//! a **cause key** in the [`Problems`] global. Filing under a key that is already there replaces
//! the problem, so the same cause never shows twice; the source clears the key when the cause is
//! gone, and the strip goes with it. A problem the user dismissed stays hidden while it reads the
//! same; when its wording changes (a different error) or it is cleared and comes back, it shows
//! again.
//!
//! [`ProblemSet`] is the plain data (and carries the tests); [`Problems`] wraps it as a GPUI global
//! the shell observes; [`strip`] draws one problem: a title, the lines under it, an optional
//! action button, "Copy details" when there is something for a bug report, and Dismiss. The strip
//! is the one component every banner uses, so spacing, colours (theme tokens only) and the
//! accessibility role are the same everywhere.

use std::collections::{BTreeMap, HashMap};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, BorrowAppContext as _, ClickEvent, ClipboardItem, FontWeight, Global,
    InteractiveElement, IntoElement, ParentElement, Role, SharedString, StatefulInteractiveElement,
    Styled, Window, div,
};

use crate::theme::Tokens;

/// How serious a problem is: picks the strip's colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Something does not work (a failed request, a refused login).
    Error,
    /// Something works with a fallback (a theme that was skipped, chat reconnecting).
    Warning,
    /// Worth knowing, nothing is wrong.
    Info,
}

/// What the strip's action button does. The shell answers it (it owns the windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    /// Start the Twitch login.
    LogIn,
    /// Open the settings window.
    OpenSettings,
    /// Fetch the third-party emotes again.
    ReloadEmotes,
    /// Log in again asking for the moderation permissions.
    EnableModeration,
    /// Log in again asking for the block list permissions.
    EnableBlockList,
}

/// One problem as the strip shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub severity: Severity,
    /// One short sentence: what is wrong.
    pub title: String,
    /// What to do or what stays in effect, one line each.
    pub lines: Vec<String>,
    /// The button that fixes it, if one does.
    pub fix: Option<(&'static str, Fix)>,
    /// Text for a bug report (the raw error); shown behind "Copy details" and never in the strip.
    pub details: Option<String>,
    /// Whether the strip has a Dismiss button. Problems the user cannot fix by dismissing the
    /// cause (they go when the cause does) are still dismissible: hiding is never blocked.
    pub dismissible: bool,
}

impl Problem {
    pub fn new(severity: Severity, title: impl Into<String>) -> Self {
        Self {
            severity,
            title: title.into(),
            lines: Vec::new(),
            fix: None,
            details: None,
            dismissible: true,
        }
    }

    pub fn error(title: impl Into<String>) -> Self {
        Self::new(Severity::Error, title)
    }

    pub fn warning(title: impl Into<String>) -> Self {
        Self::new(Severity::Warning, title)
    }

    pub fn info(title: impl Into<String>) -> Self {
        Self::new(Severity::Info, title)
    }

    pub fn line(mut self, line: impl Into<String>) -> Self {
        self.lines.push(line.into());
        self
    }

    pub fn lines(mut self, lines: impl IntoIterator<Item = String>) -> Self {
        self.lines.extend(lines);
        self
    }

    pub fn fix(mut self, label: &'static str, fix: Fix) -> Self {
        self.fix = Some((label, fix));
        self
    }

    /// No Dismiss button: the strip goes when the cause does.
    pub fn pinned(mut self) -> Self {
        self.dismissible = false;
        self
    }

    pub fn details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    /// What "the same problem" means for dismissal: its words.
    fn fingerprint(&self) -> String {
        format!(
            "{:?}\n{}\n{}",
            self.severity,
            self.title,
            self.lines.join("\n")
        )
    }

    /// The text "Copy details" puts on the clipboard: the title, the lines and the raw detail.
    pub fn report(&self, app: &str, version: &str) -> String {
        let mut text = format!("{app} {version}\n{}", self.title);
        for line in &self.lines {
            text.push('\n');
            text.push_str(line);
        }
        if let Some(details) = &self.details {
            text.push_str("\n\n");
            text.push_str(details);
        }
        text
    }
}

/// The problems currently reported, by cause key, and which of them the user dismissed.
#[derive(Debug, Default, Clone)]
pub struct ProblemSet {
    items: BTreeMap<&'static str, Problem>,
    /// Cause key → the fingerprint that was dismissed.
    dismissed: HashMap<&'static str, String>,
}

impl ProblemSet {
    /// Files `problem` under `key`, replacing what was there. Returns whether anything changed
    /// (so a source can report on every poll without redrawing).
    pub fn report(&mut self, key: &'static str, problem: Problem) -> bool {
        if self.items.get(key) == Some(&problem) {
            return false;
        }
        // A different message is news again, even if the old one was dismissed.
        if self
            .dismissed
            .get(key)
            .is_some_and(|dismissed| *dismissed != problem.fingerprint())
        {
            self.dismissed.remove(key);
        }
        self.items.insert(key, problem);
        true
    }

    /// The cause is gone. A later report of it shows again.
    pub fn clear(&mut self, key: &'static str) -> bool {
        self.dismissed.remove(key);
        self.items.remove(key).is_some()
    }

    /// The user hid `key`'s strip.
    pub fn dismiss(&mut self, key: &'static str) -> bool {
        let Some(problem) = self.items.get(key) else {
            return false;
        };
        self.dismissed.insert(key, problem.fingerprint());
        true
    }

    /// The problems to show, in cause-key order (stable between frames).
    pub fn visible(&self) -> Vec<(&'static str, &Problem)> {
        self.items
            .iter()
            .filter(|(key, problem)| {
                self.dismissed
                    .get(*key)
                    .is_none_or(|dismissed| *dismissed != problem.fingerprint())
            })
            .map(|(key, problem)| (*key, problem))
            .collect()
    }

    pub fn get(&self, key: &'static str) -> Option<&Problem> {
        self.items.get(key)
    }
}

/// The set as a GPUI global: the shell observes it and redraws on any change.
#[derive(Default)]
pub struct Problems(ProblemSet);

impl Global for Problems {}

/// Files `problem` under `key` (see [`ProblemSet::report`]); the strips redraw when it changed.
pub fn report(key: &'static str, problem: Problem, cx: &mut App) {
    // Look first: `update_global` notifies observers whether or not anything changed.
    if cx
        .try_global::<Problems>()
        .is_some_and(|problems| problems.0.get(key) == Some(&problem))
    {
        return;
    }
    cx.default_global::<Problems>().0.report(key, problem);
    notify(cx);
}

/// The cause is gone.
pub fn clear(key: &'static str, cx: &mut App) {
    if cx
        .try_global::<Problems>()
        .is_none_or(|problems| problems.0.get(key).is_none())
    {
        return;
    }
    cx.default_global::<Problems>().0.clear(key);
    notify(cx);
}

/// Hides the strip until its wording changes or it comes back.
pub fn dismiss(key: &'static str, cx: &mut App) {
    cx.default_global::<Problems>().0.dismiss(key);
    notify(cx);
}

/// A copy of what is visible now.
pub fn visible(cx: &App) -> Vec<(&'static str, Problem)> {
    cx.try_global::<Problems>()
        .map_or_else(Vec::new, |problems| {
            problems
                .0
                .visible()
                .into_iter()
                .map(|(key, problem)| (key, problem.clone()))
                .collect()
        })
}

/// Wakes the observers of the global (`observe_global` fires on `set_global` / `update_global`).
fn notify(cx: &mut App) {
    cx.update_global::<Problems, _>(|_, _| {});
}

/// A change to report from a thread that has no GPUI context (the backend runtime, a worker).
enum Op {
    Report(&'static str, Problem),
    Clear(&'static str),
}

static SINK: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<Op>> =
    std::sync::OnceLock::new();

/// Starts the receiving end of [`report_from_anywhere`] / [`clear_from_anywhere`]: a task on the
/// UI thread that applies each report as it arrives (nothing polls; it sleeps on the channel).
/// Call once at startup.
pub fn init(cx: &mut App) {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Op>();
    if SINK.set(sender).is_err() {
        return;
    }
    cx.spawn(async move |cx| {
        while let Some(op) = receiver.recv().await {
            cx.update(|cx| match op {
                Op::Report(key, problem) => report(key, problem, cx),
                Op::Clear(key) => clear(key, cx),
            });
        }
    })
    .detach();
}

/// [`report`] from any thread. Does nothing before [`init`] (tests, the offline sources).
pub fn report_from_anywhere(key: &'static str, problem: Problem) {
    if let Some(sink) = SINK.get() {
        sink.send(Op::Report(key, problem)).ok();
    }
}

/// A file watcher could not start: edits to `what` are only seen after a restart. One strip for all
/// the watchers, listing the files in the details.
pub fn report_watch_failure(what: &str, error: &str) {
    report_from_anywhere(
        "watch",
        Problem::warning("Edits to settings, themes or keymap are not picked up live")
            .line("Restart the app after editing those files.")
            .details(format!("{what}: {error}")),
    );
}

/// [`clear`] from any thread.
pub fn clear_from_anywhere(key: &'static str) {
    if let Some(sink) = SINK.get() {
        sink.send(Op::Clear(key)).ok();
    }
}

/// A strip button's callback.
pub type Callback = Box<dyn Fn(&mut Window, &mut App)>;
/// The fix button's callback: told which fix it is.
pub type FixCallback = Box<dyn Fn(Fix, &mut Window, &mut App)>;

/// What the strip's buttons call back into.
pub struct StripActions {
    pub on_fix: FixCallback,
    pub on_dismiss: Callback,
}

/// `line` with every absolute path cut down to its file name (`/Users/me/.config/x/themes/a.toml:1:6:`
/// becomes `a.toml:1:6:`): the message says which file, "Copy details" carries the full text.
pub fn plain(line: &str) -> String {
    line.split(' ')
        .map(|word| match word.starts_with('/') {
            true => word.rsplit('/').next().unwrap_or(word),
            false => word,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A strip with no buttons the caller handles: Dismiss does nothing, so use it for problems that
/// have `dismissible: false`.
pub fn static_strip(id: &'static str, problem: &Problem, tokens: &Tokens) -> AnyElement {
    strip(
        id,
        problem,
        tokens,
        StripActions {
            on_fix: Box::new(|_, _, _| {}),
            on_dismiss: Box::new(|_, _| {}),
        },
    )
}

/// Draws `problem` as a full-width strip. `id` must be unique among the strips on screen (the
/// cause key does).
pub fn strip(
    id: &'static str,
    problem: &Problem,
    tokens: &Tokens,
    actions: StripActions,
) -> AnyElement {
    let (tint, title_color) = match problem.severity {
        Severity::Error => (tokens.danger, tokens.danger_text),
        Severity::Warning => (tokens.warning, tokens.text),
        Severity::Info => (tokens.accent, tokens.accent_link),
    };
    let StripActions { on_fix, on_dismiss } = actions;
    let mut buttons = h_flex().flex_none().items_center().gap_1();
    if let Some((label, fix)) = problem.fix {
        buttons = buttons.child(
            Button::new(SharedString::from(format!("{id}-fix")))
                .small()
                .label(label)
                .on_click(move |_: &ClickEvent, window, cx| on_fix(fix, window, cx)),
        );
    }
    if problem.details.is_some() {
        let report = problem.report(gpui_chat_core::APP_NAME, env!("CARGO_PKG_VERSION"));
        buttons = buttons.child(
            Button::new(SharedString::from(format!("{id}-copy")))
                .small()
                .ghost()
                .label("Copy details")
                .accessibility_label(format!("Copy details of: {}", problem.title))
                .on_click(move |_: &ClickEvent, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(report.clone()));
                }),
        );
    }
    if problem.dismissible {
        buttons = buttons.child(
            Button::new(SharedString::from(format!("{id}-dismiss")))
                .small()
                .ghost()
                .icon(Icon::new(IconName::Close))
                .accessibility_label(format!("Dismiss: {}", problem.title))
                .on_click(move |_: &ClickEvent, window, cx| on_dismiss(window, cx)),
        );
    }
    h_flex()
        .id(id)
        .role(Role::Alert)
        .aria_label(SharedString::from(problem.title.clone()))
        .w_full()
        .flex_none()
        .px_3()
        .py_2()
        .gap_3()
        .items_center()
        .bg(tint.opacity(0.14))
        .border_b_1()
        .border_color(tint)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .text_color(title_color)
                        .child(problem.title.clone()),
                )
                .children(
                    problem
                        .lines
                        .iter()
                        .map(|line| div().text_sm().child(line.clone())),
                ),
        )
        .child(buttons)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offline() -> Problem {
        Problem::error("Chat is offline")
            .line("Retrying")
            .details("connection refused")
    }

    #[test]
    fn one_cause_is_one_strip() {
        let mut set = ProblemSet::default();
        assert!(set.report("connection", offline()));
        assert!(
            !set.report("connection", offline()),
            "the same report again changes nothing"
        );
        assert!(set.report(
            "connection",
            Problem::error("Chat is offline").line("Retry 2")
        ));
        assert_eq!(set.visible().len(), 1, "a new wording replaces, not adds");
    }

    #[test]
    fn causes_are_kept_apart_and_ordered() {
        let mut set = ProblemSet::default();
        set.report("theme", Problem::warning("Theme problem"));
        set.report("connection", offline());
        let keys: Vec<_> = set.visible().iter().map(|(key, _)| *key).collect();
        assert_eq!(keys, ["connection", "theme"]);
    }

    #[test]
    fn dismissing_hides_until_the_wording_changes() {
        let mut set = ProblemSet::default();
        set.report("connection", offline());
        assert!(set.dismiss("connection"));
        assert!(set.visible().is_empty());
        assert!(
            !set.report("connection", offline()),
            "the same problem stays hidden"
        );
        assert!(set.visible().is_empty());
        set.report(
            "connection",
            Problem::error("Chat is offline").line("Retry 2"),
        );
        assert_eq!(set.visible().len(), 1, "news shows again");
    }

    #[test]
    fn a_cleared_cause_shows_again_when_it_returns() {
        let mut set = ProblemSet::default();
        set.report("connection", offline());
        set.dismiss("connection");
        assert!(set.clear("connection"));
        assert!(!set.clear("connection"), "nothing left to clear");
        set.report("connection", offline());
        assert_eq!(set.visible().len(), 1);
    }

    #[test]
    fn dismissing_what_is_not_reported_does_nothing() {
        let mut set = ProblemSet::default();
        assert!(!set.dismiss("nothing"));
        set.report("nothing", offline());
        assert_eq!(set.visible().len(), 1, "no stale dismissal waits for it");
    }

    #[test]
    fn paths_are_cut_to_file_names_and_other_words_stay() {
        assert_eq!(
            plain("theme /home/me/.config/app/themes/a.toml:1:6: key with no value"),
            "theme a.toml:1:6: key with no value"
        );
        assert_eq!(plain("no paths here, just 1/2"), "no paths here, just 1/2");
    }

    #[test]
    fn the_report_carries_title_lines_and_raw_detail() {
        let text = offline().report("app", "1.2.3");
        assert!(text.starts_with("app 1.2.3\nChat is offline\nRetrying"));
        assert!(text.ends_with("connection refused"));
        assert!(!Problem::info("x").report("a", "1").contains("\n\n"));
    }
}
