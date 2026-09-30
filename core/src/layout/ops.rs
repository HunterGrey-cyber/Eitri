//! The tmux verbs that rearrange the tree (modules spec §4.2, §4.3, §6.3): open a module next to
//! another or move it there, swap two modules, even a row or a column. Each is a new tree over the
//! same modules, never a new widget: `shell`'s grid allocates the same children somewhere else, which
//! is the whole reason it never reparents (§5). Each ends a zoom first, as tmux's own do.

use super::geometry::{navigate, Direction, Frame, Nav};
use super::module::ModuleId;
use super::tree::{Axis, Branch, Layout, LayoutError, Node, Pin, MAX_RATIO, MIN_RATIO};

/// `Ctrl+a \ <key>` (`Axis::Row`: right of `target`) and `Ctrl+a " <key>` (`Axis::Column`: below
/// it), and `eitri.layout.split` (spec §4.3). `module` goes into a new split after `target`, at
/// half of `target`'s space, as tmux's `split-window` makes one. A module already in the tree is
/// MOVED, as tmux's `join-pane` moves a pane -- never duplicated: its old leaf goes, and its sibling
/// takes that space, pin and all. A hidden module is shown by being placed; a module never placed
/// (P3's canvas) joins the tree here. `module` then has the keys, as tmux's new pane does.
///
/// Refused: `module` is `target`; `target` is not in the tree or is hidden (`target` is the module
/// with the keys, which is never hidden).
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
        .root()
        .clone()
        .without(module)
        .expect("`target` is another module, still in the tree");
    root.replace_leaf(target, |target| {
        Node::split(axis, 0.5, target, Node::Leaf(module.clone()))
    });
    layout.unzoom();
    layout.replace_root(root);
    layout.unhide(module);
    layout.set_focus(module)
}

/// `Ctrl+a H/J/K/L` (spec §6.3, `base.conf:64-67`, tmux's `swap-pane`): `focused` changes places
/// with its neighbour in `dir`, chosen as `Ctrl+h/j/k/l` would choose it (`navigate`, on the
/// unzoomed geometry). The two leaves exchange ids and every split keeps its axis, ratio and pin, so
/// the space stays where it was and the modules move through it. `focused` keeps the keys. With no
/// module that way nothing happens and a zoom stays on (the rule `resize` follows). The neighbour,
/// if they swapped.
pub fn swap(layout: &mut Layout, focused: &ModuleId, dir: Direction, frame: &Frame) -> Option<ModuleId> {
    let Nav::Module(other) = navigate(layout, focused, dir, frame) else {
        return None;
    };
    let mut root = layout.root().clone();
    root.swap_leaves(focused, &other);
    layout.unzoom();
    layout.replace_root(root);
    Some(other)
}

/// `Ctrl+a |` (`Axis::Row`) and `Ctrl+a _` (`Axis::Column`) (spec §6.3, `base.conf:53-54`, tmux's
/// `even-horizontal`/`even-vertical`): every shown module, in tree order, goes into one row or one
/// column at equal shares -- the ratios `1/n, 1/(n-1), ...` down a chain, tmux's n cells as a
/// binary tree (§4.1). A bottom row on screen joins them and loses its pin: equal shares are the
/// point. A **hidden** module's bottom row -- the default window's terminal -- is not one of them:
/// it stays around the new chain, pin and length included, so `Ctrl+a t` still opens it below
/// everything at its height and `F11` does not grow it (the plan review's second round, finding 3).
/// Any other hidden module stays hidden at the end of the chain, where it collapses and takes no
/// space until shown. The keys stay put.
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
    let last = items
        .last()
        .expect("the module with the keys is shown, so it is never a kept row")
        .clone();
    let mut node = Node::Leaf(last);
    for (i, id) in items.iter().enumerate().rev().skip(1) {
        let ratio = if i + 1 < n { 1.0 / (n - i) as f32 } else { 0.5 };
        node = Node::split(axis, ratio.clamp(MIN_RATIO, MAX_RATIO), Node::Leaf(id.clone()), node);
    }
    for row in kept.into_iter().rev() {
        node = row.around(node);
    }
    layout.replace_root(node);
}

/// `swap.prev` / `swap.next` (keymap spec §2.1, tmux's `swap-pane -U/-D`): `focused` changes places
/// with the module before or after it among those on screen, in tree order (HINT's order),
/// wrapping. Every split keeps its axis, ratio and pin, as `swap` does. `None`, and nothing moves,
/// for a hidden `focused` or one alone on screen.
pub fn swap_adjacent(layout: &mut Layout, focused: &ModuleId, forward: bool) -> Option<ModuleId> {
    let shown: Vec<ModuleId> = layout.leaves().into_iter().filter(|id| layout.is_shown(id)).collect();
    let at = shown.iter().position(|id| id == focused)?;
    if shown.len() < 2 {
        return None;
    }
    let other = if forward {
        shown[(at + 1) % shown.len()].clone()
    } else {
        shown[(at + shown.len() - 1) % shown.len()].clone()
    };
    let mut root = layout.root().clone();
    root.swap_leaves(focused, &other);
    layout.unzoom();
    layout.replace_root(root);
    Some(other)
}

/// A pinned split whose kept side is a hidden module: the row [`even`] leaves where it is.
struct KeptRow {
    axis: Axis,
    ratio: f32,
    pin: Pin,
    id: ModuleId,
}

impl KeptRow {
    /// The row again, with `rest` where the rest of the tree was.
    fn around(self, rest: Node) -> Node {
        let row = Node::Leaf(self.id);
        let (first, second) = match self.pin.side {
            Branch::First => (row, rest),
            Branch::Second => (rest, row),
        };
        Node::Split {
            axis: self.axis,
            ratio: self.ratio,
            pin: Some(self.pin),
            first: Box::new(first),
            second: Box::new(second),
        }
    }
}

/// Every pinned split whose kept side is a hidden module's leaf -- a bottom row whose module is
/// hidden -- outermost first. [`even`] puts each back around the chain it builds, so the row is below
/// everything the chain holds, as `Ctrl+a t` opens it; a Lua `side` panel that sat beside the row is
/// now in the chain above it. A row on screen is not one: its module is evened with the rest.
fn hidden_bottom_rows(layout: &Layout) -> Vec<KeptRow> {
    fn walk(node: &Node, layout: &Layout, rows: &mut Vec<KeptRow>) {
        let Node::Split {
            axis,
            ratio,
            pin,
            first,
            second,
        } = node
        else {
            return;
        };
        if let Some(pin) = pin {
            let kept = match pin.side {
                Branch::First => first,
                Branch::Second => second,
            };
            if let Node::Leaf(id) = kept.as_ref() {
                if !layout.is_shown(id) {
                    rows.push(KeptRow {
                        axis: *axis,
                        ratio: *ratio,
                        pin: *pin,
                        id: id.clone(),
                    });
                }
            }
        }
        walk(first, layout, rows);
        walk(second, layout, rows);
    }
    let mut rows = Vec::new();
    walk(layout.root(), layout, &mut rows);
    rows
}

#[cfg(test)]
mod tests {
    use super::super::geometry::{arrange, hide, settle_pins, Size};
    use super::super::module::{ModuleDecl, Placement};
    use super::super::tree::{ZoomChange, DEFAULT_EDITOR_SHARE};
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
    fn leaf(id: ModuleId) -> Node {
        Node::Leaf(id)
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

    /// The owner's ask for P2: "the chat can go below or left of the editor" -- `Ctrl+a " a` from
    /// the editor, and `Ctrl+a \ e` from the agent. Moved, never duplicated, and the moved module
    /// has the keys.
    #[test]
    fn the_chat_goes_below_or_left_of_the_editor() {
        let mut layout = Layout::initial(&[]).unwrap();
        place(&mut layout, &agent(), &editor(), Axis::Column).unwrap();
        assert_eq!(
            layout.root(),
            &Node::split(Axis::Column, 0.5, leaf(editor()), leaf(agent()))
        );
        assert_eq!(layout.focus(), &agent());

        let mut layout = Layout::initial(&[]).unwrap();
        layout.set_focus(&agent()).unwrap();
        place(&mut layout, &editor(), &agent(), Axis::Row).unwrap();
        assert_eq!(
            layout.root(),
            &Node::split(Axis::Row, 0.5, leaf(agent()), leaf(editor()))
        );
        assert_eq!(layout.leaves(), [agent(), editor()], "the chat is left of the editor");
    }

    /// `Ctrl+a \ t` with the terminal hidden: it leaves the pinned bottom row -- which collapses,
    /// pin and all -- and opens right of the editor, shown and with the keys.
    #[test]
    fn placing_a_hidden_module_moves_it_out_of_its_old_split_and_shows_it() {
        let mut layout = window();
        place(&mut layout, &term(), &editor(), Axis::Row).unwrap();
        assert_eq!(
            layout.root(),
            &Node::split(
                Axis::Row,
                DEFAULT_EDITOR_SHARE,
                Node::split(Axis::Row, 0.5, leaf(editor()), leaf(term())),
                leaf(agent())
            )
        );
        assert!(layout.is_visible(&term()));
        assert_eq!(layout.focus(), &term());
    }

    /// P3's canvas is placed the first time it draws: a module the tree has never held joins it,
    /// and the MRU list, so `neighbor()` can rank it.
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
        let Node::Split {
            pin: Some(_),
            first: evened,
            second: bottom,
            ..
        } = layout.root()
        else {
            panic!("the hidden terminal's row is kept: {:?}", layout.root());
        };
        assert_eq!(**bottom, leaf(term()));
        let mut pins = 0;
        let mut node = evened.as_ref();
        while let Node::Split { pin, second, .. } = node {
            pins += usize::from(pin.is_some());
            node = second;
        }
        assert_eq!(pins, 0);
        // Shown later, the terminal opens below everything, full width, a third of the height.
        layout.show(&term()).unwrap();
        let a = arrange(&layout, &frame());
        assert_eq!(a.rect_of(&term()).map(|r| (r.x, r.w, r.h)), Some((0, 1280, 240)));
    }

    /// The reviewer's case (the plan review's second round, finding 3): `Ctrl+a |` in the default
    /// window, the terminal hidden -- "make the editor and the chat equal". The terminal's row stays
    /// below them with the height it was left at, so the next `Ctrl+a t` opens a bottom row that
    /// `F11` does not grow; before, it came back as a right-hand column 479px wide at 1920x1041. A
    /// terminal on screen when evened joins the row instead, and its pin goes (owner decision 8).
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

    /// The documented third kind of module `even` meets: hidden, and not a bottom row -- `Ctrl+a x`
    /// in the editor, then `Ctrl+a |`, which the owner can do on day one. The editor stays in the
    /// tree, still hidden, at the end of the chain where it takes no space, and `Ctrl+a e` shows it
    /// back beside the chat. (Task 4's review, minor 1; the whole-branch review's M4 left it out of
    /// the chain and failed nothing -- and in a release build, where `replace_root`'s check is only a
    /// `debug_assert`, `Ctrl+a e` would then have PLACED the editor as a new split.)
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
        assert_eq!(
            layout.root(),
            &Node::split(Axis::Column, 0.5, leaf(editor()), leaf(agent()))
        );
        let heights: Vec<i32> = arrange(&layout, &frame()).modules.iter().map(|(_, r)| r.h).collect();
        assert_eq!(heights, [360, 360]);
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

    /// Ruling 9: tmux's `swap-pane -U/-D` wrap, over the modules on screen in tree order.
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
