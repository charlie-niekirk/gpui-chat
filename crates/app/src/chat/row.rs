//! One row of chat with its reply action (PR 3.4): the [`MessageElement`], a Reply button on the
//! row the pointer is over, and one right-click menu per chat (or popover) that resolves the row
//! under the pointer. For a moderator (PR 5.2) the same hover strip also holds Delete, Timeout
//! 10m, Ban (and Unban on a removed message) and the menu adds Delete / Timeout… / Ban / Unban;
//! a removed message is a [`collapsed_row`] when `chat.deleted_messages` says so.
//!
//! The message element itself paints everything (and stays a single custom element). Rows
//! report the pointer through [`MessageStyle::hovered_row`]; only the hovered row is wrapped and
//! carries a button, so the other rows cost nothing extra, and the menu is a single element
//! around all of them ([`row_menu`] asks the selection surface what the right-click was over:
//! the message, and a link under the pointer, PR 5.6).

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::{
    ChannelId, ChannelRole, DEFAULT_TIMEOUT_SECS, Message, MessageId, RowActions, TIMEOUT_PRESETS,
    row_actions,
};
use gpui_chat_settings::DeletedMessages;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement as _,
    MouseButton, ParentElement as _, StatefulInteractiveElement as _, Styled as _, WeakEntity,
    Window, div, px,
};

use super::display;
use super::element::{MessageElement, MessageStyle};
use super::layout::LayoutCache;
use super::selection::{self, one_line};
use super::selection_layer::Surface;
use super::view::ChatView;
use crate::channels::ChannelStore;
use crate::moderation::{RowAction, is_collapsed};
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
    /// What the logged-in user is here, when the pane can run moderation commands (else a viewer):
    /// moderators get the moderation controls.
    pub role: ChannelRole,
    pub channel: ChannelId,
    /// How removed messages are drawn (`chat.deleted_messages`).
    pub deleted: DeletedMessages,
    /// Removed messages the user clicked open.
    pub revealed: Rc<HashSet<MessageId>>,
}

/// The row for `message`: the plain element, or with the hover buttons (Reply, and for a
/// moderator Delete / Timeout / Ban / Unban) when the pointer is over it; a removed message is
/// one line "<message deleted>" while `chat.deleted_messages` collapses it.
pub fn message_row(
    seq: u64,
    message: &Arc<Message>,
    style: Rc<MessageStyle>,
    cache: Rc<RefCell<LayoutCache>>,
    row: &RowContext,
) -> AnyElement {
    if is_collapsed(message, row.deleted, row.revealed.contains(&message.id)) {
        return collapsed_row(seq, message, &style, row);
    }
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
    if row.hovered != Some(seq) {
        return element;
    }
    let reply = row.can_reply && can_reply_to(message);
    let actions = row_actions(row.role, &row.channel, message);
    if !reply && !actions.any() {
        return element;
    }
    let tokens = row.tokens;
    let mut buttons = h_flex().gap_0p5().items_center();
    if reply {
        let (view, message) = (row.view.clone(), message.clone());
        buttons = buttons.child(
            Button::new((row.id, seq))
                .ghost()
                .xsmall()
                .icon(Icon::new(gpui_kit::assets::IconName::Undo2))
                .tooltip("Reply")
                .accessibility_label("Reply")
                .tab_stop(false)
                .on_click(move |_, _, cx| request_reply(&view, &message, cx)),
        );
    }
    // Moderation (PR 5.2): a compact strip beside Reply, only for moderators.
    for (id, action, icon, label) in mod_buttons(actions, message) {
        let (view, message) = (row.view.clone(), message.clone());
        let button = Button::new((id, seq)).ghost().xsmall();
        let button = match icon {
            Some(icon) => button.icon(Icon::new(icon)),
            None => button.label(label.clone()),
        };
        buttons = buttons.child(
            button
                .tooltip(action.label())
                .accessibility_label(action.label())
                .tab_stop(false)
                .on_click(move |_, _, cx| request_moderation(&view, &message, action, cx)),
        );
    }
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
                // The context line of a reply row sits under the buttons: a press on them must
                // not reach it (and open the thread).
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(buttons),
        )
        .into_any_element()
}

/// The moderation buttons of the hover strip: `(element id prefix, action, icon, label)`. Delete,
/// a ten-minute timeout and Ban on a live message; Ban and Unban once it was removed. The icon is
/// `None` for the ones that are text.
fn mod_buttons(
    actions: RowActions,
    message: &Message,
) -> Vec<(
    &'static str,
    RowAction,
    Option<gpui_kit::assets::IconName>,
    String,
)> {
    let mut buttons = Vec::new();
    if actions.delete {
        buttons.push((
            "mod-delete",
            RowAction::Delete,
            Some(gpui_kit::assets::IconName::Delete),
            String::new(),
        ));
    }
    if actions.timeout && !message.flags.deleted {
        let action = RowAction::Timeout(DEFAULT_TIMEOUT_SECS);
        buttons.push(("mod-timeout", action, None, "10m".to_owned()));
    }
    if actions.ban {
        buttons.push((
            "mod-ban",
            RowAction::Ban,
            Some(gpui_kit::assets::IconName::Ban),
            String::new(),
        ));
    }
    if actions.unban && message.flags.deleted {
        buttons.push(("mod-unban", RowAction::Unban, None, "Unban".to_owned()));
    }
    buttons
}

/// A removed message under `chat.deleted_messages = "collapse"`: one muted line, `name:
/// <message deleted>`, that shows the message when clicked.
fn collapsed_row(
    seq: u64,
    message: &Arc<Message>,
    style: &MessageStyle,
    row: &RowContext,
) -> AnyElement {
    let clock = style
        .options
        .timestamps
        .then(|| format!("{} ", display::clock(message.timestamp.unix_millis())));
    let name = message
        .author
        .as_ref()
        .map(|author| format!("{}: ", selection::username(author)))
        .unwrap_or_default();
    let label = format!("{}{name}<message deleted>", clock.unwrap_or_default());
    let (view, id) = (row.view.clone(), message.id.clone());
    let (muted, text) = (style.palette.muted, style.palette.text);
    div()
        .id((row.id, seq))
        .w_full()
        .px(style.padding.x)
        .py(style.padding.y)
        .font_family(style.font.family.clone())
        .text_size(style.font_size)
        .line_height(style.line_height)
        .text_color(muted)
        .italic()
        .truncate()
        .cursor_pointer()
        .hover(move |row| row.text_color(text))
        .tooltip(|window, cx| Tooltip::new("Click to show the message").build(window, cx))
        .on_click(move |_, _, cx| {
            view.update(cx, |view, cx| view.reveal(&id, cx)).ok();
        })
        .child(label)
        .into_any_element()
}

/// The right-click menu for the row the right-click was over (resolved by `surface` at the click):
/// Copy (when there is a selection), Copy message, Copy username, Open / Copy link (over a link),
/// then Reply and View thread when `can_reply`, then (for a moderator, PR 5.2) Delete message,
/// Timeout… (a submenu of lengths), Ban and Unban. Empty (which Kit does not show) over no row.
/// `store` is where the message is looked up. *Copy message* is the message's text alone; ⌘C on a
/// selected whole message copies the chat line (`HH:MM name: text`).
pub fn row_menu(
    view: WeakEntity<ChatView>,
    store: Entity<ChannelStore>,
    surface: Rc<Surface>,
    can_reply: bool,
    moderation: bool,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, window, cx| {
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
        menu = menu.item(copy_item("Copy message", one_line(&message.plain_text())));
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
        if can_reply && can_reply_to(&message) {
            let in_thread = in_thread(store, &message);
            let (reply_view, reply_message) = (view.clone(), message.clone());
            let (thread_view, root) = (view.clone(), message.thread_root().clone());
            menu = menu
                .separator()
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
                );
        }
        if !moderation {
            return menu;
        }
        let actions = row_actions(store.role(), store.channel(), &message);
        if !actions.any() {
            return menu;
        }
        let choose = |view: &WeakEntity<ChatView>, message: &Arc<Message>, action| {
            let (view, message) = (view.clone(), message.clone());
            move |_: &_, _: &mut Window, cx: &mut App| {
                request_moderation(&view, &message, action, cx);
            }
        };
        menu = menu.separator();
        if actions.delete {
            menu = menu.item(
                PopupMenuItem::new(RowAction::Delete.label()).on_click(choose(
                    &view,
                    &message,
                    RowAction::Delete,
                )),
            );
        }
        if actions.timeout {
            let (view, message) = (view.clone(), message.clone());
            menu = menu.submenu("Timeout…", window, cx, move |mut submenu, _, _| {
                for seconds in TIMEOUT_PRESETS {
                    let action = RowAction::Timeout(seconds);
                    submenu = submenu.item(
                        PopupMenuItem::new(gpui_chat_core::format_seconds(seconds))
                            .on_click(choose(&view, &message, action)),
                    );
                }
                submenu
            });
        }
        if actions.ban {
            menu = menu.item(PopupMenuItem::new("Ban…").on_click(choose(
                &view,
                &message,
                RowAction::Ban,
            )));
        }
        if actions.unban {
            menu = menu.item(PopupMenuItem::new("Unban").on_click(choose(
                &view,
                &message,
                RowAction::Unban,
            )));
        }
        menu
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

fn request_moderation(
    view: &WeakEntity<ChatView>,
    message: &Arc<Message>,
    action: RowAction,
    cx: &mut App,
) {
    let message = message.clone();
    view.update(cx, |view, cx| view.request_moderation(message, action, cx))
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
