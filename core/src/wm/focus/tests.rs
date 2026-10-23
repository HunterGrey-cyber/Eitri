use super::*;
use crate::keymap::keys_return::Opened;
use crate::keymap::Action;
use crate::layout::{Axis, Child, Con, ConLayout, Float, FloatRect, Frame, Layout, Node, Size, Zoom};
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::verbs::rig::{self, decl, moves};
use crate::wm::*;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

const WIN: WinId = WinId::MAIN;
const GRID: Size = Size { w: 1200, h: 800 };

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn agent() -> ModuleId {
    ModuleId::agent()
}
fn terminal() -> ModuleId {
    ModuleId::terminal()
}
fn lua(name: &str) -> ModuleId {
    ModuleId::lua(name)
}

fn leaf(id: ModuleId) -> Node {
    Node::Leaf(id)
}

/// `editor | (agent / terminal)`, the keys on `keys`.
fn three(keys: ModuleId) -> Layout {
    let root = Node::split(
        Axis::Row,
        0.5,
        leaf(editor()),
        Node::split(Axis::Column, 0.5, leaf(agent()), leaf(terminal())),
    );
    Layout::new(root, keys).unwrap()
}

/// `editor | agent`, the keys on the editor.
fn two() -> Layout {
    Layout::new(Node::split(Axis::Row, 0.5, leaf(editor()), leaf(agent())), editor()).unwrap()
}

/// `editor` beside a tabbed container of `first` (drawn) and `second`, the keys on the editor.
fn editor_beside_tabs(first: ModuleId, second: ModuleId) -> Layout {
    let tabs = Con::new(
        ConLayout::Tabbed,
        vec![Child::new(leaf(first.clone()), 0.5), Child::new(leaf(second), 0.5)],
    );
    let root = Con::new(
        ConLayout::SplitH,
        vec![Child::new(leaf(editor()), 0.5), Child::new(Node::Con(tabs), 0.5)],
    );
    let mut layout = Layout::new(Node::Con(root), first).unwrap();
    layout.set_focus(&editor()).unwrap();
    layout
}

fn hide(layout: &mut Layout, id: &ModuleId) {
    crate::layout::hide(layout, id, &Frame::new(GRID, 1)).unwrap();
}

/// A controller holding `layout` as the main window behind a driver, over the fake host with a surface for every
/// module, every module the layout shows mapped by the first apply.
struct Rig {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    scheduler: Rc<FakeScheduler>,
    layout: Rc<RefCell<Layout>>,
}

fn driven(layout: Layout) -> Rig {
    let host = FakeHost::new();
    host.set_grid_size(GRID);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
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
            title: "project".into(),
        },
        WindowParts {
            layout: layout.clone(),
            workspaces: Rc::new(RefCell::new(crate::layout::Workspaces::single())),
            saver: None,
        },
    );
    let driver = Driver::new(controller, host.clone(), scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    host.attach(Rc::downgrade(&sink));
    scheduler.attach(Rc::downgrade(&sink));
    driver.first_apply(WIN, hosted);
    Rig {
        driver,
        host,
        scheduler,
        layout,
    }
}

impl Rig {
    /// Where the host's log stands now, to read what follows.
    fn mark(&self) -> usize {
        self.host.log().len()
    }

    /// The ops the host was asked to apply since `mark`, with how each went.
    fn applied_since(&self, mark: usize) -> Vec<(HostOp, Applied)> {
        self.host.log()[mark..]
            .iter()
            .filter_map(|logged| match logged {
                Logged::Op(op, applied) => Some((op.clone(), *applied)),
                _ => None,
            })
            .collect()
    }

    fn ops_since(&self, mark: usize) -> Vec<HostOp> {
        self.applied_since(mark).into_iter().map(|(op, _)| op).collect()
    }

    fn post(&self, event: HostEvent) {
        self.host.post(event);
    }

    /// The host reports `module` holds the keys, outside any batch.
    fn native(&self, module: &ModuleId) {
        self.post(native(module, None));
    }

    /// Computes one batch from the window's record and applies it; `f`'s answer comes back.
    fn batch<R: 'static>(&self, f: impl FnOnce(&mut Window, &dyn HostReads, &mut Batch) -> R) -> (R, Batch) {
        let out = Rc::new(Cell::new(None));
        let kept = Rc::new(RefCell::new(Batch::default()));
        {
            let out = out.clone();
            let kept = kept.clone();
            self.driver.test_batch(move |c| {
                let reads = c.reads();
                let w = c.window_mut(WIN).expect("the rig's window");
                let mut batch = Batch::default();
                out.set(Some(f(w, &*reads, &mut batch)));
                *kept.borrow_mut() = batch.clone();
                batch
            });
        }
        let batch = kept.borrow().clone();
        (out.take().expect("the batch ran"), batch)
    }

    /// Records a `prefix ?` opening that found the keys on `from`, as the `prefix ?` arm does once the sheet is
    /// shown: through the driver's own entry.
    fn open_sheet(&self, opened: Opened) -> u64 {
        self.driver.keymap_opened(WIN, crate::tabs::ViewId::BASE, opened)
    }

    fn close_sheet(&self, opening: u64, forced: bool) {
        self.post(HostEvent::Chat(ChatEvent::KeymapClosed {
            win: WIN,
            view: crate::tabs::ViewId::BASE,
            opening,
            forced,
        }));
    }

    fn previous(&self) -> Option<ModuleId> {
        self.driver
            .query(|c| c.focus_state(WIN).and_then(|focus| focus.history.previous().cloned()))
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }
}

fn native(module: &ModuleId, op_index: Option<u32>) -> HostEvent {
    HostEvent::NativeFocus {
        win: WIN,
        owner: Some(FocusOwner::Module(module.clone())),
        chain: FocusChain(vec![module.clone()]),
        active: true,
        op_index,
    }
}

fn focus_op(module: &ModuleId, on_grab: Vec<HostOp>) -> HostOp {
    HostOp::Focus {
        win: WIN,
        to: FocusTo::Module(module.clone()),
        on_grab,
    }
}

/// Arriving in the base chat where its reader left it, as every keyboard move between modules does.
fn arrive() -> HostOp {
    HostOp::Chat(ChatOp::Arrive {
        win: WIN,
        view: crate::tabs::ViewId::BASE,
        card_first: false,
    })
}

/// Arriving in the base chat on its oldest waiting card, as a route that picks the chat by name does.
fn arrive_at_card() -> HostOp {
    HostOp::Chat(ChatOp::Arrive {
        win: WIN,
        view: crate::tabs::ViewId::BASE,
        card_first: true,
    })
}

/// Whether `op` tells `module` anything about the keys.
fn signals(op: &HostOp, module: &ModuleId) -> bool {
    match op {
        HostOp::Editor(EditorCall::SetFocused { .. }) | HostOp::Chat(ChatOp::EditorHasKeys { .. }) => {
            *module == editor()
        }
        HostOp::Chat(ChatOp::PaneFocused { view, .. }) => view.module() == *module,
        HostOp::Terminal(TermOp::SetFocused { module: m, .. })
        | HostOp::Plugin(PluginOp::HasKeys { module: m, .. }) => m == module,
        _ => false,
    }
}

fn answers(map: &[(ModuleId, bool)]) -> BTreeMap<ModuleId, bool> {
    map.iter().cloned().collect()
}

// --- The cursors' rule and the chain's owner, as they were in the shell.

#[test]
fn every_module_gets_one_first_report_then_only_changes() {
    let (e, a) = (editor(), agent());
    let both = [e.clone(), a.clone()];
    assert_eq!(
        focus_change(&answers(&[]), &both, Some(&e), true),
        vec![(e.clone(), true), (a.clone(), false)]
    );
    assert_eq!(
        focus_change(
            &answers(&[(e.clone(), true), (a.clone(), false)]),
            &both,
            Some(&e),
            true
        ),
        vec![]
    );
    assert_eq!(
        focus_change(
            &answers(&[(e.clone(), true), (a.clone(), false)]),
            &both,
            Some(&a),
            true
        ),
        vec![(e, false), (a, true)]
    );
}

/// A module the window gains later is reported the first time a focus change sees it, and only it: the modules
/// already known report nothing new.
#[test]
fn a_module_added_later_gets_its_first_report_and_nothing_else_moves() {
    let (e, a, canvas) = (editor(), agent(), ModuleId::parse("canvas").unwrap());
    let known = answers(&[(e.clone(), true), (a.clone(), false)]);
    assert_eq!(
        focus_change(&known, &[e.clone(), a.clone(), canvas.clone()], Some(&e), true),
        vec![(canvas, false)]
    );
}

#[test]
fn the_owning_module_is_named_by_its_id_not_its_position() {
    let modules = [agent(), editor(), lua("notes")];
    assert_eq!(FocusChain(vec![editor()]).owner(&modules), Some(editor()));
    assert_eq!(FocusChain(vec![lua("notes")]).owner(&modules), Some(lua("notes")));
    assert_eq!(
        FocusChain(vec![lua("gone"), editor()]).owner(&modules),
        Some(editor()),
        "a host that is no module of the window is walked past"
    );
    assert_eq!(FocusChain(vec![]).owner(&modules), None, "no focus at all");
}

#[test]
fn a_module_nested_in_another_module_owns_its_own_focus() {
    let modules = [agent(), lua("inner")];
    assert_eq!(
        FocusChain(vec![lua("inner"), agent()]).owner(&modules),
        Some(lua("inner"))
    );
}

#[test]
fn an_inactive_window_gives_no_pane_the_keys() {
    let (e, a) = (editor(), agent());
    let both = [e.clone(), a.clone()];
    // Alt-tab away: the editor stops drawing its cursor.
    assert_eq!(
        focus_change(
            &answers(&[(e.clone(), true), (a.clone(), false)]),
            &both,
            Some(&e),
            false
        ),
        vec![(e.clone(), false)]
    );
    // The keys on the top bar: neither module has them.
    assert_eq!(
        focus_change(&answers(&[(e, false), (a.clone(), true)]), &both, None, true),
        vec![(a, false)]
    );
}

// --- The report drives the cursors and the history.

#[test]
fn native_focus_not_the_request_drives_the_cursors() {
    let a = lua("a");
    let root = Node::split(
        Axis::Row,
        0.5,
        leaf(editor()),
        Node::split(Axis::Column, 0.5, leaf(terminal()), leaf(a.clone())),
    );
    let mut layout = Layout::new(root, editor()).unwrap();
    hide(&mut layout, &a);
    let r = driven(layout);
    r.native(&editor());
    let mark = r.mark();
    assert!(
        !r.driver.focus_now(WIN, &a),
        "the host refuses the keys for a module it does not show"
    );
    assert!(
        !r.ops_since(mark).iter().any(|op| signals(op, &a)),
        "a refused request tells nobody it has the keys"
    );
    let mark = r.mark();
    r.native(&terminal());
    let ops = r.ops_since(mark);
    assert!(ops.contains(&HostOp::Terminal(TermOp::SetFocused {
        win: WIN,
        module: terminal(),
        focused: true,
    })));
    assert!(ops.contains(&HostOp::Editor(EditorCall::SetFocused {
        win: WIN,
        focused: false,
    })));
    assert!(!ops.iter().any(|op| signals(op, &a)), "nothing for a: {ops:?}");
}

/// A module the host already reports with the keys before the lifecycle holds it, untracked then: declaring it is
/// what tells it, or its input method would stay shut until the keys left and came back.
#[test]
fn a_module_given_the_keys_before_its_declaration_hears_them_once_declared() {
    let r = driven(two());
    r.native(&editor());
    let t2 = ModuleId::parse("terminal#2").unwrap();
    r.host.with_surface(WIN, t2.clone(), decl(&t2).kind, true);
    crate::layout::place(&mut r.layout.borrow_mut(), &t2, &editor(), Axis::Column).unwrap();
    r.post(HostEvent::LayoutChangedByShell { win: WIN });
    let mark = r.mark();
    r.native(&t2);
    let ops = r.ops_since(mark);
    assert!(
        !ops.iter().any(|op| signals(op, &t2)),
        "an undeclared module is no tracked module yet: {ops:?}"
    );
    let declare = {
        let decl = decl(&t2);
        move |w: &mut Window, _: &dyn HostReads, batch: &mut Batch| {
            w.lifecycle.declare(&decl, true);
            crate::wm::focus::modules_changed(w, WIN, batch);
        }
    };
    let mark = r.mark();
    r.batch(declare.clone());
    let ops = r.ops_since(mark);
    let told: Vec<_> = ops.iter().filter(|op| signals(op, &t2)).collect();
    assert_eq!(
        told,
        vec![&HostOp::Terminal(TermOp::SetFocused {
            win: WIN,
            module: t2.clone(),
            focused: true,
        })],
        "told once that it has the keys: {ops:?}"
    );
    assert!(
        !ops.iter().any(|op| signals(op, &editor()) || signals(op, &agent())),
        "the modules already told hear nothing again: {ops:?}"
    );
    // A second declaration of the same instance (an idle one taken again) changes no answer.
    let mark = r.mark();
    r.batch(declare);
    assert!(!r.ops_since(mark).iter().any(|op| signals(op, &t2)));
}

#[test]
fn a_module_that_held_the_keys_mid_batch_is_noted_in_the_history() {
    let r = driven(three(editor()));
    r.native(&editor());
    let (_, batch) = r.batch(|w, _, batch| {
        assert!(give_keys(w, WIN, &agent(), vec![], batch));
        // A later op of the same batch takes it off screen again.
        hide(&mut w.layout.borrow_mut(), &agent());
        batch.push_layout(rig::unmap(&agent()));
        assert!(give_keys(w, WIN, &terminal(), vec![], batch));
    });
    assert_eq!(
        moves(&batch),
        vec![
            focus_op(&agent(), vec![]),
            rig::unmap(&agent()),
            focus_op(&terminal(), vec![])
        ]
    );
    assert!(
        !r.layout.borrow().is_shown(&agent()),
        "the premise: hidden by the batch's end"
    );
    assert_eq!(r.previous(), Some(agent()));
    assert_eq!(r.layout.borrow().focus(), &terminal());
}

#[test]
fn a_refused_grab_withdraws_only_its_own_request() {
    let r = driven(three(editor()));
    r.native(&editor());
    // The host refuses the agent's grab: its surface is not on screen there.
    let kind = decl(&agent()).kind;
    r.host.with_surface(WIN, agent(), kind, false);
    r.batch(|w, _, batch| {
        assert!(give_keys(w, WIN, &agent(), vec![], batch));
        assert!(give_keys(w, WIN, &terminal(), vec![], batch));
        // A later op of the same batch takes the terminal off screen again.
        hide(&mut w.layout.borrow_mut(), &terminal());
        batch.push_layout(rig::unmap(&terminal()));
    });
    assert_eq!(
        r.previous(),
        Some(editor()),
        "the terminal held the keys although another grab of its batch was refused"
    );
}

#[test]
fn a_grab_of_the_module_holding_the_keys_leaves_no_request_behind() {
    let r = driven(three(editor()));
    r.native(&editor());
    r.native(&terminal());
    // A toolkit reports nothing when a grab puts the keys where they already are.
    r.host.set_model(false);
    r.batch(|w, _, batch| assert!(give_keys(w, WIN, &terminal(), vec![], batch)));
    assert!(r
        .driver
        .query(|c| c.focus_state(WIN).is_some_and(|focus| focus.asked.is_empty())));
    // So a later report naming the terminal once it is hidden is not a grab this window asked for.
    r.native(&agent());
    r.batch(|w, _, _| hide(&mut w.layout.borrow_mut(), &terminal()));
    r.post(native(&terminal(), Some(0)));
    assert_eq!(
        r.previous(),
        Some(terminal()),
        "the history did not take the hidden terminal"
    );
}

#[test]
fn a_grab_the_batch_went_past_leaves_the_layouts_tab_and_zoom_alone() {
    // A tabbed container of the agent (hidden), the terminal and the editor, the editor zoomed and holding the keys.
    let tabs = Con::new(
        ConLayout::Tabbed,
        vec![
            Child::new(leaf(agent()), 0.25),
            Child::new(leaf(terminal()), 0.25),
            Child::new(leaf(editor()), 0.5),
        ],
    );
    let mut layout = Layout::new(Node::Con(tabs), editor()).unwrap();
    hide(&mut layout, &agent());
    layout.set_focus(&editor()).unwrap();
    layout.toggle_zoom(&editor());
    assert_eq!(
        layout.zoom(),
        Some(&Zoom::Module(editor())),
        "the premise: the editor is zoomed"
    );
    let r = driven(layout);
    r.native(&editor());
    // `prefix ?` showed the sheet, ending the zoom, and gave it the keys.
    r.batch(|w, _, batch| {
        w.layout.borrow_mut().show(&agent()).unwrap();
        w.layout.borrow_mut().unzoom();
        assert!(give_keys(w, WIN, &agent(), vec![], batch));
    });
    let opening = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: Some(Zoom::Module(editor())),
        showed_sheet: true,
    });
    assert_eq!(r.host.focus_owner(WIN), Some(agent()));
    let mark = r.mark();
    r.close_sheet(opening, false);
    let grabs: Vec<_> = r
        .ops_since(mark)
        .into_iter()
        .filter_map(|op| match op {
            HostOp::Focus {
                to: FocusTo::Module(module),
                ..
            } => Some(module),
            _ => None,
        })
        .collect();
    assert_eq!(
        grabs,
        vec![terminal(), editor()],
        "the premise: hiding the sheet gives the terminal the keys on the way"
    );
    let layout = r.layout.borrow();
    assert_eq!(
        layout.zoom(),
        Some(&Zoom::Module(editor())),
        "the zoom the sheet ended is back"
    );
    assert_eq!(layout.focus(), &editor());
    assert!(layout.is_visible(&editor()));
}

#[test]
fn focus_moved_is_reported_only_for_an_accepted_move() {
    // Refused before the batch as well: a module the tree does not hold, or one hidden that nobody asked for.
    let hidden = lua("hidden");
    let root = Node::split(
        Axis::Row,
        0.5,
        leaf(editor()),
        Node::split(Axis::Column, 0.5, leaf(agent()), leaf(hidden.clone())),
    );
    let mut layout = Layout::new(root, editor()).unwrap();
    hide(&mut layout, &hidden);
    let r = driven(layout);
    r.native(&editor());
    let opening = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: None,
        showed_sheet: false,
    });
    r.native(&agent());
    r.native(&lua("ghost"));
    r.post(native(&hidden, Some(0)));
    assert_eq!(r.previous(), Some(editor()), "neither was noted");
    r.native(&agent());
    let mark = r.mark();
    r.close_sheet(opening, false);
    assert_eq!(
        moves(&batch_of(r.ops_since(mark))),
        vec![focus_op(&editor(), vec![])],
        "neither was reported, so the sheet's way back survived them"
    );

    // Hidden only by a later op of the same batch: noted, and reported, so the way back is gone.
    let r = driven(three(editor()));
    r.native(&editor());
    let opening = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: None,
        showed_sheet: false,
    });
    r.native(&agent());
    r.batch(|w, _, batch| {
        give_keys(w, WIN, &terminal(), vec![], batch);
        hide(&mut w.layout.borrow_mut(), &terminal());
        batch.push_layout(rig::unmap(&terminal()));
        give_keys(w, WIN, &agent(), vec![], batch);
    });
    assert_eq!(r.previous(), Some(terminal()));
    let mark = r.mark();
    r.close_sheet(opening, false);
    assert!(
        !r.ops_since(mark).iter().any(|op| matches!(op, HostOp::Focus { .. })),
        "the keys left the sheet for the terminal, which forgot the way back"
    );
}

/// The ops of a host log as one batch, so the batch helpers read them.
fn batch_of(ops: Vec<HostOp>) -> Batch {
    let mut batch = Batch::default();
    for op in ops {
        batch.push_layout(op);
    }
    batch
}

#[test]
fn a_plugin_page_and_the_editors_typing_window_hear_has_keys() {
    let page = lua("page");
    let second = ModuleId::instance(ModuleKind::Terminal, 2).unwrap();
    let root = Node::split(
        Axis::Row,
        0.5,
        leaf(editor()),
        Node::split(
            Axis::Column,
            0.5,
            leaf(page.clone()),
            Node::split(Axis::Row, 0.5, leaf(terminal()), leaf(second.clone())),
        ),
    );
    let r = driven(Layout::new(root, editor()).unwrap());
    let has_keys = |module: &ModuleId, has_keys: bool| {
        HostOp::Plugin(PluginOp::HasKeys {
            win: WIN,
            module: module.clone(),
            has_keys,
        })
    };
    let mark = r.mark();
    r.native(&page);
    assert!(r.ops_since(mark).contains(&has_keys(&page, true)));
    let mark = r.mark();
    r.native(&editor());
    let ops = r.ops_since(mark);
    assert!(ops.contains(&has_keys(&page, false)));
    assert!(ops.contains(&HostOp::Editor(EditorCall::SetFocused {
        win: WIN,
        focused: true,
    })));
    assert!(ops.contains(&HostOp::Chat(ChatOp::EditorHasKeys {
        win: WIN,
        has_keys: true,
    })));
    let mark = r.mark();
    r.native(&terminal());
    let ops = r.ops_since(mark);
    assert!(ops.contains(&HostOp::Chat(ChatOp::EditorHasKeys {
        win: WIN,
        has_keys: false,
    })));
    let told_off: Vec<&HostOp> = ops
        .iter()
        .filter(|op| matches!(op, HostOp::Terminal(TermOp::SetFocused { focused: false, .. })))
        .collect();
    assert_eq!(
        told_off,
        vec![&HostOp::Terminal(TermOp::SetFocused {
            win: WIN,
            module: second,
            focused: false,
        })],
        "one solid cursor: every other live terminal is told once"
    );
}

#[test]
fn an_owner_change_refreshes_the_selection_chrome() {
    let r = driven(three(editor()));
    r.native(&editor());
    let derived = || r.driver.query(|c| c.chrome_derivations(WIN)).top_bar;
    let before = derived();
    r.native(&agent());
    assert!(derived() > before, "the selection's text is derived again");
    let before = derived();
    r.native(&agent());
    assert_eq!(derived(), before, "a report of the same owner moved nothing");
}

#[test]
fn taking_the_keys_raises_a_lower_float_from_an_idle_not_inside_the_signal() {
    let (x, y) = (lua("x"), lua("y"));
    let float = |id: &ModuleId| Float {
        id: id.clone(),
        rect: FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 300,
            h: 200,
        },
        scratch: false,
    };
    let layout = Layout::from_parts_floating(
        Node::split(Axis::Row, 0.5, leaf(editor()), leaf(agent())),
        vec![float(&x), float(&y)],
        BTreeSet::new(),
        y.clone(),
        Default::default(),
    )
    .unwrap();
    let r = driven(layout);
    let mark = r.mark();
    r.native(&x);
    assert!(
        !r.ops_since(mark).iter().any(|op| matches!(op, HostOp::Restack { .. })),
        "nothing restacks inside the report"
    );
    assert_eq!(
        r.layout.borrow().floats().last().map(|f| &f.id),
        Some(&x),
        "the layout raised it"
    );
    let mark = r.mark();
    r.scheduler.advance(Duration::ZERO);
    let ops: Vec<HostOp> = r
        .ops_since(mark)
        .into_iter()
        .filter(|op| !matches!(op, HostOp::Chrome { .. }))
        .collect();
    assert_eq!(
        ops,
        vec![HostOp::Restack {
            win: WIN,
            moves: vec![
                RestackMove {
                    module: y.clone(),
                    after: None,
                },
                RestackMove {
                    module: x,
                    after: Some(y),
                },
            ],
        },]
    );
}

// --- Requests.

#[test]
fn a_refused_grab_runs_no_arrive_in_land_by_key() {
    let mut layout = two();
    hide(&mut layout, &agent());
    let r = driven(layout);
    let (asked, batch) = r.batch(|w, _, batch| land_by_key(w, WIN, &agent(), batch));
    assert!(!asked);
    assert!(batch.is_empty(), "a hidden chat is asked for nothing: {batch:?}");
    assert!(r
        .notes()
        .contains(&"[focus] agent is hidden; the keys stay where they are".to_string()));

    // Shown in the layout, but the host refuses the grab: its tail never runs.
    let r = driven(two());
    let kind = decl(&agent()).kind;
    r.host.with_surface(WIN, agent(), kind, false);
    let mark = r.mark();
    let (asked, _) = r.batch(|w, _, batch| land_by_key(w, WIN, &agent(), batch));
    assert!(asked);
    let applied = r.applied_since(mark);
    assert_eq!(applied, vec![(focus_op(&agent(), vec![arrive()]), Applied::Refused)]);
}

#[test]
fn giving_the_keys_to_a_tab_not_drawn_switches_its_tab_first() {
    let (a, b) = (lua("a"), lua("b"));
    let r = driven(editor_beside_tabs(a.clone(), b.clone()));
    let (asked, batch) = r.batch(|w, _, batch| give_keys(w, WIN, &b, vec![], batch));
    assert!(asked);
    assert_eq!(
        moves(&batch),
        vec![rig::map(&b), focus_op(&b, vec![]), rig::unmap(&a), rig::allocate()]
    );
    assert_eq!(r.host.focus_owner(WIN), Some(b));
}

#[test]
fn a_strip_entry_click_and_a_hint_landing_give_the_keys_and_arrive() {
    let b = lua("b");
    let r = driven(editor_beside_tabs(b.clone(), agent()));
    let press = PointerEv::Press {
        x: 1.0,
        y: 1.0,
        button: 1,
    };
    let mark = r.mark();
    r.post(HostEvent::Pointer {
        win: WIN,
        on: PointerTarget::StripEntry(agent()),
        ev: press,
    });
    let applied = r.applied_since(mark);
    assert_eq!(
        moves(&batch_of(applied.iter().map(|(op, _)| op.clone()).collect())),
        vec![
            rig::map(&agent()),
            focus_op(&agent(), vec![arrive_at_card()]),
            rig::unmap(&b),
            rig::allocate()
        ]
    );
    assert!(
        applied.contains(&(arrive_at_card(), Applied::Done)),
        "the grab took, so it arrived"
    );

    let landed = |target| HostEvent::Hint(HintEvent::Landed { win: WIN, target });
    let mark = r.mark();
    r.post(landed(HintTarget::StripEntry(b.clone())));
    assert!(r.ops_since(mark).contains(&focus_op(&b, vec![])));
    let mark = r.mark();
    r.post(landed(HintTarget::StripEntry(agent())));
    let applied = r.applied_since(mark);
    assert!(applied.contains(&(focus_op(&agent(), vec![arrive_at_card()]), Applied::Done)));
    assert!(applied.contains(&(arrive_at_card(), Applied::Done)));
    // A module HINT lands on only gets the keys, as the label it chose promised.
    let mark = r.mark();
    r.post(landed(HintTarget::Module(editor())));
    let ops = r.ops_since(mark);
    assert!(ops.contains(&focus_op(&editor(), vec![])));
    assert!(!ops.iter().any(|op| matches!(op, HostOp::Chat(ChatOp::Arrive { .. }))));
}

#[test]
fn the_editor_feeds_direction_moves_focus_from_the_editor() {
    // The keys are on the terminal, from which nothing lies to the right.
    let r = driven(three(terminal()));
    let mark = r.mark();
    r.post(HostEvent::Editor(EditorEvent::Navigate {
        win: WIN,
        direction: Direction::Right,
    }));
    assert!(
        r.ops_since(mark).iter().any(|op| matches!(op, HostOp::Focus { .. })),
        "from the editor there is a module to the right"
    );
    assert!(r
        .notes()
        .iter()
        .any(|line| line.starts_with("[pane_switch] editor Right -> ")));
}

#[test]
fn up_with_nothing_above_goes_to_the_top_bar_and_its_way_back_returns_the_keys() {
    let r = driven(three(editor()));
    r.native(&editor());
    let mark = r.mark();
    r.post(HostEvent::Editor(EditorEvent::Navigate {
        win: WIN,
        direction: Direction::Up,
    }));
    assert_eq!(
        r.ops_since(mark).first(),
        Some(&HostOp::Focus {
            win: WIN,
            to: FocusTo::TopBarItem(FIRST_TOP_BAR_ITEM),
            on_grab: vec![],
        })
    );
    let mark = r.mark();
    r.post(HostEvent::TopBarLeft { win: WIN });
    assert_eq!(r.ops_since(mark).first(), Some(&focus_op(&editor(), vec![])));
}

#[test]
fn select_last_and_select_next_refuse_with_todays_texts() {
    let mut r = rig::rig(Layout::new(leaf(editor()), editor()).unwrap());
    let said = r.said();
    let batch = r.run(Action::SelectLast);
    r.assert_refused(&batch, &said, "no last module");
    let said = r.said();
    let batch = r.run(Action::SelectNext);
    r.assert_refused(&batch, &said, "no other module on screen");
    let notes = r.controller.notes(rig::WIN);
    assert!(notes.contains(&"[prefix] no last module".to_string()));
    assert!(notes.contains(&"[prefix] no other module on screen".to_string()));
}

#[test]
fn select_last_lands_on_the_module_before_and_select_next_on_the_next_one_on_screen() {
    let r = driven(three(editor()));
    r.native(&editor());
    r.native(&terminal());
    let mark = r.mark();
    r.driver.run(WIN, Action::SelectLast, Origin::Key { keycode: 7 });
    assert_eq!(moves(&batch_of(r.ops_since(mark))), vec![focus_op(&editor(), vec![])]);
    let mark = r.mark();
    r.driver.run(WIN, Action::SelectNext, Origin::Key { keycode: 7 });
    assert_eq!(
        moves(&batch_of(r.ops_since(mark))),
        vec![focus_op(&agent(), vec![arrive()])],
        "the next module on screen after the editor, arrived in"
    );
}

#[test]
fn a_keyboard_move_into_a_chat_keeps_the_readers_place_and_a_strip_entry_goes_to_its_card() {
    // `Ctrl+l` from the editor lands on the chat to its right.
    let r = driven(three(editor()));
    r.native(&editor());
    let mark = r.mark();
    r.post(HostEvent::Editor(EditorEvent::Navigate {
        win: WIN,
        direction: Direction::Right,
    }));
    assert_eq!(
        moves(&batch_of(r.ops_since(mark))),
        vec![focus_op(&agent(), vec![arrive()])]
    );

    // `select.last` back into the chat.
    r.native(&terminal());
    let mark = r.mark();
    r.driver.run(WIN, Action::SelectLast, Origin::Key { keycode: 7 });
    assert_eq!(
        moves(&batch_of(r.ops_since(mark))),
        vec![focus_op(&agent(), vec![arrive()])]
    );

    // A strip entry names the chat itself, so what waits there comes first.
    let r = driven(editor_beside_tabs(lua("b"), agent()));
    let mark = r.mark();
    r.post(HostEvent::Pointer {
        win: WIN,
        on: PointerTarget::StripEntry(agent()),
        ev: PointerEv::Press {
            x: 1.0,
            y: 1.0,
            button: 1,
        },
    });
    let ops = r.ops_since(mark);
    assert!(ops.contains(&arrive_at_card()), "{ops:?}");
    assert!(!ops.contains(&arrive()), "{ops:?}");
}

// --- The `?` sheet's way back.

#[test]
fn the_keys_return_after_the_sheet_closes_where_they_were() {
    let r = driven(three(editor()));
    r.native(&editor());
    let opening = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: None,
        showed_sheet: false,
    });
    r.native(&agent());
    let mark = r.mark();
    r.close_sheet(opening, false);
    assert_eq!(moves(&batch_of(r.ops_since(mark))), vec![focus_op(&editor(), vec![])]);

    // A forced close moves nothing.
    r.native(&editor());
    let opening = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: None,
        showed_sheet: false,
    });
    r.native(&agent());
    let mark = r.mark();
    r.close_sheet(opening, true);
    assert!(moves(&batch_of(r.ops_since(mark))).is_empty());

    // An opening that showed the chat hides it again: the keys go first, then the unmap.
    let mut layout = two();
    hide(&mut layout, &agent());
    let r = driven(layout);
    r.native(&editor());
    r.batch(|w, _, batch| {
        w.layout.borrow_mut().show(&agent()).unwrap();
        w.changed_by_controller(WIN, batch);
        give_keys(w, WIN, &agent(), vec![], batch);
    });
    let opening = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: None,
        showed_sheet: true,
    });
    assert_eq!(r.host.focus_owner(WIN), Some(agent()));
    let mark = r.mark();
    r.close_sheet(opening, false);
    assert_eq!(
        moves(&batch_of(r.ops_since(mark))),
        vec![focus_op(&editor(), vec![]), rig::unmap(&agent()), rig::allocate()]
    );
    assert!(!r.layout.borrow().is_shown(&agent()));
}

#[test]
fn a_sheet_superseded_by_a_newer_request_returns_nothing_on_its_close() {
    let r = driven(three(editor()));
    r.native(&editor());
    let opening = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: None,
        showed_sheet: false,
    });
    r.native(&agent());
    r.driver.keymap_superseded(WIN);
    let mark = r.mark();
    r.close_sheet(opening, false);
    assert!(
        moves(&batch_of(r.ops_since(mark))).is_empty(),
        "the newer request keeps the keys"
    );
}

#[test]
fn a_sheet_close_of_an_earlier_opening_returns_nothing() {
    let r = driven(three(editor()));
    r.native(&editor());
    let first = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: Some(Zoom::Module(editor())),
        showed_sheet: true,
    });
    r.native(&agent());
    r.native(&editor());
    let second = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: None,
        showed_sheet: false,
    });
    r.native(&agent());
    let mark = r.mark();
    r.close_sheet(first, false);
    let ops = r.ops_since(mark);
    assert!(
        moves(&batch_of(ops.clone())).is_empty(),
        "no focus, no hide, no zoom: {ops:?}"
    );
    assert_eq!(r.layout.borrow().zoom(), None);
    let mark = r.mark();
    r.close_sheet(second, false);
    assert_eq!(moves(&batch_of(r.ops_since(mark))), vec![focus_op(&editor(), vec![])]);
}

#[test]
fn the_keys_go_back_before_the_zoom_after_the_sheet_closes() {
    let r = driven(three(editor()));
    r.native(&editor());
    let opening = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: Some(Zoom::Module(editor())),
        showed_sheet: false,
    });
    r.native(&agent());
    let mark = r.mark();
    r.close_sheet(opening, false);
    let ops = moves(&batch_of(r.ops_since(mark)));
    let focus = ops.iter().position(|op| *op == focus_op(&editor(), vec![]));
    let allocate = ops.iter().position(|op| *op == rig::allocate());
    assert!(
        matches!((focus, allocate), (Some(f), Some(a)) if f < a),
        "the keys move before the zoom's change reaches the grid: {ops:?}"
    );
    assert!(
        ops[..focus.unwrap()]
            .iter()
            .all(|op| !matches!(op, HostOp::Unmap { .. })),
        "nothing leaves the screen before the keys have moved: {ops:?}"
    );
    assert_eq!(r.layout.borrow().zoom(), Some(&Zoom::Module(editor())));
}

#[test]
fn a_refused_return_grab_leaves_the_zoom_off_and_the_chat_on_screen() {
    let r = driven(three(editor()));
    r.native(&editor());
    let opening = r.open_sheet(Opened {
        from: Some(editor()),
        ended_zoom: Some(Zoom::Module(editor())),
        showed_sheet: false,
    });
    r.native(&agent());
    // The host refuses the editor's grab: its surface is not on screen there.
    let kind = decl(&editor()).kind;
    r.host.with_surface(WIN, editor(), kind, false);
    let mark = r.mark();
    r.close_sheet(opening, false);
    let applied = r.applied_since(mark);
    assert!(
        applied
            .iter()
            .any(|(op, how)| *op == focus_op(&editor(), vec![]) && *how == Applied::Refused),
        "the premise: the return grab was refused: {applied:?}"
    );
    assert_eq!(
        r.layout.borrow().zoom(),
        None,
        "the zoom would hide the chat that still has the keys"
    );
    assert!(
        !applied.iter().any(|(op, _)| *op == rig::unmap(&agent())),
        "the chat holding the keys stays on screen: {applied:?}"
    );
    assert_eq!(
        r.driver
            .query(|c| c.focus_state(WIN).and_then(|focus| focus.native.clone())),
        Some(FocusOwner::Module(agent())),
        "the keys are still reported in the chat"
    );
    // A later report of the editor taking the keys is not the return's grab: the zoom stays off.
    r.native(&editor());
    assert_eq!(r.layout.borrow().zoom(), None);
}
