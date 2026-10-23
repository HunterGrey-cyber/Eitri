use super::*;
use crate::keymap::prefix::Press;
use crate::keymap::{Action, KeySpec};
use crate::layout::{hide, Axis, FloatRect, Frame, Node, Size};
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::verbs::rig::{decl, rig, WIN};
use crate::wm::*;
use std::cell::RefCell;

const GRID: Size = Size { w: 1200, h: 800 };
const PREFIX: u32 = 50;
const K1: u32 = 51;

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn agent() -> ModuleId {
    ModuleId::agent()
}
fn terminal() -> ModuleId {
    ModuleId::terminal()
}
fn leaf(id: ModuleId) -> Node {
    Node::Leaf(id)
}

/// `left | right`, the keys on `keys`.
fn pair(left: ModuleId, right: ModuleId, keys: ModuleId) -> Layout {
    Layout::new(Node::split(Axis::Row, 0.5, leaf(left), leaf(right)), keys).unwrap()
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

fn key(text: &str, keycode: u32) -> KeyPress {
    let spec = KeySpec::parse(text).unwrap();
    KeyPress {
        keycode,
        press: Press::Key(spec),
        key: Some(spec.key),
        mods: Mods {
            ctrl: spec.ctrl,
            alt: spec.meta,
            shift: spec.shift,
        },
        super_or_hyper: false,
        super_or_meta: false,
    }
}

/// The key the default keymap binds `action` to after the prefix.
fn bound(action: &Action) -> String {
    let keymap = Keymap::defaults();
    let key = *keymap
        .keys_for(action)
        .first()
        .unwrap_or_else(|| panic!("{action:?} has a default key"));
    key.to_string()
}

/// A controller holding `layout` as the main window behind a driver, over the fake host with a surface for every
/// module and the default keymap's prefix, after its first apply.
struct Rig {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    scheduler: Rc<FakeScheduler>,
    layout: Rc<RefCell<Layout>>,
    workspaces: Rc<RefCell<Workspaces>>,
}

fn driven(layout: Layout) -> Rig {
    let host = FakeHost::new();
    host.set_grid_size(GRID);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let keymap = Keymap::defaults();
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
    let hosted: Vec<_> = layout.modules().iter().map(decl).collect();
    for d in &hosted {
        host.with_surface(WIN, d.module.clone(), d.kind.clone(), false);
    }
    let layout = Rc::new(RefCell::new(layout));
    let workspaces = Rc::new(RefCell::new(Workspaces::single()));
    controller.open_window(
        WIN,
        WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::Owned,
            title: "project".into(),
        },
        WindowParts {
            layout: layout.clone(),
            workspaces: workspaces.clone(),
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
        workspaces,
    }
}

impl Rig {
    fn mark(&self) -> usize {
        self.host.log().len()
    }

    fn ops_since(&self, mark: usize) -> Vec<HostOp> {
        self.host.log()[mark..]
            .iter()
            .filter_map(|logged| match logged {
                Logged::Op(op, _) => Some(op.clone()),
                _ => None,
            })
            .collect()
    }

    fn chrome(&self) -> ChromeModel {
        self.driver.query(|c| c.chrome(WIN))
    }

    fn derivations(&self) -> Derivations {
        self.driver.query(|c| c.chrome_derivations(WIN))
    }

    /// What the chrome was last told the prefix waits for.
    fn waiting(&self) -> Waiting {
        self.driver.query(|c| c.prefix_shown(WIN)).expect("the window").0
    }

    fn window_key(&self, press: KeyPress) -> Verdict {
        self.driver.key(WIN, &KeyStage::Window, press)
    }

    /// `prefix <key>`, both released.
    fn prefix_then(&self, text: &str) -> Verdict {
        assert_eq!(self.window_key(key("C-b", PREFIX)), Verdict::Consumed);
        self.host.post(HostEvent::KeyReleased {
            win: WIN,
            keycode: PREFIX,
        });
        let verdict = self.window_key(key(text, K1));
        self.host.post(HostEvent::KeyReleased { win: WIN, keycode: K1 });
        verdict
    }
}

fn chrome_ops(ops: &[HostOp]) -> usize {
    ops.iter().filter(|op| matches!(op, HostOp::Chrome { .. })).count()
}

fn one_card() -> Attention {
    Attention {
        pending: 1,
        arrived: 1,
        ..Default::default()
    }
}

#[test]
fn the_digits_drawn_are_the_plans_with_the_tenth_module_at_zero() {
    let plan: Vec<(char, ModuleId)> = "1234567890"
        .chars()
        .enumerate()
        .map(|(i, digit)| (digit, ModuleId::lua(&format!("p{i}"))))
        .collect();
    let drawn = digits(&plan);
    assert_eq!(drawn.len(), 10);
    assert_eq!(drawn[0], (ModuleId::lua("p0"), 1));
    assert_eq!(drawn[8], (ModuleId::lua("p8"), 9));
    assert_eq!(drawn[9], (ModuleId::lua("p9"), 0), "the tenth module's key is 0");
}

#[test]
fn a_window_starts_with_the_prefix_waiting_for_nothing_and_no_plan() {
    let keys = super::super::keys::KeyState::default();
    assert_eq!(keys.waiting, Waiting::No);
    assert!(keys.plan.is_empty());
    let model = ChromeModel::default();
    assert_eq!(model.prefix_strip, None);
    assert_eq!(model.modes.mode, modes::WindowMode::Windowed);
    assert!(!model.modes.top_bar_revealed);
}

#[test]
fn the_first_apply_announces_the_whole_model_once() {
    let r = driven(three(editor()));
    let model = r.chrome();
    let order: Vec<ModuleId> = model.tray.iter().map(|chip| chip.module.clone()).collect();
    assert_eq!(
        order,
        chip_order(&r.layout.borrow().modules()),
        "every chip, in its fixed order"
    );
    assert!(
        model.tray.iter().all(|chip| !chip.shown),
        "everything is on screen: {:?}",
        model.tray
    );
    assert_eq!(model.top_bar.workspace, None, "one workspace has no word for it");
    assert_eq!(model.top_bar.selection, None);
    assert_eq!(model.top_bar.mark, None);
    let ops = r.ops_since(0);
    assert_eq!(chrome_ops(&ops), 1, "{ops:#?}");
}

#[test]
fn wait_needs_the_top_bar() {
    let r = driven(three(editor()));
    r.driver.run(WIN, Action::WindowImmersive, Origin::Key { keycode: K1 });
    assert!(!r.chrome().modes.mode.shows_top_bar());
    assert!(!r.chrome().modes.top_bar_revealed, "immersive hides the bar");
    r.driver.await_workspace(WIN);
    assert!(
        r.chrome().modes.top_bar_revealed,
        "the chooser's strip is the only list of its keys, so the bar comes out"
    );
    r.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    assert_eq!(r.waiting(), Waiting::No);
    assert!(!r.chrome().modes.top_bar_revealed, "and goes back once the wait ends");
}

#[test]
fn only_the_workspace_chooser_needs_the_top_bar_shown() {
    let r = driven(three(editor()));
    assert_eq!(r.window_key(key("C-b", PREFIX)), Verdict::Consumed);
    assert_eq!(r.waiting(), Waiting::Command);
    assert!(
        !r.chrome().modes.top_bar_revealed,
        "an armed prefix reminds of fixed keys"
    );
    assert!(r.chrome().prefix_strip.is_some_and(|pieces| !pieces.is_empty()));
    r.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    assert_eq!(r.prefix_then(&bound(&Action::SelectNumber)), Verdict::Consumed);
    assert_eq!(r.waiting(), Waiting::Number);
    assert!(
        !r.chrome().modes.top_bar_revealed,
        "the digits are drawn on the modules"
    );
    r.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    r.driver.await_workspace(WIN);
    assert!(matches!(r.waiting(), Waiting::Workspace { .. }));
    assert!(r.chrome().modes.top_bar_revealed);
    assert_eq!(
        r.chrome().prefix_strip,
        Some(prefix_strip::workspace_pieces(&["main".to_string()], "main", false))
    );
}

#[test]
fn a_chat_on_an_undrawn_tab_has_no_chip_while_its_title_is_on_a_strip() {
    let r = driven(pair(editor(), agent(), editor()));
    r.driver.run(WIN, Action::ContainerTabbed, Origin::Key { keycode: K1 });
    assert!(r.layout.borrow().is_titled_on_strip(&agent()));
    assert!(!r.layout.borrow().is_visible(&agent()), "the editor's tab is drawn");
    r.host.set_attention(ViewId::BASE, one_card());
    let mark = r.mark();
    r.host.post(HostEvent::Chat(ChatEvent::Attention {
        win: WIN,
        view: ViewId::BASE,
    }));
    let model = r.chrome();
    let chip = model.tray.iter().find(|chip| chip.module == agent()).unwrap();
    assert!(!chip.shown, "its strip names it, so the tray does not: {chip:?}");
    assert_eq!(model.strips.len(), 1, "{:?}", model.strips);
    let strip = &model.strips[0];
    let entry = strip.entries.iter().find(|entry| entry.module == agent()).unwrap();
    assert_eq!(entry.label, "agent \u{2691}1", "the entry reads as the chip would");
    assert_eq!(strip.entries[strip.active.unwrap()].module, editor());
    assert_eq!(
        chrome_ops(&r.ops_since(mark)),
        1,
        "the card's mark is a change of the model"
    );
}

#[test]
fn a_hidden_chats_card_marks_its_chip() {
    let r = driven(three(editor()));
    hide(&mut r.layout.borrow_mut(), &agent(), &Frame::new(GRID, 1)).unwrap();
    r.host.post(HostEvent::LayoutChangedByShell { win: WIN });
    let chip = |r: &Rig| r.chrome().tray.into_iter().find(|chip| chip.module == agent()).unwrap();
    assert!(chip(&r).shown && !chip(&r).attention, "{:?}", chip(&r));
    assert_eq!(chip(&r).label, "agent");
    r.host.set_attention(ViewId::BASE, one_card());
    let mark = r.mark();
    r.host.post(HostEvent::Chat(ChatEvent::Attention {
        win: WIN,
        view: ViewId::BASE,
    }));
    assert!(chip(&r).attention, "{:?}", chip(&r));
    assert_eq!(chip(&r).label, "agent \u{2691}1");
    assert_eq!(chrome_ops(&r.ops_since(mark)), 1);
}

#[test]
fn a_repeated_toast_is_a_new_serial() {
    let mut r = rig(Layout::new(leaf(editor()), editor()).unwrap());
    let first = r.run(Action::SelectNext);
    let one = r.controller.chrome(WIN);
    let second = r.run(Action::SelectNext);
    let two = r.controller.chrome(WIN);
    let (a, b) = (one.toast.unwrap(), two.toast.unwrap());
    assert_eq!(a.text, "no other module on screen");
    assert_eq!(a.text, b.text, "the same refusal");
    assert_ne!(a.serial, b.serial, "a repeat is a new toast, not a missed edge");
    assert_eq!(two.top_bar.flash, one.top_bar.flash + 1, "and a new flash");
    for batch in [first, second] {
        let ops: Vec<HostOp> = batch.ops().cloned().collect();
        assert_eq!(chrome_ops(&ops), 1, "{ops:#?}");
    }
}

#[test]
fn a_toast_goes_when_its_time_is_up() {
    let r = driven(Layout::new(leaf(editor()), editor()).unwrap());
    r.driver.run(WIN, Action::SelectNext, Origin::Key { keycode: K1 });
    assert!(r.chrome().toast.is_some());
    let mark = r.mark();
    r.scheduler.advance(TOAST_FOR - Duration::from_millis(1));
    assert!(r.chrome().toast.is_some(), "still up before its time");
    r.scheduler.advance(Duration::from_millis(1));
    assert_eq!(r.chrome().toast, None);
    assert_eq!(chrome_ops(&r.ops_since(mark)), 1);
}

#[test]
fn a_refusal_flashes_but_leaves_a_waiting_cards_toast_up() {
    let r = driven(Layout::new(leaf(editor()), editor()).unwrap());
    r.host.post(HostEvent::Chat(ChatEvent::Toast {
        win: WIN,
        text: "agent \u{2691}1".into(),
        kind: ToastKind::Attention,
    }));
    let before = r.chrome();
    assert_eq!(
        before.toast.as_ref().map(|toast| toast.kind),
        Some(ToastKind::Attention),
        "a chat's toast for a waiting card is up in the model"
    );
    r.driver.run(WIN, Action::SelectNext, Origin::Key { keycode: K1 });
    let after = r.chrome();
    assert_eq!(
        after.toast, before.toast,
        "a stray key does not erase the word of a waiting card"
    );
    assert_eq!(
        after.top_bar.flash,
        before.top_bar.flash + 1,
        "the refusal still flashes"
    );
}

#[test]
fn a_field_whose_inputs_did_not_change_is_not_derived() {
    let r = driven(three(editor()));
    let before = r.derivations();
    let mark = r.mark();
    r.host.post(HostEvent::Wake);
    r.host.post(HostEvent::Chat(ChatEvent::Busy {
        win: WIN,
        view: ViewId::BASE,
        busy: true,
    }));
    let after = r.derivations();
    assert_eq!(after.tray, before.tray, "the agent is on screen: no chip shows it");
    assert_eq!(after.top_bar, before.top_bar);
    assert_eq!(after, before, "nothing at all was derived again");
    assert_eq!(chrome_ops(&r.ops_since(mark)), 0);
    r.host.post(HostEvent::LayoutChangedByShell { win: WIN });
    assert!(
        r.derivations().top_bar > before.top_bar,
        "a change of the layout is derived again"
    );
}

#[test]
fn pointer_motion_during_a_drag_derives_no_label() {
    let r = driven(three(editor()));
    let before = r.derivations();
    let mark = r.mark();
    for i in 0..100 {
        r.host.post(HostEvent::Pointer {
            win: WIN,
            on: PointerTarget::Divider(0),
            ev: PointerEv::Motion {
                x: f64::from(i),
                y: 10.0,
            },
        });
    }
    assert_eq!(r.derivations(), before);
    assert_eq!(chrome_ops(&r.ops_since(mark)), 0);
}

/// The ops the coverage judgement left: the coverage the panel was told, and the pages it was told were seen.
fn judged(ops: &[HostOp]) -> (Vec<Vec<ViewId>>, Vec<ViewId>) {
    let told = ops
        .iter()
        .filter_map(|op| match op {
            HostOp::Chat(ChatOp::SetCoverage { covered, .. }) => Some(covered.clone()),
            _ => None,
        })
        .collect();
    let seen = ops
        .iter()
        .filter_map(|op| match op {
            HostOp::Chat(ChatOp::PageShown { view, .. }) => Some(*view),
            _ => None,
        })
        .collect();
    (told, seen)
}

#[test]
fn a_page_shown_mid_batch_reports_attention_with_the_final_coverage() {
    let r = driven(pair(editor(), agent(), editor()));
    let float = ModuleId::lua("x");
    let mark = r.mark();
    let layout = r.layout.clone();
    r.driver.test_batch(move |_| {
        layout
            .borrow_mut()
            .add_float(&float, FloatRect::centred(GRID, 0.6), false)
            .unwrap();
        let mut batch = Batch::default();
        batch.push_layout(HostOp::Map {
            win: WIN,
            module: editor(),
        });
        // The chat's page comes on screen here, with nothing over it yet.
        batch.push_layout(HostOp::Map {
            win: WIN,
            module: agent(),
        });
        batch.push_layout(HostOp::Allocate { win: WIN });
        // And here the float the batch shows goes over it.
        batch.push_layout(HostOp::Map {
            win: WIN,
            module: float,
        });
        batch
    });
    let ops = r.ops_since(mark);
    let posted = r.host.log()[mark..]
        .iter()
        .any(|logged| matches!(logged, Logged::Event(HostEvent::Chat(ChatEvent::PageShown { .. }))));
    assert!(posted, "the chat's map posted its page coming on screen");
    let (told, seen) = judged(&ops);
    assert_eq!(told, vec![vec![ViewId::BASE]], "judged once, as covered: {ops:#?}");
    assert!(seen.is_empty(), "a page under the batch's float was not seen: {ops:#?}");
}

#[test]
fn a_page_shown_with_nothing_over_it_is_seen_once() {
    let r = driven(pair(editor(), agent(), editor()));
    let mark = r.mark();
    r.driver.test_batch(|_| {
        let mut batch = Batch::default();
        batch.push_layout(HostOp::Map {
            win: WIN,
            module: agent(),
        });
        batch.push_layout(HostOp::Map {
            win: WIN,
            module: agent(),
        });
        batch
    });
    let (told, seen) = judged(&r.ops_since(mark));
    assert!(told.is_empty(), "the coverage the panel knows did not change");
    assert_eq!(seen, vec![ViewId::BASE], "two maps in one batch: seen once");
}

#[test]
fn a_refusal_flashes_and_toasts_through_the_model() {
    let mut r = rig(Layout::new(leaf(editor()), editor()).unwrap());
    let before = r.controller.chrome(WIN);
    let batch = r.run(Action::Rotate { down: false });
    let ops: Vec<HostOp> = batch.ops().cloned().collect();
    let text = "rotate: fewer than two modules on screen";
    assert!(
        matches!(ops.as_slice(), [HostOp::Chrome { win: WIN, .. }]),
        "a refusal moves nothing and is drawn from the model alone: {ops:#?}"
    );
    let after = r.controller.chrome(WIN);
    assert_eq!(after.top_bar.flash, before.top_bar.flash + 1);
    assert_eq!(after.toast.map(|toast| toast.text), Some(text.to_string()));
}

/// An arm still in the shell refuses through the same model: its words on the toast and the app name's flash,
/// or the flash alone for none.
#[test]
fn a_shell_arms_refusal_flashes_and_toasts_through_the_model() {
    let mut r = rig(Layout::new(leaf(editor()), editor()).unwrap());
    let before = r.controller.chrome(WIN);
    let batch = r.controller.handle(HostEvent::ShellRefused {
        win: WIN,
        text: "the scratchpad is empty".into(),
    });
    let ops: Vec<HostOp> = batch.ops().cloned().collect();
    assert!(matches!(ops.as_slice(), [HostOp::Chrome { win: WIN, .. }]), "{ops:#?}");
    let after = r.controller.chrome(WIN);
    assert_eq!(after.top_bar.flash, before.top_bar.flash + 1);
    let toast = after.toast.expect("the refusal's words are up");
    assert_eq!(toast.text, "the scratchpad is empty");
    let batch = r.controller.handle(HostEvent::ShellRefused {
        win: WIN,
        text: String::new(),
    });
    assert!(batch.ops().all(|op| matches!(op, HostOp::Chrome { .. })));
    let flashed = r.controller.chrome(WIN);
    assert_eq!(flashed.top_bar.flash, after.top_bar.flash + 1, "a bare refusal flashes");
    assert_eq!(
        flashed.toast.map(|t| t.serial),
        Some(toast.serial),
        "and says nothing new"
    );
}

#[test]
fn a_container_verb_with_no_container_flashes_without_a_toast() {
    let mut r = rig(Layout::new(leaf(editor()), editor()).unwrap());
    let before = r.controller.chrome(WIN);
    r.run(Action::ContainerTabbed);
    let after = r.controller.chrome(WIN);
    assert_eq!(after.top_bar.flash, before.top_bar.flash + 1);
    assert_eq!(after.toast, None);
}

#[test]
fn window_immersive_toggles_the_mode_and_conceals_the_bar() {
    let mut r = rig(three(editor()));
    let batch = r.run(Action::WindowImmersive);
    assert_eq!(batch.shell_verb(), None, "decided here");
    let ops: Vec<HostOp> = batch.ops().cloned().collect();
    assert_eq!(
        ops[0],
        HostOp::SetMode {
            win: WIN,
            mode: crate::wm::WindowMode {
                immersive: true,
                fullscreen: true,
                top_bar_revealed: false,
            },
        },
        "{ops:#?}"
    );
    assert_eq!(chrome_ops(&ops), 1);
    let model = r.controller.chrome(WIN);
    assert_eq!(
        model.modes.mode,
        modes::WindowMode::Immersive { from_fullscreen: false }
    );
    assert!(
        !model.modes.mode.shows_top_bar() && !model.modes.top_bar_revealed,
        "the bar is concealed"
    );
    let back = r.run(Action::WindowImmersive);
    assert!(back.ops().any(|op| matches!(
        op,
        HostOp::SetMode {
            mode: crate::wm::WindowMode {
                immersive: false,
                fullscreen: false,
                ..
            },
            ..
        }
    )));
    assert_eq!(r.controller.chrome(WIN).modes.mode, modes::WindowMode::Windowed);
}

#[test]
fn a_host_that_refuses_the_mode_gets_the_shells_arm() {
    let mut r = rig(three(editor()));
    r.run(Action::WindowImmersive);
    let batch = r.controller.handle(HostEvent::Refused {
        what: OpTag::SetMode,
        why: "not carried out by this shell yet".into(),
        surface: None,
    });
    assert_eq!(
        batch.shell_verb(),
        Some(&ShellVerb {
            win: WIN,
            call: ShellCall::Action(Action::WindowImmersive),
            keycode: 0,
        })
    );
    let again = r.controller.handle(HostEvent::Refused {
        what: OpTag::SetMode,
        why: "not carried out by this shell yet".into(),
        surface: None,
    });
    assert_eq!(again.shell_verb(), None, "one toggle, one arm");
}

#[test]
fn titles_name_the_project_the_lua_panels_and_the_tray() {
    let notes = ModuleId::lua("notes");
    let r = driven(pair(editor(), notes.clone(), editor()));
    let mark = r.mark();
    r.driver.set_titles(
        WIN,
        ChromeTitles {
            project: "neovibe".into(),
            lua: vec![(notes.clone(), "Notes".into())],
            tray: vec![editor(), agent(), notes.clone()],
        },
    );
    let model = r.chrome();
    assert_eq!(model.top_bar.project, "neovibe");
    let order: Vec<ModuleId> = model.tray.iter().map(|chip| chip.module.clone()).collect();
    assert_eq!(order, chip_order(&[editor(), agent(), notes.clone()]));
    let chip = model.tray.iter().find(|chip| chip.module == notes).unwrap();
    assert_eq!(chip.label, "Notes");
    assert_eq!(chrome_ops(&r.ops_since(mark)), 1);
}

#[test]
fn the_top_bar_reads_the_selection_the_mark_and_the_workspace() {
    let r = driven(three(editor()));
    r.driver.run(WIN, Action::MarkToggle, Origin::Key { keycode: K1 });
    assert_eq!(r.chrome().top_bar.mark.as_deref(), Some("marked: editor"));
    r.driver.run(WIN, Action::FocusParent, Origin::Key { keycode: K1 });
    assert!(r
        .chrome()
        .top_bar
        .selection
        .is_some_and(|text| text.starts_with("selected: ")));
    r.workspaces.borrow_mut().create("study", three(editor())).unwrap();
    r.host.post(HostEvent::LayoutChangedByShell { win: WIN });
    assert_eq!(r.chrome().top_bar.workspace.as_deref(), Some("main"));
}

#[test]
fn a_float_on_screen_has_its_bar_named() {
    let r = driven(pair(editor(), agent(), editor()));
    let float = ModuleId::lua("x");
    r.layout
        .borrow_mut()
        .add_float(&float, FloatRect::centred(GRID, 0.5), false)
        .unwrap();
    r.host.post(HostEvent::LayoutChangedByShell { win: WIN });
    assert_eq!(
        r.chrome().float_bars,
        vec![FloatLabel {
            module: float.clone(),
            label: "lua:x".into()
        }]
    );
    let chip = r.chrome().tray.into_iter().find(|chip| chip.module == agent()).unwrap();
    assert!(chip.shown, "a chat under a float has its chip: {chip:?}");
}

#[test]
fn the_chooser_pick_leaves_the_prefix_strip() {
    let r = driven(three(editor()));
    r.driver.await_workspace(WIN);
    assert!(r.chrome().prefix_strip.is_some());
    r.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    assert_eq!(r.chrome().prefix_strip, None);
}

/// The coverage judgements and the uncovered notices in `ops`.
fn uncovered(ops: &[HostOp]) -> Vec<Vec<ViewId>> {
    ops.iter()
        .filter_map(|op| match op {
            HostOp::Chat(ChatOp::TellUncovered { views, .. }) => Some(views.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_float_leaving_a_chat_tells_the_panel_once_the_queue_drains() {
    let r = driven(pair(editor(), agent(), editor()));
    let float = ModuleId::lua("x");
    r.layout
        .borrow_mut()
        .add_float(&float, FloatRect::centred(GRID, 0.6), false)
        .unwrap();
    let mark = r.mark();
    r.host.post(HostEvent::LayoutChangedByShell { win: WIN });
    let ops = r.ops_since(mark);
    let (told, _) = judged(&ops);
    assert_eq!(
        told,
        vec![vec![ViewId::BASE]],
        "the float the shell put up covers the chat: {ops:#?}"
    );
    assert!(uncovered(&ops).is_empty());

    *r.layout.borrow_mut() = pair(editor(), agent(), editor());
    let mark = r.mark();
    r.host.post(HostEvent::LayoutChangedByShell { win: WIN });
    let ops = r.ops_since(mark);
    let (told, seen) = judged(&ops);
    assert_eq!(
        told,
        vec![Vec::<ViewId>::new()],
        "nothing is covered once the float is gone: {ops:#?}"
    );
    assert_eq!(
        uncovered(&ops),
        vec![vec![ViewId::BASE]],
        "what finished under the float is seen once it is clear, with no page mapped: {ops:#?}"
    );
    assert!(seen.is_empty(), "no page came on screen by a map");

    let mark = r.mark();
    r.host.post(HostEvent::LayoutChangedByShell { win: WIN });
    let ops = r.ops_since(mark);
    assert_eq!(
        judged(&ops),
        (vec![], vec![]),
        "a change that leaves the coverage alone tells nothing"
    );
    assert!(uncovered(&ops).is_empty());
}

#[test]
fn a_resize_that_moves_a_float_onto_a_chat_puts_its_chip_up() {
    let r = driven(pair(editor(), agent(), editor()));
    let float = ModuleId::lua("x");
    // A 400 pixel float centred at a quarter of the width: over the editor alone at 1200 pixels, and over the
    // chat too once the grid is half as wide.
    let rect = FloatRect {
        cx: 0.25,
        cy: 0.5,
        w: 400,
        h: 400,
    };
    r.layout.borrow_mut().add_float(&float, rect, false).unwrap();
    r.host.post(HostEvent::LayoutChangedByShell { win: WIN });
    let allocator: &dyn Allocator = &*r.driver;
    allocator.allocate(WIN, GRID);
    r.scheduler.advance(Duration::ZERO);
    let chip = |r: &Rig| r.chrome().tray.into_iter().find(|chip| chip.module == agent()).unwrap();
    assert!(!chip(&r).shown, "the chat is clear of the float at the full width");

    let narrow = Size { w: 600, h: 800 };
    let mark = r.mark();
    r.host.set_grid_size(narrow);
    allocator.allocate(WIN, narrow);
    allocator.allocate(WIN, narrow);
    assert!(r.ops_since(mark).is_empty(), "nothing is decided inside an allocation");
    r.scheduler.advance(Duration::ZERO);
    let ops = r.ops_since(mark);
    assert!(
        chip(&r).shown,
        "the float covers the chat at half the width, so its chip is up"
    );
    assert_eq!(
        chrome_ops(&ops),
        1,
        "the resize's idle announces the model once: {ops:#?}"
    );
    assert_eq!(
        judged(&ops).0,
        vec![vec![ViewId::BASE]],
        "and tells the panel the chat is covered"
    );
}

#[test]
fn a_resize_with_no_float_up_derives_nothing() {
    let r = driven(pair(editor(), agent(), editor()));
    let allocator: &dyn Allocator = &*r.driver;
    let before = r.derivations();
    let armed = r.scheduler.armed().len();
    allocator.allocate(WIN, Size { w: 600, h: 800 });
    assert_eq!(
        r.scheduler.armed().len(),
        armed,
        "no idle for a resize nothing floats over"
    );
    assert_eq!(r.derivations(), before);
}
