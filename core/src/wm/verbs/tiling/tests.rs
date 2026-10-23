use super::super::rig::*;
use crate::keymap::Action;
use crate::layout::fixture::fixture;
use crate::layout::{
    move_far, Axis, Direction, Float, FloatRect, Frame, Layout, ModuleId, Node, Selection, Size, Workspaces,
};
use crate::plugin::test_support::Scratch;
use crate::wm::grid::UNALLOCATED;
use crate::wm::persist::Saver;
use crate::wm::*;

fn float(id: &ModuleId) -> Float {
    Float {
        id: id.clone(),
        rect: FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 300,
            h: 200,
        },
        scratch: false,
    }
}

/// `editor | agent` tiled, with `floats` above them bottom first, the keys on `keys`.
fn with_floats(floats: &[ModuleId], keys: &ModuleId) -> Layout {
    Layout::from_parts_floating(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        ),
        floats.iter().map(float).collect(),
        Default::default(),
        keys.clone(),
        Default::default(),
    )
    .unwrap()
}

fn restacks(batch: &Batch) -> Vec<Vec<RestackMove>> {
    batch
        .ops()
        .filter_map(|op| match op {
            HostOp::Restack { moves, .. } => Some(moves.clone()),
            _ => None,
        })
        .collect()
}

fn saving(scratch: &Scratch) -> impl FnOnce(&Layout, &Workspaces) -> Option<Saver> + '_ {
    move |layout, workspaces| {
        Some(Saver::new(
            Some(scratch.0.clone()),
            &scratch.0,
            Vec::new(),
            None,
            layout,
            workspaces,
        ))
    }
}

#[test]
fn a_refused_verb_emits_no_allocate_and_is_no_change_for_the_saver() {
    let scratch = Scratch::new("refused-verb");
    let mut r = rig_with(fixture("row(editor, agent hidden)"), saving(&scratch));
    let before = r.layout.borrow().clone();
    let batch = r.run(Action::ToggleSplit);
    assert_eq!(
        ops(&batch),
        vec![refuse("toggle-split: no split around the module on screen")]
    );
    assert!(batch.lua().is_empty() && batch.shell_verb().is_none());
    assert_eq!(*r.layout.borrow(), before);
    assert!(r.scheduler.armed().is_empty(), "no save is armed for a refusal");
    assert!(r
        .controller
        .notes(WIN)
        .contains(&"[modules] toggle-split: no split around the module on screen".to_string()));
}

#[test]
fn a_verb_that_changes_the_layout_arms_the_save_and_redraws_after_its_layout_ops() {
    let scratch = Scratch::new("changed-verb");
    let mut r = rig_with(fixture("row(editor, agent)"), saving(&scratch));
    assert!(r.scheduler.armed().is_empty(), "the first apply is not a change");
    let batch = r.run(Action::ToggleSplit);
    let ops = ops(&batch);
    assert_eq!(
        moves(&batch),
        vec![allocate()],
        "nothing comes or goes; the grid allocates"
    );
    let allocated = ops.iter().position(|op| *op == allocate()).unwrap();
    let hooks = ops.iter().position(|op| *op == chrome_hooks()).unwrap();
    assert!(allocated < hooks, "the change hooks follow the layout ops: {ops:#?}");
    assert_eq!(r.scheduler.armed().len(), 1, "the save's debounce is armed");
}

#[test]
fn the_first_apply_maps_every_visible_hosted_module_and_restacks_the_floats() {
    let (x, y) = (ModuleId::lua("x"), ModuleId::lua("y"));
    let r = rig(with_floats(&[x.clone(), y.clone()], &y));
    assert_eq!(
        moves(&r.first),
        vec![
            map(&ModuleId::editor()),
            map(&ModuleId::agent()),
            map(&x),
            map(&y),
            HostOp::Restack {
                win: WIN,
                moves: vec![
                    RestackMove {
                        module: x.clone(),
                        after: None
                    },
                    RestackMove {
                        module: y.clone(),
                        after: Some(x)
                    },
                ],
            },
            allocate(),
        ]
    );
    assert_eq!(ops(&r.first).last(), Some(&chrome_hooks()));
}

#[test]
fn restack_never_names_the_editor() {
    let (x, y) = (ModuleId::lua("x"), ModuleId::lua("y"));
    let mut r = rig(with_floats(&[x.clone(), y.clone()], &y));
    // The top float docks along the bottom edge: the floats on screen change.
    let batch = r.run(Action::MoveFar(Direction::Down));
    assert!(!r.layout.borrow().is_floating(&y), "the premise: the float docked");
    let restacked = restacks(&batch);
    assert_eq!(
        restacked,
        vec![vec![RestackMove {
            module: x.clone(),
            after: None
        }]]
    );
    for moves in restacks(&r.first).into_iter().chain(restacked) {
        assert!(
            moves
                .iter()
                .all(|m| m.module != ModuleId::editor() && m.after != Some(ModuleId::editor())),
            "{moves:?}"
        );
    }
}

#[test]
fn a_swap_marked_between_a_float_and_a_tile_restacks_the_float_band() {
    let x = ModuleId::lua("x");
    let mut layout = with_floats(std::slice::from_ref(&x), &x);
    layout.set_mark(&ModuleId::agent()).unwrap();
    let mut r = rig(layout);
    let batch = r.run(Action::SwapMarked);
    assert!(r.layout.borrow().is_floating(&ModuleId::agent()) && !r.layout.borrow().is_floating(&x));
    assert_eq!(
        moves(&batch),
        vec![
            HostOp::Restack {
                win: WIN,
                moves: vec![RestackMove {
                    module: ModuleId::agent(),
                    after: None
                }],
            },
            allocate(),
        ],
        "both stay on screen; only the band changes"
    );
    assert!(r
        .controller
        .notes(WIN)
        .contains(&"[modules] lua:x: swapped with the marked agent".to_string()));
}

#[test]
fn a_zoom_unmaps_what_it_hides_and_its_end_maps_it_back_before_allocating() {
    let mut r = rig(fixture("row(editor, column(agent, terminal))"));
    let (agent, terminal) = (ModuleId::agent(), ModuleId::terminal());
    let zoomed = r.run(Action::Zoom);
    assert_eq!(moves(&zoomed), vec![unmap(&agent), unmap(&terminal), allocate()]);
    let unzoomed = r.run(Action::Zoom);
    assert_eq!(moves(&unzoomed), vec![map(&agent), map(&terminal), allocate()]);
    assert_eq!(
        r.controller.notes(WIN),
        vec![
            "[module_grid] zoomed editor".to_string(),
            "[module_grid] unzoomed".to_string()
        ]
    );
}

#[test]
fn a_verb_before_the_first_allocation_arranges_against_the_stand_in_content() {
    let layout = fixture("row(editor focus, agent)");
    let mut want = layout.clone();
    assert!(move_far(
        &mut want,
        &ModuleId::editor(),
        Direction::Right,
        &Frame::new(UNALLOCATED, 1)
    )
    .unwrap());
    let mut r = rig(layout);
    assert_eq!(r.host.grid_size(WIN), UNALLOCATED, "the premise: nothing allocated yet");
    let batch = r.run(Action::MoveFar(Direction::Right));
    assert_eq!(*r.layout.borrow(), want);
    assert_eq!(moves(&batch), vec![allocate()]);
}

#[test]
fn a_resize_steps_by_the_editors_cell_or_the_fallback_cell() {
    let layout = fixture("row(editor focus, agent)");
    let at = |cell: Option<(f64, f64)>| {
        let mut r = rig(layout.clone());
        r.host.set_editor_cell(cell);
        r.run(Action::Resize {
            dir: Direction::Right,
            cells: 5,
        });
        let rects = crate::layout::arrange(&r.layout.borrow(), &Frame::new(UNALLOCATED, 1));
        rects.rect_of(&ModuleId::editor()).unwrap().w
    };
    let before = crate::layout::arrange(&layout, &Frame::new(UNALLOCATED, 1))
        .rect_of(&ModuleId::editor())
        .unwrap()
        .w;
    assert_eq!(
        at(None) - before,
        super::resize_px(Direction::Right, super::FALLBACK_CELL, 5)
    );
    assert_eq!(at(Some((10.0, 20.0))) - before, 50);
}

#[test]
fn a_container_verb_with_no_container_flashes_without_a_toast() {
    let mut r = rig(fixture("row(editor, agent hidden)"));
    let batch = r.run(Action::ContainerTabbed);
    assert_eq!(ops(&batch), vec![refuse("")]);
    assert!(r
        .controller
        .notes(WIN)
        .contains(&"[modules] container.tabbed: no container around editor".to_string()));
}

#[test]
fn a_focus_parent_redraws_the_selection_and_is_no_change_for_the_grid() {
    let mut r = rig(fixture("row(editor, column(agent focus, terminal))"));
    let batch = r.run(Action::FocusParent);
    assert!(matches!(r.layout.borrow().selection(), Selection::Con(_)));
    assert_eq!(ops(&batch), vec![chrome_hooks()]);
    // Up to the root, then nothing above it.
    let mut last = Batch::default();
    for _ in 0..3 {
        last = r.run(Action::FocusParent);
    }
    assert_eq!(ops(&last), vec![refuse("focus.parent: nothing to select")]);
}

#[test]
fn the_mark_verbs_redraw_the_mark_without_moving_a_host() {
    let mut r = rig(fixture("row(editor, agent)"));
    let marked = r.run(Action::MarkToggle);
    assert_eq!(ops(&marked), vec![chrome_hooks()]);
    assert_eq!(r.layout.borrow().mark(), Some(&ModuleId::editor()));
    let cleared = r.run(Action::MarkClear);
    assert_eq!(ops(&cleared), vec![chrome_hooks()]);
    let none = r.run(Action::MarkClear);
    assert_eq!(ops(&none), vec![chrome_hooks()], "the mark's text is redrawn as before");
    assert!(r
        .controller
        .notes(WIN)
        .contains(&"[modules] no module is marked".to_string()));
}

#[test]
fn a_layout_borrowed_by_the_shell_runs_no_verb() {
    let mut r = rig(fixture("row(editor, agent)"));
    let layout = r.layout.clone();
    let _held = layout.borrow_mut();
    let batch = r.run(Action::ToggleSplit);
    assert!(batch.is_empty());
    assert!(r
        .controller
        .notes(WIN)
        .iter()
        .any(|line| line.starts_with("[wm] BUG: the layout was borrowed when")));
}

#[test]
fn the_stand_in_frame_is_the_one_the_grid_would_measure_now() {
    let r = rig(fixture("row(editor, agent)"));
    r.host.set_grid_size(Size { w: 1920, h: 1040 });
    assert_eq!(
        r.controller.min_size(WIN),
        crate::layout::min_size(&r.layout.borrow(), &Frame::new(Size { w: 1920, h: 1040 }, 1))
    );
}
