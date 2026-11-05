//! Integer geometry for ordered split containers. Visible children divide the remaining
//! rectangle as a chain, so hidden children surrender space in proportion to visible shares.

use super::con::{Child, Con, ConId, ConLayout, Node, SHARE_SLACK};
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
    /// The drawn length of the one child right after the divider (not the whole rest), and its minimum: what a fixed
    /// child's own edge may take from that neighbour.
    pub next_px: i32,
    pub next_min: i32,
    /// The drawn length of every child of the container on screen, by its index, as this arrangement drew them: what a
    /// drag that keeps the others' pixels solves the shares against.
    pub lengths: Vec<(usize, i32)>,
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
/// A child's explicit length along its container's axis: a height in a column, a width in a row. Only a leaf has one:
/// the length is one module's (a one-line strip, a rail), and a slot that has become a container (a tab added to it,
/// then split) holds modules with minimums of their own, which a length meant for one page would squeeze to nothing.
/// The stored length stays with the slot and counts again if the container dissolves back into one leaf.
fn fixed_length(child: &Child, axis: Axis) -> Option<i32> {
    if !matches!(child.node, Node::Leaf(_)) {
        return None;
    }
    match axis {
        Axis::Column => child.fixed_px,
        Axis::Row => child.fixed_w_px,
    }
}
/// The range a fixed length may take along `axis`.
fn fixed_range(axis: Axis) -> std::ops::RangeInclusive<i32> {
    match axis {
        Axis::Column => super::con::FIXED_HEIGHT_PX,
        Axis::Row => super::con::FIXED_WIDTH_PX,
    }
}
fn set_fixed_length(child: &mut Child, axis: Axis, px: i32) {
    let range = fixed_range(axis);
    let px = px.clamp(*range.start(), *range.end());
    match axis {
        Axis::Column => child.fixed_px = Some(px),
        Axis::Row => child.fixed_w_px = Some(px),
    }
}
fn child_min(child: &Child, axis: Axis, shown: &dyn Fn(&ModuleId) -> bool, frame: &Frame, floor: bool) -> Option<Size> {
    let mut min = node_min(&child.node, shown, frame, floor)?;
    if let Some(px) = fixed_length(child, axis) {
        // The explicit length is also the minimum: a one-line page or a rail can be smaller than a normal tile.
        match axis {
            Axis::Column => min.h = px,
            Axis::Row => min.w = px,
        }
    }
    Some(min)
}
/// Explicit lengths and already settled pins reserve space before fractional siblings divide it.
fn reserved(child: &Child, axis: Axis) -> Option<i32> {
    fixed_length(child, axis).or_else(|| child.pin.and_then(|pin| pin.px))
}
fn fixed_first(c: &Con, rest: &[usize], length: i32, handle: i32) -> Option<i32> {
    let axis = c.layout.axis()?;
    if !rest.iter().any(|&i| fixed_length(&c.children[i], axis).is_some()) {
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
        let mut drawn: Vec<(usize, i32)> = Vec::with_capacity(v.len());
        for (j, &i) in v.iter().enumerate() {
            if j + 1 == v.len() {
                drawn.push((i, along(axis, Size { w: rect.w, h: rect.h })));
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
            // The child after this divider is drawn by the next step (or is the last, taking the rest): its length is
            // filled in then.
            if j > 0 {
                if let Some(prev) = out
                    .dividers
                    .iter_mut()
                    .rev()
                    .find(|p| p.con == c.id && p.index == v[j - 1])
                {
                    prev.next_px = first_px;
                }
            }
            out.dividers.push(Divider {
                con: c.id,
                index: i,
                axis,
                rect: handle_rect,
                avail,
                first_px,
                min_first: min_a,
                min_second: min_b,
                next_px: second_px,
                next_min: along(
                    axis,
                    child_min(&c.children[v[j + 1]], axis, shown, frame, true).unwrap(),
                ),
                lengths: Vec::new(),
            });
            drawn.push((i, first_px));
            place(&c.children[i].node, first_rect, shown, frame, out);
            rect = second_rect;
        }
        for d in out.dividers.iter_mut().filter(|d| d.con == c.id) {
            d.lengths.clone_from(&drawn);
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
/// The pixels the children without a fixed length share in the step `v` of `d`: the step's length less its handles
/// and the reserved lengths in it, as [`fixed_first`] divides it.
fn fractional_pixels(c: &Con, v: &[usize], d: &Divider) -> i64 {
    let handle = along(
        d.axis,
        Size {
            w: d.rect.w,
            h: d.rect.h,
        },
    );
    let pixels: i64 = v
        .iter()
        .filter_map(|&i| reserved(&c.children[i], d.axis))
        .map(i64::from)
        .sum();
    i64::from(d.avail + handle) - i64::from(handle) * (v.len() - 1) as i64 - pixels
}
/// A child's pixel length that the arrangement takes before the shares: a leaf's fixed length, or a settled pin.
fn pixel_length(child: &Child, axis: Axis) -> Option<i32> {
    reserved(child, axis)
}
/// A fixed child's own edge moved by `delta` pixels (positive: it grows): the fixed child's length and the one
/// neighbour across the dragged edge change, in pixels, and every other child keeps its pixels.
///
/// - A neighbour with a pixel length of its own (a fixed length, a settled pin) trades length for length, each kept in
///   its range (a pin at its minimum).
/// - A neighbour drawn by its share gives or takes the pixels as share: its share moves by `delta` pixels' worth of the
///   container's fractional space, and every share drawn by a fraction is then scaled alike so the shares add up to
///   1 again, which moves no other child's pixels. Every share stays at least `MIN_RATIO`; a drag that would break that
///   is cut back to the largest that does not.
///
/// The neighbour never goes below its drawn minimum (`next_min` or `min_first`), nor the fixed child out of its range.
fn resize_fixed(
    c: &mut Con,
    v: &[usize],
    d: &Divider,
    shown: &dyn Fn(&ModuleId) -> bool,
    fixed: usize,
    neighbour: usize,
    delta: i32,
) {
    let axis = d.axis;
    let range = fixed_range(axis);
    let Some(length) = fixed_length(&c.children[fixed], axis) else {
        return;
    };
    let (neighbour_px, neighbour_min) = if neighbour == v[0] {
        (d.first_px, d.min_first)
    } else {
        (d.next_px, d.next_min)
    };
    // The fixed child's range, and the neighbour's minimum, bound the drag.
    let mut delta = delta.clamp(*range.start() - length, *range.end() - length);
    if fixed_length(&c.children[neighbour], axis).is_none() {
        // A neighbour without a length of its own stops the growth at its drawn minimum (its page's, or the floor).
        delta = delta.min((neighbour_px - neighbour_min).max(0));
    }
    if let Some(other) = pixel_length(&c.children[neighbour], axis) {
        let floor = match fixed_length(&c.children[neighbour], axis) {
            Some(_) => *range.start(),
            None => neighbour_min.max(0),
        };
        let ceiling = match fixed_length(&c.children[neighbour], axis) {
            Some(_) => *range.end(),
            None => i32::MAX,
        };
        delta = delta.min(other - floor).max(other - ceiling);
        set_fixed_length(&mut c.children[fixed], axis, length + delta);
        if fixed_length(&c.children[neighbour], axis).is_some() {
            set_fixed_length(&mut c.children[neighbour], axis, other - delta);
        } else if let Some(pin) = c.children[neighbour].pin.as_mut() {
            pin.px = Some(other - delta);
        }
        return;
    }
    // A neighbour drawn by its share: solve for the shares that draw the fixed child and its neighbour at their new
    // lengths and every other child at the length it has now, as the arrangement draws them (pins, clamps and all).
    let lengths: Vec<(usize, i32)> = d.lengths.clone();
    let shares_for = |delta: i32| -> Option<Vec<f32>> {
        let targets: Vec<(usize, i32)> = lengths
            .iter()
            .map(|&(i, px)| {
                if i == fixed {
                    (i, length + delta)
                } else if i == neighbour {
                    (i, px - delta)
                } else {
                    (i, px)
                }
            })
            .collect();
        let handle = along(
            axis,
            Size {
                w: d.rect.w,
                h: d.rect.h,
            },
        );
        shares_drawing(c, &targets, handle, axis)
    };
    let mut shares = shares_for(delta);
    if shares.is_none() {
        // Cut the drag back to the largest that keeps every share at least the smallest.
        let (mut ok, mut bad) = (0, delta);
        for _ in 0..24 {
            let mid = ok + (bad - ok) / 2;
            if mid == ok {
                break;
            }
            if shares_for(mid).is_some() {
                ok = mid;
            } else {
                bad = mid;
            }
        }
        delta = ok;
        shares = shares_for(delta);
    }
    set_fixed_length(&mut c.children[fixed], axis, length + delta);
    // With no shares that draw the others as they are (a window too small to draw them at the smallest share), the
    // length still changes and the children drawn by a fraction divide the difference by their shares.
    if let Some(shares) = shares {
        for (child, share) in c.children.iter_mut().zip(shares) {
            child.share = share;
        }
    }
    let _ = shown;
}
/// The shares, for every child of `c`, that make the arrangement draw each child on screen at its length in
/// `targets` (the container's visible children, in order, with the lengths wanted), or `None` when that needs a share
/// below the smallest. The arrangement draws the chain step by step, each step's first child from a pixel length
/// (a fixed length, a pin), or from its share against the shares after it: solved from the last step back, each such
/// share is the one that gives its child the wanted length against the shares already solved after it. Lengths that
/// meet every child's minimum are drawn as asked, clamps not binding. The shares of the children without a fixed length
/// are then scaled together so all add up to 1, which changes no ratio the arrangement reads.
fn shares_drawing(c: &Con, targets: &[(usize, i32)], handle: i32, axis: Axis) -> Option<Vec<f32>> {
    let mut shares: Vec<f64> = c.children.iter().map(|ch| f64::from(ch.share)).collect();
    let rest = |j: usize| -> Vec<usize> { targets[j..].iter().map(|&(i, _)| i).collect() };
    for j in (0..targets.len().saturating_sub(1)).rev() {
        let r = rest(j);
        let (first, want) = targets[j];
        let want = f64::from(want);
        let length: f64 =
            targets[j..].iter().map(|&(_, px)| f64::from(px)).sum::<f64>() + f64::from(handle) * (r.len() - 1) as f64;
        if r.iter().any(|&i| fixed_length(&c.children[i], axis).is_some()) {
            if reserved(&c.children[first], axis).is_some() {
                continue;
            }
            // The reserved children's lengths are the wanted ones (the fixed child's new length), not those stored.
            let pixels: f64 = targets[j..]
                .iter()
                .filter(|&&(i, _)| reserved(&c.children[i], axis).is_some())
                .map(|&(_, px)| f64::from(px))
                .sum();
            let available = length - f64::from(handle) * (r.len() - 1) as f64 - pixels;
            let after: f64 = r[1..]
                .iter()
                .filter(|&&i| reserved(&c.children[i], axis).is_none())
                .map(|&i| shares[i])
                .sum();
            if after <= 0.0 {
                continue;
            }
            if available - want <= 0.0 {
                return None;
            }
            shares[first] = want * after / (available - want);
        } else if governing(c, &r).is_some_and(|(i, _)| c.children[i].pin.is_some_and(|p| p.px.is_some())) {
            continue;
        } else {
            let after: f64 = r[1..].iter().map(|&i| shares[i]).sum();
            let avail = length - f64::from(handle);
            if avail - want <= 0.0 || after <= 0.0 {
                return None;
            }
            shares[first] = want * after / (avail - want);
        }
    }
    // A fixed child's share is never read while it has its length (a step holding it divides by the others'), so it
    // stays as it is; the others are scaled together to fill the rest.
    let fixed = |i: usize| fixed_length(&c.children[i], axis).is_some();
    let kept: f64 = (0..shares.len()).filter(|&i| fixed(i)).map(|i| shares[i]).sum();
    let total: f64 = (0..shares.len()).filter(|&i| !fixed(i)).map(|i| shares[i]).sum();
    if !total.is_finite() || total <= 0.0 || kept >= 1.0 {
        return None;
    }
    let scale = (1.0 - kept) / total;
    let shares: Vec<f64> = shares
        .into_iter()
        .enumerate()
        .map(|(i, s)| if fixed(i) { s } else { s * scale })
        .collect();
    shares
        .iter()
        .all(|&s| s >= f64::from(MIN_RATIO - SHARE_SLACK) && s <= 1.0)
        .then(|| shares.into_iter().map(|s| s as f32).collect())
}
pub(crate) fn move_divider_in(root: &mut Con, d: &Divider, first_px: i32, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
    let Some((c, v)) = step(root, d, shown) else {
        return false;
    };
    let axis = d.axis;
    // Where the divider may go: off both sides' minimums, and still inside a step too small for them both (a narrow
    // window), where the first side's minimum gives way.
    let hi = (d.avail - d.min_second).max(0);
    let lo = d.min_first.min(d.avail).min(hi);
    let first = first_px.clamp(lo, hi);
    if fixed_length(&c.children[v[0]], axis).is_some() {
        // The fixed child's own right (or lower) edge, before any pin: the drag (or `Ctrl+a l`) sets its length, and the
        // neighbour after it alone gives or takes (`resize_fixed`). Its own minimum is the length it has now, so the
        // lower bound is its range's, not `min_first`.
        let range = fixed_range(axis);
        // `min_second` counts the neighbour's minimum; a fixed neighbour's minimum is its present length, which a trade
        // of lengths may take down to its range's start.
        let min_second = match fixed_length(&c.children[v[1]], axis) {
            Some(_) => d.min_second - d.next_min + *range.start(),
            None => d.min_second,
        };
        let hi = (d.avail - min_second).max(0);
        let px = first_px.clamp((*range.start()).min(hi), hi);
        resize_fixed(c, &v, d, shown, v[0], v[1], px - d.first_px);
    } else if let Some(px) = fixed_length(&c.children[v[1]], axis) {
        // Its left (or upper) edge: the child before it alone gives or takes what the drag moves, down to that child's
        // minimum one way and to the fixed child's smallest length the other (`min_second` counts the fixed child's
        // present length, so it does not bound this edge).
        let range = fixed_range(axis);
        let hi = (d.first_px + px - *range.start()).max(0);
        let lo = d.min_first.min(d.avail).min(hi);
        let first = first_px.clamp(lo, hi);
        resize_fixed(c, &v, d, shown, v[1], v[0], d.first_px - first);
    } else if let Some((i, far)) = governing(c, &v) {
        let first = d.clamp_first(first_px);
        c.children[i].pin.as_mut().unwrap().px = Some(if far { d.avail - first } else { first });
    } else if v.iter().any(|&i| fixed_length(&c.children[i], axis).is_some()) {
        // A fixed child further along: the arrangement gives it its pixels first and divides the rest by the shares of
        // the others, so the drag is turned into a share over that rest, traded with the next child without a fixed
        // length (the fixed one between them just moves along).
        let available = fractional_pixels(c, &v, d);
        let Some(&partner) = v[1..].iter().find(|&&i| reserved(&c.children[i], axis).is_none()) else {
            return true;
        };
        if available <= 0 {
            return true;
        }
        let shares: f32 = v
            .iter()
            .filter(|&&i| reserved(&c.children[i], axis).is_none())
            .map(|&i| c.children[i].share)
            .sum();
        let pair = c.children[v[0]].share + c.children[partner].share;
        if pair < 2.0 * MIN_RATIO {
            return true;
        }
        let wanted = (f64::from(first) / available as f64 * f64::from(shares)) as f32;
        let new = wanted.clamp(MIN_RATIO, pair - MIN_RATIO);
        c.children[v[0]].share = new;
        c.children[partner].share = pair - new;
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

    fn lengths(c: &Con, size: Size) -> Vec<i32> {
        let axis = c.layout.axis().unwrap();
        at(c, &|_| true, size)
            .0
            .iter()
            .map(|(_, r)| along(axis, Size { w: r.w, h: r.h }))
            .collect()
    }
    fn divider(c: &Con, size: Size, k: usize) -> Divider {
        at(c, &|_| true, size).1.remove(k)
    }

    /// A drag beside a fixed width puts the divider where the pointer is: the arrangement gives the fixed child its
    /// pixels first, so the drag is turned into shares over what is left, not over the whole row.
    #[test]
    fn a_drag_or_a_key_resize_beside_a_fixed_width_lands_where_it_asked() {
        let size = Size { w: 1200, h: 700 };
        let mut c = flat(&[0.5, 0.25, 0.25]);
        c.children[2].fixed_w_px = Some(62);
        assert_eq!(lengths(&c, size), [757, 379, 62]);
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 500, &|_| true));
        assert_eq!(lengths(&c, size)[0], 500);
        // `Ctrl+a h` by ten pixels: ten pixels to the left, not a jump.
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, d.first_px - 10, &|_| true));
        assert_eq!(lengths(&c, size)[0], 490);
        assert_eq!(lengths(&c, size)[2], 62);
        // The fixed child's own divider sets its width, within 16..=400.
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, d.first_px - 100, &|_| true));
        assert_eq!(lengths(&c, size)[2], 162);
        assert_eq!(c.children[2].fixed_w_px, Some(162));
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 0, &|_| true));
        assert_eq!(c.children[2].fixed_w_px, Some(400));
        assert!((c.children.iter().map(|ch| ch.share).sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn dragging_a_fixed_childs_own_edge_sets_its_length_in_a_row_and_in_a_column() {
        // A rail first in a row.
        let size = Size { w: 1200, h: 700 };
        let mut c = flat(&[0.25, 0.5, 0.25]);
        c.children[0].fixed_w_px = Some(62);
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 100, &|_| true));
        assert_eq!(lengths(&c, size)[0], 100);
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 600, &|_| true));
        assert_eq!(lengths(&c, size)[0], 400, "a width stays within 16..=400");
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 3, &|_| true));
        assert_eq!(lengths(&c, size)[0], 16);
        // The editor beside it still lands where it is dragged.
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 700, &|_| true));
        assert_eq!(lengths(&c, size)[1], 700);
        // The same for a one-line strip on top of a column, and for one at its bottom.
        let size = Size { w: 1000, h: 900 };
        let mut c = flat(&[0.2, 0.5, 0.3]);
        c.layout = ConLayout::SplitV;
        c.children[0].fixed_px = Some(28);
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 40, &|_| true));
        assert_eq!(lengths(&c, size)[0], 40);
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 500, &|_| true));
        assert_eq!(lengths(&c, size)[0], 200, "a height stays within 16..=200");
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 300, &|_| true));
        assert_eq!(lengths(&c, size)[1], 300);
        let mut c = flat(&[0.5, 0.3, 0.2]);
        c.layout = ConLayout::SplitV;
        c.children[2].fixed_px = Some(28);
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 400, &|_| true));
        assert_eq!(lengths(&c, size), [400, 470, 28]);
    }

    /// A step too narrow for its minimums (a tiling compositor's narrow window): every drag and key resize near a
    /// fixed child is taken without a panic, and everything stays inside the row.
    #[test]
    fn a_drag_in_a_cramped_step_beside_a_fixed_width_does_not_panic() {
        for w in [60, 100, 110, 120] {
            for (k, shares, fixed) in [
                (2, [0.5, 0.25, 0.25], 2usize),
                (0, [0.25, 0.5, 0.25], 0),
                (1, [0.4, 0.2, 0.4], 1),
            ] {
                let _ = k;
                let size = Size { w, h: 80 };
                for d_index in 0..2 {
                    for px in [-10, 0, 14, 32, 50, 62, 109, 200, 5000] {
                        let mut c = flat(&shares);
                        c.children[fixed].fixed_w_px = Some(62);
                        let Some(d) = at(&c, &|_| true, size).1.get(d_index).cloned() else {
                            continue;
                        };
                        assert!(move_divider_in(&mut c, &d, px, &|_| true), "{w} {d_index} {px}");
                        assert_eq!(c.check(true), Ok(()), "{w} {d_index} {px}: {c:?}");
                        let (modules, _) = at(&c, &|_| true, size);
                        assert!(modules.iter().all(|(_, r)| r.x >= 0 && r.w >= 0 && r.x + r.w <= w));
                    }
                }
            }
        }
        // The same in two fractional leaves and no fixed one.
        let mut c = flat(&[0.5, 0.5]);
        let d = at(&c, &|_| true, Size { w: 50, h: 80 }).1.remove(0);
        assert!(move_divider_in(&mut c, &d, 30, &|_| true));
    }

    /// A fixed pane in the middle: dragging either of its edges changes its length and the neighbour across that edge
    /// only; the divider on its other side stays where it was. In a row and in a column.
    #[test]
    fn a_middle_fixed_panes_edges_move_only_their_own_neighbour() {
        let size = Size { w: 1200, h: 700 };
        let middle = || {
            let mut c = flat(&[0.475, 0.05, 0.475]);
            c.children[1].fixed_w_px = Some(62);
            c
        };
        let c0 = middle();
        assert_eq!(lengths(&c0, size), [568, 62, 568]);
        // Its right edge, 100 px to the right (a drag from x 631 to 731), and a key resize back by ten.
        let mut c = middle();
        let d = divider(&c, size, 1);
        assert_eq!(d.rect.x, 631);
        assert!(move_divider_in(&mut c, &d, 162, &|_| true));
        assert_eq!(lengths(&c, size), [568, 162, 468]);
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, d.first_px - 10, &|_| true));
        assert_eq!(lengths(&c, size), [568, 152, 478]);
        assert_eq!(c.check(true), Ok(()));
        // Its left edge, 100 px to the left.
        let mut c = middle();
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 468, &|_| true));
        assert_eq!(lengths(&c, size), [468, 162, 568]);
        assert_eq!(c.check(true), Ok(()));
        // A strip of 28 px in the middle of a column, both edges.
        let size = Size { w: 1000, h: 900 };
        let column = || {
            let mut c = flat(&[0.475, 0.05, 0.475]);
            c.layout = ConLayout::SplitV;
            c.children[1].fixed_px = Some(28);
            c
        };
        assert_eq!(lengths(&column(), size), [435, 28, 435]);
        let mut c = column();
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 128, &|_| true));
        assert_eq!(lengths(&c, size), [435, 128, 335]);
        let mut c = column();
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 385, &|_| true));
        assert_eq!(lengths(&c, size), [385, 78, 435]);
        // A rail at the end of a row: its edge moves the module beside it only, not the one before that.
        let mut c = flat(&[0.5, 0.25, 0.25]);
        c.children[2].fixed_w_px = Some(62);
        let before = lengths(&c, Size { w: 1200, h: 700 });
        let d = divider(&c, Size { w: 1200, h: 700 }, 1);
        assert!(move_divider_in(&mut c, &d, d.first_px - 100, &|_| true));
        let after = lengths(&c, Size { w: 1200, h: 700 });
        assert_eq!((after[0], after[1], after[2]), (before[0], before[1] - 100, 162));
    }

    /// The fixed-edge rule: a fixed child's own edge changes its length and the neighbour across that edge, in pixels;
    /// every other child keeps its pixels. Shrinking from the default share (the smallest), in a row and a column.
    #[test]
    fn shrinking_a_fixed_pane_from_its_default_share_moves_only_its_neighbour() {
        let size = Size { w: 1200, h: 700 };
        let middle = || {
            let mut c = flat(&[0.475, 0.05, 0.475]);
            c.children[1].fixed_w_px = Some(62);
            c
        };
        let mut c = middle();
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 16, &|_| true));
        assert_eq!(lengths(&c, size), [568, 16, 614]);
        assert_eq!(c.check(true), Ok(()));
        let mut c = middle();
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 614, &|_| true));
        assert_eq!(lengths(&c, size), [614, 16, 568]);
        // The rail at the left of a 1280 px row, narrowed to 40: the editor takes the 22 px, the chat keeps its width.
        let size = Size { w: 1280, h: 700 };
        let mut c = flat(&[0.05, 0.5, 0.45]);
        c.children[0].fixed_w_px = Some(62);
        let before = lengths(&c, size);
        let d = divider(&c, size, 0);
        assert!(move_divider_in(&mut c, &d, 40, &|_| true));
        let after = lengths(&c, size);
        assert_eq!(after, [40, before[1] + 22, before[2]]);
        // A strip in a column.
        let size = Size { w: 1000, h: 900 };
        let mut c = flat(&[0.475, 0.05, 0.475]);
        c.layout = ConLayout::SplitV;
        c.children[1].fixed_px = Some(28);
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 16, &|_| true));
        assert_eq!(lengths(&c, size), [435, 16, 447]);
    }

    /// Between two fixed panes the drag trades length for length, each within its range; the panes outside keep theirs.
    #[test]
    fn dragging_between_two_fixed_panes_trades_their_lengths() {
        let size = Size { w: 1201, h: 700 };
        let mut c = flat(&[0.45, 0.05, 0.05, 0.45]);
        c.children[1].fixed_w_px = Some(62);
        c.children[2].fixed_w_px = Some(62);
        let before = lengths(&c, size);
        assert_eq!(&before[1..3], [62, 62]);
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 82, &|_| true));
        assert_eq!(lengths(&c, size), [before[0], 82, 42, before[3]]);
        // As far as the other's range allows: it stops at 16.
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 300, &|_| true));
        assert_eq!(lengths(&c, size), [before[0], 108, 16, before[3]]);
    }

    /// Growth stops at the neighbour's drawn minimum (here the 32 px floor), not at its share: the pane past the
    /// neighbour never pays.
    #[test]
    fn a_fixed_pane_grows_only_as_far_as_its_neighbours_minimum() {
        let size = Size { w: 300, h: 700 };
        let mut c = flat(&[0.4, 0.1, 0.1, 0.4]);
        c.children[1].fixed_w_px = Some(62);
        let before = lengths(&c, size);
        assert_eq!(before[2], 32, "the neighbour is at its floor: {before:?}");
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, d.first_px + 10, &|_| true));
        assert_eq!(lengths(&c, size), before);
    }

    /// A fixed pane's own edge wins over a pin beside it: the pinned neighbour gives or takes, the rest stays.
    #[test]
    fn a_fixed_panes_edge_beside_a_pinned_row_moves_the_pin_not_the_rest() {
        let size = Size { w: 1000, h: 900 };
        let mut c = flat(&[0.5, 0.2, 0.3]);
        c.layout = ConLayout::SplitV;
        c.children[1].fixed_px = Some(28);
        c.children[2].pin = Some(Pin { px: Some(200) });
        let before = lengths(&c, size);
        assert_eq!(&before[1..], [28, 200]);
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 78, &|_| true));
        assert_eq!(lengths(&c, size), [before[0], 78, 150]);
    }

    /// Lengths and dividers of `c` in `size` with `minimum` as every module's measured minimum.
    fn measured(c: &Con, size: Size, minimum: &dyn Fn(&ModuleId) -> Size) -> (Vec<i32>, Vec<Divider>) {
        let frame = Frame {
            min: minimum,
            ..Frame::new(size, 1)
        };
        let axis = c.layout.axis().unwrap();
        let (modules, dividers) = arrange_con(
            c,
            Rect {
                x: 0,
                y: 0,
                w: size.w,
                h: size.h,
            },
            &|_| true,
            &frame,
        );
        (
            modules
                .iter()
                .map(|(_, r)| along(axis, Size { w: r.w, h: r.h }))
                .collect(),
            dividers,
        )
    }

    /// The share given or taken is solved against the lengths drawn, not the shares: a neighbour held at its measured
    /// minimum, or a pin further on, leaves every other child where it was. In a row and in a column.
    #[test]
    fn a_fixed_edge_drag_keeps_the_others_drawn_lengths_past_minimums_and_pins() {
        for vertical in [false, true] {
            let size = if vertical {
                Size { w: 700, h: 1200 }
            } else {
                Size { w: 1200, h: 700 }
            };
            let set = |c: &mut Con, i: usize, px: i32| {
                if vertical {
                    c.layout = ConLayout::SplitV;
                    c.children[i].fixed_px = Some(px.min(200));
                } else {
                    c.children[i].fixed_w_px = Some(px);
                }
            };
            // Case A: the neighbour measures at least 200 px and is drawn there, past its share.
            let at_least_200 = |id: &ModuleId| {
                if *id == ModuleId::lua("m1") {
                    Size { w: 200, h: 200 }
                } else {
                    Size::default()
                }
            };
            let mut c = flat(&[0.05, 0.1, 0.4, 0.45]);
            set(&mut c, 0, 62);
            let (before, dividers) = measured(&c, size, &at_least_200);
            assert_eq!(before, [62, 200, 440, 495], "vertical {vertical}");
            assert!(move_divider_in(&mut c, &dividers[0], 16, &|_| true));
            assert_eq!(c.check(true), Ok(()));
            assert_eq!(
                measured(&c, size, &at_least_200).0,
                [16, 246, 440, 495],
                "vertical {vertical}"
            );
            // Case B: a 200 px pin at the end.
            let none = |_: &ModuleId| Size::default();
            let mut c = flat(&[0.05, 0.2, 0.3, 0.45]);
            set(&mut c, 0, 62);
            c.children[3].pin = Some(Pin { px: Some(200) });
            let (before, dividers) = measured(&c, size, &none);
            assert_eq!(before, [62, 239, 696, 200], "vertical {vertical}");
            assert!(move_divider_in(&mut c, &dividers[0], 16, &|_| true));
            assert_eq!(c.check(true), Ok(()));
            assert_eq!(measured(&c, size, &none).0, [16, 285, 696, 200], "vertical {vertical}");
        }
    }

    /// Two fixed children side by side at the end: their shared edge trades length for length, both ways, down to 16,
    /// though each one's present length is its minimum.
    #[test]
    fn the_edge_between_two_fixed_children_moves_both_ways() {
        let size = Size { w: 1000, h: 700 };
        let mut c = flat(&[0.9, 0.05, 0.05]);
        c.children[1].fixed_w_px = Some(62);
        c.children[2].fixed_w_px = Some(62);
        assert_eq!(lengths(&c, size), [874, 62, 62]);
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 82, &|_| true));
        assert_eq!(lengths(&c, size), [874, 82, 42]);
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 62, &|_| true));
        assert_eq!(lengths(&c, size), [874, 62, 62]);
        let d = divider(&c, size, 1);
        assert!(move_divider_in(&mut c, &d, 300, &|_| true));
        assert_eq!(lengths(&c, size), [874, 108, 16]);
    }

    /// A slot that held the rail and now holds a container (a tab added, then split) does not squeeze the container's
    /// modules into the rail's width: only a leaf has a fixed length.
    #[test]
    fn a_fixed_width_holds_for_a_leaf_not_for_a_container_in_its_slot() {
        let inner = Con::new(
            ConLayout::SplitH,
            vec![
                Child::new(leaf("rail"), 0.34),
                Child::new(leaf("a"), 0.33),
                Child::new(leaf("b"), 0.33),
            ],
        );
        let mut c = Con::new(
            ConLayout::SplitH,
            vec![Child::new(Node::Con(inner), 0.25), Child::new(leaf("editor"), 0.75)],
        );
        c.children[0].fixed_w_px = Some(62);
        let (modules, _) = at(&c, &|_| true, SIZE);
        let w = |name: &str| modules.iter().find(|(m, _)| *m == ModuleId::lua(name)).unwrap().1.w;
        assert!(w("rail") >= 32 && w("a") >= 32 && w("b") >= 32, "{modules:?}");
        assert_eq!(
            w("rail") + w("a") + w("b") + 2,
            320,
            "the slot's share, a quarter of 1280"
        );
        // Back to one leaf, the slot's width counts again.
        let Node::Con(inner) = &c.children[0].node else {
            unreachable!()
        };
        let rail = inner.children[0].node.clone();
        c.children[0].node = rail;
        assert_eq!(lengths(&c, SIZE)[0], 62);
    }

    /// Fixed lengths that add up to more than the row: every module stays inside the row, none is negative.
    #[test]
    fn fixed_widths_wider_than_the_row_stay_inside_it() {
        let mut c = flat(&[0.3, 0.3, 0.4]);
        c.children[0].fixed_w_px = Some(300);
        c.children[1].fixed_w_px = Some(300);
        for w in [400, 610, 1200] {
            let (modules, _) = at(&c, &|_| true, Size { w, h: 700 });
            assert!(
                modules.iter().all(|(_, r)| r.w >= 0 && r.x >= 0 && r.x + r.w <= w),
                "{w}: {modules:?}"
            );
            assert_eq!(modules.last().map(|(_, r)| r.x + r.w), Some(w));
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
