//! Selection and links on screen (PR 5.6): the window side of [`super::selection`].
//!
//! A [`Surface`] is one place messages are drawn and selected in: the chat list, or the thread
//! popover. It holds the selection, and the geometry of the rows drawn in the current frame:
//!
//! * The surface's **layer** ([`Surface::layer`]) is an empty canvas put *before* the rows, covering
//!   the surface. Its prepaint starts a new frame of geometry; each [`MessageElement`] then reports
//!   where it was laid out ([`Surface::prepaint_row`]). Its paint registers the surface's mouse
//!   handling once (not one set of handlers per row): bubbling runs in reverse paint order, so
//!   anything drawn over the rows (the Reply button, a reply's context line) sees a press first and
//!   can keep it.
//! * A press hit-tests the reported rows ([`super::selection::row_at`], then the row's
//!   [`super::flow::Flow`]) and starts a selection of the unit its click count picks; dragging
//!   extends it, also above or below the surface, where the list scrolls along. While there is a
//!   selection the list stops following new messages (it is held where it is, like scrolling up),
//!   so the text does not run away under the pointer.
//! * A click without a drag clears it, as does a press anywhere outside the surface; Esc clears it
//!   too (handled by the pane, see `crate::pane`). Esc and a press outside also let the list
//!   follow new messages again; a click in the chat does not (see [`Surface::release`]).
//! * Rows paint their part of the selection behind their text ([`Surface::paint_row`]) and give
//!   their links a pointing-hand cursor. A click that starts and ends on the same link opens it
//!   in the browser.
//!
//! A mouse move that changes the selection re-renders the view once; the rows' layouts come from
//! the layout cache, so nothing is shaped or flowed again.
//!
//! [`MessageElement`]: super::element::MessageElement

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_chat_core::MessageStore;
use gpui_kit::{
    App, Bounds, CursorStyle, DispatchPhase, FollowMode, Hitbox, HitboxBehavior, Hsla, IntoElement,
    ListOffset, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, Styled as _, Window, actions, canvas, fill, point, px, size,
};

use super::display::{self, DisplayOptions, DisplayText};
use super::layout::{LayoutCache, MessageLayout, ShapedMessage};
use super::selection::{
    self, CopyRow, Granularity, RowSide, Selection, TextPos, atom_under, caret_at, highlight,
    row_at, unit_at,
};

actions!(
    gpui_chat,
    [
        /// Copies the selected chat text.
        CopySelection,
        /// Clears the chat selection (Esc), handing focus back to the input box.
        ClearSelection
    ]
);

/// How far the pointer moves before a press becomes a drag (logical pixels): a click that
/// wobbles a little is still a click.
const DRAG_THRESHOLD: f32 = 3.0;
/// Autoscroll while dragging past the top or bottom: per frame, a quarter of the distance past
/// the edge, at least this…
const AUTOSCROLL_MIN: f32 = 2.0;
/// …and at most this.
const AUTOSCROLL_MAX: f32 = 40.0;

/// Where one row was laid out this frame.
struct RowGeometry {
    seq: u64,
    bounds: Bounds<Pixels>,
    /// Where its flowed text starts.
    origin: Point<Pixels>,
    layout: MessageLayout,
}

/// What a point on the surface is over.
#[derive(Clone)]
pub struct Hit {
    pub seq: u64,
    /// The caret nearest the point.
    pub caret: usize,
    /// Byte offset of the atom drawn under the point, if any.
    pub atom: Option<usize>,
    layout: MessageLayout,
}

impl Hit {
    /// The link drawn under the point.
    pub fn link(&self) -> Option<Arc<str>> {
        let atom = self.atom?;
        self.layout
            .shaped
            .display
            .link_at(atom)
            .map(|link| link.url.clone())
    }

    /// The selection unit at the point: a word or a message around it, or a caret. Off the text
    /// (above or below every row) there is no word, so a caret.
    fn unit(&self, granularity: Granularity) -> Range<TextPos> {
        let display = &self.layout.shaped.display;
        match (granularity, self.atom) {
            (Granularity::Word, Some(atom)) => unit_at(display, self.seq, atom, granularity),
            (Granularity::Word, None) => unit_at(display, self.seq, self.caret, Granularity::Char),
            _ => unit_at(display, self.seq, self.caret, granularity),
        }
    }
}

/// A press that has not been released yet.
struct Drag {
    origin: Point<Pixels>,
    /// It moved past [`DRAG_THRESHOLD`].
    moved: bool,
    /// The link it started on, which a release on the same link opens.
    link: Option<(u64, Arc<str>)>,
}

/// A message's display text for copying: from its cached layout, or built for the occasion.
enum Shown {
    Cached(Rc<ShapedMessage>),
    Built(DisplayText),
}

impl Shown {
    fn display(&self) -> &DisplayText {
        match self {
            Self::Cached(shaped) => &shaped.display,
            Self::Built(display) => display,
        }
    }
}

/// What a right-click was over, for the context menu (resolved at the click, before the menu
/// opens).
#[derive(Clone)]
pub struct MenuTarget {
    pub seq: u64,
    pub link: Option<Arc<str>>,
}

/// One place messages are selected in. Shared (`Rc`) by its view, its layer and its rows.
pub struct Surface {
    rows: RefCell<Vec<RowGeometry>>,
    bounds: Cell<Option<Bounds<Pixels>>>,
    selection: RefCell<Option<Selection>>,
    drag: RefCell<Option<Drag>>,
    /// The last pointer position during a drag (autoscroll keeps going while it rests outside).
    pointer: Cell<Point<Pixels>>,
    /// The list the rows are in, to freeze and scroll it; `None` for the thread popover.
    list: Option<ListState>,
    /// Sequence number of the list's first item.
    first_seq: Cell<u64>,
    /// The selection stopped the list following new messages: clearing it resumes.
    resume: Cell<bool>,
    /// The layout generation the offsets of the selection belong to.
    generation: Cell<u64>,
    options: Cell<DisplayOptions>,
    /// Where the rows' layouts are cached (for copying rows no longer on screen).
    cache: Rc<RefCell<LayoutCache>>,
    /// The rows this surface shows, when it shows only some of the store (the thread popover).
    members: RefCell<Option<Vec<u64>>>,
    menu_target: RefCell<Option<MenuTarget>>,
    /// The selection background.
    color: Cell<Hsla>,
}

impl Surface {
    pub fn new(list: Option<ListState>, cache: Rc<RefCell<LayoutCache>>) -> Rc<Self> {
        Rc::new(Self {
            rows: RefCell::default(),
            bounds: Cell::default(),
            selection: RefCell::default(),
            drag: RefCell::default(),
            pointer: Cell::new(Point::default()),
            list,
            first_seq: Cell::new(0),
            resume: Cell::new(false),
            generation: Cell::new(0),
            options: Cell::new(DisplayOptions::default()),
            cache,
            members: RefCell::default(),
            menu_target: RefCell::default(),
            color: Cell::new(Hsla::default()),
        })
    }

    /// Called by the view each render. A new layout generation (another font, timestamps on or
    /// off, badge definitions) moves every offset, so it drops the selection.
    pub fn begin_frame(
        &self,
        generation: u64,
        first_seq: u64,
        options: DisplayOptions,
        color: Hsla,
        members: Option<Vec<u64>>,
    ) {
        if self.generation.replace(generation) != generation {
            self.clear();
        }
        self.first_seq.set(first_seq);
        self.options.set(options);
        self.color.set(color);
        *self.members.borrow_mut() = members;
    }

    /// Whether something is selected.
    pub fn has_selection(&self) -> bool {
        self.selection
            .borrow()
            .as_ref()
            .is_some_and(|selection| !selection.is_empty())
    }

    /// Drops the selection; whether there was one.
    /// Drops the selection; whether there was one, or the list starts following again.
    pub fn clear(&self) -> bool {
        self.drag.borrow_mut().take();
        let had = self
            .selection
            .borrow_mut()
            .take()
            .is_some_and(|selection| !selection.is_empty());
        had | self.resume_following()
    }

    /// If the selection stopped the list following new messages, it follows them again.
    fn resume_following(&self) -> bool {
        match &self.list {
            Some(list) if self.resume.replace(false) => {
                list.set_follow_mode(FollowMode::Tail);
                true
            }
            _ => false,
        }
    }

    /// The selected bytes of message `seq` (display text `len` bytes long).
    pub fn range_for(&self, seq: u64, len: usize) -> Option<Range<usize>> {
        self.selection.borrow().as_ref()?.range_in(seq, len)
    }

    /// The text the selection copies (see [`selection::copy_text`]), or `None` without one.
    pub fn selected_text(&self, store: &MessageStore) -> Option<String> {
        let selection = self.selection.borrow().clone()?;
        if selection.is_empty() {
            return None;
        }
        let members = self.members.borrow();
        let cache = self.cache.borrow();
        let options = self.options.get();
        let mut parts = Vec::new();
        for seq in selection.seqs() {
            if members.as_ref().is_some_and(|m| !m.contains(&seq)) {
                continue;
            }
            let Some(message) = store.get_by_seq(seq) else {
                continue;
            };
            // Rows that were drawn are in the cache; rebuild (without badges) any that are not.
            let shown = match cache.shaped(seq) {
                Some(shaped) => Shown::Cached(shaped),
                None => Shown::Built(display::build(message, options, &[])),
            };
            let Some(range) = selection.range_in(seq, shown.display().text.len()) else {
                continue;
            };
            parts.push((message.clone(), shown, range));
        }
        let text = selection::copy_text(parts.iter().map(|(message, shown, range)| CopyRow {
            message,
            display: shown.display(),
            range: range.clone(),
        }));
        (!text.is_empty()).then_some(text)
    }

    /// What the last right-click was over.
    pub fn menu_target(&self) -> Option<MenuTarget> {
        self.menu_target.borrow().clone()
    }

    /// Where (in the window) the caret before byte `offset` of row `seq` was drawn this frame,
    /// half a pixel into the character after it.
    #[cfg(test)]
    pub fn point_of(&self, seq: u64, offset: usize) -> Option<Point<Pixels>> {
        let rows = self.rows.borrow();
        let row = rows.iter().find(|row| row.seq == seq)?;
        let MessageLayout { shaped, flow } = &row.layout;
        let (line, x) = flow.position_of(&shaped.atoms, offset);
        let line = &flow.lines[line];
        Some(row.origin + point(px(x + 0.5), px(line.top + line.height / 2.0)))
    }

    /// The element that handles the mouse for this surface: put it first among the surface's
    /// children, covering it (it is absolutely positioned and fills its parent).
    pub fn layer(self: &Rc<Self>) -> impl IntoElement {
        let (start, surface) = (self.clone(), self.clone());
        canvas(
            move |bounds, window, _| {
                start.rows.borrow_mut().clear();
                start.bounds.set(Some(bounds));
                window.insert_hitbox(bounds, HitboxBehavior::Normal)
            },
            move |bounds, hitbox, window, cx| surface.paint_layer(bounds, hitbox, window, cx),
        )
        .absolute()
        .size_full()
    }

    /// Records where row `seq` is this frame (called from its prepaint) and gives each of its
    /// links a hit area for the pointing-hand cursor.
    pub fn prepaint_row(
        &self,
        seq: u64,
        bounds: Bounds<Pixels>,
        origin: Point<Pixels>,
        layout: &MessageLayout,
        window: &mut Window,
    ) -> Vec<Hitbox> {
        let mut rows = self.rows.borrow_mut();
        // A row prepainted twice in one frame keeps its last position.
        rows.retain(|row| row.seq != seq);
        rows.push(RowGeometry {
            seq,
            bounds,
            origin,
            layout: layout.clone(),
        });
        drop(rows);
        let MessageLayout { shaped, flow } = layout;
        let mut hitboxes = Vec::new();
        for link in &shaped.display.links {
            for rect in highlight(flow, &shaped.atoms, link.range.clone()) {
                let line = &flow.lines[rect.line];
                let bounds = Bounds::new(
                    origin + point(px(rect.x), px(line.top)),
                    size(px(rect.width), px(line.height)),
                );
                hitboxes.push(window.insert_hitbox(bounds, HitboxBehavior::Normal));
            }
        }
        hitboxes
    }

    /// Paints row `seq`'s part of the selection (call before its text) and sets the cursor over
    /// its links.
    pub fn paint_row(
        &self,
        seq: u64,
        origin: Point<Pixels>,
        layout: &MessageLayout,
        links: &[Hitbox],
        window: &mut Window,
    ) {
        let MessageLayout { shaped, flow } = layout;
        if let Some(range) = self.range_for(seq, shaped.display.text.len()) {
            let color = self.color.get();
            for rect in highlight(flow, &shaped.atoms, range) {
                let line = &flow.lines[rect.line];
                let bounds = Bounds::new(
                    origin + point(px(rect.x), px(line.top)),
                    size(px(rect.width), px(line.height)),
                );
                window.paint_quad(fill(bounds, color));
            }
        }
        for hitbox in links {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
    }

    /// What `position` (window coordinates) is over: the rows drawn this frame, the point clamped
    /// into the surface first, so a drag above or below it lands on the first or last row shown.
    pub fn hit(&self, position: Point<Pixels>) -> Option<Hit> {
        let bounds = self.bounds.get()?;
        let rows = self.rows.borrow();
        let mut order: Vec<&RowGeometry> = rows.iter().collect();
        order.sort_by(|a, b| a.bounds.top().as_f32().total_cmp(&b.bounds.top().as_f32()));
        let extents: Vec<(f32, f32)> = order
            .iter()
            .map(|row| (row.bounds.top().as_f32(), row.bounds.bottom().as_f32()))
            .collect();
        let top = bounds.top().as_f32();
        let y = position
            .y
            .as_f32()
            .clamp(top, (bounds.bottom().as_f32() - 1.0).max(top));
        let (index, side) = row_at(&extents, y)?;
        let row = order[index];
        let shaped = &row.layout.shaped;
        let len = shaped.display.text.len();
        let (caret, atom) = match side {
            RowSide::Before => (0, None),
            RowSide::After => (len, None),
            RowSide::Inside => {
                let x = (position.x - row.origin.x).as_f32();
                let y = y - row.origin.y.as_f32();
                let flow = &row.layout.flow;
                let caret = caret_at(flow, &shaped.atoms, len, x, y);
                let atom =
                    atom_under(flow, &shaped.atoms, x, y).map(|i| shaped.atoms[i].range.start);
                (caret, atom)
            }
        };
        Some(Hit {
            seq: row.seq,
            caret,
            atom,
            layout: row.layout.clone(),
        })
    }

    /// The list position of the top of the surface, from the rows drawn this frame.
    fn scroll_top(&self) -> Option<ListOffset> {
        let bounds = self.bounds.get()?;
        let rows = self.rows.borrow();
        let first = self.first_seq.get();
        rows.iter()
            .filter(|row| row.bounds.bottom() > bounds.top())
            .min_by(|a, b| a.bounds.top().as_f32().total_cmp(&b.bounds.top().as_f32()))
            .and_then(|row| {
                Some(ListOffset {
                    item_ix: usize::try_from(row.seq.checked_sub(first)?).ok()?,
                    offset_in_item: (bounds.top() - row.bounds.top()).max(px(0.0)),
                })
            })
    }

    /// Starts a selection (or extends it, with Shift) at a left press.
    fn press(&self, event: &MouseDownEvent) -> bool {
        let Some(hit) = self.hit(event.position) else {
            return false;
        };
        let mut selection = self.selection.borrow_mut();
        let link = (event.click_count <= 1)
            .then(|| hit.link().map(|url| (hit.seq, url)))
            .flatten();
        match selection.as_mut() {
            Some(current) if event.modifiers.shift => {
                current.extend_to(hit.unit(current.granularity));
            }
            _ => {
                let granularity = Granularity::from_clicks(event.click_count);
                *selection = Some(Selection::new(hit.unit(granularity), granularity));
            }
        }
        *self.drag.borrow_mut() = Some(Drag {
            origin: event.position,
            moved: event.modifiers.shift,
            link,
        });
        self.pointer.set(event.position);
        true
    }

    /// Moves the head of the selection to `position`; whether it changed.
    fn drag_to(&self, position: Point<Pixels>) -> bool {
        self.pointer.set(position);
        let mut drag = self.drag.borrow_mut();
        let Some(drag) = drag.as_mut() else {
            return false;
        };
        if !drag.moved {
            let delta = position - drag.origin;
            if delta.x.as_f32().hypot(delta.y.as_f32()) < DRAG_THRESHOLD {
                return false;
            }
            drag.moved = true;
        }
        let Some(hit) = self.hit(position) else {
            return false;
        };
        let mut selection = self.selection.borrow_mut();
        let Some(selection) = selection.as_mut() else {
            return false;
        };
        let unit = hit.unit(selection.granularity);
        selection.extend_to(unit)
    }

    /// Ends a press: a click (no drag, one click) leaves nothing selected, and opens the link it
    /// started and ended on. Returns the link to open.
    ///
    /// A click does not make the list follow again (the second click of a double-click must find
    /// the text where it was): the list stays where it is, like after scrolling up, until it is
    /// scrolled back down, the "new messages" pill is clicked, or a press outside the chat (say, in
    /// the input box) or Esc resumes it.
    fn release(&self, position: Point<Pixels>) -> Option<Arc<str>> {
        let drag = self.drag.borrow_mut().take()?;
        let click = !drag.moved
            && self
                .selection
                .borrow()
                .as_ref()
                .is_some_and(|selection| selection.granularity == Granularity::Char);
        if !click {
            return None;
        }
        // The press already replaced any selection with an empty one: nothing to repaint.
        self.selection.borrow_mut().take();
        let open = drag.link.filter(|(seq, url)| {
            self.hit(position)
                .is_some_and(|hit| hit.seq == *seq && hit.link().as_deref() == Some(&**url))
        });
        open.map(|(_, url)| url)
    }

    /// While a press is held with something selected: keep the list where it is, and scroll it
    /// when the pointer is past the top or bottom. Runs every frame of the drag.
    fn tick(&self, bounds: Bounds<Pixels>, window: &mut Window) -> bool {
        let Some(list) = &self.list else {
            return false;
        };
        let dragging = self.drag.borrow().as_ref().is_some_and(|drag| drag.moved);
        // While something is selected (or being dragged over), keep still. GPUI re-engages
        // following whenever the list is at its end, so this checks every frame.
        if !(dragging || self.has_selection()) {
            // Following again by other means (scrolled to the end, the pill): nothing to resume.
            if list.is_following_tail() {
                self.resume.set(false);
            }
            return false;
        }
        if list.is_following_tail()
            && let Some(top) = self.scroll_top()
        {
            list.scroll_to(top);
            self.resume.set(true);
        }
        if !dragging {
            return false;
        }
        let pointer = self.pointer.get();
        let past = if pointer.y < bounds.top() {
            (pointer.y - bounds.top()).as_f32()
        } else if pointer.y > bounds.bottom() {
            (pointer.y - bounds.bottom()).as_f32()
        } else {
            return false;
        };
        let step = (past.abs() / 4.0).clamp(AUTOSCROLL_MIN, AUTOSCROLL_MAX);
        list.scroll_by(px(step.copysign(past)));
        window.request_animation_frame();
        // The rows drawn this frame: the head moves to the first or last of them; the next frame
        // (scrolled) moves it further.
        self.drag_to(pointer)
    }

    fn paint_layer(
        self: &Rc<Self>,
        bounds: Bounds<Pixels>,
        hitbox: Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let view = window.current_view();
        if self.tick(bounds, window) {
            cx.notify(view);
        }

        // A press anywhere else (another pane, the input box, the sidebar, a menu) clears it.
        let surface = self.clone();
        let outside = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture
                && event.button == MouseButton::Left
                && !outside.is_hovered(window)
                && surface.clear()
            {
                cx.notify(view);
            }
        });

        let surface = self.clone();
        let inside = hitbox;
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if !phase.bubble() || !inside.is_hovered(window) {
                return;
            }
            match event.button {
                MouseButton::Left => {
                    if surface.press(event) {
                        cx.notify(view);
                    }
                }
                // Before the context menu opens (its handler runs after this one).
                MouseButton::Right => surface.aim_menu(event.position),
                _ => {}
            }
        });

        let surface = self.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase.bubble()
                && event.pressed_button == Some(MouseButton::Left)
                && surface.drag_to(event.position)
            {
                cx.notify(view);
            }
        });

        let surface = self.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if !phase.bubble() || event.button != MouseButton::Left {
                return;
            }
            if let Some(url) = surface
                .release(event.position)
                .filter(|url| selection::openable(url))
            {
                cx.open_url(&url);
            }
        });
    }

    /// Resolves what a right-click at `position` is over, for the menu that is about to open.
    fn aim_menu(&self, position: Point<Pixels>) {
        // Only over a row: the empty space above the first message is no message's.
        let on_row = self
            .rows
            .borrow()
            .iter()
            .any(|row| row.bounds.contains(&position));
        let target = on_row
            .then(|| self.hit(position))
            .flatten()
            .map(|hit| MenuTarget {
                seq: hit.seq,
                link: hit.link(),
            });
        *self.menu_target.borrow_mut() = target;
    }
}
