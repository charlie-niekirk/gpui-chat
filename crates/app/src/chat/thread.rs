//! The thread popover (PR 3.4): the message a thread hangs off and the replies to it that the
//! store still holds, in a panel over the top right of the chat.
//!
//! It reads the thread from the [`gpui_chat_core::MessageStore`] each time it renders, so a reply
//! that arrives while it is open shows up, and a message that was evicted or deleted changes
//! with the store. Its rows are ordinary [`super::element::MessageElement`]s (emotes, badges,
//! timestamps), with a layout cache and hover state of their own: the same message is shaped at
//! another width than in the list. A thread whose root is no longer in the store is introduced by
//! what the replies' own tags say about it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::{Message, MessageId, MessageStore};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, Entity, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::element::HoveredRow;
use super::element::MessageStyle;
use super::layout::LayoutCache;
use super::row::{RowContext, message_row, row_menu};
use super::selection_layer::Surface;
use super::tooltip::EmoteHover;
use super::view::ChatView;
use crate::channels::ChannelStore;
use crate::reply::single_line;

/// Replies shown at most: the newest ones, so a very long thread stays cheap to draw (the rows are
/// not virtualised).
pub const MAX_REPLIES: usize = 200;

/// The open thread popover of a [`ChatView`].
pub struct ThreadPanel {
    /// The id of the message the thread hangs off.
    pub root: MessageId,
    pub cache: Rc<RefCell<LayoutCache>>,
    pub hover: Rc<RefCell<EmoteHover>>,
    /// The popover's row the pointer is over.
    pub hovered: HoveredRow,
    /// Text selection and links in the popover (it does not scroll the list).
    pub surface: Rc<Surface>,
}

impl ThreadPanel {
    pub fn new(root: MessageId) -> Self {
        let cache: Rc<RefCell<LayoutCache>> = Rc::default();
        Self {
            root,
            surface: Surface::new(None, cache.clone()),
            cache,
            hover: Rc::default(),
            hovered: Rc::default(),
        }
    }
}

impl ThreadView {
    /// The sequence numbers of the rows it shows, in order.
    pub fn seqs(&self) -> Vec<u64> {
        self.root
            .iter()
            .chain(&self.replies)
            .map(|(seq, _)| *seq)
            .collect()
    }
}

/// What the popover shows for a thread.
#[derive(Debug)]
pub struct ThreadView {
    /// The root message and its sequence number, if the store still has it.
    pub root: Option<(u64, Arc<Message>)>,
    /// Who and what it said, from the replies' tags, when the root is gone.
    pub root_stub: Option<String>,
    /// The newest replies (at most [`MAX_REPLIES`]), oldest first, with their sequence numbers.
    pub replies: Vec<(u64, Arc<Message>)>,
    /// Replies held but not shown.
    pub hidden: usize,
}

/// The thread rooted at `root` as the store holds it.
pub fn thread_view(store: &MessageStore, root: &str) -> ThreadView {
    let thread = store.thread(root);
    let hidden = thread.replies.len().saturating_sub(MAX_REPLIES);
    let root_stub = thread.root.is_none().then(|| stub(&thread.replies, root));
    let replies = thread
        .replies
        .iter()
        .skip(hidden)
        .filter_map(|message| Some((store.seq_of(message.id.as_str())?, Arc::clone(message))))
        .collect();
    ThreadView {
        root: thread
            .root
            .and_then(|root| Some((store.seq_of(root.id.as_str())?, Arc::clone(root)))),
        root_stub: root_stub.flatten(),
        replies,
        hidden,
    }
}

/// `Name: what they said` for a root that has left the store, if a reply to it names it: its
/// author and (when the reply was straight to it) its text.
fn stub(replies: &[&Arc<Message>], root: &str) -> Option<String> {
    let direct = replies
        .iter()
        .filter_map(|message| message.reply.as_ref())
        .find(|reply| reply.parent_id.as_str() == root)?;
    let body = single_line(&direct.parent_body);
    Some(if body.is_empty() {
        direct.parent_name().to_owned()
    } else {
        format!("{}: {body}", direct.parent_name())
    })
}

/// The panel. `style` is the list's, adapted by the caller; `rows` says how its rows are built and
/// `store` is where the right-click menu looks the row under the pointer up.
pub fn render(
    panel: &ThreadPanel,
    thread: &ThreadView,
    style: &Rc<MessageStyle>,
    rows: &RowContext,
    store: Entity<ChannelStore>,
    cx: &mut Context<ChatView>,
) -> AnyElement {
    let tokens = rows.tokens;
    let row = |seq: u64, message: &Arc<Message>| {
        message_row(seq, message, style.clone(), panel.cache.clone(), rows)
    };

    let mut body = v_flex().w_full().py_1();
    match (&thread.root, &thread.root_stub) {
        (Some((seq, root)), _) => body = body.child(row(*seq, root)),
        (None, stub) => {
            body = body.child(
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(tokens.text_muted)
                    .child(match stub {
                        Some(stub) => format!("{stub}\n(no longer in chat)"),
                        None => "The original message is no longer in chat.".to_owned(),
                    }),
            );
        }
    }
    body = body.child(
        div()
            .mx_2()
            .my_1()
            .border_t_1()
            .border_color(tokens.border)
            .pt_1()
            .text_xs()
            .text_color(tokens.text_muted)
            .child(match (thread.replies.len(), thread.hidden) {
                (0, _) => "No replies yet".to_owned(),
                (1, 0) => "1 reply".to_owned(),
                (n, 0) => format!("{n} replies"),
                (n, hidden) => format!("{} replies, showing the latest {n}", n + hidden),
            }),
    );
    for (seq, message) in &thread.replies {
        body = body.child(row(*seq, message));
    }

    let header = h_flex()
        .flex_none()
        .items_center()
        .justify_between()
        .px_3()
        .py_1()
        .border_b_1()
        .border_color(tokens.border)
        .child(
            div()
                .text_sm()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child("Thread"),
        )
        .child(
            Button::new("thread-close")
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Close))
                .tooltip("Close")
                .accessibility_label("Close thread")
                .on_click(cx.listener(|this, _, _, cx| this.close_thread(cx))),
        );

    // Fills the space the chat leaves (inset from its edges) and pushes the panel to the right;
    // the panel is as tall as its content up to that space, then scrolls.
    div()
        .absolute()
        .inset_2()
        .flex()
        .justify_end()
        .items_start()
        // Pointer events on the empty part of the overlay belong to the chat underneath.
        .child(
            v_flex()
                .id("thread-panel")
                // Everything behind the panel is hidden from the pointer: wheel events, clicks and
                // hover belong to it, not to the chat rows underneath (the empty part of the
                // overlay stays pass-through).
                .occlude()
                .w(px(400.0))
                .max_w_full()
                .max_h_full()
                .bg(tokens.main_bg)
                .border_1()
                .border_color(tokens.border)
                .rounded_lg()
                .shadow_lg()
                .overflow_hidden()
                // First, so the rows and buttons drawn after it see a press before it does.
                .child(panel.surface.layer())
                .child(header)
                .child(
                    div()
                        .id("thread-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(body),
                )
                .context_menu(row_menu(
                    rows.view.clone(),
                    store,
                    panel.surface.clone(),
                    rows.can_reply,
                    rows.role.can_moderate(),
                )),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{Author, Fragment, ReplyRef, Timestamp};

    use super::*;

    fn chat(id: &str, login: &str) -> Arc<Message> {
        Arc::new(Message::chat(
            id,
            Timestamp(0),
            Author::new(login, login),
            vec![Fragment::Text(format!("text of {id}"))],
        ))
    }

    fn reply(id: &str, parent: &str, root: &str, body: &str) -> Arc<Message> {
        let mut m = Message::chat(
            id,
            Timestamp(0),
            Author::new("replier", "replier"),
            vec![Fragment::Text(id.to_owned())],
        );
        m.reply = Some(ReplyRef {
            parent_id: parent.into(),
            parent_login: "alice".into(),
            parent_display_name: "Alice".into(),
            parent_body: body.into(),
            thread_parent_id: Some(root.into()),
        });
        Arc::new(m)
    }

    #[test]
    fn the_view_holds_the_root_and_the_replies_with_their_sequence_numbers() {
        let mut store = MessageStore::new();
        store.push(chat("root", "alice"));
        store.push(chat("noise", "bob"));
        store.push(reply("r1", "root", "root", "hello"));
        store.push(reply("r2", "r1", "root", "x"));
        let view = thread_view(&store, "root");
        assert_eq!(view.root.unwrap().1.id.as_str(), "root");
        assert_eq!(view.root_stub, None);
        assert_eq!(
            view.replies
                .iter()
                .map(|(seq, m)| (*seq, m.id.as_str()))
                .collect::<Vec<_>>(),
            [(2, "r1"), (3, "r2")]
        );
        assert_eq!(view.hidden, 0);
    }

    #[test]
    fn a_root_that_left_the_store_is_introduced_by_the_reply_tags() {
        let mut store = MessageStore::with_capacity(5);
        store.push(reply("r1", "gone", "gone", "what is\nthe game"));
        store.push(reply("r2", "r1", "gone", "it is chess"));
        let view = thread_view(&store, "gone");
        assert!(view.root.is_none());
        assert_eq!(view.root_stub.as_deref(), Some("Alice: what is the game"));
        assert_eq!(view.replies.len(), 2);
        // Only replies to replies: nothing names the root's author or text.
        let mut only_deep = MessageStore::with_capacity(5);
        only_deep.push(reply("r2", "r1", "gone", "it is chess"));
        assert_eq!(thread_view(&only_deep, "gone").root_stub, None);
    }

    #[test]
    fn a_long_thread_shows_the_newest_replies() {
        let mut store = MessageStore::new();
        store.push(chat("root", "alice"));
        for n in 0..MAX_REPLIES + 5 {
            store.push(reply(&format!("r{n}"), "root", "root", "x"));
        }
        let view = thread_view(&store, "root");
        assert_eq!(view.replies.len(), MAX_REPLIES);
        assert_eq!(view.hidden, 5);
        assert_eq!(
            view.replies[0].1.id.as_str(),
            "r5",
            "the oldest five are cut"
        );
    }
}
