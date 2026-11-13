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
//! * **Settings:** font family/size (via the `FontConfig` global), `appearance.line_height` and
//!   `chat.timestamps` apply live: a change bumps the layout generation (every cached layout is
//!   re-shaped on next use) and asks the list to remeasure. Theme changes only re-render: colours
//!   are resolved at paint time.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::{
    Context, Entity, FollowMode, InteractiveElement, IntoElement, ListAlignment, ListState,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Window, canvas, div, font, list, point, px, white,
};

use super::display::DisplayOptions;
use super::element::{MessageElement, MessageStyle, Palette};
use super::layout::LayoutCache;
use super::list_sync::ListMirror;
use crate::channels::ChannelStore;
use crate::config;
use crate::fonts::FontConfig;
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
}

pub struct ChatView {
    store: Entity<ChannelStore>,
    list: ListState,
    mirror: ListMirror,
    cache: Rc<RefCell<LayoutCache>>,
    text_config: Option<TextConfig>,
    generation: u64,
    /// Sequence number of the first message that arrived while not following. The pill counts
    /// from there to the newest message, skipping any the scrollback cap already evicted, so it
    /// never claims more new messages than the list still holds.
    unseen_from: Option<u64>,
    /// Whether the last render showed the pill.
    pill_shown: bool,
    /// Line height of the last render.
    line_height: Pixels,
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
        ];
        Self {
            store,
            list,
            mirror,
            cache: Rc::new(RefCell::new(LayoutCache::default())),
            text_config: None,
            generation: 0,
            unseen_from: None,
            pill_shown: false,
            line_height: px(0.0),
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

/// A pill label: `1 new message`, `12 new messages`, or a plain jump when nothing is new.
pub fn pill_label(unseen: usize) -> String {
    match unseen {
        0 => "Jump to latest".to_owned(),
        1 => "1 new message".to_owned(),
        n => format!("{n} new messages"),
    }
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        let config = self.update_text_config(cx);
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
        // this frame no longer matches, render once more.
        let probe = {
            let list = self.list.clone();
            let pill_shown = self.pill_shown;
            canvas(
                move |_, window, _| {
                    if list.is_following_tail() == pill_shown {
                        window.request_animation_frame();
                    }
                },
                |_, _, _, _| {},
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
}
