//! Frozen binary geometry: n-ary arrangement must reproduce it pixel for pixel.
//! Keeping this copy independent preserves the oracle when the live binary code is deleted.

use super::geometry::{Frame, Rect, Size};
use super::module::ModuleId;
use super::tree::{Axis, Branch, MAX_RATIO, MIN_RATIO};

type SplitPath = Vec<Branch>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V1Pin {
    pub side: Branch,
    pub px: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum V1Node {
    Leaf(ModuleId),
    Split {
        axis: Axis,
        ratio: f32,
        pin: Option<V1Pin>,
        first: Box<V1Node>,
        second: Box<V1Node>,
    },
}

impl V1Node {
    pub fn split(axis: Axis, ratio: f32, first: V1Node, second: V1Node) -> V1Node {
        V1Node::Split {
            axis,
            ratio,
            pin: None,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    pub fn pinned(axis: Axis, ratio: f32, side: Branch, first: V1Node, second: V1Node) -> V1Node {
        V1Node::Split {
            axis,
            ratio,
            pin: Some(V1Pin { side, px: None }),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    pub fn leaves(&self) -> Vec<ModuleId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<ModuleId>) {
        match self {
            V1Node::Leaf(id) => out.push(id.clone()),
            V1Node::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    fn ratios(&self, out: &mut Vec<f32>) {
        if let V1Node::Split {
            ratio, first, second, ..
        } = self
        {
            out.push(*ratio);
            first.ratios(out);
            second.ratios(out);
        }
    }

    fn pins(&self, out: &mut Vec<V1Pin>) {
        if let V1Node::Split { pin, first, second, .. } = self {
            out.extend(*pin);
            first.pins(out);
            second.pins(out);
        }
    }

    fn any_leaf(&self, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
        match self {
            V1Node::Leaf(id) => shown(id),
            V1Node::Split { first, second, .. } => first.any_leaf(shown) || second.any_leaf(shown),
        }
    }

    fn has_pin_to_settle(&self, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
        match self {
            V1Node::Leaf(_) => false,
            V1Node::Split { pin, first, second, .. } => {
                (matches!(pin, Some(V1Pin { px: None, .. })) && first.any_leaf(shown) && second.any_leaf(shown))
                    || first.has_pin_to_settle(shown)
                    || second.has_pin_to_settle(shown)
            }
        }
    }

    fn at_path_mut(&mut self, path: &[Branch]) -> Option<&mut V1Node> {
        match (path.split_first(), self) {
            (None, node) => Some(node),
            (Some((branch, rest)), V1Node::Split { first, second, .. }) => match branch {
                Branch::First => first.at_path_mut(rest),
                Branch::Second => second.at_path_mut(rest),
            },
            (Some(_), V1Node::Leaf(_)) => None,
        }
    }

    pub(crate) fn without(self, id: &ModuleId) -> Option<V1Node> {
        match self {
            V1Node::Leaf(leaf) if leaf == *id => None,
            V1Node::Leaf(leaf) => Some(V1Node::Leaf(leaf)),
            V1Node::Split {
                axis,
                ratio,
                pin,
                first,
                second,
            } => match (first.without(id), second.without(id)) {
                (Some(first), Some(second)) => Some(V1Node::Split {
                    axis,
                    ratio,
                    pin,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    pub(crate) fn swap_leaves(&mut self, a: &ModuleId, b: &ModuleId) {
        match self {
            V1Node::Leaf(id) if id == a => *id = b.clone(),
            V1Node::Leaf(id) if id == b => *id = a.clone(),
            V1Node::Leaf(_) => {}
            V1Node::Split { first, second, .. } => {
                first.swap_leaves(a, b);
                second.swap_leaves(a, b);
            }
        }
    }

    pub(crate) fn map_leaves(&mut self, f: &dyn Fn(&ModuleId) -> ModuleId) {
        match self {
            V1Node::Leaf(id) => *id = f(id),
            V1Node::Split { first, second, .. } => {
                first.map_leaves(f);
                second.map_leaves(f);
            }
        }
    }

    pub(crate) fn replace_leaf(&mut self, id: &ModuleId, with: impl FnOnce(V1Node) -> V1Node) -> bool {
        match self {
            V1Node::Leaf(leaf) if leaf == id => {
                let old = std::mem::replace(self, V1Node::Leaf(id.clone()));
                *self = with(old);
                true
            }
            V1Node::Leaf(_) => false,
            V1Node::Split { first, second, .. } => {
                if first.leaves().contains(id) {
                    first.replace_leaf(id, with)
                } else {
                    second.replace_leaf(id, with)
                }
            }
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct V1Divider {
    pub path: SplitPath,
    pub axis: Axis,
    pub rect: Rect,
    pub avail: i32,
    pub first_px: i32,
    pub min_first: i32,
    pub min_second: i32,
}

impl V1Divider {
    pub fn ratio_for(&self, first_px: i32) -> f32 {
        if self.avail <= 0 {
            return 0.5;
        }
        let hi = (self.avail - self.min_second).max(0);
        let lo = self.min_first.min(hi);
        let px = first_px.clamp(lo, hi);
        (px as f32 / self.avail as f32).clamp(MIN_RATIO, MAX_RATIO)
    }

    fn clamp_first(&self, first_px: i32) -> i32 {
        let (band_lo, band_hi) = band(self.avail);
        let hi = (self.avail - self.min_second).max(0).min(band_hi);
        let lo = self.min_first.max(band_lo).min(hi);
        first_px.clamp(lo, hi)
    }
}

fn band(avail: i32) -> (i32, i32) {
    let hi = (avail as f32 * MAX_RATIO).floor() as i32;
    (((avail as f32 * MIN_RATIO).ceil() as i32).min(hi), hi)
}

fn subtree_min(node: &V1Node, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame) -> Option<Size> {
    match node {
        V1Node::Leaf(id) => shown(id).then(|| (frame.min)(id)),
        V1Node::Split {
            axis, first, second, ..
        } => match (subtree_min(first, shown, frame), subtree_min(second, shown, frame)) {
            (None, None) => None,
            (Some(only), None) | (None, Some(only)) => Some(only),
            (Some(a), Some(b)) => Some(match axis {
                Axis::Row => Size {
                    w: a.w + frame.handle_px + b.w,
                    h: a.h.max(b.h),
                },
                Axis::Column => Size {
                    w: a.w.max(b.w),
                    h: a.h + frame.handle_px + b.h,
                },
            }),
        },
    }
}

const MIN_VISIBLE_LEAF_PX: i32 = 32;

fn subtree_min_for_place(node: &V1Node, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame) -> Option<Size> {
    match node {
        V1Node::Leaf(id) => shown(id).then(|| {
            let m = (frame.min)(id);
            Size {
                w: m.w.max(MIN_VISIBLE_LEAF_PX),
                h: m.h.max(MIN_VISIBLE_LEAF_PX),
            }
        }),
        V1Node::Split {
            axis, first, second, ..
        } => match (
            subtree_min_for_place(first, shown, frame),
            subtree_min_for_place(second, shown, frame),
        ) {
            (None, None) => None,
            (Some(only), None) | (None, Some(only)) => Some(only),
            (Some(a), Some(b)) => Some(match axis {
                Axis::Row => Size {
                    w: a.w + frame.handle_px + b.w,
                    h: a.h.max(b.h),
                },
                Axis::Column => Size {
                    w: a.w.max(b.w),
                    h: a.h + frame.handle_px + b.h,
                },
            }),
        },
    }
}

fn along(axis: Axis, size: Size) -> i32 {
    match axis {
        Axis::Row => size.w,
        Axis::Column => size.h,
    }
}

fn place(
    node: &V1Node,
    rect: Rect,
    path: &mut SplitPath,
    shown: &dyn Fn(&ModuleId) -> bool,
    frame: &Frame,
    out: &mut Arrangement,
) {
    match node {
        V1Node::Leaf(id) => {
            if shown(id) {
                out.modules.push((id.clone(), rect));
            }
        }
        V1Node::Split {
            axis,
            ratio,
            pin,
            first,
            second,
        } => {
            let (min_a, min_b) = (
                subtree_min_for_place(first, shown, frame),
                subtree_min_for_place(second, shown, frame),
            );
            let (min_a, min_b) = match (min_a, min_b) {
                (None, None) => return,
                (Some(_), None) => {
                    path.push(Branch::First);
                    place(first, rect, path, shown, frame, out);
                    path.pop();
                    return;
                }
                (None, Some(_)) => {
                    path.push(Branch::Second);
                    place(second, rect, path, shown, frame, out);
                    path.pop();
                    return;
                }
                (Some(a), Some(b)) => (along(*axis, a), along(*axis, b)),
            };
            let length = match axis {
                Axis::Row => rect.w,
                Axis::Column => rect.h,
            };
            let handle = frame.handle_px.min(length).max(0);
            let avail = length - handle;
            let in_band = |px: i32| {
                let (lo, hi) = band(avail);
                px.clamp(lo, hi)
            };
            let wanted = match pin {
                Some(V1Pin {
                    side: Branch::First,
                    px: Some(px),
                }) => in_band(*px),
                Some(V1Pin {
                    side: Branch::Second,
                    px: Some(px),
                }) => in_band(avail - px),
                _ => (avail as f32 * ratio).round() as i32,
            };
            let hi = (avail - min_b).max(0);
            let first_px = wanted.clamp(0, avail).min(hi).max(min_a.min(avail));
            let second_px = avail - first_px;
            let (first_rect, handle_rect, second_rect) = match axis {
                Axis::Row => (
                    Rect { w: first_px, ..rect },
                    Rect {
                        x: rect.x + first_px,
                        w: handle,
                        ..rect
                    },
                    Rect {
                        x: rect.x + first_px + handle,
                        w: second_px,
                        ..rect
                    },
                ),
                Axis::Column => (
                    Rect { h: first_px, ..rect },
                    Rect {
                        y: rect.y + first_px,
                        h: handle,
                        ..rect
                    },
                    Rect {
                        y: rect.y + first_px + handle,
                        h: second_px,
                        ..rect
                    },
                ),
            };
            out.dividers.push(V1Divider {
                path: path.clone(),
                axis: *axis,
                rect: handle_rect,
                avail,
                first_px,
                min_first: min_a,
                min_second: min_b,
            });
            path.push(Branch::First);
            place(first, first_rect, path, shown, frame, out);
            path.pop();
            path.push(Branch::Second);
            place(second, second_rect, path, shown, frame, out);
            path.pop();
        }
    }
}

#[derive(Default)]
struct Arrangement {
    modules: Vec<(ModuleId, Rect)>,
    dividers: Vec<V1Divider>,
}
pub(crate) fn v1_arrange(
    root: &V1Node,
    shown: &dyn Fn(&ModuleId) -> bool,
    frame: &Frame,
) -> (Vec<(ModuleId, Rect)>, Vec<V1Divider>) {
    let mut out = Arrangement::default();
    place(
        root,
        Rect {
            x: 0,
            y: 0,
            w: frame.size.w.max(0),
            h: frame.size.h.max(0),
        },
        &mut Vec::new(),
        shown,
        frame,
        &mut out,
    );
    (out.modules, out.dividers)
}
pub(crate) fn v1_min(root: &V1Node, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame) -> Option<Size> {
    subtree_min(root, shown, frame)
}
pub(crate) fn v1_move_divider(root: &mut V1Node, divider: &V1Divider, first_px: i32) {
    let ratio = divider.ratio_for(first_px);
    let Some(V1Node::Split { ratio: r, pin, .. }) = root.at_path_mut(&divider.path) else {
        return;
    };
    match pin {
        None => *r = ratio,
        Some(pin) => {
            let first = divider.clamp_first(first_px);
            pin.px = Some(match pin.side {
                Branch::First => first,
                Branch::Second => divider.avail - first,
            });
        }
    }
}
pub(crate) fn v1_settle(root: &mut V1Node, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame) -> bool {
    let arrangement = v1_arrange(root, shown, frame);
    let mut settled = false;
    for divider in &arrangement.1 {
        if divider.avail <= 0 {
            continue;
        }
        if let Some(V1Node::Split { pin: Some(pin), .. }) = root.at_path_mut(&divider.path) {
            if pin.px.is_none() {
                pin.px = Some(match pin.side {
                    Branch::First => divider.first_px,
                    Branch::Second => divider.avail - divider.first_px,
                });
                settled = true;
            }
        }
    }
    settled
}
pub(crate) fn v1_has_pin_to_settle(root: &V1Node, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
    root.has_pin_to_settle(shown)
}
pub(crate) fn to_con_node(node: &V1Node) -> super::con::Node {
    use super::con::{Node, Pin};
    match node {
        V1Node::Leaf(id) => Node::Leaf(id.clone()),
        V1Node::Split {
            axis,
            ratio,
            pin,
            first,
            second,
        } => {
            let Node::Con(mut c) = Node::split(*axis, *ratio, to_con_node(first), to_con_node(second)) else {
                unreachable!()
            };
            if let Some(pin) = pin {
                c.children[if pin.side == Branch::First { 0 } else { 1 }].pin = Some(Pin { px: pin.px });
            }
            Node::Con(c)
        }
    }
}
pub(crate) struct Rng(u64);
impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub(crate) fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as u32
    }
}
pub(crate) fn random_v1(rng: &mut Rng, leaves: usize) -> V1Node {
    fn build(rng: &mut Rng, leaves: usize, next: &mut usize) -> V1Node {
        if leaves == 1 {
            let id = ModuleId::lua(&format!("m{}", *next));
            *next += 1;
            return V1Node::Leaf(id);
        }
        let n = 1 + rng.next() as usize % (leaves - 1);
        let axis = if rng.next() % 2 == 0 { Axis::Row } else { Axis::Column };
        let ratio = 0.05 + (rng.next() % 9001) as f32 / 10000.0;
        let pin = if rng.next() % 3 == 0 {
            Some(V1Pin {
                side: if rng.next() % 2 == 0 {
                    Branch::First
                } else {
                    Branch::Second
                },
                px: if rng.next() % 2 == 0 {
                    None
                } else {
                    Some((rng.next() % 601) as i32)
                },
            })
        } else {
            None
        };
        V1Node::Split {
            axis,
            ratio,
            pin,
            first: Box::new(build(rng, n, next)),
            second: Box::new(build(rng, leaves - n, next)),
        }
    }
    build(rng, leaves, &mut 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::arrange::*;
    use crate::layout::con::Con;

    fn cases(mut f: impl FnMut(&V1Node, &Con, &dyn Fn(&ModuleId) -> bool, &Frame)) {
        let mut rng = Rng::new(0x67c9b31472);
        for case in 0..2000 {
            let count = 2 + rng.next() as usize % 7;
            let old = random_v1(&mut rng, count);
            let new = to_con_node(&old);
            let leaves = old.leaves();
            let mask = rng.next() | 1;
            let mins: Vec<_> = leaves
                .iter()
                .map(|_| Size {
                    w: (rng.next() % 201) as i32,
                    h: (rng.next() % 201) as i32,
                })
                .collect();
            let shown = |id: &ModuleId| mask & (1 << leaves.iter().position(|m| m == id).unwrap()) != 0;
            let min = |id: &ModuleId| mins[leaves.iter().position(|m| m == id).unwrap()];
            for size in [
                Size { w: 1280, h: 721 },
                Size { w: 1920, h: 1040 },
                Size { w: 800, h: 600 },
                Size { w: 300, h: 200 },
            ] {
                let frame = Frame {
                    size,
                    handle_px: 1,
                    strip_px: crate::layout::geometry::DEFAULT_STRIP_PX,
                    min: &min,
                };
                f(&old, new.as_con().unwrap(), &shown, &frame);
            }
            assert_eq!(old.leaves().len(), count, "case {case}");
        }
    }
    fn arranged(c: &Con, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame) -> (Vec<(ModuleId, Rect)>, Vec<Divider>) {
        arrange_con(
            c,
            Rect {
                x: 0,
                y: 0,
                w: frame.size.w,
                h: frame.size.h,
            },
            shown,
            frame,
        )
    }
    fn equal(old: &(Vec<(ModuleId, Rect)>, Vec<V1Divider>), new: &(Vec<(ModuleId, Rect)>, Vec<Divider>)) {
        assert_eq!(old.0, new.0);
        assert_eq!(old.1.len(), new.1.len());
        for (a, b) in old.1.iter().zip(&new.1) {
            assert_eq!(
                (a.rect, a.axis, a.avail, a.first_px, a.min_first, a.min_second),
                (b.rect, b.axis, b.avail, b.first_px, b.min_first, b.min_second)
            );
        }
    }
    #[test]
    fn every_binary_tree_arranges_like_its_two_child_containers() {
        cases(|old, new, shown, frame| equal(&v1_arrange(old, shown, frame), &arranged(new, shown, frame)));
    }
    #[test]
    fn the_minimum_size_agrees() {
        cases(|old, new, shown, frame| assert_eq!(v1_min(old, shown, frame), con_min(new, shown, frame)));
    }
    #[test]
    fn dragging_any_divider_lands_where_the_binary_split_lands() {
        cases(|old, new, shown, frame| {
            let a = v1_arrange(old, shown, frame);
            let b = arranged(new, shown, frame);
            for (k, d) in a.1.iter().enumerate() {
                for px in [0, d.first_px - 37, d.first_px + 41, d.avail] {
                    let mut old = old.clone();
                    let mut new = new.clone();
                    v1_move_divider(&mut old, d, px);
                    assert!(move_divider_in(&mut new, &b.1[k], px, shown));
                    equal(&v1_arrange(&old, shown, frame), &arranged(&new, shown, frame));
                }
            }
        });
    }
    #[test]
    fn settling_pins_agrees() {
        cases(|old, new, shown, frame| {
            let mut old = old.clone();
            let mut new = new.clone();
            v1_settle(&mut old, shown, frame);
            for d in arranged(&new, shown, frame).1 {
                settle_in(&mut new, &d, shown);
            }
            let second = Frame {
                size: Size { w: 1537, h: 913 },
                ..*frame
            };
            equal(&v1_arrange(&old, shown, &second), &arranged(&new, shown, &second));
        });
    }
    #[test]
    fn the_pin_to_settle_agrees() {
        cases(|old, new, shown, frame| {
            let mut old = old.clone();
            let mut new = new.clone();
            assert_eq!(v1_has_pin_to_settle(&old, shown), con_has_pin_to_settle(&new, shown));
            v1_settle(&mut old, shown, frame);
            for d in arranged(&new, shown, frame).1 {
                settle_in(&mut new, &d, shown);
            }
            assert_eq!(v1_has_pin_to_settle(&old, shown), con_has_pin_to_settle(&new, shown));
        });
    }
    #[test]
    fn a_two_child_containers_step_ratio_is_its_first_share_exactly() {
        for i in 500..=9500 {
            let r = i as f32 / 10000.0;
            assert_eq!(step_ratio(&[r, 1.0 - r]), r);
        }
    }
}
