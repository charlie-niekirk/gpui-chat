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

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::{
    Context, Entity, FollowMode, InteractiveElement, IntoElement, ListAlignment, ListState,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Window, canvas, div, font, list, point, px, white,
};

use super::display::DisplayOptions;
use super::element::{BadgeStyle, EmoteStyle, MessageElement, MessageStyle, Palette};
use super::layout::LayoutCache;
use super::list_sync::ListMirror;
use super::tooltip::EmoteHover;
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

pub struct ChatView {
    store: Entity<ChannelStore>,
    list: ListState,
    mirror: ListMirror,
    cache: Rc<RefCell<LayoutCache>>,
    text_config: Option<TextConfig>,
    generation: u64,
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
    /// Line height of the last render.
    line_height: Pixels,
    images: Rc<RefCell<FrameImages>>,
    hover: Rc<RefCell<EmoteHover>>,
    _subscriptions: Vec<Subscription>,
}

impl ChatView {
    pub fn new(store: Entity<ChannelStore>, cx: &mut Context<Self>) -> Self {
        let mirror = {
            let messages = store.read(cx).messages();
            ListMirror::new(messages.first_seq(), messages.next_seq())
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
        Self {
            store,
            list,
            mirror,
            cache: Rc::new(RefCell::new(LayoutCache::default())),
            text_config: None,
            generation: 0,
            image_version: None,
            badge_version: None,
            unseen_from: None,
            pill_shown: false,
            replaced_seen: 0,
            line_height: px(0.0),
            images: Rc::default(),
            hover: Rc::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Brings the list up to date with the store: splices, drops evicted layouts, counts unseen.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (first, next) = {
            let messages = self.store.read(cx).messages();
            (messages.first_seq(), messages.next_seq())
        };
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
            self.cache.borrow_mut().evict_before(first);
            if change.appended > 0 && !self.list.is_following_tail() {
                self.unseen_from.get_or_insert(previous_next);
            }
        }
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
            light: cx.global::<Tokens>().main_bg.l > 0.5,
        };
        if self.text_config.as_ref() != Some(&current) {
            if self.text_config.is_some() {
                self.generation += 1;
                self.list.remeasure();
                self.cache.borrow_mut().retain_key(self.generation);
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
                let seqs = self.cache.borrow_mut().invalidate_urls(&urls);
                self.remeasure_seqs(&seqs);
            }
            LayoutChanges::Everything => {
                self.generation += 1;
                self.list.remeasure();
                self.cache.borrow_mut().retain_key(self.generation);
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
            self.list.remeasure();
            self.cache.borrow_mut().retain_key(self.generation);
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
        });
        let store = self.store.clone();
        let cache = self.cache.clone();
        let first = self.mirror.first_seq();
        let messages = list(self.list.clone(), move |ix, _window, cx| {
            let seq = first + ix as u64;
            match store.read(cx).messages().get_by_seq(seq) {
                Some(message) => {
                    MessageElement::new(seq, message.clone(), style.clone(), cache.clone())
                        .into_any_element()
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
                        .text_color(white())
                        .text_sm()
                        .shadow_md()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_latest(cx)))
                        .child(format!("↓ {}", pill_label(self.unseen()))),
                )
        });

        // The channel name and state are shown by the pane header (`crate::pane`).
        div().size_full().flex().flex_col().child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(messages)
                .child(probe)
                .children(pill),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_chat_core::{Author, ChannelId, ChatEvent, Fragment, Message, Timestamp};
    use gpui_chat_settings::{AppearanceSettings, Settings, SettingsEvent};
    use gpui_kit::{
        AppContext as _, Entity, ScrollDelta, ScrollEvent, TestAppContext, VisualTestContext, size,
    };

    use super::*;
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
