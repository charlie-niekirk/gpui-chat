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
//!   extends it, also above or below the surface, where the list scrolls along (until it reaches
//!   its first or last message).
//! * **Holding the list.** From a press until the selection is gone, the list stops following new
//!   messages (it is held where it is, like scrolling up), so the text does not run away under the
//!   pointer and the second and third press of a double / triple click land on the same text. A
//!   click that selects nothing lets it follow again once the double-click interval passes without
//!   another press; Esc (`ClearSelection`, handled by the chat view) and a press outside the
//!   surface clear the selection and let it follow again at once. It resumes only if the
//!   selection is what stopped it (a list the user had scrolled up stays where it is).
//! * Rows paint their part of the selection behind their text ([`Surface::paint_row`]) and give
//!   their links a pointing-hand cursor. A click that starts and ends on the same link opens it in
//!   the browser once the double-click interval has passed without another press, so a
//!   double-click on a link selects it instead.
//! * **Copying** uses the display text of the selection's two end messages as it was when the
//!   pointer was on them (the offsets were measured on it), even if their layouts were dropped or
//!   rebuilt since; the messages in between are copied whole from the store.
//!
//! A mouse move that changes the selection re-renders the view once; the rows' layouts come from
//! the layout cache, so nothing is shaped or flowed again.
//!
//! [`MessageElement`]: super::element::MessageElement

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::MessageStore;
use gpui_kit::{
    App, Bounds, CursorStyle, DispatchPhase, EntityId, FollowMode, Hitbox, HitboxBehavior, Hsla,
    IntoElement, ListOffset, ListState, LongPressEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Styled as _, TouchPhase, Window, actions, canvas,
    fill, point, px, size,
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
        /// Clears the chat selection (Esc); with nothing selected it lets the key go on.
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
/// How long after a click a second press still makes it a double-click (macOS's default).
pub const DOUBLE_CLICK: Duration = Duration::from_millis(500);

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

/// An end of the selection: its message's sequence number and display text as the pointer saw it.
type End = (u64, Rc<ShapedMessage>);

/// A click that selected nothing, waiting out the double-click interval: a second press cancels
/// it; otherwise the list may follow again and the link it was on opens.
struct PendingClick {
    /// Which click (a later one replaces it).
    id: u64,
    link: Option<Arc<str>>,
}

/// A message's display text for copying: the one its offsets were measured on, from its cached
/// layout, or built for the occasion.
enum Shown {
    Shaped(Rc<ShapedMessage>),
    Built(DisplayText),
}

impl Shown {
    fn display(&self) -> &DisplayText {
        match self {
            Self::Shaped(shaped) => &shaped.display,
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
    /// The display text of the anchor's and the head's message when the pointer was on them.
    ends: RefCell<[Option<End>; 2]>,
    pending: RefCell<Option<PendingClick>>,
    clicks: Cell<u64>,
    /// Whether the window was active at the last paint.
    active: Cell<bool>,
    /// The display-text generation the offsets of the selection belong to.
    generation: Cell<u64>,
    options: Cell<DisplayOptions>,
    /// Where the rows' layouts are cached (for copying rows no longer on screen).
    cache: Rc<RefCell<LayoutCache>>,
    /// The rows this surface shows, when it shows only some of the store (the thread popover).
    members: RefCell<Option<Vec<u64>>>,
    menu_target: RefCell<Option<MenuTarget>>,
    #[cfg(test)]
    last_aimed: RefCell<Option<MenuTarget>>,
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
            ends: RefCell::default(),
            pending: RefCell::default(),
            clicks: Cell::new(0),
            active: Cell::new(false),
            generation: Cell::new(0),
            options: Cell::new(DisplayOptions::default()),
            cache,
            members: RefCell::default(),
            menu_target: RefCell::default(),
            #[cfg(test)]
            last_aimed: RefCell::default(),
            color: Cell::new(Hsla::default()),
        })
    }

    /// Called by the view each render. `generation` changes when display texts may have changed
    /// (timestamps on or off, badge definitions): that moves offsets, so it drops the selection.
    /// Font, line height or theme changes only lay the same text out again and keep it.
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

    /// Moves everything keyed by a message's sequence number to the number `map` gives, after the
    /// store renumbered its messages (history merged before them, see [`gpui_chat_core::Rebuild`]):
    /// the selection and the display texts of its two ends, the link a press is on, the context
    /// menu's target and the rows of the thread popover. A selection one of whose ends is gone
    /// is cleared (and the menu target dropped); one that stays still covers whatever lies
    /// between its ends, new rows included.
    pub fn remap(&self, map: &dyn Fn(u64) -> Option<u64>) {
        // The geometry was measured under the old numbers; the next paint measures it again.
        self.rows.borrow_mut().clear();
        let kept = self
            .selection
            .borrow_mut()
            .as_mut()
            .is_none_or(|selection| selection.remap(map));
        if !kept {
            self.clear();
            self.menu_target.borrow_mut().take();
            self.ends
                .borrow_mut()
                .iter_mut()
                .for_each(|end| *end = None);
            return;
        }
        for end in self.ends.borrow_mut().iter_mut() {
            *end = end
                .take()
                .and_then(|(seq, shaped)| Some((map(seq)?, shaped)));
        }
        if let Some(drag) = self.drag.borrow_mut().as_mut() {
            drag.link = drag
                .link
                .take()
                .and_then(|(seq, url)| Some((map(seq)?, url)));
        }
        let moved = |target: &mut Option<MenuTarget>| {
            *target = target.take().and_then(|target| {
                Some(MenuTarget {
                    seq: map(target.seq)?,
                    ..target
                })
            });
        };
        moved(&mut self.menu_target.borrow_mut());
        #[cfg(test)]
        moved(&mut self.last_aimed.borrow_mut());
        if let Some(members) = self.members.borrow_mut().as_mut() {
            *members = members.iter().filter_map(|seq| map(*seq)).collect();
        }
    }

    /// Drops everything keyed by sequence numbers that no longer mean what they did (the list
    /// replaced every row): the selection, the menu target, the press in progress.
    pub fn reset(&self) {
        self.rows.borrow_mut().clear();
        self.clear();
        self.menu_target.borrow_mut().take();
        self.ends
            .borrow_mut()
            .iter_mut()
            .for_each(|end| *end = None);
    }

    /// Whether something is selected.
    pub fn has_selection(&self) -> bool {
        self.selection
            .borrow()
            .as_ref()
            .is_some_and(|selection| !selection.is_empty())
    }

    /// Drops the selection; whether there was one, or the list starts following again.
    pub fn clear(&self) -> bool {
        self.drag.borrow_mut().take();
        self.pending.borrow_mut().take();
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
            // The two ends as the pointer saw them; the others are whole lines from the store, so
            // their display text only gives their length (cached, else rebuilt without badges).
            let end = self
                .ends
                .borrow()
                .iter()
                .flatten()
                .find(|(end, _)| *end == seq)
                .map(|(_, shaped)| shaped.clone());
            let shown = match end.or_else(|| cache.shaped(seq)) {
                Some(shaped) => Shown::Shaped(shaped),
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

    /// What the right-click (or long press) that is opening a menu was over; taken, so a menu
    /// opened some other way finds nothing stale.
    pub fn take_menu_target(&self) -> Option<MenuTarget> {
        self.menu_target.borrow_mut().take()
    }

    /// Peeks at the menu target without taking it.
    #[cfg(test)]
    pub fn menu_target(&self) -> Option<MenuTarget> {
        self.menu_target.borrow().clone()
    }

    /// The messages the selection touches.
    #[cfg(test)]
    pub fn selection_seqs(&self) -> Option<std::ops::RangeInclusive<u64>> {
        self.selection.borrow().as_ref().map(Selection::seqs)
    }

    /// What the last right-click was aimed at (kept after the menu took it).
    #[cfg(test)]
    pub fn last_aimed(&self) -> Option<MenuTarget> {
        self.last_aimed.borrow().clone()
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

    /// Records the display text of the message an end of the selection is on (`which`: 0 the
    /// anchor, 1 the head).
    fn remember_end(&self, which: usize, hit: &Hit) {
        self.ends.borrow_mut()[which] = Some((hit.seq, hit.layout.shaped.clone()));
    }

    /// Stops a following list where it is (see the module docs).
    fn hold(&self) {
        if let Some(list) = &self.list
            && list.is_following_tail()
            && let Some(top) = self.scroll_top()
        {
            list.scroll_to(top);
            self.resume.set(true);
        }
    }

    /// Starts a selection (or extends it, with Shift) at a left press.
    fn press(&self, event: &MouseDownEvent) -> bool {
        let Some(hit) = self.hit(event.position) else {
            return false;
        };
        // A second press: the first was not a click on its own.
        self.pending.borrow_mut().take();
        self.hold();
        let mut selection = self.selection.borrow_mut();
        let link = (event.click_count <= 1)
            .then(|| hit.link().map(|url| (hit.seq, url)))
            .flatten();
        match selection.as_mut() {
            Some(current) if event.modifiers.shift => {
                current.extend_to(hit.unit(current.granularity));
                self.remember_end(1, &hit);
            }
            _ => {
                let granularity = Granularity::from_clicks(event.click_count);
                *selection = Some(Selection::new(hit.unit(granularity), granularity));
                self.remember_end(0, &hit);
                self.remember_end(1, &hit);
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
        let changed = selection.extend_to(unit);
        if changed {
            self.remember_end(1, &hit);
        }
        changed
    }

    /// A press whose release never arrived (the pointer moved with no button, or the window lost
    /// focus): forget it, so it cannot hold the list or keep extending.
    fn abandon_press(&self) -> bool {
        if self.drag.borrow_mut().take().is_none() {
            return false;
        }
        if !self.has_selection() {
            self.selection.borrow_mut().take();
            self.resume_following();
        }
        true
    }

    /// Ends a press. A click (no drag, one click) leaves nothing selected and becomes a
    /// [`PendingClick`] with the link it started and ended on; returns its id, for the timer that
    /// settles it after [`DOUBLE_CLICK`].
    fn release(&self, position: Point<Pixels>) -> Option<u64> {
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
        let link = drag
            .link
            .filter(|(seq, url)| {
                self.hit(position)
                    .is_some_and(|hit| hit.seq == *seq && hit.link().as_deref() == Some(&**url))
            })
            .map(|(_, url)| url);
        let id = self.clicks.get() + 1;
        self.clicks.set(id);
        *self.pending.borrow_mut() = Some(PendingClick { id, link });
        Some(id)
    }

    /// The double-click interval after click `id` passed: if no press came since, the list may
    /// follow again, and the link is returned to open.
    fn settle(&self, id: u64) -> Option<Option<Arc<str>>> {
        let mut pending = self.pending.borrow_mut();
        if pending.as_ref().is_none_or(|click| click.id != id) {
            return None;
        }
        let click = pending.take()?;
        drop(pending);
        if self.drag.borrow().is_none() && !self.has_selection() {
            self.resume_following();
        }
        Some(click.link)
    }

    /// Whether the list already shows its first (`up`) or last message in full, so scrolling
    /// further that way does nothing.
    fn at_end(&self, up: bool, bounds: Bounds<Pixels>) -> bool {
        let Some(list) = &self.list else {
            return true;
        };
        let first = self.first_seq.get();
        let rows = self.rows.borrow();
        if up {
            rows.iter()
                .any(|row| row.seq == first && row.bounds.top() >= bounds.top() - px(0.5))
        } else {
            let last = (first + list.item_count() as u64).saturating_sub(1);
            rows.iter()
                .any(|row| row.seq == last && row.bounds.bottom() <= bounds.bottom() + px(0.5))
        }
    }

    /// Each frame: keep the list held while it should be (GPUI re-engages following whenever the
    /// list is at its end), and scroll it while a drag is past its top or bottom.
    fn tick(&self, bounds: Bounds<Pixels>, window: &mut Window) -> bool {
        let Some(list) = &self.list else {
            return false;
        };
        let (pressed, dragging) = match self.drag.borrow().as_ref() {
            Some(drag) => (true, drag.moved),
            None => (false, false),
        };
        let waiting = self.pending.borrow().is_some();
        if !(pressed || waiting || self.has_selection()) {
            // Following again by other means (scrolled to the end, the pill): nothing to resume.
            if list.is_following_tail() {
                self.resume.set(false);
            }
            return false;
        }
        self.hold();
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
        if !self.at_end(past < 0.0, bounds) {
            let step = (past.abs() / 4.0).clamp(AUTOSCROLL_MIN, AUTOSCROLL_MAX);
            list.scroll_by(px(step.copysign(past)));
            window.request_animation_frame();
        }
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
        // The window was deactivated mid-press: its release may never arrive.
        let active = window.is_window_active();
        let lost = self.active.replace(active) && !active && self.abandon_press();
        if self.tick(bounds, window) || lost {
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
        let pressing = inside.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if !phase.bubble() || !pressing.is_hovered(window) {
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

        // Kit opens the context menu on a long press too.
        let surface = self.clone();
        let pressed = inside.clone();
        window.on_mouse_event(move |event: &LongPressEvent, phase, window, _| {
            if phase.bubble() && event.phase == TouchPhase::Started && pressed.is_hovered(window) {
                surface.aim_menu(event.start_position);
            }
        });

        let surface = self.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if !phase.bubble() {
                return;
            }
            let changed = if event.pressed_button == Some(MouseButton::Left) {
                surface.drag_to(event.position)
            } else {
                surface.abandon_press()
            };
            if changed {
                cx.notify(view);
            }
        });

        let surface = self.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if !phase.bubble() || event.button != MouseButton::Left {
                return;
            }
            if let Some(id) = surface.release(event.position) {
                surface.settle_later(id, view, cx);
            }
        });
    }

    /// Settles click `id` after the double-click interval: lets the list follow again, opens its
    /// link.
    fn settle_later(self: &Rc<Self>, id: u64, view: EntityId, cx: &mut App) {
        let surface = self.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(DOUBLE_CLICK).await;
            cx.update(|cx| {
                let Some(link) = surface.settle(id) else {
                    return;
                };
                if let Some(url) = link.filter(|url| selection::openable(url)) {
                    cx.open_url(&url);
                }
                cx.notify(view);
            });
        })
        .detach();
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
        #[cfg(test)]
        {
            *self.last_aimed.borrow_mut() = target.clone();
        }
        *self.menu_target.borrow_mut() = target;
    }
}

#[cfg(test)]
mod remap_tests {
    use super::*;

    fn surface() -> Rc<Surface> {
        Surface::new(None, Rc::default())
    }

    fn select(surface: &Surface, from: u64, to: u64) {
        let mut selection = Selection::new(
            TextPos::new(from, 1)..TextPos::new(from, 1),
            Granularity::Char,
        );
        selection.extend_to(TextPos::new(to, 3)..TextPos::new(to, 3));
        *surface.selection.borrow_mut() = Some(selection);
    }

    fn aim(surface: &Surface, seq: u64) {
        *surface.menu_target.borrow_mut() = Some(MenuTarget {
            seq,
            link: Some("https://example.com".into()),
        });
    }

    #[test]
    fn the_selection_and_the_menu_target_follow_their_messages() {
        let surface = surface();
        select(&surface, 2, 4);
        aim(&surface, 3);
        surface.remap(&|seq| Some(seq + 7));
        assert_eq!(surface.selection_seqs(), Some(9..=11));
        let target = surface.take_menu_target().expect("still aimed");
        assert_eq!(
            (target.seq, target.link.as_deref()),
            (10, Some("https://example.com"))
        );
    }

    #[test]
    fn a_selection_whose_message_is_gone_is_cleared_with_the_menu_target() {
        let surface = surface();
        select(&surface, 2, 4);
        aim(&surface, 3);
        surface.remap(&|seq| (seq != 4).then_some(seq + 1));
        assert_eq!(surface.selection_seqs(), None);
        assert!(surface.take_menu_target().is_none());
    }

    #[test]
    fn a_menu_target_whose_message_is_gone_is_dropped_but_the_selection_stays() {
        let surface = surface();
        select(&surface, 2, 4);
        aim(&surface, 3);
        surface.remap(&|seq| (seq != 3).then_some(seq));
        assert_eq!(surface.selection_seqs(), Some(2..=4));
        assert!(surface.take_menu_target().is_none());
    }

    #[test]
    fn reset_drops_selection_and_target() {
        let surface = surface();
        select(&surface, 2, 4);
        aim(&surface, 3);
        surface.reset();
        assert_eq!(surface.selection_seqs(), None);
        assert!(surface.take_menu_target().is_none());
    }
}
