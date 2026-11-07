//! Random verbs over random layouts, checking after every step what every layout must hold.

use std::collections::BTreeSet;

use super::super::con::Child;
use super::super::float::FloatRect;
use super::super::geometry::{arrange, hide, resize, settle_pins, Direction, Frame, Rect, Size};
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

const FRAMES: [Size; 4] = [
    Size { w: 1280, h: 721 },
    Size { w: 1920, h: 1040 },
    Size { w: 800, h: 600 },
    // A window narrower and lower than its modules' minimums (a tiling compositor's small tile): drags and resizes
    // there must still be taken, never panic, and keep every module inside the window.
    Size { w: 120, h: 80 },
];
const RECTS: [FloatRect; 4] = [
    FloatRect {
        cx: 0.5,
        cy: 0.5,
        w: 768,
        h: 433,
    },
    FloatRect {
        cx: 0.1,
        cy: 0.9,
        w: 300,
        h: 200,
    },
    FloatRect {
        cx: 1.0,
        cy: 0.0,
        w: 2000,
        h: 2000,
    },
    FloatRect {
        cx: 0.3,
        cy: 0.6,
        w: 40,
        h: 40,
    },
];
const DIRS: [Direction; 4] = [Direction::Left, Direction::Down, Direction::Up, Direction::Right];
const PLACEMENTS: [Placement; 6] = [
    Placement::RightOfRoot,
    Placement::LeftOfRoot,
    Placement::BelowRoot,
    Placement::AboveRoot,
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

/// A root of one tile -- the editor alone, or one time in three a tabbed root holding only the editor
/// -- with every other module floating: the start where a sink's anchor is the root's only child.
fn lone_tile_layout(rng: &mut Rng) -> Layout {
    let editor = ModuleId::editor();
    let root = if rng.below(3) == 0 {
        Node::Con(Con::new(
            ConLayout::Tabbed,
            vec![Child::new(Node::Leaf(editor.clone()), 1.0)],
        ))
    } else {
        Node::Leaf(editor.clone())
    };
    let mut layout = Layout::new(root, editor).expect("one tile is a valid layout");
    let mut floats = vec![ModuleId::agent()];
    for name in EXTRA {
        if rng.below(4) != 0 {
            floats.push(ModuleId::parse(name).unwrap());
        }
    }
    for id in floats {
        layout
            .add_float(&id, RECTS[rng.below(RECTS.len())], false)
            .expect("a new module with a valid rectangle");
    }
    layout
}

fn step(rng: &mut Rng, layout: &mut Layout, frame: &Frame) {
    let ids = layout.modules();
    let any = ids[rng.below(ids.len())].clone();
    let focus = layout.focus().clone();
    let dir = DIRS[rng.below(4)];
    let axis = if rng.below(2) == 0 { Axis::Row } else { Axis::Column };
    let flag = rng.below(2) == 0;
    let verb = rng.below(40);
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
        29 => {
            let _ = layout.set_mark(&any);
        }
        30 => {
            let id = (2..=4)
                .map(|number| ModuleId::instance(super::super::module::ModuleKind::Terminal, number).unwrap())
                .find(|id| !layout.contains(id));
            if let Some(id) = id {
                let _ = layout.add_float(&id, RECTS[rng.below(4)], flag);
            }
        }
        31 => {
            if any.number() >= 2 {
                let _ = layout.remove(&any);
            }
        }
        32 => {
            let _ = float_toggle(layout, &focus, frame);
        }
        33 => {
            let _ = scratchpad_move(layout, &focus, frame);
        }
        34 => {
            let _ = scratchpad_show(layout, None, frame);
        }
        35 => {
            let _ = scratchpad_show(layout, Some(&any), frame);
        }
        36 => {
            let _ = move_float(layout, &focus, dir, 40, frame);
        }
        37 => {
            let _ = set_float_rect(
                layout,
                &focus,
                Rect {
                    x: rng.below(1200) as i32 - 100,
                    y: rng.below(700) as i32 - 50,
                    w: 50 + rng.below(1000) as i32,
                    h: 50 + rng.below(800) as i32,
                },
                frame,
            );
        }
        38 => {
            // A pixel length on a module's slot (a rail's width in a row, a strip's height in a column), as a rule
            // gives one; kept small enough that nine modules still fit the smallest frame.
            let rule = super::super::rules::OpenRule {
                size: Some(super::super::rules::RuleSize::Pixels(16 + rng.below(45) as i32)),
                focus: false,
                ..super::super::rules::OpenRule::default()
            };
            let _ = super::super::rules::apply_launch_rule(layout, &any, &rule, super::super::rules::NOMINAL_GRID);
        }
        39 => {
            // A drag of any divider on screen, fixed children's edges included.
            // One module measures a minimum of its own (a page's), so the drags meet clamped children too.
            let measured = |id: &ModuleId| {
                if *id == ModuleId::lua("b") {
                    Size { w: 150, h: 90 }
                } else {
                    Size::default()
                }
            };
            let frame = &Frame {
                min: &measured,
                ..Frame::new(frame.size, frame.handle_px)
            };
            let before = arrange(layout, frame);
            if !before.dividers.is_empty() {
                let d = before.dividers[rng.below(before.dividers.len())].clone();
                // A fixed child's own (right or lower) edge: only it and the neighbour across that edge may change.
                let own_edge = layout.root().find(d.con).and_then(|c| {
                    let Node::Leaf(m) = &c.children[d.index].node else {
                        return None;
                    };
                    let (h, w) = super::super::rules::fixed_lengths_of(layout, m);
                    let fixed = match d.axis {
                        Axis::Row => w,
                        Axis::Column => h,
                    };
                    fixed?;
                    let next = (d.index + 1..c.children.len())
                        .find(|&i| c.children[i].node.leaves().iter().any(|l| layout.is_shown(l)))?;
                    let mut moving = c.children[next].node.leaves();
                    moving.push(m.clone());
                    Some(moving)
                });
                let _ = super::super::geometry::move_divider(layout, &d, rng.below(150) as i32);
                let roomy = frame.size.w >= 800 && frame.size.h >= 600;
                if let (Some(moving), true) = (own_edge, roomy) {
                    let after = arrange(layout, frame);
                    for (id, r) in &before.modules {
                        if moving.contains(id) {
                            continue;
                        }
                        let Some(now) = after.rect_of(id) else { continue };
                        assert!(
                            (now.x - r.x).abs() <= 1
                                && (now.w - r.w).abs() <= 1
                                && (now.y - r.y).abs() <= 1
                                && (now.h - r.h).abs() <= 1,
                            "a fixed edge drag moved {id}: {r:?} -> {now:?}"
                        );
                    }
                }
            }
        }
        _ => unreachable!(),
    }
    if matches!(verb, 3 | 5..=19 | 21..=22 | 24..=30 | 32 | 36..=39) {
        assert_eq!(layout.focus(), &focus, "verb {verb} moved the keys");
    }
}

fn check(layout: &Layout, label: &str) {
    let modules = layout.modules();
    let mut seen = BTreeSet::new();
    for id in &modules {
        assert!(seen.insert(id.clone()), "{label}: {id} appears twice");
    }
    assert!(
        layout.hidden().iter().all(|id| seen.contains(id)),
        "{label}: hidden names a stranger"
    );
    assert!(
        !layout.visible_modules().is_empty(),
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
        assert!(layout.visible_modules().iter().all(|m| !layout.zoom_hides(m)));
    }
    if let Some(z) = layout.zoomed() {
        assert!(!layout.hidden().contains(z), "{label}: {z} zoomed and hidden");
    }
    assert!(
        seen.contains(layout.focus()) && layout.is_in_view(layout.focus()),
        "{label}: the keys are on {}, hidden or not in the layout",
        layout.focus()
    );
    let mut mru = layout.mru().to_vec();
    mru.sort();
    let mut sorted = modules.clone();
    sorted.sort();
    assert_eq!(mru, sorted, "{label}: the MRU list holds every module once");
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
    let leaves = layout.leaves();
    for float in layout.floats() {
        assert!(
            !leaves.contains(&float.id),
            "{label}: {} is tiled and floating",
            float.id
        );
        assert_ne!(float.id, ModuleId::editor(), "{label}: the editor floats");
        assert!(float.rect.is_valid(), "{label}: invalid float rectangle {float:?}");
    }
    for size in FRAMES {
        let arrangement = arrange(layout, &Frame::new(size, 1));
        for spot in &arrangement.floats {
            let rect = spot.rect;
            assert!(
                rect.x >= 0 && rect.y >= 0 && rect.x + rect.w <= size.w && rect.y + rect.h <= size.h,
                "{label}: {} {rect:?} outside {size:?}",
                spot.id
            );
            assert!(
                layout.is_floating(&spot.id) && layout.is_shown(&spot.id),
                "{label}: stray float spot {spot:?}"
            );
        }
        let modules = arrangement.modules;
        for (i, (a, ra)) in modules.iter().enumerate() {
            assert!(
                ra.x >= 0 && ra.y >= 0 && ra.x + ra.w <= size.w && ra.y + ra.h <= size.h,
                "{label}: {a} {ra:?} outside {size:?}"
            );
            // Nine modules (six plus terminal#2..#4) fit at 32px each in an 800x600 frame; a slot with a pixel length
            // may be narrower, down to that length. The small frame cannot hold them all.
            let (fixed_h, fixed_w) = super::super::rules::fixed_lengths_of(layout, a);
            let roomy = size.w >= 800 && size.h >= 600;
            assert!(
                !roomy || (ra.w >= 32.min(fixed_w.unwrap_or(32)) && ra.h >= 32.min(fixed_h.unwrap_or(32))),
                "{label}: {a} {ra:?} below 32px at {size:?}"
            );
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
            let frame = Frame::new(FRAMES[rng.below(FRAMES.len())], 1);
            step(&mut rng, &mut layout, &frame);
            check(&layout, &format!("run {run}, step {n}"));
        }
    }
}

#[test]
fn lone_tile_starts_keep_every_invariant() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for run in 0..50 {
        let mut layout = lone_tile_layout(&mut rng);
        check(&layout, &format!("lone run {run}, start"));
        for n in 0..100 {
            let frame = Frame::new(FRAMES[rng.below(FRAMES.len())], 1);
            step(&mut rng, &mut layout, &frame);
            check(&layout, &format!("lone run {run}, step {n}"));
        }
    }
}
