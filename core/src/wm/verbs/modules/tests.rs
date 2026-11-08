use super::*;
use crate::keymap::prefix::{Press, Waiting};
use crate::keymap::KeySpec;
use crate::layout::{Float, FloatRect, Frame, ModuleKeys, Nav, Node, Selection, Size, Workspaces};
use crate::lua::layout::LayoutRequest;
use crate::window_link::names::WindowName;
use crate::wm::chrome::ChromeModel;
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::verbs::rig::{decl, Said};
use crate::wm::*;
use std::cell::RefCell;
use std::rc::Rc;

const WIN: WinId = WinId::MAIN;
const GRID: Size = Size { w: 1200, h: 800 };
// Arbitrary, distinct hardware keycodes: a real keyboard never reuses one for two keys.
const PREFIX: u32 = 50;
const K1: u32 = 51;
const K2: u32 = 52;

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn agent() -> ModuleId {
    ModuleId::agent()
}
fn terminal() -> ModuleId {
    ModuleId::terminal()
}
fn agent2() -> ModuleId {
    ModuleId::instance(ModuleKind::Agent, 2).unwrap()
}
fn terminal2() -> ModuleId {
    ModuleId::instance(ModuleKind::Terminal, 2).unwrap()
}

/// `editor | (agent / terminal)`, the keys on `keys`.
fn three(keys: ModuleId) -> Layout {
    let root = Node::split(
        Axis::Row,
        0.5,
        Node::Leaf(editor()),
        Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(terminal())),
    );
    Layout::new(root, keys).unwrap()
}

/// `editor | agent`, the float `float` over the agent's half, the keys on `keys`.
fn agent_under(float: &ModuleId, scratch: bool, keys: ModuleId) -> Layout {
    Layout::from_parts_floating(
        Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(agent())),
        vec![Float {
            id: float.clone(),
            rect: FloatRect {
                cx: 0.75,
                cy: 0.5,
                w: 400,
                h: 400,
            },
            scratch,
        }],
        Default::default(),
        keys,
        Default::default(),
    )
    .unwrap()
}

/// A key as the shell translates it: its prefix reading, its name and its modifiers.
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

/// A controller holding a layout as the main window behind a driver, over the fake host with a surface for every
/// module but those left without one, the default keymap, and a second workspace `two` holding the same modules.
struct Rig {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    layout: Rc<RefCell<Layout>>,
    workspaces: Rc<RefCell<Workspaces>>,
    /// What the chrome said of refusals at each mark taken, for [`Rig::refusals_since`].
    saids: RefCell<std::collections::HashMap<usize, Said>>,
}

fn driven_without(layout: Layout, without: &[ModuleId]) -> Rig {
    driven_as(layout, without, WindowRole::Main, EditorPresence::Owned)
}

/// [`driven_without`], the window opened in `role` with `presence`.
fn driven_as(layout: Layout, without: &[ModuleId], role: WindowRole, presence: EditorPresence) -> Rig {
    let host = FakeHost::new();
    host.set_grid_size(GRID);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let keymap = Keymap::defaults();
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
    let hosted: Vec<_> = layout.modules().iter().map(decl).collect();
    for d in hosted.iter().filter(|d| !without.contains(&d.module)) {
        host.with_surface(WIN, d.module.clone(), d.kind.clone(), false);
    }
    let mut workspaces = Workspaces::single();
    workspaces.create("two", layout.fresh_copy()).unwrap();
    let workspaces = Rc::new(RefCell::new(workspaces));
    let layout = Rc::new(RefCell::new(layout));
    controller.open_window(
        WIN,
        WindowSpec {
            role,
            presence,
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
    driver.set_titles(
        WIN,
        crate::wm::chrome::ChromeTitles {
            project: "project".into(),
            lua: vec![(ModuleId::lua("notes"), "notes".into())],
            tray: vec![],
        },
    );
    Rig {
        driver,
        host,
        layout,
        workspaces,
        saids: RefCell::default(),
    }
}

fn driven(layout: Layout) -> Rig {
    driven_without(layout, &[])
}

impl Rig {
    fn mark(&self) -> usize {
        let at = self.host.log().len();
        self.saids.borrow_mut().insert(at, self.said());
        at
    }

    /// What the chrome says of refusals now.
    fn said(&self) -> Said {
        let chrome = self.chrome();
        Said {
            flash: chrome.top_bar.flash,
            toast: chrome.toast.map(|toast| (toast.serial, toast.text)),
        }
    }

    /// The refusals the chrome drew since `mark`, one per flash of the app name: the toast's words, or an empty
    /// string for a flash that said nothing.
    fn refusals_since(&self, mark: usize) -> Vec<String> {
        let before = self.saids.borrow().get(&mark).cloned().expect("a mark taken by `mark`");
        let now = self.said();
        let flashed = (now.flash - before.flash) as usize;
        let text = match (&now.toast, &before.toast) {
            (Some((serial, text)), Some((was, _))) if serial != was => text.clone(),
            (Some((_, text)), None) => text.clone(),
            _ => String::new(),
        };
        vec![text; flashed]
    }

    /// What the prefix was last told to wait for.
    fn waiting(&self) -> Waiting {
        self.driver.query(|c| c.prefix_shown(WIN)).expect("the window").0
    }

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

    /// The host reports `module` holds the keys, outside any batch.
    fn native(&self, module: &ModuleId) {
        self.host.post(HostEvent::NativeFocus {
            win: WIN,
            owner: Some(FocusOwner::Module(module.clone())),
            chain: FocusChain(vec![module.clone()]),
            active: true,
            op_index: None,
        });
    }

    fn run(&self, action: Action, origin: Origin) {
        self.driver.run(WIN, action, origin);
    }

    /// Drives the named-instance creation directly so its refusal text is checked independently of float gates.
    fn make_scratch(&self, id: &ModuleId) {
        self.driver.test_batch(|c| {
            let (reads, keymap, setup) = c.module_parts(WIN);
            let cx = Cx {
                reads: &*reads,
                keymap: &keymap,
                setup: &setup,
            };
            let mut batch = Batch::default();
            scratch_new(c.window_mut(WIN).unwrap(), &cx, WIN, id, &mut batch);
            // Direct family calls leave chrome dirty; a wake settles it before the host draws the batch.
            batch.append(c.handle(HostEvent::Wake));
            batch
        });
    }

    /// A key pressed at the window stage and released.
    fn window_key(&self, press: KeyPress) -> Verdict {
        let keycode = press.keycode;
        let verdict = self.driver.key(WIN, &KeyStage::Window, press);
        self.host.post(HostEvent::KeyReleased { win: WIN, keycode });
        verdict
    }

    fn lua(&self, request: LayoutRequest) {
        self.driver.test_batch(|c| c.lua_request(WIN, request));
    }

    fn chrome(&self) -> ChromeModel {
        self.driver.query(|c| c.chrome(WIN))
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }

    fn stored_two(&self) -> Layout {
        self.workspaces.borrow().stored("two").unwrap().clone()
    }
}

/// A split key's terminal key pressed in the module with the keys, with `cwd` as the split key says.
fn split_from(c: &mut Controller, cwd: SplitCwd) -> Batch {
    let (reads, keymap, setup) = c.module_parts(WIN);
    let window = c.window_mut(WIN).unwrap();
    let cx = Cx {
        reads: &*reads,
        keymap: &keymap,
        setup: &setup,
    };
    let mut batch = Batch::default();
    place(window, &cx, WIN, &terminal(), Axis::Row, cwd, &mut batch);
    batch
}

/// The number of the one directory read `ops` ask of `module`.
fn cwd_read_of(ops: &[HostOp], module: &ModuleId) -> u64 {
    let reads: Vec<u64> = ops
        .iter()
        .filter_map(|op| match op {
            HostOp::Terminal(TermOp::ReadCwd { module: m, read, .. }) if m == module => Some(*read),
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 1, "one directory read of {module}: {ops:?}");
    reads[0]
}

fn grabs_of(ops: &[HostOp], module: &ModuleId) -> usize {
    ops.iter()
        .filter(|op| matches!(op, HostOp::Focus { to: FocusTo::Module(m), .. } if m == module))
        .count()
}

/// The last module's question belongs to the window close, so its answer can also confirm the window's work.
#[test]
fn prefix_x_on_the_last_module_asks_the_window_close() {
    let r = driven(Layout::new(Node::Leaf(agent()), agent()).unwrap());
    let mark = r.mark();
    r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
    assert_eq!(
        r.chrome().prompt,
        Some(crate::wm::chrome::Prompt {
            text: "kill-pane agent? closes Eitri (y/n)".into(),
            closes_window: true,
            offers_move: false,
        })
    );
    assert!(matches!(
        r.ops_since(mark).as_slice(),
        [HostOp::Chrome { win: WIN, .. }]
    ));
    assert!(r.layout.borrow().is_shown(&agent()));
}

/// A card answer from a chat view a float covers is refused with the text that names the float and the key that
/// clears it; with nothing over the view it is taken. A layout held for writing while the gate is asked, a window
/// the controller does not hold and a view the layout does not hold refuse too: the gate fails toward the card.
#[test]
fn the_answer_gate_refuses_while_a_float_covers_the_chat() {
    let notes = ModuleId::lua("notes");
    let keymap = Keymap::defaults();
    let r = driven(agent_under(&notes, false, editor()));
    let sink = super::super::key_text(&keymap, &Action::FloatToggle);
    let text = feedback::covered_text("agent", "notes", &sink, false);
    assert_eq!(
        r.driver
            .query_or(Err(crate::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()), |c| c
                .answer_gate(WIN, ViewId::BASE)),
        Err(text.clone())
    );

    let scratch = driven(agent_under(&notes, true, editor()));
    let stow = super::super::key_text(&keymap, &Action::ScratchpadShow { module: None });
    assert_eq!(
        scratch
            .driver
            .query_or(Err(crate::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()), |c| c
                .answer_gate(WIN, ViewId::BASE)),
        Err(feedback::covered_text("agent", "notes", &stow, true)),
        "a scratch float is stowed, not sunk"
    );

    let clear = driven(three(editor()));
    assert_eq!(
        clear
            .driver
            .query_or(Err(crate::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()), |c| c
                .answer_gate(WIN, ViewId::BASE)),
        Ok(())
    );
    {
        let _writing = clear.layout.borrow_mut();
        assert_eq!(
            clear
                .driver
                .query_or(Err(crate::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()), |c| c
                    .answer_gate(WIN, ViewId::BASE)),
            Err(feedback::LAYOUT_CHANGING_TEXT.to_string()),
            "a layout being written cannot be judged"
        );
    }
    assert!(clear
        .driver
        .query_or(Err(crate::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()), |c| c
            .answer_gate(WinId(9), ViewId::BASE))
        .is_err());
    let absent = ViewId::of_module(&agent2()).unwrap();
    assert!(clear
        .driver
        .query_or(Err(crate::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()), move |c| c
            .answer_gate(WIN, absent))
        .is_err());

    // The toast is drawn from the model, by the event the shell posts for the refusal.
    let flashes = r.chrome().top_bar.flash;
    r.host.post(HostEvent::AnswerRefused {
        win: WIN,
        text: text.clone(),
    });
    let chrome = r.chrome();
    assert_eq!(chrome.top_bar.flash, flashes + 1);
    assert_eq!(chrome.toast.map(|toast| toast.text), Some(text));
}

/// The host refuses a new chat view's surface: no workspace gains it, the layout is as it was, and the host's reason
/// is the toast. Made, the next one is placed, added hidden to the other workspace, and has its new empty tab
/// opened before it takes the keys.
#[test]
fn a_failed_open_rolls_back_before_it_adds_everywhere() {
    let r = driven(three(editor()));
    r.host
        .refuse_kind(|kind| matches!(kind, SurfaceKind::Chat { view } if !view.is_base()));
    let before = r.layout.borrow().clone();
    let mark = r.mark();
    r.run(
        Action::InstanceNew {
            kind: ModuleKind::Agent,
        },
        Origin::Key { keycode: K1 },
    );
    let applied = r.applied_since(mark);
    assert!(
        applied.iter().any(
            |(op, how)| matches!(op, HostOp::CreateSurface { module, .. } if *module == agent2())
                && *how == Applied::Refused
        ),
        "{applied:?}"
    );
    assert_eq!(*r.layout.borrow(), before);
    assert!(!r.stored_two().contains(&agent2()), "no workspace gains it");
    let ops = r.ops_since(mark);
    assert!(!ops.iter().any(|op| matches!(op, HostOp::Chat(ChatOp::OpenTab { .. }))));
    let why = "the native model cannot apply this operation".to_string();
    assert_eq!(r.refusals_since(mark), vec![why.clone()]);
    assert_eq!(r.chrome().toast.map(|toast| toast.text), Some(why));

    r.host.refuse_kind(|_| false);
    let mark = r.mark();
    r.run(
        Action::InstanceNew {
            kind: ModuleKind::Agent,
        },
        Origin::Key { keycode: K2 },
    );
    let ops = r.ops_since(mark);
    assert!(r.layout.borrow().is_shown(&agent2()));
    assert!(r.stored_two().contains(&agent2()));
    assert!(!r.stored_two().is_shown(&agent2()), "hidden in the other workspace");
    let view = ViewId::of_module(&agent2()).unwrap();
    let tab = ops
        .iter()
        .position(|op| *op == HostOp::Chat(ChatOp::OpenTab { win: WIN, view }))
        .expect("its first tab");
    let grab = ops
        .iter()
        .position(|op| matches!(op, HostOp::Focus { to: FocusTo::Module(m), .. } if *m == agent2()))
        .expect("it takes the keys");
    assert!(tab < grab, "the tab is there before the view takes the keys: {ops:?}");
}

/// A new chat view the panel cannot give its first tab is never placed: the layout is as it was, no workspace gains
/// it, no tab is asked for, its surface is left idle, and the panel's words are the toast.
#[test]
fn a_chat_view_with_no_first_tab_is_not_placed() {
    let r = driven(three(editor()));
    let why = "the window is closing".to_string();
    r.host.set_chat_first_tab_refused(Some(why.clone()));
    let before = r.layout.borrow().clone();
    let mark = r.mark();
    r.run(
        Action::InstanceNew {
            kind: ModuleKind::Agent,
        },
        Origin::Key { keycode: K1 },
    );
    assert_eq!(*r.layout.borrow(), before);
    assert!(!r.stored_two().contains(&agent2()), "no workspace gains it");
    let ops = r.ops_since(mark);
    assert!(
        !ops.iter()
            .any(|op| matches!(op, HostOp::Chat(ChatOp::OpenTab { .. } | ChatOp::BreakOut { .. }))),
        "{ops:?}"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, HostOp::DropSurface { module, .. } if *module == agent2())),
        "its surface is left idle: {ops:?}"
    );
    assert_eq!(r.refusals_since(mark), vec![why.clone()]);
    assert_eq!(r.chrome().toast.map(|toast| toast.text), Some(why));
}

/// A follow command meets the gates its action meets: under a container selection the show runs and ends the
/// selection as a module key does, and the hide is refused as `module.hide` is; with the keys on a float both run,
/// as the float's gate says of a module key and of a hide.
#[test]
fn a_follow_show_meets_the_selection_and_float_gates() {
    let mut layout = three(agent());
    crate::layout::hide(&mut layout, &terminal(), &Frame::new(GRID, 1)).unwrap();
    assert!(
        crate::layout::focus_parent(&mut layout),
        "the premise: a container is selected"
    );
    assert_eq!(
        super::super::selection_gate(&Action::Module(terminal()), layout.selection()),
        super::super::SelectionGate::Run { reset: true }
    );
    let r = driven(layout);
    r.driver.test_batch(|c| c.follow(WIN, &terminal(), true));
    assert!(r.layout.borrow().is_shown(&terminal()), "the follow's show ran");
    assert_eq!(
        r.layout.borrow().selection(),
        Selection::Module,
        "and ended the selection"
    );

    let mut layout = three(agent());
    assert!(crate::layout::focus_parent(&mut layout));
    let r = driven(layout);
    let mark = r.mark();
    r.driver.test_batch(|c| c.follow(WIN, &terminal(), false));
    let text = super::super::one_module_text(&Keymap::defaults());
    assert_eq!(r.refusals_since(mark), vec![text]);
    assert!(r.layout.borrow().is_shown(&terminal()), "the refused hide hid nothing");

    let notes = ModuleId::lua("notes");
    for action in [Action::Module(agent()), Action::ModuleHide] {
        assert_eq!(super::super::float_gate(&action, true), super::super::FloatGate::Run);
    }
    let r = driven(agent_under(&notes, false, notes.clone()));
    let mark = r.mark();
    r.driver.test_batch(|c| c.follow(WIN, &agent(), false));
    assert!(
        !r.layout.borrow().is_shown(&agent()),
        "the hide ran with the keys on the float"
    );
    r.driver.test_batch(|c| c.follow(WIN, &agent(), true));
    assert!(r.layout.borrow().is_shown(&agent()), "and so did the show");
    assert!(r.refusals_since(mark).is_empty());
    assert_eq!(r.layout.borrow().focus(), &notes, "the keys stay where they are");
}

/// A chip is a module key from the top bar: on the module with the keys it gives it the keys and never hides it,
/// and neither a container selection nor the keys on a float refuse it. Neither gate refuses a module key either,
/// and the show's grab ends a selection as the gate's reset would, so this pins what a chip does under each, not
/// that the gates were passed by.
#[test]
fn a_chip_click_shows_and_focuses_never_hides_and_meets_no_gate() {
    let r = driven(three(terminal()));
    r.native(&terminal());
    let mark = r.mark();
    r.run(Action::Module(terminal()), Origin::Chip);
    assert!(r.layout.borrow().is_shown(&terminal()), "a chip never hides");
    assert!(r.refusals_since(mark).is_empty());

    let mut layout = three(agent());
    crate::layout::hide(&mut layout, &terminal(), &Frame::new(GRID, 1)).unwrap();
    assert!(crate::layout::focus_parent(&mut layout));
    assert!(matches!(layout.selection(), Selection::Con(_)));
    let r = driven(layout);
    let mark = r.mark();
    r.run(Action::Module(terminal()), Origin::Chip);
    let ops = r.ops_since(mark);
    assert!(r.refusals_since(mark).is_empty(), "no gate: {ops:?}");
    assert!(r.layout.borrow().is_shown(&terminal()));
    assert_eq!(grabs_of(&ops, &terminal()), 1, "{ops:?}");

    let notes = ModuleId::lua("notes");
    let r = driven(agent_under(&notes, false, notes.clone()));
    let mark = r.mark();
    r.run(Action::Module(editor()), Origin::Chip);
    let ops = r.ops_since(mark);
    assert!(r.refusals_since(mark).is_empty(), "{ops:?}");
    assert_eq!(grabs_of(&ops, &editor()), 1, "{ops:?}");
}

/// Each Lua module request acts on its own id with its own options: `hide(terminal)` with the editor holding the
/// keys hides the terminal, `show` and `focus` of the module with the keys never hide it, `open` with explicit
/// options places the new instance where they say, and `split(id, axis)` places `id`.
#[test]
fn lua_show_focus_hide_open_and_split_keep_their_ids_and_options() {
    let r = driven(three(editor()));
    r.native(&editor());
    r.lua(LayoutRequest::Hide(terminal()));
    assert!(!r.layout.borrow().is_shown(&terminal()));
    assert_eq!(r.layout.borrow().focus(), &editor());

    r.lua(LayoutRequest::Show(editor()));
    r.lua(LayoutRequest::Focus(editor()));
    assert!(r.layout.borrow().is_shown(&editor()), "a show or a focus never hides");
    assert_eq!(r.layout.borrow().focus(), &editor());

    let rule = OpenRule {
        at: Some(At::Beside(Direction::Left)),
        ..OpenRule::default()
    };
    let mut expected = r.layout.borrow().clone();
    layout::open_by_rule(&mut expected, &terminal2(), &rule, Some(&editor()), GRID).unwrap();
    r.lua(LayoutRequest::Open(ModuleKind::Terminal, Some(rule)));
    assert_eq!(r.layout.borrow().root(), expected.root(), "placed where the call said");
    assert!(r.stored_two().contains(&terminal2()));

    let mut expected = r.layout.borrow().clone();
    let keys = expected.focus().clone();
    layout::place(&mut expected, &agent(), &keys, Axis::Column).unwrap();
    r.lua(LayoutRequest::Split(agent(), Axis::Column));
    assert_eq!(
        r.layout.borrow().root(),
        expected.root(),
        "split places the module it names"
    );
    assert_eq!(r.layout.borrow().focus(), &agent());
}

/// A split key then the chat's key places the chat beside the module with the keys and gives it them; it arrives
/// (its oldest card first) only as the grab's tail, so a grab the host refuses arrives nowhere.
#[test]
fn a_split_key_on_an_existing_module_places_it_and_arrives_only_if_the_grab_took() {
    let keymap = Keymap::defaults();
    let split = keymap
        .bindings()
        .iter()
        .find(|b| matches!(b.action, Action::Split { axis: Axis::Row, .. }))
        .expect("a default split.right key")
        .clone();
    let arrive = HostOp::Chat(ChatOp::Arrive {
        win: WIN,
        view: ViewId::BASE,
        card_first: true,
        tab: None,
        intent: crate::agents::Intent::Show,
    });
    for grab_takes in [true, false] {
        let without = if grab_takes { vec![] } else { vec![agent()] };
        let r = driven_without(three(editor()), &without);
        r.native(&editor());
        assert_eq!(r.window_key(key("C-b", PREFIX)), Verdict::Consumed);
        assert_eq!(r.window_key(key(&split.key.to_string(), K1)), Verdict::Consumed);
        let mark = r.mark();
        assert_eq!(r.window_key(key("a", K2)), Verdict::Consumed);
        let applied = r.applied_since(mark);
        let grab = applied
            .iter()
            .position(|(op, _)| {
                matches!(op, HostOp::Focus { to: FocusTo::Module(m), on_grab, .. } if *m == agent() && *on_grab == vec![arrive.clone()])
            })
            .unwrap_or_else(|| panic!("the chat is given the keys, its arrival the tail: {applied:?}"));
        let arrived = applied.iter().position(|(op, _)| *op == arrive);
        if grab_takes {
            assert_eq!(applied[grab].1, Applied::Done);
            assert_eq!(arrived, Some(grab + 1), "the arrival follows the grab that took");
        } else {
            assert_eq!(applied[grab].1, Applied::Refused);
            assert_eq!(arrived, None, "a refused grab arrives nowhere");
        }
        let layout = r.layout.borrow();
        assert_eq!(
            crate::layout::navigate(&layout, &editor(), Direction::Right, &Frame::new(GRID, 1)),
            Nav::Module(agent()),
            "placed right of the module that had the keys"
        );
    }
}

/// The editor's edit key puts the keys in the chat and opens its composer: the grab's tail is the composer's
/// opening, not the reader's arrival, and a grab the host refuses opens nothing.
#[test]
fn the_edit_key_gives_the_chat_the_keys_and_opens_its_composer_instead_of_arriving() {
    let edit = HostOp::Chat(ChatOp::EditSelection {
        win: WIN,
        view: ViewId::BASE,
    });
    for grab_takes in [true, false] {
        let without = if grab_takes { vec![] } else { vec![agent()] };
        let r = driven_without(three(editor()), &without);
        r.native(&editor());
        let mark = r.mark();
        r.driver.post(HostEvent::Chat(ChatEvent::EditSelection { win: WIN }));
        let applied = r.applied_since(mark);
        let grab = applied
            .iter()
            .position(|(op, _)| {
                matches!(op, HostOp::Focus { to: FocusTo::Module(m), on_grab, .. } if *m == agent() && *on_grab == vec![edit.clone()])
            })
            .unwrap_or_else(|| panic!("the chat is given the keys, the composer its tail: {applied:?}"));
        assert!(
            !applied
                .iter()
                .any(|(op, _)| matches!(op, HostOp::Chat(ChatOp::Arrive { .. }))),
            "the composer replaces the arrival: {applied:?}"
        );
        let opened = applied.iter().position(|(op, _)| *op == edit);
        if grab_takes {
            assert_eq!(applied[grab].1, Applied::Done);
            assert_eq!(opened, Some(grab + 1), "the composer opens after the grab that took");
        } else {
            assert_eq!(applied[grab].1, Applied::Refused);
            assert_eq!(opened, None, "a refused grab opens nothing");
        }
    }
}

/// A chat that is hidden comes back, and one the keys are already in is not hidden by a second press: the edit
/// key never toggles.
#[test]
fn the_edit_key_shows_a_hidden_chat_and_never_hides_one() {
    let edit = HostOp::Chat(ChatOp::EditSelection {
        win: WIN,
        view: ViewId::BASE,
    });
    let mut layout = three(editor());
    crate::layout::hide(&mut layout, &agent(), &Frame::new(GRID, 1)).unwrap();
    let r = driven(layout);
    assert!(!r.layout.borrow().is_shown(&agent()));
    r.driver.post(HostEvent::Chat(ChatEvent::EditSelection { win: WIN }));
    assert!(r.layout.borrow().is_shown(&agent()), "the chat is shown");
    assert_eq!(r.layout.borrow().focus(), &agent(), "and holds the keys");

    for _ in 0..2 {
        let mark = r.mark();
        r.driver.post(HostEvent::Chat(ChatEvent::EditSelection { win: WIN }));
        assert!(
            r.layout.borrow().is_shown(&agent()),
            "a press with the keys there hides nothing"
        );
        assert!(r.ops_since(mark).contains(&edit), "and opens the composer again");
    }
}

/// A chat in the other window of a pair is that window's: that window is raised, as the chat's key raises it (its own
/// panel hears the same press and opens the composer), and this window takes none of it.
#[test]
fn the_edit_key_raises_the_other_window_for_a_chat_that_lives_there() {
    let r = driven(three(editor()));
    r.layout.borrow_mut().set_away(&agent(), true).unwrap();
    let mark = r.mark();
    r.driver.post(HostEvent::Chat(ChatEvent::EditSelection { win: WIN }));
    assert_eq!(
        r.refusals_since(mark),
        vec!["'agent' is in the other window".to_string()],
        "with no window there, the key says where the chat is, as the chat's key does"
    );
    assert!(
        !r.ops_since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::Chat(ChatOp::EditSelection { .. }))),
        "and opens no composer"
    );

    r.driver
        .attach_peer(WIN, PeerId(1), WindowName::Second, crate::wm::fake::FakePeerPort::new());
    let mark = r.mark();
    r.driver.post(HostEvent::Chat(ChatEvent::EditSelection { win: WIN }));
    let ops = r.ops_since(mark);
    assert!(
        ops.contains(&HostOp::RaisePeer {
            win: WIN,
            peer: PeerId(1)
        }),
        "the window holding the chat comes forward: {ops:?}"
    );
    assert!(
        !ops.iter()
            .any(|op| matches!(op, HostOp::Chat(ChatOp::EditSelection { .. }))),
        "and this window opens no composer of its own: {ops:?}"
    );
}

/// While the kill's y/n is shown, the window stage answers it before the prefix: the prefix key closes it without
/// arming the prefix and kills nothing, and `y` kills.
#[test]
fn the_kill_prompt_takes_y_before_the_prefix_and_any_other_key_closes_it() {
    let r = driven(three(terminal()));
    r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
    let prompt = r.chrome().prompt.expect("the y/n is asked");
    assert_eq!(prompt.text, "kill-pane terminal? (y/n)");
    assert!(!prompt.closes_window);

    assert_eq!(r.window_key(key("C-b", PREFIX)), Verdict::Consumed);
    assert_eq!(r.chrome().prompt, None, "any other key closes it");
    assert_eq!(r.waiting(), Waiting::No, "and arms nothing");
    assert!(r.layout.borrow().is_shown(&terminal()), "nothing was killed");

    r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
    assert!(r.chrome().prompt.is_some());
    let mark = r.mark();
    assert_eq!(r.window_key(key("y", K2)), Verdict::Consumed);
    assert_eq!(r.chrome().prompt, None);
    assert!(!r.layout.borrow().is_shown(&terminal()), "y kills");
    assert!(r.ops_since(mark).contains(&HostOp::Terminal(TermOp::HangUp {
        win: WIN,
        module: terminal(),
    })));
}

/// A terminal number killed before has its host left idle: `instance.new` takes that number again with no create,
/// and the lifecycle declares it afresh, so its shell starts anew once it is shown.
#[test]
fn a_reused_idle_terminal_is_declared_afresh_and_starts_a_new_shell() {
    let root = Node::split(
        Axis::Row,
        0.5,
        Node::Leaf(editor()),
        Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(terminal2())),
    );
    let r = driven(Layout::new(root, terminal2()).unwrap());
    r.host.post(HostEvent::Terminal(TermEvent::Started {
        win: WIN,
        module: terminal2(),
        pid: 41,
    }));
    r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
    r.window_key(key("y", K2));
    assert!(!r.layout.borrow().contains(&terminal2()));
    assert_eq!(
        r.driver
            .query(|c| c.lifecycle(WIN).and_then(|l| l.state(&terminal2()).cloned())),
        Some(ModuleState::Killed(Reopen::Remove))
    );

    let mark = r.mark();
    r.run(
        Action::InstanceNew {
            kind: ModuleKind::Terminal,
        },
        Origin::Key { keycode: K1 },
    );
    let ops = r.ops_since(mark);
    assert!(
        !ops.iter().any(|op| matches!(op, HostOp::CreateSurface { .. })),
        "the idle host is reused: {ops:?}"
    );
    assert!(r.layout.borrow().is_shown(&terminal2()));
    let start = ops
        .iter()
        .position(|op| {
            *op == HostOp::Terminal(TermOp::Start {
                win: WIN,
                module: terminal2(),
            })
        })
        .unwrap_or_else(|| panic!("a new shell: {ops:?}"));
    let dir = ops
        .iter()
        .position(|op| {
            *op == HostOp::Terminal(TermOp::SetStartDir {
                win: WIN,
                module: terminal2(),
                dir: PathBuf::from("."),
            })
        })
        .unwrap_or_else(|| panic!("the new shell's directory, the project root: {ops:?}"));
    assert!(dir < start, "the directory is set before the shell starts: {ops:?}");
}

/// A split key's terminal from a terminal, `cwd = "pane"`, on a number whose pane a kill left idle: no create is
/// asked, so the directory read goes to the host ahead of the new shell's start.
#[test]
fn a_reused_idle_terminal_from_a_split_starts_where_the_directory_read_says() {
    let root = Node::split(
        Axis::Row,
        0.5,
        Node::Leaf(editor()),
        Node::split(Axis::Column, 0.5, Node::Leaf(terminal()), Node::Leaf(terminal2())),
    );
    let r = driven(Layout::new(root, terminal2()).unwrap());
    r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
    r.window_key(key("y", K2));
    assert!(!r.layout.borrow().contains(&terminal2()));
    r.lua(LayoutRequest::Focus(terminal()));
    assert_eq!(r.layout.borrow().focus(), &terminal());

    let mark = r.mark();
    r.driver.test_batch(|c| split_from(c, SplitCwd::Pane));
    let read = cwd_read_of(&r.ops_since(mark), &terminal());
    let mark = r.mark();
    r.host.post(HostEvent::Terminal(TermEvent::Cwd {
        win: WIN,
        module: terminal(),
        read,
        dir: Ok(PathBuf::from("/work/b")),
    }));
    let ops = r.ops_since(mark);
    assert!(
        !ops.iter().any(|op| matches!(op, HostOp::CreateSurface { .. })),
        "the idle pane is reused: {ops:?}"
    );
    let dir = ops
        .iter()
        .position(|op| {
            *op == HostOp::Terminal(TermOp::SetStartDir {
                win: WIN,
                module: terminal2(),
                dir: PathBuf::from("/work/b"),
            })
        })
        .unwrap_or_else(|| panic!("the directory read reaches the host: {ops:?}"));
    let start = ops
        .iter()
        .position(|op| {
            *op == HostOp::Terminal(TermOp::Start {
                win: WIN,
                module: terminal2(),
            })
        })
        .unwrap_or_else(|| panic!("a new shell: {ops:?}"));
    assert!(dir < start, "{ops:?}");
    assert!(r.layout.borrow().is_shown(&terminal2()));
}

/// The base terminal's kill leaves its pane live and hidden, so a split key and its key from the editor places it
/// again, and it gets a fresh shell, as the split did before the kill.
#[test]
fn the_killed_base_terminal_is_placed_again_by_a_split_key() {
    let r = driven(three(terminal()));
    r.host.post(HostEvent::Terminal(TermEvent::Started {
        win: WIN,
        module: terminal(),
        pid: 41,
    }));
    r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
    r.window_key(key("y", K2));
    assert!(
        r.layout.borrow().contains(&terminal()) && !r.layout.borrow().is_shown(&terminal()),
        "the premise: the kill left it hidden in the layout"
    );
    assert!(matches!(
        r.driver
            .query(|c| c.lifecycle(WIN).and_then(|l| l.state(&terminal()).cloned())),
        Some(ModuleState::Killed(_))
    ));
    r.lua(LayoutRequest::Focus(editor()));
    r.native(&editor());

    let keymap = Keymap::defaults();
    let split = keymap
        .bindings()
        .iter()
        .find(|b| matches!(b.action, Action::Split { axis: Axis::Row, .. }))
        .expect("a default split.right key")
        .clone();
    assert_eq!(r.window_key(key("C-b", PREFIX)), Verdict::Consumed);
    assert_eq!(r.window_key(key(&split.key.to_string(), K1)), Verdict::Consumed);
    let mark = r.mark();
    assert_eq!(r.window_key(key("t", K2)), Verdict::Consumed);
    let ops = r.ops_since(mark);
    assert!(r.refusals_since(mark).is_empty(), "{ops:?}");
    assert!(r.layout.borrow().is_shown(&terminal()));
    assert_eq!(
        crate::layout::navigate(&r.layout.borrow(), &editor(), Direction::Right, &Frame::new(GRID, 1)),
        Nav::Module(terminal()),
        "placed right of the editor"
    );
    assert!(
        ops.contains(&HostOp::Terminal(TermOp::Start {
            win: WIN,
            module: terminal(),
        })),
        "a fresh shell: {ops:?}"
    );

    let r = driven(three(terminal()));
    r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
    r.window_key(key("y", K2));
    r.lua(LayoutRequest::Focus(editor()));
    r.lua(LayoutRequest::Split(terminal(), Axis::Column));
    assert!(
        r.layout.borrow().is_shown(&terminal()),
        "and so does eitri.layout.split"
    );
}

/// A chat's kill while one of its tabs cannot close is refused before anything leaves the screen: the view stays
/// in the layout and every workspace, no tab is asked to close and its page is not dropped.
#[test]
fn a_chat_kill_while_a_tab_cannot_close_leaves_the_view_where_it_is() {
    let blocked = "a tab is still being handed off to a terminal; kill the chat once that finishes".to_string();
    for chat in [agent2(), agent()] {
        let root = Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(editor()),
            Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(agent2())),
        );
        let r = driven(Layout::new(root, chat.clone()).unwrap());
        let view = ViewId::of_module(&chat).unwrap();
        r.host.set_chat_kill_counts(
            view,
            ChatKillCounts {
                blocked: Some(blocked.clone()),
                ..ChatKillCounts::default()
            },
        );
        r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
        assert!(r.chrome().prompt.is_some(), "the y/n is asked as before");
        let mark = r.mark();
        r.window_key(key("y", K2));
        let ops = r.ops_since(mark);
        assert!(r.layout.borrow().is_shown(&chat), "{chat} stays on screen: {ops:?}");
        assert!(r.stored_two().contains(&chat), "and in every workspace");
        assert!(
            !ops.iter().any(|op| matches!(
                op,
                HostOp::Chat(ChatOp::CloseTabs { .. }) | HostOp::DropSurface { .. } | HostOp::Unmap { .. }
            )),
            "{ops:?}"
        );
        assert!(!r.refusals_since(mark).is_empty(), "the refusal is flashed: {ops:?}");
        assert!(
            r.notes().iter().any(|line| line.contains(&blocked)),
            "and its reason said: {:?}",
            r.notes()
        );
        assert_eq!(
            r.driver
                .query(|c| c.lifecycle(WIN).and_then(|l| l.state(&chat).cloned())),
            Some(ModuleState::Shown)
        );
    }
}

/// The terminal's key: hidden, it is shown and given the keys; holding them, it is hidden, the keys going first to
/// the module the layout chooses. A number the layout does not hold is refused.
#[test]
fn a_terminal_key_shows_focuses_and_hides_by_where_the_keys_are() {
    let mut layout = three(editor());
    crate::layout::hide(&mut layout, &terminal(), &Frame::new(GRID, 1)).unwrap();
    let r = driven(layout);
    r.native(&editor());
    r.run(Action::Module(terminal()), Origin::Key { keycode: K1 });
    assert!(r.layout.borrow().is_shown(&terminal()));
    assert_eq!(r.host.focus_owner(WIN), Some(terminal()));

    r.run(Action::Module(terminal()), Origin::Key { keycode: K1 });
    assert!(!r.layout.borrow().is_shown(&terminal()), "holding the keys, it hides");
    assert_ne!(r.host.focus_owner(WIN), Some(terminal()));

    let mark = r.mark();
    r.run(Action::Module(terminal2()), Origin::Key { keycode: K1 });
    assert_eq!(r.refusals_since(mark), vec![feedback::not_in_window_text(&terminal2())]);
}

/// The chat's key reaches the view holding the oldest waiting card when the keys are in no chat view, and a module
/// the window has no host for is refused as not here yet.
#[test]
fn the_chat_key_goes_to_the_oldest_card_and_a_module_with_no_host_is_not_here_yet() {
    let root = Node::split(
        Axis::Row,
        0.5,
        Node::Leaf(editor()),
        Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(agent2())),
    );
    let r = driven(Layout::new(root, editor()).unwrap());
    r.native(&editor());
    r.host.set_oldest_card_view(ViewId::of_module(&agent2()));
    r.run(Action::Module(agent()), Origin::Key { keycode: K1 });
    assert_eq!(r.layout.borrow().focus(), &agent2());

    let canvas = ModuleId::parse("canvas").unwrap();
    let mark = r.mark();
    r.run(Action::Module(canvas.clone()), Origin::Key { keycode: K1 });
    assert_eq!(r.refusals_since(mark), vec![feedback::not_here_yet_text(&canvas)]);
}

/// A split from a terminal whose key says to start in its directory asks the host for it, and the new terminal is
/// made beside that terminal in the directory that came back; a split whose terminal ended meanwhile is refused
/// with the host's words.
#[test]
fn a_split_from_a_terminal_starts_its_shell_where_the_directory_read_says() {
    let r = driven(three(terminal()));
    let split_here = |c: &mut Controller| {
        let (reads, keymap, setup) = c.module_parts(WIN);
        let window = c.window_mut(WIN).unwrap();
        let cx = Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        let mut batch = Batch::default();
        place(window, &cx, WIN, &terminal(), Axis::Row, SplitCwd::Pane, &mut batch);
        batch
    };
    let mark = r.mark();
    r.driver.test_batch(split_here);
    let read = cwd_read_of(&r.ops_since(mark), &terminal());
    let mark = r.mark();
    r.host.post(HostEvent::Terminal(TermEvent::Cwd {
        win: WIN,
        module: terminal(),
        read,
        dir: Ok(PathBuf::from("/work/sub")),
    }));
    let ops = r.ops_since(mark);
    assert!(
        ops.contains(&HostOp::CreateSurface {
            win: WIN,
            module: terminal2(),
            kind: SurfaceKind::Terminal {
                cwd: PathBuf::from("/work/sub"),
            },
        }),
        "{ops:?}"
    );
    assert!(r.layout.borrow().is_shown(&terminal2()));

    // The keys are in the new terminal now, so the next split is pressed there.
    let mark = r.mark();
    r.driver.test_batch(split_here);
    let read = cwd_read_of(&r.ops_since(mark), &terminal2());
    let mark = r.mark();
    let gone = "terminal#2 closed while its directory was read; no terminal was opened".to_string();
    r.host.post(HostEvent::Terminal(TermEvent::Cwd {
        win: WIN,
        module: terminal2(),
        read,
        dir: Err(gone.clone()),
    }));
    assert_eq!(r.refusals_since(mark), vec![gone]);
    assert!(!r.notes().iter().any(|line| line.contains("BUG")), "{:?}", r.notes());
}

/// Two directory reads of one terminal may come back in either order: each answer makes its own split's terminal,
/// along that split's axis, and a late refusal refuses its own split, never the other.
#[test]
fn each_directory_read_answers_its_own_split_whatever_order_they_come_back_in() {
    let r = driven(three(terminal()));
    let split = |axis: Axis| {
        move |c: &mut Controller| {
            let (reads, keymap, setup) = c.module_parts(WIN);
            let window = c.window_mut(WIN).unwrap();
            let cx = Cx {
                reads: &*reads,
                keymap: &keymap,
                setup: &setup,
            };
            let mut batch = Batch::default();
            place(window, &cx, WIN, &terminal(), axis, SplitCwd::Pane, &mut batch);
            batch
        }
    };
    let mark = r.mark();
    r.driver.test_batch(split(Axis::Row));
    let right = cwd_read_of(&r.ops_since(mark), &terminal());
    let mark = r.mark();
    r.driver.test_batch(split(Axis::Column));
    let below = cwd_read_of(&r.ops_since(mark), &terminal());
    assert_ne!(right, below, "each read has its own number");

    r.host.post(HostEvent::Terminal(TermEvent::Cwd {
        win: WIN,
        module: terminal(),
        read: below,
        dir: Ok(PathBuf::from("/work/below")),
    }));
    {
        let layout = r.layout.borrow();
        assert!(layout.is_shown(&terminal2()));
        assert_eq!(
            crate::layout::navigate(&layout, &terminal(), Direction::Down, &Frame::new(GRID, 1)),
            Nav::Module(terminal2()),
            "placed along the axis of the split the answer belongs to"
        );
    }
    let mark = r.mark();
    let gone = "terminal closed while its directory was read; no terminal was opened".to_string();
    r.host.post(HostEvent::Terminal(TermEvent::Cwd {
        win: WIN,
        module: terminal(),
        read: right,
        dir: Err(gone.clone()),
    }));
    assert_eq!(
        r.refusals_since(mark),
        vec![gone],
        "the earlier split is the one refused"
    );
    assert!(!r.notes().iter().any(|line| line.contains("BUG")), "{:?}", r.notes());
}

/// A covered chat revealed: the scratch float over it that does not hold the keys is stowed, the keys staying where
/// they are, and the answer gate allows answers; a plain float stays, and the gate gives its refusal's text.
#[test]
fn a_reveal_stows_the_scratch_floats_over_the_chat_and_names_what_stays() {
    let notes = ModuleId::lua("notes");
    let r = driven(agent_under(&notes, true, editor()));
    r.host.post(HostEvent::Chat(ChatEvent::Covered {
        win: WIN,
        view: ViewId::BASE,
    }));
    assert_eq!(
        r.driver
            .query_or(Err(crate::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()), |c| c
                .answer_gate(WIN, ViewId::BASE)),
        Ok(())
    );
    assert!(!r.layout.borrow().is_shown(&notes), "stowed");
    assert_eq!(r.layout.borrow().focus(), &editor());

    let r = driven(agent_under(&notes, false, editor()));
    let sink = super::super::key_text(&Keymap::defaults(), &Action::FloatToggle);
    r.host.post(HostEvent::Chat(ChatEvent::Covered {
        win: WIN,
        view: ViewId::BASE,
    }));
    assert_eq!(
        r.driver
            .query_or(Err(crate::wm::feedback::LAYOUT_CHANGING_TEXT.to_string()), |c| c
                .answer_gate(WIN, ViewId::BASE)),
        Err(feedback::covered_text("agent", "notes", &sink, false))
    );
    assert!(r.layout.borrow().is_shown(&notes), "a reveal never sinks a plain float");
}

fn gate(r: &Rig, view: ViewId) -> Result<(), String> {
    r.driver
        .query_or(Err(feedback::LAYOUT_CHANGING_TEXT.to_string()), move |c| {
            c.answer_gate(WIN, view)
        })
}

/// A page's answer arrives by message, not by key, so holding the keys is not enough: a chooser over the view
/// refuses it as a float over the view does, and the refusal is drawn the same way.
#[test]
fn a_card_answer_is_refused_while_the_chooser_covers_the_view() {
    let r = driven(three(editor()));
    assert_eq!(gate(&r, ViewId::BASE), Ok(()));
    r.driver.run(WIN, Action::ProjectChoose, Origin::Lua);
    let text = "close the project chooser to answer".to_string();
    assert_eq!(gate(&r, ViewId::BASE), Err(text.clone()));
    r.host.post(HostEvent::AnswerRefused {
        win: WIN,
        text: text.clone(),
    });
    assert_eq!(r.chrome().toast.map(|toast| toast.text), Some(text));
}

/// Nothing about the refusal ends the chooser or the card: however the chooser closes, the same answer is taken.
#[test]
fn the_card_stays_and_can_be_answered_once_it_closes() {
    let r = driven(three(editor()));
    r.driver.run(WIN, Action::ProjectChoose, Origin::Lua);
    assert!(gate(&r, ViewId::BASE).is_err());
    assert!(r.chrome().project_chooser.is_some(), "a refused answer leaves it open");
    r.host.post(HostEvent::Project(ProjectEvent::Line {
        win: WIN,
        ev: crate::project_chooser::LineEv::Escape,
    }));
    assert!(r.chrome().project_chooser.is_none());
    assert_eq!(gate(&r, ViewId::BASE), Ok(()));

    r.driver.run(WIN, Action::ProjectChoose, Origin::Lua);
    assert!(gate(&r, ViewId::BASE).is_err(), "and it covers again when it is back");
}

/// With no chooser up every answer is judged as before, and with one up each earlier check still speaks first: the
/// float over the chat is what the person has to clear, and a view the window does not hold stays that refusal.
#[test]
fn the_gate_is_as_it_was_with_no_chooser_up() {
    let notes = ModuleId::lua("notes");
    let keymap = Keymap::defaults();
    let sink = super::super::key_text(&keymap, &Action::FloatToggle);
    let covered = feedback::covered_text("agent", "notes", &sink, false);

    let r = driven(agent_under(&notes, false, editor()));
    assert_eq!(gate(&r, ViewId::BASE), Err(covered.clone()));
    r.driver.run(WIN, Action::ProjectChoose, Origin::Lua);
    assert_eq!(gate(&r, ViewId::BASE), Err(covered), "the float's refusal comes first");

    let clear = driven(three(editor()));
    let absent = ViewId::of_module(&agent2()).unwrap();
    let away = gate(&clear, absent);
    assert!(away.is_err());
    clear.driver.run(WIN, Action::ProjectChoose, Origin::Lua);
    assert_eq!(gate(&clear, absent), away, "a view not held is no cover's business");
    {
        let _writing = clear.layout.borrow_mut();
        assert_eq!(
            gate(&clear, ViewId::BASE),
            Err(feedback::LAYOUT_CHANGING_TEXT.to_string()),
            "a layout being written cannot be judged"
        );
    }
}

/// What a covered chat's reveal stows: every scratch float but the one with the keys; a plain float is never
/// stowed and keeps the chat covered.
#[test]
fn a_reveal_stows_scratch_floats_without_the_keys_only() {
    let (scratch, plain, editor) = (ModuleId::lua("s"), ModuleId::lua("p"), editor());
    assert_eq!(
        covered_reveal(&[(scratch.clone(), true), (plain.clone(), false)], &editor),
        (vec![scratch.clone()], true)
    );
    assert_eq!(
        covered_reveal(&[(scratch.clone(), true), (plain.clone(), false)], &scratch),
        (vec![], true)
    );
    assert_eq!(
        covered_reveal(&[(scratch.clone(), true), (plain.clone(), true)], &editor),
        (vec![scratch, plain], false)
    );
    assert_eq!(covered_reveal(&[], &editor), (vec![], false));
}

// --- Tests that came with the module verbs from the shell, under their names.

/// A killed numbered terminal, whose host the grid keeps, is not placed again by name; a live one
/// and every other kind are left to the grid.
#[test]
fn a_killed_instance_is_not_placed_by_name() {
    let id = |text: &str| ModuleId::parse(text).unwrap();
    assert_eq!(
        placeable_by_id(&id("terminal#2"), false),
        Err(LayoutError::NotInTree(id("terminal#2")))
    );
    assert_eq!(placeable_by_id(&id("terminal#2"), true), Ok(()));
    assert_eq!(placeable_by_id(&id("terminal"), true), Ok(()));
    assert_eq!(placeable_by_id(&id("agent"), false), Ok(()));
    assert_eq!(placeable_by_id(&id("editor"), false), Ok(()));
    assert_eq!(
        placeable_by_id(&id("agent#2"), false),
        Err(LayoutError::NotInTree(id("agent#2"))),
        "a removed chat view's page is idle"
    );
    assert_eq!(placeable_by_id(&id("agent#2"), true), Ok(()));
}

/// A call's own options decide where its one module opens; without them a rule for the module wins
/// over its kind's, and with neither the point's built-in default: right of the keys for a new
/// instance, below the editor for a terminal back after a kill.
#[test]
fn an_explicit_open_beats_the_rules_and_the_rules_beat_the_built_in() {
    use crate::layout::rules::{parse_at, At, RuleTarget};
    let t2 = ModuleId::parse("terminal#2").unwrap();
    let t3 = ModuleId::parse("terminal#3").unwrap();
    let at = |name: &str| OpenRule {
        at: parse_at(name),
        ..OpenRule::default()
    };
    let none = OpenRules::default();
    assert_eq!(
        open_rule(&none, &t2, OpenPoint::NewInstance, None).at,
        Some(At::Beside(Direction::Right))
    );
    assert_eq!(
        open_rule(&none, &ModuleId::parse("agent#2").unwrap(), OpenPoint::Break, None).at,
        Some(At::Beside(Direction::Right))
    );
    assert_eq!(
        open_rule(&none, &ModuleId::terminal(), OpenPoint::Reopen, None).at,
        Some(At::Placement(Placement::BelowEditor))
    );
    let mut rules = OpenRules::default();
    rules.add(RuleTarget::parse("terminal").unwrap(), at("below")).unwrap();
    rules.add(RuleTarget::parse("terminal#3").unwrap(), at("tab")).unwrap();
    assert_eq!(
        open_rule(&rules, &t2, OpenPoint::NewInstance, None),
        at("below"),
        "the kind's rule"
    );
    assert_eq!(
        open_rule(&rules, &t3, OpenPoint::NewInstance, None),
        at("tab"),
        "the module's own"
    );
    let explicit = OpenRule {
        focus: false,
        ..at("left")
    };
    assert_eq!(
        open_rule(&rules, &t3, OpenPoint::NewInstance, Some(&explicit)),
        explicit
    );
    assert_eq!(open_rule(&none, &t2, OpenPoint::NewInstance, Some(&explicit)), explicit);
}

/// The covered refusal names the chat, the topmost float over it, the key that clears it -- the
/// scratchpad's for a scratch float, `float.toggle`'s for another, or the action's name when
/// nothing binds it -- and that it is pressed from the float.
#[test]
fn the_covered_refusal_names_the_float_and_the_key_that_clears_it() {
    assert_eq!(
        crate::wm::feedback::covered_text("agent", "terminal 2", "C-b M--", true),
        "agent is under terminal 2: C-b M-- from terminal 2 stows it"
    );
    assert_eq!(
        crate::wm::feedback::covered_text("agent", "terminal", "C-b M-S-Space", false),
        "agent is under terminal: C-b M-S-Space from terminal sinks it"
    );
    let keymap = crate::keymap::Keymap::defaults();
    let stow = crate::wm::verbs::key_text(&keymap, &Action::ScratchpadShow { module: None });
    match keymap.keys_for(&Action::ScratchpadShow { module: None }).first() {
        Some(key) => assert_eq!(stow, format!("{} {}", keymap.prefix().human(), key.human())),
        None => assert_eq!(stow, "scratchpad.show"),
    }
    let sink = crate::wm::verbs::key_text(&keymap, &Action::FloatToggle);
    match keymap.keys_for(&Action::FloatToggle).first() {
        Some(key) => assert_eq!(sink, format!("{} {}", keymap.prefix().human(), key.human())),
        None => assert_eq!(sink, "float.toggle"),
    }
    assert_eq!(
        crate::wm::verbs::key_text(&keymap, &Action::WorkspaceNew),
        "workspace.new",
        "an unbound action is named"
    );
}

/// The covered refusal names the view the answer came from, not the base chat.
#[test]
fn the_covered_refusal_names_the_view_and_the_float() {
    let title = crate::wm::titles::module_title(&ModuleId::parse("agent#2").unwrap(), &[]);
    assert_eq!(
        crate::wm::feedback::covered_text(&title, "terminal 2", "C-b M--", true),
        "agent 2 is under terminal 2: C-b M-- from terminal 2 stows it"
    );
}

/// `prefix a`'s instance: the chat view with the keys first, then the view with the oldest card when
/// the layout holds it, then the most recently used chat view.
#[test]
fn prefix_a_picks_the_view_with_the_keys_before_the_oldest_card() {
    use crate::layout::{Axis, Layout, Node};
    let id = |text: &str| ModuleId::parse(text).unwrap();
    let (two, three) = (id("agent#2"), id("agent#3"));
    let oldest = ViewId::of_module(&two);
    let tree = || {
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::split(Axis::Column, 0.5, Node::Leaf(two.clone()), Node::Leaf(three.clone())),
            ),
        )
    };
    let mut layout = Layout::new(tree(), three.clone()).unwrap();
    assert_eq!(
        chat_target(&layout, oldest),
        three,
        "keys in agent#3: the key hides that one"
    );
    layout.set_focus(&ModuleId::agent()).unwrap();
    assert_eq!(chat_target(&layout, oldest), ModuleId::agent(), "keys in the base view");
    layout.set_focus(&ModuleId::editor()).unwrap();
    assert_eq!(
        chat_target(&layout, oldest),
        two,
        "keys in the editor: the oldest card's view"
    );
    let mut without = Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(three.clone()),
            ),
        ),
        three.clone(),
    )
    .unwrap();
    without.set_focus(&ModuleId::editor()).unwrap();
    assert_eq!(
        chat_target(&without, oldest),
        crate::layout::module_target(&without, &ModuleId::agent()),
        "the card's view not in the layout: the most recently used chat view"
    );
    assert_eq!(
        chat_target(&without, None),
        crate::layout::module_target(&without, &ModuleId::agent())
    );
}

// --- Named scratch instances, a broken-out chat view, and the kill with no y/n.

fn terminal3() -> ModuleId {
    ModuleId::instance(ModuleKind::Terminal, 3).unwrap()
}

/// `scratchpad.show` named a terminal that does not exist yet: its surface is made, then it goes straight into the
/// scratchpad as a scratch float, shown and given the keys, and every other workspace holds it hidden. Its shell
/// starts once it shows.
#[test]
fn a_missing_scratchpad_instance_is_made_into_the_scratchpad_with_the_keys() {
    let r = driven(three(editor()));
    let mark = r.mark();
    r.make_scratch(&terminal3());
    let ops = r.ops_since(mark);
    let create = ops
        .iter()
        .position(|op| matches!(op, HostOp::CreateSurface { module, kind: SurfaceKind::Terminal { .. }, .. } if *module == terminal3()))
        .unwrap_or_else(|| panic!("its surface first: {ops:?}"));
    let grab = ops
        .iter()
        .position(|op| matches!(op, HostOp::Focus { to: FocusTo::Module(m), .. } if *m == terminal3()))
        .unwrap_or_else(|| panic!("it takes the keys: {ops:?}"));
    assert!(create < grab);
    assert!(ops.contains(&HostOp::Terminal(TermOp::Start {
        win: WIN,
        module: terminal3(),
    })));
    let layout = r.layout.borrow();
    assert!(
        layout.float_of(&terminal3()).is_some_and(|float| float.scratch),
        "a scratch float"
    );
    assert!(layout.is_shown(&terminal3()));
    drop(layout);
    assert!(r.stored_two().contains(&terminal3()) && !r.stored_two().is_shown(&terminal3()));
    assert!(r
        .notes()
        .iter()
        .any(|line| line == "[terminal] terminal#3: opened as a float for the scratchpad"));
    assert!(r.refusals_since(mark).is_empty());
}

/// A number another workspace holds, or one a later build's kept data names, makes nothing: two modules must never
/// share a name. Nor does a kind with no instances.
#[test]
fn a_scratchpad_instance_is_refused_for_a_number_already_held() {
    let r = driven(three(editor()));
    {
        let mut workspaces = r.workspaces.borrow_mut();
        let two = workspaces.stored_mut_of("two").unwrap();
        crate::layout::place(two, &terminal3(), &editor(), Axis::Row).unwrap();
    }
    let mark = r.mark();
    r.make_scratch(&terminal3());
    assert!(!r
        .ops_since(mark)
        .iter()
        .any(|op| matches!(op, HostOp::CreateSurface { .. })));
    assert_eq!(
        r.refusals_since(mark),
        vec!["terminal#3: another workspace holds it".to_string()]
    );
    let mark = r.mark();
    r.make_scratch(&ModuleId::lua("notes"));
    assert_eq!(
        r.refusals_since(mark),
        vec![feedback::not_in_window_text(&ModuleId::lua("notes"))]
    );
}

/// `tab.break` from the base view: a new chat view where the open rules put a broken-out tab, its first tab the base
/// view's active one rather than a new empty one, and given the keys once the tab is there.
#[test]
fn a_tab_break_makes_a_view_that_takes_the_base_views_tab() {
    let r = driven(three(agent()));
    r.host.set_can_break_out(Ok(()));
    r.native(&agent());
    let mark = r.mark();
    r.run(Action::TabBreak, Origin::Key { keycode: 0 });
    let ops = r.ops_since(mark);
    let view = ViewId::of_module(&agent2()).unwrap();
    assert!(
        !ops.contains(&HostOp::Chat(ChatOp::OpenTab { win: WIN, view })),
        "{ops:?}"
    );
    let tab = ops
        .iter()
        .position(|op| *op == HostOp::Chat(ChatOp::BreakOut { win: WIN, view }))
        .unwrap_or_else(|| panic!("the base view's tab broken out: {ops:?}"));
    let grab = ops
        .iter()
        .position(|op| matches!(op, HostOp::Focus { to: FocusTo::Module(m), .. } if *m == agent2()))
        .expect("it takes the keys");
    assert!(tab < grab);
    assert!(r.layout.borrow().is_shown(&agent2()));
    assert!(r
        .notes()
        .iter()
        .any(|line| line.starts_with("[chat] agent#2: opened where its open rule says (Break")));
}

/// A terminal whose shell ended by itself closes as `prefix x` then `y` closes it, with no y/n: hung up, out of every
/// workspace, its pane left idle for the next terminal of its number. The last module on screen is not killed here:
/// its close is the window's.
#[test]
fn a_kill_with_no_prompt_ends_the_terminal_as_a_yes_would() {
    let root = Node::split(
        Axis::Row,
        0.5,
        Node::Leaf(editor()),
        Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(terminal2())),
    );
    let r = driven(Layout::new(root, terminal2()).unwrap());
    let mark = r.mark();
    r.driver.kill_confirmed(WIN, &terminal2());
    let ops = r.ops_since(mark);
    assert_eq!(r.chrome().prompt, None, "nothing is asked");
    assert!(ops.contains(&HostOp::Terminal(TermOp::HangUp {
        win: WIN,
        module: terminal2(),
    })));
    assert!(ops.contains(&HostOp::DropSurface {
        win: WIN,
        module: terminal2(),
        why: DropWhy::Killed,
    }));
    assert!(!r.layout.borrow().contains(&terminal2()));
    assert!(!r.stored_two().contains(&terminal2()));

    let alone = driven(Layout::new(Node::Leaf(terminal()), terminal()).unwrap());
    let mark = alone.mark();
    alone.driver.kill_confirmed(WIN, &terminal());
    assert!(
        alone.layout.borrow().is_shown(&terminal()),
        "the window close is the caller's"
    );
    assert!(!alone
        .ops_since(mark)
        .iter()
        .any(|op| matches!(op, HostOp::Terminal(TermOp::HangUp { .. }))));
}

/// What a plugin's follow commands read: shown, and on screen too, which a zoom or an undrawn tab takes away.
#[test]
fn module_shown_reads_shown_and_on_screen() {
    let r = driven(three(editor()));
    assert_eq!(r.driver.query(|c| c.module_shown(WIN, &terminal())), (true, true));
    r.layout.borrow_mut().toggle_zoom(&editor());
    assert_eq!(
        r.driver.query(|c| c.module_shown(WIN, &terminal())),
        (true, false),
        "behind the zoom"
    );
    assert_eq!(
        r.driver.query(|c| c.module_shown(WinId(9), &terminal())),
        (false, false)
    );
    let _writing = r.layout.borrow_mut();
    assert_eq!(
        r.driver.query(|c| c.module_shown(WIN, &terminal())),
        (false, false),
        "a layout being written says neither"
    );
}

/// A chat view number killed before is asked for again when a new view takes it: the host takes the idle page back
/// with a fresh document, and only then does its first tab open there.
#[test]
fn a_chat_view_number_killed_before_is_asked_for_again() {
    let root = Node::split(
        Axis::Row,
        0.5,
        Node::Leaf(editor()),
        Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(agent2())),
    );
    let r = driven(Layout::new(root, agent2()).unwrap());
    r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
    r.window_key(key("y", K2));
    assert!(!r.layout.borrow().contains(&agent2()));
    let mark = r.mark();
    r.run(
        Action::InstanceNew {
            kind: ModuleKind::Agent,
        },
        Origin::Key { keycode: K1 },
    );
    let ops = r.ops_since(mark);
    let view = ViewId::of_module(&agent2()).unwrap();
    let create = ops
        .iter()
        .position(|op| matches!(op, HostOp::CreateSurface { module, .. } if *module == agent2()))
        .unwrap_or_else(|| panic!("its page asked for again: {ops:?}"));
    let tab = ops
        .iter()
        .position(|op| *op == HostOp::Chat(ChatOp::OpenTab { win: WIN, view }))
        .expect("its first tab");
    assert!(create < tab);
    assert!(r.layout.borrow().is_shown(&agent2()));
}

#[test]
fn killing_an_agent_view_with_background_shells_asks_and_names_them() {
    for chat in [agent(), agent2()] {
        let root = Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(chat.clone()));
        let r = driven(Layout::new(root, chat.clone()).unwrap());
        r.host.set_chat_kill_counts(
            ViewId::of_module(&chat).unwrap(),
            ChatKillCounts {
                background: 2,
                ..ChatKillCounts::default()
            },
        );
        r.run(Action::ModuleKill, Origin::Key { keycode: K1 });
        let prompt = r.chrome().prompt.unwrap();
        assert_eq!(
            prompt.text,
            format!(
                "kill-pane {}? 2 with background tasks (y/n)",
                if chat == agent() { "agent" } else { "agent 2" }
            )
        );
        assert!(r.layout.borrow().is_shown(&chat));
        r.window_key(key("n", K2));
        assert!(r.layout.borrow().is_shown(&chat));
    }
}

#[test]
fn new_instances_skip_the_other_windows_numbers_and_refuse_when_none_is_free() {
    use crate::wm::fake::FakePeerPort;
    let r = driven(three(editor()));
    r.driver
        .attach_peer(WIN, PeerId(2), WindowName::Second, FakePeerPort::new());
    let held = |last| {
        (2..=last)
            .map(|n| ModuleId::instance(ModuleKind::Terminal, n).unwrap())
            .collect()
    };
    r.host.post(HostEvent::Peer(PeerEvent::Held {
        peer: PeerId(2),
        ids: held(5),
    }));
    let mark = r.mark();
    r.run(
        Action::InstanceNew {
            kind: ModuleKind::Terminal,
        },
        Origin::Key { keycode: K1 },
    );
    assert!(r
        .ops_since(mark)
        .iter()
        .any(|op| matches!(op, HostOp::CreateSurface { module, .. } if module.to_string() == "terminal#6")));
    r.host.post(HostEvent::Peer(PeerEvent::Held {
        peer: PeerId(2),
        ids: held(9),
    }));
    let mark = r.mark();
    r.run(
        Action::InstanceNew {
            kind: ModuleKind::Terminal,
        },
        Origin::Key { keycode: K1 },
    );
    assert_eq!(
        r.refusals_since(mark),
        vec!["no free terminal number: the two windows share terminal#2 to terminal#9"]
    );
    r.driver.detach_peer(WIN, PeerId(2));
    let r = driven(three(editor()));
    for _ in 2..=9 {
        r.run(
            Action::InstanceNew {
                kind: ModuleKind::Terminal,
            },
            Origin::Key { keycode: K1 },
        );
    }
    let mark = r.mark();
    r.run(
        Action::InstanceNew {
            kind: ModuleKind::Terminal,
        },
        Origin::Key { keycode: K1 },
    );
    assert_eq!(r.refusals_since(mark), vec!["at most 9 terminals"]);
}

#[test]
fn a_named_scratchpad_instance_cannot_take_a_number_held_by_the_peer() {
    let r = driven(three(editor()));
    r.driver
        .attach_peer(WIN, PeerId(2), WindowName::Second, crate::wm::fake::FakePeerPort::new());
    r.host.post(HostEvent::Peer(PeerEvent::Held {
        peer: PeerId(2),
        ids: vec![terminal2()],
    }));
    let mark = r.mark();
    r.make_scratch(&terminal2());
    assert!(!r
        .ops_since(mark)
        .iter()
        .any(|op| matches!(op, HostOp::CreateSurface { .. })));
    assert_eq!(r.refusals_since(mark), vec!["terminal#2: the other window holds it"]);
}

/// A window 2 left without its editor window: the editor key asks whether to start it. `n` starts nothing, `y`
/// asks the host to start it and a refusal says why, and once the editor window is linked again the key raises it.
#[test]
fn the_editor_key_in_an_unlinked_window_2_asks_to_start_the_editor() {
    let layout = Layout::new(
        Node::split(Axis::Row, 0.5, Node::Leaf(agent()), Node::Leaf(terminal())),
        agent(),
    )
    .unwrap();
    let r = driven_as(
        layout,
        &[],
        WindowRole::Linked { side: LinkSide::Second },
        EditorPresence::None,
    );
    let start = HostOp::StartEditorWindow { win: WIN };

    r.run(Action::Module(editor()), Origin::Key { keycode: K1 });
    let prompt = r.chrome().prompt.expect("the editor key asks");
    assert_eq!(prompt.text, "start the editor? (y/n)");
    assert!(!prompt.closes_window, "the window stays whatever the answer");
    let mark = r.mark();
    assert_eq!(r.window_key(key("n", K2)), Verdict::Consumed);
    assert_eq!(r.chrome().prompt, None);
    assert!(!r.ops_since(mark).contains(&start), "n starts nothing");

    r.run(Action::Module(editor()), Origin::Key { keycode: K1 });
    let mark = r.mark();
    assert_eq!(r.window_key(key("y", K2)), Verdict::Consumed);
    assert_eq!(r.chrome().prompt, None);
    assert_eq!(
        r.ops_since(mark).iter().filter(|op| **op == start).count(),
        1,
        "y asks for the editor window once"
    );

    let mark = r.mark();
    r.host.post(HostEvent::Refused {
        what: OpTag::StartEditorWindow,
        why: "eitri could not be started".into(),
        surface: None,
    });
    assert_eq!(r.refusals_since(mark), vec!["eitri could not be started".to_string()]);

    r.driver
        .attach_peer(WIN, PeerId(1), WindowName::Second, crate::wm::fake::FakePeerPort::new());
    let mark = r.mark();
    r.run(Action::Module(editor()), Origin::Key { keycode: K1 });
    assert_eq!(r.chrome().prompt, None, "nothing is asked with the editor window there");
    let ops = r.ops_since(mark);
    assert!(ops.contains(&HostOp::RaisePeer {
        win: WIN,
        peer: PeerId(1)
    }));
    assert!(!ops.contains(&start));
}

/// A start-hidden plugin's module opens by the open rules, its own declared rule where none names it, with the
/// keys as the way it was opened asks: a module key gives them, a show from nvim leaves them, and anything
/// else follows the rule's own `focus`.
#[test]
fn a_plugin_module_opens_by_its_own_rule_with_the_keys_as_asked() {
    use crate::layout::rules::{At, OpenRule, OpenRules, RuleTarget};
    use crate::layout::{Direction, Placement};
    let id = ModuleId::lua("preview");
    let own = OpenRule {
        at: Some(At::Beside(Direction::Right)),
        focus: false,
        ..OpenRule::default()
    };
    let keymap = crate::keymap::Keymap::defaults();
    let with = |rules: OpenRules, own: Option<OpenRule>, take: Option<bool>| {
        let setup = ModuleSetup {
            rules,
            start_hidden: vec![(id.clone(), own)],
            ..ModuleSetup::default()
        };
        let cx = Cx {
            reads: &NoReads,
            keymap: &keymap,
            setup: &setup,
        };
        plugin_open_rule(&cx, &id, take)
    };
    let none = OpenRules::default;
    let rule = with(none(), Some(own.clone()), Some(true));
    assert_eq!((rule.at, rule.focus), (Some(At::Beside(Direction::Right)), true));
    let rule = with(none(), Some(own.clone()), Some(false));
    assert_eq!((rule.at, rule.focus), (Some(At::Beside(Direction::Right)), false));
    assert_eq!(with(none(), Some(own.clone()), None), own, "the rule's own focus");
    // No declared rule: a Lua panel's built-in place, the side.
    let rule = with(none(), None, None);
    assert_eq!(
        (rule.at, rule.focus),
        (Some(At::Placement(Placement::RightOfRoot)), true)
    );

    let rules = || {
        let mut rules = OpenRules::default();
        rules
            .add(
                RuleTarget::Module(id.clone()),
                OpenRule {
                    at: Some(At::Beside(Direction::Down)),
                    focus: true,
                    ..OpenRule::default()
                },
            )
            .unwrap();
        rules
    };
    let rule = with(rules(), Some(own.clone()), None);
    assert_eq!(
        (rule.at, rule.focus),
        (Some(At::Beside(Direction::Down)), true),
        "a rule for the module wins"
    );
    let rule = with(rules(), Some(own), Some(false));
    assert_eq!((rule.at, rule.focus), (Some(At::Beside(Direction::Down)), false));
}

fn terminal_n(n: u8) -> ModuleId {
    ModuleId::instance(ModuleKind::Terminal, n).unwrap()
}

#[test]
fn attaching_a_second_peer_keeps_the_first() {
    use crate::wm::fake::FakePeerPort;
    let r = driven(three(editor()));
    r.driver
        .attach_peer(WIN, PeerId(2), WindowName::Second, FakePeerPort::new());
    r.driver
        .attach_peer(WIN, PeerId(16), WindowName::Chat(1), FakePeerPort::new());
    r.host.post(HostEvent::Peer(PeerEvent::Held {
        peer: PeerId(2),
        ids: vec![terminal_n(2)],
    }));
    let ids = |r: &Rig| {
        r.driver.query(|c| {
            c.window(WIN)
                .map(|w| w.peers().map(|(id, _)| *id).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    };
    assert_eq!(ids(&r), vec![PeerId(2), PeerId(16)]);
    assert_eq!(
        r.driver.query(|c| c.window(WIN).and_then(|w| w.second_peer())),
        Some(PeerId(2))
    );
    r.driver.detach_peer(WIN, PeerId(16));
    assert_eq!(ids(&r), vec![PeerId(2)]);
    assert_eq!(
        r.driver
            .query(|c| c.window(WIN).and_then(|w| w.held_by(&terminal_n(2)))),
        Some(PeerId(2)),
        "the first peer's held set is untouched"
    );
    r.driver
        .attach_peer(WIN, PeerId(2), WindowName::Second, FakePeerPort::new());
    assert_eq!(
        ids(&r),
        vec![PeerId(2)],
        "attaching an attached id replaces that peer alone"
    );
}

#[test]
fn a_module_held_by_any_peer_is_away() {
    use crate::wm::fake::FakePeerPort;
    let r = driven(three(editor()));
    r.driver
        .attach_peer(WIN, PeerId(2), WindowName::Second, FakePeerPort::new());
    r.driver
        .attach_peer(WIN, PeerId(16), WindowName::Chat(1), FakePeerPort::new());
    r.host.post(HostEvent::Peer(PeerEvent::Held {
        peer: PeerId(2),
        ids: vec![terminal_n(2)],
    }));
    r.host.post(HostEvent::Peer(PeerEvent::Held {
        peer: PeerId(16),
        ids: vec![terminal_n(3)],
    }));
    let held_by = |n| {
        r.driver
            .query(|c| c.window(WIN).and_then(|w| w.held_by(&terminal_n(n))))
    };
    assert_eq!(held_by(2), Some(PeerId(2)));
    assert_eq!(held_by(3), Some(PeerId(16)));
    assert_eq!(held_by(4), None);
    let mark = r.mark();
    r.run(
        Action::InstanceNew {
            kind: ModuleKind::Terminal,
        },
        Origin::Key { keycode: K1 },
    );
    assert!(
        r.ops_since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::CreateSurface { module, .. } if module.to_string() == "terminal#4")),
        "a new instance counts the held sets of every peer"
    );
}

#[test]
fn a_peer_going_clears_only_the_question_about_that_peer() {
    use crate::wm::fake::FakePeerPort;
    let r = driven(three(editor()));
    r.driver
        .attach_peer(WIN, PeerId(2), WindowName::Second, FakePeerPort::new());
    r.driver
        .attach_peer(WIN, PeerId(16), WindowName::Chat(1), FakePeerPort::new());
    r.driver.test_batch(|c| {
        c.window_mut(WIN).unwrap().modules.end_peer = Some(PeerId(2));
        Batch::default()
    });
    let asked = |r: &Rig| r.driver.query(|c| c.window(WIN).and_then(|w| w.modules.end_peer));
    r.host.post(HostEvent::Peer(PeerEvent::LinkClosed { peer: PeerId(16) }));
    assert_eq!(
        asked(&r),
        Some(PeerId(2)),
        "another window's end leaves the question alone"
    );
    r.host.post(HostEvent::Peer(PeerEvent::LinkClosed { peer: PeerId(2) }));
    assert_eq!(asked(&r), None);
}

fn open_request(id: u64) -> HostEvent {
    HostEvent::OpenFile {
        win: WIN,
        id,
        path: "/project/a.rs".into(),
        line: None,
        col: None,
        token: None,
    }
}

#[test]
fn an_open_in_a_window_that_does_not_own_the_editor_answers_busy() {
    for presence in [EditorPresence::None, EditorPresence::Proxy] {
        let r = driven_as(three(agent()), &[], WindowRole::Main, presence.clone());
        let mark = r.mark();
        r.driver.post(open_request(1));
        assert_eq!(
            r.ops_since(mark)
                .into_iter()
                .filter(|op| !matches!(op, HostOp::Chrome { .. }))
                .collect::<Vec<_>>(),
            [HostOp::OpenAnswer { id: 1, accepted: false }],
            "{presence:?}"
        );
    }
}

#[test]
fn an_open_in_a_workspace_that_lacks_the_editor_places_it_beside_the_keys_and_answers() {
    let r = driven(three(agent()));
    r.host.set_editor_running(true);
    r.driver.post(HostEvent::Editor(EditorEvent::Ready { win: WIN }));
    *r.layout.borrow_mut() = Layout::new(
        Node::split(Axis::Row, 0.5, Node::Leaf(agent()), Node::Leaf(terminal())),
        agent(),
    )
    .unwrap();
    let mark = r.mark();
    r.driver.post(open_request(1));
    let ops = r.ops_since(mark);
    assert!(r.layout.borrow().is_shown(&editor()));
    assert_eq!(r.layout.borrow().focus(), &editor());
    let present = ops.iter().position(|op| matches!(op, HostOp::Present { .. }));
    let focus = ops.iter().position(|op| matches!(op, HostOp::Focus { .. }));
    assert!(present.is_some() && present < focus, "{ops:#?}");
    assert!(ops.contains(&HostOp::OpenAnswer { id: 1, accepted: true }), "{ops:#?}");
    assert!(
        ops.iter()
            .any(|op| matches!(op, HostOp::Editor(EditorCall::Open { .. }))),
        "{ops:#?}"
    );
}

/// The Strips sidebar as shipped: its manifest starts it hidden with a default key, the window takes that key when
/// `init.lua` wrote none, and the prefix then that key shows the sidebar by its own rule with the keys on it, and
/// hides it again from there.
#[test]
fn the_strips_sidebar_starts_hidden_and_its_default_key_shows_then_hides_it() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/agents-strips/eitri-plugin.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let manifest = crate::plugin::manifest::parse_manifest(&text).unwrap();
    let table = manifest.module.clone().expect("a [module] table");
    assert_eq!(
        table.start,
        crate::plugin::manifest::ModuleStart::Hidden,
        "a panel you open"
    );
    let default = table.key.expect("a default key");
    assert_eq!(default, 'U');

    let strips = ModuleId::lua(&manifest.id);
    let keymap = Keymap::defaults();
    let (module_keys, refused, unused) =
        ModuleKeys::build_with_defaults(&[(strips.clone(), None)], &[(strips.clone(), default)], &keymap);
    assert!(refused.is_empty() && unused.is_empty(), "{refused:?} {unused:?}");
    assert_eq!(module_keys.module(default), Some(&strips));

    let layout = three(editor());
    let host = FakeHost::new();
    host.set_grid_size(GRID);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
    let mut hosted: Vec<_> = layout.modules().iter().map(decl).collect();
    hosted.push(SurfaceDecl {
        module: strips.clone(),
        kind: SurfaceKind::PluginPage {
            plugin: manifest.id.as_str().into(),
        },
        deferred: true,
    });
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
            workspaces: Rc::new(RefCell::new(Workspaces::single())),
            saver: None,
        },
    );
    controller.set_module_setup(
        WIN,
        ModuleSetup {
            start_hidden: vec![(
                strips.clone(),
                Some(crate::layout::rules::OpenRule {
                    at: table.at,
                    size: table.rule_size(),
                    focus: table.focus,
                    ..Default::default()
                }),
            )],
            ..ModuleSetup::default()
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
        workspaces: Rc::new(RefCell::new(Workspaces::single())),
        saids: RefCell::default(),
    };
    // The keys in the right-hand column: a sidebar still opens at the window's left edge, not beside them.
    r.native(&agent());
    assert!(!r.layout.borrow().contains(&strips), "not placed at start");

    let prefix_then_key = |r: &Rig| {
        assert_eq!(r.window_key(key("C-b", PREFIX)), Verdict::Consumed);
        assert_eq!(r.window_key(key(&default.to_string(), K1)), Verdict::Consumed);
    };
    prefix_then_key(&r);
    r.native(&strips);
    {
        let layout = r.layout.borrow();
        assert!(layout.is_shown(&strips), "shown by the key");
        assert_eq!(layout.focus(), &strips, "with the keys, whatever its rule's focus says");
        let root = layout.root();
        assert_eq!(root.layout, crate::layout::ConLayout::SplitH, "{root:?}");
        assert_eq!(
            root.children[0].node,
            Node::Leaf(strips.clone()),
            "the window's left edge: {root:?}"
        );
    }
    let applied = r.applied_since(0);
    assert!(
        applied.iter().any(
            |(op, done)| matches!(op, HostOp::Focus { to: FocusTo::Module(m), .. } if *m == strips)
                && *done == Applied::Done
        ),
        "the key gives it the keys: {applied:?}"
    );

    prefix_then_key(&r);
    let layout = r.layout.borrow();
    assert!(!layout.is_shown(&strips), "the same key hides it from there");
    assert_ne!(layout.focus(), &strips);
}
