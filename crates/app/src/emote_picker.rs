//! The emote picker (PR 3.3): a popover over the input box with a tab per provider, a search field
//! and the emotes used last, driven from the keyboard.
//!
//! Open it with the button next to the input or ⌘E (Ctrl-E elsewhere). The search field has the
//! focus: type to filter (a search covers every provider, best match first), ←/→/↑/↓ move through
//! the grid, Tab / Shift-Tab change tab, Enter inserts the highlighted emote and closes the picker,
//! Esc closes it. A click inserts too.
//!
//! The grid is a `uniform_list` of rows, so only what is on screen is built and its images
//! requested, however many emotes a channel has. Thumbnails go through [`crate::emote_thumb`].

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::{Emote, EmoteProvider, Recents, rank};
use gpui_chat_emotes::EmoteCatalog;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, ParentElement as _, Render,
    ScrollStrategy, SharedString, Styled as _, Subscription, UniformListScrollHandle, Window,
    actions, div, px, uniform_list,
};

use crate::emote_thumb;
use crate::images::{FrameImages, Images};
use crate::theme::Tokens;

actions!(
    gpui_chat,
    [
        EmotePickerLeft,
        EmotePickerRight,
        EmotePickerUp,
        EmotePickerDown,
        EmotePickerNextTab,
        EmotePickerPreviousTab,
        EmotePickerClose
    ]
);

/// Key context of the picker; its bindings are scoped to the search input inside it.
pub const PICKER_CONTEXT: &str = "EmotePicker";

/// Key bindings of the picker. Registered after the composer's (same depth, the later one wins), so
/// inside the picker ↑/↓ move through the grid instead of recalling sent lines.
pub fn bindings() -> Vec<KeyBinding> {
    let context = Some("EmotePicker > Input");
    vec![
        KeyBinding::new("left", EmotePickerLeft, context),
        KeyBinding::new("right", EmotePickerRight, context),
        KeyBinding::new("up", EmotePickerUp, context),
        KeyBinding::new("down", EmotePickerDown, context),
        KeyBinding::new("tab", EmotePickerNextTab, context),
        KeyBinding::new("shift-tab", EmotePickerPreviousTab, context),
        KeyBinding::new("escape", EmotePickerClose, context),
    ]
}

/// What the picker tells its owner.
pub enum PickerEvent {
    /// Insert this emote and close.
    Picked(Arc<Emote>),
    /// Close without inserting.
    Dismissed,
}

impl EventEmitter<PickerEvent> for EmotePicker {}

const COLUMNS: usize = 9;
const CELL: f32 = 38.0;
const THUMB: f32 = 30.0;
/// Rows of the grid on screen.
const GRID_ROWS: f32 = 6.5;
const WIDTH: f32 = COLUMNS as f32 * CELL + 16.0;
/// Most results of a search.
const SEARCH_LIMIT: usize = 300;

/// A tab of the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Recent,
    Provider(EmoteProvider),
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::Provider(provider) => provider.name(),
        }
    }
}

/// The tabs for a catalog: Recent, Twitch (always: it says why it is empty), then each third-party
/// provider that has emotes.
pub fn tabs_for(catalog: &EmoteCatalog) -> Vec<Tab> {
    let mut tabs = vec![Tab::Recent, Tab::Provider(EmoteProvider::Twitch)];
    tabs.extend(
        catalog
            .providers()
            .filter(|provider| *provider != EmoteProvider::Twitch)
            .map(Tab::Provider),
    );
    tabs
}

/// The tab to open on: Recent when there is anything in it, else the first provider with emotes.
pub fn first_tab(catalog: &EmoteCatalog, recents: &Recents) -> Tab {
    if recents
        .names()
        .iter()
        .any(|name| catalog.find(name).is_some())
    {
        return Tab::Recent;
    }
    catalog
        .providers()
        .next()
        .map_or(Tab::Provider(EmoteProvider::Twitch), Tab::Provider)
}

/// What the grid shows: a search (across every provider, best match first), else the tab.
pub fn results_for(
    catalog: &EmoteCatalog,
    recents: &Recents,
    tab: Tab,
    query: &str,
) -> Vec<Arc<Emote>> {
    let query = query.trim();
    if !query.is_empty() {
        let all = catalog.all();
        return rank(
            query,
            all,
            |emote| &emote.name,
            |emote| recents.rank_of(&emote.name),
            SEARCH_LIMIT,
        )
        .into_iter()
        .map(|i| all[i].clone())
        .collect();
    }
    match tab {
        Tab::Recent => recents
            .names()
            .iter()
            .filter_map(|name| catalog.find(name).cloned())
            .collect(),
        Tab::Provider(provider) => catalog.provider(provider).to_vec(),
    }
}

/// The grid index after a key: `delta` columns left/right or rows up/down, clamped to the list (no
/// wrap: a grid has edges). `columns` of an up/down move is the row length.
pub fn step(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as isize + delta).clamp(0, len as isize - 1) as usize
}

/// The picker view.
pub struct EmotePicker {
    input: Entity<InputState>,
    catalog: Arc<EmoteCatalog>,
    recents: Recents,
    tab: Tab,
    query: String,
    results: Vec<Arc<Emote>>,
    selected: usize,
    /// Why the Twitch tab may be short of emotes (still loading, failed, log in again).
    note: Option<SharedString>,
    scroll: UniformListScrollHandle,
    frame: Rc<RefCell<FrameImages>>,
    _search: Subscription,
}

impl EmotePicker {
    pub fn new(
        catalog: Arc<EmoteCatalog>,
        recents: Recents,
        note: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |input, cx| {
            input.set_placeholder("Search emotes", window, cx);
            input.focus(window, cx);
        });
        let search =
            cx.subscribe_in(
                &input,
                window,
                |this, input, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => {
                        this.query = input.read(cx).value().to_string();
                        this.recompute(cx);
                    }
                    InputEvent::PressEnter { .. } => this.accept(cx),
                    InputEvent::Focus | InputEvent::Blur => {}
                },
            );
        let tab = first_tab(&catalog, &recents);
        let mut picker = Self {
            input,
            catalog,
            recents,
            tab,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            note,
            scroll: UniformListScrollHandle::new(),
            frame: Rc::default(),
            _search: search,
        };
        picker.results = results_for(&picker.catalog, &picker.recents, picker.tab, &picker.query);
        picker
    }

    /// New data arrived while the picker is open (the user's emotes finished loading).
    pub fn update_data(
        &mut self,
        catalog: Arc<EmoteCatalog>,
        note: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.catalog = catalog;
        self.note = note;
        self.recompute(cx);
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        self.results = results_for(&self.catalog, &self.recents, self.tab, &self.query);
        self.selected = step(0, self.results.len(), 0);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    fn rows(&self) -> usize {
        self.results.len().div_ceil(COLUMNS)
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if index != self.selected && index < self.results.len() {
            self.selected = index;
            cx.notify();
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.selected = step(self.selected, self.results.len(), delta);
        self.scroll
            .scroll_to_item(self.selected / COLUMNS, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn change_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.searching() {
            return;
        }
        let tabs = tabs_for(&self.catalog);
        let at = tabs.iter().position(|tab| *tab == self.tab).unwrap_or(0);
        let next = if forward {
            (at + 1) % tabs.len()
        } else {
            (at + tabs.len() - 1) % tabs.len()
        };
        self.set_tab(tabs[next], cx);
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.recompute(cx);
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        if let Some(emote) = self.results.get(self.selected) {
            cx.emit(PickerEvent::Picked(emote.clone()));
        }
    }

    /// What to say in place of an empty grid.
    fn empty_message(&self) -> String {
        if self.searching() {
            return "No emotes match.".to_owned();
        }
        match self.tab {
            Tab::Recent => "Emotes you use show up here.".to_owned(),
            Tab::Provider(EmoteProvider::Twitch) => self
                .note
                .as_ref()
                .map_or_else(|| "No Twitch emotes.".to_owned(), ToString::to_string),
            Tab::Provider(provider) => format!("No {} emotes loaded.", provider.name()),
        }
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        tokens: Tokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let owner = cx.entity_id();
        let light = tokens.main_bg.l > 0.5;
        let placeholder = tokens.text_muted.opacity(0.15);
        range
            .map(|row| {
                let start = row * COLUMNS;
                let end = (start + COLUMNS).min(self.results.len());
                let cells: Vec<AnyElement> = (start..end)
                    .map(|index| {
                        let emote = &self.results[index];
                        div()
                            .id(("emote-cell", index))
                            .flex()
                            .items_center()
                            .justify_center()
                            .flex_none()
                            .size(px(CELL))
                            .rounded_md()
                            .cursor_pointer()
                            .when(index == self.selected, |cell| {
                                cell.bg(tokens.accent.opacity(0.3))
                            })
                            .on_mouse_move(
                                cx.listener(move |this, _, _, cx| this.select(index, cx)),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.selected = index;
                                    this.accept(cx);
                                }),
                            )
                            .child(emote_thumb::thumb(
                                emote,
                                THUMB,
                                placeholder,
                                light,
                                self.frame.clone(),
                                owner,
                                window,
                            ))
                            .into_any_element()
                    })
                    .collect();
                div()
                    .flex()
                    .flex_row()
                    .h(px(CELL))
                    .children(cells)
                    .into_any_element()
            })
            .collect()
    }

    fn tab_strip(&self, tokens: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = self.searching();
        let tabs = tabs_for(&self.catalog);
        div()
            .flex()
            .flex_row()
            .gap_1()
            .px_2()
            .pt_2()
            .children(tabs.into_iter().enumerate().map(|(i, tab)| {
                let active = !searching && tab == self.tab;
                div()
                    .id(("emote-tab", i))
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .text_xs()
                    .cursor_pointer()
                    .text_color(if active {
                        tokens.text
                    } else {
                        tokens.text_muted
                    })
                    .when(active, |t| {
                        t.bg(tokens.accent.opacity(0.3))
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(searching, |t| t.opacity(0.5))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            // Clicking a tab ends a search: it is where the user wants to look now.
                            if this.searching() {
                                let input = this.input.clone();
                                input.update(cx, |_, cx| cx.notify());
                            }
                            this.set_tab(tab, cx);
                        }),
                    )
                    .child(tab.label())
            }))
    }

    fn footer(&self, tokens: &Tokens) -> impl IntoElement {
        let text = match self.results.get(self.selected) {
            Some(emote) => format!(
                "{} · {}{}",
                emote.name,
                emote.provider.name(),
                if emote.zero_width {
                    " · zero-width"
                } else {
                    ""
                }
            ),
            None => self.empty_message(),
        };
        let note = self.note.clone().filter(|_| {
            self.tab == Tab::Provider(EmoteProvider::Twitch) && !self.results.is_empty()
        });
        div()
            .flex()
            .flex_row()
            .justify_between()
            .gap_2()
            .px_3()
            .pb_2()
            .text_xs()
            .text_color(tokens.text_muted)
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(text),
            )
            .children(note.map(|note| div().flex_none().child(note)))
    }
}

impl Render for EmotePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        self.frame.borrow_mut().begin_frame();
        Images::sweep(window, cx);
        let empty = self.results.is_empty();
        let message = empty.then(|| self.empty_message());
        let rows = self.rows();
        let grid = uniform_list(
            "emote-grid",
            rows,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                this.render_rows(range, tokens, window, cx)
            }),
        )
        .track_scroll(&self.scroll)
        .w_full()
        .h(px(CELL * GRID_ROWS));
        div()
            .id("emote-picker")
            .key_context(PICKER_CONTEXT)
            .occlude()
            .on_action(cx.listener(|this, _: &EmotePickerLeft, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &EmotePickerRight, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &EmotePickerUp, _, cx| {
                this.move_selection(-(COLUMNS as isize), cx);
            }))
            .on_action(cx.listener(|this, _: &EmotePickerDown, _, cx| {
                this.move_selection(COLUMNS as isize, cx);
            }))
            .on_action(cx.listener(|this, _: &EmotePickerNextTab, _, cx| this.change_tab(true, cx)))
            .on_action(
                cx.listener(|this, _: &EmotePickerPreviousTab, _, cx| this.change_tab(false, cx)),
            )
            .on_action(
                cx.listener(|_, _: &EmotePickerClose, _, cx| cx.emit(PickerEvent::Dismissed)),
            )
            .flex()
            .flex_col()
            .w(px(WIDTH))
            .rounded_lg()
            .border_1()
            .border_color(tokens.border)
            .bg(tokens.window_bg)
            .shadow_lg()
            .text_color(tokens.text)
            .child(self.tab_strip(&tokens, cx))
            .child(div().p_2().child(Input::new(&self.input)))
            .child(
                div()
                    .relative()
                    .px_2()
                    .child(grid)
                    .children(message.map(|message| {
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .text_color(tokens.text_muted)
                            .child(message)
                    })),
            )
            .child(self.footer(&tokens))
    }
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::ImageUrls;
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

    fn set(provider: EmoteProvider, names: &[&str]) -> Arc<EmoteSet> {
        Arc::new(EmoteSet::new(
            provider,
            EmoteScope::Global,
            names
                .iter()
                .map(|name| SetEmote {
                    emote: emote(provider, name),
                    original_name: None,
                    owner: None,
                })
                .collect(),
        ))
    }

    fn catalog() -> EmoteCatalog {
        EmoteCatalog::build(
            &[emote(EmoteProvider::Twitch, "Kappa")],
            &[
                set(EmoteProvider::SevenTv, &["OMEGALUL", "monkaW"]),
                set(EmoteProvider::Bttv, &["Kappa", "KEKW"]),
            ],
        )
    }

    fn names(emotes: &[Arc<Emote>]) -> Vec<&str> {
        emotes.iter().map(|e| &*e.name).collect()
    }

    #[test]
    fn the_tabs_are_recent_twitch_and_each_provider_that_has_emotes() {
        let tabs = tabs_for(&catalog());
        assert_eq!(
            tabs,
            [
                Tab::Recent,
                Tab::Provider(EmoteProvider::Twitch),
                Tab::Provider(EmoteProvider::SevenTv),
                Tab::Provider(EmoteProvider::Bttv),
            ]
        );
        // Twitch stays when the user's emotes are missing, to say why; FFZ only with emotes.
        let tabs = tabs_for(&EmoteCatalog::default());
        assert_eq!(tabs, [Tab::Recent, Tab::Provider(EmoteProvider::Twitch)]);
    }

    #[test]
    fn the_picker_opens_on_recent_only_when_it_has_something_to_show() {
        let catalog = catalog();
        assert_eq!(
            first_tab(&catalog, &Recents::default()),
            Tab::Provider(EmoteProvider::Twitch)
        );
        // A recent emote this channel cannot use does not count.
        let gone = Recents::from_names(["NotHere".to_owned()]);
        assert_ne!(first_tab(&catalog, &gone), Tab::Recent);
        let used = Recents::from_names(["monkaW".to_owned()]);
        assert_eq!(first_tab(&catalog, &used), Tab::Recent);
        // Nothing at all: the Twitch tab, which explains.
        assert_eq!(
            first_tab(&EmoteCatalog::default(), &Recents::default()),
            Tab::Provider(EmoteProvider::Twitch)
        );
    }

    #[test]
    fn a_tab_lists_its_provider_and_recent_lists_what_was_used() {
        let catalog = catalog();
        let recents = Recents::from_names(["KEKW", "Gone", "Kappa"].map(String::from));
        let tab = |tab| results_for(&catalog, &recents, tab, "");
        assert_eq!(
            names(&tab(Tab::Provider(EmoteProvider::SevenTv))),
            ["OMEGALUL", "monkaW"]
        );
        // BTTV's own list keeps Kappa (the Twitch one wins it for typing, not for browsing).
        assert_eq!(
            names(&tab(Tab::Provider(EmoteProvider::Bttv))),
            ["Kappa", "KEKW"]
        );
        // Recent: newest first, without what this channel lacks.
        assert_eq!(names(&tab(Tab::Recent)), ["KEKW", "Kappa"]);
    }

    #[test]
    fn a_search_covers_every_provider_whatever_the_tab() {
        let catalog = catalog();
        let recents = Recents::default();
        let found = results_for(
            &catalog,
            &recents,
            Tab::Provider(EmoteProvider::Twitch),
            "ka",
        );
        // Case-insensitive prefix first, then the rest by tier.
        assert_eq!(found[0].name.as_ref(), "Kappa");
        assert!(names(&found).contains(&"monkaW"));
        assert!(results_for(&catalog, &recents, Tab::Recent, "zzz").is_empty());
        // Blank is not a search.
        assert_eq!(
            results_for(
                &catalog,
                &recents,
                Tab::Provider(EmoteProvider::SevenTv),
                "  "
            )
            .len(),
            2
        );
    }

    /// A picker in a test window with the app's key bindings, and the events it emitted.
    fn open(
        cx: &mut gpui_kit::TestAppContext,
        recents: Recents,
    ) -> (
        Entity<EmotePicker>,
        Rc<RefCell<Vec<String>>>,
        &mut gpui_kit::VisualTestContext,
    ) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply_settings(
                &gpui_chat_settings::AppearanceSettings::default(),
                None,
                cx,
            );
            cx.bind_keys(bindings());
        });
        let (picker, cx) = cx.add_window_view(|window, cx| {
            EmotePicker::new(Arc::new(catalog()), recents, None, window, cx)
        });
        cx.simulate_resize(gpui_kit::size(px(800.0), px(600.0)));
        let events = Rc::new(RefCell::new(Vec::new()));
        let log = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(&picker, move |_, event: &PickerEvent, _| {
                log.borrow_mut().push(match event {
                    PickerEvent::Picked(emote) => format!("picked {}", emote.name),
                    PickerEvent::Dismissed => "dismissed".to_owned(),
                });
            })
            .detach();
        });
        cx.run_until_parked();
        (picker, events, cx)
    }

    #[gpui_kit::test]
    fn keyboard_only_a_search_an_arrow_and_enter_pick_an_emote(cx: &mut gpui_kit::TestAppContext) {
        let (picker, events, cx) = open(cx, Recents::default());
        // Opens on the first provider with emotes (Twitch has the user's Kappa).
        assert_eq!(
            picker.read_with(cx, |p, _| p.tab),
            Tab::Provider(EmoteProvider::Twitch)
        );
        // Type to search; the best match is highlighted.
        cx.simulate_input("ka");
        cx.run_until_parked();
        let first = picker.read_with(cx, |p, _| names(&p.results).join(","));
        assert!(first.starts_with("Kappa"), "{first}");
        // Arrow keys move the highlight, Enter picks it.
        cx.simulate_keystrokes("right");
        assert_eq!(picker.read_with(cx, |p, _| p.selected), 1);
        cx.simulate_keystrokes("left");
        cx.simulate_keystrokes("enter");
        assert_eq!(*events.borrow(), ["picked Kappa"]);
    }

    #[gpui_kit::test]
    fn tab_changes_the_tab_and_escape_closes(cx: &mut gpui_kit::TestAppContext) {
        let (picker, events, cx) = open(cx, Recents::default());
        cx.simulate_keystrokes("tab");
        assert_eq!(
            picker.read_with(cx, |p, _| p.tab),
            Tab::Provider(EmoteProvider::SevenTv)
        );
        cx.simulate_keystrokes("shift-tab shift-tab");
        assert_eq!(picker.read_with(cx, |p, _| p.tab), Tab::Recent);
        // Down moves a row (the 7TV tab has two emotes: it clamps at the last).
        cx.simulate_keystrokes("tab tab down");
        assert_eq!(picker.read_with(cx, |p, _| p.selected), 1);
        cx.simulate_keystrokes("escape");
        assert_eq!(*events.borrow(), ["dismissed"]);
    }

    #[test]
    fn the_highlight_moves_by_cell_and_stops_at_the_edges() {
        assert_eq!(step(0, 20, 1), 1);
        assert_eq!(step(0, 20, -1), 0);
        assert_eq!(step(3, 20, COLUMNS as isize), 12);
        assert_eq!(step(15, 20, COLUMNS as isize), 19);
        assert_eq!(step(19, 20, 1), 19);
        assert_eq!(step(5, 0, 1), 0);
    }
}
