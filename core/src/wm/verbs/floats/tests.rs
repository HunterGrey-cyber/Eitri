use super::*;
use crate::keymap::{Action, Keymap};
use crate::layout::rules::{OpenRule, RuleSize, RuleTarget};
use crate::layout::{
    self, Axis, Child, Con, ConLayout, FloatRect, Frame, Layout, ModuleId, ModuleKeys, Node, Size, Workspaces,
};
use crate::lua::layout::LayoutRequest;
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::verbs::modules::ModuleSetup;
use crate::wm::verbs::rig::{allocate, decl, map, moves, ops, unmap, Said, WIN};
use crate::wm::*;
use std::cell::RefCell;
use std::rc::Rc;

const GRID: Size = Size { w: 1200, h: 800 };

struct Rig {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    layout: Rc<RefCell<Layout>>,
}

fn driven(layout: Layout, setup: ModuleSetup) -> Rig {
    let host = FakeHost::new();
    host.set_grid_size(GRID);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    controller.set_module_setup(WIN, setup);
    let keymap = Keymap::defaults();
    let keys = ModuleKeys::build(&[], &keymap).unwrap();
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(keys));
    let hosted: Vec<_> = layout.modules().iter().map(decl).collect();
    for d in &hosted {
        host.with_surface(WIN, d.module.clone(), d.kind.clone(), false);
    }
    let layout = Rc::new(RefCell::new(layout));
    controller.open_window(
        WIN,
        WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::Owned,
            title: "floats".into(),
        },
        WindowParts {
            layout: layout.clone(),
            workspaces: Rc::new(RefCell::new(Workspaces::single())),
            saver: None,
        },
    );
    let driver = Driver::new(controller, host.clone(), scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    host.attach(Rc::downgrade(&sink));
    scheduler.attach(Rc::downgrade(&sink));
    driver.first_apply(WIN, hosted);
    Rig { driver, host, layout }
}

impl Rig {
    fn batch(&self, decide: impl FnOnce(&mut Controller) -> Batch) -> Batch {
        let mut asked = Batch::default();
        self.driver.test_batch(|c| {
            let batch = decide(c);
            asked = batch.clone();
            batch
        });
        asked
    }

    fn run(&self, action: Action) -> Batch {
        self.batch(|c| c.run(WIN, action, Origin::Key { keycode: 7 }))
    }

    fn said(&self) -> Said {
        let chrome = self.driver.query(|c| c.chrome(WIN));
        Said {
            flash: chrome.top_bar.flash,
            toast: chrome.toast.map(|t| (t.serial, t.text)),
        }
    }

    fn refused(&self, batch: &Batch, before: Said, text: &str) {
        assert!(ops(batch).is_empty(), "{batch:?}");
        assert!(batch.ops().any(|op| matches!(op, HostOp::Chrome { .. })));
        let after = self.said();
        assert_eq!(after.flash, before.flash + 1);
        assert_eq!(after.toast.as_ref().map(|(_, text)| text.as_str()), Some(text));
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }
}

fn three(keys: ModuleId) -> Layout {
    Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(ModuleId::terminal()),
            ),
        ),
        keys,
    )
    .unwrap()
}

fn floating(keys: &ModuleId, scratch: bool) -> Layout {
    let mut l = three(keys.clone());
    layout::float_at(&mut l, &ModuleId::terminal(), None, &Frame::new(GRID, 1)).unwrap();
    l.set_scratch(&ModuleId::terminal(), scratch);
    l
}

fn focus(to: &ModuleId, card_first: bool) -> HostOp {
    HostOp::Focus {
        win: WIN,
        to: FocusTo::Module(to.clone()),
        on_grab: crate::wm::focus::arrive_tail(WIN, to, card_first),
    }
}

fn restack(ids: &[ModuleId]) -> HostOp {
    HostOp::Restack {
        win: WIN,
        moves: ids
            .iter()
            .enumerate()
            .map(|(i, id)| RestackMove {
                module: id.clone(),
                after: i.checked_sub(1).map(|i| ids[i].clone()),
            })
            .collect(),
    }
}

#[test]
fn regression_unnamed_scratch_show_ends_zoom_and_focuses_the_float() {
    let editor = ModuleId::editor();
    let terminal = ModuleId::terminal();
    let mut l = floating(&editor, true);
    l.toggle_zoom(&editor);
    let r = driven(l, ModuleSetup::default());
    let batch = r.run(Action::ScratchpadShow { module: None });
    assert!(!r.layout.borrow().is_zoomed());
    assert!(r.layout.borrow().is_visible(&terminal));
    assert_eq!(r.layout.borrow().focus(), &terminal);
    let ops = moves(&batch);
    let mapped = ops.iter().position(|op| op == &map(&terminal)).unwrap();
    let focused = ops.iter().position(|op| op == &focus(&terminal, true)).unwrap();
    assert!(mapped < focused);
}

#[test]
fn stowing_the_float_with_the_keys_gives_them_away_before_it_unmaps() {
    let terminal = ModuleId::terminal();
    for last in [ModuleId::agent(), ModuleId::editor()] {
        for action in [Action::ScratchpadMove, Action::ScratchpadShow { module: None }] {
            let tabs = Con::new(
                ConLayout::Tabbed,
                vec![
                    Child::new(Node::Leaf(ModuleId::editor()), 0.5),
                    Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                ],
            );
            let mut l = Layout::new(Node::Con(tabs), ModuleId::editor()).unwrap();
            l.set_focus(&last).unwrap();
            l.add_float(&terminal, FloatRect::centred(GRID, 0.6), true).unwrap();
            l.set_focus(&terminal).unwrap();
            l.toggle_zoom(&terminal);
            let r = driven(l, ModuleSetup::default());
            let batch = r.run(action);
            assert_eq!(
                moves(&batch),
                vec![
                    map(&last),
                    HostOp::Focus {
                        win: WIN,
                        to: FocusTo::Module(last.clone()),
                        on_grab: vec![]
                    },
                    unmap(&terminal),
                    restack(&[]),
                    allocate()
                ]
            );
            assert!(r.layout.borrow().is_stowed(&terminal));
            assert_eq!(r.layout.borrow().focus(), &last);
        }
    }
}

#[test]
fn a_shown_scratch_float_is_mapped_and_raised_before_it_gets_the_keys() {
    let terminal = ModuleId::terminal();
    for state in ["stowed", "raised", "plain"] {
        let mut l = floating(&ModuleId::agent(), state != "plain");
        if state == "stowed" {
            layout::scratchpad_move(&mut l, &terminal, &Frame::new(GRID, 1)).unwrap();
        }
        let other = ModuleId::instance(layout::ModuleKind::Terminal, 3).unwrap();
        l.add_float(&other, FloatRect::centred(GRID, 0.3), false).unwrap();
        let r = driven(l, ModuleSetup::default());
        let batch = r.run(Action::ScratchpadShow {
            module: (state == "plain").then(|| terminal.clone()),
        });
        let mut expected = vec![];
        if state == "stowed" {
            expected.push(map(&terminal));
        }
        expected.extend([restack(&[other, terminal.clone()]), allocate(), focus(&terminal, true)]);
        assert_eq!(moves(&batch), expected);
        assert_eq!(r.layout.borrow().focus(), &terminal);
    }
}

#[test]
fn float_toggle_floats_a_tile_at_its_rule_size_the_first_time_and_where_it_last_was_after() {
    let terminal = ModuleId::terminal();
    let mut setup = ModuleSetup::default();
    setup
        .rules
        .add(
            RuleTarget::Kind(layout::ModuleKind::Terminal),
            OpenRule {
                float: true,
                size: Some(RuleSize::Float { w: 0.4, h: 0.3 }),
                ..Default::default()
            },
        )
        .unwrap();
    let r = driven(three(terminal.clone()), setup);
    assert_eq!(
        moves(&r.run(Action::FloatToggle)),
        vec![restack(&[terminal.clone()]), allocate()]
    );
    assert_eq!(
        r.layout.borrow().float_of(&terminal).unwrap().rect,
        FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 480,
            h: 240
        }
    );
    r.run(Action::Move(layout::Direction::Right));
    let remembered = r.layout.borrow().float_of(&terminal).unwrap().rect;
    assert_eq!(moves(&r.run(Action::FloatToggle)), vec![restack(&[]), allocate()]);
    r.run(Action::FloatToggle);
    assert_eq!(r.layout.borrow().float_of(&terminal).unwrap().rect, remembered);
    assert_eq!(r.layout.borrow().focus(), &terminal);
    for change in ["Floated", "Sank"] {
        assert!(r.notes().contains(&format!("[modules] terminal: {change}")));
    }
}

#[test]
fn focus_mode_toggle_lands_on_the_other_layer_or_refuses_with_todays_text() {
    let terminal = ModuleId::terminal();
    let r = driven(floating(&ModuleId::agent(), false), ModuleSetup::default());
    assert_eq!(moves(&r.run(Action::FocusModeToggle)), vec![focus(&terminal, false)]);
    assert!(r
        .notes()
        .contains(&"[modules] focus.mode-toggle -> terminal (grab_focus=true)".into()));
    r.run(Action::Zoom);
    let batch = r.run(Action::FocusModeToggle);
    assert_eq!(
        moves(&batch),
        vec![
            map(&ModuleId::editor()),
            map(&ModuleId::agent()),
            allocate(),
            focus(&ModuleId::agent(), false)
        ]
    );
    assert!(!r.layout.borrow().is_zoomed());
    let r = driven(three(ModuleId::agent()), ModuleSetup::default());
    let before = r.said();
    let batch = r.run(Action::FocusModeToggle);
    r.refused(&batch, before, "focus.mode-toggle: nothing in view on the other layer");
}

#[test]
fn a_float_moves_by_float_move_cells_editor_cells_and_logs_at_an_edge() {
    for cell in [None, Some((9.5, 21.0))] {
        for dir in [
            layout::Direction::Left,
            layout::Direction::Right,
            layout::Direction::Up,
            layout::Direction::Down,
        ] {
            let terminal = ModuleId::terminal();
            let r = driven(floating(&terminal, false), ModuleSetup::default());
            r.host.set_editor_cell(cell);
            let before = r.layout.borrow().float_of(&terminal).unwrap().rect;
            let batch = r.run(Action::Move(dir));
            assert_eq!(moves(&batch), vec![allocate()]);
            let after = r.layout.borrow().float_of(&terminal).unwrap().rect;
            let delta = match dir {
                layout::Direction::Left => (before.cx - after.cx) * GRID.w as f32,
                layout::Direction::Right => (after.cx - before.cx) * GRID.w as f32,
                layout::Direction::Up => (before.cy - after.cy) * GRID.h as f32,
                layout::Direction::Down => (after.cy - before.cy) * GRID.h as f32,
            };
            assert!((delta - resize_px(dir, cell.unwrap_or(FALLBACK_CELL), 5) as f32).abs() < 0.001);
            for _ in 0..30 {
                r.run(Action::Move(dir));
            }
            let said = r.said();
            let batch = r.run(Action::Move(dir));
            assert!(batch.is_empty(), "an edge only logs: {batch:?}");
            assert_eq!(r.said(), said);
            assert!(r
                .notes()
                .contains(&format!("[modules] terminal: the float is against the {dir:?} edge")));
        }
    }
    assert_eq!(FLOAT_MOVE_CELLS, 5);
}

#[test]
fn scratchpad_show_of_an_instance_not_yet_made_opens_it_into_the_scratchpad() {
    for kind in [layout::ModuleKind::Terminal, layout::ModuleKind::Agent] {
        let id = ModuleId::instance(kind, 2).unwrap();
        let r = driven(three(ModuleId::editor()), ModuleSetup::default());
        let mark = r.host.log().len();
        let batch = r.run(Action::ScratchpadShow {
            module: Some(id.clone()),
        });
        assert!(matches!(ops(&batch).as_slice(), [HostOp::CreateSurface { module, .. }] if module == &id));
        let mut applied = Batch::default();
        for logged in &r.host.log()[mark..] {
            if let Logged::Op(op, Applied::Done) = logged {
                applied.push_layout(op.clone());
            }
        }
        assert_eq!(
            moves(&applied),
            vec![
                map(&id),
                restack(&[id.clone()]),
                allocate(),
                HostOp::Focus {
                    win: WIN,
                    to: FocusTo::Module(id.clone()),
                    on_grab: vec![]
                },
            ]
        );
        let l = r.layout.borrow();
        assert!(l.float_of(&id).unwrap().scratch);
        assert!(l.is_shown(&id));
        assert_eq!(l.focus(), &id);
    }
}

#[test]
fn scratchpad_show_with_nothing_stowed_refuses_with_todays_text() {
    let r = driven(three(ModuleId::editor()), ModuleSetup::default());
    let before = r.said();
    let batch = r.run(Action::ScratchpadShow { module: None });
    let text = format!(
        "the scratchpad is empty ({} sends a module there)",
        super::super::key_text(&Keymap::defaults(), &Action::ScratchpadMove)
    );
    assert_eq!(scratch_empty_text(&Keymap::defaults()), text);
    r.refused(&batch, before, &text);
    assert!(r.notes().contains(&format!("[modules] refused: {text}")));
}

#[test]
fn a_lua_float_places_the_float_and_logs_its_refusal() {
    let terminal = ModuleId::terminal();
    let r = driven(three(ModuleId::editor()), ModuleSetup::default());
    let rect = FloatRect {
        cx: 0.7,
        cy: 0.3,
        w: 400,
        h: 200,
    };
    let request = LayoutRequest::Float(terminal.clone(), Some(rect));
    assert!(crate::wm::lua::decided(&request));
    let batch = r.batch(|c| c.lua_request(WIN, request.clone()));
    assert_eq!(moves(&batch), vec![restack(&[terminal.clone()]), allocate()]);
    assert_eq!(r.layout.borrow().float_of(&terminal).unwrap().rect, rect);
    assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
    assert!(r.batch(|c| c.lua_request(WIN, request)).is_empty());
    let before = r.said();
    let request = LayoutRequest::Float(ModuleId::editor(), None);
    assert!(r.batch(|c| c.lua_request(WIN, request.clone())).is_empty());
    assert_eq!(r.said(), before);
    assert!(r.notes().contains(&format!(
        "[lua] layout: {request:?} refused: {}",
        layout::LayoutError::EditorNeverFloats
    )));
}
