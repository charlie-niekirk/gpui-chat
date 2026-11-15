//! Draws the split-pane [`Layout`]: rows and columns of panes with Kit's resize handle on the
//! line between neighbours, an accent outline on the focused pane, and drop zones while a pane (from its
//! header) or a channel (from the sidebar) is dragged over one.
//!
//! The listeners call back into the [`AppShell`], which owns the layout and the panes.

use std::collections::HashMap;

use gpui_chat_core::ChannelId;
use gpui_kit::base::component_traits::Collapsible;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::resizable::resize_handle;
use gpui_kit::component::sidebar::{SidebarItem, SidebarMenuItem};
use gpui_kit::component::{Sizable as _, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, Context, DragMoveEvent, ElementId, EmptyView, Entity,
    InteractiveElement, IntoElement, ParentElement, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, px, relative,
};

use super::{
    Axis, Direction, DropZone, Layout, Leaf, Node, PaneId, Split, divider_fractions, zone_in,
};
use crate::pane::ChannelPane;
use crate::shell::AppShell;
use crate::theme::Tokens;

/// Dragging a line stops when a pane would get narrower (or shorter) than this.
const MIN_PANE: f32 = 220.0;

/// A pane picked up by its header, to move it.
#[derive(Clone, Debug)]
pub struct DraggedPane {
    pub channel: ChannelId,
}

/// A channel picked up in the sidebar, to open it in a split.
#[derive(Clone, Debug)]
pub struct DraggedChannel {
    pub channel: ChannelId,
}

/// The line after child `handle` of the split at `path`.
#[derive(Clone, Debug)]
struct DraggedDivider {
    path: Vec<usize>,
    handle: usize,
}

/// What follows the pointer while a pane or a channel is dragged.
pub struct DragChip {
    label: SharedString,
}

impl DragChip {
    pub fn build(label: impl Into<SharedString>, cx: &mut App) -> Entity<Self> {
        let label = label.into();
        cx.new(|_| Self { label })
    }
}

impl Render for DragChip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = *cx.global::<Tokens>();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(tokens.accent)
            .bg(tokens.title_bar_bg)
            .text_color(tokens.text)
            .text_sm()
            .child(self.label.clone())
    }
}

/// Sidebar rows (Kit's [`SidebarMenuItem`]s, as a `SidebarMenu` lays them out) that can be
/// dragged into a split by their channel.
#[derive(Clone)]
pub struct ChannelRows {
    rows: Vec<(SidebarMenuItem, Option<ChannelId>)>,
    collapsed: bool,
}

impl ChannelRows {
    pub fn new(rows: Vec<(SidebarMenuItem, Option<ChannelId>)>) -> Self {
        Self {
            rows,
            collapsed: false,
        }
    }
}

impl Collapsible for ChannelRows {
    fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    fn is_collapsed(&self) -> bool {
        self.collapsed
    }
}

impl SidebarItem for ChannelRows {
    fn render(
        self,
        id: impl Into<ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let id = id.into();
        let collapsed = self.collapsed;
        v_flex().gap_2().children(
            self.rows
                .into_iter()
                .enumerate()
                .map(|(ix, (item, channel))| {
                    let row_id = SharedString::from(format!("{id}-{ix}"));
                    let row = item
                        .collapsed(collapsed)
                        .render(row_id.clone(), window, cx)
                        .into_any_element();
                    let Some(channel) = channel else {
                        return row;
                    };
                    // A press that does not move still reaches the row as a click.
                    let label = channel.login().to_owned();
                    div()
                        .id(SharedString::from(format!("{row_id}-drag")))
                        .w_full()
                        .child(row)
                        .on_drag(DraggedChannel { channel }, move |_, _, _, cx| {
                            DragChip::build(label.clone(), cx)
                        })
                        .into_any_element()
                }),
        )
    }
}

/// What the view needs from the shell.
pub struct Splits<'a> {
    pub layout: &'a Layout,
    pub panes: &'a HashMap<ChannelId, Entity<ChannelPane>>,
    /// The pane and zone under the pointer during a drag.
    pub drop: Option<(PaneId, DropZone)>,
    pub tokens: Tokens,
}

impl Splits<'_> {
    pub fn render(&self, cx: &mut Context<AppShell>) -> AnyElement {
        self.node(self.layout.root(), Vec::new(), cx)
    }

    fn node(&self, node: &Node, path: Vec<usize>, cx: &mut Context<AppShell>) -> AnyElement {
        match node {
            Node::Leaf(leaf) => self.leaf(leaf, cx),
            Node::Split(split) => self.split(split, path, cx),
        }
    }

    fn split(&self, split: &Split, path: Vec<usize>, cx: &mut Context<AppShell>) -> AnyElement {
        let axis = split.axis;
        let key = path_key(&path);
        let mut container = div()
            .id(SharedString::from(format!("split-{key}")))
            .flex()
            .size_full()
            .min_w_0()
            .min_h_0();
        container = match axis {
            Axis::Row => container.flex_row(),
            Axis::Column => container.flex_col(),
        };
        // Every split hears every divider drag; only the one that owns the line acts on it.
        let own = path.clone();
        container = container.on_drag_move(cx.listener(
            move |this, event: &DragMoveEvent<DraggedDivider>, _, cx| {
                let drag = event.drag(cx).clone();
                if drag.path != own {
                    return;
                }
                let (offset, length) = along(axis, event.bounds, event.event.position);
                if let Some((at, min_pane)) = divider_fractions(offset, length, MIN_PANE) {
                    this.resize_split(&own, drag.handle, at, min_pane, cx);
                }
            },
        ));
        for (i, (child, size)) in split.children.iter().zip(&split.sizes).enumerate() {
            let mut child_path = path.clone();
            child_path.push(i);
            let cell = div()
                .relative()
                .flex()
                .flex_grow(*size)
                .flex_shrink(1.0)
                .flex_basis(px(0.0))
                .min_w_0()
                .min_h_0();
            let cell = match axis {
                Axis::Row => cell.h_full(),
                Axis::Column => cell.w_full(),
            };
            let cell = cell.child(self.node(child, child_path, cx));
            // Every child after the first carries the handle for the line before it, straddling
            // its leading edge (drawn over both neighbours, so the panes need no gap).
            let cell = if i > 0 {
                cell.child(divider(axis, &path, i - 1))
            } else {
                cell
            };
            container = container.child(cell);
        }
        container.into_any_element()
    }

    fn leaf(&self, leaf: &Leaf, cx: &mut Context<AppShell>) -> AnyElement {
        let id = leaf.id;
        let split = self.layout.is_split();
        let outlined = split && self.layout.focused() == id;
        let content = match leaf.channel.as_ref().and_then(|c| self.panes.get(c)) {
            Some(pane) => pane.clone().into_any_element(),
            None => empty_pane(id, split, &self.tokens, cx),
        };
        let drop = self
            .drop
            .filter(|(target, _)| *target == id && cx.has_active_drag())
            .map(|(_, zone)| zone);
        div()
            .id(("pane-slot", id.get()))
            .relative()
            .size_full()
            .overflow_hidden()
            .min_w_0()
            .min_h_0()
            // Capture: a press anywhere in the pane focuses it, even where a child stops the
            // event (the reply button, a link).
            .capture_any_mouse_down(cx.listener(move |this, _, _, cx| this.pane_pressed(id, cx)))
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedPane>, _, cx| {
                    if let Some(zone) = zone_under(event.bounds, event.event.position) {
                        let channel = event.drag(cx).channel.clone();
                        this.drag_pane_over(&channel, id, zone, cx);
                    }
                }),
            )
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedChannel>, _, cx| {
                    if let Some(zone) = zone_under(event.bounds, event.event.position) {
                        let channel = event.drag(cx).channel.clone();
                        this.drag_channel_over(&channel, id, zone, cx);
                    }
                }),
            )
            .on_drop(cx.listener(move |this, dragged: &DraggedPane, _, cx| {
                this.drop_pane(&dragged.channel, id, cx);
            }))
            .on_drop(cx.listener(move |this, dragged: &DraggedChannel, _, cx| {
                this.drop_channel(dragged.channel.clone(), id, cx);
            }))
            .child(content)
            .children(outlined.then(|| {
                div()
                    .absolute()
                    .inset_0()
                    .border_1()
                    .border_color(self.tokens.accent.opacity(0.8))
            }))
            .children(drop.map(|zone| drop_overlay(zone, &self.tokens)))
            .into_any_element()
    }
}

/// `0.1.2` for the split at children 0, 1, 2 (`root` for the outermost).
fn path_key(path: &[usize]) -> String {
    if path.is_empty() {
        return "root".to_owned();
    }
    path.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// The pointer's offset into `bounds` and their length, along `axis`.
fn along(axis: Axis, bounds: Bounds<Pixels>, position: Point<Pixels>) -> (f32, f32) {
    match axis {
        Axis::Row => (
            (position.x - bounds.origin.x).as_f32(),
            bounds.size.width.as_f32(),
        ),
        Axis::Column => (
            (position.y - bounds.origin.y).as_f32(),
            bounds.size.height.as_f32(),
        ),
    }
}

/// The drop zone under `position`, if it is over `bounds` at all.
fn zone_under(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<DropZone> {
    zone_in(
        (position.x - bounds.origin.x).as_f32(),
        (position.y - bounds.origin.y).as_f32(),
        bounds.size.width.as_f32(),
        bounds.size.height.as_f32(),
    )
}

/// Kit's resize handle for the line after child `handle` of the split at `path`: its band,
/// cursor and themed hairline/pill; the drag is followed by the split's `on_drag_move`.
fn divider(axis: Axis, path: &[usize], handle: usize) -> impl IntoElement {
    let kit_axis = match axis {
        Axis::Row => gpui_kit::Axis::Horizontal,
        Axis::Column => gpui_kit::Axis::Vertical,
    };
    resize_handle::<DraggedDivider, EmptyView>(
        SharedString::from(format!("divider-{}-{handle}", path_key(path))),
        kit_axis,
    )
    .on_drag(
        DraggedDivider {
            path: path.to_vec(),
            handle,
        },
        |_, _, _, cx| cx.new(|_| EmptyView),
    )
}

/// Shades where a drop would land: the whole pane, or the half on that side.
fn drop_overlay(zone: DropZone, tokens: &Tokens) -> impl IntoElement {
    let shade = div()
        .absolute()
        .rounded_md()
        .border_2()
        .border_color(tokens.accent.opacity(0.9))
        .bg(tokens.accent.opacity(0.18));
    match zone {
        DropZone::Center => shade.inset_0(),
        DropZone::Edge(Direction::Left) => shade.left_0().top_0().bottom_0().w(relative(0.5)),
        DropZone::Edge(Direction::Right) => shade.right_0().top_0().bottom_0().w(relative(0.5)),
        DropZone::Edge(Direction::Up) => shade.top_0().left_0().right_0().h(relative(0.5)),
        DropZone::Edge(Direction::Down) => shade.bottom_0().left_0().right_0().h(relative(0.5)),
    }
}

/// A pane with no channel: a hint, and (in a split) a way to close it.
fn empty_pane(
    id: PaneId,
    closable: bool,
    tokens: &Tokens,
    cx: &mut Context<AppShell>,
) -> AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(tokens.main_bg)
        .child(
            div()
                .text_sm()
                .text_color(tokens.text_muted)
                .child("No channel in this pane"),
        )
        .child(
            div()
                .text_xs()
                .text_color(tokens.text_muted)
                .child("Pick one in the sidebar, or drag one here."),
        )
        .children(closable.then(|| {
            Button::new(("close-empty-pane", id.get()))
                .small()
                .ghost()
                .label("Close Pane")
                .on_click(cx.listener(move |this, _, _, cx| this.close_pane(id, cx)))
        }))
        .into_any_element()
}
