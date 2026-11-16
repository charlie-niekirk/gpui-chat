//! [`ChatView`]: one channel's messages in a virtualised, bottom-following list.
//!
//! * **Virtualisation:** GPUI `list()` over a `ListState` with `ListAlignment::Bottom` and
//!   `FollowMode::Tail`. Only visible items (plus overdraw) are rendered and measured; every item
//!   is a [`MessageElement`] whose layout comes from the shared [`LayoutCache`].
//! * **Incremental updates:** the view observes its [`ChannelStore`]; each store notification
//!   (one per frame batch) is turned by [`ListMirror`] into at most two splices (evicted at the
//!   front, appended at the back) from the store's sequence numbers. The list is never reset per
//!   batch, so measured heights and the scroll position of untouched rows survive.
//! * **Follow / pause:** scrolling up stops tail-following (GPUI does this on an upward wheel
//!   delta) and shows a pill with the number of messages that arrived since; clicking it, or
//!   scrolling back to the bottom, resumes following. GPUI re-engages following during list
//!   layout, after this view rendered, so a zero-size probe painted after the list requests one
//!   more frame when the pill's visibility is stale.
//! * **Settings:** font family/size (via the `FontConfig` global), `appearance.line_height`,
//!   `chat.timestamps` and `emotes.scale` apply live: a change bumps the layout generation (every
//!   cached layout is re-shaped on next use) and asks the list to remeasure. An emote box that
//!   turned out another size, or fell back to text, does not: the image store logs its URL and
//!   the view drops only the cached layouts that draw that URL and remeasures only those rows
//!   (third-party emotes often lack sizes, so this happens a lot and must stay local).
//!   Theme changes and `emotes.animate` only re-render (colours and frames are chosen at paint),
//!   except a switch between light and dark, which re-shapes for Twitch's light emote variants.
//! * **Images:** the view owns the [`FrameImages`] its rows paint into (pinning what is on
//!   screen) and the emote hover state. After the list has painted, the probe schedules one
//!   wake-up for the soonest animation frame change, only if an animated emote was painted,
//!   animation is on and the window is active (GPUI refreshes on (de)activation, so animation
//!   stops and resumes with focus). Nothing animated on screen means no timers at all.
//! * **User card** (PR 5.4): a click on a name or `@mention`, *View user* in the menu or the
//!   `OpenUserCard` action opens [`super::user_card`] over the chat; the view has focus while it
//!   is open, so Esc reaches [`ClearSelection`] here first (it closes the card and hands focus back
//!   to the input box, whose own Esc chain then applies as before).
//! * **Selection and links** (PR 5.6): the list and the thread popover are each a selection
//!   [`Surface`] (see [`super::selection_layer`]). A press in the chat gives the view keyboard
//!   focus (key context [`CHAT_CONTEXT`]), where ⌘C / Ctrl-C copies the selection and Esc clears
//!   it (and asks the owner to focus the input box again); with nothing selected both go on. The
//!   pane also handles Copy, so the command palette reaches it from the input box.

mod find;

pub use find::{SearchHighlight, bindings as find_bindings};

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_chat_core::{
    BotList, ChannelRole, Filters, Ignores, Message, MessageId, Rebuild, visibility,
};
use gpui_chat_settings::DeletedMessages;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FollowMode,
    InteractiveElement, IntoElement, ListAlignment, ListOffset, ListState, MouseButton,
    ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, Window, canvas, div, font, list, point, px,
};

use super::display::DisplayOptions;
use super::element::{BadgeStyle, EmoteStyle, HoveredRow, MessageStyle, OpenThread, Palette};
use super::layout::LayoutCache;
use super::list_sync::{Change, Rows, Splice};
use super::row::{RowContext, message_row, row_menu};
use super::selection_layer::{ClearSelection, CopySelection, OnUser, Surface, UserTarget};
use super::thread::{self, ThreadPanel};
use super::tooltip::EmoteHover;
use super::user_card::{self, CardInputs, CardModel, UserCard};
use crate::actions::CHAT_CONTEXT;
use crate::badges::Badges;
use crate::channels::ChannelStore;
use crate::config;
use crate::fonts::FontConfig;
use crate::ignores;
use crate::images::{FrameImages, Images, LayoutChanges};
use crate::moderation::RowAction;
use crate::theme::Tokens;
use crate::user_directory::{self, Follow};

/// Rows rendered beyond the viewport, so short scrolls do not pop in.
const OVERDRAW: Pixels = px(240.0);
/// Rows shown above a message jumped to (the Mentions view, a notification).
const JUMP_CONTEXT_ROWS: usize = 3;
/// How long a jumped-to row flashes.
const FLASH: Duration = Duration::from_millis(1800);
const PADDING_X: Pixels = px(8.0);
const PADDING_Y: Pixels = px(1.0);

/// The settings a layout depends on.
#[derive(Clone, Debug, PartialEq)]
struct TextConfig {
    family: SharedString,
    size: Pixels,
    line_height: f32,
    options: DisplayOptions,
    emote_scale: f32,
    /// Light theme: Twitch emote URLs (picked when shaping) have light variants.
    light: bool,
    /// How removed messages are drawn: collapsed rows have another height (PR 5.2).
    deleted: DeletedMessages,
}

/// Badges are drawn as squares a bit taller than the text's x-height: 18 px at the default
/// 14 px font (Twitch's own badge size), growing and shrinking with the font.
fn badge_size(font_size: Pixels) -> f32 {
    (font_size.as_f32() * 1.3).round().clamp(12.0, 36.0)
}

/// What the chat view asks of whoever owns it.
#[derive(Debug, Clone)]
pub enum ChatViewEvent {
    /// The user chose Reply on this message: put the composer in reply mode.
    ReplyRequested(Arc<Message>),
    /// Esc cleared the selection: hand focus back to the input box.
    SelectionDismissed,
    /// The user chose a moderation action (a row button or a menu item) on this message (PR 5.2);
    /// the owner confirms it if need be and runs it.
    Moderate(Arc<Message>, RowAction),
    /// Mention was chosen on the user card: put `@login ` in the input box.
    Mention(String),
    /// A moderation action was chosen on the user card (PR 5.4), for a user who may have no message
    /// left in the chat: same confirmation and same way of running it as [`Self::Moderate`].
    /// `user_id` is the user's Twitch id when known (saves a lookup).
    ModerateUser {
        login: String,
        user_id: Option<String>,
        action: RowAction,
    },
    /// *Block on Twitch…* was chosen for this user (PR 5.7): the owner asks first, then blocks.
    BlockUser {
        login: String,
        user_id: Option<String>,
    },
    /// *Unblock on Twitch* was chosen for this user.
    UnblockUser {
        login: String,
        user_id: Option<String>,
    },
}

impl EventEmitter<ChatViewEvent> for ChatView {}

pub struct ChatView {
    store: Entity<ChannelStore>,
    list: ListState,
    /// Which message each item of the list shows: all of them, or only those that are not hidden by
    /// an ignore or a filter (PR 5.7). Shared with the selection surface.
    rows: Rc<RefCell<Rows>>,
    /// What is ignored and which bots are hidden, as of `ignore_generation` of the hub.
    engine: Arc<Ignores>,
    bots: Arc<BotList>,
    ignore_generation: u64,
    /// The find bar's filters (PR 5.7).
    filters: Filters,
    /// The find bar and its search, while it is open (PR 5.7).
    find: Option<find::Find>,
    cache: Rc<RefCell<LayoutCache>>,
    text_config: Option<TextConfig>,
    generation: u64,
    /// Bumped when display texts may change (timestamps, badge definitions): selection offsets
    /// are into display texts, so the selection is dropped then — not on a font or theme change.
    text_generation: u64,
    /// How far into the image store's layout change log this view has applied
    /// ([`Images::layout_version`]); `None` before the first render.
    image_version: Option<u64>,
    /// The badge registry's version for this channel at the last render
    /// ([`gpui_chat_emotes::BadgeRegistry::version`]); `None` before the first render. A change
    /// means badge definitions arrived (a channel's subscriber badges, a few hundred ms after it
    /// was joined): messages already laid out may now resolve more badges.
    badge_version: Option<u64>,
    /// Sequence number of the first message that arrived while not following. The pill counts
    /// from there to the newest message, skipping any the scrollback cap already evicted, so it
    /// never claims more new messages than the list still holds.
    unseen_from: Option<u64>,
    /// Whether the last render showed the pill.
    pill_shown: bool,
    /// How far into the store's log of replaced messages this view has applied
    /// ([`ChannelStore::replaced_since`]).
    replaced_seen: u64,
    /// How many history merges that renumbered the store this view has applied
    /// ([`gpui_chat_core::MessageStore::rebuilds_since`]).
    rebuilds_seen: u64,
    /// Line height of the last render.
    line_height: Pixels,
    images: Rc<RefCell<FrameImages>>,
    hover: Rc<RefCell<EmoteHover>>,
    /// Whether rows offer Reply (there is a composer to reply with).
    can_reply: bool,
    /// The list's row the pointer is over (kept current by the rows when `can_reply`).
    hovered: HoveredRow,
    /// Whether moderation controls can act (the pane has someone to run commands); they are
    /// shown when the logged-in user is also a moderator here (PR 5.2).
    moderation: bool,
    /// Removed messages the user clicked open (`chat.deleted_messages = "collapse"`); only ones the
    /// store still holds are kept.
    revealed: Rc<RefCell<HashSet<MessageId>>>,
    /// How far into the store's log of struck-out messages this view has applied
    /// ([`ChannelStore::struck_since`]).
    struck_seen: u64,
    /// The pointer is on a row's moderation strip: the list stands still (`Some(true)`: it was
    /// following the newest messages and goes back to that afterwards).
    strip_hold: Option<bool>,
    /// The thread popover, while one is open.
    thread: Option<ThreadPanel>,
    /// The user card, while one is open (PR 5.4).
    user_card: Option<UserCard>,
    /// Text selection and links in the list.
    surface: Rc<Surface>,
    /// A message was jumped to (the Mentions view, a notification): the list stands still, so a busy
    /// channel does not scroll it away, until the user scrolls or goes back to the latest messages.
    jump_hold: bool,
    /// The jumped-to row, painted with a flash for a moment (see [`FLASH`]).
    flash: Rc<Cell<Option<u64>>>,
    flash_task: Option<Task<()>>,
    /// Focused by a press in the chat, so ⌘C and Esc reach the selection.
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ChatView {
    pub fn new(store: Entity<ChannelStore>, cx: &mut Context<Self>) -> Self {
        let hub = ignores::current(cx);
        let (engine, bots, ignore_generation) = hub.as_ref().map_or_else(
            || (Arc::default(), Arc::default(), 0),
            |hub| {
                let hub = hub.read(cx);
                (hub.engine(), hub.bots(), hub.generation())
            },
        );
        let filters = Filters::default();
        let (rows, rebuilds_seen) = {
            let messages = store.read(cx).messages();
            let mut rows = Rows::new(messages.first_seq(), messages.next_seq());
            // Ignored users are hidden from the first frame.
            if !engine.is_empty() {
                let shows = |seq: u64| {
                    messages.get_by_seq(seq).is_none_or(|message| {
                        visibility(message, &engine, &filters, &bots).is_visible()
                    })
                };
                rows.show_only(Some(&shows), messages.first_seq(), messages.next_seq());
            }
            (rows, messages.rebuild_serial())
        };
        let list = ListState::new(rows.len(), ListAlignment::Bottom, OVERDRAW);
        list.set_follow_mode(FollowMode::Tail);
        let subscriptions = vec![
            // One notify per frame batch from the store becomes one sync here.
            cx.observe(&store, |this, _, cx| this.sync(cx)),
            // Line height and timestamps live in the settings store; font family and size in
            // the `FontConfig` global (resolved against installed fonts); colours in `Tokens`.
            cx.observe(&config::store(cx), |_, _, cx| cx.notify()),
            cx.observe_global::<FontConfig>(|_, cx| cx.notify()),
            cx.observe_global::<Tokens>(|_, cx| cx.notify()),
            // Loaders store badge sets off the UI thread; this is how a view hears about it.
            cx.observe_global::<Badges>(|_, cx| cx.notify()),
        ];
        let mut subscriptions = subscriptions;
        // What is ignored changed (the settings, the block list, a login): rows come and go.
        if let Some(hub) = &hub {
            subscriptions.push(cx.observe(hub, |this, _, cx| this.ignores_changed(cx)));
        }
        // An account lookup arriving redraws the user card (nothing else shows it).
        if let Some(directory) = user_directory::current(cx) {
            subscriptions.push(cx.observe(&directory, |this, _, cx| {
                if this.user_card.is_some() {
                    cx.notify();
                }
            }));
        }
        let cache = Rc::new(RefCell::new(LayoutCache::default()));
        let rows = Rc::new(RefCell::new(rows));
        let surface = Surface::new(Some(list.clone()), cache.clone());
        surface.set_rows(rows.clone());
        Self {
            store,
            surface,
            focus_handle: cx.focus_handle(),
            list,
            rows,
            engine,
            bots,
            ignore_generation,
            filters,
            find: None,
            cache,
            text_config: None,
            generation: 0,
            text_generation: 0,
            image_version: None,
            badge_version: None,
            unseen_from: None,
            pill_shown: false,
            replaced_seen: 0,
            rebuilds_seen,
            line_height: px(0.0),
            images: Rc::default(),
            hover: Rc::default(),
            can_reply: false,
            hovered: Rc::default(),
            moderation: false,
            revealed: Rc::default(),
            struck_seen: 0,
            strip_hold: None,
            jump_hold: false,
            flash: Rc::default(),
            flash_task: None,
            thread: None,
            user_card: None,
            _subscriptions: subscriptions,
        }
    }

    /// Whether rows offer Reply: only a chat with a composer can reply.
    pub fn set_can_reply(&mut self, can_reply: bool, cx: &mut Context<Self>) {
        if self.can_reply != can_reply {
            self.can_reply = can_reply;
            cx.notify();
        }
    }

    /// Whether moderation controls can act: the pane can run commands (PR 5.2). What the user may
    /// do is decided per channel by their role there.
    pub fn set_moderation(&mut self, moderation: bool, cx: &mut Context<Self>) {
        if self.moderation != moderation {
            self.moderation = moderation;
            cx.notify();
        }
    }

    /// *Block on Twitch…* (`block`) or *Unblock on Twitch* was chosen for `login`: tell the owner,
    /// which asks first and runs it.
    pub fn request_block(
        &mut self,
        login: String,
        user_id: Option<String>,
        block: bool,
        cx: &mut Context<Self>,
    ) {
        cx.emit(if block {
            ChatViewEvent::BlockUser { login, user_id }
        } else {
            ChatViewEvent::UnblockUser { login, user_id }
        });
    }

    /// A moderation action was chosen on `message`: tell the owner.
    pub fn request_moderation(
        &mut self,
        message: Arc<Message>,
        action: RowAction,
        cx: &mut Context<Self>,
    ) {
        cx.emit(ChatViewEvent::Moderate(message, action));
    }

    /// Shows the text of a collapsed removed message.
    pub fn reveal(&mut self, id: &MessageId, cx: &mut Context<Self>) {
        if !self.revealed.borrow_mut().insert(id.clone()) {
            return;
        }
        let seq = self.store.read(cx).messages().seq_of(id.as_str());
        if let Some(seq) = seq {
            self.remeasure_seqs(&[seq]);
        }
        cx.notify();
    }

    /// Scrolls the list so that message `id` is in view, a few rows below the top (so the lines
    /// before it show, as when following a link to a message). Pauses following, like any scroll
    /// away from the bottom. `false` when the store no longer holds it (it left the scrollback).
    pub fn scroll_to_message(&mut self, id: &MessageId, cx: &mut Context<Self>) -> bool {
        let Some(seq) = self.store.read(cx).messages().seq_of(id.as_str()) else {
            return false;
        };
        let mut index = self.rows.borrow().index_of(seq);
        // A filter hides it: the user wants to see the message, so the filters go.
        if index.is_none() && self.filters.is_active() {
            self.clear_filters(cx);
            index = self.rows.borrow().index_of(seq);
        }
        let Some(ix) = index else {
            return false;
        };
        self.scroll_to_row(ix, seq, cx);
        true
    }

    /// Scrolls to row `ix`, which shows message `seq`: a few rows below the top, held there, with
    /// the row flashing.
    fn scroll_to_row(&mut self, ix: usize, seq: u64, cx: &mut Context<Self>) {
        self.list.scroll_to(ListOffset {
            item_ix: ix.saturating_sub(JUMP_CONTEXT_ROWS),
            offset_in_item: px(0.0),
        });
        // Near the end the list would be following again at once and new messages would carry the
        // target away: it stands still instead, behind the "new messages" pill.
        self.jump_hold = true;
        self.update_hold();
        self.flash_row(seq, cx);
    }

    /// Flashes the row of message `seq` for a moment.
    fn flash_row(&mut self, seq: u64, cx: &mut Context<Self>) {
        self.flash.set(Some(seq));
        let flash = self.flash.clone();
        self.flash_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLASH).await;
            flash.set(None);
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
        cx.notify();
    }

    /// The pointer entered (`true`) or left a row's moderation strip. While it is on it the list
    /// stands still, so no message can arrive and slide another row under the pointer between aiming
    /// at a button and clicking it (a Delete or a 10-minute timeout asks no confirmation); the
    /// new messages wait behind the "N new messages" pill and the list resumes following when the
    /// pointer leaves.
    pub fn hold_for_strip(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == self.strip_hold.is_some() {
            return;
        }
        if on {
            // Stopped where it is, the way a selection stops it (a bare pause would re-engage at
            // the bottom as soon as the next message is laid out).
            self.strip_hold = Some(self.surface.freeze_list());
        } else if self.strip_hold.take() == Some(true) && !self.jump_hold {
            self.list.set_follow_mode(FollowMode::Tail);
            self.unseen_from = None;
        }
        self.update_hold();
        cx.notify();
    }

    /// The list is held every frame while a moderation strip or a jump asks for it.
    fn update_hold(&self) {
        self.surface
            .keep_frozen(self.strip_hold.is_some() || self.jump_hold);
    }

    /// The user took over (scrolled, or went back to the latest messages): the jump no longer
    /// holds the list. Following resumes by itself if the view is at the end.
    fn end_jump(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.jump_hold) {
            self.update_hold();
            cx.notify();
        }
    }

    /// Reply was chosen on `message`: tell the owner.
    pub fn request_reply(&mut self, message: Arc<Message>, cx: &mut Context<Self>) {
        cx.emit(ChatViewEvent::ReplyRequested(message));
    }

    /// Opens the thread popover on the thread rooted at `root` (replacing an open one).
    pub fn open_thread(&mut self, root: MessageId, _window: &mut Window, cx: &mut Context<Self>) {
        if self.thread.as_ref().is_some_and(|panel| panel.root == root) {
            return;
        }
        self.thread = Some(ThreadPanel::new(root));
        cx.notify();
    }

    pub fn close_thread(&mut self, cx: &mut Context<Self>) {
        if self.thread.take().is_some() {
            cx.notify();
        }
    }

    /// Opens the user card for `login` (a name, a mention or "View user" was chosen) at `anchor`
    /// (window coordinates), replacing an open one. The view takes focus, so Esc closes the card.
    /// A lookup that failed earlier is asked again.
    pub fn open_user_card(
        &mut self,
        login: &str,
        origin_id: Option<&str>,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let login = login.trim_start_matches('@').to_ascii_lowercase();
        if !gpui_chat_core::is_login(&login) {
            return;
        }
        if let Some(directory) = user_directory::current(cx) {
            directory.update(cx, |directory, cx| directory.retry(&login, cx));
        }
        self.user_card = Some(UserCard::new(&login, origin_id, anchor));
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// The Twitch id of the author of message `seq` when `login` is that author's login: who the
    /// card moderates when it is opened from their name or message.
    pub fn sender_id(&self, seq: u64, login: &str, cx: &App) -> Option<String> {
        let store = self.store.read(cx);
        let author = store.messages().get_by_seq(seq)?.author.as_ref()?;
        author
            .login
            .eq_ignore_ascii_case(login)
            .then(|| author.user_id.clone())
            .flatten()
    }

    /// Opens the card of the author of the first selected message (the `OpenUserCard` action);
    /// whether there was one.
    pub fn open_user_card_for_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let selected = self
            .thread
            .as_ref()
            .and_then(|panel| {
                panel
                    .surface
                    .selection_start()
                    .map(|seq| (seq, &panel.surface))
            })
            .or_else(|| {
                self.surface
                    .selection_start()
                    .map(|seq| (seq, &self.surface))
            });
        let Some((seq, surface)) = selected else {
            return false;
        };
        let anchor = surface.anchor_of(seq);
        let author = self
            .store
            .read(cx)
            .messages()
            .get_by_seq(seq)
            .and_then(|message| message.author.as_ref())
            .map(|author| (author.login.clone(), author.user_id.clone()));
        match (author, anchor) {
            (Some((login, id)), Some(anchor)) => {
                self.open_user_card(&login, id.as_deref(), anchor, window, cx);
                self.user_card.is_some()
            }
            _ => false,
        }
    }

    /// Closes the user card; whether one was open.
    pub fn close_user_card(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.user_card.take().is_some();
        if open {
            cx.notify();
        }
        open
    }

    /// The login of the open user card, if any.
    #[cfg(test)]
    pub fn user_card_login(&self) -> Option<&str> {
        self.user_card.as_ref().map(|card| &*card.login)
    }

    /// Asks Helix again for `login` after a failed lookup (the card's Retry).
    pub fn retry_user_lookup(&mut self, login: &str, cx: &mut Context<Self>) {
        if let Some(directory) = user_directory::current(cx) {
            directory.update(cx, |directory, cx| directory.retry(login, cx));
        }
    }

    /// Mention was chosen on the card.
    pub fn mention(&mut self, login: String, cx: &mut Context<Self>) {
        self.close_user_card(cx);
        cx.emit(ChatViewEvent::Mention(login));
    }

    /// Reply was chosen on the card, for the user's latest message.
    pub fn reply_from_card(&mut self, message: Arc<Message>, cx: &mut Context<Self>) {
        self.close_user_card(cx);
        cx.emit(ChatViewEvent::ReplyRequested(message));
    }

    /// A moderation action was chosen on the card.
    pub fn moderate_user(
        &mut self,
        login: String,
        user_id: Option<String>,
        action: RowAction,
        cx: &mut Context<Self>,
    ) {
        self.close_user_card(cx);
        cx.emit(ChatViewEvent::ModerateUser {
            login,
            user_id,
            action,
        });
    }

    /// A message of the card's list was clicked: close the card and scroll the chat to it, by the
    /// one scroll-to-message path ([`ChatView::scroll_to_message`]: a few rows of context, following
    /// paused, the row flashed). Nothing if the message left the scrollback.
    pub fn jump_from_card(&mut self, seq: u64, cx: &mut Context<Self>) {
        self.close_user_card(cx);
        let id = self
            .store
            .read(cx)
            .messages()
            .get_by_seq(seq)
            .map(|message| message.id.clone());
        if let Some(id) = id {
            self.scroll_to_message(&id, cx);
        }
    }

    /// What ⌘C would copy: the popover's selection, else the list's.
    pub fn selected_text(&self, cx: &App) -> Option<String> {
        let messages = self.store.read(cx).messages();
        self.thread
            .as_ref()
            .and_then(|panel| panel.surface.selected_text(messages))
            .or_else(|| self.surface.selected_text(messages))
    }

    /// Puts the selection on the clipboard; whether there was one.
    pub fn copy_selection(&self, cx: &mut App) -> bool {
        let Some(text) = self.selected_text(cx) else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    /// Drops the selection (list and popover); whether there was one.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let list = self.surface.clear();
        let popover = self
            .thread
            .as_ref()
            .is_some_and(|panel| panel.surface.clear());
        if list || popover {
            cx.notify();
        }
        list || popover
    }

    /// The root of the open thread popover, if any.
    #[cfg(test)]
    pub fn open_thread_root(&self) -> Option<&MessageId> {
        self.thread.as_ref().map(|panel| &panel.root)
    }

    /// The layout caches in use: the list's and the thread popover's.
    fn caches(&self) -> impl Iterator<Item = &Rc<RefCell<LayoutCache>>> {
        std::iter::once(&self.cache).chain(self.thread.as_ref().map(|panel| &panel.cache))
    }

    /// Brings the list up to date with the store: splices, drops evicted layouts, counts unseen.
    fn sync(&mut self, cx: &mut Context<Self>) {
        // History merged *before* messages already shown renumbers the store's messages. Each such
        // merge is replayed as the row insertions it was (`Rebuild`), so measured rows, the
        // reader's scroll position (GPUI shifts its anchor for splices above it) and the
        // "new messages" count all survive. More merges than the store remembers: the plain sync
        // below sees numbers that all moved on and replaces every row.
        let (rebuilds, serial) = self
            .store
            .read(cx)
            .messages()
            .rebuilds_since(self.rebuilds_seen);
        self.rebuilds_seen = serial;
        match &rebuilds {
            Some(rebuilds) => rebuilds
                .iter()
                .for_each(|rebuild| self.apply_rebuild(rebuild, cx)),
            // The list replaces every row below: nothing keyed by an old number means anything.
            None => {
                self.surface.reset();
                if let Some(panel) = &self.thread {
                    panel.surface.reset();
                }
            }
        }
        let (first, next) = {
            let messages = self.store.read(cx).messages();
            (messages.first_seq(), messages.next_seq())
        };
        self.sync_range(first, next, cx);
        debug_assert_eq!(self.list.item_count(), self.rows.borrow().len());
        self.sync_find(cx);
        // A message replaced in place (the user's echo swapped for Twitch's copy, with its real
        // emotes) can be a different height: measure those rows again.
        let (replaced, serial) = self.store.read(cx).replaced_since(self.replaced_seen);
        if serial != self.replaced_seen {
            self.replaced_seen = serial;
            match replaced {
                Some(seqs) => self.remeasure_seqs(&seqs),
                None => self.list.remeasure(),
            }
        }
        // Messages struck out: when removed ones are drawn collapsed that is another row height,
        // for those rows only.
        let (struck, serial) = self.store.read(cx).struck_since(self.struck_seen);
        if serial != self.struck_seen {
            self.struck_seen = serial;
            if config::current(cx).chat.deleted_messages == DeletedMessages::Collapse {
                match struck {
                    Some(seqs) => self.remeasure_seqs(&seqs),
                    None => self.list.remeasure(),
                }
            }
        }
        // Revealed messages that left the store are forgotten.
        {
            let store = self.store.read(cx);
            self.revealed
                .borrow_mut()
                .retain(|id| store.messages().contains(id.as_str()));
        }
        // Also covers changes without new messages (deletions): rows re-render.
        cx.notify();
    }

    /// Whether `message` gets a row: not ignored (or ignored and collapsed) and passing the
    /// filters (PR 5.7).
    fn shows(&self, message: &Message) -> bool {
        visibility(message, &self.engine, &self.filters, &self.bots).is_visible()
    }

    /// Whether any message may be hidden, so the rows are an explicit list.
    fn hiding(&self) -> bool {
        !self.engine.is_empty() || self.filters.is_active()
    }

    /// Moves the list from the range it shows to `first..next` of the store's sequence numbers.
    fn sync_range(&mut self, first: u64, next: u64, cx: &App) {
        let previous_next = self.rows.borrow().next_seq();
        let change: Change = {
            let messages = self.store.read(cx).messages();
            let shows = |seq: u64| messages.get_by_seq(seq).is_none_or(|m| self.shows(m));
            self.rows.borrow_mut().sync(first, next, &shows)
        };
        if !change.is_empty() {
            if change.removed > 0 {
                self.list.splice(0..change.removed, 0);
            }
            if change.appended > 0 {
                let end = self.list.item_count();
                self.list.splice(end..end, change.appended);
            }
            for cache in self.caches() {
                cache.borrow_mut().evict_before(first);
            }
            if change.appended > 0 && !self.list.is_following_tail() {
                self.unseen_from.get_or_insert(previous_next);
            }
        }
    }

    /// Replays one history merge that renumbered the store: the list first catches up with the
    /// state the merge started from, then the rows it dropped at the front go and the rows it
    /// brought are inserted, and everything keyed by an old sequence number is moved to its new one.
    fn apply_rebuild(&mut self, rebuild: &Rebuild, cx: &App) {
        self.sync_range(rebuild.old_first, rebuild.old_next, cx);
        let splices = {
            let messages = self.store.read(cx).messages();
            let shows = |seq: u64| messages.get_by_seq(seq).is_none_or(|m| self.shows(m));
            self.rows.borrow_mut().renumber(
                &|old| rebuild.map_seq(old),
                rebuild.new_first,
                rebuild.new_next,
                self.hiding().then_some(&shows as &dyn Fn(u64) -> bool),
            )
        };
        self.apply_splices(&splices);
        for cache in self.caches() {
            cache.borrow_mut().remap(|seq| rebuild.map_seq(seq));
        }
        // The pill counts from the first message that arrived while scrolled away: still that
        // message, under its new number (the oldest survivor if it was dropped).
        self.unseen_from = self
            .unseen_from
            .map(|from| rebuild.map_seq(from).unwrap_or(rebuild.new_first));
        // The selection (and the right-click target, the press in progress) follow their messages.
        let map = |seq| rebuild.map_seq(seq);
        self.surface.remap(&map);
        if let Some(panel) = &self.thread {
            panel.surface.remap(&map);
        }
        // The row under the pointer is found again by the next mouse move.
        self.hovered.set(None);
        if let Some(panel) = &self.thread {
            panel.hovered.set(None);
        }
    }

    fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        self.jump_hold = false;
        self.update_hold();
        self.list.set_follow_mode(FollowMode::Tail);
        self.unseen_from = None;
        cx.notify();
    }

    #[cfg(test)]
    pub fn list_state(&self) -> &ListState {
        &self.list
    }

    /// New messages below the paused view that are still in the store.
    pub fn unseen(&self) -> usize {
        self.unseen_from
            .map_or(0, |from| self.rows.borrow().rows_from(from))
    }

    #[cfg(test)]
    pub fn pill_shown(&self) -> bool {
        self.pill_shown
    }

    #[cfg(test)]
    pub fn cache_stats(&self) -> super::layout::CacheStats {
        self.cache.borrow().stats()
    }

    /// The current text config; bumps the generation (and remeasures) when it changed.
    fn update_text_config(&mut self, cx: &Context<Self>) -> TextConfig {
        let font = cx.global::<FontConfig>();
        let settings = config::current(cx);
        let current = TextConfig {
            family: font.family.clone(),
            size: font.size,
            line_height: settings.appearance.line_height,
            options: DisplayOptions {
                timestamps: settings.chat.timestamps,
            },
            emote_scale: settings.emotes.scale,
            light: cx.global::<Tokens>().light,
            deleted: settings.chat.deleted_messages,
        };
        if self.text_config.as_ref() != Some(&current) {
            if let Some(previous) = &self.text_config {
                if previous.options != current.options {
                    self.text_generation += 1;
                }
                self.generation += 1;
                self.list.remeasure();
                self.retain_generation();
            }
            self.text_config = Some(current.clone());
        }
        current
    }
}

impl ChatView {
    /// Applies the emote boxes that changed since the last render: only the cached layouts that
    /// draw a changed URL are dropped (they are shaped again with the box the image store knows
    /// by now) and only their rows are measured again. If the log no longer reaches back far
    /// enough, everything is laid out again.
    fn apply_image_changes(&mut self, cx: &Context<Self>) {
        let Some(images) = cx.try_global::<Images>() else {
            return;
        };
        let version = images.layout_version();
        // Nothing is cached before the first render, so there is nothing to apply.
        let Some(seen) = self.image_version.replace(version) else {
            return;
        };
        match images.changes_since(seen) {
            LayoutChanges::None => {}
            LayoutChanges::Urls(urls) => {
                // The popover's rows are not in the list: they are measured at every frame, so
                // dropping their layouts is all they need.
                for cache in self.caches().skip(1) {
                    cache.borrow_mut().invalidate_urls(&urls);
                }
                let seqs = self.cache.borrow_mut().invalidate_urls(&urls);
                self.remeasure_seqs(&seqs);
            }
            LayoutChanges::Everything => {
                self.generation += 1;
                self.list.remeasure();
                self.retain_generation();
            }
        }
    }

    /// Lays every message out again if badge definitions for this channel arrived since the last
    /// render. (Only the rows on screen are shaped again, lazily; and a registry that did not
    /// change for this channel costs one lock-free-ish read.)
    fn apply_badge_changes(&mut self, cx: &Context<Self>) {
        let Some(badges) = cx.try_global::<Badges>() else {
            return;
        };
        let version = badges.registry().version(self.store.read(cx).channel());
        let Some(seen) = self.badge_version.replace(version) else {
            return;
        };
        if seen != version {
            self.generation += 1;
            self.text_generation += 1;
            self.list.remeasure();
            self.retain_generation();
        }
    }

    /// Drops the cached layouts of older generations from every cache.
    fn retain_generation(&self) {
        for cache in self.caches() {
            cache.borrow_mut().retain_key(self.generation);
        }
    }

    /// Asks the list to measure the rows of the messages `seqs` again.
    fn remeasure_seqs(&self, seqs: &[u64]) {
        let rows = self.rows.borrow();
        // `seqs` is ascending: merge neighbours into one call.
        let mut run: Option<std::ops::Range<usize>> = None;
        for &seq in seqs {
            // A hidden message has no row to measure.
            let Some(ix) = rows.index_of(seq) else {
                continue;
            };
            run = match run {
                Some(range) if range.end == ix => Some(range.start..ix + 1),
                Some(range) => {
                    self.list.remeasure_items(range);
                    Some(ix..ix + 1)
                }
                None => Some(ix..ix + 1),
            };
        }
        if let Some(range) = run {
            self.list.remeasure_items(range);
        }
    }
}

/// A pill label: `1 new message`, `12 new messages`, or a plain jump when nothing is new.
pub fn pill_label(unseen: usize) -> String {
    match unseen {
        0 => "Jump to latest".to_owned(),
        1 => "1 new message".to_owned(),
        n => format!("{n} new messages"),
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        let config = self.update_text_config(cx);
        self.apply_image_changes(cx);
        self.apply_badge_changes(cx);
        // What the rows painted last frame is what is on screen: pin it, let older pins go and
        // release the textures of evicted images nothing shows any more.
        self.images.borrow_mut().begin_frame();
        Images::sweep(window, cx);
        let animate = config::current(cx).emotes.animate;
        let clock = cx
            .try_global::<Images>()
            .filter(|_| animate)
            .map(|images| images.clock(Instant::now()));
        let following = self.list.is_following_tail();
        if following {
            self.unseen_from = None;
        }
        self.pill_shown = !following;

        let regular = font(config.family.clone());
        self.line_height = px((config.size.as_f32() * config.line_height).round());
        let selection_color = tokens.accent.opacity(0.3);
        self.surface
            .begin_frame(self.text_generation, config.options, selection_color, None);
        // Moderation controls appear for a moderator of this channel, when the pane can run them.
        let (channel, role) = {
            let store = self.store.read(cx);
            let role = if self.moderation {
                store.role()
            } else {
                ChannelRole::Viewer
            };
            (store.channel().clone(), role)
        };
        let track_hover = self.can_reply || role.can_moderate();
        // The pointer left the rows while a strip held the list (the strip went away with it).
        if self.strip_hold.is_some() && self.hovered.get().is_none() {
            self.hold_for_strip(false, cx);
        }
        let own_login: Option<Arc<str>> = crate::account::own_login(cx).map(Arc::from);
        let style = Rc::new(MessageStyle {
            bold_font: regular.clone().bold(),
            font: regular,
            font_size: config.size,
            line_height: self.line_height,
            options: config.options,
            generation: self.generation,
            padding: point(PADDING_X, PADDING_Y),
            palette: Palette::from_tokens(&tokens),
            emotes: EmoteStyle {
                scale: config.emote_scale,
                clock,
                light: config.light,
            },
            badges: BadgeStyle {
                size: badge_size(config.size),
            },
            channel: self.store.read(cx).channel().clone(),
            images: self.images.clone(),
            hover: self.hover.clone(),
            reply_line: true,
            // A reply's context line opens its thread.
            on_open_thread: Some({
                let view = cx.entity().downgrade();
                Rc::new(move |root: &MessageId, window: &mut Window, cx: &mut App| {
                    view.update(cx, |view, cx| view.open_thread(root.clone(), window, cx))
                        .ok();
                }) as OpenThread
            }),
            hovered_row: track_hover.then(|| self.hovered.clone()),
            selection: Some(self.surface.clone()),
            flash: Some(self.flash.clone()),
            search: self.search_highlight(),
        });
        let handle = cx.entity().downgrade();
        // A click on a name or mention opens its card (the callback is the live view's).
        let on_user: Rc<OnUser> = {
            let view = handle.clone();
            Rc::new(
                move |target: UserTarget, window: &mut Window, cx: &mut App| {
                    view.update(cx, |view, cx| {
                        // The id of the account that sent the message the name or mention is
                        // in, when it is that account's own name.
                        let origin = target
                            .seq
                            .and_then(|seq| view.sender_id(seq, &target.login, cx));
                        view.open_user_card(
                            &target.login,
                            origin.as_deref(),
                            target.position,
                            window,
                            cx,
                        );
                    })
                    .ok();
                },
            )
        };
        self.surface.set_on_user(on_user.clone());
        if let Some(panel) = &self.thread {
            panel.surface.set_on_user(on_user);
        }
        let card = self.user_card_element(&tokens, config.deleted, window, cx);
        let find_bar = self.find_bar(&tokens, window, cx);
        let filter_strip = self.filter_strip(&tokens, cx);
        let rows = |id, hovered: &HoveredRow| RowContext {
            id,
            view: handle.clone(),
            tokens,
            hovered: track_hover.then(|| hovered.get()).flatten(),
            can_reply: self.can_reply,
            role,
            channel: channel.clone(),
            deleted: config.deleted,
            revealed: self.revealed.clone(),
            own_login: own_login.clone(),
            ignored: (self.engine.show_collapsed() && !self.filters.is_active())
                .then(|| self.engine.clone()),
        };
        let thread_panel = self.thread.as_ref().map(|panel| {
            // The thread is the context: no reply lines inside it, and no popover on top of it.
            let style = Rc::new(MessageStyle {
                reply_line: false,
                on_open_thread: None,
                hover: panel.hover.clone(),
                hovered_row: track_hover.then(|| panel.hovered.clone()),
                selection: Some(panel.surface.clone()),
                flash: None,
                search: None,
                ..(*style).clone()
            });
            let view = thread::thread_view(self.store.read(cx).messages(), panel.root.as_str());
            panel.surface.begin_frame(
                self.text_generation,
                config.options,
                selection_color,
                Some(view.seqs()),
            );
            let rows = rows("thread-row", &panel.hovered);
            thread::render(panel, &view, &style, &rows, self.store.clone(), cx)
        });
        let store = self.store.clone();
        let cache = self.cache.clone();
        let order = self.rows.clone();
        let list_rows = rows("chat-row", &self.hovered);
        let messages = list(self.list.clone(), move |ix, _window, cx| {
            let Some(seq) = order.borrow().seq_at(ix) else {
                return div().into_any_element();
            };
            match store.read(cx).messages().get_by_seq(seq) {
                Some(message) => {
                    message_row(seq, message, style.clone(), cache.clone(), &list_rows)
                }
                None => div().into_any_element(),
            }
        })
        .size_full();

        // Runs after the list's prepaint (which updates the follow state): if the pill shown
        // this frame no longer matches, render once more. Its paint runs after the rows painted:
        // schedule the next animation frame, if anything animated was drawn.
        let probe = {
            let list = self.list.clone();
            let pill_shown = self.pill_shown;
            let images = self.images.clone();
            let view = cx.entity_id();
            canvas(
                move |_, window, _| {
                    if list.is_following_tail() == pill_shown {
                        window.request_animation_frame();
                    }
                },
                move |_, _, window, cx| {
                    let active = window.is_window_active();
                    images.borrow_mut().schedule_wake(view, animate, active, cx);
                },
            )
            .absolute()
            .size_0()
        };

        let pill = self.pill_shown.then(|| {
            div()
                .absolute()
                .bottom_3()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("chat-jump-to-latest")
                        .px_3()
                        .py_1()
                        .rounded_full()
                        .bg(tokens.accent)
                        .text_color(tokens.accent_text)
                        .text_sm()
                        .shadow_md()
                        .cursor_pointer()
                        // Not a press in the chat: it must not start (or hold for) a selection.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_latest(cx)))
                        .child(format!("↓ {}", pill_label(self.unseen()))),
                )
        });

        // The channel name and state are shown by the pane header (`crate::pane`).
        // One right-click menu for the whole list: it resolves the row under the pointer.
        let menu = row_menu(
            cx.entity().downgrade(),
            self.store.clone(),
            self.surface.clone(),
            self.can_reply,
            self.moderation,
        );
        div()
            .id("chat-view")
            .key_context(CHAT_CONTEXT)
            .track_focus(&self.focus_handle)
            // ⌘C and Esc with the chat focused. With nothing selected they go on (the pane, the
            // window), so other handlers of the key still run.
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                if !this.copy_selection(cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                // The card first: it is the thing on top, and one Esc closes one thing.
                if this.close_user_card(cx) || this.clear_selection(cx) {
                    cx.emit(ChatViewEvent::SelectionDismissed);
                } else {
                    cx.propagate();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .children(find_bar)
            .children(filter_strip)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    // The user scrolling takes the list back from a jump.
                    .on_scroll(cx.listener(|this, _, _, cx| this.end_jump(cx)))
                    // First, so the rows and what is drawn over them see a press before it does.
                    .child(self.surface.layer())
                    .child(messages)
                    .child(probe)
                    .children(pill)
                    .children(thread_panel)
                    .context_menu(menu),
            )
            .children(card)
    }
}

impl ChatView {
    /// The open user card as an element, after starting the lookups it needs; `None` without one.
    fn user_card_element(
        &mut self,
        tokens: &Tokens,
        deleted: DeletedMessages,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        let directory = user_directory::current(cx);
        // The account changed (log out, log in as someone else): what was looked up belonged to
        // the old one. Deferred, since this is a render and the directory notifies.
        if let Some(directory) = directory.clone() {
            cx.defer(move |cx| directory.update(cx, |directory, cx| directory.sync_account(cx)));
        }
        let Some(card) = self.user_card.as_mut() else {
            self.surface.set_overlay(None);
            if let Some(panel) = &self.thread {
                panel.surface.set_overlay(None);
            }
            return None;
        };
        let (login, origin_id, anchor) = (card.login.clone(), card.origin_id.clone(), card.anchor);
        let logged_in = user_directory::available(cx);
        let own_login = crate::account::own_login(cx);
        let now = crate::live::now().unix_millis().div_euclid(1000);
        let (model, role, room_id, channel) = {
            let store = self.store.read(cx);
            // Only what arrived since the last frame is looked at, not the scrollback.
            let messages = card.messages.update(store.messages());
            let profile = directory
                .as_ref()
                .and_then(|directory| directory.read(cx).profile(&login).cloned());
            let room_id = store.room().room_id.clone();
            let follow = |user_id: &str| -> Option<Follow> {
                let (directory, room_id) = (directory.as_ref()?, room_id.as_deref()?);
                directory.read(cx).follow(room_id, user_id)
            };
            let model = CardModel::build(
                &login,
                origin_id.as_deref(),
                messages,
                store.channel(),
                profile.as_ref(),
                &follow,
                CardInputs {
                    role: store.role(),
                    own_login: own_login.as_deref(),
                    moderation: self.moderation,
                    can_reply: self.can_reply,
                    logged_in,
                    now,
                },
            );
            (model, store.role(), room_id, store.channel().clone())
        };
        // Account details are asked for once per user and session (idempotent, off the UI thread).
        if logged_in {
            let wants_follow = model.wants_follow(role, room_id.as_deref());
            let login = login.to_string();
            if let Some(directory) = directory {
                cx.defer(move |cx| {
                    directory.update(cx, |directory, cx| {
                        directory.lookup(&login, cx);
                        if let Some((room, user)) = wants_follow {
                            directory.lookup_follow(&room, &user, cx);
                        }
                    });
                });
            }
        }
        let mut surfaces = vec![self.surface.clone()];
        if let Some(panel) = &self.thread {
            surfaces.push(panel.surface.clone());
        }
        Some(user_card::render(
            user_card::Env {
                tokens: *tokens,
                view: cx.entity().downgrade(),
                owner: cx.entity_id(),
                images: self.images.clone(),
                deleted,
                model: &model,
                channel,
                anchor,
                can_login: crate::account::current(cx).is_some(),
                surfaces,
            },
            window,
            cx,
        ))
    }
}

impl Focusable for ChatView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_chat_core::{Author, ChannelId, ChatEvent, Fragment, Message, Timestamp};
    use gpui_chat_settings::{AppearanceSettings, DeletedMessages, Settings, SettingsEvent};
    use gpui_kit::{
        AppContext as _, Entity, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Point,
        ScrollDelta, ScrollEvent, TestAppContext, VisualTestContext, size,
    };

    use std::cell::Cell;

    use super::*;
    use crate::chat::selection;
    use crate::chat::selection_layer::DOUBLE_CLICK;
    use crate::{fonts, theme};

    #[test]
    fn pill_labels() {
        assert_eq!(pill_label(0), "Jump to latest");
        assert_eq!(pill_label(1), "1 new message");
        assert_eq!(pill_label(42), "42 new messages");
    }

    fn chat(n: u64, text: &str) -> ChatEvent {
        ChatEvent::Message {
            channel: ChannelId::new("test"),
            message: Arc::new(Message::chat(
                format!("m{n}"),
                Timestamp(0),
                Author::new("al", "Al"),
                vec![Fragment::Text(text.into())],
            )),
        }
    }

    struct Harness {
        store: Entity<ChannelStore>,
        view: Entity<ChatView>,
        settings: Entity<config::SettingsStore>,
        next: u64,
    }

    impl Harness {
        fn new(cx: &mut TestAppContext, capacity: usize) -> (Self, &mut VisualTestContext) {
            Self::with_settings(cx, capacity, Settings::default(), false)
        }

        /// A harness whose settings are `settings`, with the app's ignore hub when `ignores`.
        fn with_settings(
            cx: &mut TestAppContext,
            capacity: usize,
            settings: Settings,
            ignores: bool,
        ) -> (Self, &mut VisualTestContext) {
            let settings = cx.update(|cx| {
                gpui_kit::init(cx);
                let settings = config::install_for_test(settings, cx);
                fonts::init(&AppearanceSettings::default(), cx);
                theme::apply_settings(&AppearanceSettings::default(), None, cx);
                if ignores {
                    crate::ignores::init(true, cx);
                }
                settings
            });
            let store = cx.new(|_| ChannelStore::new(ChannelId::new("test"), capacity));
            let view_store = store.clone();
            let (view, cx) = cx.add_window_view(|_, cx| ChatView::new(view_store, cx));
            cx.simulate_resize(size(px(800.0), px(600.0)));
            (
                Self {
                    store,
                    view,
                    settings,
                    next: 0,
                },
                cx,
            )
        }

        /// Pushes `count` messages as one batch (one store notify), then lets the window draw.
        fn push(&mut self, count: u64, text: &str, cx: &mut VisualTestContext) {
            let events: Vec<_> = (self.next..self.next + count)
                .map(|n| chat(n, text))
                .collect();
            self.next += count;
            self.store.update(cx, |store, cx| {
                store.apply_all(events);
                cx.notify();
            });
            cx.run_until_parked();
        }

        fn items(&self, cx: &mut VisualTestContext) -> usize {
            self.view
                .read_with(cx, |view, _| view.list_state().item_count())
        }

        fn following(&self, cx: &mut VisualTestContext) -> bool {
            self.view
                .read_with(cx, |view, _| view.list_state().is_following_tail())
        }

        /// Lines of the newest message as last laid out, and the line height in effect.
        fn last_message_lines(&self, cx: &mut VisualTestContext) -> (usize, Pixels) {
            self.view.read_with(cx, |view, _| {
                let flow = view
                    .cache
                    .borrow()
                    .flow_of(self.next - 1)
                    .expect("the newest message was laid out");
                (flow.line_count(), view.line_height)
            })
        }

        fn scroll(&self, dy: f32, cx: &mut VisualTestContext) {
            cx.simulate_event(ScrollEvent {
                position: point(px(400.0), px(300.0)),
                delta: ScrollDelta::Pixels(point(px(0.0), px(dy))),
                ..Default::default()
            });
            cx.run_until_parked();
        }

        /// Delivers next-frame callbacks (the follow-state probe), then lets the window draw.
        fn next_frame(&self, cx: &mut VisualTestContext) {
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
        }
    }

    fn reply_event(n: u64, parent: &str, root: &str) -> ChatEvent {
        let mut message = Message::chat(
            format!("m{n}"),
            Timestamp(0),
            Author::new("bo", "Bo"),
            vec![Fragment::Text("in reply".into())],
        );
        message.reply = Some(gpui_chat_core::ReplyRef {
            parent_id: parent.into(),
            parent_login: "al".into(),
            parent_display_name: "Al".into(),
            parent_body: "what was said".into(),
            thread_parent_id: Some(root.into()),
        });
        ChatEvent::Message {
            channel: ChannelId::new("test"),
            message: Arc::new(message),
        }
    }

    #[gpui_kit::test]
    fn a_reply_row_is_one_context_line_taller_and_the_cache_stays_correct(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(1, "hello", cx);
        h.store.update(cx, |store, cx| {
            store.apply_all(vec![reply_event(1, "m0", "m0")]);
            cx.notify();
        });
        cx.run_until_parked();
        let (plain, reply) = h.view.read_with(cx, |view, _| {
            let cache = view.cache.borrow();
            (cache.flow_of(0).unwrap(), cache.flow_of(1).unwrap())
        });
        // Both are one line of text; the reply's extra line is not part of the flow but of the
        // layout's height, which the row measures with.
        assert_eq!((plain.line_count(), reply.line_count()), (1, 1));
        let (plain_height, reply_height) = h.view.read_with(cx, |view, _| {
            let cache = view.cache.borrow();
            let height = |seq| cache.layout_height(seq).expect("the message was laid out");
            (height(0), height(1))
        });
        assert!(
            reply_height > plain_height,
            "{reply_height} vs {plain_height}"
        );
        // A resize of the window re-flows but leaves the reply line in place.
        cx.simulate_resize(size(px(500.0), px(600.0)));
        cx.run_until_parked();
        let again = h
            .view
            .read_with(cx, |view, _| view.cache.borrow().layout_height(1).unwrap());
        assert_eq!(again, reply_height);
    }

    #[gpui_kit::test]
    fn the_thread_popover_opens_follows_the_store_and_closes(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(1, "root message", cx);
        assert_eq!(
            h.view.read_with(cx, |v, _| v.open_thread_root().cloned()),
            None
        );
        h.view.update_in(cx, |view, window, cx| {
            view.open_thread("m0".into(), window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.open_thread_root().map(|id| id.to_string())),
            Some("m0".to_owned())
        );
        // A reply that arrives while it is open is shaped into the popover's own cache.
        h.store.update(cx, |store, cx| {
            store.apply_all(vec![reply_event(1, "m0", "m0")]);
            cx.notify();
        });
        cx.run_until_parked();
        let in_popover = h.view.read_with(cx, |v, _| {
            let panel = v.thread.as_ref().unwrap();
            let cache = panel.cache.borrow();
            (cache.layout_height(0).is_some(), cache.layout_height(1))
        });
        assert!(in_popover.0, "the root is drawn in the popover");
        let reply_in_popover = in_popover.1.expect("the reply is drawn in the popover");
        // No context line inside the popover: the thread is the context.
        let in_list = h
            .view
            .read_with(cx, |v, _| v.cache.borrow().layout_height(1).unwrap());
        assert!(
            reply_in_popover < in_list,
            "{reply_in_popover} vs {in_list}"
        );

        h.view.update(cx, |view, cx| view.close_thread(cx));
        cx.run_until_parked();
        assert!(h.view.read_with(cx, |v, _| v.open_thread_root().is_none()));
    }

    #[gpui_kit::test]
    fn the_row_under_the_pointer_is_tracked_only_when_replying_is_possible(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(3, "hello", cx);
        // The newest row is at the bottom of the (800x600) window.
        let over_newest = point(px(400.0), px(595.0));
        cx.simulate_mouse_move(over_newest, None, Default::default());
        cx.run_until_parked();
        assert_eq!(
            h.view.read_with(cx, |v, _| v.hovered.get()),
            None,
            "nothing tracks the pointer in a chat that cannot reply"
        );

        h.view.update(cx, |view, cx| view.set_can_reply(true, cx));
        cx.run_until_parked();
        cx.simulate_mouse_move(over_newest, None, Default::default());
        cx.run_until_parked();
        assert_eq!(h.view.read_with(cx, |v, _| v.hovered.get()), Some(2));

        // Pointing at an older row moves it; leaving the rows clears it.
        cx.simulate_mouse_move(point(px(400.0), px(570.0)), None, Default::default());
        cx.run_until_parked();
        let hovered = h.view.read_with(cx, |v, _| v.hovered.get());
        assert!(matches!(hovered, Some(seq) if seq < 2), "{hovered:?}");
    }

    #[gpui_kit::test]
    fn choosing_reply_tells_the_owner_which_message(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(1, "hello", cx);
        let heard = Rc::new(RefCell::new(Vec::new()));
        let sink = heard.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&h.view, move |_, event: &ChatViewEvent, _| {
                let ChatViewEvent::ReplyRequested(message) = event else {
                    return;
                };
                sink.borrow_mut().push(message.id.to_string());
            })
        });
        let message = h.store.read_with(cx, |store, _| {
            store.messages().get_by_seq(0).unwrap().clone()
        });
        h.view
            .update(cx, |view, cx| view.request_reply(message, cx));
        assert_eq!(*heard.borrow(), ["m0"]);
    }

    #[gpui_kit::test]
    fn a_moderator_gets_hover_tracking_and_others_do_not(cx: &mut TestAppContext) {
        use gpui_chat_core::ChannelRole;
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(3, "hello", cx);
        let over_newest = point(px(400.0), px(595.0));
        let hovered =
            |h: &Harness, cx: &mut VisualTestContext| h.view.read_with(cx, |v, _| v.hovered.get());
        let become_ = |h: &Harness, role, cx: &mut VisualTestContext| {
            h.store.update(cx, |store, cx| {
                store.apply(ChatEvent::UserState {
                    channel: ChannelId::new("test"),
                    role,
                });
                cx.notify();
            });
            cx.run_until_parked();
        };
        // A moderator by USERSTATE, but nothing to run commands with: no controls, no tracking.
        become_(&h, ChannelRole::Moderator, cx);
        cx.simulate_mouse_move(over_newest, None, Default::default());
        cx.run_until_parked();
        assert_eq!(hovered(&h, cx), None);
        // With a pane that can run them, the moderator's rows track the pointer (for the buttons).
        h.view.update(cx, |view, cx| view.set_moderation(true, cx));
        cx.run_until_parked();
        cx.simulate_mouse_move(over_newest, None, Default::default());
        cx.run_until_parked();
        assert_eq!(hovered(&h, cx), Some(2));
    }

    #[gpui_kit::test]
    fn the_list_stands_still_while_the_pointer_is_on_a_moderation_strip(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(100, "hello", cx);
        assert!(h.following(cx));
        // The pointer is over a row (a moderator's rows track it), as it is when it is on the strip.
        h.view.update(cx, |view, cx| view.set_can_reply(true, cx));
        cx.run_until_parked();
        cx.simulate_mouse_move(point(px(400.0), px(595.0)), None, Default::default());
        cx.run_until_parked();
        h.view.update(cx, |view, cx| view.hold_for_strip(true, cx));
        assert!(!h.following(cx), "holding stops following");
        h.push(5, "more", cx);
        assert!(!h.following(cx), "new messages do not move the list");
        assert!(
            h.view.read_with(cx, |v, _| v.unseen()) >= 5,
            "they wait behind the pill"
        );
        // Entering again changes nothing; leaving resumes following the newest.
        h.view.update(cx, |view, cx| view.hold_for_strip(true, cx));
        h.view.update(cx, |view, cx| view.hold_for_strip(false, cx));
        cx.run_until_parked();
        assert!(h.following(cx));
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 0);
        // A list that was not following (the user scrolled up) is left where it is.
        h.scroll(200.0, cx);
        assert!(!h.following(cx));
        h.view.update(cx, |view, cx| view.hold_for_strip(true, cx));
        h.view.update(cx, |view, cx| view.hold_for_strip(false, cx));
        assert!(!h.following(cx));
    }

    #[gpui_kit::test]
    fn choosing_a_moderation_action_tells_the_owner_which_message_and_action(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(1, "hello", cx);
        let heard = Rc::new(RefCell::new(Vec::new()));
        let sink = heard.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&h.view, move |_, event: &ChatViewEvent, _| {
                if let ChatViewEvent::Moderate(message, action) = event {
                    sink.borrow_mut().push((message.id.to_string(), *action));
                }
            })
        });
        let message = h.store.read_with(cx, |store, _| {
            store.messages().get_by_seq(0).unwrap().clone()
        });
        h.view.update(cx, |view, cx| {
            view.request_moderation(message.clone(), RowAction::Timeout(600), cx);
            view.request_moderation(message, RowAction::Ban, cx);
        });
        assert_eq!(
            *heard.borrow(),
            [
                ("m0".to_owned(), RowAction::Timeout(600)),
                ("m0".to_owned(), RowAction::Ban)
            ]
        );
    }

    #[gpui_kit::test]
    fn a_collapsed_message_is_one_line_until_revealed(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        // Messages long enough to wrap in the 800 px window, and enough of them to scroll.
        let long = "word ".repeat(80);
        h.push(30, &long, cx);
        let (lines, _) = h.last_message_lines(cx);
        assert!(lines > 1, "the messages wrap: {lines} lines");
        // How far the content reaches past the window: the sum of the row heights, less 600 px.
        let overflow = |h: &Harness, cx: &mut VisualTestContext| {
            h.view
                .read_with(cx, |view, _| view.list_state().max_offset_for_scrollbar().y)
        };
        let intact = overflow(&h, cx);
        assert!(intact > px(200.0), "{intact:?}");

        // Struck through, the messages keep their height.
        h.store.update(cx, |store, cx| {
            store.apply(ChatEvent::ClearChat {
                channel: ChannelId::new("test"),
                target: gpui_chat_core::ClearTarget::Channel,
                at: None,
            });
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(h.store.read_with(cx, |s, _| s.struck_count()), 30);
        assert_eq!(overflow(&h, cx), intact);

        // Collapsing is a setting; every removed message is laid out again as one line.
        let mut settings = Settings::default();
        settings.chat.deleted_messages = DeletedMessages::Collapse;
        h.settings.update(cx, |store, cx| {
            store.apply_events([SettingsEvent::Reloaded(settings)]);
            cx.notify();
        });
        cx.run_until_parked();
        let collapsed = overflow(&h, cx);
        assert!(collapsed < intact / 2.0, "{collapsed:?} < {intact:?}");

        let shown = |h: &Harness, cx: &mut VisualTestContext| {
            h.view.read_with(cx, |view, _| {
                view.cache
                    .borrow()
                    .shaped(29)
                    .expect("laid out")
                    .display
                    .text
                    .clone()
            })
        };
        assert!(
            shown(&h, cx).ends_with("Al: <message deleted>"),
            "{}",
            shown(&h, cx)
        );

        // Revealing one brings its text back, and only that one's.
        let id = MessageId::from("m29");
        h.view.update(cx, |view, cx| view.reveal(&id, cx));
        cx.run_until_parked();
        let revealed = overflow(&h, cx);
        assert!(revealed > collapsed, "{revealed:?}");
        assert!(shown(&h, cx).contains("Al: word word"), "{}", shown(&h, cx));
        assert!(
            h.view
                .read_with(cx, |view, _| view.revealed.borrow().contains(&id))
        );
    }

    /// Presses (with `clicks`), optionally drags to `to`, and releases there.
    fn press(from: Point<Pixels>, to: Point<Pixels>, clicks: usize, cx: &mut VisualTestContext) {
        cx.simulate_event(MouseDownEvent {
            position: from,
            button: MouseButton::Left,
            modifiers: Modifiers::default(),
            click_count: clicks,
            first_mouse: false,
        });
        if to != from {
            cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
        }
        cx.simulate_event(MouseUpEvent {
            position: to,
            button: MouseButton::Left,
            modifiers: Modifiers::default(),
            click_count: clicks,
        });
        cx.run_until_parked();
    }

    fn point_of(h: &Harness, seq: u64, offset: usize, cx: &mut VisualTestContext) -> Point<Pixels> {
        h.view
            .read_with(cx, |v, _| v.surface.point_of(seq, offset))
            .expect("the row was drawn")
    }

    fn message(h: &Harness, seq: u64, cx: &mut VisualTestContext) -> Arc<Message> {
        h.store
            .read_with(cx, |s, _| s.messages().get_by_seq(seq).unwrap().clone())
    }

    fn selected(h: &Harness, cx: &mut VisualTestContext) -> Option<String> {
        h.view.read_with(cx, |v, cx| v.selected_text(cx))
    }

    #[gpui_kit::test]
    fn dragging_selects_across_messages_and_copies_one_line_each(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(4, "hello world", cx);
        // From the far left of message 1 to past the end of message 3: three whole messages.
        let from = point(px(1.0), point_of(&h, 1, 0, cx).y);
        let to = point(px(799.0), point_of(&h, 3, 0, cx).y);
        press(from, to, 1, cx);
        let lines: Vec<String> = (1..=3)
            .map(|seq| selection::message_line(&message(&h, seq, cx)))
            .collect();
        assert_eq!(selected(&h, cx), Some(lines.join("\n")));

        // From the middle of "world" in message 2 up into message 1: the end of 1, the start of 2.
        let text = h.view.read_with(cx, |v, _| {
            v.cache.borrow().shaped(2).unwrap().display.text.clone()
        });
        let world = text.find("world").unwrap();
        press(
            point_of(&h, 2, world + 2, cx),
            point_of(&h, 1, world, cx),
            1,
            cx,
        );
        assert_eq!(
            selected(&h, cx),
            Some("world\n".to_owned() + &text[..world + 2])
        );

        // ⌘C's action puts it on the clipboard.
        assert!(h.view.update(cx, |v, cx| v.copy_selection(cx)));
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("world\n".to_owned() + &text[..world + 2])
        );

        // A click without a drag clears it, and there is nothing to copy.
        let somewhere = point_of(&h, 0, 2, cx);
        press(somewhere, somewhere, 1, cx);
        assert_eq!(selected(&h, cx), None);
        assert!(!h.view.update(cx, |v, cx| v.copy_selection(cx)));
    }

    #[gpui_kit::test]
    fn double_click_selects_a_word_and_triple_click_the_message(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(2, "hello world", cx);
        let text = h.view.read_with(cx, |v, _| {
            v.cache.borrow().shaped(1).unwrap().display.text.clone()
        });
        let at = point_of(&h, 1, text.find("world").unwrap() + 1, cx);
        press(at, at, 2, cx);
        assert_eq!(selected(&h, cx).as_deref(), Some("world"));
        press(at, at, 3, cx);
        assert_eq!(
            selected(&h, cx),
            Some(selection::message_line(&message(&h, 1, cx)))
        );
        // Esc's action drops it.
        assert!(h.view.update(cx, |v, cx| v.clear_selection(cx)));
        assert_eq!(selected(&h, cx), None);
        assert!(!h.view.update(cx, |v, cx| v.clear_selection(cx)));
    }

    #[gpui_kit::test]
    fn clicking_a_link_opens_it_and_dragging_from_one_does_not(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        let fragments = gpui_chat_core::Tokenizer::new()
            .tokenize("see https://example.com/x now", &[])
            .fragments;
        h.store.update(cx, |store, cx| {
            store.apply_all([ChatEvent::Message {
                channel: ChannelId::new("test"),
                message: Arc::new(Message::chat(
                    "link",
                    Timestamp(0),
                    Author::new("al", "Al"),
                    fragments,
                )),
            }]);
            cx.notify();
        });
        h.next += 1;
        cx.run_until_parked();
        let text = h.view.read_with(cx, |v, _| {
            v.cache.borrow().shaped(0).unwrap().display.text.clone()
        });
        let link = text.find("https").unwrap();
        let start = point_of(&h, 0, link + 3, cx);
        // A drag that starts on the link selects instead.
        press(start, point_of(&h, 0, link + 10, cx), 1, cx);
        assert_eq!(cx.opened_url(), None);
        assert!(selected(&h, cx).is_some());
        // A click opens it once the double-click interval passed without another press.
        press(start, start, 1, cx);
        assert_eq!(
            cx.opened_url(),
            None,
            "not before the double-click interval"
        );
        settle(cx);
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com/x"));
        // A double-click selects the link instead of opening it.
        press(start, start, 1, cx);
        press(start, start, 2, cx);
        settle(cx);
        assert_eq!(selected(&h, cx).as_deref(), Some("https://example.com/x"));
        // Plain text opens nothing.
        h.push(1, "no links here", cx);
        let plain = point_of(&h, 1, 4, cx);
        press(plain, plain, 1, cx);
        settle(cx);
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com/x"));
    }

    /// Lets the double-click interval pass (pending clicks settle).
    fn settle(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(DOUBLE_CLICK);
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn a_drag_keeps_the_list_still_while_messages_arrive(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(100, "hello", cx);
        assert!(h.following(cx));
        let from = point_of(&h, 90, 1, cx);
        cx.simulate_event(MouseDownEvent {
            position: from,
            button: MouseButton::Left,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        let to = point_of(&h, 95, 1, cx);
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
        cx.run_until_parked();
        h.push(5, "hello", cx);
        assert!(
            !h.following(cx),
            "the list stopped following during the drag"
        );
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.surface.hit(from).map(|hit| hit.seq)),
            Some(90),
            "the message under the pointer did not move"
        );
        cx.simulate_mouse_up(from, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        assert!(selected(&h, cx).is_some());
        // It stays still while the selection is there, even when nothing arrived for a while (the
        // list would otherwise find itself at its end and follow again).
        h.next_frame(cx);
        h.push(5, "hello", cx);
        assert!(!h.following(cx));
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.surface.hit(from).map(|hit| hit.seq)),
            Some(90)
        );
        // A click in the chat drops the selection but keeps the list where it is for the
        // double-click interval (a second click must find the same text), then follows again.
        press(from, from, 1, cx);
        assert_eq!(selected(&h, cx), None);
        h.push(2, "hello", cx);
        assert!(!h.following(cx));
        settle(cx);
        h.push(1, "hello", cx);
        assert!(h.following(cx));
        // Esc (or a press outside the chat) on a selection follows the chat again at once.
        let at = point_of(&h, 100, 1, cx);
        press(at, at, 3, cx);
        h.push(2, "hello", cx);
        assert!(!h.following(cx));
        assert!(h.view.update(cx, |v, cx| v.clear_selection(cx)));
        h.push(1, "hello", cx);
        assert!(h.following(cx));
    }

    #[gpui_kit::test]
    fn a_double_click_in_a_moving_chat_selects_a_word_of_the_message_first_clicked(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(50, "hello world", cx);
        let text = h.view.read_with(cx, |v, _| {
            v.cache.borrow().shaped(45).unwrap().display.text.clone()
        });
        let at = point_of(&h, 45, text.find("world").unwrap() + 1, cx);
        // First click; messages arrive before the second (a busy chat).
        press(at, at, 1, cx);
        h.push(3, "other text", cx);
        press(at, at, 2, cx);
        assert_eq!(selected(&h, cx).as_deref(), Some("world"));
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.surface.hit(at).map(|hit| hit.seq)),
            Some(45),
            "the list did not move between the clicks"
        );
        // A press whose release was lost (the pointer moves with no button) does not hold the
        // list forever.
        assert!(h.view.update(cx, |v, cx| v.clear_selection(cx)));
        h.push(1, "x", cx);
        assert!(h.following(cx));
        cx.simulate_event(MouseDownEvent {
            position: at,
            button: MouseButton::Left,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        h.push(2, "x", cx);
        assert!(!h.following(cx), "held while pressed");
        cx.simulate_mouse_move(at, None, Modifiers::default());
        cx.run_until_parked();
        h.push(1, "x", cx);
        assert!(h.following(cx));
    }

    #[gpui_kit::test]
    fn copying_uses_the_end_messages_as_they_were_selected(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(3, "héllo wörld", cx);
        let text = h.view.read_with(cx, |v, _| {
            v.cache.borrow().shaped(2).unwrap().display.text.clone()
        });
        let o = text.find('ö').unwrap();
        press(point_of(&h, 1, o, cx), point_of(&h, 2, o, cx), 1, cx);
        let before = selected(&h, cx).expect("a selection");
        // Every layout dropped (as an emote image arriving does to the rows that draw it): the
        // copy still uses the texts the offsets were measured on.
        h.view
            .read_with(cx, |v, _| v.cache.borrow_mut().retain_key(u64::MAX));
        assert_eq!(selected(&h, cx), Some(before));
    }

    #[gpui_kit::test]
    fn a_settings_change_that_keeps_the_text_keeps_the_selection(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(2, "hello world", cx);
        let at = point_of(&h, 1, 1, cx);
        press(at, at, 3, cx);
        let mut settings = Settings::default();
        settings.appearance.line_height = 2.0;
        settings.emotes.scale = 2.0;
        h.settings.update(cx, |store, cx| {
            store.apply_events([SettingsEvent::Reloaded(settings)]);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            h.view.read_with(cx, |v, _| v.generation),
            1,
            "laid out again"
        );
        assert!(
            selected(&h, cx).is_some(),
            "the same text: the selection stays"
        );
    }

    #[gpui_kit::test]
    fn copying_in_the_thread_popover_takes_only_its_messages(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(1, "root message", cx);
        h.push(1, "not in the thread", cx);
        h.store.update(cx, |store, cx| {
            store.apply_all(vec![reply_event(2, "m0", "m0")]);
            cx.notify();
        });
        h.next += 1;
        cx.run_until_parked();
        h.view.update_in(cx, |view, window, cx| {
            view.open_thread("m0".into(), window, cx)
        });
        cx.run_until_parked();
        let in_popover = |seq, cx: &mut VisualTestContext| {
            h.view
                .read_with(cx, |v, _| {
                    v.thread.as_ref().unwrap().surface.point_of(seq, 0)
                })
                .expect("drawn in the popover")
        };
        let (from, to) = (in_popover(0, cx), in_popover(2, cx));
        press(
            point(px(from.x.as_f32() - 6.0), from.y),
            point(px(to.x.as_f32() + 300.0), to.y),
            1,
            cx,
        );
        let copied = selected(&h, cx).expect("a selection in the popover");
        let lines: Vec<&str> = copied.lines().collect();
        assert_eq!(lines.len(), 2, "{copied}");
        assert!(lines[0].ends_with("root message"), "{copied}");
        assert!(lines[1].ends_with("in reply"), "{copied}");
        assert!(!copied.contains("not in the thread"));
    }

    #[gpui_kit::test]
    fn a_selection_made_while_scrolled_up_does_not_jump_to_the_end(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(200, "hello", cx);
        h.scroll(400.0, cx);
        assert!(!h.following(cx));
        let top = h
            .view
            .read_with(cx, |v, _| v.list_state().logical_scroll_top());
        let at = point(px(400.0), px(300.0));
        press(at, at, 3, cx);
        assert!(selected(&h, cx).is_some());
        assert!(h.view.update(cx, |v, cx| v.clear_selection(cx)));
        h.push(3, "hello", cx);
        assert!(!h.following(cx), "it was not following before");
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.list_state().logical_scroll_top()),
            top
        );
    }

    #[gpui_kit::test]
    fn a_right_click_aims_the_menu_at_the_message_and_link_under_it(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(1, "plain", cx);
        let fragments = gpui_chat_core::Tokenizer::new()
            .tokenize("go to https://example.com/y", &[])
            .fragments;
        h.store.update(cx, |store, cx| {
            store.apply_all([ChatEvent::Message {
                channel: ChannelId::new("test"),
                message: Arc::new(Message::chat(
                    "l",
                    Timestamp(0),
                    Author::new("al", "Al"),
                    fragments,
                )),
            }]);
            cx.notify();
        });
        h.next += 1;
        cx.run_until_parked();
        let right_click = |at: Point<Pixels>, cx: &mut VisualTestContext| {
            cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::default());
            cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::default());
            cx.run_until_parked();
        };
        let target = |h: &Harness, cx: &mut VisualTestContext| {
            h.view.read_with(cx, |v, _| {
                v.surface
                    .last_aimed()
                    .map(|t| (t.seq, t.link.map(|url| url.to_string())))
            })
        };
        right_click(point_of(&h, 0, 2, cx), cx);
        assert_eq!(target(&h, cx), Some((0, None)));
        let text = h.view.read_with(cx, |v, _| {
            v.cache.borrow().shaped(1).unwrap().display.text.clone()
        });
        right_click(point_of(&h, 1, text.find("example").unwrap(), cx), cx);
        assert_eq!(
            target(&h, cx),
            Some((1, Some("https://example.com/y".to_owned())))
        );
        // The empty space above the two messages is neither's.
        right_click(point(px(400.0), px(20.0)), cx);
        assert_eq!(target(&h, cx), None);
        // The menu takes the target as it opens: nothing stale is left for another menu.
        right_click(point_of(&h, 0, 2, cx), cx);
        assert_eq!(target(&h, cx).map(|(seq, _)| seq), Some(0));
        assert!(
            h.view
                .read_with(cx, |v, _| v.surface.menu_target())
                .is_none()
        );
    }

    #[gpui_kit::test]
    fn copy_and_esc_keys_act_on_a_selection_and_otherwise_go_on(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(2, "hello world", cx);
        let copy_key = if cfg!(target_os = "macos") {
            "cmd-c"
        } else {
            "ctrl-c"
        };
        let (copies, escapes) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let dismissed = Rc::new(Cell::new(0));
        cx.update(|window, cx| {
            cx.bind_keys([
                gpui_kit::KeyBinding::new(copy_key, CopySelection, Some(CHAT_CONTEXT)),
                gpui_kit::KeyBinding::new("escape", ClearSelection, Some(CHAT_CONTEXT)),
            ]);
            // What runs when the chat lets a key go on.
            let (c, e) = (copies.clone(), escapes.clone());
            cx.on_action(move |_: &CopySelection, _| c.set(c.get() + 1));
            cx.on_action(move |_: &ClearSelection, _| e.set(e.get() + 1));
            let d = dismissed.clone();
            cx.subscribe(&h.view, move |_, event: &ChatViewEvent, _| {
                if matches!(event, ChatViewEvent::SelectionDismissed) {
                    d.set(d.get() + 1);
                }
            })
            .detach();
            let focus = h.view.read(cx).focus_handle.clone();
            window.focus(&focus, cx);
        });
        // Nothing selected: both keys go on.
        cx.simulate_keystrokes(copy_key);
        cx.simulate_keystrokes("escape");
        assert_eq!((copies.get(), escapes.get()), (1, 1));
        // A selection: ⌘C copies it, Esc clears it and asks for the input box; neither goes on.
        let at = point_of(&h, 1, 1, cx);
        press(at, at, 3, cx);
        cx.update(|window, cx| {
            let focus = h.view.read(cx).focus_handle.clone();
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("before".into()));
        cx.simulate_keystrokes(copy_key);
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(selection::message_line(&message(&h, 1, cx)))
        );
        cx.simulate_keystrokes("escape");
        assert_eq!(selected(&h, cx), None);
        assert_eq!((copies.get(), escapes.get(), dismissed.get()), (1, 1, 1));
    }

    #[gpui_kit::test]
    fn a_layout_change_drops_the_selection(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(2, "hello world", cx);
        let at = point_of(&h, 1, 1, cx);
        press(at, at, 3, cx);
        assert!(selected(&h, cx).is_some());
        let mut settings = Settings::default();
        settings.chat.timestamps = !settings.chat.timestamps;
        h.settings.update(cx, |store, cx| {
            store.apply_events([SettingsEvent::Reloaded(settings)]);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(selected(&h, cx), None, "its offsets were into the old text");
    }

    #[gpui_kit::test]
    fn list_tracks_the_store_incrementally_and_respects_the_cap(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        assert_eq!(h.items(cx), 0);
        h.push(30, "hello", cx);
        assert_eq!(h.items(cx), 30);
        assert!(h.following(cx));
        // 30 + 120 with a cap of 100: the 50 oldest are spliced out at the front.
        h.push(120, "hello", cx);
        assert_eq!(h.items(cx), 100);
        assert_eq!(h.view.read_with(cx, |v, _| v.rows.borrow().first_seq()), 50);
        assert!(h.following(cx), "still at the bottom");
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 0);
        // Only rows that were drawn were shaped, and evicted ones were dropped from the cache.
        let cached = h.view.read_with(cx, |v, _| v.cache.borrow().len());
        assert!(cached > 0 && cached < 100, "{cached} layouts cached");
    }

    #[gpui_kit::test]
    fn scrolling_up_pauses_counts_new_messages_and_the_pill_jumps_back(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(200, "hello", cx);
        assert!(!h.view.read_with(cx, |v, _| v.pill_shown()));

        h.scroll(400.0, cx);
        assert!(!h.following(cx), "scrolling up pauses following");
        h.push(7, "hello", cx);
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 7);
        assert!(h.view.read_with(cx, |v, _| v.pill_shown()));
        let top = h
            .view
            .read_with(cx, |v, _| v.list_state().logical_scroll_top());
        assert!(top.item_ix < 200, "the view stays where the user scrolled");

        h.view.update(cx, |view, cx| view.jump_to_latest(cx));
        cx.run_until_parked();
        assert!(h.following(cx));
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 0);
        assert!(!h.view.read_with(cx, |v, _| v.pill_shown()));
    }

    #[gpui_kit::test]
    fn jumping_to_a_message_shows_it_a_few_rows_from_the_top_and_stops_following(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(200, "hello", cx);
        assert!(h.following(cx));
        let id = |n: u64| MessageId::from(format!("m{n}"));
        let found = h
            .view
            .update(cx, |view, cx| view.scroll_to_message(&id(50), cx));
        cx.run_until_parked();
        assert!(found);
        assert!(
            !h.following(cx),
            "a jump away from the bottom pauses following"
        );
        let top = h
            .view
            .read_with(cx, |v, _| v.list_state().logical_scroll_top());
        assert_eq!(top.item_ix, 50 - JUMP_CONTEXT_ROWS);
        // A message the store no longer holds is not found, and nothing moves.
        let missing = h.view.update(cx, |view, cx| {
            view.scroll_to_message(&MessageId::from("gone"), cx)
        });
        assert!(!missing);
        let again = h
            .view
            .read_with(cx, |v, _| v.list_state().logical_scroll_top());
        assert_eq!(again.item_ix, top.item_ix);
        // The first rows clamp at the top.
        assert!(
            h.view
                .update(cx, |view, cx| view.scroll_to_message(&id(1), cx))
        );
        let top = h
            .view
            .read_with(cx, |v, _| v.list_state().logical_scroll_top());
        assert_eq!(top.item_ix, 0);
    }

    #[gpui_kit::test]
    fn a_jump_to_a_recent_message_stays_put_while_the_channel_keeps_talking(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(200, "hello", cx);
        let last = MessageId::from("m199");
        assert!(
            h.view
                .update(cx, |view, cx| view.scroll_to_message(&last, cx))
        );
        cx.run_until_parked();
        h.next_frame(cx);
        // The newest rows fit on screen, so the list is at its end: without the hold it would be
        // following again and the next messages would carry the target away.
        assert!(!h.following(cx), "the jump holds the list");
        h.push(30, "more", cx);
        h.next_frame(cx);
        assert!(!h.following(cx), "still held while messages arrive");
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 30);
        assert_eq!(h.view.read_with(cx, |v, _| v.flash.get()), Some(199));
        // The target is still on screen: it is not past the top.
        let top = h
            .view
            .read_with(cx, |v, _| v.list_state().logical_scroll_top());
        assert!(top.item_ix <= 199);
        // Going back to the latest messages (the pill) lets go of the hold.
        h.view.update(cx, |view, cx| view.jump_to_latest(cx));
        cx.run_until_parked();
        h.next_frame(cx);
        assert!(h.following(cx));
        assert!(!h.view.read_with(cx, |v, _| v.jump_hold));
        // So does scrolling.
        assert!(
            h.view
                .update(cx, |view, cx| view.scroll_to_message(&last, cx))
        );
        cx.run_until_parked();
        assert!(h.view.read_with(cx, |v, _| v.jump_hold));
        h.scroll(200.0, cx);
        assert!(
            !h.view.read_with(cx, |v, _| v.jump_hold),
            "the user took over"
        );
    }

    fn history_before(count: i64, cx: &mut VisualTestContext, h: &Harness) {
        // Older than every live message (`chat` stamps them 0).
        let messages = (0..count)
            .map(|n| {
                let mut message = Message::chat(
                    format!("h{n}"),
                    Timestamp(n - count - 10),
                    Author::new("old", "Old"),
                    vec![Fragment::Text("earlier".into())],
                );
                message.flags.history = true;
                Arc::new(message)
            })
            .collect();
        h.store.update(cx, |store, cx| {
            store.apply(ChatEvent::History {
                channel: ChannelId::new("test"),
                messages,
                moderation: Vec::new(),
            });
            cx.notify();
        });
        cx.run_until_parked();
    }

    /// The id of the message at the top of the list.
    fn top_id(h: &Harness, cx: &mut VisualTestContext) -> (usize, String) {
        let top = h
            .view
            .read_with(cx, |v, _| v.list_state().logical_scroll_top());
        let id = h.view.read_with(cx, |v, cx| {
            let seq = v.rows.borrow().first_seq() + top.item_ix as u64;
            let id = v
                .store
                .read(cx)
                .messages()
                .get_by_seq(seq)
                .map(|m| m.id.to_string());
            id.expect("the top row is a message")
        });
        (top.item_ix, id)
    }

    #[gpui_kit::test]
    fn history_inserted_above_a_scrolled_up_reader_does_not_move_them(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(200, "hello", cx);
        h.scroll(400.0, cx);
        assert!(!h.following(cx));
        h.push(7, "hello", cx);
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 7);
        let (top_ix, top_message) = top_id(&h, cx);
        let cached = h.view.read_with(cx, |v, _| v.cache.borrow().len());
        assert!(cached > 0);

        history_before(25, cx, &h);

        assert_eq!(h.items(cx), 232, "25 rows inserted, none replaced");
        assert!(!h.following(cx), "still paused");
        let (new_ix, still) = top_id(&h, cx);
        assert_eq!(still, top_message, "the reader still sees the same message");
        assert_eq!(
            new_ix,
            top_ix + 25,
            "…which moved down by what was inserted above it"
        );
        assert_eq!(
            h.view.read_with(cx, |v, _| v.unseen()),
            7,
            "the pill still counts only the 7"
        );
        assert!(h.view.read_with(cx, |v, _| v.pill_shown()));
        // Layouts moved with their messages instead of being thrown away.
        let after = h.view.read_with(cx, |v, _| v.cache.borrow().len());
        assert!(after >= cached, "{after} layouts vs {cached} before");
        let seq_of_top = h.view.read_with(cx, |v, cx| {
            v.store.read(cx).messages().seq_of(&top_message).unwrap()
        });
        assert!(
            h.view
                .read_with(cx, |v, _| v.cache.borrow().flow_of(seq_of_top).is_some())
        );
    }

    #[gpui_kit::test]
    fn history_on_join_while_following_keeps_following(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(5, "hello", cx);
        assert!(h.following(cx));
        history_before(40, cx, &h);
        assert_eq!(h.items(cx), 45);
        assert!(h.following(cx), "history above does not unfollow");
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 0);
        h.push(3, "live", cx);
        assert_eq!(h.items(cx), 48);
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 0);
    }

    #[gpui_kit::test]
    fn history_that_overflows_the_cap_drops_the_oldest_rows_in_step(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 50);
        h.push(40, "hello", cx);
        history_before(30, cx, &h);
        // 70 merged, 50 kept: the 20 oldest history rows fall off.
        assert_eq!(h.items(cx), 50);
        let (items, store_len) = h.view.read_with(cx, |v, cx| {
            (
                v.list_state().item_count(),
                v.store.read(cx).messages().len(),
            )
        });
        assert_eq!(items, store_len);
        let oldest = h.view.read_with(cx, |v, cx| {
            v.store
                .read(cx)
                .messages()
                .get_index(0)
                .map(|m| m.id.to_string())
        });
        assert_eq!(oldest.as_deref(), Some("h20"));
        assert!(h.following(cx));
    }

    #[gpui_kit::test]
    fn a_selection_follows_its_messages_when_history_is_inserted_before_them(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(4, "hello world", cx);
        // From inside message 1 to inside message 2: a partial first line, a partial second.
        press(point_of(&h, 1, 3, cx), point_of(&h, 2, 8, cx), 1, cx);
        let before = selected(&h, cx).expect("a selection");
        assert!(before.contains('\n'), "{before:?}");
        let ids = ("m1", "m2");
        let seq_of = |h: &Harness, id: &str, cx: &mut VisualTestContext| {
            h.store
                .read_with(cx, |s, _| s.messages().seq_of(id).expect("the message"))
        };
        let old = (seq_of(&h, ids.0, cx), seq_of(&h, ids.1, cx));

        history_before(6, cx, &h);

        let new = (seq_of(&h, ids.0, cx), seq_of(&h, ids.1, cx));
        assert_ne!(new, old, "the messages were renumbered");
        assert_eq!(
            h.view.read_with(cx, |v, _| v.surface.selection_seqs()),
            Some(new.0..=new.1),
            "the selection moved with them"
        );
        assert_eq!(
            selected(&h, cx),
            Some(before.clone()),
            "…and still copies the same text"
        );
        // ⌘C's action.
        assert!(h.view.update(cx, |v, cx| v.copy_selection(cx)));
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(before)
        );
    }

    #[gpui_kit::test]
    fn a_selection_is_dropped_when_the_list_replaces_every_row(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(4, "hello world", cx);
        press(point_of(&h, 1, 3, cx), point_of(&h, 2, 8, cx), 1, cx);
        assert!(selected(&h, cx).is_some());
        // More renumbering merges in one batch than the store remembers: the view cannot replay
        // them, so it replaces every row and nothing keyed by an old number can be trusted.
        h.store.update(cx, |store, cx| {
            for n in 0..10i64 {
                let mut message = Message::chat(
                    format!("x{n}"),
                    Timestamp(-100 - n),
                    Author::new("old", "Old"),
                    vec![Fragment::Text("earlier".into())],
                );
                message.flags.history = true;
                store.apply(ChatEvent::History {
                    channel: ChannelId::new("test"),
                    messages: vec![Arc::new(message)],
                    moderation: Vec::new(),
                });
            }
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(selected(&h, cx), None);
        assert_eq!(
            h.view.read_with(cx, |v, _| v.surface.selection_seqs()),
            None
        );
        assert_eq!(h.items(cx), 14, "every row is there, replaced");
    }

    #[gpui_kit::test]
    fn the_pill_never_counts_more_than_the_store_still_holds(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(100, "hello", cx);
        h.scroll(300.0, cx);
        assert!(!h.following(cx));
        h.push(30, "hello", cx);
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 30);
        // 500 more with a cap of 100: only the newest 100 exist, and all of them are new.
        h.push(500, "hello", cx);
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 100);
        assert_eq!(h.items(cx), 100);
    }

    #[gpui_kit::test]
    fn scrolling_back_to_the_bottom_resumes_following(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(200, "hello", cx);
        h.scroll(300.0, cx);
        assert!(!h.following(cx));
        assert!(h.view.read_with(cx, |v, _| v.pill_shown()));
        h.scroll(-10_000.0, cx);
        // GPUI re-engages following in list layout; the probe re-renders the view once more.
        h.next_frame(cx);
        assert!(h.following(cx));
        assert!(!h.view.read_with(cx, |v, _| v.pill_shown()));
        h.push(3, "hello", cx);
        assert_eq!(h.view.read_with(cx, |v, _| v.unseen()), 0);
    }

    #[gpui_kit::test]
    fn scrolling_to_the_bottom_resumes_even_while_messages_arrive(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 1000);
        h.push(200, "hello", cx);
        h.scroll(300.0, cx);
        for _ in 0..5 {
            h.push(4, "hello", cx);
        }
        assert!(!h.following(cx));
        for _ in 0..10 {
            h.scroll(-200.0, cx);
            h.push(4, "hello", cx);
        }
        h.next_frame(cx);
        assert!(h.following(cx));
    }

    #[gpui_kit::test]
    fn resize_rewraps_without_reshaping(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        // About 90 characters: one line at 800 px, several at 200 px.
        h.push(1, &"lorem ipsum ".repeat(7), cx);
        let (wide, _) = h.last_message_lines(cx);
        assert_eq!(wide, 1);
        let shaped = h.view.read_with(cx, |v, _| v.cache_stats().shaped);

        cx.simulate_resize(size(px(200.0), px(600.0)));
        cx.run_until_parked();
        let (narrow, _) = h.last_message_lines(cx);
        assert!(narrow >= 4, "{narrow} lines at 200 px");
        assert_eq!(
            h.view.read_with(cx, |v, _| v.cache_stats().shaped),
            shaped,
            "a resize re-flows, it does not re-shape"
        );

        cx.simulate_resize(size(px(800.0), px(600.0)));
        cx.run_until_parked();
        assert_eq!(h.last_message_lines(cx).0, 1);
    }

    /// A fetcher whose downloads never finish: images stay placeholders.
    struct NeverFetcher;

    impl gpui_chat_media::Fetcher for NeverFetcher {
        fn fetch<'a>(&'a self, _: &'a str) -> gpui_chat_media::FetchFuture<'a> {
            Box::pin(std::future::pending())
        }
    }

    #[gpui_kit::test]
    fn emote_rows_take_the_emote_height_before_images_load(cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mut h, cx) = Harness::new(cx, 100);
        cx.update(|_, cx| {
            Images::install(
                gpui_chat_media::ImageCache::builder(runtime.handle().clone())
                    .fetcher(Arc::new(NeverFetcher)),
                cx,
            );
        });
        let message = Message::chat(
            "e",
            Timestamp(0),
            Author::new("al", "Al"),
            vec![
                Fragment::Text("hi ".into()),
                Fragment::Emote {
                    emote: Arc::new(gpui_chat_core::Emote::twitch("25", "Kappa")),
                    zero_width_overlays: vec![],
                },
            ],
        );
        h.store.update(cx, |store, cx| {
            store.apply_all([ChatEvent::Message {
                channel: ChannelId::new("test"),
                message: Arc::new(message),
            }]);
            cx.notify();
        });
        h.next += 1;
        cx.run_until_parked();
        let (flow, line_height) = h.view.read_with(cx, |view, _| {
            (view.cache.borrow().flow_of(0).unwrap(), view.line_height)
        });
        // 20 px text lines; the 28 px placeholder box makes the row's only line 28 px tall.
        assert_eq!(line_height, px(20.0));
        assert_eq!((flow.line_count(), flow.height()), (1, 28.0));
        // Only a placeholder was painted: nothing animated, so no animation timer.
        assert!(
            !h.view
                .read_with(cx, |view, _| view.images.borrow().wake_pending())
        );
    }

    fn emote_message(n: u64, urls: &[&str]) -> ChatEvent {
        let mut fragments = vec![Fragment::Text("hi ".into())];
        for (i, url) in urls.iter().enumerate() {
            let emote = gpui_chat_core::Emote {
                urls: gpui_chat_core::ImageUrls::new().with(gpui_chat_core::ImageScale::X1, *url),
                ..gpui_chat_core::Emote::twitch(format!("{n}-{i}"), format!("E{n}x{i}"))
            };
            fragments.push(Fragment::Emote {
                emote: Arc::new(emote),
                zero_width_overlays: vec![],
            });
            fragments.push(Fragment::Text(" ".into()));
        }
        ChatEvent::Message {
            channel: ChannelId::new("test"),
            message: Arc::new(Message::chat(
                format!("m{n}"),
                Timestamp(0),
                Author::new("al", "Al"),
                fragments,
            )),
        }
    }

    #[gpui_kit::test]
    fn a_changed_emote_box_reshapes_only_the_messages_that_draw_it(cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mut h, cx) = Harness::new(cx, 100);
        cx.update(|_, cx| {
            Images::install(
                gpui_chat_media::ImageCache::builder(runtime.handle().clone())
                    .fetcher(Arc::new(NeverFetcher)),
                cx,
            );
        });
        // Rows 0 and 3 draw "wide" (alone / with another emote), rows 1 and 4 only "plain", 2 is
        // text, and the rest are text too: 30 rows, all on screen or measured.
        let events = [
            emote_message(0, &["wide"]),
            emote_message(1, &["plain"]),
            chat(2, "just text"),
            emote_message(3, &["plain", "wide"]),
            emote_message(4, &["plain"]),
        ];
        h.store.update(cx, |store, cx| {
            store.apply_all(events);
            cx.notify();
        });
        h.next += 5;
        cx.run_until_parked();
        let shaped = |cx: &mut VisualTestContext, h: &Harness| {
            h.view.read_with(cx, |v, _| v.cache_stats().shaped)
        };
        let baseline = shaped(cx, &h);
        assert_eq!(baseline, 5, "every row was shaped once");

        // "wide" turned out to have another size than its placeholder.
        cx.update(|_, cx| {
            cx.global_mut::<Images>()
                .record_layout_change(&gpui_chat_media::ImageKey::new("wide"));
        });
        h.view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(
            shaped(cx, &h) - baseline,
            2,
            "only rows 0 and 3 are shaped again, not all five"
        );

        // Nothing new logged: another draw shapes nothing.
        h.view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(shaped(cx, &h) - baseline, 2);
        assert_eq!(
            h.view.read_with(cx, |v, _| v.generation),
            0,
            "no global re-shape"
        );

        // A change nobody draws re-shapes nothing at all.
        cx.update(|_, cx| {
            cx.global_mut::<Images>()
                .record_layout_change(&gpui_chat_media::ImageKey::new("elsewhere"));
        });
        h.view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(shaped(cx, &h) - baseline, 2);
    }

    #[gpui_kit::test]
    fn line_height_and_timestamps_apply_live(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        h.push(1, "hello", cx);
        let before = h.last_message_lines(cx).1;
        let shaped = h.view.read_with(cx, |v, _| v.cache_stats().shaped);

        let mut settings = Settings::default();
        settings.appearance.line_height = 2.5;
        settings.chat.timestamps = false;
        h.settings.update(cx, |store, cx| {
            store.apply_events([SettingsEvent::Reloaded(settings)]);
            cx.notify();
        });
        cx.run_until_parked();
        let after = h.last_message_lines(cx).1;
        // 14 px font: 20 px lines at 1.4, 35 px at 2.5.
        assert_eq!((before, after), (px(20.0), px(35.0)));
        assert!(h.view.read_with(cx, |v, _| v.cache_stats().shaped) > shaped);
        assert_eq!(h.view.read_with(cx, |v, _| v.generation), 1);
    }
    #[gpui_kit::test]
    fn badge_definitions_arriving_for_this_channel_lay_messages_out_again(cx: &mut TestAppContext) {
        use gpui_chat_core::{Badge, BadgeId, ImageUrls};
        use gpui_chat_emotes::BadgeSet;

        let (mut h, cx) = Harness::new(cx, 100);
        h.push(3, "hello", cx);
        cx.update(|_, cx| Badges::install(cx));
        // The first render with a registry only notes its version.
        h.push(1, "again", cx);
        let generation =
            |h: &Harness, cx: &mut VisualTestContext| h.view.read_with(cx, |v, _| v.generation);
        assert_eq!(generation(&h, cx), 0);

        let (registry, changed) = cx.update(|_, cx| {
            let badges = cx.global::<Badges>();
            (badges.registry().clone(), badges.changed_handle())
        });
        let set = |title: &str| {
            let mut set = BadgeSet::new();
            set.insert(Badge {
                id: BadgeId::new("subscriber", "42"),
                title: title.to_owned(),
                urls: ImageUrls::new(),
            });
            set
        };
        // Another channel's badges are none of this view's business: it renders, shapes nothing.
        let shaped = h.view.read_with(cx, |v, _| v.cache_stats().shaped);
        registry.store_channel(&ChannelId::new("elsewhere"), set("Other"));
        changed.notify_one();
        cx.run_until_parked();
        assert_eq!(generation(&h, cx), 0);
        assert_eq!(h.view.read_with(cx, |v, _| v.cache_stats().shaped), shaped);

        // This channel's arrive (a moment after the join): everything is shaped again.
        registry.store_channel(&ChannelId::new("test"), set("Mine"));
        changed.notify_one();
        cx.run_until_parked();
        assert_eq!(generation(&h, cx), 1);
        assert!(h.view.read_with(cx, |v, _| v.cache_stats().shaped) > shaped);
    }

    // --- the user card (PR 5.4) ---

    /// A message by `login` with `text` (which may hold `@mentions`), tokenized.
    fn said(n: u64, login: &str, text: &str) -> ChatEvent {
        ChatEvent::Message {
            channel: ChannelId::new("test"),
            message: Arc::new(Message::chat(
                format!("m{n}"),
                Timestamp(0),
                Author::new(login, login),
                gpui_chat_core::Tokenizer::new()
                    .tokenize(text, &[])
                    .fragments,
            )),
        }
    }

    fn push_events(h: &mut Harness, events: Vec<ChatEvent>, cx: &mut VisualTestContext) {
        h.next += events.len() as u64;
        h.store.update(cx, |store, cx| {
            store.apply_all(events);
            cx.notify();
        });
        cx.run_until_parked();
    }

    /// Where the first `needle` of row `seq`'s display text is drawn (its second character).
    fn at_text(h: &Harness, seq: u64, needle: &str, cx: &mut VisualTestContext) -> Point<Pixels> {
        let text = h.view.read_with(cx, |v, _| {
            v.cache.borrow().shaped(seq).unwrap().display.text.clone()
        });
        point_of(
            h,
            seq,
            text.find(needle).expect("the text is drawn") + 1,
            cx,
        )
    }

    fn card_login(h: &Harness, cx: &mut VisualTestContext) -> Option<String> {
        h.view
            .read_with(cx, |v, _| v.user_card_login().map(str::to_owned))
    }

    #[gpui_kit::test]
    fn a_click_on_a_name_or_a_mention_opens_that_users_card(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        push_events(
            &mut h,
            vec![
                said(0, "al", "hello @Bob_1 how are you"),
                said(1, "cy", "hi"),
            ],
            cx,
        );
        assert_eq!(card_login(&h, cx), None);
        let name = at_text(&h, 0, "al", cx);
        press(name, name, 1, cx);
        assert_eq!(card_login(&h, cx).as_deref(), Some("al"));
        // A mention opens the mentioned user (lowercase), replacing the open card.
        // The card sits over the bottom rows (they are drawn at the bottom of the window), so it
        // is closed before aiming at the mention.
        h.view.update(cx, |v, cx| v.close_user_card(cx));
        cx.run_until_parked();
        let mention = at_text(&h, 0, "@Bob_1", cx);
        press(mention, mention, 1, cx);
        assert_eq!(card_login(&h, cx).as_deref(), Some("bob_1"));
        // A press outside the card closes it.
        let outside = point(px(2.0), px(300.0));
        press(outside, outside, 1, cx);
        assert_eq!(card_login(&h, cx), None);
    }

    #[gpui_kit::test]
    fn dragging_from_a_name_selects_instead_of_opening_a_card(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        push_events(&mut h, vec![said(0, "al", "hello there")], cx);
        let from = at_text(&h, 0, "al", cx);
        let to = at_text(&h, 0, "there", cx);
        press(from, to, 1, cx);
        assert_eq!(card_login(&h, cx), None);
        assert!(selected(&h, cx).is_some());
    }

    #[gpui_kit::test]
    fn esc_closes_the_card_and_asks_for_the_input_box_and_otherwise_goes_on(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 100);
        push_events(&mut h, vec![said(0, "al", "hello")], cx);
        let escapes = Rc::new(Cell::new(0));
        let dismissed = Rc::new(Cell::new(0));
        cx.update(|_, cx| {
            cx.bind_keys([gpui_kit::KeyBinding::new(
                "escape",
                ClearSelection,
                Some(CHAT_CONTEXT),
            )]);
            let e = escapes.clone();
            cx.on_action(move |_: &ClearSelection, _| e.set(e.get() + 1));
            let d = dismissed.clone();
            cx.subscribe(&h.view, move |_, event: &ChatViewEvent, _| {
                if matches!(event, ChatViewEvent::SelectionDismissed) {
                    d.set(d.get() + 1);
                }
            })
            .detach();
        });
        let name = at_text(&h, 0, "al", cx);
        press(name, name, 1, cx);
        // The click is over (the double-click interval passed): the list is not held for it.
        settle(cx);
        assert_eq!(card_login(&h, cx).as_deref(), Some("al"));
        // Opening the card gave the chat the focus, so Esc reaches it: it closes the card, the
        // owner is asked to focus the input box, and the key does not go on.
        cx.simulate_keystrokes("escape");
        assert_eq!(card_login(&h, cx), None);
        assert_eq!((escapes.get(), dismissed.get()), (0, 1));
        // With no card (and no selection) Esc goes on, to the composer's chain.
        cx.update(|window, cx| {
            let focus = h.view.read(cx).focus_handle.clone();
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("escape");
        assert_eq!((escapes.get(), dismissed.get()), (1, 1));
    }

    #[gpui_kit::test]
    fn the_cards_actions_tell_the_owner_and_close_the_card(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        push_events(&mut h, vec![said(0, "al", "hello")], cx);
        let heard = Rc::new(RefCell::new(Vec::new()));
        let sink = heard.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&h.view, move |_, event: &ChatViewEvent, _| {
                sink.borrow_mut().push(match event {
                    ChatViewEvent::Mention(login) => format!("mention {login}"),
                    ChatViewEvent::ReplyRequested(message) => format!("reply {}", message.id),
                    ChatViewEvent::ModerateUser {
                        login,
                        user_id,
                        action,
                    } => format!("{action:?} {login} {user_id:?}"),
                    _ => return,
                });
            })
        });
        let open = |cx: &mut VisualTestContext| {
            let name = at_text(&h, 0, "al", cx);
            press(name, name, 1, cx);
            assert_eq!(card_login(&h, cx).as_deref(), Some("al"));
        };
        open(cx);
        h.view.update(cx, |v, cx| v.mention("al".into(), cx));
        assert_eq!(card_login(&h, cx), None);
        open(cx);
        let message = h.store.read_with(cx, |store, _| {
            store.messages().get_by_seq(0).unwrap().clone()
        });
        h.view.update(cx, |v, cx| v.reply_from_card(message, cx));
        assert_eq!(card_login(&h, cx), None);
        open(cx);
        h.view.update(cx, |v, cx| {
            v.moderate_user("al".into(), Some("7".into()), RowAction::Timeout(600), cx);
        });
        assert_eq!(card_login(&h, cx), None);
        assert_eq!(
            *heard.borrow(),
            ["mention al", "reply m0", "Timeout(600) al Some(\"7\")"]
        );
    }

    #[gpui_kit::test]
    fn view_user_and_the_selection_action_open_the_card_and_a_non_login_does_not(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 100);
        push_events(
            &mut h,
            vec![said(0, "al", "hello world"), said(1, "cy", "hi")],
            cx,
        );
        // Nothing selected: the action finds nothing to open.
        let opened = cx.update(|window, cx| {
            h.view
                .update(cx, |v, cx| v.open_user_card_for_selection(window, cx))
        });
        assert!(!opened);
        // The author of the first selected message.
        let at = at_text(&h, 1, "hi", cx);
        press(at, at, 3, cx);
        let opened = cx.update(|window, cx| {
            h.view
                .update(cx, |v, cx| v.open_user_card_for_selection(window, cx))
        });
        assert!(opened);
        assert_eq!(card_login(&h, cx).as_deref(), Some("cy"));
        // "@" and an `@`-prefixed login are accepted; nonsense is not.
        cx.update(|window, cx| {
            h.view.update(cx, |v, cx| {
                v.open_user_card("@Dee", None, point(px(10.0), px(10.0)), window, cx);
            });
        });
        assert_eq!(card_login(&h, cx).as_deref(), Some("dee"));
        cx.update(|window, cx| {
            h.view.update(cx, |v, cx| {
                v.open_user_card("not a login!", None, point(px(10.0), px(10.0)), window, cx);
            });
        });
        assert_eq!(card_login(&h, cx).as_deref(), Some("dee"));
    }

    #[gpui_kit::test]
    fn the_card_draws_in_every_lookup_state(cx: &mut TestAppContext) {
        use crate::user_directory::{self, Follow, Profile};
        let (mut h, cx) = Harness::new(cx, 100);
        cx.update(|_, cx| user_directory::init(cx));
        push_events(
            &mut h,
            vec![said(0, "al", "hello"), said(1, "al", "again")],
            cx,
        );
        let directory = cx.update(|_, cx| user_directory::current(cx).unwrap());
        let profile = gpui_chat_twitch::UserProfile {
            id: "77".into(),
            login: "al".into(),
            display_name: "Al".into(),
            avatar_url: Some("https://example.com/al.png".into()),
            created_at: gpui_chat_core::parse_rfc3339("2015-03-14T09:26:53Z"),
        };
        let states = [
            Profile::Loading,
            Profile::Missing,
            Profile::Failed("Could not load account details (boom).".into()),
            Profile::Found(Arc::new(profile)),
        ];
        let name = at_text(&h, 0, "al", cx);
        press(name, name, 1, cx);
        for state in states {
            directory.update(cx, |d, cx| {
                d.seed_state("al", state);
                d.seed_follow("1", "77", Follow::Since(Some(1_600_000_000)));
                cx.notify();
            });
            cx.run_until_parked();
            assert_eq!(card_login(&h, cx).as_deref(), Some("al"));
        }
    }

    #[gpui_kit::test]
    fn a_message_clicked_in_the_card_closes_it_and_scrolls_to_it(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 500);
        push_events(
            &mut h,
            (0..300)
                .map(|n| said(n, if n == 7 { "al" } else { "cy" }, "some text"))
                .collect(),
            cx,
        );
        cx.update(|window, cx| {
            h.view.update(cx, |v, cx| {
                v.open_user_card("al", None, point(px(100.0), px(100.0)), window, cx);
            });
        });
        h.view.update(cx, |v, cx| v.jump_from_card(7, cx));
        cx.run_until_parked();
        assert_eq!(card_login(&h, cx), None);
        let top = h
            .view
            .read_with(cx, |v, _| v.list.logical_scroll_top().item_ix);
        assert_eq!(top, 7usize.saturating_sub(JUMP_CONTEXT_ROWS));
        assert!(!h.view.read_with(cx, |v, _| v.list.is_following_tail()));
        // A message that left the scrollback is not a place to go to.
        h.view.update(cx, |v, cx| v.jump_from_card(9_999, cx));
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.list.logical_scroll_top().item_ix),
            top
        );
    }

    #[gpui_kit::test]
    fn a_double_click_on_a_bottom_row_name_selects_the_word_the_card_opens_above(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 100);
        push_events(&mut h, vec![said(0, "al", "hello there")], cx);
        let name = at_text(&h, 0, "al", cx);
        // The newest lines are at the bottom of the window: the card opens above the pointer, so
        // it is not under it for the second press.
        press(name, name, 1, cx);
        assert_eq!(card_login(&h, cx).as_deref(), Some("al"));
        press(name, name, 2, cx);
        assert_eq!(selected(&h, cx).as_deref(), Some("al"));
        assert_eq!(
            card_login(&h, cx),
            None,
            "the second press is outside the card"
        );
    }

    #[gpui_kit::test]
    fn a_press_inside_the_card_keeps_the_chat_selection(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        push_events(
            &mut h,
            vec![
                said(0, "al", "hello there"),
                said(1, "cy", "general kenobi"),
            ],
            cx,
        );
        // Select something in the chat, then open a card (a name click replaces the selection,
        // so open it from the menu path instead) and press inside it.
        let from = at_text(&h, 1, "general", cx);
        let to = at_text(&h, 1, "kenobi", cx);
        press(from, to, 1, cx);
        assert!(selected(&h, cx).is_some());
        cx.update(|window, cx| {
            h.view.update(cx, |v, cx| {
                v.open_user_card("al", None, point(px(100.0), px(500.0)), window, cx);
            });
        });
        cx.run_until_parked();
        let card = h.view.read_with(cx, |v, _| v.surface.overlay_bounds());
        let card = card.expect("the card told the surface where it is");
        let inside = point(card.origin.x + px(20.0), card.origin.y + px(10.0));
        press(inside, inside, 1, cx);
        assert_eq!(
            card_login(&h, cx).as_deref(),
            Some("al"),
            "a press on the card keeps it"
        );
        assert!(selected(&h, cx).is_some(), "and the selection");
        // A press outside both clears the selection and closes the card.
        let outside = point(px(2.0), px(20.0));
        press(outside, outside, 1, cx);
        assert_eq!(card_login(&h, cx), None);
        assert_eq!(selected(&h, cx), None);
    }

    #[gpui_kit::test]
    fn a_name_click_gives_the_card_the_id_of_the_account_that_sent_the_message(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 100);
        let mut message = Message::chat(
            "m0",
            Timestamp(0),
            {
                let mut author = Author::new("al", "Al");
                author.user_id = Some("55".into());
                author
            },
            vec![Fragment::Text("hello".into())],
        );
        message.flags.echo = false;
        push_events(
            &mut h,
            vec![ChatEvent::Message {
                channel: ChannelId::new("test"),
                message: Arc::new(message),
            }],
            cx,
        );
        let name = at_text(&h, 0, "Al", cx);
        press(name, name, 1, cx);
        // The card knows whose message it was opened from: 55, from the message.
        let origin = h.view.read_with(cx, |v, _| {
            v.user_card.as_ref().and_then(|c| c.origin_id.clone())
        });
        assert_eq!(origin.as_deref(), Some("55"));
    }

    // ---- ignores, filters and find (PR 5.7) ----

    fn ignoring(users: &[&str]) -> Settings {
        let mut settings = Settings::default();
        settings.ignores.users = users.iter().map(|u| (*u).to_owned()).collect();
        settings
    }

    fn reload(h: &Harness, settings: Settings, cx: &mut VisualTestContext) {
        h.settings.update(cx, |store, cx| {
            if store.apply_events([SettingsEvent::Reloaded(settings)]) {
                cx.notify();
            }
        });
        cx.run_until_parked();
    }

    /// The rows the view reports, as the texts of the messages they show.
    fn row_texts(h: &Harness, cx: &mut VisualTestContext) -> Vec<String> {
        h.view.read_with(cx, |view, cx| {
            let rows = view.rows.borrow();
            let store = view.store.read(cx);
            rows.seqs()
                .iter()
                .map(|seq| store.messages().get_by_seq(*seq).unwrap().plain_text())
                .collect()
        })
    }

    /// The list and its rows agree, and the rows are exactly what the view shows of the store.
    fn assert_consistent(h: &Harness, cx: &mut VisualTestContext) {
        h.view.read_with(cx, |view, cx| {
            let rows = view.rows.borrow();
            assert_eq!(view.list.item_count(), rows.len());
            let store = view.store.read(cx);
            let expected: Vec<u64> = store
                .messages()
                .iter_with_seq()
                .filter(|(_, m)| view.shows(m))
                .map(|(seq, _)| seq)
                .collect();
            assert_eq!(rows.seqs(), expected);
        });
    }

    #[gpui_kit::test]
    fn an_ignored_user_has_no_row_and_the_settings_change_brings_rows_back_and_takes_them(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::with_settings(cx, 100, ignoring(&["bo"]), true);
        push_events(
            &mut h,
            vec![
                said(0, "al", "one"),
                said(1, "bo", "two"),
                said(2, "al", "three"),
                said(3, "BO", "four"),
            ],
            cx,
        );
        assert_eq!(row_texts(&h, cx), ["one", "three"]);
        assert_eq!(h.items(cx), 2);
        assert_consistent(&h, cx);
        // More messages: only the ones that pass get rows.
        push_events(
            &mut h,
            vec![said(4, "bo", "five"), said(5, "cy", "six")],
            cx,
        );
        assert_eq!(row_texts(&h, cx), ["one", "three", "six"]);
        // Unignored: what they said is still in the scrollback, and comes back in order.
        reload(&h, Settings::default(), cx);
        assert_eq!(h.items(cx), 6);
        assert_consistent(&h, cx);
        // Ignored again, another one.
        reload(&h, ignoring(&["al"]), cx);
        assert_eq!(row_texts(&h, cx), ["two", "four", "five", "six"]);
        assert_consistent(&h, cx);
    }

    #[gpui_kit::test]
    fn ignored_messages_collapse_to_one_line_when_asked_and_a_click_opens_them(
        cx: &mut TestAppContext,
    ) {
        let mut settings = ignoring(&["bo"]);
        settings.ignores.show_collapsed = true;
        let (mut h, cx) = Harness::with_settings(cx, 100, settings, true);
        push_events(
            &mut h,
            vec![
                said(0, "al", "one"),
                said(1, "bo", "secret words"),
                said(2, "al", "two"),
            ],
            cx,
        );
        assert_eq!(h.items(cx), 3, "the ignored message keeps a row");
        let (shown, ignored) = h.view.read_with(cx, |v, _| {
            let cache = v.cache.borrow();
            (
                cache.shaped(0).unwrap().display.text.clone(),
                cache.shaped(1).unwrap().display.text.clone(),
            )
        });
        assert!(shown.contains("one"), "{shown}");
        assert!(ignored.contains("1 ignored message"), "{ignored}");
        assert!(
            !ignored.contains("secret") && !ignored.contains("bo"),
            "{ignored}"
        );
        // A filter shows no placeholders.
        h.view.update(cx, |view, cx| view.toggle_links_filter(cx));
        cx.run_until_parked();
        assert_eq!(h.items(cx), 0);
        h.view.update(cx, |view, cx| view.clear_filters(cx));
        cx.run_until_parked();
        assert_eq!(h.items(cx), 3);
        assert_consistent(&h, cx);
        // Revealed, it is drawn in full.
        h.view
            .update(cx, |view, cx| view.reveal(&MessageId::from("m1"), cx));
        cx.run_until_parked();
        let revealed = h.view.read_with(cx, |v, _| {
            v.cache.borrow().shaped(1).unwrap().display.text.clone()
        });
        assert!(revealed.contains("secret words"), "{revealed}");
    }

    #[gpui_kit::test]
    fn filters_hide_rows_the_strip_counts_and_clearing_brings_them_back(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::with_settings(cx, 100, Settings::default(), true);
        push_events(
            &mut h,
            vec![
                said(0, "al", "plain"),
                said(1, "bo", "see https://example.com"),
                said(2, "al", "hey @viewer"),
                said(3, "nightbot", "!rules"),
            ],
            cx,
        );
        assert_eq!(h.items(cx), 4);
        h.view.update(cx, |view, cx| view.toggle_links_filter(cx));
        cx.run_until_parked();
        assert_eq!(row_texts(&h, cx), ["see https://example.com"]);
        assert_eq!(h.view.read_with(cx, |v, cx| v.row_counts(cx)), (1, 4));
        h.view.update(cx, |view, cx| view.toggle_links_filter(cx));
        h.view
            .update(cx, |view, cx| view.filter_from(Some("AL"), cx));
        cx.run_until_parked();
        assert_eq!(row_texts(&h, cx), ["plain", "hey @viewer"]);
        h.view.update(cx, |view, cx| view.toggle_bots_filter(cx));
        h.view.update(cx, |view, cx| view.filter_from(None, cx));
        cx.run_until_parked();
        assert_eq!(h.items(cx), 3, "the bot's line is hidden");
        h.view.update(cx, |view, cx| view.clear_filters(cx));
        cx.run_until_parked();
        assert_eq!(h.items(cx), 4);
        assert!(!h.view.read_with(cx, |v, _| v.filters().is_active()));
        assert_consistent(&h, cx);
    }

    #[gpui_kit::test]
    fn new_messages_follow_the_filter_and_history_merges_keep_the_rows_consistent(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::with_settings(cx, 12, ignoring(&["bo"]), true);
        h.view
            .update(cx, |view, cx| view.filter_from(Some("al"), cx));
        // Everything from "al" and "bo" mixed; the ignore and the filter both apply.
        let events: Vec<_> = (0..10)
            .map(|n| said(n, if n % 2 == 0 { "al" } else { "bo" }, &format!("m{n}")))
            .collect();
        push_events(&mut h, events, cx);
        assert_eq!(h.items(cx), 5);
        assert_consistent(&h, cx);
        // History arrives before them and the cap drops the oldest: still consistent.
        history_before(6, cx, &h);
        assert_consistent(&h, cx);
        // Wait: history is by "old", which the filter hides.
        assert!(row_texts(&h, cx).iter().all(|t| t != "earlier"));
        h.view.update(cx, |view, cx| view.clear_filters(cx));
        cx.run_until_parked();
        assert_consistent(&h, cx);
        assert!(row_texts(&h, cx).iter().any(|t| t == "earlier"));
    }

    #[gpui_kit::test]
    fn a_jump_to_a_message_a_filter_hides_clears_the_filter_first(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 100);
        let events: Vec<_> = (0..40)
            .map(|n| said(n, if n == 7 { "bo" } else { "al" }, "text"))
            .collect();
        push_events(&mut h, events, cx);
        h.view
            .update(cx, |view, cx| view.filter_from(Some("bo"), cx));
        cx.run_until_parked();
        assert_eq!(h.items(cx), 1);
        let found = h.view.update(cx, |view, cx| {
            view.scroll_to_message(&MessageId::from("m3"), cx)
        });
        cx.run_until_parked();
        assert!(found);
        assert!(!h.view.read_with(cx, |v, _| v.filters().is_active()));
        assert_eq!(h.items(cx), 40);
    }

    /// Types `text` into the open find bar.
    fn type_find(h: &Harness, text: &str, cx: &mut VisualTestContext) {
        let input = h
            .view
            .read_with(cx, |v, _| v.find.as_ref().unwrap().input.clone());
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        });
        h.view.update(cx, |view, cx| view.find_text_changed(cx));
        cx.run_until_parked();
    }

    fn open_find(h: &Harness, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            h.view.update(cx, |view, cx| view.open_find(window, cx));
        });
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn find_counts_matches_steps_through_them_and_wraps(cx: &mut TestAppContext) {
        let (mut h, cx) = Harness::new(cx, 1000);
        let events: Vec<_> = (0..200)
            .map(|n| said(n, "al", &format!("line {n}")))
            .collect();
        push_events(&mut h, events, cx);
        open_find(&h, cx);
        assert!(h.view.read_with(cx, |v, _| v.find_is_open()));
        type_find(&h, "LINE 1", cx);
        // line 1, line 10..19, line 100..199: 1 + 10 + 100.
        let status = h.view.read_with(cx, |v, _| v.find_status().unwrap());
        assert_eq!(status.matches, 111);
        assert_eq!(status.position, Some(111), "starts on the newest match");
        assert_eq!(status.label().as_deref(), Some("111 of 111"));
        // The newest match is on screen; stepping back goes to the one before and scrolls there.
        h.view.update(cx, |v, cx| v.find_step(false, cx));
        cx.run_until_parked();
        let status = h.view.read_with(cx, |v, _| v.find_status().unwrap());
        assert_eq!(status.position, Some(110));
        // Forward from the newest wraps to the oldest, which is far above: the list scrolled there.
        h.view.update(cx, |v, cx| v.find_step(true, cx));
        h.view.update(cx, |v, cx| v.find_step(true, cx));
        cx.run_until_parked();
        let (status, top) = h.view.read_with(cx, |v, _| {
            (
                v.find_status().unwrap(),
                v.list_state().logical_scroll_top(),
            )
        });
        assert_eq!(status.position, Some(1));
        assert!(top.item_ix <= 1, "{top:?}");
        assert!(!h.following(cx), "a jump pauses following");
        // No match.
        type_find(&h, "zzz", cx);
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.find_status().unwrap().label()),
            Some("No matches".to_owned())
        );
        // Closing returns focus to the box (the pane hears the event) and clears the search.
        let closed = h.view.update(cx, |v, cx| v.close_find(cx));
        assert!(closed);
        assert!(h.view.read_with(cx, |v, _| v.find_status().is_none()));
    }

    #[gpui_kit::test]
    fn find_follows_new_messages_and_a_from_token_filters_instead_of_searching(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 1000);
        push_events(
            &mut h,
            vec![
                said(0, "al", "needle one"),
                said(1, "bo", "needle two"),
                said(2, "al", "hay"),
            ],
            cx,
        );
        open_find(&h, cx);
        type_find(&h, "needle", cx);
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.find_status().unwrap().matches),
            2
        );
        push_events(&mut h, vec![said(3, "cy", "another needle")], cx);
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.find_status().unwrap().matches),
            3
        );
        type_find(&h, "from:bo needle", cx);
        assert_eq!(row_texts(&h, cx), ["needle two"]);
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.find_status().unwrap().matches),
            1
        );
        // Taking the token away takes the filter away.
        type_find(&h, "needle", cx);
        assert_eq!(h.items(cx), 4);
        assert_eq!(
            h.view
                .read_with(cx, |v, _| v.find_status().unwrap().matches),
            3
        );
        assert_consistent(&h, cx);
    }

    #[gpui_kit::test]
    fn closing_the_bar_asks_the_owner_for_the_input_box_and_keeps_the_filters(
        cx: &mut TestAppContext,
    ) {
        let (mut h, cx) = Harness::new(cx, 100);
        push_events(&mut h, vec![said(0, "al", "a"), said(1, "bo", "b")], cx);
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&h.view, move |_, event: &ChatViewEvent, _| {
                sink.borrow_mut().push(format!("{event:?}"));
            })
        });
        open_find(&h, cx);
        type_find(&h, "from:al", cx);
        assert_eq!(h.items(cx), 1);
        h.view.update(cx, |v, cx| {
            v.close_find(cx);
        });
        assert!(
            events
                .borrow()
                .iter()
                .any(|e| e.contains("SelectionDismissed"))
        );
        assert_eq!(h.items(cx), 1, "the filter outlives the bar");
        // Reopened, the bar starts with the filter it still has.
        open_find(&h, cx);
        let text = h.view.read_with(cx, |v, cx| {
            v.find.as_ref().unwrap().input.read(cx).value().to_string()
        });
        assert_eq!(text, "from:al ");
        // Closing again with Esc does nothing when the bar is already closed.
        h.view.update(cx, |v, cx| {
            v.close_find(cx);
        });
        assert!(!h.view.update(cx, |v, cx| v.close_find(cx)));
    }
}
