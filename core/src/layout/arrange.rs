//! Integer geometry for ordered split containers. Visible children divide the remaining
//! rectangle as a chain, so hidden children surrender space in proportion to visible shares.

use super::con::{Child, Con, ConId, ConLayout, Node};
use super::geometry::{Arrangement, ConStripEntry, Frame, Rect, Size, Strip};
use super::module::ModuleId;
use super::tree::{Axis, MAX_RATIO, MIN_RATIO};

#[derive(Debug, Clone, PartialEq)]
pub struct Divider {
    pub con: ConId,
    /// Index in `con.children` of the child before the divider.
    pub index: usize,
    pub axis: Axis,
    pub rect: Rect,
    pub avail: i32,
    pub first_px: i32,
    pub min_first: i32,
    pub min_second: i32,
}
impl Divider {
    pub fn ratio_for(&self, first_px: i32) -> f32 {
        if self.avail <= 0 {
            return 0.5;
        }
        let hi = (self.avail - self.min_second).max(0);
        let lo = self.min_first.min(hi);
        let px = first_px.clamp(lo, hi);
        (px as f32 / self.avail as f32).clamp(MIN_RATIO, MAX_RATIO)
    }
    pub(crate) fn clamp_first(&self, first_px: i32) -> i32 {
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
pub(crate) fn step_ratio(rest: &[f32]) -> f32 {
    (rest[0] as f64 / rest.iter().map(|s| *s as f64).sum::<f64>()) as f32
}
/// A shown leaf's floor inside [`arrange_con`]'s own clamp accounting, never in [`con_min`]'s report
/// to the window manager -- that report must stay exact at 0 when a module truly registers none. A
/// `GtkGLArea` editor host normally registers no minimum at all, so without this floor a step whose
/// share-driven length is overridden by a *sibling's* real minimum could leave a still-shown,
/// still-focused leaf exactly 0 pixels wide or tall while giving every pixel to the sibling --
/// reachable through ordinary nested splits and resizes, not just a contrived minimum.
///
/// **32, not 1**: a 1px editor is as invisible and as impossible to click as a 0px one, and a shown
/// module has no tray chip to bring it back. 32 logical px holds a row of editor text at the default
/// font and is a target a pointer lands on; it is below the 5% the ratio band already guarantees a
/// directly placed leaf at any ordinary window size (36px of a 721px column), so it only bites where
/// a sibling's own minimum overrides the band.
pub(crate) const MIN_VISIBLE_LEAF_PX: i32 = 32;
fn along(axis: Axis, size: Size) -> i32 {
    match axis {
        Axis::Row => size.w,
        Axis::Column => size.h,
    }
}
fn combine(axis: Axis, a: Size, b: Size, handle: i32) -> Size {
    match axis {
        Axis::Row => Size {
            w: a.w + handle + b.w,
            h: a.h.max(b.h),
        },
        Axis::Column => Size {
            w: a.w.max(b.w),
            h: a.h + handle + b.h,
        },
    }
}
fn node_min(node: &Node, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame, floor: bool) -> Option<Size> {
    match node {
        Node::Leaf(id) => shown(id).then(|| {
            let m = (frame.min)(id);
            if floor {
                Size {
                    w: m.w.max(MIN_VISIBLE_LEAF_PX),
                    h: m.h.max(MIN_VISIBLE_LEAF_PX),
                }
            } else {
                m
            }
        }),
        Node::Con(c) => children_min(c, shown, frame, floor),
    }
}
fn fixed_height(child: &Child, axis: Axis) -> Option<i32> {
    (axis == Axis::Column).then_some(child.fixed_px).flatten()
}
fn child_min(child: &Child, axis: Axis, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame, floor: bool) -> Option<Size> {
    let mut min = node_min(&child.node, shown, frame, floor)?;
    if let Some(px) = fixed_height(child, axis) {
        // The explicit height is also the minimum: a one-line page can be shorter than a normal tile.
        min.h = px;
    }
    Some(min)
}
/// Explicit heights and already settled pins reserve space before fractional siblings divide it.
fn reserved(child: &Child, axis: Axis) -> Option<i32> {
    fixed_height(child, axis).or_else(|| child.pin.and_then(|pin| pin.px))
}
fn fixed_first(c: &Con, rest: &[usize], length: i32, handle: i32) -> Option<i32> {
    let axis = c.layout.axis()?;
    if !rest.iter().any(|&i| fixed_height(&c.children[i], axis).is_some()) {
        return None;
    }
    if let Some(px) = reserved(&c.children[rest[0]], axis) {
        return Some(px);
    }
    let pixels: i64 = rest
        .iter()
        .filter_map(|&i| reserved(&c.children[i], axis))
        .map(i64::from)
        .sum();
    let shares: f64 = rest
        .iter()
        .filter(|&&i| reserved(&c.children[i], axis).is_none())
        .map(|&i| f64::from(c.children[i].share))
        .sum();
    let available = (i64::from(length) - i64::from(handle) * (rest.len() - 1) as i64 - pixels).max(0);
    Some((available as f64 * f64::from(c.children[rest[0]].share) / shares).round() as i32)
}
fn children_min(c: &Con, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame, floor: bool) -> Option<Size> {
    if c.layout.axis().is_none() {
        let mut min = node_min(&c.children[c.drawn(shown)?].node, shown, frame, floor)?;
        let rows = if c.layout == ConLayout::Stacked {
            visible(c, shown).len() as i32
        } else {
            1
        };
        min.h += frame.strip_px.max(0).saturating_mul(rows);
        return Some(min);
    }

    c.children
        .iter()
        .filter_map(|ch| child_min(ch, c.layout.axis().unwrap(), shown, frame, floor))
        .reduce(|a, b| combine(c.layout.axis().unwrap(), a, b, frame.handle_px))
}
pub(crate) fn con_min(root: &Con, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame) -> Option<Size> {
    children_min(root, shown, frame, false)
}
fn visible(c: &Con, shown: &dyn Fn(&ModuleId) -> bool) -> Vec<usize> {
    c.children
        .iter()
        .enumerate()
        .filter(|(_, ch)| ch.node.leaves().iter().any(shown))
        .map(|(i, _)| i)
        .collect()
}
// The last child keeps a far-side length only when the preceding child's own pin does not govern.
fn governing(c: &Con, rest: &[usize]) -> Option<(usize, bool)> {
    if c.children[rest[0]].pin.is_some() {
        Some((rest[0], false))
    } else if rest.len() == 2 && c.children[rest[1]].pin.is_some() {
        Some((rest[1], true))
    } else {
        None
    }
}
#[cfg(test)]
pub(crate) fn arrange_con(
    root: &Con,
    rect: Rect,
    shown: &dyn Fn(&ModuleId) -> bool,
    frame: &Frame,
) -> (Vec<(ModuleId, Rect)>, Vec<Divider>) {
    let out = arrange_tree(root, rect, shown, frame);
    (out.modules, out.dividers)
}

pub(crate) fn arrange_tree(root: &Con, rect: Rect, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame) -> Arrangement {
    fn place(node: &Node, rect: Rect, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame, out: &mut Arrangement) {
        match node {
            Node::Leaf(id) => {
                if shown(id) {
                    out.modules.push((id.clone(), rect));
                    out.cells.push((id.clone(), rect));
                }
            }
            Node::Con(c) => place_con(c, rect, shown, frame, out),
        }
    }
    fn place_con(c: &Con, mut rect: Rect, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame, out: &mut Arrangement) {
        let v = visible(c, shown);
        if c.layout.axis().is_none() {
            let Some(drawn) = c.drawn(shown) else {
                return;
            };
            let entries: Vec<_> = v
                .iter()
                .filter_map(|&i| {
                    Some(ConStripEntry {
                        id: c.children[i].node.focused_leaf(shown)?,
                        active: i == drawn,
                    })
                })
                .collect();
            let rows = if c.layout == ConLayout::Stacked {
                entries.len() as i32
            } else {
                1
            };
            let height = frame.strip_px.max(0).saturating_mul(rows).min(rect.h);
            out.strips.push(Strip {
                con: c.id,
                layout: c.layout,
                rect: Rect { h: height, ..rect },
                entries,
            });
            let start = out.cells.len();
            place(
                &c.children[drawn].node,
                Rect {
                    y: rect.y + height,
                    h: rect.h - height,
                    ..rect
                },
                shown,
                frame,
                out,
            );
            for (_, cell) in &mut out.cells[start..] {
                if cell.y == rect.y + height {
                    cell.y = rect.y;
                    cell.h += height;
                }
            }
            return;
        }

        let axis = c.layout.axis().unwrap();
        for (j, &i) in v.iter().enumerate() {
            if j + 1 == v.len() {
                place(&c.children[i].node, rect, shown, frame, out);
                break;
            }
            let min_a = along(axis, child_min(&c.children[i], axis, shown, frame, true).unwrap());
            let min_b = along(
                axis,
                v[j + 1..]
                    .iter()
                    .map(|&i| child_min(&c.children[i], axis, shown, frame, true).unwrap())
                    .reduce(|a, b| combine(axis, a, b, frame.handle_px))
                    .unwrap(),
            );
            let length = along(axis, Size { w: rect.w, h: rect.h });
            let handle = frame.handle_px.min(length).max(0);
            let avail = length - handle;
            let pinned = governing(c, &v[j..]).and_then(|(i, far)| {
                c.children[i]
                    .pin
                    .unwrap()
                    .px
                    .map(|px| if far { avail - px } else { px })
            });
            let wanted = if let Some(px) = fixed_first(c, &v[j..], length, handle) {
                px
            } else if let Some(px) = pinned {
                let (lo, hi) = band(avail);
                px.clamp(lo, hi)
            } else {
                let shares: Vec<_> = v[j..].iter().map(|&i| c.children[i].share).collect();
                (avail as f32 * step_ratio(&shares)).round() as i32
            };
            let first_px = wanted.clamp(0, avail).min((avail - min_b).max(0)).max(min_a.min(avail));
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
            out.dividers.push(Divider {
                con: c.id,
                index: i,
                axis,
                rect: handle_rect,
                avail,
                first_px,
                min_first: min_a,
                min_second: min_b,
            });
            place(&c.children[i].node, first_rect, shown, frame, out);
            rect = second_rect;
        }
    }
    let mut out = Arrangement::default();
    place_con(root, rect, shown, frame, &mut out);
    out
}
/// Whether a divider that was arranged still has a pin with no length. Only the children that get
/// arranged count -- the drawn child of a tabbed or stacked container, the shown children of a split --
/// since `settle_in` can reach no other, and an unsettled pin in one would keep this true for good.
pub(crate) fn con_has_pin_to_settle(root: &Con, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
    let arranged: Vec<usize> = if root.layout.axis().is_some() {
        visible(root, shown)
    } else {
        root.drawn(shown).into_iter().collect()
    };
    (0..arranged.len().saturating_sub(1))
        .any(|j| governing(root, &arranged[j..]).is_some_and(|(i, _)| root.children[i].pin.unwrap().px.is_none()))
        || arranged.iter().any(|&i| match &root.children[i].node {
            Node::Con(c) => con_has_pin_to_settle(c, shown),
            _ => false,
        })
}
fn step<'a>(root: &'a mut Con, d: &Divider, shown: &dyn Fn(&ModuleId) -> bool) -> Option<(&'a mut Con, Vec<usize>)> {
    let c = root.find_mut(d.con)?;
    c.layout.axis()?;
    let v = visible(c, shown);
    let j = v.iter().position(|&i| i == d.index)?;
    if j + 1 == v.len() {
        return None;
    }
    Some((c, v[j..].to_vec()))
}
/// Whether `d` still names a step of `root`: its container is there, and the child before it and a
/// later one are both shown.
pub(crate) fn has_step(root: &Con, d: &Divider, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
    root.find(d.con).is_some_and(|c| {
        let v = if c.layout.axis().is_some() {
            visible(c, shown)
        } else {
            Vec::new()
        };
        v.iter().position(|&i| i == d.index).is_some_and(|j| j + 1 < v.len())
    })
}
fn trade(children: &mut [Child], rest: &[usize], ratio: f32) {
    let t: f32 = rest.iter().map(|&i| children[i].share).sum();
    let pair = children[rest[0]].share + children[rest[1]].share;
    if pair < 2.0 * MIN_RATIO {
        return;
    }
    let new = ((ratio as f64 * t as f64) as f32).clamp(MIN_RATIO, pair - MIN_RATIO);
    children[rest[0]].share = new;
    children[rest[1]].share = pair - new;
}
pub(crate) fn move_divider_in(root: &mut Con, d: &Divider, first_px: i32, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
    let Some((c, v)) = step(root, d, shown) else {
        return false;
    };
    if let Some((i, far)) = governing(c, &v) {
        let first = d.clamp_first(first_px);
        c.children[i].pin.as_mut().unwrap().px = Some(if far { d.avail - first } else { first });
    } else {
        trade(&mut c.children, &v, d.ratio_for(first_px));
    }
    true
}
pub(crate) fn set_step_ratio(root: &mut Con, d: &Divider, ratio: f32, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
    if ratio.is_nan() {
        return false;
    }
    let Some((c, v)) = step(root, d, shown) else {
        return false;
    };
    if let Some((i, _)) = governing(c, &v) {
        c.children[i].pin.as_mut().unwrap().px = None;
    }
    trade(&mut c.children, &v, ratio.clamp(MIN_RATIO, MAX_RATIO));
    true
}
pub(crate) fn settle_in(root: &mut Con, d: &Divider, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
    let Some((c, v)) = step(root, d, shown) else {
        return false;
    };
    if d.avail <= 0 {
        return false;
    }
    if let Some((i, far)) = governing(c, &v) {
        let pin = c.children[i].pin.as_mut().unwrap();
        if pin.px.is_none() {
            pin.px = Some(if far { d.avail - d.first_px } else { d.first_px });
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::con::{ConLayout, Pin};
    use crate::layout::v1_reference::{random_v1, to_con_node, Rng};

    fn leaf(name: &str) -> Node {
        Node::Leaf(ModuleId::lua(name))
    }
    fn flat(shares: &[f32]) -> Con {
        Con::new(
            ConLayout::SplitH,
            shares
                .iter()
                .enumerate()
                .map(|(i, &s)| Child::new(leaf(&format!("m{i}")), s))
                .collect(),
        )
    }
    fn at(c: &Con, shown: &dyn Fn(&ModuleId) -> bool, size: Size) -> (Vec<(ModuleId, Rect)>, Vec<Divider>) {
        arrange_con(
            c,
            Rect {
                x: 0,
                y: 0,
                w: size.w,
                h: size.h,
            },
            shown,
            &Frame::new(size, 1),
        )
    }
    const SIZE: Size = Size { w: 1280, h: 721 };
    #[test]
    fn fixed_heights_are_reserved_before_fractional_children_share_the_rest() {
        let mut c = flat(&[0.25; 4]);
        c.layout = ConLayout::SplitV;
        c.children[0].fixed_px = Some(24);
        c.children[2].fixed_px = Some(16);
        for h in [443, 843, 1643] {
            let (modules, _) = at(&c, &|_| true, Size { w: 1000, h });
            let heights: Vec<_> = modules.iter().map(|(_, rect)| rect.h).collect();
            assert_eq!(heights, [24, (h - 43) / 2, 16, (h - 43) / 2]);
        }
        let (modules, _) = at(&c, &|id| *id != ModuleId::lua("m0"), Size { w: 1000, h: 418 });
        assert_eq!(
            modules.iter().map(|(_, rect)| rect.h).collect::<Vec<_>>(),
            [200, 16, 200]
        );
    }

    #[test]
    fn a_fixed_height_overrides_the_pages_measured_minimum() {
        let mut c = flat(&[0.5, 0.5]);
        c.layout = ConLayout::SplitV;
        c.children[0].fixed_px = Some(16);
        let minimum = |_: &ModuleId| Size { w: 100, h: 300 };
        let frame = Frame {
            min: &minimum,
            ..Frame::new(Size { w: 1000, h: 800 }, 1)
        };
        let arranged = arrange_tree(
            &c,
            Rect {
                x: 0,
                y: 0,
                w: 1000,
                h: 800,
            },
            &|_| true,
            &frame,
        );
        assert_eq!(arranged.modules[0].1.h, 16);
        assert_eq!(arranged.modules[1].1.h, 783);
        assert_eq!(con_min(&c, &|_| true, &frame).unwrap().h, 317);
    }

    #[test]
    fn fixed_heights_preserve_settled_pins_and_fit_a_short_frame() {
        let mut c = flat(&[0.5, 0.25, 0.25]);
        c.layout = ConLayout::SplitV;
        c.children[1].fixed_px = Some(24);
        c.children[2].pin = Some(Pin { px: Some(200) });
        for h in [400, 800, 1600] {
            let (modules, _) = at(&c, &|_| true, Size { w: 1000, h });
            assert_eq!(
                modules.iter().map(|(_, rect)| rect.h).collect::<Vec<_>>(),
                [h - 226, 24, 200]
            );
        }
        for h in [0, 1, 10, 32] {
            let (modules, _) = at(&c, &|_| true, Size { w: 1000, h });
            assert_eq!(modules.last().map(|(_, rect)| rect.y + rect.h).unwrap(), h);
            assert!(modules.iter().all(|(_, rect)| rect.h >= 0));
        }
    }

    #[test]
    fn three_even_children_split_like_the_even_chain() {
        let c = flat(&[1.0 / 3.0; 3]);
        let chain = Node::split(
            Axis::Row,
            1.0 / 3.0,
            leaf("m0"),
            Node::split(Axis::Row, 0.5, leaf("m1"), leaf("m2")),
        );
        for size in [SIZE, Size { w: 1920, h: 1040 }, Size { w: 800, h: 600 }] {
            let a = at(&c, &|_| true, size);
            let b = at(chain.as_con().unwrap(), &|_| true, size);
            assert_eq!(a.0, b.0);
            for (a, b) in a.1.iter().zip(b.1) {
                assert_eq!(
                    (a.axis, a.rect, a.avail, a.first_px, a.min_first, a.min_second),
                    (b.axis, b.rect, b.avail, b.first_px, b.min_first, b.min_second)
                );
            }
        }
    }
    #[test]
    fn a_hidden_middle_child_gives_its_length_to_the_visible_ones_in_proportion() {
        let c = flat(&[0.5, 0.25, 0.25]);
        let (modules, dividers) = at(&c, &|id| *id != ModuleId::lua("m1"), SIZE);
        assert_eq!(
            modules,
            vec![
                (
                    ModuleId::lua("m0"),
                    Rect {
                        x: 0,
                        y: 0,
                        w: 853,
                        h: 721
                    }
                ),
                (
                    ModuleId::lua("m2"),
                    Rect {
                        x: 854,
                        y: 0,
                        w: 426,
                        h: 721
                    }
                )
            ]
        );
        assert_eq!(dividers.len(), 1);
    }
    #[test]
    fn dragging_beside_a_hidden_middle_child_trades_with_the_next_visible_one() {
        let mut c = flat(&[0.4, 0.2, 0.4]);
        let shown = |id: &ModuleId| *id != ModuleId::lua("m1");
        let d = at(&c, &shown, SIZE).1.remove(0);
        assert_eq!(d.first_px, 640);
        assert!(move_divider_in(&mut c, &d, 767, &shown));
        assert_eq!(at(&c, &shown, SIZE).0[0].1.w, 767);
        assert_eq!(c.children[1].share, 0.2);
        assert!((c.children.iter().map(|ch| ch.share).sum::<f32>() - 1.0).abs() < 1e-6);
    }
    #[test]
    fn a_pinned_child_keeps_its_length_and_the_last_one_takes_the_remainder_when_all_are_pinned() {
        let mut c = flat(&[0.5, 0.5]);
        c.children[0].pin = Some(Pin { px: Some(200) });
        let widths = |c: &Con| at(c, &|_| true, SIZE).0.iter().map(|(_, r)| r.w).collect::<Vec<_>>();
        assert_eq!(widths(&c), vec![200, 1079]);
        c.children[1].pin = Some(Pin { px: Some(300) });
        assert_eq!(widths(&c), vec![200, 1079]);
        c.children[0].pin = None;
        assert_eq!(widths(&c), vec![979, 300]);
    }
    #[test]
    fn a_pin_inside_an_undrawn_tab_is_never_waiting_to_settle() {
        let mut inner = flat(&[0.5, 0.5]);
        inner.children[1].pin = Some(Pin { px: None });
        let mut tabs = Con::new(
            ConLayout::Tabbed,
            vec![Child::new(leaf("a"), 0.5), Child::new(Node::Con(inner), 0.5)],
        );
        assert!(!con_has_pin_to_settle(&tabs, &|_| true));
        tabs.focus = 1;
        assert!(con_has_pin_to_settle(&tabs, &|_| true));
    }
    #[test]
    fn a_pin_that_governs_no_step_is_never_waiting_to_settle() {
        let mut c = flat(&[0.5, 0.5]);
        for ch in &mut c.children {
            ch.pin = Some(Pin { px: None });
        }
        assert!(con_has_pin_to_settle(&c, &|_| true));
        let d = at(&c, &|_| true, SIZE).1.remove(0);
        assert!(settle_in(&mut c, &d, &|_| true));
        assert!(!con_has_pin_to_settle(&c, &|_| true));
        assert_eq!(c.children[1].pin.unwrap().px, None);
    }
    #[test]
    fn a_tree_over_n_leaves_has_at_most_n_minus_1_dividers() {
        let mut rng = Rng::new(0x73428ab);
        for _ in 0..2000 {
            let n = 2 + rng.next() as usize % 7;
            let node = to_con_node(&random_v1(&mut rng, n));
            let mask = rng.next() | 1;
            let leaves = node.leaves();
            let shown = |id: &ModuleId| mask & (1 << leaves.iter().position(|m| m == id).unwrap()) != 0;
            assert!(at(node.as_con().unwrap(), &shown, SIZE).1.len() < n);
        }
        for n in 2..=20 {
            assert_eq!(at(&flat(&vec![1.0 / n as f32; n]), &|_| true, SIZE).1.len(), n - 1);
        }
    }
    #[test]
    fn setting_a_step_ratio_trades_shares_forgets_the_governing_length_and_refuses_stale_steps() {
        let mut c = flat(&[0.4, 0.2, 0.4]);
        c.children[2].pin = Some(Pin { px: Some(300) });
        let shown = |id: &ModuleId| *id != ModuleId::lua("m1");
        let d = at(&c, &shown, SIZE).1.remove(0);
        let before = c.clone();
        assert!(!set_step_ratio(&mut c, &d, f32::NAN, &shown));
        assert_eq!(c, before);
        assert!(set_step_ratio(&mut c, &d, 0.75, &shown));
        assert_eq!(c.children[1].share, 0.2);
        assert_eq!(c.children[2].pin.unwrap().px, None);
        assert!((step_ratio(&[c.children[0].share, c.children[2].share]) - 0.75).abs() < 1e-6);
        assert!(!move_divider_in(&mut c, &d, 100, &|_| false));
        assert!(!settle_in(&mut c, &d, &|_| false));
        assert!(!set_step_ratio(&mut c, &d, 0.5, &|_| false));
        let mut stale = d;
        stale.con = ConId::fresh();
        assert!(!move_divider_in(&mut c, &stale, 100, &shown));
    }
}
