use super::*;
use crate::keymap::{prefix::Press, Action, KeySpec, Keymap};
use crate::layout::{self, Arrangement, Axis, Frame, Layout, ModuleKeys, Node, Rect, Workspaces};
use crate::lua::layout::LayoutRequest;
use crate::plugin::test_support::Scratch;
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::grid::UNALLOCATED;
use crate::wm::persist::Saver;
use crate::wm::verbs::rig::{decl, WIN};
use crate::wm::*;
use std::cell::RefCell;
use std::rc::Rc;

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn terminal() -> ModuleId {
    ModuleId::terminal()
}
fn lower() -> ModuleId {
    ModuleId::lua("lower")
}
fn upper() -> ModuleId {
    ModuleId::lua("upper")
}

/// `editor | (agent / terminal)`, the keys on the editor.
fn three() -> Layout {
    Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(editor()),
            Node::split(Axis::Column, 0.5, Node::Leaf(ModuleId::agent()), Node::Leaf(terminal())),
        ),
        editor(),
    )
    .unwrap()
}

/// `editor | terminal` with two floats over them, `lower` under `upper`, the keys on the editor.
fn two_floats() -> Layout {
    let mut l = Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(editor()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(terminal()),
                Node::split(Axis::Row, 0.5, Node::Leaf(lower()), Node::Leaf(upper())),
            ),
        ),
        editor(),
    )
    .unwrap();
    let frame = Frame::new(UNALLOCATED, 1);
    layout::float_at(&mut l, &lower(), None, &frame).unwrap();
    layout::float_at(&mut l, &upper(), None, &frame).unwrap();
    l
}

struct Rig {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    layout: Rc<RefCell<Layout>>,
    workspaces: Rc<RefCell<Workspaces>>,
    _scratch: Scratch,
}

/// `layout` as the main window behind a driver over the fake host, saved to a scratch state directory, after its
/// first apply and its first allocation.
fn driven(layout: Layout, name: &str) -> Rig {
    let scratch = Scratch::new(name);
    let host = FakeHost::new();
    host.set_grid_size(UNALLOCATED);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let keymap = Keymap::defaults();
    let keys = ModuleKeys::build(&[], &keymap).unwrap();
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(keys));
    let hosted: Vec<_> = layout.modules().iter().map(decl).collect();
    for d in &hosted {
        host.with_surface(WIN, d.module.clone(), d.kind.clone(), false);
    }
    let workspaces = Workspaces::single();
    let saver = Saver::new(
        Some(scratch.0.clone()),
        &scratch.0,
        Vec::new(),
        None,
        &layout,
        &workspaces,
    );
    let layout = Rc::new(RefCell::new(layout));
    let workspaces = Rc::new(RefCell::new(workspaces));
    controller.open_window(
        WIN,
        WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::Owned,
            title: "drags".into(),
        },
        WindowParts {
            layout: layout.clone(),
            workspaces: workspaces.clone(),
            saver: Some(saver),
        },
    );
    let driver = Driver::new(controller, host.clone(), scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    host.attach(Rc::downgrade(&sink));
    scheduler.attach(Rc::downgrade(&sink));
    driver.first_apply(WIN, hosted);
    let r = Rig {
        driver,
        host,
        layout,
        workspaces,
        _scratch: scratch,
    };
    r.allocate();
    r
}

impl Rig {
    fn allocate(&self) -> Rc<Arrangement> {
        Allocator::allocate(&*self.driver, WIN, UNALLOCATED)
    }

    fn mark(&self) -> usize {
        self.host.log().len()
    }

    /// Every op the host was asked for since `mark`, the chrome's announcements included.
    fn ops_since(&self, mark: usize) -> Vec<HostOp> {
        self.host.log()[mark..]
            .iter()
            .filter_map(|logged| match logged {
                Logged::Op(op, _) => Some(op.clone()),
                _ => None,
            })
            .collect()
    }

    /// Reads the window's record; the batch it answers is empty.
    fn window<T: Default + 'static>(&self, read: impl FnOnce(&mut Window) -> T) -> T {
        let mut out = T::default();
        self.driver.test_batch(|c| {
            if let Some(w) = c.window_mut(WIN) {
                out = read(w);
            }
            Batch::default()
        });
        out
    }

    fn dragging(&self) -> Option<Drag> {
        self.window(|w| w.drag.clone())
    }

    fn save_armed(&self) -> bool {
        self.window(|w| w.save_timer.is_some())
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }

    fn run(&self, action: Action) {
        self.driver.run(WIN, action, Origin::Key { keycode: 7 });
    }

    fn key(&self, press: Press) -> Verdict {
        self.driver.key(
            WIN,
            &KeyStage::Window,
            KeyPress {
                keycode: 9,
                key: match press {
                    Press::Key(spec) => Some(spec.key),
                    _ => None,
                },
                press,
                mods: Mods::default(),
                super_or_hyper: false,
                super_or_meta: false,
            },
        )
    }
}

fn spot(arranged: &Arrangement, module: &ModuleId) -> Rect {
    arranged.float_spot(module).expect("the float is drawn").rect
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

fn focus(to: &ModuleId) -> HostOp {
    HostOp::Focus {
        win: WIN,
        to: FocusTo::Module(to.clone()),
        on_grab: crate::wm::focus::arrive_tail(WIN, to, true),
    }
}

fn allocate_op() -> HostOp {
    HostOp::Allocate { win: WIN }
}

fn no_bug(r: &Rig) {
    let bugs: Vec<String> = r.notes().into_iter().filter(|line| line.contains("BUG")).collect();
    assert!(bugs.is_empty(), "{bugs:?}");
}

#[test]
fn a_bar_drag_stores_the_rect_and_raises_on_press() {
    for grip in [false, true] {
        let r = driven(two_floats(), if grip { "grip-drag" } else { "bar-drag" });
        let on = if grip {
            PointerTarget::FloatGrip(lower())
        } else {
            PointerTarget::FloatBar(lower())
        };
        assert!(!r.save_armed(), "the launch is not a change");
        let mark = r.mark();
        r.host.press_on(WIN, on.clone());
        let ops = r.ops_since(mark);
        assert_eq!(
            ops.get(..2),
            Some(&[restack(&[upper(), lower()]), focus(&lower())][..]),
            "the pressed float goes on top before it gets the keys: {ops:?}"
        );
        assert_eq!(r.layout.borrow().focus(), &lower());
        assert!(!r.save_armed(), "a raise is no change for the state file");
        assert_eq!(r.dragging(), Some(Drag::Float { module: lower(), grip }));

        let grabbed = spot(&r.allocate(), &lower());
        let mark = r.mark();
        r.host.drag_by(WIN, on.clone(), 30.0, 10.0);
        assert_eq!(r.ops_since(mark), vec![allocate_op()], "a motion only allocates");
        let moved = |from: Rect, dx: i32, dy: i32| {
            if grip {
                Rect {
                    w: from.w + dx,
                    h: from.h + dy,
                    ..from
                }
            } else {
                Rect {
                    x: from.x + dx,
                    y: from.y + dy,
                    ..from
                }
            }
        };
        let first = spot(&r.allocate(), &lower());
        assert_eq!(first, moved(grabbed, 30, 10));
        r.host.drag_by(WIN, on.clone(), 60.0, 20.0);
        let second = spot(&r.allocate(), &lower());
        assert_eq!(
            second,
            moved(first, 60, 20),
            "each offset is from where the float is drawn now, and the last one wins"
        );
        assert!(r.save_armed(), "a drag is a change for the state file");
        r.host.release_on(WIN, on.clone());
        assert_eq!(r.dragging(), None);
        let mark = r.mark();
        r.host.drag_by(WIN, on, 5.0, 5.0);
        assert!(r.ops_since(mark).is_empty(), "a motion after the release moves nothing");
        no_bug(&r);
    }
}

#[test]
fn a_divider_drag_keeps_its_divider_between_motion_events() {
    let r = driven(three(), "divider-drag");
    let begin = r.allocate().dividers[0].clone();
    assert_eq!(
        begin.con,
        r.layout.borrow().root().id,
        "the premise: handle 0 draws the root's split"
    );
    r.host.press_on(WIN, PointerTarget::Divider(0));
    assert_eq!(
        r.dragging(),
        Some(Drag::Divider {
            handle: 0,
            grabbed: begin.clone()
        })
    );
    let pointer_start = begin.rect.x;
    let mut now = begin.clone();
    for step in 1..=2 {
        let pointer = pointer_start + 20 * step;
        let mark = r.mark();
        r.host
            .drag_by(WIN, PointerTarget::Divider(0), f64::from(pointer - now.rect.x), 0.0);
        assert_eq!(r.ops_since(mark), vec![allocate_op()]);
        now = r.allocate().dividers[0].clone();
        assert_eq!(now.rect.x, pointer, "the divider stays under the pointer");
    }
    assert!(r.save_armed());

    // The split around the editor turns from a row into a column: handle 0 draws another split now.
    r.run(Action::ToggleSplit);
    let other = r.allocate().dividers[0].clone();
    assert_ne!(other.axis, begin.axis, "the premise: handle 0 draws another split");
    let before = r.layout.borrow().clone();
    let mark = r.mark();
    r.host.drag_by(WIN, PointerTarget::Divider(0), 0.0, 40.0);
    assert!(r.ops_since(mark).is_empty(), "a divider not grabbed is not moved");
    assert_eq!(*r.layout.borrow(), before);
    no_bug(&r);
}

#[test]
fn a_divider_turned_before_its_allocation_is_not_moved() {
    let r = driven(three(), "divider-turned");
    let begin = r.allocate().dividers[0].clone();
    assert_eq!(begin.axis, Axis::Row, "the premise: handle 0 draws the root's row");
    r.host.press_on(WIN, PointerTarget::Divider(0));

    // The root's row turns into a column, and the allocation that verb queued has not run: the window's last
    // arrangement still draws handle 0 as the row it was.
    r.run(Action::ToggleSplit);
    let root = r.layout.borrow().root().id;
    assert_eq!(
        r.layout.borrow().root().find(root).and_then(|con| con.layout.axis()),
        Some(Axis::Column),
        "the premise: the root is a column now"
    );
    let before = r.layout.borrow().clone();
    let mark = r.mark();
    r.host.drag_by(WIN, PointerTarget::Divider(0), 40.0, 0.0);
    assert!(
        r.ops_since(mark).is_empty(),
        "a split turned since it was drawn is not moved by the old divider"
    );
    assert_eq!(*r.layout.borrow(), before);
    no_bug(&r);
}

#[test]
fn a_press_on_the_top_float_raises_nothing_and_still_gives_the_keys() {
    let r = driven(two_floats(), "top-float");
    let mark = r.mark();
    r.host.press_on(WIN, PointerTarget::FloatBar(upper()));
    let ops = r.ops_since(mark);
    assert_eq!(ops.first(), Some(&focus(&upper())), "{ops:?}");
    assert!(
        !ops.iter().any(|op| matches!(op, HostOp::Restack { .. })),
        "the float on top is not restacked: {ops:?}"
    );
    assert_eq!(r.layout.borrow().focus(), &upper());
    assert_eq!(r.layout.borrow().floats().last().map(|f| f.id.clone()), Some(upper()));
}

#[test]
fn a_press_elsewhere_or_with_another_button_ends_the_drag() {
    let r = driven(two_floats(), "other-press");
    r.host.press_on(WIN, PointerTarget::FloatBar(lower()));
    assert!(r.dragging().is_some());
    r.host.post(HostEvent::Pointer {
        win: WIN,
        on: PointerTarget::FloatBar(upper()),
        ev: PointerEv::Press {
            x: 0.0,
            y: 0.0,
            button: 3,
        },
    });
    assert_eq!(r.dragging(), None, "a secondary press on a bar drags nothing");
    assert_eq!(
        r.layout.borrow().floats().last().map(|f| f.id.clone()),
        Some(lower()),
        "nor raises"
    );
    r.host.press_on(WIN, PointerTarget::Divider(0));
    assert!(matches!(r.dragging(), Some(Drag::Divider { handle: 0, .. })));
    r.host.press_on(WIN, PointerTarget::Window);
    assert_eq!(r.dragging(), None);
    r.host.press_on(WIN, PointerTarget::Divider(40));
    assert_eq!(r.dragging(), None, "a handle that draws nothing begins no drag");
}

#[test]
fn a_cancel_ends_a_divider_bar_or_grip_drag() {
    for on in [
        PointerTarget::Divider(0),
        PointerTarget::FloatBar(lower()),
        PointerTarget::FloatGrip(lower()),
    ] {
        let r = driven(two_floats(), "drag-cancel");
        r.host.press_on(WIN, on.clone());
        assert!(r.dragging().is_some());
        r.host.post(HostEvent::Pointer {
            win: WIN,
            on: on.clone(),
            ev: PointerEv::Cancel,
        });
        assert_eq!(r.dragging(), None);
        let before = r.layout.borrow().clone();
        let mark = r.mark();
        r.host.drag_by(WIN, on, 30.0, 20.0);
        assert!(r.ops_since(mark).is_empty());
        assert_eq!(*r.layout.borrow(), before);
        no_bug(&r);
    }
}

#[test]
fn losing_activation_ends_a_divider_bar_or_grip_drag() {
    for on in [
        PointerTarget::Divider(0),
        PointerTarget::FloatBar(lower()),
        PointerTarget::FloatGrip(lower()),
    ] {
        let r = driven(two_floats(), "drag-inactive");
        r.host.press_on(WIN, on.clone());
        assert!(r.dragging().is_some());
        r.host.post(HostEvent::Active {
            win: WIN,
            active: false,
        });
        assert_eq!(r.dragging(), None);
        let before = r.layout.borrow().clone();
        let mark = r.mark();
        r.host.drag_by(WIN, on, 30.0, 20.0);
        assert!(r.ops_since(mark).is_empty());
        assert_eq!(*r.layout.borrow(), before);
        no_bug(&r);
    }
}

#[test]
fn escape_stops_a_divider_bar_or_grip_drag_without_undoing_its_motion() {
    for on in [
        PointerTarget::Divider(0),
        PointerTarget::FloatBar(lower()),
        PointerTarget::FloatGrip(lower()),
    ] {
        let r = driven(two_floats(), "drag-escape");
        assert_eq!(r.key(Press::Escape), Verdict::PassThrough);
        r.host.press_on(WIN, on.clone());
        r.allocate();
        let before = r.layout.borrow().clone();
        r.host.drag_by(WIN, on.clone(), 30.0, 20.0);
        r.allocate();
        let moved = r.layout.borrow().clone();
        assert_ne!(moved, before, "the premise: the drag moved geometry");
        assert!(r.dragging().is_some());
        assert_eq!(r.key(Press::Escape), Verdict::Consumed);
        assert_eq!(r.dragging(), None);
        assert_eq!(*r.layout.borrow(), moved, "Esc stops without undoing");
        let mark = r.mark();
        r.host.drag_by(WIN, on, 30.0, 20.0);
        assert!(r.ops_since(mark).is_empty());
        assert_eq!(*r.layout.borrow(), moved);
        assert_eq!(r.key(Press::Escape), Verdict::PassThrough);
        no_bug(&r);
    }
}

#[test]
fn escape_ending_a_drag_does_not_reach_the_prefix() {
    let r = driven(two_floats(), "drag-escape-prefix");
    r.host.press_on(WIN, PointerTarget::FloatBar(lower()));
    assert_eq!(r.key(Press::Key(KeySpec::parse("C-b").unwrap())), Verdict::Consumed);
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: 9 });
    let waiting = r.driver.query(|c| c.prefix_shown(WIN)).unwrap().0;
    assert_eq!(waiting, crate::keymap::prefix::Waiting::Command);
    assert_eq!(r.key(Press::Escape), Verdict::Consumed);
    assert_eq!(r.dragging(), None);
    assert_eq!(r.driver.query(|c| c.prefix_shown(WIN)).unwrap().0, waiting);
    let before = r.layout.borrow().clone();
    let mark = r.mark();
    r.host.drag_by(WIN, PointerTarget::FloatBar(lower()), 30.0, 20.0);
    assert!(r.ops_since(mark).is_empty());
    assert_eq!(*r.layout.borrow(), before);
    no_bug(&r);
}

#[test]
fn a_divider_removed_during_its_drag_ends_it() {
    let module = ModuleId::instance(layout::ModuleKind::Terminal, 2).unwrap();
    let l = Layout::new(
        Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(module.clone())),
        module.clone(),
    )
    .unwrap();
    let r = driven(l, "divider-removed");
    r.host.press_on(WIN, PointerTarget::Divider(0));
    assert!(r.dragging().is_some());
    r.run(Action::ModuleKill);
    assert_eq!(r.key(Press::Key(KeySpec::parse("y").unwrap())), Verdict::Consumed);
    assert!(!r.layout.borrow().contains(&module));
    assert!(r.allocate().dividers.is_empty());
    let before = r.layout.borrow().clone();
    let mark = r.mark();
    r.host.drag_by(WIN, PointerTarget::Divider(0), 30.0, 20.0);
    assert_eq!(r.dragging(), None);
    assert!(r.ops_since(mark).is_empty());
    assert_eq!(*r.layout.borrow(), before);
    no_bug(&r);
}

#[test]
fn a_float_removed_during_its_drag_ends_it() {
    for reallocate in [false, true] {
        let module = ModuleId::instance(layout::ModuleKind::Terminal, 2).unwrap();
        let mut l = Layout::new(
            Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(module.clone())),
            editor(),
        )
        .unwrap();
        layout::float_at(&mut l, &module, None, &Frame::new(UNALLOCATED, 1)).unwrap();
        let r = driven(l, "float-removed");
        let on = PointerTarget::FloatBar(module.clone());
        r.host.press_on(WIN, on.clone());
        assert!(r.dragging().is_some());
        r.run(Action::ModuleKill);
        assert_eq!(r.key(Press::Key(KeySpec::parse("y").unwrap())), Verdict::Consumed);
        assert!(!r.layout.borrow().contains(&module), "removed, rather than hidden");
        if reallocate {
            assert!(r.allocate().float_spot(&module).is_none());
        }
        let before = r.layout.borrow().clone();
        let mark = r.mark();
        r.host.drag_by(WIN, on, 30.0, 20.0);
        assert_eq!(r.dragging(), None);
        assert!(r.ops_since(mark).is_empty());
        assert_eq!(*r.layout.borrow(), before);
        no_bug(&r);
    }
}

/// A way for a float to leave the screen in the middle of its drag, by name.
type Leave<'a> = (&'a str, &'a dyn Fn(&Rig));

#[test]
fn a_drag_whose_float_left_the_screen_moves_nothing() {
    let switch = |r: &Rig, to: &str| crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Switch(to.into()));
    let leaves: [Leave<'_>; 3] = [
        ("lua-hide", &|r| {
            crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Hide(lower()))
        }),
        ("stow", &|r| r.run(Action::ScratchpadMove)),
        ("switch", &|r| switch(r, "other")),
    ];
    for (name, leave) in leaves {
        let r = driven(two_floats(), name);
        if name == "switch" {
            // A new workspace starts as a copy of the current one: the float is hidden there, so switching to it
            // takes the float off screen.
            let first = r.workspaces.borrow().order()[0].clone();
            switch(&r, "other");
            crate::wm::fake::lua(&r.driver, WIN, LayoutRequest::Hide(lower()));
            switch(&r, &first);
            assert!(
                r.layout.borrow().is_visible(&lower()),
                "the premise: back where the float is drawn"
            );
        }
        let on = PointerTarget::FloatBar(lower());
        r.host.press_on(WIN, on.clone());
        r.allocate();
        r.host.drag_by(WIN, on.clone(), 10.0, 10.0);
        leave(&r);
        assert!(
            !r.layout.borrow().is_visible(&lower()),
            "{name}: the premise: the float is off screen"
        );
        // Before the grid allocates again the last arrangement still draws the float where it was.
        let before = r.layout.borrow().clone();
        let mark = r.mark();
        r.host.drag_by(WIN, on.clone(), 20.0, 20.0);
        assert!(r.ops_since(mark).is_empty(), "{name}: {:?}", r.ops_since(mark));
        assert_eq!(
            *r.layout.borrow(),
            before,
            "{name}: nothing is written for a float not drawn"
        );
        assert_eq!(r.dragging(), None, "{name}: the drag ended");
        r.allocate();
        let mark = r.mark();
        r.host.drag_by(WIN, on, 30.0, 30.0);
        assert!(r.ops_since(mark).is_empty(), "{name}");
        assert_eq!(*r.layout.borrow(), before, "{name}");
        no_bug(&r);
    }
}

#[test]
fn no_drag_op_names_the_editor() {
    let r = driven(three(), "editor-divider");
    let mark = r.mark();
    r.host.press_on(WIN, PointerTarget::Divider(0));
    for dx in [15.0, -30.0, 45.0] {
        r.host.drag_by(WIN, PointerTarget::Divider(0), dx, 0.0);
        r.allocate();
    }
    r.host.release_on(WIN, PointerTarget::Divider(0));
    let ops = r.ops_since(mark);
    assert_eq!(
        ops,
        vec![allocate_op(); 3],
        "a divider drag beside the editor only allocates"
    );

    let r = driven(two_floats(), "editor-raise");
    let mark = r.mark();
    r.host.press_on(WIN, PointerTarget::FloatGrip(lower()));
    r.host.drag_by(WIN, PointerTarget::FloatGrip(lower()), 25.0, 25.0);
    let ops = r.ops_since(mark);
    assert!(
        ops.iter().any(|op| matches!(op, HostOp::Restack { .. })),
        "a press on the lower float raises it: {ops:?}"
    );
    for op in ops {
        if let HostOp::Restack { moves, .. } = &op {
            assert!(
                moves.iter().all(|m| m.module != editor() && m.after != Some(editor())),
                "a restack names floats only: {op:?}"
            );
        }
        assert!(
            !matches!(&op, HostOp::Map { module, .. } | HostOp::Unmap { module, .. } if *module == editor()),
            "{op:?}"
        );
    }
}
