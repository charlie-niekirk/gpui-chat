//! One row of chat with its reply action (PR 3.4): the [`MessageElement`], a Reply button on the
//! row the pointer is over, and one right-click menu per chat (or popover) that resolves the row
//! under the pointer.
//!
//! The message element itself paints everything (and stays a single custom element). Rows
//! report the pointer through [`MessageStyle::hovered_row`]; only the hovered row is wrapped and
//! carries a button, so the other rows cost nothing extra, and the menu is a single element
//! around all of them ([`row_menu`] reads the hovered row when it opens).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::Message;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::{
    AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement as _, MouseButton,
    ParentElement as _, Styled as _, WeakEntity, Window, div, px,
};

use super::element::{HoveredRow, MessageElement, MessageStyle};
use super::layout::LayoutCache;
use super::view::ChatView;
use crate::channels::ChannelStore;
use crate::reply::can_reply_to;
use crate::theme::Tokens;

/// What a row needs besides its message.
pub struct RowContext {
    /// Element-id prefix, so the list's buttons and the thread popover's (same message, same
    /// sequence number) have ids of their own.
    pub id: &'static str,
    pub view: WeakEntity<ChatView>,
    pub tokens: Tokens,
    /// The row the pointer is over, if rows can be replied to (there is a composer): only that
    /// one gets a button.
    pub hovered: Option<u64>,
}

/// The row for `message`: the plain element, or with the Reply button when the pointer is over it
/// and it can be replied to.
pub fn message_row(
    seq: u64,
    message: &Arc<Message>,
    style: Rc<MessageStyle>,
    cache: Rc<RefCell<LayoutCache>>,
    row: &RowContext,
) -> AnyElement {
    let element = MessageElement::new(seq, message.clone(), style, cache);
    // Recent-messages history (PR 5.5) is shown dimmer than live chat.
    let element = if message.flags.history {
        div()
            .w_full()
            .opacity(row.tokens.history_opacity)
            .child(element)
            .into_any_element()
    } else {
        element.into_any_element()
    };
    if row.hovered != Some(seq) || !can_reply_to(message) {
        return element;
    }
    let reply_button = {
        let (view, message) = (row.view.clone(), message.clone());
        Button::new((row.id, seq))
            .ghost()
            .xsmall()
            .icon(Icon::new(gpui_kit::assets::IconName::Undo2))
            .tooltip("Reply")
            .accessibility_label("Reply")
            .tab_stop(false)
            .on_click(move |_, _, cx| request_reply(&view, &message, cx))
    };
    let tokens = row.tokens;
    div()
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
                .bg(tokens.surface)
                // The context line of a reply row sits under the button: a press on the button
                // must not reach it (and open the thread).
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(reply_button),
        )
        .into_any_element()
}

/// The right-click menu for the row under the pointer when it opens (Reply, View thread), or an
/// empty menu (which Kit does not show) when the pointer is over no row that can be replied to.
/// `hovered` is the cell the rows keep current; `store` is where its message is looked up.
pub fn row_menu(
    view: WeakEntity<ChatView>,
    store: Entity<ChannelStore>,
    hovered: HoveredRow,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, _, cx| {
        let Some(seq) = hovered.get() else {
            return menu;
        };
        let store = store.read(cx);
        let Some(message) = store
            .messages()
            .get_by_seq(seq)
            .filter(|message| can_reply_to(message))
            .cloned()
        else {
            return menu;
        };
        let in_thread = in_thread(store, &message);
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
