//! The Mentions view (PR 5.3): a panel beside the chat listing the recent highlight hits of every
//! channel, newest first, each with its channel, time and author; a click shows the message in its
//! channel. It is drawn from [`crate::activity::Activity`]'s log (the rows are copied out so the
//! panel owns its data, rebuilt only when the log's generation moved), and the shell decides what a
//! click does. The list is virtualised (`uniform_list`, fixed row height), so only the rows on
//! screen are built each frame however many hits are logged.

use std::rc::Rc;

use gpui_chat_core::{ChannelId, HighlightColor, MessageId};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};

use crate::activity::MentionEntry;
use crate::chat::display::{author_name, clock};
use crate::chat::element::Palette;
use crate::theme::Tokens;

/// Width of the panel.
pub const WIDTH: f32 = 340.0;
/// Rows listed (the log keeps up to 500; the oldest are not worth scrolling to).
const SHOWN: usize = 200;
/// Height of a row: the channel line and two lines of text, with padding.
const ROW_HEIGHT: f32 = 76.0;

/// One row of the panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub channel: ChannelId,
    pub id: MessageId,
    pub author: String,
    /// `HH:MM`.
    pub time: String,
    pub text: SharedString,
    pub color: HighlightColor,
    pub unseen: bool,
}

/// The rows for `entries` (newest first).
pub fn rows<'a>(entries: impl Iterator<Item = &'a MentionEntry>) -> Vec<Row> {
    entries
        .take(SHOWN)
        .map(|entry| {
            let message = &entry.message;
            Row {
                channel: entry.channel.clone(),
                id: message.id.clone(),
                author: message
                    .author
                    .as_ref()
                    .map_or_else(String::new, |author| author_name(author).to_owned()),
                time: clock(message.timestamp.unix_millis()),
                text: SharedString::from(entry.text.clone()),
                color: entry.color,
                unseen: entry.unseen,
            }
        })
        .collect()
}

/// The rows the panel shows, held still while the pointer is over it. The log is newest first and
/// grows at the top, so a row under the pointer would slide away between seeing it and clicking it:
/// while held, the rows stay as they were and the hits that arrive meanwhile are only counted (the
/// panel offers "N new" to show them); leaving the panel lets go.
#[derive(Default)]
pub struct Feed {
    shown: Option<Rc<Vec<Row>>>,
    /// The log generation `shown` was built from.
    generation: u64,
    /// [`crate::activity::Activity::added`] when `shown` was built.
    added: u64,
    /// The pointer is over the panel and the rows are pinned.
    pinned: bool,
}

impl Feed {
    /// The rows to show this frame and how many hits are waiting behind them. `held`: the pointer
    /// is on the panel. `build` makes the rows from the log; it only runs when the rows change.
    pub fn update(
        &mut self,
        held: bool,
        generation: u64,
        added: u64,
        build: impl FnOnce() -> Vec<Row>,
    ) -> (Rc<Vec<Row>>, usize) {
        self.pinned = held && self.shown.is_some();
        let stale = self.shown.is_none() || (!self.pinned && self.generation != generation);
        if stale {
            self.shown = Some(Rc::new(build()));
            self.generation = generation;
            self.added = added;
            self.pinned = held;
        }
        let rows = self.shown.clone().unwrap_or_default();
        let waiting = if self.pinned {
            usize::try_from(added.saturating_sub(self.added)).unwrap_or(usize::MAX)
        } else {
            0
        };
        (rows, waiting)
    }

    /// "N new" was clicked: show what is waiting (the rows are rebuilt at the next frame and the
    /// pointer, still on the panel, holds that list).
    pub fn reveal(&mut self) {
        self.shown = None;
    }
}

/// Shows a row's message in its channel.
pub type Pick = Rc<dyn Fn(&ChannelId, &MessageId, &mut Window, &mut App)>;

/// A button's action.
pub type Callback = Rc<dyn Fn(&mut Window, &mut App)>;
/// The pointer entering (`true`) or leaving the panel.
pub type HoverCallback = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// What the panel's controls do.
pub struct PanelActions {
    pub on_pick: Pick,
    pub on_close: Callback,
    /// The pointer entered (`true`) or left the panel.
    pub on_hover: HoverCallback,
    /// "N new" was clicked.
    pub on_reveal: Callback,
}

/// The panel. `waiting` hits are held back behind the rows (see [`Feed`]).
pub fn panel(
    rows: Rc<Vec<Row>>,
    waiting: usize,
    scroll: &UniformListScrollHandle,
    tokens: &Tokens,
    actions: PanelActions,
) -> AnyElement {
    let PanelActions {
        on_pick,
        on_close,
        on_hover,
        on_reveal,
    } = actions;
    let palette = Palette::from_tokens(tokens);
    let unseen = rows.iter().filter(|row| row.unseen).count();
    let header = h_flex()
        .flex_none()
        .w_full()
        .px_3()
        .py_2()
        .gap_2()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(tokens.border)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Mentions"))
                .child(
                    div()
                        .text_xs()
                        .text_color(tokens.text_muted)
                        .child(if unseen == 0 {
                            format!("{}", rows.len())
                        } else {
                            format!("{unseen} unseen of {}", rows.len())
                        }),
                ),
        )
        .child(
            Button::new("mentions-close")
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Close).size_4())
                .accessibility_label("Close Mentions")
                .on_click(move |_, window, cx| on_close(window, cx)),
        );
    // Hits that came while the pointer was on the panel wait here (see `Feed`).
    let waiting_bar = (waiting > 0).then(|| {
        div()
            .id("mentions-waiting")
            .flex_none()
            .w_full()
            .px_3()
            .py_1()
            .flex()
            .justify_center()
            .bg(tokens.accent)
            .text_color(tokens.accent_text)
            .text_sm()
            .cursor_pointer()
            .on_click(move |_, window, cx| on_reveal(window, cx))
            .child(format!("↑ {waiting} new"))
    });
    let (muted, surface, border) = (tokens.text_muted, tokens.surface, tokens.border);
    let body = if rows.is_empty() {
        div()
            .p_3()
            .text_sm()
            .text_color(tokens.text_muted)
            .child("No highlights yet. Messages that mention you, match a phrase or come from a highlighted user show up here.")
            .into_any_element()
    } else {
        let count = rows.len();
        uniform_list("mentions-list", count, move |range, _, _| {
            range
                .filter_map(|index| rows.get(index).map(|row| (index, row.clone())))
                .map(|(index, row)| {
                    let color = palette.rule_color(row.color);
                    let (channel, id) = (row.channel.clone(), row.id.clone());
                    let on_pick = on_pick.clone();
                    h_flex()
                        .id(SharedString::from(format!("mention-{index}")))
                        .w_full()
                        .h(px(ROW_HEIGHT))
                        .overflow_hidden()
                        .cursor_pointer()
                        .hover(|style| style.bg(surface))
                        .border_b_1()
                        .border_color(border.opacity(0.5))
                        .on_click(move |_, window, cx| on_pick(&channel, &id, window, cx))
                        .child(
                            div()
                                .flex_none()
                                .w(px(3.0))
                                .self_stretch()
                                .bg(color.opacity(0.9)),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .px_3()
                                .py_2()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .items_center()
                                        .text_xs()
                                        .text_color(muted)
                                        .children(row.unseen.then(|| {
                                            div().flex_none().size(px(6.0)).rounded_full().bg(color)
                                        }))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .font_weight(if row.unseen {
                                                    FontWeight::SEMIBOLD
                                                } else {
                                                    FontWeight::NORMAL
                                                })
                                                .child(format!(
                                                    "#{} · {}",
                                                    row.channel, row.author
                                                )),
                                        )
                                        .child(div().flex_none().child(row.time)),
                                )
                                .child(div().text_sm().line_clamp(2).child(row.text)),
                        )
                })
                .collect()
        })
        .track_scroll(scroll)
        .size_full()
        .into_any_element()
    };
    v_flex()
        .id("mentions-panel")
        .flex_none()
        .w(px(WIDTH))
        .h_full()
        .min_h_0()
        .occlude()
        .bg(tokens.main_bg)
        .border_l_1()
        .border_color(tokens.border)
        .on_hover(move |hovered, window, cx| on_hover(*hovered, window, cx))
        .child(header)
        .children(waiting_bar)
        .child(div().flex_1().min_h_0().child(body))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_chat_core::{Author, Fragment, Message, Timestamp};

    use super::*;

    fn entry(channel: &str, id: &str, at: i64, unseen: bool) -> MentionEntry {
        MentionEntry {
            channel: ChannelId::new(channel),
            message: Arc::new(Message::chat(
                id,
                Timestamp(at),
                Author::new("alice", "Alice"),
                vec![Fragment::Text(format!("text {id}"))],
            )),
            text: format!("text {id}").into(),
            color: HighlightColor::Red,
            unseen,
        }
    }

    #[test]
    fn a_row_names_the_channel_the_author_and_the_time() {
        let entries = [entry("forsen", "1", 1_700_000_000_000, true)];
        let rows = rows(entries.iter());
        assert_eq!(
            rows,
            vec![Row {
                channel: ChannelId::new("forsen"),
                id: MessageId::from("1"),
                author: "Alice".into(),
                time: "22:13".into(),
                text: "text 1".into(),
                color: HighlightColor::Red,
                unseen: true,
            }]
        );
    }

    fn row(id: &str) -> Row {
        Row {
            channel: ChannelId::new("a"),
            id: MessageId::from(id),
            author: "Alice".into(),
            time: "00:00".into(),
            text: id.to_owned().into(),
            color: HighlightColor::Accent,
            unseen: true,
        }
    }

    fn ids(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.id.as_str().to_owned()).collect()
    }

    #[test]
    fn the_rows_stay_as_they_were_while_the_pointer_is_on_the_panel() {
        let mut feed = Feed::default();
        // Not held: follows the log, rebuilding only when it changed.
        let (rows, waiting) = feed.update(false, 1, 2, || vec![row("2"), row("1")]);
        assert_eq!(
            (ids(&rows), waiting),
            (vec!["2".to_owned(), "1".to_owned()], 0)
        );
        let (_, _) = feed.update(false, 1, 2, || panic!("nothing changed"));
        // The pointer comes in: what is under it is pinned, and new hits only count.
        let (rows, waiting) = feed.update(true, 1, 2, || panic!("pinned as it is"));
        assert_eq!(
            (ids(&rows), waiting),
            (vec!["2".to_owned(), "1".to_owned()], 0)
        );
        let (rows, waiting) = feed.update(true, 2, 3, || panic!("held"));
        assert_eq!(
            (ids(&rows), waiting),
            (vec!["2".to_owned(), "1".to_owned()], 1)
        );
        let (_, waiting) = feed.update(true, 4, 5, || panic!("held"));
        assert_eq!(
            waiting, 3,
            "counted since the hold began, not since the last frame"
        );
        // The pointer leaves: the list catches up at once.
        let (rows, waiting) = feed.update(false, 4, 5, || vec![row("5"), row("4")]);
        assert_eq!(
            (ids(&rows), waiting),
            (vec!["5".to_owned(), "4".to_owned()], 0)
        );
    }

    #[test]
    fn showing_the_new_ones_rebuilds_the_list_and_holds_that_one() {
        let mut feed = Feed::default();
        feed.update(false, 1, 1, || vec![row("1")]);
        feed.update(true, 1, 1, || panic!());
        let (_, waiting) = feed.update(true, 3, 3, || panic!());
        assert_eq!(waiting, 2);
        feed.reveal();
        let (rows, waiting) = feed.update(true, 3, 3, || vec![row("3"), row("2"), row("1")]);
        assert_eq!((rows.len(), waiting), (3, 0));
        // And it is held again from there.
        let (rows, waiting) = feed.update(true, 4, 4, || panic!());
        assert_eq!((rows.len(), waiting), (3, 1));
    }

    #[test]
    fn a_panel_first_opened_under_the_pointer_still_shows_the_log() {
        let mut feed = Feed::default();
        let (rows, waiting) = feed.update(true, 5, 5, || vec![row("5")]);
        assert_eq!((rows.len(), waiting), (1, 0));
    }

    #[test]
    fn only_the_newest_rows_are_drawn() {
        let entries: Vec<_> = (0..SHOWN + 50)
            .map(|n| entry("a", &n.to_string(), 0, false))
            .collect();
        assert_eq!(rows(entries.iter()).len(), SHOWN);
    }
}
