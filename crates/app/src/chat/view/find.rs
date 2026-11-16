//! The find bar, the filters and the hidden rows of a chat view (PR 5.7).
//!
//! * **Find** (⌘F): a bar over the list with a text field, the match count, previous / next and the
//!   filter toggles. The text is searched with [`gpui_chat_core::Search`], which is incremental (see
//!   there): typing only looks at what matched before, a new message is looked at once, so the bar
//!   stays smooth with the biggest scrollback and a firehose. Stepping goes through the matches
//!   (Enter / ↓ newer, Shift-Enter / ↑ older, wrapping) with the same scroll as a jump from the
//!   Mentions view; matches are drawn behind the text in the theme's warning colour, the current one
//!   stronger ([`SearchHighlight`], painted by the message element). `from:login` in the text is a
//!   filter, not a search term.
//! * **Filters** ([`gpui_chat_core::Filters`]): only one user's messages, only mentions and
//!   highlights, only links, no bots. They hide rows (the list is [`Rows`], an explicit list of the
//!   messages that are drawn), and a strip says "Showing N of M messages" with a button that clears
//!   them, whether the bar is open or not. A jump to a hidden message clears the filters first.
//! * **Ignores** ([`crate::ignores`]): ignored messages have no row either, or, with
//!   `ignores.show_collapsed`, one line "1 ignored message" that a click opens.
//!
//! Esc in the bar closes it and gives the input box focus back (the pane does that when it hears
//! [`ChatViewEvent::SelectionDismissed`]); it does not touch the box's own Esc chain (completion
//! list, then reply mode), which only runs while the box is focused.

use gpui_chat_core::{Filters, Needle, Visibility, parse_query};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    Disableable as _, Icon, IconName, Selectable as _, Sizable as _, h_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, Styled as _, Subscription, Window, div, px,
};

use super::{ChatView, ChatViewEvent, Splice};
use crate::actions::{FindClose, FindNext, FindPrevious};
use crate::ignores;
use crate::theme::Tokens;

/// Key context of the find bar; its Esc / ↑ / ↓ bindings are scoped to the text field inside it
/// (see [`bindings`]) so they win over the field's own.
pub const FIND_CONTEXT: &str = "FindBar";

/// The keys of the find bar's text field.
pub fn bindings() -> Vec<gpui_kit::KeyBinding> {
    let context = Some("FindBar > Input");
    vec![
        gpui_kit::KeyBinding::new("escape", FindClose, context),
        gpui_kit::KeyBinding::new("up", FindPrevious, context),
        gpui_kit::KeyBinding::new("down", FindNext, context),
    ]
}

/// What the rows draw for the text being searched for.
#[derive(Debug, Clone)]
pub struct SearchHighlight {
    pub needle: Needle,
    /// The match the list was scrolled to.
    pub current: Option<u64>,
    /// Where each row's matches are, remembered while the row's layout stays the same (the rows
    /// ask every frame; the text and the needle change far less often).
    pub ranges: std::rc::Rc<std::cell::RefCell<RangeCache>>,
}

/// The matches in the rows drawn lately, by message and layout (the address of the shaped message,
/// which a new layout replaces).
/// The byte ranges of the matches in one row.
pub type Ranges = std::rc::Rc<[std::ops::Range<usize>]>;

#[derive(Debug, Default)]
pub struct RangeCache {
    entries: std::collections::HashMap<u64, (usize, Ranges)>,
}

impl RangeCache {
    /// Most rows kept; more than a screenful of rows is more than a frame ever asks for.
    const LIMIT: usize = 512;

    /// The matches of `needle` in `text`, the display text of message `seq` as laid out at
    /// `layout` (an address that changes whenever the layout does).
    pub fn get(
        &mut self,
        seq: u64,
        layout: usize,
        text: &str,
        needle: &Needle,
    ) -> std::rc::Rc<[std::ops::Range<usize>]> {
        if let Some((at, ranges)) = self.entries.get(&seq)
            && *at == layout
        {
            return ranges.clone();
        }
        if self.entries.len() >= Self::LIMIT {
            self.entries.clear();
        }
        let ranges: std::rc::Rc<[std::ops::Range<usize>]> =
            gpui_chat_core::find_ranges(text, needle).into();
        self.entries.insert(seq, (layout, ranges.clone()));
        ranges
    }
}

/// An open find bar.
pub(super) struct Find {
    pub(super) input: Entity<InputState>,
    /// The match the list was last scrolled to; stepping goes on from it.
    pub(super) current: Option<u64>,
    /// The text last read from the field.
    text: String,
    /// Waits a moment after a `from:` token was typed before filtering by it, so each prefix of
    /// a login does not filter the whole chat (see [`SETTLE`]).
    settle: Option<gpui_kit::Task<()>>,
    /// The highlight cache of the rows; a new one whenever the query changes.
    ranges: std::rc::Rc<std::cell::RefCell<RangeCache>>,
    /// Text to put in the field at the next draw (a filter set from outside rewrites `from:`).
    pending_text: Option<String>,
    _subscription: Subscription,
}

/// How long a `from:` token that is still being typed waits before it filters (Enter or a space
/// after it does not wait).
const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether the last word of `text` is a `from:` token the user may still be typing.
fn from_is_unfinished(text: &str) -> bool {
    !text.ends_with(char::is_whitespace)
        && text
            .split_whitespace()
            .next_back()
            .and_then(|word| word.get(..5))
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("from:"))
}

/// What the bar says about the matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindStatus {
    /// Matches of the text, among the rows shown.
    pub matches: usize,
    /// The position (from 1) of the match the list was scrolled to.
    pub position: Option<usize>,
    /// Whether there is anything to look for.
    pub searching: bool,
}

impl FindStatus {
    /// `3 of 12`, `12 matches`, `No matches`, or nothing while the field is empty.
    pub fn label(&self) -> Option<String> {
        if !self.searching {
            return None;
        }
        Some(match (self.matches, self.position) {
            (0, _) => "No matches".to_owned(),
            (n, Some(at)) => format!("{at} of {n}"),
            (1, None) => "1 match".to_owned(),
            (n, None) => format!("{n} matches"),
        })
    }
}

/// `Showing 12 of 1000 messages · from alice · mentions`.
/// `total` counts every message the chat holds; with something ignored that includes the ignored
/// ones (they are never counted as hidden by the filter), and the strip says so.
pub fn strip_label(shown: usize, total: usize, filters: &Filters, ignoring: bool) -> String {
    format!(
        "Showing {shown} of {total} message{}{} · {}",
        if total == 1 { "" } else { "s" },
        if ignoring { " (incl. ignored)" } else { "" },
        filters.describe()
    )
}

impl ChatView {
    /// Whether message `id` is in the scrollback but has no row because it is ignored (nothing else
    /// hides it once the filters are off).
    pub fn is_hidden_by_ignore(&self, id: &gpui_chat_core::MessageId, cx: &App) -> bool {
        self.store
            .read(cx)
            .messages()
            .seq_of(id.as_str())
            .is_some_and(|seq| !self.rows.borrow().contains(seq))
    }
}

impl ChatView {
    /// The text being searched for, for the rows to draw.
    pub(super) fn search_highlight(&self) -> Option<std::rc::Rc<SearchHighlight>> {
        let find = self.find.as_ref()?;
        let needle = self.search.needle()?.clone();
        Some(std::rc::Rc::new(SearchHighlight {
            needle,
            current: find.current,
            ranges: find.ranges.clone(),
        }))
    }

    /// Whether the find bar is open.
    #[cfg(test)]
    pub fn find_is_open(&self) -> bool {
        self.find.is_some()
    }

    /// What the bar says about the matches (`None` while it is closed).
    pub fn find_status(&self) -> Option<FindStatus> {
        let find = self.find.as_ref()?;
        Some(FindStatus {
            matches: self.search.count(),
            position: find
                .current
                .and_then(|seq| self.search.position_of(seq))
                .map(|at| at + 1),
            searching: self.search.needle().is_some(),
        })
    }

    /// The filters in force.
    #[cfg(test)]
    pub fn filters(&self) -> &Filters {
        &self.filters
    }

    /// How many rows are drawn, and how many messages the chat holds.
    pub fn row_counts(&self, cx: &App) -> (usize, usize) {
        (
            self.rows.borrow().len(),
            self.store.read(cx).messages().len(),
        )
    }

    /// Opens the find bar, or, when it is open, selects its text again (⌘F twice). It takes focus.
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = &self.find {
            let input = find.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        // A user filter that outlived the last bar is where this one starts.
        let start = self
            .filters
            .from
            .as_ref()
            .map_or_else(String::new, |from| format!("from:{from} "));
        input.update(cx, |input, cx| {
            input.set_placeholder("Find in chat  (from:user filters)", window, cx);
            if !start.is_empty() {
                input.set_value(start.clone(), window, cx);
            }
            input.focus(window, cx);
        });
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => this.find_text_changed(false, cx),
                    InputEvent::PressEnter { shift, .. } => {
                        // Enter finishes a `from:` token being typed, then steps.
                        this.find_text_changed(true, cx);
                        this.find_step(!*shift, cx);
                    }
                    InputEvent::Focus | InputEvent::Blur => {}
                },
            );
        // The index outlives the bar (it is only brought up to date here, not rebuilt).
        let rows = self.rows.clone();
        self.search
            .sync(self.store.read(cx).messages(), &move |seq| {
                rows.borrow().is_shown(seq)
            });
        self.find = Some(Find {
            input,
            current: None,
            text: start,
            settle: None,
            ranges: Default::default(),
            pending_text: None,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Closes the bar and the search (the filters stay: the strip says so and clears them) and hands
    /// focus back to the input box.
    pub fn close_find(&mut self, cx: &mut Context<Self>) -> bool {
        if self.find.take().is_none() {
            return false;
        }
        // No query, no matches; the index stays for the next time.
        self.search.set_query("", &|_| false);
        cx.emit(ChatViewEvent::SelectionDismissed);
        cx.notify();
        true
    }

    /// The field's text changed. `settled`: apply a `from:` token even if it may still be being
    /// typed (Enter, or the pause after the last key); otherwise such a token waits for [`SETTLE`]
    /// so each prefix of a login does not filter the whole chat.
    pub(super) fn find_text_changed(&mut self, settled: bool, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else {
            return;
        };
        let text = find.input.read(cx).value().to_string();
        if text == find.text && !settled {
            return;
        }
        find.text = text.clone();
        let unfinished = !settled && from_is_unfinished(&text);
        // A token still being typed is neither searched for nor applied yet.
        let searched = if unfinished {
            text.rsplit_once(char::is_whitespace)
                .map_or("", |(rest, _)| rest)
                .to_owned()
        } else {
            text.clone()
        };
        let typed = parse_query(&text);
        let query = parse_query(&searched);
        let from = if unfinished {
            self.filters.from.clone()
        } else {
            typed.from.clone()
        };
        find.settle = (unfinished && typed.from != self.filters.from).then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SETTLE).await;
                this.update(cx, |this, cx| this.find_text_changed(true, cx))
                    .ok();
            })
        });
        let mut filters = self.filters.clone();
        filters.set_from(from.as_deref());
        if filters != self.filters {
            self.filters = filters;
            self.update_rows(cx);
            // The matches are among the rows shown: look again.
            self.after_rows_changed(cx);
        }
        let rows = self.rows.clone();
        let allowed = move |seq: u64| rows.borrow().is_shown(seq);
        let before = self.search.needle().cloned();
        self.search.set_query(&query.text, &allowed);
        let Some(find) = &mut self.find else {
            return;
        };
        if self.search.needle() != before.as_ref() {
            find.ranges = Default::default();
        }
        // Typing keeps the match the user is on while it still matches, else goes to the newest.
        let keep = find
            .current
            .is_some_and(|seq| self.search.position_of(seq).is_some());
        let target = if keep {
            None
        } else {
            find.current = None;
            self.search.step(None, false)
        };
        if let Some(seq) = target {
            find.current = Some(seq);
            self.jump_to_match(seq, cx);
        }
        cx.notify();
    }

    /// Goes to the next (`forward`: newer) or previous match, wrapping round.
    pub fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else {
            return;
        };
        let target = self.search.step(find.current, forward);
        if let Some(seq) = target {
            find.current = Some(seq);
            self.jump_to_match(seq, cx);
        }
        cx.notify();
    }

    /// Takes the user to match `seq`: the list scrolls to it with the Mentions view's jump, unless
    /// it is on screen already (the newest match, usually: nothing to scroll, and nothing to hold
    /// the list against), in which case it only flashes.
    fn jump_to_match(&mut self, seq: u64, cx: &mut Context<Self>) {
        let Some(ix) = self.rows.borrow().index_of(seq) else {
            return;
        };
        if self.surface.is_fully_visible(seq) {
            self.flash_row(seq, cx);
        } else {
            self.scroll_to_row(ix, seq, cx);
        }
    }

    /// Takes the store's new messages into the search (after the rows took them).
    pub(super) fn sync_find(&mut self, cx: &mut Context<Self>) {
        let rows = self.rows.clone();
        let allowed = move |seq: u64| rows.borrow().is_shown(seq);
        let store = self.store.read(cx).messages();
        let Some(find) = &mut self.find else {
            return;
        };
        self.search.sync(store, &allowed);
        // The match the user was on left the scrollback.
        if find
            .current
            .is_some_and(|seq| self.search.position_of(seq).is_none())
        {
            find.current = None;
        }
    }

    /// Messages were replaced in place (`None`: too many to name): their text is read again.
    pub(super) fn find_replaced(&mut self, seqs: Option<&[u64]>, cx: &mut Context<Self>) {
        if self.find.is_none() {
            // The index is not kept up to date while the bar is closed: read it all at the next ⌘F.
            self.search.invalidate_all();
            return;
        }
        let rows = self.rows.clone();
        let allowed = move |seq: u64| rows.borrow().is_shown(seq);
        match seqs {
            Some(seqs) => {
                let store = self.store.read(cx).messages();
                self.search.replaced(store, seqs, &allowed);
            }
            None => {
                self.search.invalidate_all();
                self.sync_find(cx);
            }
        }
    }

    // ---- filters ----

    /// Replaces the filters.
    pub fn set_filters(&mut self, filters: Filters, cx: &mut Context<Self>) {
        if filters == self.filters {
            return;
        }
        let from_changed = filters.from != self.filters.from;
        self.filters = filters;
        self.update_rows(cx);
        // The field's text follows a `from:` set from outside (the clear button, a menu).
        if from_changed {
            self.show_from_in_field();
        }
        self.after_rows_changed(cx);
    }

    /// Switches every filter off.
    pub fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.set_filters(Filters::default(), cx);
    }

    /// Shows only `login`'s messages (or everyone's again with `None`).
    pub fn filter_from(&mut self, login: Option<&str>, cx: &mut Context<Self>) {
        let mut filters = self.filters.clone();
        filters.set_from(login);
        self.set_filters(filters, cx);
    }

    pub fn toggle_mentions_filter(&mut self, cx: &mut Context<Self>) {
        let mut filters = self.filters.clone();
        filters.mentions = !filters.mentions;
        self.set_filters(filters, cx);
    }

    pub fn toggle_links_filter(&mut self, cx: &mut Context<Self>) {
        let mut filters = self.filters.clone();
        filters.links = !filters.links;
        self.set_filters(filters, cx);
    }

    pub fn toggle_bots_filter(&mut self, cx: &mut Context<Self>) {
        let mut filters = self.filters.clone();
        filters.hide_bots = !filters.hide_bots;
        self.set_filters(filters, cx);
    }

    /// Rewrites the `from:` token in the field to match the filter.
    fn show_from_in_field(&mut self) {
        let Some(find) = &mut self.find else {
            return;
        };
        let query = parse_query(&find.text);
        let text = match &self.filters.from {
            Some(from) => format!("from:{from} {}", query.text),
            None => query.text,
        };
        find.text = text.clone();
        // Written at the next draw, which has the window the field needs.
        find.pending_text = Some(text);
    }

    /// The rows follow the search: its matches are among the rows shown.
    fn after_rows_changed(&mut self, cx: &mut Context<Self>) {
        let rows = self.rows.clone();
        let allowed = move |seq: u64| rows.borrow().is_shown(seq);
        if let Some(find) = &mut self.find {
            self.search.refilter(&allowed);
            if find
                .current
                .is_some_and(|seq| self.search.position_of(seq).is_none())
            {
                find.current = None;
            }
        }
        cx.notify();
    }

    // ---- rows ----

    pub(super) fn apply_splices(&self, splices: &[Splice]) {
        for splice in splices {
            self.list
                .splice(splice.at..splice.at + splice.remove, splice.insert);
        }
    }

    /// The ignores or the filters changed: the rows are the messages that pass them now.
    pub(super) fn update_rows(&mut self, cx: &mut Context<Self>) {
        // Anything the store did since the last frame first, so both start from the same numbers.
        self.sync(cx);
        let (first, next) = {
            let messages = self.store.read(cx).messages();
            (messages.first_seq(), messages.next_seq())
        };
        let splices = {
            let messages = self.store.read(cx).messages();
            let shows = |seq: u64| {
                messages
                    .get_by_seq(seq)
                    .map_or(Visibility::Shown, |m| self.verdict(seq, m))
            };
            self.rows.borrow_mut().show_only(
                self.hiding()
                    .then_some(&shows as &dyn Fn(u64) -> Visibility),
                first,
                next,
            )
        };
        self.apply_splices(&splices);
        debug_assert_eq!(self.list.item_count(), self.rows.borrow().len());
        cx.notify();
    }

    /// The hub says what is ignored differently: rows come and go.
    pub(super) fn ignores_changed(&mut self, cx: &mut Context<Self>) {
        let Some(hub) = ignores::current(cx) else {
            return;
        };
        let (engine, bots, generation) = {
            let hub = hub.read(cx);
            (hub.engine(), hub.bots(), hub.generation())
        };
        if generation == self.ignore_generation {
            return;
        }
        self.ignore_generation = generation;
        let collapsing = self.engine.show_collapsed() || engine.show_collapsed();
        self.engine = engine;
        self.bots = bots;
        // The verdicts remembered are the old ignores'.
        self.ignore_cache.borrow_mut().clear();
        self.update_rows(cx);
        if collapsing {
            // A message is drawn as one line or in full: another height.
            self.list.remeasure();
        }
        self.after_rows_changed(cx);
    }

    // ---- ignoring from a row ----

    /// The login and Twitch id of the author of the first selected message (the actions that act on
    /// "the selected message": ignore, unignore, block).
    pub fn selected_author(&self, cx: &App) -> Option<(String, Option<String>)> {
        let seq = self
            .thread
            .as_ref()
            .and_then(|panel| panel.surface.selection_start())
            .or_else(|| self.surface.selection_start())?;
        let message = self.store.read(cx).messages().get_by_seq(seq)?.clone();
        let author = message.author.as_ref()?;
        Some((author.login.clone(), author.user_id.clone()))
    }

    /// Ignores `login` (`[ignores] users`); a failure to write the file is logged.
    pub fn ignore_user(&mut self, login: &str, cx: &mut Context<Self>) {
        if let Some(hub) = ignores::current(cx) {
            let login = login.to_owned();
            let result = hub.update(cx, |hub, cx| hub.ignore_user(&login, cx));
            if let Err(error) = result {
                tracing::warn!(%error, %login, "could not ignore the user");
            }
        }
    }

    /// Stops ignoring `login`.
    pub fn unignore_user(&mut self, login: &str, cx: &mut Context<Self>) {
        if let Some(hub) = ignores::current(cx) {
            let login = login.to_owned();
            let result = hub.update(cx, |hub, cx| hub.unignore_user(&login, cx));
            if let Err(error) = result {
                tracing::warn!(%error, %login, "could not stop ignoring the user");
            }
        }
    }

    // ---- drawing ----

    /// The find bar, when it is open.
    pub(super) fn find_bar(
        &mut self,
        tokens: &Tokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let status = self.find_status()?;
        let find = self.find.as_mut()?;
        if let Some(text) = find.pending_text.take() {
            find.input
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
        let find = self.find.as_ref()?;
        let filters = &self.filters;
        let view = cx.entity().downgrade();
        let toggle = |id: &'static str,
                      label: &'static str,
                      on: bool,
                      run: fn(&mut ChatView, &mut Context<ChatView>)| {
            let view = view.clone();
            Button::new(id)
                .xsmall()
                .ghost()
                .label(label)
                .selected(on)
                .on_click(move |_, _, cx| {
                    view.update(cx, run).ok();
                })
        };
        let step = |id: &'static str, icon: IconName, tip: &'static str, forward: bool| {
            let view = view.clone();
            Button::new(id)
                .xsmall()
                .ghost()
                .icon(Icon::new(icon))
                .tooltip(tip)
                .accessibility_label(tip)
                .disabled(status.matches == 0)
                .on_click(move |_, _, cx| {
                    view.update(cx, |view, cx| view.find_step(forward, cx)).ok();
                })
        };
        let close = {
            let view = view.clone();
            Button::new("find-close")
                .xsmall()
                .ghost()
                .icon(Icon::new(IconName::Close))
                .tooltip("Close (Esc)")
                .accessibility_label("Close find bar")
                .on_click(move |_, _, cx| {
                    view.update(cx, |view, cx| {
                        view.close_find(cx);
                    })
                    .ok();
                })
        };
        let label: Option<SharedString> = status.label().map(Into::into);
        Some(
            h_flex()
                .id("find-bar")
                .key_context(FIND_CONTEXT)
                .on_action(cx.listener(|this, _: &FindClose, _, cx| {
                    this.close_find(cx);
                }))
                .w_full()
                .flex_none()
                .gap_1()
                .px_2()
                .py_1()
                .items_center()
                .border_b_1()
                .border_color(tokens.border)
                .bg(tokens.surface)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&find.input).small()),
                )
                .children(label.map(|label| {
                    div()
                        .flex_none()
                        .min_w(px(64.0))
                        .text_xs()
                        .text_color(tokens.text_muted)
                        .child(label)
                }))
                .child(step(
                    "find-previous",
                    IconName::ChevronUp,
                    "Previous match (Shift-Enter)",
                    false,
                ))
                .child(step(
                    "find-next",
                    IconName::ChevronDown,
                    "Next match (Enter)",
                    true,
                ))
                .child(toggle(
                    "find-mentions",
                    "Mentions",
                    filters.mentions,
                    ChatView::toggle_mentions_filter,
                ))
                .child(toggle(
                    "find-links",
                    "Links",
                    filters.links,
                    ChatView::toggle_links_filter,
                ))
                .child(toggle(
                    "find-bots",
                    "No bots",
                    filters.hide_bots,
                    ChatView::toggle_bots_filter,
                ))
                .child(close)
                .into_any_element(),
        )
    }

    /// "Showing N of M messages · filters · Clear" while any filter is on.
    pub(super) fn filter_strip(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.filters.is_active() {
            return None;
        }
        let (shown, total) = self.row_counts(cx);
        let view = cx.entity().downgrade();
        Some(
            h_flex()
                .id("filter-strip")
                .w_full()
                .flex_none()
                .gap_2()
                .px_3()
                .py_0p5()
                .items_center()
                .bg(tokens.accent.opacity(0.14))
                .text_xs()
                .child(div().flex_1().min_w_0().truncate().child(strip_label(
                    shown,
                    total,
                    &self.filters,
                    !self.engine.is_empty(),
                )))
                .child(
                    Button::new("filter-clear")
                        .xsmall()
                        .ghost()
                        .label("Clear filter")
                        .tab_stop(false)
                        .on_click(move |_, _, cx| {
                            view.update(cx, |view, cx| view.clear_filters(cx)).ok();
                        }),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_reads_as_a_person_would_say_it() {
        let status = |matches, position, searching| FindStatus {
            matches,
            position,
            searching,
        };
        assert_eq!(status(0, None, false).label(), None);
        assert_eq!(status(0, None, true).label().as_deref(), Some("No matches"));
        assert_eq!(
            status(12, Some(3), true).label().as_deref(),
            Some("3 of 12")
        );
        assert_eq!(status(1, None, true).label().as_deref(), Some("1 match"));
        assert_eq!(
            status(12, None, true).label().as_deref(),
            Some("12 matches")
        );
    }

    #[test]
    fn the_strip_says_what_is_shown_and_which_filters_hide_the_rest() {
        let mut filters = Filters::default();
        filters.set_from(Some("alice"));
        filters.mentions = true;
        assert_eq!(
            strip_label(12, 1000, &filters, false),
            "Showing 12 of 1000 messages · from alice · mentions"
        );
        assert_eq!(
            strip_label(1, 1, &filters, false),
            "Showing 1 of 1 message · from alice · mentions"
        );
        assert_eq!(
            strip_label(3, 40, &filters, true),
            "Showing 3 of 40 messages (incl. ignored) · from alice · mentions"
        );
    }
}
