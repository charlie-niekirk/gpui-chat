//! One row of chat with its reply action (PR 3.4): the [`MessageElement`], a Reply button on the
//! row the pointer is over, and one right-click menu per chat (or popover) that resolves the row
//! under the pointer. For a moderator (PR 5.2) the same hover strip also holds Delete, Timeout
//! 10m, Ban (and Unban on a removed message) and the menu adds Delete / Timeout… / Ban / Unban;
//! a removed message is the collapsed line of [`MessageElement::collapsed`] when
//! `chat.deleted_messages` says so (same element, so selecting, copying and the right-click menu work on it).
//! The strip has the same four slots on every row (the ones that do not apply are disabled), and the
//! chat view holds the list while the pointer is on it ([`ChatView::hold_for_strip`]), so a click
//! always lands on the button, and the row, that was aimed at.
//!
//! The message element itself paints everything (and stays a single custom element). Rows
//! report the pointer through [`MessageStyle::hovered_row`]; only the hovered row is wrapped and
//! carries a button, so the other rows cost nothing extra, and the menu is a single element
//! around all of them ([`row_menu`] asks the selection surface what the right-click was over:
//! the message, a link under the pointer, PR 5.6, and the name or mention for *View user*, PR 5.4).

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::{
    ChannelId, ChannelRole, DEFAULT_TIMEOUT_SECS, Ignores, Message, MessageId, RowActions,
    TIMEOUT_PRESETS, row_actions,
};
use gpui_chat_settings::DeletedMessages;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement as _,
    MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    WeakEntity, Window, div, px,
};

use super::element::{MessageElement, MessageStyle};
use super::events;
use super::layout::LayoutCache;
use super::selection::{self};
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
    pub revealed: Rc<RefCell<HashSet<MessageId>>>,
    /// The logged-in user's login: no moderation controls on their own messages.
    pub own_login: Option<Arc<str>>,
    /// What is ignored, when ignored messages are shown as one collapsed line (PR 5.7).
    pub ignored: Option<Arc<Ignores>>,
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
    let revealed = row.revealed.borrow().contains(&message.id);
    // An ignored message (PR 5.7) is one line "1 ignored message" until it is clicked open.
    let ignored = !revealed
        && row
            .ignored
            .as_ref()
            .is_some_and(|ignores| ignores.is_ignored(message));
    let collapsed = !ignored && is_collapsed(message, row.deleted, revealed);
    let element = MessageElement::new(seq, message.clone(), style, cache)
        .collapsed(collapsed)
        .ignored(ignored);
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
    // A collapsed line shows the message when clicked.
    let element = if collapsed || ignored {
        let (view, id) = (row.view.clone(), message.id.clone());
        div()
            .id(row_id(row, "collapsed", seq))
            .w_full()
            .cursor_pointer()
            .tooltip(|window, cx| Tooltip::new("Click to show the message").build(window, cx))
            .on_click(move |_, _, cx| {
                view.update(cx, |view, cx| view.reveal(&id, cx)).ok();
            })
            .child(element)
            .into_any_element()
    } else {
        element
    };
    if row.hovered != Some(seq) {
        return element;
    }
    let reply = row.can_reply && can_reply_to(message);
    let actions = row_actions(row.role, &row.channel, row.own_login.as_deref(), message);
    if !reply && !actions.any() {
        return element;
    }
    let tokens = row.tokens;
    let mut buttons = h_flex().gap_0p5().items_center();
    if reply {
        let (view, message) = (row.view.clone(), message.clone());
        buttons = buttons.child(
            Button::new(row_id(row, "reply", seq))
                .ghost()
                .xsmall()
                .icon(Icon::new(gpui_kit::assets::IconName::Undo2))
                .tooltip("Reply")
                .accessibility_label("Reply")
                .tab_stop(false)
                .on_click(move |_, _, cx| request_reply(&view, &message, cx)),
        );
    }
    // Moderation (PR 5.2): the same four slots on every row, in the same places, the ones that do
    // not apply disabled, so what is aimed at is what a click does.
    if actions.any() {
        for slot in mod_slots(actions) {
            let (view, message) = (row.view.clone(), message.clone());
            let action = slot.action;
            let button = Button::new(row_id(row, slot.name, seq)).ghost().xsmall();
            let button = match slot.icon {
                Some(icon) => button.icon(Icon::new(icon)),
                None => button.label(slot.label),
            };
            buttons = buttons.child(
                button
                    .disabled(!slot.enabled)
                    .tooltip(action.label())
                    .accessibility_label(action.label())
                    .tab_stop(false)
                    .on_click(move |_, _, cx| request_moderation(&view, &message, action, cx)),
            );
        }
    }
    let view = row.view.clone();
    div()
        .relative()
        .w_full()
        .child(element)
        .child(
            div()
                .id(row_id(row, "strip", seq))
                .absolute()
                .top(px(-2.0))
                .right(px(10.0))
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .bg(tokens.surface)
                // While the pointer is on the strip the list stands still: a message arriving
                // cannot slide another row (and its buttons) under the pointer before the click.
                .on_hover(move |on, _, cx| {
                    view.update(cx, |view, cx| view.hold_for_strip(*on, cx))
                        .ok();
                })
                // The context line of a reply row sits under the buttons: a press on them must
                // not reach it (and open the thread).
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(buttons),
        )
        .into_any_element()
}

/// An element id for part of a row: the list's and the thread popover's rows share sequence
/// numbers, so every id carries the row's own prefix.
fn row_id(row: &RowContext, part: &str, seq: u64) -> SharedString {
    SharedString::from(format!("{}-{part}-{seq}", row.id))
}

/// One slot of the moderation strip.
struct ModSlot {
    name: &'static str,
    action: RowAction,
    icon: Option<gpui_kit::assets::IconName>,
    label: String,
    enabled: bool,
}

/// The four slots of the strip, always in this order: Delete, a ten-minute timeout, Ban, Unban.
fn mod_slots(actions: RowActions) -> [ModSlot; 4] {
    [
        ModSlot {
            name: "mod-delete",
            action: RowAction::Delete,
            icon: Some(gpui_kit::assets::IconName::Delete),
            label: String::new(),
            enabled: actions.delete,
        },
        ModSlot {
            name: "mod-timeout",
            action: RowAction::Timeout(DEFAULT_TIMEOUT_SECS),
            icon: None,
            label: "10m".to_owned(),
            enabled: actions.timeout,
        },
        ModSlot {
            name: "mod-ban",
            action: RowAction::Ban,
            icon: Some(gpui_kit::assets::IconName::Ban),
            label: String::new(),
            enabled: actions.ban,
        },
        ModSlot {
            name: "mod-unban",
            action: RowAction::Unban,
            icon: None,
            label: "Unban".to_owned(),
            enabled: actions.unban,
        },
    ]
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
        menu = menu.item(copy_item("Copy message", events::copy_text(&message)));
        if let Some(author) = &message.author {
            menu = menu.item(copy_item(
                "Copy username",
                selection::username(author).to_owned(),
            ));
        }
        // The user card (PR 5.4): of the name or mention under the pointer, else the author.
        let mentioned = target.user.clone();
        let viewed = mentioned
            .clone()
            .or_else(|| message.author.as_ref().map(|a| Arc::from(a.login.as_str())));
        if let Some(login) = viewed.filter(|login| gpui_chat_core::is_login(login)) {
            let label = match (&mentioned, &message.author) {
                (Some(login), Some(author)) if !login.eq_ignore_ascii_case(&author.login) => {
                    format!("View @{login}")
                }
                _ => "View user".to_owned(),
            };
            let (card_view, position) = (view.clone(), target.position);
            // The author's own name: the card is about the account that sent this message.
            let origin = message
                .author
                .as_ref()
                .filter(|author| author.login.eq_ignore_ascii_case(&login))
                .and_then(|author| author.user_id.clone());
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                card_view
                    .update(cx, |view, cx| {
                        view.open_user_card(&login, origin.as_deref(), position, window, cx)
                    })
                    .ok();
            }));
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
        menu = ignore_items(menu, &view, &message, cx);
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
        let own = crate::account::own_login(cx);
        let actions = row_actions(store.role(), store.channel(), own.as_deref(), &message);
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

/// *Show only …*, *Ignore user* / *Unignore user* and the Twitch block list items for the author of
/// `message` (PR 5.7): none for the user's own messages or a line nobody wrote. *Block on Twitch…*
/// needs a login; whether the login may use the block list is found out when it is chosen (a
/// strip says to enable it).
fn ignore_items(
    menu: PopupMenu,
    view: &WeakEntity<ChatView>,
    message: &Message,
    cx: &App,
) -> PopupMenu {
    let Some(author) = message
        .author
        .as_ref()
        .filter(|a| gpui_chat_core::is_login(&a.login))
    else {
        return menu;
    };
    let own = crate::account::own_login(cx);
    if own
        .as_deref()
        .is_some_and(|own| own.eq_ignore_ascii_case(&author.login))
    {
        return menu;
    }
    let Some(hub) = crate::ignores::current(cx) else {
        return menu;
    };
    let (listed, blocked) = {
        let hub = hub.read(cx);
        (hub.is_listed(&author.login), hub.is_blocked(&author.login))
    };
    let login = author.login.clone();
    let user_id = author.user_id.clone();
    let mut menu = menu.separator();
    let show_view = view.clone();
    let shown = login.clone();
    menu = menu.item(
        PopupMenuItem::new(format!("Show only {}", author.display_name)).on_click(
            move |_, _, cx| {
                show_view
                    .update(cx, |view, cx| view.filter_from(Some(&shown), cx))
                    .ok();
            },
        ),
    );
    let ignore_view = view.clone();
    let ignored_login = login.clone();
    menu = if listed {
        menu.item(
            PopupMenuItem::new("Unignore user").on_click(move |_, _, cx| {
                ignore_view
                    .update(cx, |view, cx| view.unignore_user(&ignored_login, cx))
                    .ok();
            }),
        )
    } else {
        menu.item(PopupMenuItem::new("Ignore user").on_click(move |_, _, cx| {
            ignore_view
                .update(cx, |view, cx| view.ignore_user(&ignored_login, cx))
                .ok();
        }))
    };
    if own.is_some() {
        let block_view = view.clone();
        let label = if blocked {
            "Unblock on Twitch"
        } else {
            "Block on Twitch…"
        };
        menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
            block_view
                .update(cx, |view, cx| {
                    view.request_block(login.clone(), user_id.clone(), !blocked, cx)
                })
                .ok();
        }));
    }
    menu
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
