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
//! * **Selection and links** (PR 5.6): the list and the thread popover are each a selection
//!   [`Surface`] (see [`super::selection_layer`]). A press in the chat gives the view keyboard
//!   focus (key context [`CHAT_CONTEXT`]), where ⌘C / Ctrl-C copies the selection and Esc clears
//!   it (and asks the owner to focus the input box again); with nothing selected both go on. The
//!   pane also handles Copy, so the command palette reaches it from the input box.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui_chat_core::{Message, MessageId, Rebuild};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FollowMode,
    InteractiveElement, IntoElement, ListAlignment, ListState, MouseButton, ParentElement, Pixels,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, canvas, div,
    font, list, point, px,
};

use super::display::DisplayOptions;
use super::element::{BadgeStyle, EmoteStyle, HoveredRow, MessageStyle, OpenThread, Palette};
use super::layout::LayoutCache;
use super::list_sync::ListMirror;
use super::row::{RowContext, message_row, row_menu};
use super::selection_layer::{ClearSelection, CopySelection, Surface};
use super::thread::{self, ThreadPanel};
use super::tooltip::EmoteHover;
use crate::actions::CHAT_CONTEXT;
use crate::badges::Badges;
use crate::channels::ChannelStore;
use crate::config;
use crate::fonts::FontConfig;
use crate::images::{FrameImages, Images, LayoutChanges};
use crate::theme::Tokens;

/// Rows rendered beyond the viewport, so short scrolls do not pop in.
const OVERDRAW: Pixels = px(240.0);
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
}

impl EventEmitter<ChatViewEvent> for ChatView {}

pub struct ChatView {
    store: Entity<ChannelStore>,
    list: ListState,
    mirror: ListMirror,
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
    /// The thread popover, while one is open.
    thread: Option<ThreadPanel>,
    /// Text selection and links in the list.
    surface: Rc<Surface>,
    /// Focused by a press in the chat, so ⌘C and Esc reach the selection.
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ChatView {
    pub fn new(store: Entity<ChannelStore>, cx: &mut Context<Self>) -> Self {
        let (mirror, rebuilds_seen) = {
            let messages = store.read(cx).messages();
            (
                ListMirror::new(messages.first_seq(), messages.next_seq()),
                messages.rebuild_serial(),
            )
        };
        let list = ListState::new(mirror.len(), ListAlignment::Bottom, OVERDRAW);
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
        let cache = Rc::new(RefCell::new(LayoutCache::default()));
        Self {
            store,
            surface: Surface::new(Some(list.clone()), cache.clone()),
            focus_handle: cx.focus_handle(),
            list,
            mirror,
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
            thread: None,
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
                .for_each(|rebuild| self.apply_rebuild(rebuild)),
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
        self.sync_range(first, next);
        debug_assert_eq!(self.list.item_count(), self.mirror.len());
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
        // Also covers changes without new messages (deletions): rows re-render.
        cx.notify();
    }

    /// Moves the list from the range it shows to `first..next` of the store's sequence numbers.
    fn sync_range(&mut self, first: u64, next: u64) {
        let previous_next = self.mirror.next_seq();
        let change = self.mirror.sync(first, next);
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
    fn apply_rebuild(&mut self, rebuild: &Rebuild) {
        self.sync_range(rebuild.old_first, rebuild.old_next);
        if rebuild.removed_front > 0 {
            self.list.splice(0..rebuild.removed_front, 0);
        }
        for &(at, count) in &rebuild.inserts {
            self.list.splice(at..at, count);
        }
        self.mirror = ListMirror::new(rebuild.new_first, rebuild.new_next);
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
        self.unseen_from.map_or(0, |from| {
            (self.mirror.next_seq() - from.max(self.mirror.first_seq())) as usize
        })
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
        let first = self.mirror.first_seq();
        let count = self.list.item_count();
        // `seqs` is ascending: merge neighbours into one call.
        let mut run: Option<std::ops::Range<usize>> = None;
        for &seq in seqs {
            let Some(ix) = seq.checked_sub(first).map(|ix| ix as usize) else {
                continue;
            };
            if ix >= count {
                continue;
            }
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
        self.surface.begin_frame(
            self.text_generation,
            self.mirror.first_seq(),
            config.options,
            selection_color,
            None,
        );
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
            hovered_row: self.can_reply.then(|| self.hovered.clone()),
            selection: Some(self.surface.clone()),
        });
        let handle = cx.entity().downgrade();
        let rows = |id, hovered: &HoveredRow| RowContext {
            id,
            view: handle.clone(),
            tokens,
            hovered: self.can_reply.then(|| hovered.get()).flatten(),
            can_reply: self.can_reply,
        };
        let thread_panel = self.thread.as_ref().map(|panel| {
            // The thread is the context: no reply lines inside it, and no popover on top of it.
            let style = Rc::new(MessageStyle {
                reply_line: false,
                on_open_thread: None,
                hover: panel.hover.clone(),
                hovered_row: self.can_reply.then(|| panel.hovered.clone()),
                selection: Some(panel.surface.clone()),
                ..(*style).clone()
            });
            let view = thread::thread_view(self.store.read(cx).messages(), panel.root.as_str());
            panel.surface.begin_frame(
                self.text_generation,
                self.mirror.first_seq(),
                config.options,
                selection_color,
                Some(view.seqs()),
            );
            let rows = rows("thread-row", &panel.hovered);
            thread::render(panel, &view, &style, &rows, self.store.clone(), cx)
        });
        let store = self.store.clone();
        let cache = self.cache.clone();
        let first = self.mirror.first_seq();
        let list_rows = rows("chat-row", &self.hovered);
        let messages = list(self.list.clone(), move |ix, _window, cx| {
            let seq = first + ix as u64;
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
                if this.clear_selection(cx) {
                    cx.emit(ChatViewEvent::SelectionDismissed);
                } else {
                    cx.propagate();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    // First, so the rows and what is drawn over them see a press before it does.
                    .child(self.surface.layer())
                    .child(messages)
                    .child(probe)
                    .children(pill)
                    .children(thread_panel)
                    .context_menu(menu),
            )
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
    use gpui_chat_settings::{AppearanceSettings, Settings, SettingsEvent};
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
            let settings = cx.update(|cx| {
                gpui_kit::init(cx);
                let settings = config::install_for_test(Settings::default(), cx);
                fonts::init(&AppearanceSettings::default(), cx);
                theme::apply_settings(&AppearanceSettings::default(), None, cx);
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
        assert_eq!(h.view.read_with(cx, |v, _| v.mirror.first_seq()), 50);
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
            let seq = v.mirror.first_seq() + top.item_ix as u64;
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
}
