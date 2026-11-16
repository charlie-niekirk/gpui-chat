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

/// Shows a row's message in its channel.
pub type Pick = Rc<dyn Fn(&ChannelId, &MessageId, &mut Window, &mut App)>;

/// The panel. `on_close` hides it.
pub fn panel(
    rows: Rc<Vec<Row>>,
    scroll: &UniformListScrollHandle,
    tokens: &Tokens,
    on_pick: Pick,
    on_close: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
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
                            format!("{unseen} new of {}", rows.len())
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
        .child(header)
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

    #[test]
    fn only_the_newest_rows_are_drawn() {
        let entries: Vec<_> = (0..SHOWN + 50)
            .map(|n| entry("a", &n.to_string(), 0, false))
            .collect();
        assert_eq!(rows(entries.iter()).len(), SHOWN);
    }
}
