//! The tmux verbs that rearrange the tree: open a module next to
//! another or move it there, swap two modules, even a row or a column. Each is a new tree over the
//! same modules, never a new widget: `shell`'s grid allocates the same children somewhere else, which
//! is the whole reason it never reparents. Each ends a zoom first, as tmux's own do.

use super::con::{insert_halving, Child, Con, ConLayout, Node, Pin, MAX_CHILDREN};
use super::geometry::{navigate, Direction, Frame, Nav};
use super::module::ModuleId;
use super::tree::{Axis, Branch, Layout, LayoutError, MAX_RATIO, MIN_RATIO};

/// `Ctrl+a \ <key>` (`Axis::Row`: right of `target`) and `Ctrl+a " <key>` (`Axis::Column`: below
/// it), and `eitri.layout.split`. The immediate parent receives a sibling when its orientation
/// matches; otherwise a new split wraps the target. Each takes half the target's share unless
/// that would make a share too small, in which case the container is evened. A module already in the tree is
/// MOVED, as tmux's `join-pane` moves a pane -- never duplicated: its old leaf goes, and its sibling
/// takes that space, pin and all. A hidden module is shown by being placed; a module never placed
/// (the canvas) joins the tree here. `module` then has the keys, as tmux's new pane does.
///
/// Refused: `module` is `target`; `target` is not in the tree or is hidden (`target` is the module
/// with the keys, which is never hidden).
pub fn place(layout: &mut Layout, module: &ModuleId, target: &ModuleId, axis: Axis) -> Result<(), LayoutError> {
    place_side(layout, module, target, axis, false)
}

fn place_side(
    layout: &mut Layout,
    module: &ModuleId,
    target: &ModuleId,
    axis: Axis,
    before: bool,
) -> Result<(), LayoutError> {
    let anchor;
    let target = if layout.is_floating(target) {
        anchor = layout
            .tiled_anchor()
            .ok_or_else(|| LayoutError::Floating(target.clone()))?;
        &anchor
    } else {
        target
    };
    if module == target {
        return Err(LayoutError::SameModule(module.clone()));
    }
    if layout.is_gone(module) {
        return Err(LayoutError::Gone(module.clone()));
    }
    if layout.is_away(module) {
        return Err(LayoutError::Away(module.clone()));
    }
    if !layout.contains(target) {
        return Err(LayoutError::NotInTree(target.clone()));
    }
    if !layout.is_shown(target) {
        return Err(LayoutError::Hidden(target.clone()));
    }
    let mut root = layout
        .detach_tile(module)
        .map_err(|_| LayoutError::LastTiled(module.clone()))?;
    fn sibling(
        root: &mut Node,
        module: &ModuleId,
        target: &ModuleId,
        axis: Axis,
        before: bool,
        layout: &Layout,
    ) -> Result<bool, LayoutError> {
        let Node::Con(c) = root else {
            return Ok(false);
        };
        if let Some(i) = c
            .children
            .iter()
            .position(|ch| matches!(&ch.node, Node::Leaf(id) if id == target))
        {
            if c.layout != ConLayout::of_axis(axis) {
                return Ok(false);
            }
            if c.children.len() >= MAX_CHILDREN {
                return Err(LayoutError::TooMany { limit: MAX_CHILDREN });
            }
            let even = c.children[i].share < 2.0 * MIN_RATIO;
            c.children[i].pin = None;
            insert_halving(c, i, Node::Leaf(module.clone()));
            if before {
                c.children.swap(i, i + 1);
            }
            if even {
                for child in &mut c.children {
                    if child.node.leaves().iter().any(|m| m == module || layout.is_shown(m)) {
                        child.pin = None;
                    }
                }
            }
            return Ok(true);
        }
        for child in &mut c.children {
            if child.node.leaves().contains(target) {
                return sibling(&mut child.node, module, target, axis, before, layout);
            }
        }
        Ok(false)
    }
    if !sibling(&mut root, module, target, axis, before, layout)? {
        root.replace_leaf(target, |target| {
            if before {
                Node::split(axis, 0.5, Node::Leaf(module.clone()), target)
            } else {
                Node::split(axis, 0.5, target, Node::Leaf(module.clone()))
            }
        });
    }
    layout.take_float(module);
    layout.unzoom();
    layout.replace_root(root);
    layout.unhide(module);
    layout.set_focus(module)
}

/// [`place`] on any side: `Left` and `Up` put `module` before `target`, `Right` and `Down` after it.
pub fn place_at(layout: &mut Layout, module: &ModuleId, target: &ModuleId, dir: Direction) -> Result<(), LayoutError> {
    let (axis, before) = match dir {
        Direction::Left => (Axis::Row, true),
        Direction::Right => (Axis::Row, false),
        Direction::Up => (Axis::Column, true),
        Direction::Down => (Axis::Column, false),
    };
    place_side(layout, module, target, axis, before)
}

/// `module` as a tab beside `target`: into `target`'s own tabbed or stacked container right after it, else both
/// into a new tabbed container in `target`'s place. `draw` makes it the drawn tab and gives it the keys; without
/// it, the drawn tab and the keys stay. The same refusals as [`place`].
pub fn place_as_tab(layout: &mut Layout, module: &ModuleId, target: &ModuleId, draw: bool) -> Result<(), LayoutError> {
    let anchor;
    let target = if layout.is_floating(target) {
        anchor = layout
            .tiled_anchor()
            .ok_or_else(|| LayoutError::Floating(target.clone()))?;
        &anchor
    } else {
        target
    };
    if module == target {
        return Err(LayoutError::SameModule(module.clone()));
    }
    if layout.is_gone(module) {
        return Err(LayoutError::Gone(module.clone()));
    }
    if layout.is_away(module) {
        return Err(LayoutError::Away(module.clone()));
    }
    if !layout.contains(target) {
        return Err(LayoutError::NotInTree(target.clone()));
    }
    if !layout.is_shown(target) {
        return Err(LayoutError::Hidden(target.clone()));
    }
    let mut root = layout
        .detach_tile(module)
        .map_err(|_| LayoutError::LastTiled(module.clone()))?;
    fn into_tabs(root: &mut Node, module: &ModuleId, target: &ModuleId, draw: bool) -> Result<bool, LayoutError> {
        let Node::Con(c) = root else { return Ok(false) };
        if let Some(i) = c
            .children
            .iter()
            .position(|ch| matches!(&ch.node, Node::Leaf(id) if id == target))
        {
            if !matches!(c.layout, ConLayout::Tabbed | ConLayout::Stacked) {
                return Ok(false);
            }
            if c.children.len() >= MAX_CHILDREN {
                return Err(LayoutError::TooMany { limit: MAX_CHILDREN });
            }
            insert_halving(c, i, Node::Leaf(module.clone()));
            if draw {
                c.focus = i + 1;
            }
            return Ok(true);
        }
        for child in &mut c.children {
            if child.node.leaves().contains(target) {
                return into_tabs(&mut child.node, module, target, draw);
            }
        }
        Ok(false)
    }
    if !into_tabs(&mut root, module, target, draw)? {
        root.replace_leaf(target, |target| {
            let mut tabs = Con::new(
                ConLayout::Tabbed,
                vec![Child::new(target, 0.5), Child::new(Node::Leaf(module.clone()), 0.5)],
            );
            tabs.focus = usize::from(draw);
            Node::Con(tabs)
        });
    }
    layout.take_float(module);
    layout.unzoom();
    layout.replace_root(root);
    layout.unhide(module);
    if draw {
        layout.set_focus(module)
    } else {
        Ok(())
    }
}

/// `Ctrl+a H/J/K/L` (`base.conf:64-67`, tmux's `swap-pane`): `focused` changes places
/// with its neighbour in `dir`, chosen as `Ctrl+h/j/k/l` would choose it (`navigate`, on the
/// unzoomed geometry). The two leaves exchange ids and every split keeps its axis, ratio and pin, so
/// the space stays where it was and the modules move through it. `focused` keeps the keys. With no
/// module that way nothing happens and a zoom stays on (the rule `resize` follows). The neighbour,
/// if they swapped.
pub fn swap(layout: &mut Layout, focused: &ModuleId, dir: Direction, frame: &Frame) -> Option<ModuleId> {
    if layout.is_floating(focused) {
        return None;
    }
    let Nav::Module(other) = navigate(layout, focused, dir, frame) else {
        return None;
    };
    let mut root = layout.root_node();
    root.swap_leaves(focused, &other);
    layout.unzoom();
    layout.replace_root(root);
    Some(other)
}

/// Equal shares put every shown module in one row or column and clear its pin. Hidden bottom
/// rows keep their placement and length around that container, so showing one restores its row.
/// Other hidden modules stay at the end, where they take no space until shown. The keys stay put.
pub fn even(layout: &mut Layout, axis: Axis) {
    layout.unzoom();
    let kept = hidden_bottom_rows(layout);
    let (shown, hidden): (Vec<ModuleId>, Vec<ModuleId>) = layout
        .leaves()
        .into_iter()
        .filter(|id| kept.iter().all(|row| row.id != *id))
        .partition(|id| layout.is_shown(id));
    let n = shown.len();
    let items: Vec<ModuleId> = shown.into_iter().chain(hidden).collect();
    let mut node = even_con(axis, items.into_iter().map(Node::Leaf).collect(), n);
    for row in kept.into_iter().rev() {
        node = row.around(node);
    }
    layout.replace_root(node);
}

pub(super) fn even_con(axis: Axis, mut items: Vec<Node>, shown: usize) -> Node {
    if items.len() > MAX_CHILDREN {
        // Keep a shown module beside the hidden tail so collapsing it preserves that module's slot.
        let prefix = shown.min(MAX_CHILDREN) - 1;
        let mut tail: Vec<_> = items.split_off(prefix).into_iter().enumerate().collect();
        let mut node = tail.pop().expect("an overflow tail needs a member").1;
        for (i, item) in tail.into_iter().rev() {
            let ratio = if i + 1 < shown - prefix {
                1.0 / (shown - prefix - i) as f32
            } else {
                0.5
            };
            node = Node::split(axis, ratio.clamp(MIN_RATIO, MAX_RATIO), item, node);
        }
        items.push(node);
    }
    if items.len() == 1 {
        return items.pop().unwrap();
    }
    let share = 1.0 / items.len() as f32;
    Node::Con(Con::new(
        ConLayout::of_axis(axis),
        items.into_iter().map(|item| Child::new(item, share)).collect(),
    ))
}

/// `swap.prev` / `swap.next` (tmux's `swap-pane -U/-D`): `focused` changes places
/// with the module before or after it among those on screen, in tree order (HINT's order),
/// wrapping. Every split keeps its axis, ratio and pin, as `swap` does. `None`, and nothing moves,
/// for a hidden `focused` or one alone on screen.
pub fn swap_adjacent(layout: &mut Layout, focused: &ModuleId, forward: bool) -> Option<ModuleId> {
    if layout.is_floating(focused) {
        return None;
    }
    let shown: Vec<ModuleId> = layout.leaves().into_iter().filter(|id| layout.is_in_view(id)).collect();
    let at = shown.iter().position(|id| id == focused)?;
    if shown.len() < 2 {
        return None;
    }
    let other = if forward {
        shown[(at + 1) % shown.len()].clone()
    } else {
        shown[(at + shown.len() - 1) % shown.len()].clone()
    };
    let mut root = layout.root_node();
    root.swap_leaves(focused, &other);
    layout.unzoom();
    layout.replace_root(root);
    Some(other)
}

/// A pinned child that is a hidden module, beside the rest of its container: the row [`even`] leaves
/// where it is.
pub(super) struct KeptRow {
    axis: Axis,
    /// The share of the side before the divider, as a split of the row and the rest reads it.
    ratio: f32,
    /// Which side of that split the row is on.
    side: Branch,
    px: Option<i32>,
    pub(super) id: ModuleId,
}

impl KeptRow {
    /// The row again, with `rest` where the rest of the tree was.
    pub(super) fn around(self, rest: Node) -> Node {
        let row = Node::Leaf(self.id);
        let (first, second) = match self.side {
            Branch::First => (row, rest),
            Branch::Second => (rest, row),
        };
        let Node::Con(mut c) = Node::pinned(self.axis, self.ratio, self.side, first, second) else {
            unreachable!("a split is a container")
        };
        c.children[usize::from(self.side == Branch::Second)].pin = Some(Pin { px: self.px });
        Node::Con(c)
    }
}

/// Every pinned child that is a hidden module's leaf and governs the length on its side -- a bottom
/// row whose module is hidden -- outermost first. [`even`] puts each back around the chain it builds,
/// so the row is below everything the chain holds, as `Ctrl+a t` opens it; a Lua `side` panel that sat
/// beside the row is now in the chain above it. A row on screen is not one: its module is evened with
/// the rest.
pub(super) fn hidden_bottom_rows(layout: &Layout) -> Vec<KeptRow> {
    fn walk(c: &Con, layout: &Layout, rows: &mut Vec<KeptRow>) {
        let n = c.children.len();
        if let Some(axis) = c.layout.axis() {
            for (i, child) in c.children.iter().enumerate() {
                let (Some(pin), Node::Leaf(id)) = (child.pin, &child.node) else {
                    continue;
                };
                // Once shown, the last child's pin governs unless the child before it on screen has a
                // pin of its own; children with nothing shown take no step, so they are passed over,
                // as the arrangement passes over them.
                let last = i + 1 == n;
                let before_has_pin = || {
                    c.children[..i]
                        .iter()
                        .rev()
                        .find(|ch| ch.node.leaves().iter().any(|m| layout.is_shown(m)))
                        .is_some_and(|ch| ch.pin.is_some())
                };
                if n < 2 || layout.is_shown(id) || (last && before_has_pin()) {
                    continue;
                }
                let others: f32 = c
                    .children
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, ch)| ch.share)
                    .sum();
                rows.push(KeptRow {
                    axis,
                    ratio: if last { others } else { child.share },
                    side: if last { Branch::Second } else { Branch::First },
                    px: pin.px,
                    id: id.clone(),
                });
            }
        }
        for child in &c.children {
            if let Node::Con(inner) = &child.node {
                walk(inner, layout, rows);
            }
        }
    }
    let mut rows = Vec::new();
    walk(layout.root(), layout, &mut rows);
    rows
}

#[cfg(test)]
mod tests {
    use super::super::fixture::fixture;
    use super::super::geometry::{arrange, hide, settle_pins, Size};
    use super::super::module::{ModuleDecl, Placement};
    use super::super::test_support::{same_rects, same_rects_opened};
    use super::super::tree::{ZoomChange, BELOW_ROOT_SHARE, DEFAULT_EDITOR_SHARE};
    use super::*;

    fn editor() -> ModuleId {
        ModuleId::editor()
    }
    fn agent() -> ModuleId {
        ModuleId::agent()
    }
    fn term() -> ModuleId {
        ModuleId::terminal()
    }
    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 721 }, 1)
    }
    /// The window as it starts: `[editor | agent]` over the hidden terminal.
    fn window() -> Layout {
        let mut layout = Layout::initial(&[ModuleDecl {
            id: term(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        hide(&mut layout, &term(), &frame()).unwrap();
        layout
    }

    /// The chat can move below or left of the editor: `Ctrl+a " a` from the editor,
    /// or `Ctrl+a \ e` from the agent. Modules are moved without duplication,
    /// and the moved module receives the keys.
    #[test]
    fn the_chat_goes_below_or_left_of_the_editor() {
        let mut layout = Layout::initial(&[]).unwrap();
        place(&mut layout, &agent(), &editor(), Axis::Column).unwrap();
        same_rects(&layout, &fixture("column(editor 0.5, agent)"));
        assert_eq!(layout.focus(), &agent());

        let mut layout = Layout::initial(&[]).unwrap();
        layout.set_focus(&agent()).unwrap();
        place(&mut layout, &editor(), &agent(), Axis::Row).unwrap();
        same_rects(&layout, &fixture("row(agent 0.5, editor)"));
        assert_eq!(layout.leaves(), [agent(), editor()], "the chat is left of the editor");
    }

    /// `Ctrl+a \ t` with the terminal hidden: it leaves the pinned bottom row -- which collapses,
    /// pin and all -- and opens right of the editor, shown and with the keys.
    #[test]
    fn placing_a_hidden_module_moves_it_out_of_its_old_split_and_shows_it() {
        let mut layout = window();
        place(&mut layout, &term(), &editor(), Axis::Row).unwrap();
        same_rects(
            &layout,
            &Layout::new(
                Node::Con(Con::new(
                    ConLayout::SplitH,
                    vec![
                        Child::new(Node::Leaf(editor()), DEFAULT_EDITOR_SHARE / 2.0),
                        Child::new(Node::Leaf(term()), DEFAULT_EDITOR_SHARE / 2.0),
                        Child::new(Node::Leaf(agent()), 1.0 - DEFAULT_EDITOR_SHARE),
                    ],
                )),
                term(),
            )
            .unwrap(),
        );
        assert!(layout.is_visible(&term()));
        assert_eq!(layout.focus(), &term());
    }

    /// A previously unplaced module joins both the tree and the MRU list,
    /// so `neighbor()` can rank it.
    #[test]
    fn a_module_never_placed_joins_the_tree() {
        let mut layout = Layout::initial(&[]).unwrap();
        let canvas = ModuleId::parse("canvas").unwrap();
        place(&mut layout, &canvas, &agent(), Axis::Row).unwrap();
        assert_eq!(layout.leaves(), [editor(), agent(), canvas.clone()]);
        assert_eq!(layout.mru().first(), Some(&canvas));
        assert_eq!(layout.mru().len(), 3);
    }

    #[test]
    fn placing_ends_a_zoom_and_refuses_itself_and_a_hidden_target() {
        let mut layout = window();
        assert_eq!(layout.toggle_zoom(&editor()), ZoomChange::Zoomed);
        place(&mut layout, &agent(), &editor(), Axis::Column).unwrap();
        assert_eq!(layout.zoomed(), None);

        let mut layout = window();
        let before = layout.clone();
        assert_eq!(
            place(&mut layout, &editor(), &editor(), Axis::Row),
            Err(LayoutError::SameModule(editor()))
        );
        assert_eq!(
            place(&mut layout, &agent(), &term(), Axis::Row),
            Err(LayoutError::Hidden(term()))
        );
        assert_eq!(
            place(&mut layout, &agent(), &ModuleId::lua("gone"), Axis::Row),
            Err(LayoutError::NotInTree(ModuleId::lua("gone")))
        );
        assert_eq!(layout, before, "a refused place changes nothing");
    }

    /// `Ctrl+a L` from the editor: the two change places, the divider stays where it was -- the
    /// agent now has the editor's 760px -- and the editor keeps the keys.
    #[test]
    fn swapping_exchanges_the_modules_and_keeps_the_space() {
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(swap(&mut layout, &editor(), Direction::Right, &frame()), Some(agent()));
        let a = arrange(&layout, &frame());
        assert_eq!(a.rect_of(&agent()).map(|r| (r.x, r.w)), Some((0, 760)));
        assert_eq!(a.rect_of(&editor()).map(|r| r.x), Some(761));
        assert_eq!(layout.focus(), &editor());
    }

    /// A swap with the pinned bottom row: the module that comes down takes the pinned height.
    #[test]
    fn a_swap_keeps_the_pin_where_it_is() {
        let mut layout = window();
        layout.show(&term()).unwrap();
        assert!(settle_pins(&mut layout, &frame()));
        layout.set_focus(&term()).unwrap();
        assert_eq!(swap(&mut layout, &term(), Direction::Up, &frame()), Some(editor()));
        assert_eq!(arrange(&layout, &frame()).rect_of(&editor()).map(|r| r.h), Some(240));
        let taller = Frame::new(Size { w: 1280, h: 1041 }, 1);
        assert_eq!(arrange(&layout, &taller).rect_of(&editor()).map(|r| r.h), Some(240));
    }

    #[test]
    fn a_swap_with_nothing_that_way_does_nothing_and_leaves_a_zoom_on() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.toggle_zoom(&editor());
        let before = layout.clone();
        assert_eq!(swap(&mut layout, &editor(), Direction::Left, &frame()), None);
        assert_eq!(layout, before);
    }

    /// `Ctrl+a |` over three shown modules: one row, equal widths (to the pixel the handles
    /// leave), no pin among them, the zoom ended -- and the hidden terminal still in its own pinned
    /// row, now below all three (the `side` panel it sat beside is in the row), so showing it opens a
    /// bottom row, as `Ctrl+a t` always has.
    #[test]
    fn even_puts_every_shown_module_in_one_row_at_equal_widths() {
        let mut layout = Layout::initial(&[
            ModuleDecl {
                id: term(),
                placement: Placement::BelowRoot,
            },
            ModuleDecl {
                id: ModuleId::lua("side"),
                placement: Placement::RightOfRoot,
            },
        ])
        .unwrap();
        hide(&mut layout, &term(), &frame()).unwrap();
        layout.toggle_zoom(&agent());
        even(&mut layout, Axis::Row);
        assert_eq!(layout.zoomed(), None);
        assert_eq!(layout.leaves(), [editor(), agent(), ModuleId::lua("side"), term()]);
        assert!(!layout.is_shown(&term()));
        let widths: Vec<i32> = arrange(&layout, &frame()).modules.iter().map(|(_, r)| r.w).collect();
        assert_eq!(widths, [426, 426, 426]);
        // The terminal's row is kept, pinned, around a row with no pin: shown and settled, the two
        // place every module alike at every size.
        same_rects_opened(
            &layout,
            &fixture(&format!(
                "column(row(editor, agent, lua:side) {BELOW_ROOT_SHARE}, terminal hidden pinned)"
            )),
        );
        // Shown later, the terminal opens below everything, full width, a third of the height.
        layout.show(&term()).unwrap();
        let a = arrange(&layout, &frame());
        assert_eq!(a.rect_of(&term()).map(|r| (r.x, r.w, r.h)), Some((0, 1280, 240)));
    }

    /// `Ctrl+a |` with the terminal hidden makes the editor and chat equal while preserving the
    /// terminal's bottom row and stored height. A later `Ctrl+a t` restores that row, and `F11`
    /// does not grow it. A terminal that is shown when evened joins the row and loses its pin.
    #[test]
    fn a_hidden_bottom_row_keeps_its_place_and_its_height_through_even() {
        let mut layout = window();
        layout.show(&term()).unwrap();
        assert!(settle_pins(&mut layout, &frame()));
        hide(&mut layout, &term(), &frame()).unwrap();
        even(&mut layout, Axis::Row);
        layout.show(&term()).unwrap();
        let fullscreen = Frame::new(Size { w: 1920, h: 1041 }, 1);
        let a = arrange(&layout, &fullscreen);
        assert_eq!(a.rect_of(&term()).map(|r| (r.x, r.w, r.h)), Some((0, 1920, 240)));
        assert_eq!(a.rect_of(&editor()).map(|r| (r.y, r.w)), Some((0, 960)));
        assert_eq!(a.rect_of(&agent()).map(|r| (r.y, r.w)), Some((0, 959)));

        even(&mut layout, Axis::Row);
        assert!(layout.is_shown(&term()));
        assert_eq!(layout.leaves(), [editor(), agent(), term()]);
        let widths: Vec<i32> = arrange(&layout, &frame()).modules.iter().map(|(_, r)| r.w).collect();
        assert_eq!(widths, [426, 426, 426]);
    }

    /// A hidden module outside a bottom row stays in the tree when `even` rebuilds it.
    /// The hidden editor goes at the chain's end and takes no space. `Ctrl+a e` then shows
    /// it beside the chat. Omitting it would make showing it create a new split in release
    /// builds, where `replace_root` checks the invariant only with `debug_assert`.
    #[test]
    fn a_hidden_module_that_is_not_a_bottom_row_stays_hidden_at_the_end_of_the_chain() {
        let mut layout = window();
        hide(&mut layout, &editor(), &frame()).unwrap();
        even(&mut layout, Axis::Row);
        assert_eq!(layout.leaves(), [agent(), editor(), term()]);
        assert!(!layout.is_shown(&editor()));
        let a = arrange(&layout, &frame());
        assert_eq!(a.rect_of(&editor()), None, "hidden, it takes no space");
        assert_eq!(a.rect_of(&agent()).map(|r| r.w), Some(1280));
        assert!(layout.show(&editor()).unwrap());
        let a = arrange(&layout, &frame());
        assert_eq!(a.rect_of(&agent()).map(|r| (r.x, r.w)), Some((0, 640)));
        assert_eq!(a.rect_of(&editor()).map(|r| (r.x, r.w)), Some((641, 639)));
    }

    #[test]
    fn even_vertical_stacks_them_at_equal_heights() {
        let mut layout = Layout::initial(&[]).unwrap();
        even(&mut layout, Axis::Column);
        same_rects(&layout, &fixture("column(editor, agent)"));
        let heights: Vec<i32> = arrange(&layout, &frame()).modules.iter().map(|(_, r)| r.h).collect();
        assert_eq!(heights, [360, 360]);
    }

    /// Two hidden bottom rows at the end of one column: the last row's pin governs once it is shown
    /// alone, because the hidden row before it takes no step, so `even` keeps both rows and the
    /// terminal reopens as its full-width 120px row.
    #[test]
    fn even_keeps_the_last_hidden_row_when_the_row_before_it_is_hidden_too() {
        use super::super::con::{Child, ConLayout};
        use std::collections::BTreeSet;
        let bottom = ModuleId::lua("bottom");
        let pinned = |node: Node, px: i32| Child {
            pin: Some(Pin { px: Some(px) }),
            ..Child::new(node, 0.25)
        };
        let root = Node::Con(Con::new(
            ConLayout::SplitV,
            vec![
                Child::new(Node::Leaf(editor()), 0.25),
                Child::new(Node::Leaf(agent()), 0.25),
                pinned(Node::Leaf(bottom.clone()), 80),
                pinned(Node::Leaf(term()), 120),
            ],
        ));
        let mut layout = Layout::from_parts(root, BTreeSet::from([bottom.clone(), term()]), editor()).unwrap();
        let kept: Vec<ModuleId> = hidden_bottom_rows(&layout).into_iter().map(|row| row.id).collect();
        assert_eq!(kept, [bottom.clone(), term()]);
        even(&mut layout, Axis::Row);
        layout.root().check(true).unwrap();
        layout.show(&term()).unwrap();
        let a = arrange(&layout, &frame());
        assert_eq!(
            a.rect_of(&term()).map(|r| (r.x, r.y + r.h, r.w, r.h)),
            Some((0, 721, 1280, 120)),
            "a full-width 120px row at the bottom"
        );
        layout.show(&bottom).unwrap();
        let a = arrange(&layout, &frame());
        assert_eq!(
            a.rect_of(&bottom).map(|r| (r.w, r.h)),
            Some((1280, 80)),
            "the other row kept its length too"
        );
    }
    #[test]
    fn a_module_placed_left_or_above_goes_before_its_target() {
        use crate::layout::fixture::fixture;
        use crate::layout::geometry::{Direction, Size};
        use crate::layout::test_support::{rects, same_rects};
        let size = Size { w: 1280, h: 721 };
        let base = || fixture("row(editor focus 0.6, agent)");
        let agent_before = rects(&base(), size)[&agent()];

        let mut layout = base();
        place_at(&mut layout, &term(), &editor(), Direction::Left).unwrap();
        let r = rects(&layout, size);
        let (t, e, a) = (r[&term()], r[&editor()], r[&agent()]);
        assert!(t.x < e.x && e.x < a.x, "{t:?} {e:?} {a:?}");
        assert!((t.w - e.w).abs() <= 1, "the two halve the editor's share: {t:?} {e:?}");
        // The new divider takes a pixel from the shares around it, so the agent's left edge may move by one.
        assert!(
            a.x + a.w == agent_before.x + agent_before.w && (a.w - agent_before.w).abs() <= 1,
            "the agent keeps its place to a pixel: {a:?} {agent_before:?}"
        );
        assert_eq!(layout.focus(), &term());
        assert_eq!(
            layout.root().children.len(),
            3,
            "a sibling in the row, not a nested split"
        );

        let mut layout = base();
        place_at(&mut layout, &term(), &editor(), Direction::Up).unwrap();
        let r = rects(&layout, size);
        let (t, e) = (r[&term()], r[&editor()]);
        assert!(t.y < e.y && t.x == e.x && t.w == e.w, "{t:?} {e:?}");

        for (dir, axis) in [(Direction::Right, Axis::Row), (Direction::Down, Axis::Column)] {
            let (mut by_side, mut by_place) = (base(), base());
            place_at(&mut by_side, &term(), &editor(), dir).unwrap();
            place(&mut by_place, &term(), &editor(), axis).unwrap();
            same_rects(&by_side, &by_place);
        }
    }

    #[test]
    fn a_module_placed_as_a_tab_joins_the_targets_tab_container_or_makes_one() {
        use crate::layout::fixture::fixture;
        fn tabs(c: &Con) -> Option<usize> {
            if matches!(c.layout, ConLayout::Tabbed | ConLayout::Stacked) {
                return Some(c.children.len());
            }
            c.children.iter().find_map(|ch| match &ch.node {
                Node::Con(inner) => tabs(inner),
                Node::Leaf(_) => None,
            })
        }
        let mut layout = fixture("row(editor focus 0.6, agent)");
        place_as_tab(&mut layout, &term(), &editor(), true).unwrap();
        assert_eq!(tabs(layout.root()), Some(2));
        assert!(layout.is_visible(&term()) && layout.is_tabbed_away(&editor()));
        assert_eq!(layout.focus(), &term());

        let t2 = ModuleId::parse("terminal#2").unwrap();
        place_as_tab(&mut layout, &t2, &term(), true).unwrap();
        assert_eq!(
            tabs(layout.root()),
            Some(3),
            "joins the container the target is a tab of"
        );
        assert!(layout.is_visible(&t2) && layout.is_tabbed_away(&term()) && layout.is_tabbed_away(&editor()));

        let mut layout = fixture("row(editor focus 0.6, agent)");
        place_as_tab(&mut layout, &term(), &editor(), false).unwrap();
        assert_eq!(tabs(layout.root()), Some(2));
        assert!(
            layout.is_visible(&editor()) && layout.is_tabbed_away(&term()),
            "not drawn, keys unmoved"
        );
        assert_eq!(layout.focus(), &editor());
        assert_eq!(
            place_as_tab(&mut layout, &editor(), &editor(), true),
            Err(LayoutError::SameModule(editor()))
        );
    }
}

#[cfg(test)]
mod swap_adjacent_tests {
    use super::super::geometry::{hide, Frame, Size};
    use super::super::module::{ModuleDecl, ModuleId, Placement};
    use super::super::tree::Layout;
    use super::swap_adjacent;

    fn position(layout: &Layout, id: &ModuleId) -> usize {
        layout.leaves().iter().position(|m| m == id).unwrap()
    }

    /// Like tmux's `swap-pane -U/-D`, adjacent swaps wrap, over the modules on screen in tree order.
    #[test]
    fn swap_adjacent_wraps_over_the_modules_on_screen_and_skips_a_hidden_one() {
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        hide(&mut layout, &ModuleId::terminal(), &frame).unwrap();
        let (editor, agent, terminal) = (
            position(&layout, &ModuleId::editor()),
            position(&layout, &ModuleId::agent()),
            position(&layout, &ModuleId::terminal()),
        );
        assert_eq!(
            swap_adjacent(&mut layout, &ModuleId::editor(), true),
            Some(ModuleId::agent())
        );
        assert_eq!(position(&layout, &ModuleId::editor()), agent);
        assert_eq!(position(&layout, &ModuleId::agent()), editor);
        assert_eq!(
            position(&layout, &ModuleId::terminal()),
            terminal,
            "the hidden module does not move"
        );
        // Two on screen: forward from the last wraps to the first, never to the hidden terminal.
        assert_eq!(
            swap_adjacent(&mut layout, &ModuleId::editor(), true),
            Some(ModuleId::agent())
        );
        assert_eq!(
            swap_adjacent(&mut layout, &ModuleId::editor(), false),
            Some(ModuleId::agent())
        );
    }

    #[test]
    fn swap_adjacent_does_nothing_for_a_hidden_module_or_a_lone_one() {
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        hide(&mut layout, &ModuleId::terminal(), &frame).unwrap();
        assert_eq!(swap_adjacent(&mut layout, &ModuleId::terminal(), true), None);
        hide(&mut layout, &ModuleId::agent(), &frame).unwrap();
        let before = layout.leaves();
        assert_eq!(swap_adjacent(&mut layout, &ModuleId::editor(), true), None);
        assert_eq!(layout.leaves(), before);
    }
}

#[cfg(test)]
mod placement_tests {
    use crate::layout::*;
    fn start() -> Layout {
        Layout::new(
            Node::split(
                Axis::Row,
                0.6,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap()
    }
    #[test]
    fn a_split_along_the_parents_orientation_inserts_a_sibling() {
        let mut l = start();
        let root = l.root().id;
        place(&mut l, &ModuleId::lua("a"), &ModuleId::editor(), Axis::Row).unwrap();
        assert_eq!(l.root().id, root);
        for (ch, expected) in l.root().children.iter().zip([0.3, 0.3, 0.4]) {
            assert!((ch.share - expected).abs() < 1e-6);
        }
        assert_eq!(l.focus(), &ModuleId::lua("a"));
        place(&mut l, &ModuleId::lua("b"), &ModuleId::editor(), Axis::Column).unwrap();
        assert_eq!(l.root().children[0].node.as_con().unwrap().layout, ConLayout::SplitV);
        assert_eq!(l.root().children[0].share, 0.3);
    }
    #[test]
    fn splitting_the_newest_module_until_a_half_is_too_small_evens_the_container() {
        let mut l = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        let mut target = ModuleId::agent();
        for i in 1..=4 {
            let next = ModuleId::lua(&format!("m{i}"));
            place(&mut l, &next, &target, Axis::Row).unwrap();
            target = next;
            if i == 3 {
                assert_eq!(
                    l.root().children.iter().map(|ch| ch.share).collect::<Vec<_>>(),
                    vec![0.5, 0.25, 0.125, 0.0625, 0.0625]
                );
            }
        }
        for ch in &l.root().children {
            assert!((ch.share - 1.0 / 6.0).abs() < 1e-6);
        }
        let a = arrange(&l, &Frame::new(Size { w: 1280, h: 721 }, 1));
        let widths: Vec<_> = a.modules.iter().map(|(_, r)| r.w).collect();
        assert!(
            widths.iter().max().unwrap() - widths.iter().min().unwrap() <= 1,
            "{widths:?}"
        );
    }
    #[test]
    fn a_twenty_first_child_is_refused() {
        let children = (0..20)
            .map(|i| Child::new(Node::Leaf(ModuleId::lua(&format!("m{i}"))), 0.05))
            .collect();
        let mut l = Layout::new(Node::Con(Con::new(ConLayout::SplitH, children)), ModuleId::lua("m0")).unwrap();
        let before = l.clone();
        assert_eq!(
            place(&mut l, &ModuleId::lua("x"), &ModuleId::lua("m0"), Axis::Row),
            Err(LayoutError::TooMany { limit: 20 })
        );
        assert_eq!(l, before);
    }
}
