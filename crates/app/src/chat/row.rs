//! One row of chat with its reply action (PR 3.4): the [`MessageElement`], a Reply button on the
//! row the pointer is over, and one right-click menu per chat (or popover) that resolves the row
//! under the pointer.
//!
//! The message element itself paints everything (and stays a single custom element). Rows
//! report the pointer through [`MessageStyle::hovered_row`]; only the hovered row is wrapped and
//! carries a button, so the other rows cost nothing extra, and the menu is a single element
//! around all of them ([`row_menu`] asks the selection surface what the right-click was over:
//! the message, and a link under the pointer, PR 5.6).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::Message;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement as _,
    MouseButton, ParentElement as _, Styled as _, WeakEntity, Window, div, px,
};

use super::element::{MessageElement, MessageStyle};
use super::events;
use super::layout::LayoutCache;
use super::selection::{self};
use super::selection_layer::Surface;
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
    /// Rows can be replied to (there is a composer).
    pub can_reply: bool,
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

/// The right-click menu for the row the right-click was over (resolved by `surface` at the click):
/// Copy (when there is a selection), Copy message, Copy username, Open / Copy link (over a link),
/// then Reply and View thread when `can_reply`. Empty (which Kit does not show) over no row.
/// `store` is where the message is looked up. *Copy message* is the message's text alone; ⌘C on a
/// selected whole message copies the chat line (`HH:MM name: text`).
pub fn row_menu(
    view: WeakEntity<ChatView>,
    store: Entity<ChannelStore>,
    surface: Rc<Surface>,
    can_reply: bool,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, _, cx| {
        let Some(target) = surface.take_menu_target() else {
            return menu;
        };
        let store = store.read(cx);
        let Some(message) = store.messages().get_by_seq(target.seq).cloned() else {
            return menu;
        };
        // The text is taken now: a press on the menu clears the selection before the item runs.
        let mut menu = match surface.selected_text(store.messages()) {
            Some(text) => menu.item(copy_item("Copy", text)).separator(),
            None => menu,
        };
        menu = menu.item(copy_item("Copy message", events::copy_text(&message)));
        if let Some(author) = &message.author {
            menu = menu.item(copy_item(
                "Copy username",
                selection::username(author).to_owned(),
            ));
        }
        if let Some(url) = target.link {
            let open = url.clone();
            menu = menu
                .separator()
                .item(
                    PopupMenuItem::new("Open link")
                        .disabled(!selection::openable(&url))
                        .on_click(move |_, _, cx| cx.open_url(&open)),
                )
                .item(copy_item("Copy link", url.to_string()));
        }
        if !can_reply || !can_reply_to(&message) {
            return menu;
        }
        let in_thread = in_thread(store, &message);
        let (reply_view, reply_message) = (view.clone(), message.clone());
        let (thread_view, root) = (view.clone(), message.thread_root().clone());
        menu.separator()
            .item(
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

/// A menu item that puts `text` on the clipboard.
fn copy_item(label: &'static str, text: String) -> PopupMenuItem {
    PopupMenuItem::new(label)
        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
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
