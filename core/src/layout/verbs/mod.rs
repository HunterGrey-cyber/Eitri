//! Layout verbs from tmux, vim and i3 rearrange the tree without creating or removing modules.
//! Acting ends a zoom through `unzoom` and `replace_root`, which fills undo and clears the last
//! preset. Finding nothing to do leaves a zoom on. The keys stay on their module. Shown modules
//! include those zoomed away: decisions use the unzoomed window.

use super::geometry::{arrange_with, Direction, Frame};
use super::module::ModuleId;
use super::ops::{even, even_chain, hidden_bottom_rows};
use super::tree::{
    Axis, Branch, Layout, LayoutError, Node, BELOW_ROOT_SHARE, DEFAULT_EDITOR_SHARE, MAX_RATIO, MIN_RATIO,
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

pub fn move_far(layout: &mut Layout, target: &ModuleId, dir: Direction, frame: &Frame) -> Result<bool, LayoutError> {
    if !layout.contains(target) {
        return Err(LayoutError::NotInTree(target.clone()));
    }
    if !layout.is_shown(target) {
        return Err(LayoutError::Hidden(target.clone()));
    }
    if shown(layout).len() < 2 {
        return Err(LayoutError::LastVisible(target.clone()));
    }
    let r = arrange_with(layout, false, frame)
        .rect_of(target)
        .expect("a shown module has a rectangle");
    let (w, h) = (frame.size.w.max(0), frame.size.h.max(0));
    let edge = match dir {
        Direction::Left => r.x == 0 && r.y == 0 && r.h == h,
        Direction::Right => r.x + r.w == w && r.y == 0 && r.h == h,
        Direction::Up => r.y == 0 && r.x == 0 && r.w == w,
        Direction::Down => r.y + r.h == h && r.x == 0 && r.w == w,
    };
    if edge {
        return Ok(false);
    }
    fn detach(node: Node, target: &ModuleId, layout: &Layout) -> (Option<Node>, Option<Node>) {
        if node.leaves().contains(target) && node.leaves().iter().all(|id| id == target || !layout.is_shown(id)) {
            return (None, Some(node));
        }
        match node {
            Node::Leaf(_) => (Some(node), None),
            Node::Split {
                axis,
                ratio,
                pin,
                first,
                second,
            } => {
                let (first, a) = detach(*first, target, layout);
                let (second, b) = detach(*second, target, layout);
                let rest = match (first, second) {
                    (Some(first), Some(second)) => Some(Node::Split {
                        axis,
                        ratio,
                        pin,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (only, None) | (None, only) => only,
                };
                (rest, a.or(b))
            }
        }
    }
    let (rest, unit) = detach(layout.root().clone(), target, layout);
    let (rest, unit) = (rest.unwrap(), unit.unwrap());
    let root = match dir {
        Direction::Left => Node::split(Axis::Row, 1.0 - RIGHT_OF_ROOT_SHARE, unit, rest),
        Direction::Right => Node::split(Axis::Row, RIGHT_OF_ROOT_SHARE, rest, unit),
        Direction::Up => Node::split(Axis::Column, 1.0 - BELOW_ROOT_SHARE, unit, rest),
        Direction::Down => Node::pinned(Axis::Column, BELOW_ROOT_SHARE, Branch::Second, rest, unit),
    };
    apply(layout, root);
    Ok(true)
}

// A path lets callers edit a clone only after a usable ancestor has been found.
fn nearest(node: &Node, target: &ModuleId, layout: &Layout) -> Option<Vec<Branch>> {
    let Node::Split { first, second, .. } = node else {
        return None;
    };
    let (branch, child) = if first.leaves().contains(target) {
        (Branch::First, first)
    } else {
        (Branch::Second, second)
    };
    if let Some(mut path) = nearest(child, target, layout) {
        path.insert(0, branch);
        return Some(path);
    }
    (has_shown(first, layout) && has_shown(second, layout)).then(Vec::new)
}

fn at_mut<'a>(node: &'a mut Node, path: &[Branch]) -> &'a mut Node {
    match path.split_first() {
        None => node,
        Some((branch, rest)) => {
            let Node::Split { first, second, .. } = node else {
                unreachable!()
            };
            at_mut(if *branch == Branch::First { first } else { second }, rest)
        }
    }
}

pub fn toggle_split(layout: &mut Layout, target: &ModuleId) -> bool {
    if !layout.is_shown(target) {
        return false;
    }
    let Some(path) = nearest(layout.root(), target, layout) else {
        return false;
    };
    let mut root = layout.root().clone();
    let Node::Split { axis, pin, .. } = at_mut(&mut root, &path) else {
        unreachable!()
    };
    *axis = match axis {
        Axis::Row => Axis::Column,
        Axis::Column => Axis::Row,
    };
    *pin = None;
    apply(layout, root);
    true
}

pub fn rotate(layout: &mut Layout, down: bool) -> bool {
    let ids = shown(layout);
    let n = ids.len();
    if n < 2 {
        return false;
    }
    let mut root = layout.root().clone();
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
    fn walk(node: &mut Node, wanted: Axis) -> bool {
        let Node::Split {
            axis,
            ratio,
            pin,
            first,
            second,
        } = node
        else {
            return false;
        };
        let mut changed = walk(first, wanted) | walk(second, wanted);
        if *axis == wanted {
            std::mem::swap(first, second);
            *ratio = (1.0 - *ratio).clamp(MIN_RATIO, MAX_RATIO);
            if let Some(pin) = pin {
                pin.side = match pin.side {
                    Branch::First => Branch::Second,
                    Branch::Second => Branch::First,
                };
            }
            changed = true;
        }
        changed
    }
    let mut root = layout.root().clone();
    if !walk(&mut root, axis) {
        return false;
    }
    apply(layout, root);
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
    let Some(mut path) = nearest(layout.root(), target, layout) else {
        return false;
    };
    let mut root = layout.root().clone();
    let Node::Split { axis, .. } = at_mut(&mut root, &path) else {
        unreachable!()
    };
    let axis = *axis;
    while !path.is_empty() {
        let parent = &path[..path.len() - 1];
        if !matches!(at_mut(&mut root, parent), Node::Split { axis: a, .. } if *a == axis) {
            break;
        }
        path.pop();
    }
    fn walk(node: &mut Node, axis: Axis, layout: &Layout) -> usize {
        if let Node::Split {
            axis: a,
            ratio,
            pin,
            first,
            second,
        } = node
        {
            if *a == axis {
                let a = walk(first, axis, layout);
                let b = walk(second, axis, layout);
                if a > 0 && b > 0 {
                    *ratio = (a as f32 / (a + b) as f32).clamp(MIN_RATIO, MAX_RATIO);
                }
                *pin = None;
                return a + b;
            }
        }
        usize::from(has_shown(node, layout))
    }
    walk(at_mut(&mut root, &path), axis, layout);
    apply(layout, root);
    true
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
    let mut root = layout.root().clone();
    root.swap_leaves(target, &mark);
    apply(layout, root);
    Ok(mark)
}

#[cfg(test)]
mod invariant_run;
#[cfg(test)]
mod tests;
