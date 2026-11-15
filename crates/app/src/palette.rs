//! The command palette (⌘K) and the quick channel switcher (⌘T), PR 4.3.
//!
//! Both are the same thing: a Kit `Dialog` holding a Kit `Command` (a search field over a list,
//! ↑/↓ to move, Enter to choose, Esc to close) whose own substring filter is off, so the rows are
//! ranked here with `gpui_chat_core::complete::rank` (case-sensitive prefix, prefix, camelCase
//! hump, substring, subsequence: `sr` finds *Split Right*, `fo` finds *forsen* before *xfoo*).
//! With nothing typed the rows keep the order they were given in.
//!
//! The palette lists every action of [`keymap::catalog`] marked for it, each with the shortcut it
//! has *now* (read from the keymap, so a rebound key shows). Choosing one closes the dialog, hands
//! focus back to where it was and dispatches the action there, exactly as the key would.

use std::rc::Rc;

use gpui_chat_core::complete::rank;
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, div, px,
};

use crate::keymap;

/// Most rows kept for a query.
const LIMIT: usize = 100;
const WIDTH: f32 = 560.0;
const MARGIN_TOP: f32 = 80.0;
const LIST_HEIGHT: f32 = 360.0;

/// One row.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The row's text; what a query is matched against, together with `keywords`.
    pub label: SharedString,
    /// Muted text after the label (its group, or where a channel is shown).
    pub detail: Option<SharedString>,
    /// The shortcut at the right end.
    pub shortcut: Option<SharedString>,
    /// Matched as well as the label, so a group name finds its actions.
    pub keywords: String,
}

impl Entry {
    /// What a query is matched against.
    fn haystack(&self) -> String {
        format!("{} {}", self.label, self.keywords)
    }
}

/// Entries of the command palette (every listed action, with its shortcut right now) and, in the
/// same order, the actions they stand for.
pub fn palette_entries(cx: &App) -> (Vec<Entry>, Vec<&'static keymap::ActionInfo>) {
    let mut entries = Vec::new();
    let mut actions = Vec::new();
    for info in keymap::catalog() {
        if !info.palette {
            continue;
        }
        entries.push(Entry {
            label: info.label.into(),
            detail: Some(info.group.into()),
            shortcut: keymap::shortcut(info.build().as_ref(), cx).map(Into::into),
            keywords: format!("{} {}", info.group, info.id),
        });
        actions.push(info);
    }
    (entries, actions)
}

/// The indices of `entries` that match `query`, best first (`rank`), or all of them in order for
/// an empty query.
pub fn ranked(query: &str, entries: &[Entry]) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..entries.len().min(LIMIT)).collect();
    }
    let haystacks: Vec<String> = entries.iter().map(Entry::haystack).collect();
    rank(query.trim(), &haystacks, |h| h.as_str(), |_| None, LIMIT)
}

/// What choosing a row does: gets the entry's index, with the dialog already closed.
type OnPick = dyn Fn(usize, &mut Window, &mut App);

/// The dialog's content: the search field and the ranked rows.
struct Picker {
    state: Entity<CommandState>,
    entries: Vec<Entry>,
    query: String,
    placeholder: SharedString,
    empty: SharedString,
    on_pick: Rc<OnPick>,
}

impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let order = Rc::new(ranked(&self.query, &self.entries));
        let items: Vec<CommandItem> = order
            .iter()
            .map(|&index| row(&self.entries[index]))
            .collect();
        let this = cx.entity().downgrade();
        let on_pick = self.on_pick.clone();
        let empty = self.empty.clone();
        Command::new(&self.state)
            .filterable(false)
            .bordered(false)
            .placeholder(self.placeholder.clone())
            .max_h(px(LIST_HEIGHT))
            .items(items)
            .empty(move |_, _, cx| {
                div()
                    .p_4()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(empty.clone())
            })
            .on_query(move |query, _, cx| {
                let query = query.to_owned();
                this.update(cx, |picker, cx| {
                    if picker.query != query {
                        picker.query = query;
                        cx.notify();
                    }
                })
                .ok();
            })
            .on_confirm(move |index, window, cx| {
                if let Some(&entry) = order.get(index.row) {
                    // Away first, so the pick runs with focus where it was before the dialog.
                    window.close_dialog(cx);
                    on_pick(entry, window, cx);
                }
            })
    }
}

fn row(entry: &Entry) -> CommandItem {
    let (label, detail, shortcut) = (
        entry.label.clone(),
        entry.detail.clone(),
        entry.shortcut.clone(),
    );
    CommandItem::new()
        .label(entry.label.clone())
        .child(move |_, cx| {
            let muted = cx.theme().muted_foreground;
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .items_baseline()
                .child(div().flex_none().child(label.clone()))
                .children(
                    detail
                        .clone()
                        .map(|detail| div().text_xs().text_color(muted).child(detail)),
                )
                .child(div().flex_1())
                .children(
                    shortcut
                        .clone()
                        .map(|shortcut| div().flex_none().text_xs().opacity(0.7).child(shortcut)),
                )
        })
}

/// Opens the dialog over `entries`; `on_pick` gets the chosen entry's index once the dialog is
/// closed. Does nothing while a dialog is already open.
pub fn open(
    entries: Vec<Entry>,
    placeholder: &'static str,
    empty: &'static str,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let state = cx.new(|cx| CommandState::new(window, cx));
    let picker = cx.new(|_| Picker {
        state: state.clone(),
        entries,
        query: String::new(),
        placeholder: placeholder.into(),
        empty: empty.into(),
        on_pick: Rc::new(on_pick),
    });
    // The builder runs again whenever the dialog layer rebuilds; focus only on the first run.
    let focus_pending = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus_pending.replace(false) {
            state.update(cx, |state, cx| state.focus(window, cx));
        }
        dialog
            .width(px(WIDTH))
            .margin_top(px(MARGIN_TOP))
            .close_button(false)
            .child(picker.clone())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str, keywords: &str) -> Entry {
        Entry {
            label: label.to_owned().into(),
            detail: None,
            shortcut: None,
            keywords: keywords.to_owned(),
        }
    }

    fn labels(query: &str, entries: &[Entry]) -> Vec<String> {
        ranked(query, entries)
            .into_iter()
            .map(|i| entries[i].label.to_string())
            .collect()
    }

    #[test]
    fn an_empty_query_keeps_the_given_order() {
        let entries = [entry("Split Right", ""), entry("Add Channel…", "")];
        assert_eq!(labels("", &entries), ["Split Right", "Add Channel…"]);
        assert_eq!(labels("  ", &entries), ["Split Right", "Add Channel…"]);
    }

    #[test]
    fn matching_is_fuzzy_and_best_first() {
        let entries = [
            entry("Toggle Sidebar", "View ToggleSidebar"),
            entry("Split Right", "Pane SplitRight"),
            entry("Split Down", "Pane SplitDown"),
            entry("Close Pane", "Pane ClosePane"),
            entry("Log In to Twitch…", "Account LogIn"),
        ];
        // A prefix beats a subsequence; both are found.
        let split = labels("split", &entries);
        assert_eq!(split.len(), 2, "{split:?}");
        assert!(
            split.contains(&"Split Right".to_owned()) && split.contains(&"Split Down".to_owned())
        );
        assert_eq!(labels("sr", &entries)[0], "Split Right");
        assert_eq!(labels("cp", &entries), ["Close Pane"]);
        // The group name and the action's id are searchable too.
        assert_eq!(labels("account", &entries), ["Log In to Twitch…"]);
        assert_eq!(labels("ToggleSidebar", &entries), ["Toggle Sidebar"]);
        assert!(labels("zzz", &entries).is_empty());
    }
}
