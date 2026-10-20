//! The tmux verbs that rearrange the tree: place or move a module beside another, swap modules,
//! or even a row or column. They build new trees over the same modules, allowing `shell`'s grid
//! to allocate existing children elsewhere without reparenting widgets. Each ends a zoom first,
//! as tmux's corresponding verbs do.

use super::con::{Child, Con, ConLayout, Node, Pin, MAX_CHILDREN};
use super::geometry::{navigate, Direction, Frame, Nav};
use super::module::ModuleId;
use super::tree::{Axis, Branch, Layout, LayoutError, MAX_RATIO, MIN_RATIO};

/// `Ctrl+a \ <key>` (`Axis::Row`: right of `target`) and `Ctrl+a " <key>` (`Axis::Column`: below
/// it), and Lua's layout split call. Adds a sibling to the immediate parent if its orientation
/// matches; otherwise wraps the target in a new split. Takes half the target's share unless
/// that would make a share too small, in which case the container is evened.
/// An existing module is moved: its old leaf is removed and its sibling takes that space,
/// pin and all. A hidden module becomes shown; an unplaced module joins the tree.
/// `module` then receives the keys.
///
/// Refuses a module equal to `target`, or a target absent from the tree or hidden.
pub fn place(layout: &mut Layout, module: &ModuleId, target: &ModuleId, axis: Axis) -> Result<(), LayoutError> {
    if module == target {
        return Err(LayoutError::SameModule(module.clone()));
    }
    if layout.is_gone(module) {
        return Err(LayoutError::Gone(module.clone()));
    }
    if !layout.contains(target) {
        return Err(LayoutError::NotInTree(target.clone()));
    }
    if !layout.is_shown(target) {
        return Err(LayoutError::Hidden(target.clone()));
    }
    let mut root = layout
        .root_node()
        .without(module)
        .expect("`target` is another module, still in the tree");
    fn sibling(
        root: &mut Node,
        module: &ModuleId,
        target: &ModuleId,
        axis: Axis,
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
            let half = c.children[i].share / 2.0;
            c.children[i].share = half;
            c.children[i].pin = None;
            c.children.insert(i + 1, Child::new(Node::Leaf(module.clone()), half));
            if half < MIN_RATIO {
                let share = 1.0 / c.children.len() as f32;
                for child in &mut c.children {
                    child.share = share;
                    if child.node.leaves().iter().any(|m| m == module || layout.is_shown(m)) {
                        child.pin = None;
                    }
                }
            }
            return Ok(true);
        }
        for child in &mut c.children {
            if child.node.leaves().contains(target) {
                return sibling(&mut child.node, module, target, axis, layout);
            }
        }
        Ok(false)
    }
    if !sibling(&mut root, module, target, axis, layout)? {
        root.replace_leaf(target, |target| {
            Node::split(axis, 0.5, target, Node::Leaf(module.clone()))
        });
    }
    layout.unzoom();
    layout.replace_root(root);
    layout.unhide(module);
    layout.set_focus(module)
}

/// `Ctrl+a H/J/K/L`: `focused` changes places
/// with its neighbour in `dir`, chosen as `Ctrl+h/j/k/l` would choose it (`navigate`, on the
/// unzoomed geometry). The two leaves exchange ids and every split keeps its axis, ratio and pin, so
/// the space stays where it was and the modules move through it. `focused` keeps the keys. With no
/// module that way nothing happens and a zoom stays on (the rule `resize` follows). The neighbour,
/// if they swapped.
pub fn swap(layout: &mut Layout, focused: &ModuleId, dir: Direction, frame: &Frame) -> Option<ModuleId> {
    let Nav::Module(other) = navigate(layout, focused, dir, frame) else {
        return None;
    };
    let mut root = layout.root_node();
    root.swap_leaves(focused, &other);
    layout.unzoom();
    layout.replace_root(root);
    Some(other)
}

/// `Ctrl+a |` (`Axis::Row`) and `Ctrl+a _` (`Axis::Column`) put shown modules in tree order
/// into one row or column with equal shares. Ratios `1/n, 1/(n-1), ...` form a binary chain.
/// A shown bottom row joins the chain and loses its pin so its share is equal too.
/// A hidden bottom row, such as the default terminal, stays around the chain with its pin
/// and length, so showing it restores its height and `F11` does not grow it.
/// Other hidden modules stay hidden at the chain's end, taking no space until shown.
/// Focus stays put.
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
    let mut node = even_chain(axis, items.into_iter().map(Node::Leaf).collect(), n);
    for row in kept.into_iter().rev() {
        node = row.around(node);
    }
    layout.replace_root(node);
}

pub(super) fn even_chain(axis: Axis, items: Vec<Node>, shown: usize) -> Node {
    let mut items: Vec<_> = items.into_iter().enumerate().collect();
    let mut node = items.pop().expect("a chain needs a member").1;
    for (i, item) in items.into_iter().rev() {
        let ratio = if i + 1 < shown { 1.0 / (shown - i) as f32 } else { 0.5 };
        node = Node::split(axis, ratio.clamp(MIN_RATIO, MAX_RATIO), item, node);
    }
    node
}

/// `swap.prev` / `swap.next`: `focused` changes places
/// with the module before or after it among those on screen, in tree order (HINT's order),
/// wrapping. Every split keeps its axis, ratio and pin, as `swap` does. `None`, and nothing moves,
/// for a hidden `focused` or one alone on screen.
pub fn swap_adjacent(layout: &mut Layout, focused: &ModuleId, forward: bool) -> Option<ModuleId> {
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
