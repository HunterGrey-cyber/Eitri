//! Floats in the layout: the model's own rules, before any verb makes one.

use std::collections::{BTreeMap, BTreeSet};

use super::con::Node;
use super::float::{Float, FloatRect};
use super::geometry::{arrange, hide, Frame, Rect, Size};
use super::kill::{can_kill, kill, KillScope, Reopen};
use super::module::{ModuleDecl, ModuleId, ModuleKind, Placement};
use super::reconcile::{reconcile_default_floating, reconcile_floating};
use super::tray::tray;
use super::tree::{Axis, Layout, LayoutError, DEFAULT_EDITOR_SHARE};

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn agent() -> ModuleId {
    ModuleId::agent()
}
fn term() -> ModuleId {
    ModuleId::terminal()
}
fn t2() -> ModuleId {
    ModuleId::instance(ModuleKind::Terminal, 2).unwrap()
}
fn t3() -> ModuleId {
    ModuleId::instance(ModuleKind::Terminal, 3).unwrap()
}
fn frame() -> Frame<'static> {
    Frame::new(Size { w: 1280, h: 721 }, 1)
}
fn centred() -> FloatRect {
    FloatRect {
        cx: 0.5,
        cy: 0.5,
        w: 768,
        h: 433,
    }
}
fn row() -> Node {
    Node::split(
        Axis::Row,
        DEFAULT_EDITOR_SHARE,
        Node::Leaf(editor()),
        Node::Leaf(agent()),
    )
}
/// `[editor | agent]` with `terminal#2` floating over both, the keys in the editor.
fn with_float() -> Layout {
    let mut layout = Layout::initial(&[]).unwrap();
    layout.add_float(&t2(), centred(), false).unwrap();
    layout
}
fn ids(layout: &Layout) -> Vec<ModuleId> {
    layout.floats().iter().map(|f| f.id.clone()).collect()
}

#[test]
fn a_float_is_a_module_of_the_layout_but_not_a_leaf_of_the_tree() {
    let layout = with_float();
    assert_eq!(layout.leaves(), vec![editor(), agent()]);
    assert_eq!(layout.modules(), vec![editor(), agent(), t2()]);
    assert!(layout.contains(&t2()) && layout.is_floating(&t2()) && !layout.is_floating(&editor()));
    assert!(layout.is_visible(&t2()) && layout.is_in_view(&t2()));
    let a = arrange(&layout, &frame());
    assert_eq!(
        a.modules.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>(),
        vec![editor(), agent()]
    );
    assert_eq!(a.floats.len(), 1);
    assert_eq!(a.floats[0].id, t2());
    assert_eq!(
        a.floats[0].rect,
        Rect {
            x: 256,
            y: 145,
            w: 768,
            h: 433
        }
    );
    assert_eq!(
        a.floats[0].bar,
        Rect {
            x: 256,
            y: 145,
            w: 768,
            h: 24
        }
    );
    assert_eq!(
        a.floats[0].host,
        Rect {
            x: 256,
            y: 169,
            w: 768,
            h: 409
        }
    );
    assert_eq!(
        a.float_spot(&t2()).map(|s| s.rect),
        Some(Rect {
            x: 256,
            y: 145,
            w: 768,
            h: 433
        })
    );
    assert_eq!(layout.visible_modules(), vec![editor(), agent(), t2()]);
    assert_eq!(layout.tiled_anchor(), Some(editor()));
}

#[test]
fn the_editor_never_floats_and_a_module_floats_once() {
    let mut layout = with_float();
    assert_eq!(
        layout.add_float(&editor(), centred(), false),
        Err(LayoutError::EditorNeverFloats)
    );
    assert_eq!(
        layout.add_float(&agent(), centred(), false),
        Err(LayoutError::Duplicate(agent()))
    );
    assert_eq!(
        layout.add_float(&t2(), centred(), false),
        Err(LayoutError::Duplicate(t2()))
    );
    let bad = FloatRect {
        cx: 2.0,
        cy: 0.5,
        w: 10,
        h: 10,
    };
    assert_eq!(layout.add_float(&t3(), bad, false), Err(LayoutError::FloatRect(t3())));
}

#[test]
fn giving_a_float_the_keys_raises_it() {
    let mut layout = with_float();
    layout
        .add_float(
            &t3(),
            FloatRect {
                cx: 0.25,
                cy: 0.5,
                w: 400,
                h: 300,
            },
            false,
        )
        .unwrap();
    assert_eq!(ids(&layout), vec![t2(), t3()]);
    layout.set_focus(&t2()).unwrap();
    assert_eq!(ids(&layout), vec![t3(), t2()]);
    assert_eq!(layout.focus(), &t2());
    assert_eq!(
        layout.tiled_anchor(),
        Some(editor()),
        "a float is never the tiled anchor"
    );
}

#[test]
fn a_refused_focus_on_a_hidden_float_leaves_the_order_alone() {
    let mut layout = with_float();
    layout
        .add_float(
            &t3(),
            FloatRect {
                cx: 0.25,
                cy: 0.5,
                w: 400,
                h: 300,
            },
            true,
        )
        .unwrap();
    hide(&mut layout, &t2(), &frame()).unwrap();
    assert_eq!(ids(&layout), vec![t2(), t3()]);
    assert_eq!(layout.set_focus(&t2()), Err(LayoutError::Hidden(t2())));
    assert_eq!(ids(&layout), vec![t2(), t3()], "a refused focus raises nothing");
}

#[test]
fn a_zoom_of_a_tiled_module_takes_every_float_off_screen_and_gives_it_a_chip() {
    let mut layout = with_float();
    layout.toggle_zoom(&editor());
    let a = arrange(&layout, &frame());
    assert!(a.floats.is_empty());
    assert!(!layout.is_visible(&t2()));
    assert_eq!(tray(&layout), vec![agent(), t2()]);
    layout.toggle_zoom(&editor());
    assert_eq!(
        arrange(&layout, &frame()).floats[0].rect,
        Rect {
            x: 256,
            y: 145,
            w: 768,
            h: 433
        }
    );
}

#[test]
fn a_zoomed_float_fills_the_grid_alone() {
    let mut layout = with_float();
    layout.set_focus(&t2()).unwrap();
    layout.toggle_zoom(&t2());
    let a = arrange(&layout, &frame());
    assert_eq!(
        a.modules,
        vec![(
            t2(),
            Rect {
                x: 0,
                y: 0,
                w: 1280,
                h: 721
            }
        )]
    );
    assert!(a.floats.is_empty());
}

#[test]
fn hiding_a_float_that_holds_the_keys_gives_them_to_the_most_recent_module_in_view() {
    let mut layout = with_float();
    layout.set_focus(&agent()).unwrap();
    layout.set_focus(&t2()).unwrap();
    assert_eq!(hide(&mut layout, &t2(), &frame()), Ok(Some(agent())));
    assert!(layout.hidden().contains(&t2()) && layout.is_floating(&t2()));
    assert!(arrange(&layout, &frame()).floats.is_empty());
    assert_eq!(tray(&layout), vec![t2()]);
}

#[test]
fn a_float_counts_as_on_screen_for_the_last_module_rule() {
    let mut layout = with_float();
    hide(&mut layout, &agent(), &frame()).unwrap();
    assert_eq!(hide(&mut layout, &editor(), &frame()), Ok(Some(t2())));
    assert_eq!(layout.focus(), &t2());
    assert_eq!(hide(&mut layout, &t2(), &frame()), Err(LayoutError::LastVisible(t2())));
    assert_eq!(can_kill(&layout, &t2()), Ok(KillScope::Window));
}

#[test]
fn removing_a_floating_instance_takes_it_out_of_the_floats_and_the_mru() {
    let mut layout = with_float();
    layout.set_mark(&t2()).unwrap();
    layout.set_focus(&t2()).unwrap();
    layout.remove(&t2()).unwrap();
    assert!(!layout.contains(&t2()) && layout.floats().is_empty());
    assert_eq!(layout.mark(), None);
    assert_eq!(layout.focus(), &editor());
    assert_eq!(layout.modules(), vec![editor(), agent()]);
}

#[test]
fn a_killed_float_reopened_at_a_placement_lands_tiled_hidden_and_remembers_its_rect() {
    let mut layout = Layout::from_parts_floating(
        row(),
        vec![Float {
            id: term(),
            rect: centred(),
            scratch: true,
        }],
        BTreeSet::new(),
        editor(),
        BTreeMap::new(),
    )
    .unwrap();
    kill(&mut layout, &term(), Reopen::At(Placement::BelowEditor), &frame()).unwrap();
    assert!(!layout.is_floating(&term()));
    assert!(layout.leaves().contains(&term()) && layout.hidden().contains(&term()));
    assert_eq!(layout.float_memory().get(&term()), Some(&centred()));
}

#[test]
fn from_parts_floating_refuses_what_breaks_an_invariant() {
    let f = |id: ModuleId| Float {
        id,
        rect: centred(),
        scratch: false,
    };
    let parts = |floats: Vec<Float>, hidden: Vec<ModuleId>, focus: ModuleId, memory: Vec<(ModuleId, FloatRect)>| {
        Layout::from_parts_floating(
            row(),
            floats,
            hidden.into_iter().collect(),
            focus,
            memory.into_iter().collect(),
        )
    };
    assert_eq!(
        parts(vec![f(editor())], vec![], agent(), vec![]),
        Err(LayoutError::EditorNeverFloats)
    );
    assert_eq!(
        parts(vec![f(agent())], vec![], editor(), vec![]),
        Err(LayoutError::Duplicate(agent()))
    );
    assert_eq!(
        parts(vec![f(t2()), f(t2())], vec![], editor(), vec![]),
        Err(LayoutError::Duplicate(t2()))
    );
    assert_eq!(
        parts(vec![f(t2())], vec![t3()], editor(), vec![]),
        Err(LayoutError::NotInTree(t3()))
    );
    assert_eq!(
        parts(vec![f(t2())], vec![t2()], t2(), vec![]),
        Err(LayoutError::Hidden(t2()))
    );
    assert_eq!(
        parts(vec![], vec![], editor(), vec![(editor(), centred())]),
        Err(LayoutError::EditorNeverFloats)
    );
    assert_eq!(
        parts(vec![], vec![], editor(), vec![(t3(), centred())]),
        Err(LayoutError::NotInTree(t3()))
    );
    let bad = Float {
        id: t2(),
        rect: FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 0,
            h: 10,
        },
        scratch: false,
    };
    assert_eq!(
        parts(vec![bad], vec![], editor(), vec![]),
        Err(LayoutError::FloatRect(t2()))
    );
    let ok = parts(vec![f(t2()), f(t3())], vec![t3()], t2(), vec![(agent(), centred())]).unwrap();
    assert_eq!(ok.focus(), &t2());
    assert_eq!(ids(&ok), vec![t2(), t3()], "the order given, not raised");
    assert!(!ok.is_stowed(&t3()), "hidden, but not in the scratchpad");
    assert_eq!(ok.float_memory().get(&agent()), Some(&centred()));
}

#[test]
fn a_float_clamped_by_a_small_window_keeps_its_own_size_when_the_window_grows() {
    let mut layout = Layout::initial(&[]).unwrap();
    layout
        .add_float(
            &t2(),
            FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 900,
                h: 600,
            },
            false,
        )
        .unwrap();
    let small = arrange(&layout, &Frame::new(Size { w: 800, h: 600 }, 1));
    assert_eq!(
        small.floats[0].rect,
        Rect {
            x: 0,
            y: 0,
            w: 800,
            h: 600
        }
    );
    let big = arrange(&layout, &Frame::new(Size { w: 1920, h: 1040 }, 1));
    assert_eq!(
        big.floats[0].rect,
        Rect {
            x: 510,
            y: 220,
            w: 900,
            h: 600
        }
    );
    assert_eq!(
        layout.float_of(&t2()).unwrap().rect,
        FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 900,
            h: 600
        }
    );
}

#[test]
fn reconcile_keeps_the_floats_it_can_and_says_why_it_left_the_others_out() {
    let lua = [ModuleDecl {
        id: ModuleId::lua("notes"),
        placement: Placement::RightOfRoot,
    }];
    let f = |id: ModuleId, rect: FloatRect| Float {
        id,
        rect,
        scratch: false,
    };
    let floats = vec![
        f(term(), centred()),
        f(editor(), centred()),
        f(ModuleId::lua("gone"), centred()),
        f(agent(), centred()),
        f(term(), centred()),
        f(
            t2(),
            FloatRect {
                cx: 1.5,
                cy: 0.5,
                w: 400,
                h: 300,
            },
        ),
    ];
    let memory = BTreeMap::from([
        (editor(), centred()),
        (ModuleId::lua("gone"), centred()),
        (agent(), centred()),
    ]);
    let r = reconcile_floating(
        row(),
        floats,
        memory,
        &BTreeSet::from([term()]),
        Some(&editor()),
        &lua,
        &[t2()],
    )
    .unwrap();
    assert_eq!(ids(&r.layout), vec![term(), t2()]);
    assert_eq!(
        r.layout.float_of(&t2()).unwrap().rect,
        FloatRect {
            cx: 1.0,
            cy: 0.5,
            w: 400,
            h: 300
        }
    );
    assert!(
        !r.layout.leaves().contains(&term()),
        "a floating terminal is not placed again below the editor"
    );
    assert!(r.layout.hidden().contains(&term()));
    assert!(r.layout.leaves().contains(&ModuleId::lua("notes")));
    assert_eq!(
        r.layout.float_memory().keys().cloned().collect::<Vec<_>>(),
        vec![agent()]
    );
    for note in [
        "'editor' never floats; it stays tiled",
        "left out floating 'lua:gone': this window has no such module",
        "floating 'agent' is also tiled; it stays tiled",
        "floating 'terminal' appears twice; the first is kept",
        "floating 'terminal#2': its rectangle was out of range and is clamped",
        "float memory of 'editor' left out: the editor never floats",
        "float memory of 'lua:gone' left out: this window has no such module",
    ] {
        assert!(
            r.notes.iter().any(|n| n == note),
            "missing note {note:?} in {:?}",
            r.notes
        );
    }
}

#[test]
fn a_lua_default_with_a_floating_instance_keeps_it() {
    let r = reconcile_default_floating(
        row(),
        vec![Float {
            id: t2(),
            rect: centred(),
            scratch: true,
        }],
        &[],
    )
    .unwrap();
    assert!(r.layout.is_floating(&t2()));
    assert!(r.layout.float_of(&t2()).unwrap().scratch);
    assert!(r.layout.leaves().contains(&term()) && r.layout.hidden().contains(&term()));
}

#[test]
fn a_floating_chat_reconciles_with_no_note_and_the_editor_must_stay_tiled() {
    let tiled = Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(term()));
    let chat = vec![Float {
        id: agent(),
        rect: centred(),
        scratch: false,
    }];
    let r = reconcile_floating(
        tiled.clone(),
        chat.clone(),
        BTreeMap::new(),
        &BTreeSet::new(),
        Some(&agent()),
        &[],
        &[],
    )
    .unwrap();
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    assert!(r.layout.is_floating(&agent()) && !r.layout.leaves().contains(&agent()));
    assert_eq!(r.layout.focus(), &agent());
    let d = reconcile_default_floating(tiled, chat, &[]).unwrap();
    assert!(d.layout.is_floating(&agent()));
    let alone = Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(term()));
    assert_eq!(
        reconcile_floating(alone, Vec::new(), BTreeMap::new(), &BTreeSet::new(), None, &[], &[]).map(|r| r.notes),
        Err(super::reconcile::ReconcileError::MissingBuiltIn(agent())),
        "a chat neither tiled nor floating is still refused"
    );
}
