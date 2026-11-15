//! One row of chat with its reply action (PR 3.4): the [`MessageElement`], a Reply button that
//! shows while the pointer is over the row, and a right-click menu (Reply, View thread).
//!
//! The message element itself paints everything (and stays a single custom element); the actions
//! live in a thin wrapper around it so the layout cache and the measuring are the same for rows
//! that can be replied to and rows that cannot (notices, the offline sources).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::Message;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::{
    AnyElement, App, Entity, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Styled as _, WeakEntity, div, px,
};

use super::element::{MessageElement, MessageStyle};
use super::layout::LayoutCache;
use super::view::ChatView;
use crate::channels::ChannelStore;
use crate::reply::can_reply_to;
use crate::theme::Tokens;

/// The group the Reply button shows with: hovering anywhere over its row.
const ROW_GROUP: &str = "chat-row";

/// What a row needs besides its message.
pub struct RowContext {
    /// Element-id prefix, so the list's rows and the thread popover's (same message, same
    /// sequence number) have ids of their own.
    pub id: &'static str,
    pub view: WeakEntity<ChatView>,
    pub store: Entity<ChannelStore>,
    pub tokens: Tokens,
    /// Whether this chat has a composer to reply with.
    pub can_reply: bool,
}

/// The row for `message`: the plain element, or with the reply actions when it can be replied to.
pub fn message_row(
    seq: u64,
    message: &Arc<Message>,
    style: Rc<MessageStyle>,
    cache: Rc<RefCell<LayoutCache>>,
    row: &RowContext,
) -> AnyElement {
    let element = MessageElement::new(seq, message.clone(), style, cache);
    if !row.can_reply || !can_reply_to(message) {
        return element.into_any_element();
    }
    let reply_button = {
        let (view, message) = (row.view.clone(), message.clone());
        Button::new((row.id, seq))
            .ghost()
            .xsmall()
            .icon(Icon::new(gpui_kit::assets::IconName::Undo2))
            .tooltip("Reply")
            .accessibility_label("Reply")
            .on_click(move |_, _, cx| request_reply(&view, &message, cx))
    };
    let tokens = row.tokens;
    let menu = {
        let (view, store, message) = (row.view.clone(), row.store.clone(), message.clone());
        move |menu: PopupMenu, _: &mut gpui_kit::Window, cx: &mut gpui_kit::Context<PopupMenu>| {
            let in_thread = in_thread(store.read(cx), &message);
            let (reply_view, reply_message) = (view.clone(), message.clone());
            let (thread_view, root) = (view.clone(), message.thread_root().clone());
            menu.item(
                PopupMenuItem::new("Reply")
                    .on_click(move |_, _, cx| request_reply(&reply_view, &reply_message, cx)),
            )
            .item(
                PopupMenuItem::new("View thread")
                    .disabled(!in_thread)
                    .on_click(move |_, window, cx| {
                        thread_view
                            .update(cx, |view, cx| view.open_thread(root.clone(), window, cx))
                            .ok();
                    }),
            )
        }
    };
    div()
        .id((row.id, seq))
        .group(ROW_GROUP)
        .relative()
        .w_full()
        .child(element)
        .child(
            div()
                .absolute()
                .top(px(-2.0))
                .right(px(10.0))
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .bg(tokens.window_bg)
                .opacity(0.0)
                .group_hover(ROW_GROUP, |style| style.opacity(1.0))
                .child(reply_button),
        )
        .context_menu(menu)
        .into_any_element()
}

fn request_reply(view: &WeakEntity<ChatView>, message: &Arc<Message>, cx: &mut App) {
    let message = message.clone();
    view.update(cx, |view, cx| view.request_reply(message, cx))
        .ok();
}

/// Whether `message` has a thread to look at: it is a reply, or something replied to it.
fn in_thread(store: &ChannelStore, message: &Message) -> bool {
    message.reply.is_some()
        || !store
            .messages()
            .thread(message.id.as_str())
            .replies
            .is_empty()
}
