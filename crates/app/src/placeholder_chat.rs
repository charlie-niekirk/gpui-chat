//! A deliberately simple stand-in for the chat view: the newest messages of one channel as plain
//! text rows in a `uniform_list`. PR 1.4 replaces this with the real `MessageElement` view; it
//! exists so the firehose and the HUD have something to drive.

use gpui_chat_core::{Message, MessageKind};
use gpui_kit::{
    Context, Entity, IntoElement, ParentElement, Render, ScrollStrategy, Styled, Subscription,
    UniformListScrollHandle, Window, div, px, uniform_list,
};

use crate::channels::ChannelStore;
use crate::theme::Tokens;

/// Rows rendered at most: the tail of the store.
pub const MAX_ROWS: usize = 200;

const ROW_HEIGHT: f32 = 18.0;

pub struct PlaceholderChat {
    store: Entity<ChannelStore>,
    scroll: UniformListScrollHandle,
    _observe: Subscription,
}

impl PlaceholderChat {
    pub fn new(store: Entity<ChannelStore>, cx: &mut Context<Self>) -> Self {
        // One notify per frame batch from the store becomes one re-render here.
        let observe = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            scroll: UniformListScrollHandle::new(),
            _observe: observe,
        }
    }
}

/// `HH:MM:SS` (UTC) of a Unix millisecond timestamp.
pub fn clock(unix_millis: i64) -> String {
    let secs = unix_millis.div_euclid(1000).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// One plain-text row: `HH:MM:SS name: text`, `* name text` for `/me`.
pub fn row_text(message: &Message) -> String {
    let name = message
        .author
        .as_ref()
        .map_or("", |author| author.display_name.as_str());
    let text = message.plain_text();
    let time = clock(message.timestamp.unix_millis());
    match message.kind {
        MessageKind::Action => format!("{time} * {name} {text}"),
        _ if name.is_empty() => format!("{time} {text}"),
        _ => format!("{time} {name}: {text}"),
    }
}

impl Render for PlaceholderChat {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        let store = self.store.read(cx);
        let total = store.messages().len();
        let rows = total.min(MAX_ROWS);
        let skipped = total - rows;
        // The rows are the newest `rows` messages; keep the view pinned to the last one.
        self.scroll
            .scroll_to_item_strict(rows.saturating_sub(1), ScrollStrategy::Bottom);
        let channel = store.channel().clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(tokens.text_muted)
                    .child(format!("#{channel} · {total} messages (placeholder list)")),
            )
            .child(
                uniform_list(
                    "placeholder-chat",
                    rows,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        let store = this.store.read(cx);
                        range
                            .filter_map(|i| store.messages().get_index(skipped + i))
                            .map(|message| {
                                div()
                                    .h(px(ROW_HEIGHT))
                                    .px_2()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_sm()
                                    .child(row_text(message))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_chat_core::{Author, Fragment, Timestamp};

    #[test]
    fn clock_formats_utc_time_of_day() {
        assert_eq!(clock(0), "00:00:00");
        assert_eq!(clock(1_700_000_000_000), "22:13:20");
        assert_eq!(clock(-1000), "23:59:59");
    }

    #[test]
    fn rows_read_like_chat() {
        let mut message = Message::chat(
            "1",
            Timestamp(3_661_000),
            Author::new("alice", "Alice"),
            vec![Fragment::Text("hi there".into())],
        );
        assert_eq!(row_text(&message), "01:01:01 Alice: hi there");
        message.kind = MessageKind::Action;
        assert_eq!(row_text(&message), "01:01:01 * Alice hi there");
        let system = Message::plain("2", Timestamp(0), MessageKind::System, "connected");
        assert_eq!(row_text(&system), "00:00:00 connected");
    }
}
