//! The geometry over a [`Layout`]: where each module goes, where each divider
//! goes, which module is "next to" which, and what `Ctrl+a h/j/k/l` moves. Pure integer arithmetic
//! over the size `shell` hands in; nothing here measures a widget.
//!
//! **Why the rectangles collapse.** A split with nothing visible on one side gives the whole
//! rectangle to the other side and draws no divider. That reproduces, on purpose, what a `GtkPaned`
//! with one visible child does -- which is exactly what today's zoom and a hidden bottom slot rely
//! on. A zoom is the extreme case: the zoomed module alone, at full size.
//!
//! **A pinned child** (`con::Pin`) keeps the length it stores and the children after it
//! share the rest, within everyone's minimums -- so a bottom row keeps its height when the window
//! grows. Until the pin has a length its step divides by the shares, and [`settle_pins`] records that
//! length the first time it is on screen beside another child. The arithmetic is
//! [`super::arrange`]'s; this file turns a [`Layout`] into its inputs.

use super::arrange::{arrange_tree, con_min, move_divider_in, settle_in, Divider};
use super::con::{ConId, ConLayout};
use super::float::{float_floor, float_parts};
use super::module::ModuleId;
use super::tree::{Axis, Layout, LayoutError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The four `Ctrl+h/j/k/l` directions, and the four `Ctrl+a h/j/k/l` resizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

pub const DEFAULT_STRIP_PX: i32 = 24;

/// What the geometry is computed against: the container's size, the divider's thickness, and each
/// module's minimum size (what its widget measures; a module with none returns `Size::default()`).
pub struct Frame<'a> {
    pub size: Size,
    pub handle_px: i32,
    pub strip_px: i32,
    pub min: &'a dyn Fn(&ModuleId) -> Size,
}

fn no_minimum(_: &ModuleId) -> Size {
    Size::default()
}

impl Frame<'static> {
    /// A frame in which no module has a minimum size.
    pub fn new(size: Size, handle_px: i32) -> Frame<'static> {
        Frame {
            size,
            handle_px,
            strip_px: DEFAULT_STRIP_PX,
            min: &no_minimum,
        }
    }
}

/// Every module on screen with its rectangle, in tree order, and every divider on screen.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Arrangement {
    pub modules: Vec<(ModuleId, Rect)>,
    pub dividers: Vec<Divider>,
    pub strips: Vec<Strip>,
    /// Floats on screen, bottom first.
    pub floats: Vec<FloatSpot>,
    /// Navigation extends the top edge over each enclosing strip without crossing another module.
    pub cells: Vec<(ModuleId, Rect)>,
}

/// A float on screen: its fitted rectangle, top bar and module host, in grid pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatSpot {
    pub id: ModuleId,
    pub rect: Rect,
    pub bar: Rect,
    pub host: Rect,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Strip {
    pub con: ConId,
    pub layout: ConLayout,
    pub rect: Rect,
    pub entries: Vec<ConStripEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConStripEntry {
    pub id: ModuleId,
    pub active: bool,
}

impl Arrangement {
    pub fn float_spot(&self, id: &ModuleId) -> Option<&FloatSpot> {
        self.floats.iter().find(|spot| &spot.id == id)
    }

    pub fn cell_of(&self, id: &ModuleId) -> Option<Rect> {
        self.cells.iter().find(|(m, _)| m == id).map(|(_, r)| *r)
    }
    pub fn rect_of(&self, id: &ModuleId) -> Option<Rect> {
        self.modules.iter().find(|(m, _)| m == id).map(|(_, r)| *r)
    }
}

/// Where everything goes now, zoom included: what `shell` allocates.
pub fn arrange(layout: &Layout, frame: &Frame) -> Arrangement {
    arrange_with(layout, true, frame)
}

/// What the layout needs at the least: the minimum size of what is on screen -- the visible tree,
/// or the zoomed module alone. `shell`'s grid reports it as its own minimum, so the window cannot be
/// made smaller than a module's floor, as a `GtkPaned` with `shrink = false` never let it be: below
/// it, [`arrange`] keeps the first child's minimum and gives the second whatever is left, so a
/// bottom module's 80px would be the first thing lost.
pub fn min_size(layout: &Layout, frame: &Frame) -> Size {
    if let Some(zoomed) = layout.zoomed() {
        return (frame.min)(zoomed);
    }
    let shown = |id: &ModuleId| layout.is_shown(id);
    let root = match layout.zoom() {
        Some(super::tree::Zoom::Con(id)) => layout.root().find(*id).unwrap(),
        _ => layout.root(),
    };
    con_min(root, &shown, frame).unwrap_or_default()
}

/// The same, as if nothing were zoomed. Navigation and resize decide on this, because tmux decides
/// on the unzoomed window and unzooms only once it knows it will act.
pub(super) fn arrange_with(layout: &Layout, honour_zoom: bool, frame: &Frame) -> Arrangement {
    let full = Rect {
        x: 0,
        y: 0,
        w: frame.size.w.max(0),
        h: frame.size.h.max(0),
    };
    let mut out = Arrangement::default();
    if honour_zoom {
        if let Some(zoomed) = layout.zoomed() {
            out.modules.push((zoomed.clone(), full));
            out.cells.push((zoomed.clone(), full));
            return out;
        }
    }
    let shown = |id: &ModuleId| layout.is_shown(id);
    let root = match (honour_zoom, layout.zoom()) {
        (true, Some(super::tree::Zoom::Con(id))) => layout.root().find(*id).unwrap(),
        _ => layout.root(),
    };
    let mut out = arrange_tree(root, full, &shown, frame);
    if !(honour_zoom && layout.is_zoomed()) {
        for float in layout.floats().iter().filter(|float| layout.is_shown(&float.id)) {
            let rect = float.rect.place(frame.size, float_floor(&float.id, frame));
            let (bar, host) = float_parts(rect, frame.strip_px);
            out.floats.push(FloatSpot {
                id: float.id.clone(),
                rect,
                bar,
                host,
            });
        }
    }
    out
}

/// Whether two spans `[a, a + a_len)` and `[b, b + b_len)` share at least one pixel.
fn overlaps(a: i32, a_len: i32, b: i32, b_len: i32) -> bool {
    a.max(b) < (a + a_len).min(b + b_len)
}

/// The gap from `from`'s edge in `dir` to `to`'s facing edge, if `to` lies on that side with an
/// overlapping span. Siblings are exactly `handle_px` apart; anything within that counts.
fn gap(from: &Rect, to: &Rect, dir: Direction, handle_px: i32) -> Option<i32> {
    let (gap, overlap) = match dir {
        Direction::Right => (to.x - (from.x + from.w), overlaps(from.y, from.h, to.y, to.h)),
        Direction::Left => (from.x - (to.x + to.w), overlaps(from.y, from.h, to.y, to.h)),
        Direction::Down => (to.y - (from.y + from.h), overlaps(from.x, from.w, to.x, to.w)),
        Direction::Up => (from.y - (to.y + to.h), overlaps(from.x, from.w, to.x, to.w)),
    };
    (overlap && (0..=handle_px.max(0)).contains(&gap)).then_some(gap)
}

/// tmux's rule: among the modules whose facing edge is adjacent to `from`'s edge in
/// `dir` with an overlapping span, the nearest; ties go to the most recently used (`mru`, most
/// recent first), then to the top-left one. In a layout built only from splits every adjacent
/// module shares the same divider, so "nearest" never decides anything here and MRU does.
pub fn neighbor(
    modules: &[(ModuleId, Rect)],
    from: &ModuleId,
    dir: Direction,
    mru: &[ModuleId],
    handle_px: i32,
) -> Option<ModuleId> {
    let (_, origin) = modules.iter().find(|(id, _)| id == from)?;
    let rank = |id: &ModuleId| mru.iter().position(|m| m == id).unwrap_or(usize::MAX);
    modules
        .iter()
        .filter(|(id, _)| id != from)
        .filter_map(|(id, rect)| gap(origin, rect, dir, handle_px).map(|g| (g, rank(id), rect.y, rect.x, id)))
        .min_by_key(|(g, r, y, x, _)| (*g, *r, *y, *x))
        .map(|(.., id)| id.clone())
}

/// Where `Ctrl+h/j/k/l` from `from` goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nav {
    Module(ModuleId),
    /// `Up` with nothing above: the top bar, which sits above every module (`Ctrl+k`, unchanged).
    TopBar,
    /// Nothing in that direction: tmux's no-op at the edge of its grid.
    Nothing,
}

/// `neighbor()` on the unzoomed geometry. The caller unzooms only when this says `Module`, as
/// tmux's `select-pane` does, and today's `'R'`/`Ctrl+h` arms did.
pub fn navigate(layout: &Layout, from: &ModuleId, dir: Direction, frame: &Frame) -> Nav {
    let arrangement = arrange_with(layout, false, frame);
    if layout.is_floating(from) {
        let Some(spot) = arrangement.float_spot(from) else {
            return Nav::Nothing;
        };
        let centre = |r: Rect| (i64::from(r.x + r.w / 2), i64::from(r.y + r.h / 2));
        let (cx, cy) = centre(spot.rect);
        let on_side = |r| {
            let (x, y) = centre(r);
            match dir {
                Direction::Left => x < cx,
                Direction::Right => x > cx,
                Direction::Up => y < cy,
                Direction::Down => y > cy,
            }
        };
        if let Some((_, next)) = arrangement
            .floats
            .iter()
            .enumerate()
            .filter(|(_, f)| &f.id != from && on_side(f.rect))
            .min_by_key(|(z, f)| {
                let (x, y) = centre(f.rect);
                ((x - cx).pow(2) + (y - cy).pow(2), std::cmp::Reverse(*z))
            })
        {
            return Nav::Module(next.id.clone());
        }
        if let Some((id, _)) = arrangement
            .cells
            .iter()
            .filter(|(_, r)| on_side(*r))
            .min_by_key(|(id, r)| {
                let x = cx.clamp(i64::from(r.x), i64::from(r.x + r.w));
                let y = cy.clamp(i64::from(r.y), i64::from(r.y + r.h));
                let rank = layout.mru().iter().position(|m| m == id).unwrap_or(usize::MAX);
                ((cx - x).pow(2) + (cy - y).pow(2), rank, r.y, r.x)
            })
        {
            return Nav::Module(id.clone());
        }
        return arrangement
            .cells
            .iter()
            .find(|(_, r)| {
                cx >= i64::from(r.x) && cx < i64::from(r.x + r.w) && cy >= i64::from(r.y) && cy < i64::from(r.y + r.h)
            })
            .map_or(Nav::Nothing, |(id, _)| Nav::Module(id.clone()));
    }
    if arrangement.rect_of(from).is_none() {
        // `from` is hidden, or not in the tree: nothing moves from it -- not even up to the top bar,
        // which is only "above" a module that is on screen.
        return Nav::Nothing;
    }
    for (c, i) in layout.ancestors(from).into_iter().rev() {
        let forward = match (c.layout, dir) {
            (ConLayout::Tabbed, Direction::Right) | (ConLayout::Stacked, Direction::Down) => true,
            (ConLayout::Tabbed, Direction::Left) | (ConLayout::Stacked, Direction::Up) => false,
            _ => continue,
        };
        let next = if forward {
            (i + 1..c.children.len()).find_map(|j| c.children[j].node.focused_leaf(&|m| layout.is_shown(m)))
        } else {
            (0..i)
                .rev()
                .find_map(|j| c.children[j].node.focused_leaf(&|m| layout.is_shown(m)))
        };
        if let Some(id) = next {
            return Nav::Module(id);
        }
    }
    match neighbor(&arrangement.cells, from, dir, layout.mru(), frame.handle_px) {
        Some(id) => Nav::Module(id),
        None if dir == Direction::Up => Nav::TopBar,
        None => Nav::Nothing,
    }
}

/// The divider on `leaf`'s edge in `dir` (`trailing`) or on the opposite edge.
fn edge_divider(arrangement: &Arrangement, leaf: Rect, dir: Direction, trailing: bool) -> Option<&Divider> {
    let horizontal = matches!(dir, Direction::Left | Direction::Right);
    arrangement.dividers.iter().find(|d| {
        let r = d.rect;
        if horizontal {
            d.axis == Axis::Row
                && overlaps(leaf.y, leaf.h, r.y, r.h)
                && if trailing {
                    r.x == leaf.x + leaf.w
                } else {
                    r.x + r.w == leaf.x
                }
        } else {
            d.axis == Axis::Column
                && overlaps(leaf.x, leaf.w, r.x, r.w)
                && if trailing {
                    r.y == leaf.y + leaf.h
                } else {
                    r.y + r.h == leaf.y
                }
        }
    })
}

/// `Ctrl+a h/j/k/l`: move the divider on `focused`'s trailing edge (right, bottom) by
/// `px`, or its leading edge when there is none, left/up for `Left`/`Up`. For `[editor | agent]`
/// that is exactly the old `resize_target`. The divider is chosen on the unzoomed geometry and the
/// zoom ends only once one is found: a direction with no divider is a no-op, and a no-op must not
/// silently undo a zoom. `false` when nothing moved for that reason.
pub fn resize(layout: &mut Layout, focused: &ModuleId, dir: Direction, px: i32, frame: &Frame) -> bool {
    if layout.is_floating(focused) {
        return super::verbs::resize_float(layout, focused, dir, px, frame);
    }
    let arrangement = arrange_with(layout, false, frame);
    let Some(leaf) = arrangement.cell_of(focused) else {
        return false;
    };
    let Some(divider) =
        edge_divider(&arrangement, leaf, dir, true).or_else(|| edge_divider(&arrangement, leaf, dir, false))
    else {
        return false;
    };
    let sign = match dir {
        Direction::Left | Direction::Up => -1,
        Direction::Right | Direction::Down => 1,
    };
    let divider = divider.clone();
    layout.unzoom();
    move_divider(layout, &divider, divider.first_px + sign * px).is_ok()
}

/// Puts `divider` where the child before it gets `first_px`, kept off both sides' minimums: shares
/// traded with the next shown child when no pin governs that step, the governing pin's pixels when one
/// does. What a divider drag and `Ctrl+a h/j/k/l` do. `divider` is one [`arrange`] returned for this
/// layout; one no longer on screen is refused.
pub fn move_divider(layout: &mut Layout, divider: &Divider, first_px: i32) -> Result<(), LayoutError> {
    let shown = layout.shown_set();
    if move_divider_in(layout.root_mut(), divider, first_px, &|id| shown.contains(id)) {
        Ok(())
    } else {
        Err(LayoutError::NoSuchDivider)
    }
}

/// Gives every pin that governs a step on screen, and has no length yet, the length the shares give it
/// now -- from then on it keeps that length. `true` if one was given. `shell`'s grid calls this with
/// its real allocation before it arranges, so a bottom row's first show is a third of the height at
/// the window's size then (`main`'s `shown_position`), and after that the row keeps its height when
/// the window grows (`main`'s `resize_end_child(false)`).
pub fn settle_pins(layout: &mut Layout, frame: &Frame) -> bool {
    let arrangement = arrange(layout, frame);
    let shown = layout.shown_set();
    let mut settled = false;
    for divider in &arrangement.dividers {
        settled |= settle_in(layout.root_mut(), divider, &|id| shown.contains(id));
    }
    settled
}

/// Hides `id`. If `id` holds the keys, first chooses where they go -- the most
/// recently used of its geometric neighbours, else the most recently used module still shown --
/// and returns it: `shell` must give that module GTK focus **before** it unmaps `id`, or GTK's own
/// focus chain picks (it was seen to pick the WebView). `Ok(None)` when `id` did not hold the keys.
pub fn hide(layout: &mut Layout, id: &ModuleId, frame: &Frame) -> Result<Option<ModuleId>, LayoutError> {
    let next = if layout.focus() == id {
        let arrangement = arrange_with(layout, false, frame);
        let adjacent: Vec<ModuleId> = [Direction::Left, Direction::Up, Direction::Right, Direction::Down]
            .into_iter()
            .filter_map(|dir| neighbor(&arrangement.modules, id, dir, layout.mru(), frame.handle_px))
            .collect();
        let tab_next = layout.ancestors(id).into_iter().rev().find_map(|(c, i)| {
            if c.layout.axis().is_some() {
                return None;
            }
            let mut next = c.clone();
            let shown = |m: &ModuleId| m != id && layout.is_shown(m);
            if next.drawn(&shown) == Some(i) {
                return None;
            }
            next.focus = i;
            next.focused_leaf(&shown)
        });
        tab_next.or_else(|| {
            layout
                .mru()
                .iter()
                .find(|m| adjacent.contains(m))
                .or_else(|| layout.mru().iter().find(|m| *m != id && layout.is_in_view(m)))
                .cloned()
        })
    } else {
        None
    };
    layout.hide_unfocused(id)?;
    if let Some(next) = &next {
        layout.set_focus(next)?;
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::super::arrange::MIN_VISIBLE_LEAF_PX;
    use super::super::con::{ConId, Node};
    use super::super::module::{ModuleDecl, Placement};
    use super::super::tree::{ZoomChange, MAX_RATIO, MIN_RATIO};
    use super::*;

    const HANDLE: i32 = 1;
    /// The default window's content area: 1280 wide, and 760 less a 39px top bar (read off its CSS,
    /// as `tree::BELOW_ROOT_SHARE` says; not measured).
    const WINDOW: Size = Size { w: 1280, h: 721 };

    fn editor() -> ModuleId {
        ModuleId::editor()
    }
    fn agent() -> ModuleId {
        ModuleId::agent()
    }
    fn bottom() -> ModuleId {
        ModuleId::lua("bottom")
    }

    /// Today's window: `[editor | agent]`.
    fn default_tree() -> Layout {
        Layout::initial(&[]).unwrap()
    }

    /// Today's window with a Lua panel in the bottom slot: `[editor | agent]` over it.
    fn bottom_tree() -> Layout {
        Layout::initial(&[ModuleDecl {
            id: bottom(),
            placement: Placement::BelowRoot,
        }])
        .unwrap()
    }

    fn frame() -> Frame<'static> {
        Frame::new(WINDOW, HANDLE)
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn the_default_tree_splits_the_width_at_the_ratio_with_one_divider() {
        let layout = default_tree();
        let a = arrange(&layout, &frame());
        // 1279 * (760 / 1279) = 760: where `main`'s `GtkPaned` put it.
        assert_eq!(
            a.modules,
            vec![(editor(), rect(0, 0, 760, 721)), (agent(), rect(761, 0, 519, 721))]
        );
        assert_eq!(a.dividers.len(), 1);
        let d = &a.dividers[0];
        assert_eq!(
            (d.con, d.index, d.axis, d.rect),
            (layout.root().id, 0, Axis::Row, rect(760, 0, 1, 721))
        );
        assert_eq!((d.avail, d.first_px), (1279, 760));
    }

    #[test]
    fn a_bottom_panel_spans_the_full_width_under_the_row() {
        let a = arrange(&bottom_tree(), &frame());
        // Column: 720 * 2/3 = 480, where `build_vertical_split` put it. Row inside it as above.
        assert_eq!(
            a.modules,
            vec![
                (editor(), rect(0, 0, 760, 480)),
                (agent(), rect(761, 0, 519, 480)),
                (bottom(), rect(0, 481, 1280, 240)),
            ]
        );
        let dividers: Vec<(Axis, Rect)> = a.dividers.iter().map(|d| (d.axis, d.rect)).collect();
        assert_eq!(
            dividers,
            vec![(Axis::Column, rect(0, 480, 1280, 1)), (Axis::Row, rect(760, 0, 1, 480))]
        );
    }

    /// The collapse rule: a hidden module's space goes to its sibling and its divider goes away --
    /// `GtkPaned`'s behaviour with one visible child, which today's zoom relies on.
    #[test]
    fn a_hidden_module_collapses_and_its_sibling_takes_the_space() {
        let mut layout = bottom_tree();
        hide(&mut layout, &bottom(), &frame()).unwrap();
        let a = arrange(&layout, &frame());
        assert_eq!(
            a.modules,
            vec![(editor(), rect(0, 0, 760, 721)), (agent(), rect(761, 0, 519, 721))]
        );
        assert_eq!(a.dividers.len(), 1);

        let mut layout = bottom_tree();
        hide(&mut layout, &agent(), &frame()).unwrap();
        hide(&mut layout, &editor(), &frame()).unwrap();
        let a = arrange(&layout, &frame());
        assert_eq!(a.modules, vec![(bottom(), rect(0, 0, 1280, 721))]);
        assert!(a.dividers.is_empty(), "a whole collapsed subtree draws no divider");
    }

    #[test]
    fn a_zoom_is_one_module_at_full_size_and_no_dividers() {
        let mut layout = bottom_tree();
        layout.toggle_zoom(&agent());
        let a = arrange(&layout, &frame());
        assert_eq!(a.modules, vec![(agent(), rect(0, 0, 1280, 721))]);
        assert!(a.dividers.is_empty());
    }

    /// A module's minimum size keeps the divider from shrinking it below that size.
    /// The bottom module has an 80px minimum height here.
    #[test]
    fn a_minimum_size_holds_the_divider_off_a_module() {
        let mut layout = bottom_tree();
        let column = arrange(&layout, &frame()).dividers[0].clone();
        assert_eq!(column.axis, Axis::Column);
        layout.set_ratio(&column, 0.95).unwrap();
        let min = |id: &ModuleId| {
            if *id == bottom() {
                Size { w: 0, h: 80 }
            } else {
                Size::default()
            }
        };
        let f = Frame {
            size: WINDOW,
            handle_px: HANDLE,
            strip_px: DEFAULT_STRIP_PX,
            min: &min,
        };
        let a = arrange(&layout, &f);
        assert_eq!(a.rect_of(&bottom()), Some(rect(0, 641, 1280, 80)));
        assert_eq!(a.dividers[0].min_second, 80);
        // Dragging the divider further down is held at the same place.
        assert_eq!(a.dividers[0].ratio_for(700), 640.0 / 720.0);
    }

    /// Seen 2026-09-27: a nested split whose outer ratio is pushed toward
    /// `MAX_RATIO` can compute a shown, focused leaf's rect to zero height even though nothing
    /// hides it. `agent | (terminal / editor)`, terminal's own minimum 80px and editor's 0 (as a
    /// `GtkGLArea` editor host normally reports): the outer split, capped at the inner subtree's
    /// combined minimum, hands the inner split exactly enough room for terminal's floor and no
    /// slack, and the inner split's own clamp then gives terminal all of it. Editor never
    /// disappears from `is_shown`/focus -- it is simply drawn, and clickable, over 0 pixels.
    #[test]
    fn a_nested_split_never_collapses_a_shown_focused_leaf_to_zero_pixels() {
        let root = Node::split(
            Axis::Column,
            MAX_RATIO,
            Node::Leaf(agent()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::terminal()),
                Node::Leaf(editor()),
            ),
        );
        let layout = Layout::new(root, editor()).unwrap();
        let min = |id: &ModuleId| {
            if *id == ModuleId::terminal() {
                Size { w: 0, h: 80 }
            } else {
                Size::default()
            }
        };
        let f = Frame {
            size: WINDOW,
            handle_px: HANDLE,
            strip_px: DEFAULT_STRIP_PX,
            min: &min,
        };
        let a = arrange(&layout, &f);
        let editor_rect = a.rect_of(&editor()).expect("editor is shown");
        // An editor floor must be usable, not merely positive: a 1px editor is as invisible and
        // impossible to click as a 0px editor.
        assert!(
            editor_rect.h >= 24,
            "editor is shown and focused, but its rect is {editor_rect:?}"
        );
        assert_eq!(
            editor_rect.h, MIN_VISIBLE_LEAF_PX,
            "the floor, and no more: the rest is the ratio's"
        );
        let terminal_rect = a.rect_of(&ModuleId::terminal()).expect("terminal is shown");
        assert_eq!(terminal_rect.h, 80, "the terminal keeps its own real minimum");
    }

    /// The grid reports this as its own minimum (`ModuleGrid::measure`), so the window cannot be made
    /// shorter than a bottom module's floor, as `GtkPaned` with `shrink = false` never let it be.
    /// Without it, the test above's rule -- the first child keeps its minimum -- takes the bottom
    /// module's 80px first.
    #[test]
    fn the_layouts_minimum_is_what_is_on_screen_needs() {
        let min = |id: &ModuleId| {
            if *id == bottom() {
                Size { w: 0, h: 80 }
            } else {
                Size { w: 10, h: 0 }
            }
        };
        let f = Frame {
            size: WINDOW,
            handle_px: HANDLE,
            strip_px: DEFAULT_STRIP_PX,
            min: &min,
        };
        let mut layout = bottom_tree();
        // Across: the editor, the divider, the agent. Down: the row, the divider, the bottom's 80.
        assert_eq!(min_size(&layout, &f), Size { w: 21, h: 81 });
        layout.toggle_zoom(&editor());
        assert_eq!(
            min_size(&layout, &f),
            Size { w: 10, h: 0 },
            "a zoom needs only the zoomed module"
        );
        layout.unzoom();
        hide(&mut layout, &bottom(), &f).unwrap();
        assert_eq!(
            min_size(&layout, &f),
            Size { w: 21, h: 0 },
            "a hidden module needs nothing"
        );
    }

    /// Nothing panics and nothing goes negative at sizes GTK hands out before the first real
    /// allocation, or when minimums cannot all be met.
    #[test]
    fn degenerate_sizes_give_empty_rectangles_not_panics() {
        for size in [Size { w: 0, h: 0 }, Size { w: -5, h: 3 }, Size { w: 1, h: 1 }] {
            let a = arrange(&bottom_tree(), &Frame::new(size, HANDLE));
            for (_, r) in &a.modules {
                assert!(r.w >= 0 && r.h >= 0, "{size:?}: {r:?}");
            }
        }
        let min = |_: &ModuleId| Size { w: 500, h: 500 };
        let f = Frame {
            size: Size { w: 600, h: 400 },
            handle_px: HANDLE,
            strip_px: DEFAULT_STRIP_PX,
            min: &min,
        };
        let a = arrange(&default_tree(), &f);
        // The first child keeps its minimum, the second gets what is left: GtkPaned's priority.
        assert_eq!(a.rect_of(&editor()).map(|r| r.w), Some(500));
        assert_eq!(a.rect_of(&agent()).map(|r| r.w), Some(99));
    }

    #[test]
    fn a_drag_ratio_round_trips_to_the_same_pixel() {
        let a = arrange(&default_tree(), &frame());
        let d = &a.dividers[0];
        for px in [64, 300, 760, 1000, 1215] {
            let mut layout = default_tree();
            let d = arrange(&layout, &frame()).dividers[0].clone();
            layout.set_ratio(&d, d.ratio_for(px)).unwrap();
            assert_eq!(arrange(&layout, &frame()).dividers[0].first_px, px);
        }
        // Past MIN_RATIO/MAX_RATIO it stops there.
        assert_eq!(d.ratio_for(-40), MIN_RATIO);
        assert_eq!(d.ratio_for(5000), MAX_RATIO);
    }

    #[test]
    fn neighbours_need_an_adjacent_edge_and_an_overlapping_span() {
        // [a | b] over [c | d] with the two rows' dividers at different places.
        let root = Node::split(
            Axis::Column,
            0.5,
            Node::split(
                Axis::Row,
                0.3,
                Node::Leaf(ModuleId::lua("a")),
                Node::Leaf(ModuleId::lua("b")),
            ),
            Node::split(
                Axis::Row,
                0.7,
                Node::Leaf(ModuleId::lua("c")),
                Node::Leaf(ModuleId::lua("d")),
            ),
        );
        let layout = Layout::new(root, ModuleId::lua("a")).unwrap();
        let a = arrange(&layout, &Frame::new(Size { w: 1001, h: 601 }, HANDLE));
        let n = |from: &str, dir| neighbor(&a.modules, &ModuleId::lua(from), dir, layout.mru(), HANDLE);
        assert_eq!(n("a", Direction::Right), Some(ModuleId::lua("b")));
        assert_eq!(n("a", Direction::Down), Some(ModuleId::lua("c")), "c spans under a");
        assert_eq!(
            n("b", Direction::Down),
            Some(ModuleId::lua("c")),
            "c's span reaches under b too"
        );
        assert_eq!(
            n("d", Direction::Up),
            Some(ModuleId::lua("b")),
            "a ends before d begins"
        );
        assert_eq!(n("a", Direction::Left), None);
        assert_eq!(n("a", Direction::Up), None);
        assert_eq!(n("ghost", Direction::Up), None);
    }

    #[test]
    fn a_tie_goes_to_the_most_recently_used() {
        let mut layout = bottom_tree();
        let a = arrange(&layout, &frame());
        assert_eq!(
            neighbor(&a.modules, &bottom(), Direction::Up, layout.mru(), HANDLE),
            Some(editor()),
            "MRU starts editor-first"
        );
        layout.set_focus(&agent()).unwrap();
        layout.set_focus(&bottom()).unwrap();
        assert_eq!(
            neighbor(&a.modules, &bottom(), Direction::Up, layout.mru(), HANDLE),
            Some(agent())
        );
        // A module missing from the MRU list loses the tie to one on it, then top-left wins.
        assert_eq!(
            neighbor(&a.modules, &bottom(), Direction::Up, &[], HANDLE),
            Some(editor())
        );
    }

    /// The default layout preserves directional navigation. The editor side uses the tmux shim's
    /// letters (`pane_switch.rs`, `main.rs`'s `'R'`/`'U'`/catch-all arms). The panel capture controller
    /// sends `Ctrl+h` to the editor and `Ctrl+k` to the top bar, while `Ctrl+l` and `Ctrl+j` pass to
    /// the WebView. Pass-through is `Nothing` here so the shell lets the key continue.
    #[test]
    fn navigation_reproduces_todays_dispatch_on_the_default_tree() {
        use Direction::*;
        let layout = default_tree();
        let rows = [
            (editor(), Right, Nav::Module(agent())),
            (editor(), Left, Nav::Nothing),
            (editor(), Up, Nav::TopBar),
            (editor(), Down, Nav::Nothing),
            (agent(), Left, Nav::Module(editor())),
            (agent(), Up, Nav::TopBar),
            (agent(), Right, Nav::Nothing),
            (agent(), Down, Nav::Nothing),
        ];
        for (from, dir, today) in rows {
            assert_eq!(navigate(&layout, &from, dir, &frame()), today, "{from} {dir:?}");
        }
    }

    /// The same on the bottom-panel tree. Every row `main` defined is unchanged. Two rows change,
    /// deliberately, and are pinned here so the change is a decision rather than an accident:
    /// on `main` the bottom slot is mouse-only ("the `'D'` arm ... was never wired for plugin
    /// panels", `pane_switch.rs:53-57`), so `Ctrl+j` from above did nothing and nothing left the
    /// bottom by key. Geometry reaches it both ways now.
    #[test]
    fn navigation_reproduces_todays_dispatch_on_the_bottom_tree_and_reaches_the_bottom() {
        use Direction::*;
        let layout = bottom_tree();
        let unchanged = [
            (editor(), Right, Nav::Module(agent())),
            (editor(), Left, Nav::Nothing),
            (editor(), Up, Nav::TopBar),
            (agent(), Left, Nav::Module(editor())),
            (agent(), Up, Nav::TopBar),
            (agent(), Right, Nav::Nothing),
        ];
        for (from, dir, today) in unchanged {
            assert_eq!(navigate(&layout, &from, dir, &frame()), today, "{from} {dir:?}");
        }
        let changed = [
            (editor(), Down, Nav::Module(bottom())),
            (agent(), Down, Nav::Module(bottom())),
            (bottom(), Up, Nav::Module(editor())),
            (bottom(), Left, Nav::Nothing),
            (bottom(), Right, Nav::Nothing),
            (bottom(), Down, Nav::Nothing),
        ];
        for (from, dir, now) in changed {
            assert_eq!(navigate(&layout, &from, dir, &frame()), now, "{from} {dir:?}");
        }
    }

    /// A module with a divider on both sides: the trailing one moves, as tmux's `resize-pane`
    /// moves a cell's own right/bottom border and only falls back to the previous cell's for the
    /// last one (`layout_resize_pane`). Neither of the two trees above can tell the two apart.
    #[test]
    fn resize_prefers_the_trailing_divider_when_both_edges_have_one() {
        let (a, b, c) = (ModuleId::lua("a"), ModuleId::lua("b"), ModuleId::lua("c"));
        let root = Node::split(
            Axis::Row,
            0.3,
            Node::Leaf(a.clone()),
            Node::split(Axis::Row, 0.5, Node::Leaf(b.clone()), Node::Leaf(c.clone())),
        );
        let first_px = |layout: &Layout, con: ConId| {
            arrange(layout, &frame())
                .dividers
                .iter()
                .find(|d| d.con == con)
                .unwrap()
                .first_px
        };
        let mut layout = Layout::new(root, b.clone()).unwrap();
        let (outer_con, inner_con) = (layout.root().id, layout.root().children[1].node.as_con().unwrap().id);
        let (outer, inner) = (first_px(&layout, outer_con), first_px(&layout, inner_con));
        assert!(resize(&mut layout, &b, Direction::Right, 40, &frame()));
        assert_eq!(first_px(&layout, outer_con), outer, "b's left divider stays");
        assert_eq!(first_px(&layout, inner_con), inner + 40, "b's right divider moves");
        // c has no right divider, so its left one (the inner) moves.
        assert!(resize(&mut layout, &c, Direction::Left, 40, &frame()));
        assert_eq!(first_px(&layout, inner_con), inner);
        assert_eq!(first_px(&layout, outer_con), outer);
    }

    /// A zoom does not change where a move goes (the unzoomed geometry decides), and a hidden
    /// module is never a destination.
    #[test]
    fn navigation_ignores_zoom_and_skips_hidden_modules() {
        let mut layout = bottom_tree();
        layout.toggle_zoom(&editor());
        assert_eq!(
            navigate(&layout, &editor(), Direction::Right, &frame()),
            Nav::Module(agent())
        );
        layout.unzoom();
        hide(&mut layout, &bottom(), &frame()).unwrap();
        assert_eq!(navigate(&layout, &editor(), Direction::Down, &frame()), Nav::Nothing);
        // Nor does anything move from one: not even `Up` to the top bar.
        assert_eq!(navigate(&layout, &bottom(), Direction::Up, &frame()), Nav::Nothing);
        assert_eq!(
            navigate(&layout, &ModuleId::lua("ghost"), Direction::Up, &frame()),
            Nav::Nothing
        );
    }

    /// Resize targets for both default trees, checked for every focus and direction,
    /// including every `None`. Each tree has at most one divider per axis: `Row` separates
    /// the editor and agent, and `Column` sits above the bottom module.
    /// Each target includes the divider's axis and movement sign.
    #[test]
    fn resize_moves_the_divider_todays_resize_target_moved() {
        use Direction::*;
        // (tree, focused, dir, Some((the divider's axis, sign)))
        let across_default = Axis::Row;
        let across_bottom = Axis::Row;
        let down = Axis::Column;
        let cases: Vec<(Layout, ModuleId, Direction, Option<(Axis, i32)>)> = vec![
            (default_tree(), editor(), Left, Some((across_default, -1))),
            (default_tree(), editor(), Right, Some((across_default, 1))),
            (default_tree(), agent(), Left, Some((across_default, -1))),
            (default_tree(), agent(), Right, Some((across_default, 1))),
            (default_tree(), editor(), Up, None),
            (default_tree(), agent(), Down, None),
            (bottom_tree(), editor(), Left, Some((across_bottom, -1))),
            (bottom_tree(), agent(), Right, Some((across_bottom, 1))),
            (bottom_tree(), editor(), Up, Some((down, -1))),
            (bottom_tree(), agent(), Down, Some((down, 1))),
            (bottom_tree(), bottom(), Up, Some((down, -1))),
            (bottom_tree(), bottom(), Down, Some((down, 1))),
            (bottom_tree(), bottom(), Left, None),
            (bottom_tree(), bottom(), Right, None),
        ];
        for (mut layout, focused, dir, expected) in cases {
            let before = arrange(&layout, &frame());
            let moved = resize(&mut layout, &focused, dir, 40, &frame());
            let after = arrange(&layout, &frame());
            let label = format!("{focused} {dir:?}");
            match expected {
                None => {
                    assert!(!moved, "{label}");
                    assert_eq!(before, after, "{label}");
                }
                Some((axis, sign)) => {
                    assert!(moved, "{label}");
                    let px = |a: &Arrangement| a.dividers.iter().find(|d| d.axis == axis).unwrap().first_px;
                    assert_eq!(px(&after) - px(&before), sign * 40, "{label}");
                }
            }
        }
    }

    #[test]
    fn a_resize_with_nothing_to_move_does_not_end_a_zoom_and_one_that_moves_does() {
        let mut layout = default_tree();
        layout.toggle_zoom(&editor());
        assert!(!resize(&mut layout, &editor(), Direction::Up, 40, &frame()));
        assert_eq!(
            layout.zoomed(),
            Some(&editor()),
            "a no-op must not silently undo a zoom"
        );
        assert!(resize(&mut layout, &editor(), Direction::Right, 40, &frame()));
        assert_eq!(layout.zoomed(), None);
    }

    /// Hiding the module holding the keys chooses their destination first, so a hidden module
    /// never retains focus.
    #[test]
    fn hiding_the_focused_module_hands_the_keys_to_its_most_recent_neighbour_first() {
        let mut layout = bottom_tree();
        layout.set_focus(&agent()).unwrap();
        layout.set_focus(&bottom()).unwrap();
        let next = hide(&mut layout, &bottom(), &frame()).unwrap();
        assert_eq!(
            next,
            Some(agent()),
            "above the full-width bottom: the most recent of the two"
        );
        assert_eq!(layout.focus(), &agent());
        assert!(layout.is_visible(layout.focus()));

        let mut layout = default_tree();
        assert_eq!(hide(&mut layout, &editor(), &frame()).unwrap(), Some(agent()));
        assert!(
            layout.is_visible(layout.focus()),
            "the keys are never left on a hidden module"
        );
    }

    #[test]
    fn hiding_a_module_without_the_keys_moves_nothing_and_the_last_one_is_refused() {
        let mut layout = bottom_tree();
        assert_eq!(hide(&mut layout, &bottom(), &frame()), Ok(None));
        assert_eq!(layout.focus(), &editor());

        let mut layout = default_tree();
        hide(&mut layout, &agent(), &frame()).unwrap();
        assert_eq!(
            hide(&mut layout, &editor(), &frame()),
            Err(LayoutError::LastVisible(editor()))
        );
        assert_eq!(layout.focus(), &editor(), "a refused hide leaves the keys alone");
    }

    /// Hiding a focused module chooses a neighbor before the most recent module. The preceding
    /// trees cannot distinguish those choices because the most recent module is also a neighbor.
    /// A Lua `side` panel right of `[editor | agent]` separates them: only the agent is adjacent,
    /// even when the editor was used more recently.
    #[test]
    fn hiding_the_focused_module_prefers_a_neighbour_over_a_more_recent_non_neighbour() {
        let side = ModuleId::lua("side");
        let mut layout = Layout::initial(&[ModuleDecl {
            id: side.clone(),
            placement: Placement::RightOfRoot,
        }])
        .unwrap();
        layout.set_focus(&agent()).unwrap();
        layout.set_focus(&editor()).unwrap();
        layout.set_focus(&side).unwrap();
        assert_eq!(layout.mru(), &[side.clone(), editor(), agent()]);
        assert_eq!(
            hide(&mut layout, &side, &frame()).unwrap(),
            Some(agent()),
            "the agent borders the side panel; the editor, more recent, does not"
        );
        assert_eq!(layout.focus(), &agent());
    }

    /// When the module with the keys has no neighbour on screen at all, they go to the most recent
    /// module still shown -- never nowhere, which would leave them on the module being hidden. A
    /// frame with no area is one way to get there: every rectangle is empty, so no two spans overlap
    /// and nothing is anyone's neighbour.
    #[test]
    fn hiding_the_focused_module_with_no_neighbour_falls_back_to_the_most_recent_shown() {
        let mut layout = bottom_tree();
        layout.set_focus(&agent()).unwrap();
        layout.set_focus(&editor()).unwrap();
        let nothing = Frame::new(Size { w: 0, h: 0 }, HANDLE);
        assert_eq!(
            neighbor(
                &arrange(&layout, &nothing).modules,
                &editor(),
                Direction::Right,
                layout.mru(),
                HANDLE
            ),
            None,
            "the premise: no neighbour anywhere"
        );
        assert_eq!(hide(&mut layout, &editor(), &nothing).unwrap(), Some(agent()));
        assert_eq!(layout.focus(), &agent());
        assert!(layout.is_visible(layout.focus()));
    }

    /// Two dividers on one line: `[a | b]` over `[c | d]` at equal ratios, what `ops::even` builds
    /// routinely. `Ctrl+a l` from `c` moves the LOWER row's divider -- the one on `c`'s edge -- and
    /// not the upper row's, which sits at the same x but borders `a` and `b`. The same across the
    /// other axis: `Ctrl+a j` from `b` in `[a / c] | [b / d]` moves the right column's divider.
    #[test]
    fn resize_takes_the_divider_that_borders_the_leaf_not_one_on_the_same_line() {
        let l = ModuleId::lua;
        let f = Frame::new(Size { w: 1001, h: 601 }, HANDLE);
        let px = |layout: &Layout, con: ConId| {
            arrange(layout, &f)
                .dividers
                .iter()
                .find(|d| d.con == con)
                .unwrap()
                .first_px
        };
        let child_con = |layout: &Layout, i: usize| layout.root().children[i].node.as_con().unwrap().id;

        let rows = Node::split(
            Axis::Column,
            0.5,
            Node::split(Axis::Row, 0.5, Node::Leaf(l("a")), Node::Leaf(l("b"))),
            Node::split(Axis::Row, 0.5, Node::Leaf(l("c")), Node::Leaf(l("d"))),
        );
        let mut layout = Layout::new(rows, l("c")).unwrap();
        let (upper, lower) = (px(&layout, child_con(&layout, 0)), px(&layout, child_con(&layout, 1)));
        assert_eq!(upper, lower, "the premise: both rows' dividers are on one line");
        assert!(resize(&mut layout, &l("c"), Direction::Right, 40, &f));
        assert_eq!(
            px(&layout, child_con(&layout, 0)),
            upper,
            "the upper row's divider only shares c's x"
        );
        assert_eq!(px(&layout, child_con(&layout, 1)), lower + 40);

        let columns = Node::split(
            Axis::Row,
            0.5,
            Node::split(Axis::Column, 0.5, Node::Leaf(l("a")), Node::Leaf(l("c"))),
            Node::split(Axis::Column, 0.5, Node::Leaf(l("b")), Node::Leaf(l("d"))),
        );
        let mut layout = Layout::new(columns, l("b")).unwrap();
        let (left, right) = (px(&layout, child_con(&layout, 0)), px(&layout, child_con(&layout, 1)));
        assert_eq!(left, right, "the premise: both columns' dividers are on one line");
        assert!(resize(&mut layout, &l("b"), Direction::Down, 40, &f));
        assert_eq!(
            px(&layout, child_con(&layout, 0)),
            left,
            "the left column's divider only shares b's y"
        );
        assert_eq!(px(&layout, child_con(&layout, 1)), right + 40);
    }

    /// `toggle_zoom`'s "not a hidden module" guard, on its own. `tree.rs`'s test of it hides one of
    /// two modules, so "fewer than two shown" refuses first and the guard is never reached. With two
    /// others shown it is the only thing between a hidden module and a zoom that `arrange` would
    /// give the whole grid while `ModuleGrid::apply` keeps it child-invisible: a blank window.
    #[test]
    fn a_hidden_module_is_never_zoomed_even_with_two_others_shown() {
        let mut layout = bottom_tree();
        hide(&mut layout, &bottom(), &frame()).unwrap();
        assert_eq!(layout.toggle_zoom(&bottom()), ZoomChange::Nothing);
        assert_eq!(layout.zoomed(), None);
        assert_eq!(arrange(&layout, &frame()).modules.len(), 2);
    }

    /// A bottom row keeps its height when the window grows, matching `GtkPaned`'s
    /// `resize_end_child(false)`: a 240px terminal stays 240px through `F11`, rather than growing
    /// with a ratio. Once the bottom module's pin has a length, every window height preserves it.
    #[test]
    fn a_pinned_bottom_row_keeps_its_height_when_the_window_grows() {
        let height_at = |layout: &Layout, h: i32| {
            arrange(layout, &Frame::new(Size { w: 1280, h }, HANDLE))
                .rect_of(&bottom())
                .unwrap()
                .h
        };
        let unsettled = bottom_tree();
        assert_eq!(height_at(&unsettled, 721), 240, "a third of the default window");
        assert_eq!(height_at(&unsettled, 1041), 347, "F11 on 1080p, as P1 measured");
        assert_eq!(height_at(&unsettled, 1080), 360, "immersive, as P1 measured");

        let mut layout = bottom_tree();
        assert!(settle_pins(&mut layout, &frame()));
        assert_eq!(height_at(&layout, 721), 240);
        assert_eq!(height_at(&layout, 1041), 240, "F11 on 1080p");
        assert_eq!(height_at(&layout, 1080), 240, "immersive");
        assert!(!settle_pins(&mut layout, &frame()), "settled once, then kept");
    }

    /// A pin is given its length only once both sides are on screen: a hidden bottom module (the
    /// terminal until `Ctrl+a t`) gets its first show at a third of whatever height the window has
    /// then -- `main`'s `shown_position(None, h)`.
    #[test]
    fn a_pin_is_settled_the_first_time_both_sides_are_on_screen() {
        let mut layout = bottom_tree();
        hide(&mut layout, &bottom(), &frame()).unwrap();
        assert!(!layout.has_pin_to_settle());
        assert!(!settle_pins(&mut layout, &frame()), "hidden: nothing to settle");
        let tall = Frame::new(Size { w: 1920, h: 1041 }, HANDLE);
        layout.show(&bottom()).unwrap();
        assert!(layout.has_pin_to_settle(), "what the grid asks before it settles");
        assert!(settle_pins(&mut layout, &tall));
        assert!(!layout.has_pin_to_settle());
        assert_eq!(arrange(&layout, &tall).rect_of(&bottom()).unwrap().h, 347);
        assert_eq!(
            arrange(&layout, &frame()).rect_of(&bottom()).unwrap().h,
            347,
            "the window shrinks back and the row keeps what it was given"
        );
        // A zoom takes the pinned split off screen; nothing settles behind it.
        let mut zoomed = bottom_tree();
        zoomed.toggle_zoom(&editor());
        assert!(!zoomed.has_pin_to_settle());
        assert!(!settle_pins(&mut zoomed, &frame()));
    }

    /// Dragging a pinned divider, or `Ctrl+a j/k` on it, moves its PIXELS: the new height is what
    /// is kept when the window grows, as a dragged `GtkPaned` end child kept its new height.
    #[test]
    fn moving_a_pinned_divider_stores_pixels_and_the_row_keeps_them() {
        let mut layout = bottom_tree();
        settle_pins(&mut layout, &frame());
        let divider = arrange(&layout, &frame()).dividers[0].clone();
        assert_eq!((divider.con, divider.index), (layout.root().id, 0));
        move_divider(&mut layout, &divider, 400).unwrap();
        assert_eq!(arrange(&layout, &frame()).rect_of(&bottom()).unwrap().h, 320);
        let big = Frame::new(Size { w: 1280, h: 1041 }, HANDLE);
        assert_eq!(arrange(&layout, &big).rect_of(&bottom()).unwrap().h, 320);
        // `Ctrl+a k` from the bottom: 40px up is 40px taller, and stays 360 at any height.
        assert!(resize(&mut layout, &bottom(), Direction::Up, 40, &frame()));
        assert_eq!(arrange(&layout, &big).rect_of(&bottom()).unwrap().h, 360);
        // Dragged past the band a ratio has, it stops where a ratio would: 5% of 720 is 36.
        let divider = arrange(&layout, &frame()).dividers[0].clone();
        move_divider(&mut layout, &divider, 719).unwrap();
        assert_eq!(arrange(&layout, &frame()).rect_of(&bottom()).unwrap().h, 36);
    }

    /// A window squeezed below what the pin asks for gives the upper row its minimum and the
    /// bottom module the rest -- and the stored length is not what the squeeze made it: grown back,
    /// the row returns to its height.
    #[test]
    fn a_squeeze_does_not_change_what_a_pin_keeps() {
        let min = |id: &ModuleId| {
            if *id == bottom() {
                Size { w: 0, h: 80 }
            } else {
                Size { w: 0, h: 100 }
            }
        };
        let at = |h: i32| Frame {
            size: Size { w: 1280, h },
            handle_px: HANDLE,
            strip_px: DEFAULT_STRIP_PX,
            min: &min,
        };
        let mut layout = bottom_tree();
        assert!(settle_pins(&mut layout, &at(721)));
        assert_eq!(arrange(&layout, &at(721)).rect_of(&bottom()).unwrap().h, 240);
        assert_eq!(
            arrange(&layout, &at(301)).rect_of(&bottom()).unwrap().h,
            200,
            "the editor keeps its 100"
        );
        assert_eq!(arrange(&layout, &at(721)).rect_of(&bottom()).unwrap().h, 240);
    }

    /// Pinned lengths remain within ratio bounds at every window size. A bottom row dragged to
    /// 95% of fullscreen must leave 5% for the modules above when shrunk: 36px of a 720px window,
    /// rather than zero if its stored 988px were used directly. Growing again restores its dragged
    /// height; focusable modules must not become invisible.
    #[test]
    fn a_pinned_row_never_takes_more_than_a_ratio_could_when_the_window_shrinks() {
        let tall = Frame::new(Size { w: 1920, h: 1041 }, HANDLE);
        let mut layout = bottom_tree();
        assert!(settle_pins(&mut layout, &tall));
        let divider = arrange(&layout, &tall).dividers[0].clone();
        move_divider(&mut layout, &divider, 0).unwrap();
        assert_eq!(
            arrange(&layout, &tall).rect_of(&bottom()).unwrap().h,
            988,
            "95% of 1040"
        );
        let small = arrange(&layout, &frame());
        assert_eq!(small.rect_of(&editor()).unwrap().h, 36, "5% of 720, not 0");
        assert_eq!(small.rect_of(&bottom()).unwrap().h, 684);
        assert_eq!(arrange(&layout, &tall).rect_of(&bottom()).unwrap().h, 988);
    }
}
