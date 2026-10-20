//! Layout verbs from tmux, vim and i3 rearrange the tree without creating or removing modules.
//! Acting ends a zoom through `unzoom` and `replace_root`, which fills undo and clears the last
//! preset. Finding nothing to do leaves a zoom on. The keys stay on their module. Shown modules
//! include those zoomed away: decisions use the unzoomed window.

pub mod containers;
pub mod floating;
pub mod move_dir;
pub mod select;
pub use containers::{container_mode, switch_tab};
pub use floating::{
    float_at, float_toggle, mode_toggle_target, move_float, resize_float, scratchpad_move, scratchpad_show,
    set_float_rect, FloatChange, ScratchShow,
};
pub use move_dir::move_dir;
pub use select::{focus_child, focus_parent};

use super::con::{Con, ConId, ConLayout, Node};
use super::geometry::{arrange_with, Direction, Frame};
use super::module::ModuleId;
use super::ops::{even, even_chain, hidden_bottom_rows};
use super::tree::{
    Axis, Branch, Layout, LayoutError, Selection, BELOW_ROOT_SHARE, DEFAULT_EDITOR_SHARE, MAX_RATIO, MIN_RATIO,
    RIGHT_OF_ROOT_SHARE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    EvenHorizontal,
    EvenVertical,
    MainHorizontal,
    MainVertical,
    Tiled,
}

impl Preset {
    /// tmux's `next-layout` order.
    pub const CYCLE: [Preset; 5] = [
        Self::EvenHorizontal,
        Self::EvenVertical,
        Self::MainHorizontal,
        Self::MainVertical,
        Self::Tiled,
    ];

    pub fn next(self) -> Preset {
        Self::CYCLE[(Self::CYCLE.iter().position(|p| *p == self).unwrap() + 1) % 5]
    }

    pub fn previous(self) -> Preset {
        Self::CYCLE[(Self::CYCLE.iter().position(|p| *p == self).unwrap() + 4) % 5]
    }
}

pub const MAX_PRESET_MODULES: usize = 20;

fn shown(layout: &Layout) -> Vec<ModuleId> {
    layout.leaves().into_iter().filter(|id| layout.is_shown(id)).collect()
}

fn has_shown(node: &Node, layout: &Layout) -> bool {
    node.leaves().iter().any(|id| layout.is_shown(id))
}

fn apply(layout: &mut Layout, root: Node) {
    layout.unzoom();
    layout.replace_root(root);
}

fn wrap_far(unit: Node, rest: Node, dir: Direction) -> Node {
    match dir {
        Direction::Left => Node::split(Axis::Row, 1.0 - RIGHT_OF_ROOT_SHARE, unit, rest),
        Direction::Right => Node::split(Axis::Row, RIGHT_OF_ROOT_SHARE, rest, unit),
        Direction::Up => Node::split(Axis::Column, 1.0 - BELOW_ROOT_SHARE, unit, rest),
        Direction::Down => Node::pinned(Axis::Column, BELOW_ROOT_SHARE, Branch::Second, rest, unit),
    }
}

pub fn move_far(layout: &mut Layout, target: &ModuleId, dir: Direction, frame: &Frame) -> Result<bool, LayoutError> {
    if !layout.contains(target) {
        return Err(LayoutError::NotInTree(target.clone()));
    }
    if !layout.is_shown(target) {
        return Err(LayoutError::Hidden(target.clone()));
    }
    if layout.is_floating(target) {
        let unit = Node::Leaf(target.clone());
        let rest = layout.root_node();
        let root = wrap_far(unit, rest, dir);
        layout.take_float(target);
        apply(layout, root);
        return Ok(true);
    }
    if shown(layout).len() < 2 {
        return Err(LayoutError::LastVisible(target.clone()));
    }
    if layout.selection() == Selection::Con(layout.root().id) {
        return Ok(false);
    }
    let arrangement = arrange_with(layout, false, frame);
    let rects = match layout.selection() {
        Selection::Module => arrangement.rect_of(target).into_iter().collect::<Vec<_>>(),
        Selection::Con(id) => {
            let c = layout.root().find(id).unwrap();
            let leaves = c.leaves();
            let ids = c.ids();
            arrangement
                .modules
                .iter()
                .filter(|(m, _)| leaves.contains(m))
                .map(|(_, r)| *r)
                .chain(
                    arrangement
                        .strips
                        .iter()
                        .filter(|s| ids.contains(&s.con))
                        .map(|s| s.rect),
                )
                .collect()
        }
    };
    if rects.is_empty() {
        return Ok(false);
    }
    let x = rects.iter().map(|r| r.x).min().unwrap();
    let y = rects.iter().map(|r| r.y).min().unwrap();
    let right = rects.iter().map(|r| r.x + r.w).max().unwrap();
    let bottom = rects.iter().map(|r| r.y + r.h).max().unwrap();
    let (w, h) = (frame.size.w.max(0), frame.size.h.max(0));
    let edge = match dir {
        Direction::Left => x == 0 && y == 0 && bottom == h,
        Direction::Right => right == w && y == 0 && bottom == h,
        Direction::Up => y == 0 && x == 0 && right == w,
        Direction::Down => bottom == h && x == 0 && right == w,
    };
    if edge {
        return Ok(false);
    }
    // The unit is the highest subtree on the way to `target` that holds nothing else shown: the
    // target with the hidden modules that share its region. Taking its leaves out one by one leaves
    // what taking the subtree out leaves, and keeps every other container as it was.
    let tree = layout.root_node();
    let selected;
    let mut unit = match layout.selection() {
        Selection::Module => &tree,
        Selection::Con(id) => {
            selected = Node::Con(layout.root().find(id).unwrap().clone());
            &selected
        }
    };
    while layout.selection() == Selection::Module && unit.leaves().iter().any(|id| id != target && layout.is_shown(id))
    {
        let Node::Con(c) = unit else {
            unreachable!("a leaf holds only itself")
        };
        unit = &c
            .children
            .iter()
            .find(|ch| ch.node.leaves().contains(target))
            .expect("the target is under every container on its way")
            .node;
    }
    let unit = unit.clone();
    let rest = unit
        .leaves()
        .iter()
        .try_fold(tree.clone(), |rest, id| rest.without(id))
        .expect("another module is shown, so it is still in the tree");
    let root = wrap_far(unit, rest, dir);
    apply(layout, root);
    Ok(true)
}

/// The containers on the way from the root to `target`'s leaf, outermost first, each with the index
/// of its child that holds `target`.
fn path_to(root: &Con, target: &ModuleId) -> Vec<(ConId, usize)> {
    let mut path = Vec::new();
    let mut c = root;
    while let Some(i) = c.children.iter().position(|ch| ch.node.leaves().contains(target)) {
        path.push((c.id, i));
        match &c.children[i].node {
            Node::Con(inner) => c = inner,
            Node::Leaf(_) => break,
        }
    }
    path
}

pub fn toggle_split(layout: &mut Layout, target: &ModuleId) -> bool {
    if !layout.is_shown(target) {
        return false;
    }
    let Some(id) = layout.selected_container(target) else {
        return false;
    };
    let mut root = layout.root().clone();
    let c = root.find_mut(id).expect("found in this tree");
    let split = c.layout.axis().is_some();
    c.layout = match c.layout {
        ConLayout::SplitH => ConLayout::SplitV,
        ConLayout::SplitV => ConLayout::SplitH,
        ConLayout::Tabbed | ConLayout::Stacked => ConLayout::of_axis(c.last_split),
    };
    if split {
        for child in &mut c.children {
            child.pin = None;
        }
    }
    root.normalise();
    apply(layout, Node::Con(root));
    true
}

pub fn rotate(layout: &mut Layout, down: bool) -> bool {
    let ids: Vec<_> = shown(layout).into_iter().filter(|id| layout.is_in_view(id)).collect();
    let n = ids.len();
    if n < 2 {
        return false;
    }
    let mut root = layout.root_node();
    root.map_leaves(&|id| {
        ids.iter()
            .position(|m| m == id)
            .map(|i| ids[(i + if down { n - 1 } else { 1 }) % n].clone())
            .unwrap_or_else(|| id.clone())
    });
    apply(layout, root);
    true
}

pub fn flip(layout: &mut Layout, axis: Axis) -> bool {
    if shown(layout).len() < 2 {
        return false;
    }
    // Each child keeps its share and its pin, so a pinned row stays pinned on its module.
    fn walk(c: &mut Con, wanted: Axis) -> bool {
        let mut changed = false;
        for child in &mut c.children {
            if let Node::Con(inner) = &mut child.node {
                changed |= walk(inner, wanted);
            }
        }
        if c.layout.axis() == Some(wanted) {
            c.children.reverse();
            changed = true;
        }
        changed
    }
    let mut root = layout.root().clone();
    let selected = match layout.selection() {
        Selection::Module => &mut root,
        Selection::Con(id) => root.find_mut(id).expect("selected container exists"),
    };
    if !walk(selected, axis) {
        return false;
    }
    apply(layout, Node::Con(root));
    true
}

pub fn preset(layout: &mut Layout, preset: Preset) -> Result<(), LayoutError> {
    let kept = hidden_bottom_rows(layout);
    let shown = shown(layout);
    let n = shown.len();
    if n > MAX_PRESET_MODULES {
        return Err(LayoutError::TooMany {
            limit: MAX_PRESET_MODULES,
        });
    }
    if n < 2 {
        return Err(LayoutError::LastVisible(layout.focus().clone()));
    }
    let hidden: Vec<_> = layout
        .leaves()
        .into_iter()
        .filter(|id| !layout.is_shown(id) && kept.iter().all(|row| row.id != *id))
        .collect();
    let leaves = |ids: Vec<ModuleId>| ids.into_iter().map(Node::Leaf).collect::<Vec<_>>();
    match preset {
        Preset::EvenHorizontal => even(layout, Axis::Row),
        Preset::EvenVertical => even(layout, Axis::Column),
        _ => {
            let mut root = match preset {
                Preset::MainVertical | Preset::MainHorizontal => {
                    let (outer, inner, ratio) = if preset == Preset::MainVertical {
                        (Axis::Row, Axis::Column, DEFAULT_EDITOR_SHARE)
                    } else {
                        (Axis::Column, Axis::Row, BELOW_ROOT_SHARE)
                    };
                    let rest = shown[1..].iter().cloned().chain(hidden).collect();
                    Node::split(
                        outer,
                        ratio,
                        Node::Leaf(shown[0].clone()),
                        even_chain(inner, leaves(rest), n - 1),
                    )
                }
                Preset::Tiled => {
                    let (mut rows, mut cols) = (1, 1);
                    while rows * cols < n {
                        rows += 1;
                        if rows * cols < n {
                            cols += 1;
                        }
                    }
                    let chunks: Vec<_> = shown.chunks(cols).collect();
                    let mut nodes = Vec::new();
                    for (i, chunk) in chunks.iter().enumerate() {
                        let mut items = chunk.to_vec();
                        if i + 1 == chunks.len() {
                            items.extend(hidden.iter().cloned());
                        }
                        nodes.push(even_chain(Axis::Row, leaves(items), chunk.len()));
                    }
                    let count = nodes.len();
                    even_chain(Axis::Column, nodes, count)
                }
                Preset::EvenHorizontal | Preset::EvenVertical => unreachable!(),
            };
            for row in kept.into_iter().rev() {
                root = row.around(root);
            }
            apply(layout, root);
        }
    }
    layout.set_last_preset(preset);
    Ok(())
}

pub fn cycle_preset(layout: &mut Layout, forward: bool) -> Result<Preset, LayoutError> {
    let p = match (layout.last_preset(), forward) {
        (Some(p), true) => p.next(),
        (Some(p), false) => p.previous(),
        (None, true) => Preset::EvenHorizontal,
        (None, false) => Preset::Tiled,
    };
    preset(layout, p)?;
    Ok(p)
}

pub fn spread(layout: &mut Layout, target: &ModuleId) -> bool {
    if !layout.is_shown(target) {
        return false;
    }
    let Some(nearest) = layout.selected_container(target) else {
        return false;
    };
    let mut root = layout.root().clone();
    let kind = root.find(nearest).expect("found in this tree").layout;
    // Up through the parents of the same orientation: the whole row or column `target` is in.
    let path = select::path_to_con(&root, nearest).expect("the container belongs to this tree");
    let mut top = nearest;
    for (id, _) in path.iter().rev() {
        if root.find(*id).expect("found in this tree").layout != kind {
            break;
        }
        top = *id;
    }
    fn walk(c: &mut Con, kind: ConLayout, layout: &Layout) -> usize {
        let counts: Vec<usize> = c
            .children
            .iter_mut()
            .map(|child| match &mut child.node {
                Node::Con(inner) if inner.layout == kind => walk(inner, kind, layout),
                node => usize::from(has_shown(node, layout)),
            })
            .collect();
        let total: usize = counts.iter().sum();
        if let [first, second] = c.children.as_mut_slice() {
            if counts.iter().all(|n| *n > 0) {
                first.share = (counts[0] as f32 / total as f32).clamp(MIN_RATIO, MAX_RATIO);
                second.share = 1.0 - first.share;
            }
        } else if total > 0 {
            spread_shares(c, &counts);
        }
        for child in &mut c.children {
            child.pin = None;
        }
        total
    }
    walk(root.find_mut(top).expect("found in this tree"), kind, layout);
    apply(layout, Node::Con(root));
    true
}

/// The children of `c` with something shown divide the shares they hold now in proportion to
/// `counts`, the shown cells under each. A child with nothing shown keeps its share: a hidden child's
/// space goes to the shown ones in proportion to their shares, so their widths still follow `counts`,
/// and the hidden module takes the same part back when it is shown. No share drops below `MIN_RATIO`:
/// a child that would is held there and the others divide what is left, again by their counts.
fn spread_shares(c: &mut Con, counts: &[usize]) {
    let on: Vec<usize> = (0..counts.len()).filter(|&i| counts[i] > 0).collect();
    let budget: f32 = on.iter().map(|&i| c.children[i].share).sum();
    let mut floored = vec![false; counts.len()];
    loop {
        let held = on.iter().filter(|&&i| floored[i]).count() as f32 * MIN_RATIO;
        let free: Vec<usize> = on.iter().copied().filter(|&i| !floored[i]).collect();
        let weight = free.iter().map(|&i| counts[i]).sum::<usize>() as f32;
        let left = (budget - held).max(0.0);
        let low: Vec<usize> = free
            .iter()
            .copied()
            .filter(|&i| left * counts[i] as f32 / weight < MIN_RATIO)
            .collect();
        if low.is_empty() || low.len() == free.len() {
            for &i in &on {
                c.children[i].share = if floored[i] || low.contains(&i) {
                    MIN_RATIO
                } else {
                    left * counts[i] as f32 / weight
                };
            }
            return;
        }
        for i in low {
            floored[i] = true;
        }
    }
}

pub fn swap_marked(layout: &mut Layout, target: &ModuleId) -> Result<ModuleId, LayoutError> {
    let mark = layout.mark().ok_or(LayoutError::NoMark)?.clone();
    if !layout.contains(target) {
        return Err(LayoutError::NotInTree(target.clone()));
    }
    if !layout.is_shown(target) {
        return Err(LayoutError::Hidden(target.clone()));
    }
    if mark == *target {
        return Err(LayoutError::MarkedIsTarget(target.clone()));
    }
    if !layout.is_shown(&mark) {
        return Err(LayoutError::MarkHidden(mark));
    }
    match (layout.float_of(target).cloned(), layout.float_of(&mark).cloned()) {
        (Some(a), Some(b)) => {
            layout.store_float_rect(target, b.rect);
            layout.store_float_rect(&mark, a.rect);
            layout.unzoom();
            return Ok(mark);
        }
        (Some(float), None) | (None, Some(float)) => {
            let tiled = if float.id == *target { &mark } else { target };
            if *tiled == ModuleId::editor() {
                return Err(LayoutError::EditorNeverFloats);
            }
            let above: Vec<_> = layout
                .floats()
                .iter()
                .skip_while(|f| f.id != float.id)
                .skip(1)
                .map(|f| f.id.clone())
                .collect();
            let mut root = layout.root_node();
            root.map_leaves(&|id| if id == tiled { float.id.clone() } else { id.clone() });
            layout.push_float(super::float::Float {
                id: tiled.clone(),
                ..float.clone()
            });
            layout.take_float(&float.id);
            for id in above {
                layout.raise(&id);
            }
            apply(layout, root);
            return Ok(mark);
        }
        (None, None) => {}
    }
    let mut root = layout.root_node();
    root.swap_leaves(target, &mark);
    apply(layout, root);
    Ok(mark)
}

#[cfg(test)]
mod invariant_run;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod spread_wide {
    use std::collections::BTreeSet;

    use super::super::con::Child;
    use super::super::geometry::{arrange, Frame, Size};
    use super::*;

    fn leaf(id: &ModuleId) -> Node {
        Node::Leaf(id.clone())
    }
    fn flat(layout: ConLayout, children: Vec<(Node, f32)>) -> Node {
        Node::Con(Con::new(
            layout,
            children
                .into_iter()
                .map(|(node, share)| Child::new(node, share))
                .collect(),
        ))
    }
    fn widths(layout: &Layout) -> Vec<(ModuleId, i32)> {
        arrange(layout, &Frame::new(Size { w: 1280, h: 721 }, 1))
            .modules
            .into_iter()
            .map(|(id, r)| (id, r.w))
            .collect()
    }

    /// A hidden child in a row of three: the two shown modules are evened between themselves, and the
    /// hidden one keeps its share for when it is shown.
    #[test]
    fn a_hidden_child_does_not_stop_its_shown_siblings_being_evened() {
        let (editor, agent, term) = (ModuleId::editor(), ModuleId::agent(), ModuleId::terminal());
        let root = flat(
            ConLayout::SplitH,
            vec![(leaf(&editor), 0.6), (leaf(&agent), 0.2), (leaf(&term), 0.2)],
        );
        let mut layout = Layout::from_parts(root, BTreeSet::from([agent.clone()]), editor.clone()).unwrap();
        assert!(spread(&mut layout, &editor));
        layout.root().check(true).unwrap();
        let w = widths(&layout);
        let width = |id: &ModuleId| w.iter().find(|(m, _)| m == id).unwrap().1;
        assert!((width(&editor) - width(&term)).abs() <= 1, "evened, got {w:?}");
        assert!(
            (layout.root().children[1].share - 0.2).abs() < 1e-6,
            "the hidden child keeps its share"
        );
    }

    /// Nineteen cells beside two: the two would get 1/21 each, below the smallest share, so they are
    /// held at it and the tree stays one `Layout::new` accepts.
    #[test]
    fn a_wide_chain_never_pushes_a_sibling_below_the_smallest_share() {
        let many: Vec<ModuleId> = (0..19).map(|i| ModuleId::lua(&format!("m{i}"))).collect();
        let inner = flat(
            ConLayout::SplitH,
            many.iter().map(|id| (leaf(id), 1.0 / 19.0)).collect(),
        );
        let (a, b) = (ModuleId::lua("a"), ModuleId::lua("b"));
        let root = flat(
            ConLayout::SplitH,
            vec![(inner, 0.9), (leaf(&a), 0.05), (leaf(&b), 0.05)],
        );
        let mut layout = Layout::from_parts(root, BTreeSet::new(), a.clone()).unwrap();
        assert!(spread(&mut layout, &a));
        layout.root().check(true).unwrap();
        let shares: Vec<f32> = layout.root().children.iter().map(|ch| ch.share).collect();
        assert!(
            (shares[1] - MIN_RATIO).abs() < 1e-6 && (shares[2] - MIN_RATIO).abs() < 1e-6,
            "{shares:?}"
        );
        Layout::new(Node::Con(layout.root().clone()), a).unwrap();
    }
}

#[cfg(test)]
mod selection_tests {
    use crate::layout::test_support::same_rects;
    use crate::layout::*;

    fn sample() -> Layout {
        Layout::new(
            Node::split(
                Axis::Row,
                0.6,
                Node::split(
                    Axis::Column,
                    0.7,
                    Node::Leaf(ModuleId::editor()),
                    Node::Leaf(ModuleId::terminal()),
                ),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap()
    }
    #[test]
    fn the_far_move_of_a_selected_container_wraps_the_root_as_phase_one_does() {
        let mut l = sample();
        focus_parent(&mut l);
        let selected = l.selection();
        let unit = l.root().children[0].node.clone();
        let rest = l.root().children[1].node.clone();
        let expected = Layout::new(
            Node::split(Axis::Row, super::RIGHT_OF_ROOT_SHARE, rest, unit),
            ModuleId::editor(),
        )
        .unwrap();
        assert_eq!(
            move_far(
                &mut l,
                &ModuleId::editor(),
                Direction::Right,
                &Frame::new(Size { w: 1280, h: 721 }, 1)
            ),
            Ok(true)
        );
        same_rects(&l, &expected);
        assert_eq!(l.selection(), selected);
        assert_eq!(l.focus(), &ModuleId::editor());
    }
    #[test]
    fn verbs_that_take_a_selection_act_on_it() {
        let mut l = sample();
        focus_parent(&mut l);
        focus_parent(&mut l);
        assert_eq!(l.selection(), Selection::Con(l.root().id));
        assert!(toggle_split(&mut l, &ModuleId::editor()));
        assert_eq!(l.root().layout, ConLayout::SplitV);
        assert_eq!(l.root().children[0].node.as_con().unwrap().layout, ConLayout::SplitV);
        assert!(spread(&mut l, &ModuleId::editor()));
        assert!((l.root().children[0].share - 2.0 / 3.0).abs() < 1e-6);
        assert!(container_mode(&mut l, &ModuleId::editor(), ConLayout::Tabbed).unwrap());
        assert_eq!(l.root().layout, ConLayout::Tabbed);

        let mut l = sample();
        focus_parent(&mut l);
        assert!(!flip(&mut l, Axis::Row));
        assert_eq!(
            l.root().children[0].node.leaves(),
            vec![ModuleId::editor(), ModuleId::terminal()]
        );
        assert!(flip(&mut l, Axis::Column));
        assert_eq!(
            l.root().children[0].node.leaves(),
            vec![ModuleId::terminal(), ModuleId::editor()]
        );
        assert_eq!(l.focus(), &ModuleId::editor());
    }
}
