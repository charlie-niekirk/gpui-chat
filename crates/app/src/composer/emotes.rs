//! The completion and emote-picker half of the composer (PR 3.3), kept apart from the sending half.
//!
//! * `:name`, `@name`, `/command` and Tab open a list over the box ([`crate::completion`]); the
//!   arrows move in it, Tab or Enter accept, Esc closes it.
//! * ⌘E (Ctrl-E) or the button next to the box open the emote picker ([`crate::emote_picker`]);
//!   what it picks is inserted at the caret.
//! * The user's own Twitch emotes are asked for from Helix once per user and room, as soon as the
//!   room id is known; the third-party sets come from the emote service.

use std::sync::Arc;

use gpui_chat_core::{Emote, recent_chatters, word_insertion};
use gpui_chat_emotes::EmoteCatalog;
use gpui_chat_twitch::UserEmote;
use gpui_chat_twitch::auth::AuthState;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::{
    Anchor, AnyElement, Context, IntoElement, ParentElement as _, Styled as _, Window, anchored,
    canvas, deferred, div, point, px,
};

use super::Composer;
use crate::completion::{self, CHATTERS, Item, Outcome};
use crate::emote_picker::{EmotePicker, PickerEvent};
use crate::emote_sources::{Key, OWN_EMOTES_SCOPE};
use crate::theme::Tokens;
use gpui_kit::AppContext as _;

/// Gap between the box's top edge and the bottom of the list or picker above it.
const OVERLAY_GAP: f32 = 4.0;
/// Left edge: in line with the text box (the row's padding).
const OVERLAY_INSET: f32 = 12.0;

impl Composer {
    /// The emotes available here now.
    fn catalog(&mut self, cx: &mut Context<Self>) -> Arc<EmoteCatalog> {
        let channel = self.store.read(cx).channel().clone();
        let sets = self.live.emote_sets(&channel);
        self.emotes.catalog(&sets)
    }

    /// Recomputes the list for the text and caret as they are, without redrawing. `explicit` is a
    /// Tab press.
    fn recompute_completion(&mut self, explicit: bool, cx: &mut Context<Self>) -> Outcome {
        let Some(input) = self.input.clone() else {
            return Outcome::Closed;
        };
        if !self.mode(cx).can_send() {
            self.completion.close();
            return Outcome::Closed;
        }
        let (text, caret) = {
            let input = input.read(cx);
            (input.value(), input.cursor())
        };
        let catalog = self.catalog(cx);
        let recents = completion::recent_emotes(cx);
        let store = self.store.clone();
        let role = store.read(cx).role();
        let sources = completion::Sources {
            catalog: &catalog,
            recents: &recents,
            role,
        };
        self.completion
            .refresh(&text, caret, explicit, sources, || {
                recent_chatters(store.read(cx).messages().iter().map(|m| &**m), CHATTERS)
            })
    }

    /// The text changed (or Tab was pressed): updates the list; a lone candidate asked for by Tab
    /// is taken at once.
    pub(super) fn refresh_completion(
        &mut self,
        explicit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Outcome {
        let outcome = self.recompute_completion(explicit, cx);
        if outcome == Outcome::Single {
            self.accept_completion(Some(0), window, cx);
        }
        cx.notify();
        outcome
    }

    /// Called while rendering: a caret moved by the arrow keys or a click no longer belongs to the
    /// word the list was opened for.
    pub(super) fn sync_completion(&mut self, cx: &mut Context<Self>) {
        let (Some(popup), Some(input)) = (self.completion.popup(), &self.input) else {
            return;
        };
        if input.read(cx).cursor() != popup.trigger.range.end {
            self.recompute_completion(false, cx);
        }
    }

    /// ↑/↓ while the list is open. `false` when there is no list (the arrows recall history).
    pub(super) fn move_completion(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(popup) = self.completion.popup_mut() else {
            return false;
        };
        popup.move_by(delta);
        cx.notify();
        true
    }

    /// Puts candidate `index` (the highlighted one by default) in the box in place of the word.
    pub(super) fn accept_completion(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(popup), Some(input)) = (self.completion.popup(), self.input.clone()) else {
            return;
        };
        let index = index.unwrap_or_else(|| popup.selected());
        let text = input.read(cx).value();
        let caret = input.read(cx).cursor();
        let Some(edit) = popup.edit_for(index, text.get(caret..).unwrap_or("")) else {
            return;
        };
        if text.get(edit.range.clone()).is_none() {
            // The text moved on without the list noticing.
            self.completion.close();
            return;
        }
        let emote = match popup.items.get(index) {
            Some(Item::Emote(emote)) => Some(emote.name.to_string()),
            _ => None,
        };
        self.completion.close();
        input.update(cx, |input, cx| {
            input.set_selected_range(edit.range.clone(), cx);
            input.replace(edit.text, window, cx);
            input.focus(window, cx);
        });
        self.history.reset();
        if let Some(name) = emote {
            completion::remember_emote(&name, cx);
        }
        cx.notify();
    }

    /// Tab: takes the highlighted candidate, or opens the list for the word before the caret.
    /// `false` when there was nothing to do (the key then falls through to the input).
    pub(super) fn complete(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.mode(cx).can_send() {
            return false;
        }
        if self.completion.is_open() {
            self.accept_completion(None, window, cx);
            true
        } else {
            self.refresh_completion(true, window, cx) != Outcome::Closed
        }
    }

    /// Shift-Tab: one up in the list. `false` when there is no list.
    pub(super) fn complete_previous(&mut self, cx: &mut Context<Self>) -> bool {
        self.move_completion(-1, cx)
    }

    /// Whether a completion list is open (Esc closes it before it cancels a reply).
    pub(super) fn completion_is_open(&self) -> bool {
        self.completion.is_open()
    }

    /// Esc. `false` when there is no list: the key then falls through (to the input's own Escape,
    /// or a parent's such as cancelling a reply).
    pub(super) fn dismiss_completion(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.completion.is_open() {
            return false;
        }
        self.completion.dismiss();
        cx.notify();
        true
    }

    /// The input lost the focus: a list over it has nothing to complete into. (While the picker is
    /// open the focus is meant to be elsewhere.)
    pub(super) fn input_blurred(&mut self, cx: &mut Context<Self>) {
        if self.picker.is_none() && self.completion.is_open() {
            self.completion.close();
            cx.notify();
        }
    }

    /// A press outside the list.
    fn close_completion(&mut self, cx: &mut Context<Self>) {
        if self.completion.is_open() {
            self.completion.close();
            cx.notify();
        }
    }

    /// Asks Helix for the user's own emotes in this room when that has not been done (called while
    /// rendering; it only acts when the user, the room or the login's permissions changed).
    pub(super) fn request_own_emotes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let account = match self.account.read(cx).state() {
            AuthState::LoggedIn(account) => Some(account.clone()),
            _ => None,
        };
        let Some(account) = account else {
            self.emotes.forget(None);
            return;
        };
        if account.missing_scopes.iter().any(|s| s == OWN_EMOTES_SCOPE) {
            self.emotes
                .forget(Some("Log in again to use your Twitch emotes."));
            return;
        }
        let Some(room_id) = self.store.read(cx).room().room_id.clone() else {
            return;
        };
        let key = Key {
            user_id: account.user_id,
            room_id,
        };
        if !self.emotes.begin(&key) {
            return;
        }
        let Some(result) = self
            .live
            .user_emotes(key.user_id.clone(), key.room_id.clone(), cx)
        else {
            self.emotes.finish(
                &key,
                Err("Your Twitch emotes are not available.".to_owned()),
            );
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            // A closed channel means the runtime is stopping; nothing to show.
            let Ok(outcome) = result.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.own_emotes_loaded(&key, outcome, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn own_emotes_loaded(
        &mut self,
        key: &Key,
        outcome: Result<Vec<UserEmote>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.emotes.finish(key, outcome);
        if let Some(picker) = self.picker.clone() {
            let catalog = self.catalog(cx);
            let note = self.emotes.note();
            picker.update(cx, |picker, cx| picker.update_data(catalog, note, cx));
        }
        // The new emotes may belong in a list that is open.
        if self.completion.is_open() {
            self.refresh_completion(false, window, cx);
        }
        cx.notify();
    }

    /// ⌘E / the button.
    pub(super) fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            self.close_picker(window, cx);
        } else if self.mode(cx).can_send() {
            self.open_picker(window, cx);
        }
    }

    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.completion.close();
        // Opening the picker is a good time to ask again after a failure.
        self.emotes.retry();
        self.request_own_emotes(window, cx);
        let catalog = self.catalog(cx);
        let recents = completion::recent_emotes(cx);
        let note = self.emotes.note();
        let button = self.picker_button_bounds.clone();
        let picker = cx.new(|cx| EmotePicker::new(catalog, recents, note, button, window, cx));
        self._picker_subscription = Some(cx.subscribe_in(
            &picker,
            window,
            |this, _, event: &PickerEvent, window, cx| match event {
                PickerEvent::Picked(emote) => {
                    this.close_picker(window, cx);
                    this.insert_emote(emote, window, cx);
                }
                PickerEvent::Dismissed => this.close_picker(window, cx),
            },
        ));
        self.picker = Some(picker);
        cx.notify();
    }

    pub(super) fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.take().is_none() {
            return;
        }
        self._picker_subscription = None;
        if let Some(input) = &self.input {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    /// Inserts `emote` at the caret (or over the selection) with a space either side as needed.
    fn insert_emote(&mut self, emote: &Emote, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else {
            return;
        };
        let (text, selection) = {
            let input = input.read(cx);
            (input.value(), input.selected_range())
        };
        let word = word_insertion(&text, selection, &emote.name);
        input.update(cx, |input, cx| {
            input.replace(word, window, cx);
            input.focus(window, cx);
        });
        self.history.reset();
        completion::remember_emote(&emote.name, cx);
        cx.notify();
    }

    /// The list or the picker, positioned above the box. Empty when neither is open.
    pub(super) fn overlays(
        &mut self,
        tokens: &Tokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let place = |child: AnyElement| {
            deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .offset(point(px(OVERLAY_INSET), px(-OVERLAY_GAP)))
                    .child(child),
            )
            .into_any_element()
        };
        if let Some(picker) = &self.picker {
            return vec![place(picker.clone().into_any_element())];
        }
        let light = tokens.light;
        let (owner, composer) = (cx.entity_id(), cx.entity());
        let outside = cx.entity();
        self.completion
            .render(
                tokens,
                light,
                owner,
                window,
                move |index, window, cx| {
                    composer.update(cx, |this, cx| {
                        this.accept_completion(Some(index), window, cx);
                    });
                },
                move |_, cx| outside.update(cx, |this, cx| this.close_completion(cx)),
            )
            .map(place)
            .into_iter()
            .collect()
    }

    /// The button that opens the picker; none while the box cannot be used. Its bounds are kept
    /// so a press on it does not count as "outside" for the open picker (it toggles instead).
    pub(super) fn picker_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.mode(cx).can_send().then(|| {
            let bounds = self.picker_button_bounds.clone();
            div()
                .relative()
                .child(
                    Button::new("composer-emotes")
                        .ghost()
                        .small()
                        .label("🙂")
                        .accessibility_label(
                            match crate::keymap::shortcut_of(
                                crate::completion::ComposerToggleEmotePicker,
                                cx,
                            ) {
                                Some(keys) => format!("Emote picker ({keys})"),
                                None => "Emote picker".to_owned(),
                            },
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.toggle_picker(window, cx)),
                        ),
                )
                .child(
                    canvas(move |b, _, _| bounds.set(b), |_, (), _, _| {})
                        .absolute()
                        .inset_0(),
                )
                .into_any_element()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui_kit::component::input::{Input, InputState};
    use gpui_kit::{
        Entity, InteractiveElement as _, IntoElement, Render, TestAppContext, VisualTestContext,
    };

    use crate::actions::{COMPOSER_CONTEXT, ComposerHistoryNext, ComposerHistoryPrevious};
    use crate::completion::{
        ComposerComplete, ComposerCompletePrevious, ComposerCompletionDismiss,
    };

    use super::*;
    use crate::composer::Escape;

    /// The composer's key wiring without its backend: the same context, the same actions, the same
    /// "act only when there is a list, else propagate" rule as `Composer::render`, around a real
    /// Kit input. `list_open` stands for a completion list being open.
    struct Probe {
        input: Entity<InputState>,
        log: Rc<RefCell<Vec<&'static str>>>,
        list_open: bool,
        /// Reply mode (PR 3.4): the box is replying to a message.
        replying: bool,
    }

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .key_context(COMPOSER_CONTEXT)
                .on_action(cx.listener(|this, _: &ComposerHistoryPrevious, _, _| {
                    this.log.borrow_mut().push("history-previous");
                }))
                .on_action(cx.listener(|this, _: &ComposerHistoryNext, _, _| {
                    this.log.borrow_mut().push("history-next");
                }))
                .on_action(acts_typed::<ComposerComplete>(cx, "complete"))
                .on_action(acts_typed::<ComposerCompletePrevious>(
                    cx,
                    "complete-previous",
                ))
                // Esc exactly as `Composer::render` routes it (`Escape::of`).
                .on_action(cx.listener(
                    |this, _: &ComposerCompletionDismiss, _, cx| match Escape::of(
                        this.list_open,
                        this.replying,
                    ) {
                        Escape::CloseList => {
                            this.log.borrow_mut().push("close-list");
                            this.list_open = false;
                        }
                        Escape::CancelReply => {
                            this.log.borrow_mut().push("cancel-reply");
                            this.replying = false;
                        }
                        Escape::Pass => {
                            this.log.borrow_mut().push("dismiss");
                            cx.propagate();
                        }
                    },
                ))
                .child(Input::new(&self.input))
        }
    }

    fn acts_typed<A: gpui_kit::Action>(
        cx: &mut Context<Probe>,
        name: &'static str,
    ) -> impl Fn(&A, &mut Window, &mut gpui_kit::App) + 'static {
        cx.listener(move |this: &mut Probe, _: &A, _, cx| {
            this.log.borrow_mut().push(name);
            if !this.list_open {
                cx.propagate();
            }
        })
    }

    fn probe(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Probe>,
        Rc<RefCell<Vec<&'static str>>>,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(crate::platform::default_bindings());
        });
        let log = Rc::new(RefCell::new(Vec::new()));
        let shared = log.clone();
        let (probe, cx) = cx.add_window_view(move |window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).clean_on_escape());
            input.update(cx, |input, cx| input.focus(window, cx));
            Probe {
                input,
                log: shared,
                list_open: false,
                replying: false,
            }
        });
        cx.run_until_parked();
        (probe, log, cx)
    }

    #[gpui_kit::test]
    fn tab_escape_and_the_arrows_in_the_input_reach_the_composer_actions(cx: &mut TestAppContext) {
        let (probe, log, cx) = probe(cx);
        probe.update(cx, |p, _| p.list_open = true);
        cx.simulate_keystrokes("tab shift-tab escape up down");
        assert_eq!(
            *log.borrow(),
            [
                "complete",
                "complete-previous",
                "close-list",
                "history-previous",
                "history-next"
            ]
        );
    }

    #[gpui_kit::test]
    fn escape_with_no_list_open_falls_through_to_the_input(cx: &mut TestAppContext) {
        let (probe, log, cx) = probe(cx);
        cx.simulate_input("abc");
        let text =
            |cx: &mut VisualTestContext| probe.read_with(cx, |p, cx| p.input.read(cx).value());
        assert_eq!(text(cx), "abc");

        // A list is open: Esc is the list's, and the input keeps its text.
        probe.update(cx, |p, _| p.list_open = true);
        cx.simulate_keystrokes("escape");
        assert_eq!(text(cx), "abc");

        // No list: the action propagates and the input's own Escape (clean on escape) runs.
        probe.update(cx, |p, _| p.list_open = false);
        cx.simulate_keystrokes("escape");
        assert_eq!(*log.borrow(), ["close-list", "dismiss"]);
        assert_eq!(text(cx), "", "the input saw the Escape");
    }

    #[gpui_kit::test]
    fn escape_closes_the_list_then_cancels_the_reply_then_falls_through(cx: &mut TestAppContext) {
        let (probe, log, cx) = probe(cx);
        cx.simulate_input("@Bob hi");
        let text =
            |cx: &mut VisualTestContext| probe.read_with(cx, |p, cx| p.input.read(cx).value());
        probe.update(cx, |p, _| {
            p.list_open = true;
            p.replying = true;
        });
        // First Esc: the list, and the reply stays.
        cx.simulate_keystrokes("escape");
        assert!(probe.read_with(cx, |p, _| p.replying && !p.list_open));
        // Second: the reply. The text is left alone (the input never saw this Esc).
        cx.simulate_keystrokes("escape");
        assert!(probe.read_with(cx, |p, _| !p.replying));
        assert_eq!(text(cx), "@Bob hi");
        // Third: nothing to close or cancel, so it propagates to the input (clean on escape).
        cx.simulate_keystrokes("escape");
        assert_eq!(*log.borrow(), ["close-list", "cancel-reply", "dismiss"]);
        assert_eq!(text(cx), "");
    }

    #[test]
    fn the_escape_order_is_list_then_reply_then_pass() {
        assert_eq!(Escape::of(true, true), Escape::CloseList);
        assert_eq!(Escape::of(true, false), Escape::CloseList);
        assert_eq!(Escape::of(false, true), Escape::CancelReply);
        assert_eq!(Escape::of(false, false), Escape::Pass);
    }
}
