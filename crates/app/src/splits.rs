//! The split-pane layout (PR 4.1): a tree of rows and columns whose leaves are panes, each showing
//! one open channel (or nothing yet).
//!
//! Plain data with no GPUI in it, so every rule (where a split goes, what closing gives back, which
//! pane is to the left, what a saved tree restores as) is unit-tested. The shell owns one
//! [`Layout`]; [`view`] draws it.
//!
//! A channel is shown in at most one pane: showing it somewhere else swaps the two panes'
//! channels. A pane's chat (and its scroll position) belongs to the channel, so moving a channel
//! between panes moves its chat with it.

use gpui_chat_core::ChannelId;

use crate::channel_list::ChannelList;
use crate::login::normalise_login;
use crate::state::{SavedAxis, SavedLayout, SavedPane};

pub mod view;

/// More panes than this is a mistake, not a layout (and each one draws a full chat).
pub const MAX_PANES: usize = 16;
/// Deeper saved trees are cut off here.
const MAX_DEPTH: usize = 8;
/// A drop this close to a pane's side (as a fraction of its size) splits it on that side.
const EDGE: f32 = 0.25;
/// Saved sizes smaller than this are treated as a broken file: the split is evened out.
const MIN_SAVED_SIZE: f32 = 0.02;

/// Identifies a pane for as long as the app runs (not saved).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(u64);

impl PaneId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// How a split lays out its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Side by side.
    Row,
    /// Stacked.
    Column,
}

impl From<SavedAxis> for Axis {
    fn from(axis: SavedAxis) -> Self {
        match axis {
            SavedAxis::Row => Self::Row,
            SavedAxis::Column => Self::Column,
        }
    }
}

impl From<Axis> for SavedAxis {
    fn from(axis: Axis) -> Self {
        match axis {
            Axis::Row => Self::Row,
            Axis::Column => Self::Column,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Row,
            Self::Up | Self::Down => Axis::Column,
        }
    }

    /// Right and down come after the pane in reading order.
    fn after(self) -> bool {
        matches!(self, Self::Right | Self::Down)
    }
}

/// Where on a pane something dragged would land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    /// Into the pane: show the dragged channel there.
    Center,
    /// Beside the pane: split it on that side.
    Edge(Direction),
}

impl DropZone {
    /// The zone under the pointer at `x`, `y`: fractions of the pane's width and height. The
    /// outer quarter on each side is that side's edge (the nearest side where two overlap).
    pub fn at(x: f32, y: f32) -> Self {
        [
            (x, Direction::Left),
            (1.0 - x, Direction::Right),
            (y, Direction::Up),
            (1.0 - y, Direction::Down),
        ]
        .into_iter()
        .filter(|(distance, _)| *distance < EDGE)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(Self::Center, |(_, direction)| Self::Edge(direction))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    pub id: PaneId,
    pub channel: Option<ChannelId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    pub axis: Axis,
    /// Two or more; never a split along the same axis (those are merged into this one).
    pub children: Vec<Node>,
    /// One per child, summing to 1.
    pub sizes: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(Leaf),
    Split(Split),
}

/// A rectangle in the unit square that the whole layout fills.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

const FULL: Rect = Rect {
    x: 0.0,
    y: 0.0,
    w: 1.0,
    h: 1.0,
};

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    root: Node,
    focused: PaneId,
    next_id: u64,
}

impl Default for Layout {
    fn default() -> Self {
        Self::single(None)
    }
}

impl Layout {
    /// One pane showing `channel`.
    pub fn single(channel: Option<ChannelId>) -> Self {
        Self {
            root: Node::Leaf(Leaf {
                id: PaneId(0),
                channel,
            }),
            focused: PaneId(0),
            next_id: 1,
        }
    }

    fn alloc(&mut self) -> PaneId {
        let id = PaneId(self.next_id);
        self.next_id += 1;
        id
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    pub fn focused(&self) -> PaneId {
        self.focused
    }

    pub fn focused_channel(&self) -> Option<&ChannelId> {
        self.leaf(self.focused)?.channel.as_ref()
    }

    /// The panes in reading order (depth first).
    pub fn leaves(&self) -> Vec<&Leaf> {
        fn collect<'a>(node: &'a Node, out: &mut Vec<&'a Leaf>) {
            match node {
                Node::Leaf(leaf) => out.push(leaf),
                Node::Split(split) => split.children.iter().for_each(|c| collect(c, out)),
            }
        }
        let mut out = Vec::new();
        collect(&self.root, &mut out);
        out
    }

    fn ids(&self) -> Vec<PaneId> {
        self.leaves().iter().map(|leaf| leaf.id).collect()
    }

    pub fn len(&self) -> usize {
        self.leaves().len()
    }

    /// More than one pane.
    pub fn is_split(&self) -> bool {
        matches!(self.root, Node::Split(_))
    }

    pub fn leaf(&self, id: PaneId) -> Option<&Leaf> {
        self.leaves().into_iter().find(|leaf| leaf.id == id)
    }

    fn leaf_mut(&mut self, id: PaneId) -> Option<&mut Leaf> {
        fn find(node: &mut Node, id: PaneId) -> Option<&mut Leaf> {
            match node {
                Node::Leaf(leaf) => (leaf.id == id).then_some(leaf),
                Node::Split(split) => split.children.iter_mut().find_map(|c| find(c, id)),
            }
        }
        find(&mut self.root, id)
    }

    /// The pane showing `channel`, if one does.
    pub fn pane_showing(&self, channel: &ChannelId) -> Option<PaneId> {
        self.leaves()
            .into_iter()
            .find(|leaf| leaf.channel.as_ref() == Some(channel))
            .map(|leaf| leaf.id)
    }

    pub fn is_shown(&self, channel: &ChannelId) -> bool {
        self.pane_showing(channel).is_some()
    }

    /// Focuses pane `id`; `false` if it already had focus or does not exist.
    pub fn focus(&mut self, id: PaneId) -> bool {
        if self.focused == id || self.leaf(id).is_none() {
            return false;
        }
        self.focused = id;
        true
    }

    /// Shows `channel` in pane `id`. A pane that showed it already gets this pane's old channel
    /// (they swap). `false` when nothing changed.
    pub fn show(&mut self, id: PaneId, channel: Option<ChannelId>) -> bool {
        let Some(old) = self.leaf(id).map(|leaf| leaf.channel.clone()) else {
            return false;
        };
        if old == channel {
            return false;
        }
        if let Some(other) = channel.as_ref().and_then(|c| self.pane_showing(c))
            && let Some(leaf) = self.leaf_mut(other)
        {
            leaf.channel = old;
        }
        if let Some(leaf) = self.leaf_mut(id) {
            leaf.channel = channel;
        }
        true
    }

    /// Splits pane `id`: a new pane showing `channel` goes on the `direction` side of it and gets
    /// focus. `None` at [`MAX_PANES`] or for an unknown pane. `channel` must not be shown already.
    pub fn split(
        &mut self,
        id: PaneId,
        direction: Direction,
        channel: Option<ChannelId>,
    ) -> Option<PaneId> {
        if self.len() >= MAX_PANES || self.leaf(id).is_none() {
            return None;
        }
        debug_assert!(channel.as_ref().is_none_or(|c| !self.is_shown(c)));
        let new = self.alloc();
        let mut node = Some(Node::Leaf(Leaf { id: new, channel }));
        insert_beside(&mut self.root, id, direction, &mut node);
        self.focused = new;
        Some(new)
    }

    /// Closes pane `id`; its space goes to the pane that takes its place, which gets focus if
    /// the closed one had it. The last pane cannot be closed.
    pub fn close(&mut self, id: PaneId) -> bool {
        let Some(index) = self.ids().iter().position(|i| *i == id) else {
            return false;
        };
        if self.len() <= 1 || detach(&mut self.root, id).is_none() {
            return false;
        }
        if self.focused == id {
            let ids = self.ids();
            self.focused = ids[index.min(ids.len() - 1)];
        }
        true
    }

    /// Moves pane `src` onto `target`: into it swaps their channels, beside it moves the pane
    /// there. The moved channel's pane gets focus.
    pub fn move_pane(&mut self, src: PaneId, target: PaneId, zone: DropZone) -> bool {
        if src == target || self.leaf(src).is_none() || self.leaf(target).is_none() {
            return false;
        }
        match zone {
            DropZone::Center => {
                let channel = self.leaf(src).and_then(|leaf| leaf.channel.clone());
                self.show(target, channel);
                self.focused = target;
            }
            DropZone::Edge(direction) => {
                let mut node = detach(&mut self.root, src);
                insert_beside(&mut self.root, target, direction, &mut node);
                self.focused = src;
            }
        }
        true
    }

    /// A channel from the sidebar dropped on `target`: into it shows it there, beside it opens a
    /// new pane for it. A channel that already has a pane moves with it.
    pub fn drop_channel(&mut self, channel: ChannelId, target: PaneId, zone: DropZone) -> bool {
        if self.leaf(target).is_none() {
            return false;
        }
        if let Some(src) = self.pane_showing(&channel) {
            return if src == target {
                self.focus(target)
            } else {
                self.move_pane(src, target, zone)
            };
        }
        match zone {
            DropZone::Center => {
                self.show(target, Some(channel));
                self.focused = target;
                true
            }
            DropZone::Edge(direction) => self.split(target, direction, Some(channel)).is_some(),
        }
    }

    /// Moves the line after child `handle` of the split at `path` to `at` (a fraction of the
    /// split's length), keeping both sides at least `min` long. `false` when nothing changed.
    pub fn resize(&mut self, path: &[usize], handle: usize, at: f32, min: f32) -> bool {
        let Some(Node::Split(split)) = node_at_mut(&mut self.root, path) else {
            return false;
        };
        if handle + 1 >= split.sizes.len() || !at.is_finite() {
            return false;
        }
        let start: f32 = split.sizes[..handle].iter().sum();
        let end = start + split.sizes[handle] + split.sizes[handle + 1];
        if end - start < 2.0 * min {
            return false;
        }
        let at = at.clamp(start + min, end - min);
        if (at - start - split.sizes[handle]).abs() < 1e-4 {
            return false;
        }
        split.sizes[handle] = at - start;
        split.sizes[handle + 1] = end - at;
        true
    }

    /// Every pane's rectangle in the unit square.
    pub fn rects(&self) -> Vec<(PaneId, Rect)> {
        fn collect(node: &Node, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
            match node {
                Node::Leaf(leaf) => out.push((leaf.id, rect)),
                Node::Split(split) => {
                    let mut offset = 0.0;
                    for (child, size) in split.children.iter().zip(&split.sizes) {
                        let part = match split.axis {
                            Axis::Row => Rect {
                                x: rect.x + offset * rect.w,
                                w: size * rect.w,
                                ..rect
                            },
                            Axis::Column => Rect {
                                y: rect.y + offset * rect.h,
                                h: size * rect.h,
                                ..rect
                            },
                        };
                        offset += size;
                        collect(child, part, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        collect(&self.root, FULL, &mut out);
        out
    }

    /// The pane next to `id` on its `direction` side: of those touching that side, the one that
    /// shares the most of it.
    pub fn neighbour(&self, id: PaneId, direction: Direction) -> Option<PaneId> {
        const EPS: f32 = 1e-4;
        let rects = self.rects();
        let from = rects.iter().find(|(i, _)| *i == id)?.1;
        let overlap =
            |a: f32, a_len: f32, b: f32, b_len: f32| (a + a_len).min(b + b_len) - a.max(b);
        rects
            .iter()
            .filter(|(i, _)| *i != id)
            .filter_map(|(i, r)| {
                let (gap, shared) = match direction {
                    Direction::Right => {
                        (r.x - (from.x + from.w), overlap(from.y, from.h, r.y, r.h))
                    }
                    Direction::Left => (from.x - (r.x + r.w), overlap(from.y, from.h, r.y, r.h)),
                    Direction::Down => (r.y - (from.y + from.h), overlap(from.x, from.w, r.x, r.w)),
                    Direction::Up => (from.y - (r.y + r.h), overlap(from.x, from.w, r.x, r.w)),
                };
                (gap > -EPS && shared > EPS).then_some((*i, gap, shared))
            })
            .min_by(|a, b| {
                // Nearest first (equal within rounding), then the most shared.
                if (a.1 - b.1).abs() > EPS {
                    a.1.total_cmp(&b.1)
                } else {
                    b.2.total_cmp(&a.2)
                }
            })
            .map(|(i, ..)| i)
    }

    /// The open channel a new pane shows: the first, from the one after the focused pane's in
    /// sidebar order (wrapping), that no pane shows yet.
    pub fn next_unshown(&self, channels: &[ChannelId]) -> Option<ChannelId> {
        let start = self
            .focused_channel()
            .and_then(|c| channels.iter().position(|x| x == c))
            .map_or(0, |i| i + 1);
        (0..channels.len())
            .map(|step| &channels[(start + step) % channels.len()])
            .find(|c| !self.is_shown(c))
            .cloned()
    }

    /// `channel` was closed in the sidebar, where it was at `index` (`channels` is the list
    /// without it). Its pane shows the channel that took its place, else the one before (the
    /// same rule as the sidebar selection), skipping channels other panes show; or nothing.
    pub fn channel_closed(&mut self, channel: &ChannelId, channels: &[ChannelId], index: usize) {
        let Some(id) = self.pane_showing(channel) else {
            return;
        };
        if let Some(leaf) = self.leaf_mut(id) {
            leaf.channel = None;
        }
        let replacement = channels
            .iter()
            .skip(index)
            .chain(channels.iter().take(index).rev())
            .find(|c| !self.is_shown(c))
            .cloned();
        if let Some(leaf) = self.leaf_mut(id) {
            leaf.channel = replacement;
        }
    }

    pub fn to_saved(&self) -> SavedLayout {
        fn save(node: &Node) -> SavedPane {
            match node {
                Node::Leaf(leaf) => SavedPane {
                    channel: leaf.channel.as_ref().map(|c| c.login().to_owned()),
                    ..SavedPane::default()
                },
                Node::Split(split) => SavedPane {
                    channel: None,
                    axis: Some(split.axis.into()),
                    sizes: split
                        .sizes
                        .iter()
                        .map(|s| (f64::from(*s) * 10_000.0).round() / 10_000.0)
                        .collect(),
                    panes: split.children.iter().map(save).collect(),
                },
            }
        }
        SavedLayout {
            focused: self
                .ids()
                .iter()
                .position(|id| *id == self.focused)
                .unwrap_or(0),
            root: save(&self.root),
        }
    }

    /// The saved layout, checked against the open channels: a pane whose channel is not open
    /// (or is shown by an earlier pane) is empty; broken sizes are evened out; splits of one pane
    /// collapse; past [`MAX_PANES`] panes or [`MAX_DEPTH`] levels the rest is dropped. When no
    /// pane shows anything (no saved layout: a file from before PR 4.1), the focused pane shows
    /// the selected channel.
    pub fn from_saved(saved: Option<&SavedLayout>, list: &ChannelList) -> Self {
        let mut layout = Self {
            next_id: 0,
            ..Self::default()
        };
        let mut used = Vec::new();
        match saved.and_then(|s| layout.restore(&s.root, list, 0, &mut used)) {
            Some(root) => layout.root = root,
            None => layout = Self::default(),
        }
        let ids = layout.ids();
        layout.focused = saved
            .and_then(|s| ids.get(s.focused))
            .copied()
            .unwrap_or(ids[0]);
        if used.is_empty()
            && let Some(selected) = list.selected().cloned()
        {
            let focused = layout.focused;
            layout.show(focused, Some(selected));
        }
        layout
    }

    fn restore(
        &mut self,
        saved: &SavedPane,
        list: &ChannelList,
        depth: usize,
        used: &mut Vec<ChannelId>,
    ) -> Option<Node> {
        if saved.panes.is_empty() || depth >= MAX_DEPTH {
            if self.next_id as usize >= MAX_PANES {
                return None;
            }
            let channel = saved
                .channel
                .as_deref()
                .and_then(|login| normalise_login(login).ok())
                .filter(|c| list.contains(c) && !used.contains(c));
            used.extend(channel.clone());
            return Some(Node::Leaf(Leaf {
                id: self.alloc(),
                channel,
            }));
        }
        let sizes_ok = saved.sizes.len() == saved.panes.len()
            && saved.sizes.iter().all(|s| s.is_finite() && *s > 0.0);
        let mut children = Vec::new();
        let mut sizes = Vec::new();
        for (i, pane) in saved.panes.iter().enumerate() {
            if let Some(node) = self.restore(pane, list, depth + 1, used) {
                children.push(node);
                sizes.push(if sizes_ok { saved.sizes[i] as f32 } else { 1.0 });
            }
        }
        if children.len() <= 1 {
            return children.pop();
        }
        let total: f32 = sizes.iter().sum();
        sizes.iter_mut().for_each(|s| *s /= total);
        if sizes.iter().any(|s| *s < MIN_SAVED_SIZE) {
            let even = 1.0 / sizes.len() as f32;
            sizes.iter_mut().for_each(|s| *s = even);
        }
        let mut node = Node::Split(Split {
            axis: saved.axis.unwrap_or_default().into(),
            children,
            sizes,
        });
        normalise(&mut node);
        Some(node)
    }
}

/// The node at `path` (child indices from the root).
fn node_at_mut<'a>(node: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    match path.split_first() {
        None => Some(node),
        Some((first, rest)) => match node {
            Node::Split(split) => node_at_mut(split.children.get_mut(*first)?, rest),
            Node::Leaf(_) => None,
        },
    }
}

/// Puts `new` on the `direction` side of pane `target`: into the parent when it already runs
/// that way (taking half of the target's space), else in a new two-way split in the pane's place.
fn insert_beside(
    node: &mut Node,
    target: PaneId,
    direction: Direction,
    new: &mut Option<Node>,
) -> bool {
    if new.is_none() {
        return false;
    }
    if matches!(node, Node::Leaf(leaf) if leaf.id == target) {
        let Some(added) = new.take() else {
            return false;
        };
        let old = std::mem::replace(
            node,
            Node::Leaf(Leaf {
                id: target,
                channel: None,
            }),
        );
        let children = if direction.after() {
            vec![old, added]
        } else {
            vec![added, old]
        };
        *node = Node::Split(Split {
            axis: direction.axis(),
            children,
            sizes: vec![0.5, 0.5],
        });
        return true;
    }
    let Node::Split(split) = node else {
        return false;
    };
    if split.axis == direction.axis()
        && let Some(i) = split
            .children
            .iter()
            .position(|c| matches!(c, Node::Leaf(leaf) if leaf.id == target))
        && let Some(added) = new.take()
    {
        let half = split.sizes[i] / 2.0;
        split.sizes[i] = half;
        let at = if direction.after() { i + 1 } else { i };
        split.children.insert(at, added);
        split.sizes.insert(at, half);
        return true;
    }
    split
        .children
        .iter_mut()
        .any(|child| insert_beside(child, target, direction, new))
}

/// Takes pane `id` out of the tree (never the root itself); its space goes to the sibling that
/// takes its place.
fn detach(node: &mut Node, id: PaneId) -> Option<Node> {
    let Node::Split(split) = node else {
        return None;
    };
    let removed = match split
        .children
        .iter()
        .position(|c| matches!(c, Node::Leaf(leaf) if leaf.id == id))
    {
        Some(i) => {
            let size = split.sizes.remove(i);
            let child = split.children.remove(i);
            let heir = i.min(split.children.len() - 1);
            split.sizes[heir] += size;
            Some(child)
        }
        None => split.children.iter_mut().find_map(|c| detach(c, id)),
    }?;
    normalise(node);
    Some(removed)
}

/// Merges child splits that run the same way into `node` and replaces a split of one by its
/// child.
fn normalise(node: &mut Node) {
    let Node::Split(split) = node else {
        return;
    };
    let mut children = Vec::with_capacity(split.children.len());
    let mut sizes = Vec::with_capacity(split.children.len());
    let old = std::mem::take(&mut split.children);
    let old_sizes = std::mem::take(&mut split.sizes);
    for (child, size) in old.into_iter().zip(old_sizes) {
        match child {
            Node::Split(inner) if inner.axis == split.axis => {
                for (c, s) in inner.children.into_iter().zip(inner.sizes) {
                    children.push(c);
                    sizes.push(s * size);
                }
            }
            other => {
                children.push(other);
                sizes.push(size);
            }
        }
    }
    split.children = children;
    split.sizes = sizes;
    if split.children.len() == 1
        && let Some(only) = split.children.pop()
    {
        *node = only;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `a` is the channel `aaaa` (real logins are at least three characters); [`shape`] and
    /// [`focused`] print the first letter back.
    fn ch(name: &str) -> ChannelId {
        ChannelId::new(&name.repeat(4))
    }

    fn letter(channel: &ChannelId) -> String {
        channel.login()[..1].to_owned()
    }

    fn list(names: &[&str]) -> ChannelList {
        let mut list = ChannelList::default();
        for name in names {
            list.add(ch(name));
        }
        list
    }

    /// The tree as text: `a`, `-` for an empty pane, `row(a, b)`, `col(a, b)`.
    fn shape(layout: &Layout) -> String {
        fn show(node: &Node) -> String {
            match node {
                Node::Leaf(leaf) => leaf.channel.as_ref().map_or("-".to_owned(), letter),
                Node::Split(split) => format!(
                    "{}({})",
                    match split.axis {
                        Axis::Row => "row",
                        Axis::Column => "col",
                    },
                    split
                        .children
                        .iter()
                        .map(show)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
        show(layout.root())
    }

    fn sizes(layout: &Layout) -> Vec<f32> {
        match layout.root() {
            Node::Split(split) => split.sizes.clone(),
            Node::Leaf(_) => vec![1.0],
        }
    }

    fn approx(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    fn focused(layout: &Layout) -> String {
        layout.focused_channel().map_or("-".to_owned(), letter)
    }

    /// `a | (b over c)`, built the way the user would: split right, then split down.
    fn three() -> (Layout, [PaneId; 3]) {
        let mut layout = Layout::single(Some(ch("a")));
        let a = layout.focused();
        let b = layout.split(a, Direction::Right, Some(ch("b"))).unwrap();
        let c = layout.split(b, Direction::Down, Some(ch("c"))).unwrap();
        (layout, [a, b, c])
    }

    #[test]
    fn split_right_then_down() {
        let (layout, [_, _, c]) = three();
        assert_eq!(shape(&layout), "row(a, col(b, c))");
        assert_eq!(layout.focused(), c, "the new pane gets focus");
        assert_eq!(layout.len(), 3);
        assert!(layout.is_split());
    }

    #[test]
    fn splitting_along_the_parent_shares_the_space_of_the_split_pane() {
        let mut layout = Layout::single(Some(ch("a")));
        let a = layout.focused();
        let b = layout.split(a, Direction::Right, Some(ch("b"))).unwrap();
        layout.split(a, Direction::Right, Some(ch("c")));
        assert_eq!(shape(&layout), "row(a, c, b)");
        assert!(approx(&sizes(&layout), &[0.25, 0.25, 0.5]));
        layout.split(b, Direction::Left, None);
        assert_eq!(shape(&layout), "row(a, c, -, b)");
        assert!(approx(&sizes(&layout), &[0.25, 0.25, 0.25, 0.25]));
    }

    #[test]
    fn closing_gives_the_space_to_the_pane_that_takes_its_place() {
        let (mut layout, [a, b, c]) = three();
        assert!(layout.close(b));
        assert_eq!(shape(&layout), "row(a, c)");
        assert_eq!(layout.focused(), c, "focus was elsewhere and stays");
        layout.focus(c);
        assert!(layout.close(c));
        assert_eq!(shape(&layout), "a");
        assert_eq!(layout.focused(), a);
        assert!(!layout.close(a), "the last pane stays");
        assert!(!layout.close(PaneId(99)));
    }

    #[test]
    fn closing_the_focused_pane_focuses_its_heir_and_merges_same_way_splits() {
        let mut layout = Layout::single(Some(ch("a")));
        let a = layout.focused();
        let b = layout.split(a, Direction::Right, Some(ch("b"))).unwrap();
        let c = layout.split(b, Direction::Down, Some(ch("c"))).unwrap();
        layout.split(c, Direction::Right, Some(ch("d")));
        assert_eq!(shape(&layout), "row(a, col(b, row(c, d)))");
        layout.focus(b);
        assert!(layout.close(b));
        // The column of one collapses and its row merges into the outer row.
        assert_eq!(shape(&layout), "row(a, c, d)");
        assert!(approx(&sizes(&layout), &[0.5, 0.25, 0.25]));
        assert_eq!(focused(&layout), "c");
    }

    #[test]
    fn showing_a_channel_that_has_a_pane_swaps_the_two() {
        let (mut layout, [a, _, c]) = three();
        assert!(layout.show(a, Some(ch("c"))));
        assert_eq!(shape(&layout), "row(c, col(b, a))");
        assert!(!layout.show(a, Some(ch("c"))), "already there");
        assert!(layout.show(c, None));
        assert_eq!(shape(&layout), "row(c, col(b, -))");
        assert_eq!(layout.pane_showing(&ch("c")), Some(a));
    }

    #[test]
    fn dragging_a_pane_beside_another_moves_it() {
        let (mut layout, [a, b, c]) = three();
        assert!(layout.move_pane(a, c, DropZone::Edge(Direction::Down)));
        assert_eq!(shape(&layout), "col(b, c, a)");
        assert_eq!(layout.focused(), a);
        assert!(layout.move_pane(b, a, DropZone::Edge(Direction::Right)));
        assert_eq!(shape(&layout), "col(c, row(a, b))");
        assert!(!layout.move_pane(b, b, DropZone::Edge(Direction::Left)));
    }

    #[test]
    fn dragging_a_pane_into_another_swaps_their_channels() {
        let (mut layout, [a, _, c]) = three();
        assert!(layout.move_pane(a, c, DropZone::Center));
        assert_eq!(shape(&layout), "row(c, col(b, a))");
        assert_eq!(layout.focused(), c);
        assert_eq!(focused(&layout), "a", "focus follows the dragged channel");
    }

    #[test]
    fn dropping_a_sidebar_channel() {
        let (mut layout, [a, b, _]) = three();
        // Into a pane: shown there.
        assert!(layout.drop_channel(ch("d"), a, DropZone::Center));
        assert_eq!(shape(&layout), "row(d, col(b, c))");
        // Beside a pane: a new pane.
        assert!(layout.drop_channel(ch("e"), b, DropZone::Edge(Direction::Up)));
        assert_eq!(shape(&layout), "row(d, col(e, b, c))");
        assert_eq!(focused(&layout), "e");
        // A channel with a pane already: that pane moves.
        assert!(layout.drop_channel(ch("d"), b, DropZone::Edge(Direction::Right)));
        assert_eq!(shape(&layout), "col(e, row(b, d), c)");
        // Onto its own pane: just focus.
        layout.focus(b);
        let d = layout.pane_showing(&ch("d")).unwrap();
        assert!(layout.drop_channel(ch("d"), d, DropZone::Center));
        assert_eq!(focused(&layout), "d");
    }

    #[test]
    fn drop_zones() {
        assert_eq!(DropZone::at(0.5, 0.5), DropZone::Center);
        assert_eq!(DropZone::at(0.1, 0.5), DropZone::Edge(Direction::Left));
        assert_eq!(DropZone::at(0.9, 0.5), DropZone::Edge(Direction::Right));
        assert_eq!(DropZone::at(0.5, 0.05), DropZone::Edge(Direction::Up));
        assert_eq!(DropZone::at(0.5, 0.8), DropZone::Edge(Direction::Down));
        // A corner goes to the nearer side.
        assert_eq!(DropZone::at(0.05, 0.2), DropZone::Edge(Direction::Left));
        assert_eq!(DropZone::at(0.2, 0.95), DropZone::Edge(Direction::Down));
    }

    #[test]
    fn resizing_moves_one_line_and_keeps_a_minimum() {
        let (mut layout, _) = three();
        assert!(layout.resize(&[], 0, 0.3, 0.1));
        assert!(approx(&sizes(&layout), &[0.3, 0.7]));
        assert!(layout.resize(&[], 0, 0.99, 0.1));
        assert!(approx(&sizes(&layout), &[0.9, 0.1]));
        assert!(!layout.resize(&[], 0, 0.99, 0.1), "already at the limit");
        // The column inside the row.
        assert!(layout.resize(&[1], 0, 0.75, 0.1));
        let Node::Split(row) = layout.root() else {
            panic!("a split")
        };
        let Node::Split(col) = &row.children[1] else {
            panic!("a column")
        };
        assert!(approx(&col.sizes, &[0.75, 0.25]));
        assert!(!layout.resize(&[0], 0, 0.5, 0.1), "a pane, not a split");
        assert!(
            !layout.resize(&[], 1, 0.5, 0.1),
            "no line after the last child"
        );
        assert!(!layout.resize(&[], 0, f32::NAN, 0.1));
    }

    #[test]
    fn rects_follow_the_sizes() {
        let (mut layout, [a, b, c]) = three();
        layout.resize(&[], 0, 0.25, 0.1);
        let rects = layout.rects();
        let rect = |id| rects.iter().find(|(i, _)| *i == id).unwrap().1;
        assert_eq!(
            rect(a),
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.25,
                h: 1.0
            }
        );
        assert_eq!(
            rect(b),
            Rect {
                x: 0.25,
                y: 0.0,
                w: 0.75,
                h: 0.5
            }
        );
        assert_eq!(
            rect(c),
            Rect {
                x: 0.25,
                y: 0.5,
                w: 0.75,
                h: 0.5
            }
        );
    }

    #[test]
    fn keyboard_focus_moves_to_the_neighbour_on_that_side() {
        let (layout, [a, b, c]) = three();
        // b and c share as much of a's side: the first in reading order.
        assert_eq!(layout.neighbour(a, Direction::Right), Some(b));
        assert_eq!(layout.neighbour(a, Direction::Left), None);
        assert_eq!(layout.neighbour(b, Direction::Down), Some(c));
        assert_eq!(layout.neighbour(c, Direction::Up), Some(b));
        assert_eq!(layout.neighbour(c, Direction::Left), Some(a));
        assert_eq!(layout.neighbour(b, Direction::Up), None);
        assert_eq!(layout.neighbour(c, Direction::Right), None);
    }

    #[test]
    fn the_nearest_pane_wins_over_a_farther_one_in_line() {
        let mut layout = Layout::single(Some(ch("a")));
        let a = layout.focused();
        let b = layout.split(a, Direction::Right, Some(ch("b"))).unwrap();
        layout.split(b, Direction::Right, Some(ch("c")));
        assert_eq!(layout.neighbour(a, Direction::Right), Some(b));
    }

    #[test]
    fn a_new_pane_shows_the_next_channel_no_pane_shows() {
        let channels = [ch("a"), ch("b"), ch("c")];
        let mut layout = Layout::single(Some(ch("b")));
        assert_eq!(layout.next_unshown(&channels), Some(ch("c")));
        let b = layout.focused();
        layout.split(b, Direction::Right, Some(ch("c")));
        // Focus is on c now: wraps to a.
        assert_eq!(layout.next_unshown(&channels), Some(ch("a")));
        layout.split(layout.focused(), Direction::Down, Some(ch("a")));
        assert_eq!(layout.next_unshown(&channels), None);
        assert_eq!(Layout::single(None).next_unshown(&channels), Some(ch("a")));
    }

    #[test]
    fn closing_a_channel_empties_or_refills_its_pane() {
        let (mut layout, [a, b, _]) = three();
        // d is open but shown nowhere; b is closed in the sidebar at index 1 of [a, b, c, d].
        layout.channel_closed(&ch("b"), &[ch("a"), ch("c"), ch("d")], 1);
        assert_eq!(shape(&layout), "row(a, col(d, c))");
        layout.channel_closed(&ch("d"), &[ch("a"), ch("c")], 2);
        assert_eq!(shape(&layout), "row(a, col(-, c))");
        assert_eq!(layout.leaf(b).unwrap().channel, None);
        // Alone, the pane takes the next channel, else the one before (like the sidebar).
        let mut single = Layout::single(Some(ch("c")));
        single.channel_closed(&ch("c"), &[ch("a"), ch("b")], 2);
        assert_eq!(shape(&single), "b");
        let _ = a;
    }

    #[test]
    fn saved_roundtrip_keeps_the_tree_sizes_and_focus() {
        let (mut layout, [_, b, _]) = three();
        layout.resize(&[], 0, 0.3, 0.1);
        layout.focus(b);
        let list = list(&["a", "b", "c"]);
        let restored = Layout::from_saved(Some(&layout.to_saved()), &list);
        assert_eq!(shape(&restored), "row(a, col(b, c))");
        assert!(approx(&sizes(&restored), &[0.3, 0.7]));
        assert_eq!(focused(&restored), "b");
        assert_eq!(restored.to_saved().focused, 1);
    }

    #[test]
    fn without_a_saved_layout_one_pane_shows_the_selection() {
        let mut list = list(&["a", "b", "c"]);
        list.select(&ch("b"));
        let layout = Layout::from_saved(None, &list);
        assert_eq!(shape(&layout), "b");
        assert_eq!(
            shape(&Layout::from_saved(None, &ChannelList::default())),
            "-"
        );
    }

    #[test]
    fn restoring_checks_channels_sizes_and_shape() {
        let list = list(&["a", "b"]);
        let pane = |channel: &str| SavedPane {
            channel: Some(channel.to_owned()),
            ..SavedPane::default()
        };
        let saved = SavedLayout {
            focused: 7,
            root: SavedPane {
                axis: Some(SavedAxis::Column),
                sizes: vec![f64::NAN, 1.0, 1.0, 1.0],
                panes: vec![
                    pane("AAAA"), // normalised
                    pane("gone"), // not open: empty
                    pane("aaaa"), // shown above: empty
                    SavedPane {
                        // A split of one is its pane; a nested column merges.
                        axis: Some(SavedAxis::Column),
                        panes: vec![pane("#bbbb")],
                        ..SavedPane::default()
                    },
                ],
                ..SavedPane::default()
            },
        };
        let layout = Layout::from_saved(Some(&saved), &list);
        assert_eq!(shape(&layout), "col(a, -, -, b)");
        assert!(
            approx(&sizes(&layout), &[0.25; 4]),
            "NaN evens the split out"
        );
        assert_eq!(
            focused(&layout),
            "a",
            "an index past the end focuses the first"
        );
    }

    #[test]
    fn restoring_a_layout_of_closed_channels_still_shows_the_selection() {
        let list = list(&["a", "b"]);
        let saved = SavedLayout {
            focused: 1,
            root: SavedPane {
                panes: vec![SavedPane::default(), SavedPane::default()],
                sizes: vec![0.5, 0.5],
                ..SavedPane::default()
            },
        };
        let layout = Layout::from_saved(Some(&saved), &list);
        assert_eq!(shape(&layout), "row(-, b)");
    }

    #[test]
    fn restoring_caps_panes_and_depth() {
        let list = list(&["a"]);
        let many = SavedLayout {
            focused: 0,
            root: SavedPane {
                panes: vec![SavedPane::default(); MAX_PANES + 5],
                ..SavedPane::default()
            },
        };
        assert_eq!(Layout::from_saved(Some(&many), &list).len(), MAX_PANES);
        let mut deep = SavedPane::default();
        for _ in 0..20 {
            deep = SavedPane {
                panes: vec![deep, SavedPane::default()],
                axis: Some(SavedAxis::Column),
                ..SavedPane::default()
            };
        }
        let layout = Layout::from_saved(
            Some(&SavedLayout {
                focused: 0,
                root: deep,
            }),
            &list,
        );
        assert!(layout.len() <= MAX_DEPTH + 1);
        // A tiny saved size evens the split out instead of hiding a pane.
        let tiny = SavedLayout {
            focused: 0,
            root: SavedPane {
                sizes: vec![0.001, 1.0],
                panes: vec![SavedPane::default(), SavedPane::default()],
                ..SavedPane::default()
            },
        };
        assert!(approx(
            &sizes(&Layout::from_saved(Some(&tiny), &list)),
            &[0.5, 0.5]
        ));
    }

    #[test]
    fn no_more_than_max_panes() {
        let mut layout = Layout::single(None);
        for _ in 1..MAX_PANES {
            assert!(
                layout
                    .split(layout.focused(), Direction::Right, None)
                    .is_some()
            );
        }
        assert_eq!(layout.split(layout.focused(), Direction::Down, None), None);
        assert_eq!(layout.len(), MAX_PANES);
    }
}
