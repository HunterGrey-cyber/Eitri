use std::collections::BTreeMap;

use super::super::fixture::fixture;
use super::super::geometry::{arrange, move_divider, settle_pins, Direction, Frame, Rect, Size};
use super::super::kill::{kill, Reopen};
use super::super::module::ModuleId;
use super::super::number::number_plan;
use super::super::ops::place;
use super::super::tree::{Axis, Layout, LayoutError, BELOW_ROOT_SHARE, DEFAULT_EDITOR_SHARE, RIGHT_OF_ROOT_SHARE};
use super::*;

const FRAMES: [Size; 3] = [
    Size { w: 1280, h: 721 },
    Size { w: 1920, h: 1040 },
    Size { w: 800, h: 600 },
];

fn frame(size: Size) -> Frame<'static> {
    Frame::new(size, 1)
}

fn f721() -> Frame<'static> {
    frame(FRAMES[0])
}

fn id(s: &str) -> ModuleId {
    ModuleId::parse(s).unwrap()
}

fn rects(layout: &Layout, size: Size) -> BTreeMap<ModuleId, Rect> {
    arrange(layout, &frame(size)).modules.into_iter().collect()
}

/// `got` puts every module on screen where `want` does, at every frame size.
fn same_rects(got: &Layout, want: &Layout) {
    for size in FRAMES {
        assert_eq!(rects(got, size), rects(want, size), "at {}x{}", size.w, size.h);
    }
}

fn e() -> String {
    DEFAULT_EDITOR_SHARE.to_string()
}

fn b() -> String {
    BELOW_ROOT_SHARE.to_string()
}

/// The window as it starts: `[editor | agent]` with the hidden terminal pinned below the editor's own column.
fn default_window() -> Layout {
    fixture(&format!(
        "row(column(editor {}, terminal hidden pinned) {}, agent)",
        b(),
        e()
    ))
}

/// `items` as one fixture list, or the one item itself.
fn list(axis: &str, items: &[String]) -> String {
    if items.len() == 1 {
        items[0].clone()
    } else {
        format!("{axis}({})", items.join(", "))
    }
}

// --- move.far-<dir> ------------------------------------------------------------------------------

#[test]
fn far_down_takes_the_editor_with_its_hidden_terminal_row_and_pins_the_bottom() {
    let mut layout = default_window();
    assert_eq!(move_far(&mut layout, &id("editor"), Direction::Down, &f721()), Ok(true));
    let mut want = fixture(&format!(
        "column(agent {b}, column(editor {b}, terminal hidden pinned) pinned)",
        b = b()
    ));
    same_rects(&layout, &want);
    assert_eq!(
        rects(&layout, FRAMES[0])[&id("editor")],
        Rect {
            x: 0,
            y: 481,
            w: 1280,
            h: 240
        }
    );
    assert_eq!(layout.focus(), &id("editor"), "the keys stay");
    let mut settled = layout.clone();
    settle_pins(&mut settled, &f721());
    assert_eq!(
        rects(&settled, Size { w: 1280, h: 1041 })[&id("editor")].h,
        240,
        "a full-width bottom row keeps its height"
    );
    layout.show(&id("terminal")).unwrap();
    want.show(&id("terminal")).unwrap();
    same_rects(&layout, &want);
}

#[test]
fn far_left_puts_the_chat_left_of_everything_at_a_third() {
    let mut layout = default_window();
    layout.set_focus(&id("agent")).unwrap();
    assert_eq!(move_far(&mut layout, &id("agent"), Direction::Left, &f721()), Ok(true));
    let want = fixture(&format!(
        "row(agent {l}, column(editor {b}, terminal hidden pinned))",
        l = 1.0 - RIGHT_OF_ROOT_SHARE,
        b = b()
    ));
    same_rects(&layout, &want);
    assert_eq!(
        rects(&layout, FRAMES[0])[&id("agent")],
        Rect {
            x: 0,
            y: 0,
            w: 422,
            h: 721
        }
    );
}

#[test]
fn far_up_and_far_down_move_a_shown_terminal_across_the_whole_window() {
    let start = || {
        fixture(&format!(
            "row(column(editor {b}, terminal pinned focus) {e}, agent)",
            b = b(),
            e = e()
        ))
    };
    let mut up = start();
    assert_eq!(move_far(&mut up, &id("terminal"), Direction::Up, &f721()), Ok(true));
    same_rects(
        &up,
        &fixture(&format!(
            "column(terminal {u}, row(editor {e}, agent))",
            u = 1.0 - BELOW_ROOT_SHARE,
            e = e()
        )),
    );
    let mut down = start();
    assert_eq!(move_far(&mut down, &id("terminal"), Direction::Down, &f721()), Ok(true));
    same_rects(
        &down,
        &fixture(&format!(
            "column(row(editor {e}, agent) {b}, terminal pinned)",
            b = b(),
            e = e()
        )),
    );
    settle_pins(&mut down, &f721());
    assert_eq!(rects(&down, FRAMES[0])[&id("terminal")].h, 240);
    assert_eq!(
        rects(&down, Size { w: 1280, h: 1041 })[&id("terminal")].h,
        240,
        "a full-width bottom row keeps its height"
    );
}

#[test]
fn a_unit_already_along_that_edge_does_not_move_and_keeps_a_zoom() {
    let mut layout = default_window();
    layout.toggle_zoom(&id("editor"));
    let before = layout.clone();
    assert_eq!(
        move_far(&mut layout, &id("agent"), Direction::Right, &f721()),
        Ok(false)
    );
    assert_eq!(
        move_far(&mut layout, &id("editor"), Direction::Left, &f721()),
        Ok(false)
    );
    assert_eq!(layout, before, "nothing moved and the zoom is still on");
}

#[test]
fn the_only_module_on_screen_is_refused() {
    let mut layout = fixture("row(editor, agent hidden)");
    assert_eq!(
        move_far(&mut layout, &id("editor"), Direction::Left, &f721()),
        Err(LayoutError::LastVisible(id("editor")))
    );
    assert_eq!(
        move_far(&mut layout, &id("agent"), Direction::Left, &f721()),
        Err(LayoutError::Hidden(id("agent")))
    );
}

#[test]
fn a_panel_in_the_editors_place_takes_the_hidden_editor_along() {
    let mut layout = fixture(&format!("row(row(lua:m 0.5, editor hidden) {e}, agent)", e = e()));
    assert_eq!(move_far(&mut layout, &id("lua:m"), Direction::Down, &f721()), Ok(true));
    let mut want = fixture(&format!(
        "column(agent {b}, row(lua:m 0.5, editor hidden) pinned)",
        b = b()
    ));
    same_rects(&layout, &want);
    layout.show(&id("editor")).unwrap();
    want.show(&id("editor")).unwrap();
    same_rects(&layout, &want);
}

// --- container.toggle-split ------------------------------------------------------------------------

#[test]
fn toggle_split_turns_the_nearest_split_with_both_sides_on_screen() {
    let mut layout = fixture("row(editor 0.6, agent)");
    assert!(toggle_split(&mut layout, &id("editor")));
    same_rects(&layout, &fixture("column(editor 0.6, agent)"));

    let mut layout = default_window();
    assert!(toggle_split(&mut layout, &id("editor")));
    same_rects(
        &layout,
        &fixture(&format!(
            "column(column(editor {b}, terminal hidden pinned) {e}, agent)",
            b = b(),
            e = e()
        )),
    );

    let mut layout = fixture("column(row(editor, agent) 0.6, terminal pinned 200 focus)");
    assert!(toggle_split(&mut layout, &id("terminal")));
    same_rects(&layout, &fixture("row(row(editor, agent) 0.6, terminal)"));

    let mut alone = fixture("row(editor, agent hidden)");
    let before = alone.clone();
    assert!(!toggle_split(&mut alone, &id("editor")));
    assert_eq!(alone, before);
}

// --- rotate --------------------------------------------------------------------------------------------

#[test]
fn rotate_moves_every_module_on_screen_one_place_through_the_same_space() {
    let mut up = fixture("row(editor, agent, terminal)");
    assert!(rotate(&mut up, false));
    same_rects(&up, &fixture("row(agent, terminal, editor)"));
    assert_eq!(up.focus(), &id("editor"), "the keys follow their module");
    let mut down = fixture("row(editor, agent, terminal)");
    assert!(rotate(&mut down, true));
    same_rects(&down, &fixture("row(terminal, editor, agent)"));
}

#[test]
fn rotate_leaves_hidden_modules_and_pinned_space_where_they_are() {
    let mut layout = fixture("row(editor, lua:a hidden, agent, terminal)");
    assert!(rotate(&mut layout, false));
    let mut want = fixture("row(agent, lua:a hidden, terminal, editor)");
    same_rects(&layout, &want);
    layout.show(&id("lua:a")).unwrap();
    want.show(&id("lua:a")).unwrap();
    same_rects(&layout, &want);

    let mut pinned = fixture(&format!(
        "row(column(editor {b}, terminal pinned 200) {e}, agent)",
        b = b(),
        e = e()
    ));
    assert!(rotate(&mut pinned, false));
    same_rects(
        &pinned,
        &fixture(&format!(
            "row(column(terminal {b}, agent pinned 200) {e}, editor)",
            b = b(),
            e = e()
        )),
    );
}

#[test]
fn rotating_up_then_down_is_where_it_started_and_one_module_does_not_rotate() {
    let start = fixture(&format!(
        "row(column(editor {b}, terminal pinned) {e}, agent)",
        b = b(),
        e = e()
    ));
    let mut layout = start.clone();
    assert!(rotate(&mut layout, false));
    assert!(rotate(&mut layout, true));
    same_rects(&layout, &start);
    let mut alone = fixture("row(editor, agent hidden)");
    assert!(!rotate(&mut alone, false));
}

// --- flip ----------------------------------------------------------------------------------------------

#[test]
fn flip_horizontal_mirrors_left_and_right() {
    let mut layout = fixture("row(editor 0.6, agent)");
    assert!(flip(&mut layout, Axis::Row));
    let r = rects(&layout, FRAMES[0]);
    assert_eq!(
        r[&id("agent")],
        Rect {
            x: 0,
            y: 0,
            w: 512,
            h: 721
        }
    );
    assert_eq!(
        r[&id("editor")],
        Rect {
            x: 513,
            y: 0,
            w: 767,
            h: 721
        }
    );
}

#[test]
fn a_pin_moves_with_its_module_when_the_layout_flips() {
    let mut layout = fixture("column(row(editor, agent) 0.6, terminal pinned 200)");
    assert!(flip(&mut layout, Axis::Column));
    assert_eq!(
        rects(&layout, FRAMES[0])[&id("terminal")],
        Rect {
            x: 0,
            y: 0,
            w: 1280,
            h: 200
        }
    );
}

#[test]
fn flipping_twice_is_where_it_started() {
    for start in [
        "row(editor 0.6, column(agent 0.3, terminal))".to_string(),
        format!(
            "row(column(editor {b}, terminal pinned 200) {e}, agent)",
            b = b(),
            e = e()
        ),
        "column(row(editor 0.7, agent) 0.6, terminal pinned 150)".to_string(),
    ] {
        for axis in [Axis::Row, Axis::Column] {
            let mut layout = fixture(&start);
            assert!(flip(&mut layout, axis));
            assert!(flip(&mut layout, axis));
            same_rects(&layout, &fixture(&start));
        }
    }
}

#[test]
fn a_verb_with_nothing_to_do_changes_nothing_and_keeps_the_zoom() {
    let mut layout = fixture("row(editor, agent)");
    layout.toggle_zoom(&id("editor"));
    let before = layout.clone();
    assert!(!flip(&mut layout, Axis::Column));
    assert_eq!(layout, before);
    let mut alone = fixture("row(editor 0.6, agent hidden)");
    let before = alone.clone();
    assert!(!flip(&mut alone, Axis::Row), "one module on screen: nothing to mirror");
    assert_eq!(alone, before);
}

// --- presets, next-layout ------------------------------------------------------------------------------

const MODULES: [&str; 6] = ["editor", "agent", "terminal", "lua:a", "lua:b", "lua:c"];

fn preset_want(p: Preset, ids: &[String]) -> String {
    let rest = &ids[1..];
    match p {
        Preset::EvenHorizontal => list("row", ids),
        Preset::EvenVertical => list("column", ids),
        Preset::MainVertical => format!("row({} {}, {})", ids[0], e(), list("column", rest)),
        Preset::MainHorizontal => format!("column({} {}, {})", ids[0], b(), list("row", rest)),
        Preset::Tiled => {
            let (mut rows, mut cols) = (1, 1);
            while rows * cols < ids.len() {
                rows += 1;
                if rows * cols < ids.len() {
                    cols += 1;
                }
            }
            let lines: Vec<String> = ids.chunks(cols).map(|c| list("row", c)).collect();
            list("column", &lines)
        }
    }
}

#[test]
fn each_preset_for_two_to_six_modules() {
    for n in 2..=6 {
        let ids: Vec<String> = MODULES[..n].iter().map(|s| s.to_string()).collect();
        for p in Preset::CYCLE {
            let mut layout = fixture(&list("row", &ids));
            assert_eq!(preset(&mut layout, p), Ok(()), "{p:?} {n}");
            same_rects(&layout, &fixture(&preset_want(p, &ids)));
            assert_eq!(layout.last_preset(), Some(p));
        }
    }
}

#[test]
fn the_main_module_is_the_first_in_tree_order() {
    let mut layout = fixture("column(row(terminal, agent), editor)");
    preset(&mut layout, Preset::MainVertical).unwrap();
    same_rects(
        &layout,
        &fixture(&format!("row(terminal {}, column(agent, editor))", e())),
    );
}

#[test]
fn main_vertical_of_the_editor_and_the_chat_is_the_default_window() {
    let mut layout = fixture("column(editor, agent)");
    preset(&mut layout, Preset::MainVertical).unwrap();
    same_rects(&layout, &Layout::initial(&[]).unwrap());
    assert_eq!(
        rects(&layout, FRAMES[0])[&id("editor")],
        Rect {
            x: 0,
            y: 0,
            w: 760,
            h: 721
        }
    );
}

#[test]
fn a_hidden_bottom_row_stays_around_a_preset_and_other_hidden_modules_collapse_at_the_end() {
    let mut layout = fixture(&format!(
        "column(row(editor, agent, terminal) {b}, lua:z hidden pinned)",
        b = b()
    ));
    preset(&mut layout, Preset::MainVertical).unwrap();
    let mut want = fixture(&format!(
        "column(row(editor {e}, column(agent, terminal)) {b}, lua:z hidden pinned)",
        b = b(),
        e = e()
    ));
    same_rects(&layout, &want);
    layout.show(&id("lua:z")).unwrap();
    want.show(&id("lua:z")).unwrap();
    same_rects(&layout, &want);

    let mut layout = fixture("row(editor, lua:h hidden, agent, terminal)");
    preset(&mut layout, Preset::MainVertical).unwrap();
    let mut want = fixture(&format!(
        "row(editor {e}, column(agent 0.5, terminal 0.5, lua:h hidden))",
        e = e()
    ));
    same_rects(&layout, &want);
    layout.show(&id("lua:h")).unwrap();
    want.show(&id("lua:h")).unwrap();
    same_rects(&layout, &want);
}

#[test]
fn a_preset_needs_two_modules_and_takes_at_most_twenty() {
    let mut one = fixture("row(editor, agent hidden)");
    let before = one.clone();
    assert_eq!(
        preset(&mut one, Preset::Tiled),
        Err(LayoutError::LastVisible(id("editor")))
    );
    assert_eq!(one, before);
    let ids: Vec<String> = ["editor".to_string(), "agent".to_string()]
        .into_iter()
        .chain((1..=19).map(|i| format!("lua:p{i}")))
        .collect();
    let mut many = fixture(&list("row", &ids));
    assert_eq!(
        preset(&mut many, Preset::EvenHorizontal),
        Err(LayoutError::TooMany {
            limit: MAX_PRESET_MODULES
        })
    );
    assert_eq!(many.last_preset(), None);
    let mut twenty = fixture(&list("row", &ids[..20]));
    assert_eq!(preset(&mut twenty, Preset::Tiled), Ok(()));
}

#[test]
fn next_layout_cycles_tmuxs_five_and_restarts_after_a_structural_change() {
    let mut layout = fixture("row(editor, agent, terminal)");
    for want in [
        Preset::EvenHorizontal,
        Preset::EvenVertical,
        Preset::MainHorizontal,
        Preset::MainVertical,
        Preset::Tiled,
        Preset::EvenHorizontal,
    ] {
        assert_eq!(cycle_preset(&mut layout, true), Ok(want));
    }
    assert_eq!(cycle_preset(&mut layout, false), Ok(Preset::Tiled));
    assert!(rotate(&mut layout, false));
    assert_eq!(layout.last_preset(), None, "a rotation changes the tree's shape");
    assert_eq!(cycle_preset(&mut layout, true), Ok(Preset::EvenHorizontal));
    let mut fresh = fixture("row(editor, agent)");
    assert_eq!(
        cycle_preset(&mut fresh, false),
        Ok(Preset::Tiled),
        "previous from none starts at the last"
    );
    let mut alone = fixture("row(editor, agent hidden)");
    assert_eq!(
        cycle_preset(&mut alone, true),
        Err(LayoutError::LastVisible(id("editor")))
    );
    assert_eq!(alone.last_preset(), None);
}

#[test]
fn a_kill_restarts_layout_next_whatever_reopens_the_module() {
    for reopen in [Reopen::InPlace, Reopen::Never] {
        let mut layout = fixture("row(editor, agent, terminal)");
        assert_eq!(cycle_preset(&mut layout, true), Ok(Preset::EvenHorizontal));
        kill(&mut layout, &id("editor"), reopen, &f721()).unwrap();
        assert_eq!(layout.last_preset(), None, "{reopen:?}");
        assert_eq!(
            cycle_preset(&mut layout, true),
            Ok(Preset::EvenHorizontal),
            "{reopen:?}"
        );
    }
}

// --- layout.spread -------------------------------------------------------------------------------------

#[test]
fn spread_evens_the_targets_row_or_column_and_clears_its_pins() {
    let mut layout = fixture("row(editor 0.7, agent 0.8, terminal)");
    assert!(spread(&mut layout, &id("editor")));
    same_rects(&layout, &fixture("row(editor, agent, terminal)"));

    let mut layout = fixture("row(column(editor 0.8, terminal) 0.7, agent)");
    assert!(spread(&mut layout, &id("editor")));
    same_rects(&layout, &fixture("row(column(editor, terminal) 0.7, agent)"));

    let mut layout = fixture("column(row(editor, agent) 0.5, terminal pinned 100 focus)");
    assert!(spread(&mut layout, &id("terminal")));
    same_rects(&layout, &fixture("column(row(editor, agent), terminal)"));

    let mut layout = fixture("row(editor 0.7, lua:h hidden 0.5, agent)");
    assert!(spread(&mut layout, &id("editor")));
    same_rects(&layout, &fixture("row(editor 0.5, lua:h hidden 0.5, agent)"));

    let mut alone = fixture("row(editor, agent hidden)");
    assert!(!spread(&mut alone, &id("editor")));
}

// --- the mark ----------------------------------------------------------------------------------------

#[test]
fn a_mark_toggles_clears_and_moves() {
    let mut layout = fixture("row(editor, agent)");
    assert_eq!(layout.mark(), None);
    assert_eq!(layout.toggle_mark(&id("editor")), Ok(true));
    assert_eq!(layout.mark(), Some(&id("editor")));
    assert_eq!(layout.toggle_mark(&id("editor")), Ok(false));
    assert_eq!(layout.mark(), None);
    layout.set_mark(&id("agent")).unwrap();
    assert_eq!(
        layout.toggle_mark(&id("editor")),
        Ok(true),
        "marking another module moves the mark"
    );
    assert_eq!(layout.mark(), Some(&id("editor")));
    assert!(layout.clear_mark());
    assert!(!layout.clear_mark());
    assert_eq!(
        layout.set_mark(&id("lua:nowhere")),
        Err(LayoutError::NotInTree(id("lua:nowhere")))
    );
}

#[test]
fn retiring_the_marked_module_clears_the_mark_and_a_gone_module_cannot_be_marked() {
    let mut layout = fixture("row(editor, agent)");
    layout.set_mark(&id("editor")).unwrap();
    kill(&mut layout, &id("editor"), Reopen::Never, &f721()).unwrap();
    assert_eq!(layout.mark(), None);
    assert_eq!(layout.set_mark(&id("editor")), Err(LayoutError::Gone(id("editor"))));
}

#[test]
fn swap_marked_exchanges_two_modules_and_keeps_the_mark_and_the_keys() {
    let mut layout = fixture("row(editor 0.6, agent)");
    assert_eq!(swap_marked(&mut layout, &id("editor")), Err(LayoutError::NoMark));
    layout.set_mark(&id("editor")).unwrap();
    assert_eq!(
        swap_marked(&mut layout, &id("editor")),
        Err(LayoutError::MarkedIsTarget(id("editor")))
    );
    layout.set_mark(&id("agent")).unwrap();
    assert_eq!(swap_marked(&mut layout, &id("editor")), Ok(id("agent")));
    same_rects(&layout, &fixture("row(agent 0.6, editor)"));
    assert_eq!(layout.mark(), Some(&id("agent")));
    assert_eq!(layout.focus(), &id("editor"));
    let mut hidden = fixture("row(editor, agent, terminal hidden)");
    hidden.set_mark(&id("terminal")).unwrap();
    assert_eq!(
        swap_marked(&mut hidden, &id("editor")),
        Err(LayoutError::MarkHidden(id("terminal")))
    );
}

// --- layout.undo ---------------------------------------------------------------------------------------

#[test]
fn undo_steps_back_through_the_last_change_and_again_forward() {
    let mut layout = fixture("row(editor 0.6, column(agent, terminal))");
    let start = layout.clone();
    assert_eq!(layout.undo(), Err(LayoutError::NothingToUndo));
    preset(&mut layout, Preset::Tiled).unwrap();
    let tiled = layout.clone();
    layout.toggle_zoom(&id("agent"));
    assert_eq!(layout.undo(), Ok(()));
    same_rects(&layout, &start);
    assert_eq!(layout.zoomed(), None);
    assert_eq!(layout.last_preset(), None);
    assert_eq!(layout.undo(), Ok(()));
    same_rects(&layout, &tiled);
}

#[test]
fn undo_is_refused_once_a_module_joined_the_tree() {
    let mut layout = fixture("row(editor, agent)");
    assert!(rotate(&mut layout, false));
    place(&mut layout, &id("lua:new"), &id("editor"), Axis::Row).unwrap();
    assert_eq!(layout.undo(), Err(LayoutError::NothingToUndo));
}

#[test]
fn undo_after_a_divider_drag_returns_to_before_the_whole_change() {
    let mut layout = fixture("row(editor, agent, terminal)");
    let start = layout.clone();
    preset(&mut layout, Preset::MainVertical).unwrap();
    let divider = arrange(&layout, &f721()).dividers[0].clone();
    move_divider(&mut layout, &divider, divider.first_px + 40).unwrap();
    layout.undo().unwrap();
    same_rects(&layout, &start);
}

// --- select.number's plan ------------------------------------------------------------------------------

#[test]
fn numbers_run_one_to_nine_then_zero_over_the_modules_on_screen() {
    assert_eq!(
        number_plan(&default_window()),
        vec![('1', id("editor")), ('2', id("agent"))]
    );
    let ids: Vec<String> = ["editor".to_string(), "agent".to_string()]
        .into_iter()
        .chain((1..=9).map(|i| format!("lua:n{i}")))
        .collect();
    let plan = number_plan(&fixture(&list("row", &ids)));
    assert_eq!(plan.iter().map(|(d, _)| *d).collect::<String>(), "1234567890");
    assert_eq!(
        plan.iter().map(|(_, m)| m.to_string()).collect::<Vec<_>>(),
        ids[..10].to_vec(),
        "an eleventh module gets no number"
    );
    let mut zoomed = default_window();
    zoomed.toggle_zoom(&id("agent"));
    assert_eq!(number_plan(&zoomed), vec![('1', id("agent"))]);
}

// --- rules common to every verb --------------------------------------------------------------------

#[test]
fn every_verb_that_acts_ends_a_zoom() {
    type Verb = fn(&mut Layout) -> bool;
    let verbs: [(&str, Verb); 8] = [
        ("move.far-down", |l| {
            move_far(l, &ModuleId::editor(), Direction::Down, &Frame::new(FRAMES[0], 1)) == Ok(true)
        }),
        ("toggle-split", |l| toggle_split(l, &ModuleId::editor())),
        ("rotate", |l| rotate(l, false)),
        ("flip", |l| flip(l, Axis::Row)),
        ("preset", |l| preset(l, Preset::EvenVertical).is_ok()),
        ("spread", |l| spread(l, &ModuleId::editor())),
        ("swap.marked", |l| {
            l.set_mark(&ModuleId::agent()).unwrap();
            swap_marked(l, &ModuleId::editor()).is_ok()
        }),
        ("undo", |l| {
            rotate(l, false);
            l.toggle_zoom(&ModuleId::editor());
            l.undo().is_ok()
        }),
    ];
    for (name, verb) in verbs {
        let mut layout = default_window();
        layout.toggle_zoom(&id("editor"));
        assert!(verb(&mut layout), "{name} acted");
        assert_eq!(layout.zoomed(), None, "{name} ended the zoom");
    }
}

#[test]
fn a_retired_editor_stays_hidden_through_every_rearrangement() {
    let mut layout = fixture("row(editor, agent, terminal)");
    kill(&mut layout, &id("editor"), Reopen::Never, &f721()).unwrap();
    assert!(rotate(&mut layout, false));
    assert!(flip(&mut layout, Axis::Row));
    for p in Preset::CYCLE {
        preset(&mut layout, p).unwrap();
    }
    assert!(!layout.is_shown(&id("editor")) && layout.is_gone(&id("editor")));
    assert!(number_plan(&layout).iter().all(|(_, m)| *m != id("editor")));
    assert_eq!(layout.show(&id("editor")), Err(LayoutError::Gone(id("editor"))));
}
