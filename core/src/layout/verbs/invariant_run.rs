//! Random verbs over random layouts, checking after every step what every layout must hold.

use std::collections::BTreeSet;

use super::super::geometry::{arrange, hide, resize, settle_pins, Direction, Frame, Size};
use super::super::kill::{kill, Reopen};
use super::super::module::{ModuleDecl, ModuleId, Placement};
use super::super::ops::{even, place, swap, swap_adjacent};
use super::super::tree::{Axis, Layout};
use super::*;

/// xorshift64*: a seeded generator without a new dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const FRAMES: [Size; 3] = [
    Size { w: 1280, h: 721 },
    Size { w: 1920, h: 1040 },
    Size { w: 800, h: 600 },
];
const DIRS: [Direction; 4] = [Direction::Left, Direction::Down, Direction::Up, Direction::Right];
const PLACEMENTS: [Placement; 4] = [
    Placement::RightOfRoot,
    Placement::BelowRoot,
    Placement::InPlaceOfEditor,
    Placement::BelowEditor,
];
const EXTRA: [&str; 4] = ["terminal", "lua:a", "lua:b", "lua:c"];

fn random_layout(rng: &mut Rng) -> Layout {
    let mut extra = Vec::new();
    for name in EXTRA {
        if rng.below(4) != 0 {
            let placement = PLACEMENTS[rng.below(PLACEMENTS.len())];
            extra.push(ModuleDecl {
                id: ModuleId::parse(name).unwrap(),
                placement,
            });
        }
    }
    Layout::initial(&extra).expect("distinct ids")
}

fn step(rng: &mut Rng, layout: &mut Layout, frame: &Frame) {
    let ids = layout.leaves();
    let any = ids[rng.below(ids.len())].clone();
    let focus = layout.focus().clone();
    let dir = DIRS[rng.below(4)];
    let axis = if rng.below(2) == 0 { Axis::Row } else { Axis::Column };
    let flag = rng.below(2) == 0;
    let verb = rng.below(30);
    match verb {
        0 => {
            let _ = hide(layout, &any, frame);
        }
        1 => {
            let _ = layout.show(&any);
        }
        2 => {
            let _ = layout.set_focus(&any);
        }
        3 => {
            let _ = layout.toggle_zoom(&focus);
        }
        4 => {
            let _ = place(layout, &any, &focus, axis);
        }
        5 => {
            let _ = swap(layout, &focus, dir, frame);
        }
        6 => {
            let _ = swap_adjacent(layout, &focus, flag);
        }
        7 => even(layout, axis),
        8 => {
            let _ = resize(layout, &focus, dir, 40, frame);
        }
        9 => {
            let _ = settle_pins(layout, frame);
        }
        10 => {
            let _ = move_far(layout, &focus, dir, frame);
        }
        11 => {
            let _ = toggle_split(layout, &focus);
        }
        12 => {
            let _ = rotate(layout, flag);
        }
        13 => {
            let _ = flip(layout, axis);
        }
        14 => {
            let _ = preset(layout, Preset::CYCLE[rng.below(5)]);
        }
        15 => {
            let _ = cycle_preset(layout, flag);
        }
        16 => {
            let _ = spread(layout, &focus);
        }
        17 => {
            let _ = layout.toggle_mark(&any);
        }
        18 => {
            let _ = swap_marked(layout, &focus);
        }
        19 => {
            let _ = layout.undo();
        }
        20 => {
            let reopen = match rng.below(3) {
                0 => Reopen::InPlace,
                1 => Reopen::At(Placement::BelowEditor),
                _ => Reopen::Never,
            };
            let _ = kill(layout, &any, reopen, frame);
        }
        21 => {
            let _ = container_mode(layout, &focus, ConLayout::Tabbed);
        }
        22 => {
            let _ = container_mode(layout, &focus, ConLayout::Stacked);
        }
        23 => {
            let _ = switch_tab(layout, &any);
        }
        24 => {
            let _ = place(layout, &focus, &any, axis);
        }
        25 => {
            let _ = move_dir(layout, &focus, dir);
        }
        26 => {
            focus_parent(layout);
        }
        27 => {
            focus_child(layout);
        }
        28 => {
            focus_parent(layout);
            layout.toggle_zoom(&focus);
        }
        _ => {
            let _ = layout.set_mark(&any);
        }
    }
    if matches!(verb, 3 | 5..=19 | 21..=22 | 24..=29) {
        assert_eq!(layout.focus(), &focus, "verb {verb} moved the keys");
    }
}

fn check(layout: &Layout, label: &str) {
    let leaves = layout.leaves();
    let mut seen = BTreeSet::new();
    for id in &leaves {
        assert!(seen.insert(id.clone()), "{label}: {id} appears twice");
    }
    assert!(
        layout.hidden().iter().all(|id| seen.contains(id)),
        "{label}: hidden names a stranger"
    );
    assert!(
        !layout.visible_leaves().is_empty(),
        "{label}: nothing on screen: {layout:?}"
    );
    if let Selection::Con(id) = layout.selection() {
        assert!(
            layout.root().find(id).is_some(),
            "{label}: selection names a missing container"
        );
    }
    if let Some(crate::layout::Zoom::Con(id)) = layout.zoom() {
        assert!(
            layout.root().find(*id).is_some(),
            "{label}: zoom names a missing container"
        );
        assert!(layout.visible_leaves().iter().all(|m| !layout.zoom_hides(m)));
    }
    if let Some(z) = layout.zoomed() {
        assert!(!layout.hidden().contains(z), "{label}: {z} zoomed and hidden");
    }
    assert!(
        seen.contains(layout.focus()) && layout.is_in_view(layout.focus()),
        "{label}: the keys are on {}, hidden or not in the tree",
        layout.focus()
    );
    let mut mru = layout.mru().to_vec();
    mru.sort();
    let mut sorted = leaves.clone();
    sorted.sort();
    assert_eq!(mru, sorted, "{label}: the MRU list holds every leaf once");
    assert_eq!(
        layout.root().check(true),
        Ok(()),
        "{label}: a container breaks an invariant: {:?}",
        layout.root()
    );
    let ids = layout.root().ids();
    let distinct: BTreeSet<_> = ids.iter().collect();
    assert_eq!(
        distinct.len(),
        ids.len(),
        "{label}: a container id appears twice {ids:?}"
    );
    if let Some(m) = layout.mark() {
        assert!(seen.contains(m) && !layout.is_gone(m), "{label}: the mark is on {m}");
    }
    for size in FRAMES {
        let modules = arrange(layout, &Frame::new(size, 1)).modules;
        for (i, (a, ra)) in modules.iter().enumerate() {
            assert!(
                ra.x >= 0 && ra.y >= 0 && ra.x + ra.w <= size.w && ra.y + ra.h <= size.h,
                "{label}: {a} {ra:?} outside {size:?}"
            );
            // At most six modules: even all in one row they fit at 32px each in 800x600.
            assert!(ra.w >= 32 && ra.h >= 32, "{label}: {a} {ra:?} below 32px at {size:?}");
            for (b, rb) in &modules[i + 1..] {
                let apart = ra.x + ra.w <= rb.x || rb.x + rb.w <= ra.x || ra.y + ra.h <= rb.y || rb.y + rb.h <= ra.y;
                assert!(apart, "{label}: {a} {ra:?} overlaps {b} {rb:?}");
            }
        }
    }
}

#[test]
fn twenty_thousand_random_verbs_keep_every_invariant() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for run in 0..200 {
        let mut layout = random_layout(&mut rng);
        check(&layout, &format!("run {run}, start"));
        for n in 0..100 {
            let frame = Frame::new(FRAMES[rng.below(3)], 1);
            step(&mut rng, &mut layout, &frame);
            check(&layout, &format!("run {run}, step {n}"));
        }
    }
}
