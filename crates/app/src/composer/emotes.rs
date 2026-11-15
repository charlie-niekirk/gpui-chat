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
    Anchor, AnyElement, Context, IntoElement, ParentElement as _, Window, anchored, deferred,
    point, px,
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
        self.completion
            .refresh(&text, caret, explicit, &catalog, &recents, || {
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
    ) {
        if self.recompute_completion(explicit, cx) == Outcome::Single {
            self.accept_completion(Some(0), window, cx);
        }
        cx.notify();
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
    pub(super) fn complete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.mode(cx).can_send() {
            return;
        }
        if self.completion.is_open() {
            self.accept_completion(None, window, cx);
        } else {
            self.refresh_completion(true, window, cx);
        }
    }

    /// Shift-Tab: one up in the list.
    pub(super) fn complete_previous(&mut self, cx: &mut Context<Self>) {
        self.move_completion(-1, cx);
    }

    /// Esc.
    pub(super) fn dismiss_completion(&mut self, cx: &mut Context<Self>) {
        if self.completion.is_open() {
            self.completion.dismiss();
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
        let picker = cx.new(|cx| EmotePicker::new(catalog, recents, note, window, cx));
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
        let light = tokens.main_bg.l > 0.5;
        let (owner, composer) = (cx.entity_id(), cx.entity());
        self.completion
            .render(tokens, light, owner, window, move |index, window, cx| {
                composer.update(cx, |this, cx| {
                    this.accept_completion(Some(index), window, cx)
                });
            })
            .map(place)
            .into_iter()
            .collect()
    }

    /// The button that opens the picker; none while the box cannot be used.
    pub(super) fn picker_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.mode(cx).can_send().then(|| {
            Button::new("composer-emotes")
                .ghost()
                .small()
                .label("🙂")
                .accessibility_label(format!(
                    "Emote picker ({})",
                    crate::platform::EMOTE_PICKER_LABEL
                ))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_picker(window, cx)))
                .into_any_element()
        })
    }
}
