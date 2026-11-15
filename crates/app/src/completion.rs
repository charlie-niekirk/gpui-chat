//! The completion list over the input box (PR 3.3): `:emote`, `@user`, `/command` and Tab.
//!
//! What the text asks for, how candidates rank and what accepting does to the text live in
//! `gpui_chat_core::complete` (unit-tested there). This module is the state of the list ([`Popup`]:
//! the candidates, the selection, the scroll window), the lists the candidates come from
//! ([`candidates`]) and its drawing; [`crate::composer::Composer`] owns one [`Autocomplete`] and
//! routes keys to it.
//!
//! Keys, while the list is open: ↑/↓ move, Tab or Enter accept, Shift-Tab moves up, Esc closes it
//! (and it stays closed until the word under the caret is another one). Tab on a plain word opens
//! the list for it as emotes; with one candidate it accepts at once.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::{
    COMMANDS, Chatter, CommandInfo, Edit, Emote, Recents, Trigger, TriggerKind, apply,
    find_trigger, rank,
};
use gpui_chat_emotes::EmoteCatalog;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, EntityId, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, Styled as _, Window, div, px,
};
use gpui_kit::{App, KeyBinding, actions};

use crate::emote_thumb;
use crate::images::FrameImages;
use crate::theme::Tokens;

/// Most candidates kept for a query.
const LIMIT: usize = 50;
/// Rows the list shows at once; it scrolls past them.
pub const VISIBLE_ROWS: usize = 8;
/// Row height and the emote thumbnail inside it.
const ROW_HEIGHT: f32 = 30.0;
const THUMB: f32 = 22.0;
/// Most recent chatters gathered from the message store for `@` completion (a 1 000-line store
/// has far fewer distinct ones).
pub const CHATTERS: usize = 400;

actions!(
    gpui_chat,
    [
        ComposerComplete,
        ComposerCompletePrevious,
        ComposerCompletionDismiss,
        ComposerToggleEmotePicker
    ]
);

/// Key bindings of the completion list and the picker, scoped to the text input of a composer
/// (`ChatComposer > Input`) so they win over the input's own Tab / Escape. Registered after the
/// composer's history bindings; the picker's come last (see `emote_picker::bindings`).
pub fn bindings() -> Vec<KeyBinding> {
    let context = Some("ChatComposer > Input");
    let mut bindings = vec![
        KeyBinding::new("tab", ComposerComplete, context),
        KeyBinding::new("shift-tab", ComposerCompletePrevious, context),
        KeyBinding::new("escape", ComposerCompletionDismiss, context),
    ];
    bindings.extend(crate::emote_picker::bindings());
    bindings
}

/// The emotes used last, shared by every input box and kept in `state.toml` (`[emotes] recent`).
struct RecentEmotes(Recents);

impl gpui_kit::Global for RecentEmotes {}

/// The recently used emotes, newest first (read from `state.toml` on first use).
pub fn recent_emotes(cx: &mut App) -> Recents {
    if cx.try_global::<RecentEmotes>().is_none() {
        let saved = crate::state::load().emotes.unwrap_or_default();
        cx.set_global(RecentEmotes(Recents::from_names(saved.recent)));
    }
    cx.global::<RecentEmotes>().0.clone()
}

/// The newest recents list, waiting to be written. One writer at a time takes it *and* writes it
/// under this lock, so saves cannot land out of order: whichever runs last writes the newest list.
#[cfg(not(test))]
static PENDING_SAVE: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// `name` was just inserted: it moves to the front, and the list is saved.
pub fn remember_emote(name: &str, cx: &mut App) {
    let mut recents = recent_emotes(cx);
    recents.push(name);
    cx.set_global(RecentEmotes(recents.clone()));
    // Tests do not write the real state file.
    #[cfg(not(test))]
    {
        *PENDING_SAVE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(recents.names().to_vec());
        cx.background_executor()
            .spawn(async move {
                let mut pending = PENDING_SAVE
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(recent) = pending.take() {
                    crate::state::save_emotes(crate::state::SavedEmotes { recent });
                }
            })
            .detach();
    }
    #[cfg(test)]
    drop(recents);
}

/// One candidate.
#[derive(Debug, Clone)]
pub enum Item {
    Emote(Arc<Emote>),
    User(Chatter),
    Command(&'static CommandInfo),
}

impl Item {
    /// What is inserted (and matched), without the sigil.
    pub fn text(&self) -> &str {
        match self {
            Self::Emote(emote) => &emote.name,
            Self::User(chatter) => &chatter.display_name,
            Self::Command(command) => command.name,
        }
    }
}

/// The candidates for `trigger`, best first. `chatters` (newest speaker first) is only asked for
/// by an `@` trigger.
pub fn candidates(
    trigger: &Trigger,
    catalog: &EmoteCatalog,
    recents: &Recents,
    chatters: impl FnOnce() -> Vec<Chatter>,
) -> Vec<Item> {
    let query = trigger.query.as_str();
    match trigger.kind {
        TriggerKind::Emote => {
            let all = catalog.all();
            rank(
                query,
                all,
                |emote| &emote.name,
                |emote| recents.rank_of(&emote.name),
                LIMIT,
            )
            .into_iter()
            .map(|i| Item::Emote(all[i].clone()))
            .collect()
        }
        TriggerKind::User => {
            let chatters = chatters();
            // What people type is the login; a display name in another script is matched as typed.
            let ascii = query.is_ascii();
            rank(
                query,
                &chatters,
                |chatter| {
                    if ascii {
                        &chatter.login
                    } else {
                        &chatter.display_name
                    }
                },
                |chatter| chatters.iter().position(|c| c.login == chatter.login),
                LIMIT,
            )
            .into_iter()
            .map(|i| Item::User(chatters[i].clone()))
            .collect()
        }
        TriggerKind::Command => rank(query, COMMANDS, |command| command.name, |_| None, LIMIT)
            .into_iter()
            .map(|i| Item::Command(&COMMANDS[i]))
            .collect(),
    }
}

/// An open list.
#[derive(Debug, Clone)]
pub struct Popup {
    pub trigger: Trigger,
    pub items: Vec<Item>,
    /// Opened by Tab: it stays a list of emotes for the plain word while that is edited.
    pub explicit: bool,
    selected: usize,
    /// First row shown.
    top: usize,
    /// The user moved through the list (arrow or Shift-Tab): they chose from it, so Enter takes
    /// the highlighted candidate instead of sending the message as typed.
    moved: bool,
}

impl Popup {
    fn new(trigger: Trigger, items: Vec<Item>, explicit: bool) -> Self {
        Self {
            trigger,
            items,
            explicit,
            selected: 0,
            top: 0,
            moved: false,
        }
    }

    /// Whether Enter should take the highlighted candidate: the list was asked for with Tab or the
    /// user has moved through it. A list that merely opened as they typed (`gg @bob⏎`) does not
    /// capture Enter.
    pub fn engaged(&self) -> bool {
        self.explicit || self.moved
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.items.get(self.selected)
    }

    /// Moves the selection by `delta` rows, wrapping around, and keeps it in the shown window.
    pub fn move_by(&mut self, delta: isize) {
        let len = self.items.len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.moved = true;
        self.scroll_to_selection();
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
            self.scroll_to_selection();
        }
    }

    fn scroll_to_selection(&mut self) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + VISIBLE_ROWS {
            self.top = self.selected + 1 - VISIBLE_ROWS;
        }
    }

    /// The rows shown: `(index, item)`.
    pub fn window(&self) -> impl Iterator<Item = (usize, &Item)> {
        self.items
            .iter()
            .enumerate()
            .skip(self.top)
            .take(VISIBLE_ROWS)
    }

    /// The edit that accepting `index` makes to a text whose part after the caret is `rest`.
    pub fn edit_for(&self, index: usize, rest: &str) -> Option<Edit> {
        let item = self.items.get(index)?;
        Some(apply(&self.trigger, item.text(), rest))
    }
}

/// What a refresh found.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to complete: no list.
    Closed,
    /// The list is open (or updated).
    Open,
    /// An explicit request (Tab) with exactly one candidate: take it, no list.
    Single,
}

/// The completion state of one input box.
#[derive(Default)]
pub struct Autocomplete {
    popup: Option<Popup>,
    /// Where the user closed a list with Esc: the word starting here stays closed.
    dismissed: Option<usize>,
    /// The pins of the thumbnails painted last frame.
    frame: Rc<RefCell<FrameImages>>,
}

impl Autocomplete {
    pub fn popup(&self) -> Option<&Popup> {
        self.popup.as_ref()
    }

    pub fn popup_mut(&mut self) -> Option<&mut Popup> {
        self.popup.as_mut()
    }

    pub fn is_open(&self) -> bool {
        self.popup.is_some()
    }

    /// Whether Enter belongs to the list (see [`Popup::engaged`]).
    pub fn engaged(&self) -> bool {
        self.popup.as_ref().is_some_and(Popup::engaged)
    }

    pub fn close(&mut self) {
        self.popup = None;
    }

    /// Esc: closes the list and keeps it closed while the caret stays in the same word.
    pub fn dismiss(&mut self) {
        if let Some(popup) = self.popup.take() {
            self.dismissed = Some(popup.trigger.range.start);
        }
    }

    /// Recomputes the list for `text` with the caret at `caret`. `explicit` is a Tab press.
    pub fn refresh(
        &mut self,
        text: &str,
        caret: usize,
        explicit: bool,
        catalog: &EmoteCatalog,
        recents: &Recents,
        chatters: impl FnOnce() -> Vec<Chatter>,
    ) -> Outcome {
        // A list Tab opened keeps treating the plain word as emotes while it is typed on.
        let sticky = !explicit
            && self.popup.as_ref().is_some_and(|popup| {
                popup.explicit && popup.trigger.range.start == word_start(text, caret)
            });
        let trigger = find_trigger(text, caret, explicit)
            .or_else(|| sticky.then(|| find_trigger(text, caret, true)).flatten());
        let Some(trigger) = trigger else {
            self.popup = None;
            self.dismissed = None;
            return Outcome::Closed;
        };
        if !explicit && self.dismissed == Some(trigger.range.start) {
            self.popup = None;
            return Outcome::Closed;
        }
        let items = candidates(&trigger, catalog, recents, chatters);
        if items.is_empty() {
            self.popup = None;
            return Outcome::Closed;
        }
        if explicit && items.len() == 1 {
            self.popup = Some(Popup::new(trigger, items, true));
            return Outcome::Single;
        }
        self.dismissed = None;
        let explicit = explicit || sticky;
        // Keep the selection on the same candidate when the list is only narrowed.
        let previous = self
            .popup
            .as_ref()
            .and_then(Popup::selected_item)
            .map(|item| item.text().to_owned());
        let mut popup = Popup::new(trigger, items, explicit);
        popup.moved = self.popup.as_ref().is_some_and(|p| p.moved);
        if let Some(previous) = previous
            && let Some(index) = popup.items.iter().position(|i| i.text() == previous)
        {
            popup.select(index);
        }
        self.popup = Some(popup);
        Outcome::Open
    }

    /// The list as an element, or `None` when closed. `pick` is called with the row clicked.
    pub fn render(
        &self,
        tokens: &Tokens,
        light: bool,
        owner: EntityId,
        window: &mut Window,
        pick: impl Fn(usize, &mut Window, &mut gpui_kit::App) + Clone + 'static,
        outside: impl Fn(&mut Window, &mut gpui_kit::App) + 'static,
    ) -> Option<AnyElement> {
        let popup = self.popup.as_ref()?;
        self.frame.borrow_mut().begin_frame();
        let placeholder = tokens.text_muted.opacity(0.15);
        let rows: Vec<AnyElement> = popup
            .window()
            .map(|(index, item)| {
                let selected = index == popup.selected();
                let pick = pick.clone();
                let leading = match item {
                    Item::Emote(emote) => emote_thumb::thumb(
                        emote,
                        THUMB,
                        placeholder,
                        light,
                        self.frame.clone(),
                        owner,
                        window,
                    )
                    .into_any_element(),
                    Item::User(_) => div()
                        .w(px(THUMB))
                        .text_color(tokens.text_muted)
                        .child("@")
                        .into_any_element(),
                    Item::Command(_) => div()
                        .w(px(THUMB))
                        .text_color(tokens.text_muted)
                        .child("/")
                        .into_any_element(),
                };
                let (name, detail) = match item {
                    Item::Emote(emote) => {
                        (emote.name.to_string(), emote.provider.name().to_owned())
                    }
                    Item::User(chatter) => (
                        chatter.display_name.clone(),
                        if chatter.display_name.to_lowercase() == chatter.login {
                            String::new()
                        } else {
                            chatter.login.clone()
                        },
                    ),
                    Item::Command(command) => {
                        (command.usage.to_owned(), command.summary.to_owned())
                    }
                };
                div()
                    .id(("completion-row", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(ROW_HEIGHT))
                    .px_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(tokens.accent.opacity(0.28)))
                    // On press, not release: the input keeps focus and the text is edited at once.
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        pick(index, window, cx);
                        cx.stop_propagation();
                    })
                    .child(leading)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_weight(if selected {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(tokens.text_muted)
                            .child(detail),
                    )
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .id("completion-list")
                .occlude()
                // A press anywhere else (the chat, another control) closes it.
                .on_mouse_down_out(move |_, window, cx| outside(window, cx))
                .flex()
                .flex_col()
                .w(px(320.0))
                .p_1()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .bg(tokens.window_bg)
                .shadow_md()
                .text_sm()
                .text_color(tokens.text)
                .children(rows)
                .into_any_element(),
        )
    }
}

/// Start (byte offset) of the word that ends at `caret`.
fn word_start(text: &str, caret: usize) -> usize {
    text.get(..caret)
        .and_then(|before| before.char_indices().rev().find(|(_, c)| c.is_whitespace()))
        .map_or(0, |(i, c)| i + c.len_utf8())
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{EmoteProvider, ImageUrls};
    use gpui_chat_emotes::{EmoteScope, EmoteSet, SetEmote};

    use super::*;

    fn emote(provider: EmoteProvider, name: &str) -> Arc<Emote> {
        Arc::new(Emote {
            id: format!("{}-{name}", provider.name()).into(),
            name: name.into(),
            provider,
            urls: ImageUrls::new(),
            animated: false,
            zero_width: false,
            width: None,
            height: None,
        })
    }

    fn catalog(names: &[&str]) -> EmoteCatalog {
        let set = EmoteSet::new(
            EmoteProvider::SevenTv,
            EmoteScope::Global,
            names
                .iter()
                .map(|name| SetEmote {
                    emote: emote(EmoteProvider::SevenTv, name),
                    original_name: None,
                    owner: None,
                })
                .collect(),
        );
        EmoteCatalog::build(&[], &[Arc::new(set)])
    }

    fn chatters(names: &[&str]) -> Vec<Chatter> {
        names
            .iter()
            .map(|name| Chatter {
                login: name.to_lowercase(),
                display_name: (*name).to_owned(),
            })
            .collect()
    }

    fn refresh(
        auto: &mut Autocomplete,
        text: &str,
        explicit: bool,
        catalog: &EmoteCatalog,
    ) -> Outcome {
        auto.refresh(
            text,
            text.len(),
            explicit,
            catalog,
            &Recents::default(),
            || chatters(&["Zed", "Zoe", "Bob"]),
        )
    }

    fn texts(auto: &Autocomplete) -> Vec<String> {
        auto.popup()
            .map(|popup| popup.items.iter().map(|i| i.text().to_owned()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_colon_word_lists_emotes_and_accepting_inserts_the_name() {
        let catalog = catalog(&["Kappa", "KappaPride", "monkaW", "OMEGALUL"]);
        let mut auto = Autocomplete::default();
        assert_eq!(
            refresh(&mut auto, "hi :kap", false, &catalog),
            Outcome::Open
        );
        assert_eq!(texts(&auto), ["Kappa", "KappaPride"]);
        let popup = auto.popup().unwrap();
        let edit = popup.edit_for(0, "").unwrap();
        assert_eq!(edit.applied_to("hi :kap"), ("hi Kappa ".to_owned(), 9));
        // Typing on narrows the list and keeps the choice where it can.
        auto.popup_mut().unwrap().move_by(1);
        assert_eq!(
            refresh(&mut auto, "hi :kappap", false, &catalog),
            Outcome::Open
        );
        assert_eq!(texts(&auto), ["KappaPride"]);
        // No match closes it.
        assert_eq!(
            refresh(&mut auto, "hi :zzz", false, &catalog),
            Outcome::Closed
        );
        assert!(!auto.is_open());
    }

    #[test]
    fn an_at_sign_lists_the_recent_chatters_newest_first() {
        let catalog = catalog(&[]);
        let mut auto = Autocomplete::default();
        assert_eq!(refresh(&mut auto, "@", false, &catalog), Outcome::Open);
        assert_eq!(texts(&auto), ["Zed", "Zoe", "Bob"]);
        assert_eq!(refresh(&mut auto, "yo @zo", false, &catalog), Outcome::Open);
        assert_eq!(texts(&auto), ["Zoe"]);
        let edit = auto.popup().unwrap().edit_for(0, "").unwrap();
        assert_eq!(edit.applied_to("yo @zo").0, "yo @Zoe ");
        // A name in another script is matched as typed.
        let mut auto = Autocomplete::default();
        let outcome = auto.refresh(
            "@ほ",
            "@ほ".len(),
            false,
            &catalog,
            &Recents::default(),
            || {
                vec![Chatter {
                    login: "hoge".into(),
                    display_name: "ほげ".into(),
                }]
            },
        );
        assert_eq!(outcome, Outcome::Open);
    }

    #[test]
    fn a_slash_at_the_start_lists_the_commands() {
        let catalog = catalog(&[]);
        let mut auto = Autocomplete::default();
        assert_eq!(refresh(&mut auto, "/", false, &catalog), Outcome::Open);
        assert_eq!(texts(&auto), ["me"]);
        let edit = auto.popup().unwrap().edit_for(0, "").unwrap();
        assert_eq!(edit.applied_to("/").0, "/me ");
        assert_eq!(refresh(&mut auto, "/x", false, &catalog), Outcome::Closed);
        // Not at the start: an ordinary word.
        assert_eq!(
            refresh(&mut auto, "hi /me", false, &catalog),
            Outcome::Closed
        );
    }

    #[test]
    fn tab_opens_a_list_for_a_plain_word_or_takes_the_only_candidate() {
        let catalog = catalog(&["Kappa", "KappaPride", "monkaW"]);
        let mut auto = Autocomplete::default();
        // Not by itself ...
        assert_eq!(refresh(&mut auto, "Kap", false, &catalog), Outcome::Closed);
        // ... but on Tab.
        assert_eq!(refresh(&mut auto, "Kap", true, &catalog), Outcome::Open);
        assert_eq!(texts(&auto), ["Kappa", "KappaPride"]);
        // Typing on keeps the list for the word.
        assert_eq!(refresh(&mut auto, "Kapp", false, &catalog), Outcome::Open);
        assert_eq!(texts(&auto), ["Kappa", "KappaPride"]);
        // A space ends the word and the list.
        assert_eq!(
            refresh(&mut auto, "Kappa ", false, &catalog),
            Outcome::Closed
        );
        // A single candidate is taken at once.
        let mut auto = Autocomplete::default();
        assert_eq!(refresh(&mut auto, "mon", true, &catalog), Outcome::Single);
        let edit = auto.popup().unwrap().edit_for(0, "").unwrap();
        assert_eq!(edit.applied_to("mon").0, "monkaW ");
        // Nothing to complete after a space.
        assert_eq!(refresh(&mut auto, "mon ", true, &catalog), Outcome::Closed);
    }

    #[test]
    fn enter_belongs_to_a_list_only_once_the_user_chose_from_it() {
        let catalog = catalog(&["Kappa", "KappaPride"]);
        let mut auto = Autocomplete::default();
        // Opened by itself while typing: Enter still sends what was typed.
        refresh(&mut auto, "gg :kap", false, &catalog);
        assert!(auto.is_open());
        assert!(!auto.engaged());
        refresh(&mut auto, "gg :kapp", false, &catalog);
        assert!(!auto.engaged(), "typing on does not engage it");
        // An arrow does, and typing on keeps it.
        auto.popup_mut().unwrap().move_by(1);
        assert!(auto.engaged());
        refresh(&mut auto, "gg :kappa", false, &catalog);
        assert!(auto.engaged());
        // Tab asked for it: engaged from the start.
        let mut auto = Autocomplete::default();
        refresh(&mut auto, "Kap", true, &catalog);
        assert!(auto.engaged());
        // Closed: nothing to engage.
        auto.close();
        assert!(!auto.engaged());
    }

    #[test]
    fn escape_keeps_the_list_closed_for_that_word_only() {
        let catalog = catalog(&["Kappa", "KappaPride"]);
        let mut auto = Autocomplete::default();
        refresh(&mut auto, ":kap", false, &catalog);
        auto.dismiss();
        assert!(!auto.is_open());
        assert_eq!(
            refresh(&mut auto, ":kapp", false, &catalog),
            Outcome::Closed
        );
        // The next word asks again.
        assert_eq!(
            refresh(&mut auto, ":kapp :kap", false, &catalog),
            Outcome::Open
        );
        // Tab opens it regardless.
        auto.dismiss();
        assert_eq!(refresh(&mut auto, ":kap", true, &catalog), Outcome::Open);
    }

    #[test]
    fn the_selection_wraps_and_the_window_follows_it() {
        let names: Vec<String> = (0..20).map(|i| format!("Emote{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let catalog = catalog(&refs);
        let mut auto = Autocomplete::default();
        refresh(&mut auto, ":emote", false, &catalog);
        let popup = auto.popup_mut().unwrap();
        assert_eq!(popup.window().count(), VISIBLE_ROWS);
        popup.move_by(-1);
        assert_eq!(popup.selected(), 19);
        let shown: Vec<usize> = popup.window().map(|(i, _)| i).collect();
        assert_eq!(shown, (12..20).collect::<Vec<_>>());
        popup.move_by(1);
        assert_eq!(popup.selected(), 0);
        assert_eq!(popup.window().next().map(|(i, _)| i), Some(0));
        for _ in 0..9 {
            popup.move_by(1);
        }
        assert_eq!(popup.selected(), 9);
        assert!(popup.window().any(|(i, _)| i == 9));
    }

    #[test]
    fn recently_used_emotes_come_first() {
        let catalog = catalog(&["KappaPride", "Kappa", "KappaHD"]);
        let mut recents = Recents::default();
        recents.push("KappaHD");
        let mut auto = Autocomplete::default();
        auto.refresh(":kap", 4, false, &catalog, &recents, Vec::new);
        assert_eq!(texts(&auto), ["KappaHD", "Kappa", "KappaPride"]);
    }
}
