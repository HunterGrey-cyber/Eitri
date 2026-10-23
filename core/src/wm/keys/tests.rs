use super::*;
use crate::alloc_count::bytes_allocated_by;
use crate::keymap::prefix::WorkspacePick;
use crate::keymap::{Action, KeySpec, Keymap};
use crate::layout::{Axis, Layout, ModuleKeys, Node, Size};
use crate::wm::chrome::ChromeModel;
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::verbs::rig::decl;
use crate::wm::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

const WIN: WinId = WinId::MAIN;
const GRID: Size = Size { w: 1200, h: 800 };
// Arbitrary, distinct hardware keycodes: a real keyboard never reuses one for two keys.
const PREFIX: u32 = 50;
const K1: u32 = 51;
const K2: u32 = 52;
const K3: u32 = 53;

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn agent() -> ModuleId {
    ModuleId::agent()
}
fn agent2() -> ModuleId {
    ModuleId::instance(ModuleKind::Agent, 2).unwrap()
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

fn escape_key(keycode: u32) -> KeyPress {
    KeyPress {
        keycode,
        press: Press::Escape,
        key: None,
        mods: Mods::default(),
        super_or_hyper: false,
        super_or_meta: false,
    }
}

/// `Super+<c>`: a key with no prefix name, the system modifier held.
fn super_key(c: char, keycode: u32) -> KeyPress {
    KeyPress {
        keycode,
        press: Press::Other,
        key: Some(KeyName::Char(c)),
        mods: Mods::default(),
        super_or_hyper: true,
        super_or_meta: true,
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
/// module, every module the layout shows mapped by the first apply, and the default keymap's prefix.
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
    let keymap = Keymap::defaults();
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
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
    fn mark(&self) -> usize {
        self.host.log().len()
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

    fn shell_verbs_since(&self, mark: usize) -> Vec<ShellVerb> {
        self.host.log()[mark..]
            .iter()
            .filter_map(|logged| match logged {
                Logged::ShellVerb(verb) => Some(verb.clone()),
                _ => None,
            })
            .collect()
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

    fn window_key(&self, press: KeyPress) -> Verdict {
        self.driver.key(WIN, &KeyStage::Window, press)
    }

    fn module_key(&self, module: &ModuleId, press: KeyPress) -> Verdict {
        self.driver.key(WIN, &KeyStage::Module(module.clone()), press)
    }

    /// The prefix pressed and released: armed.
    fn arm(&self) {
        assert_eq!(self.window_key(key("C-b", PREFIX)), Verdict::Consumed);
        self.host.post(HostEvent::KeyReleased {
            win: WIN,
            keycode: PREFIX,
        });
    }

    /// `prefix <key>`, released.
    fn prefix_then(&self, text: &str, keycode: u32) -> Verdict {
        self.arm();
        let verdict = self.window_key(key(text, keycode));
        self.host.post(HostEvent::KeyReleased { win: WIN, keycode });
        verdict
    }

    fn chrome(&self) -> ChromeModel {
        self.driver.query(|c| c.chrome(WIN))
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }
}

fn chrome_ops(ops: &[HostOp]) -> usize {
    ops.iter().filter(|op| matches!(op, HostOp::Chrome { .. })).count()
}

fn focus_to(ops: &[HostOp]) -> Vec<ModuleId> {
    ops.iter()
        .filter_map(|op| match op {
            HostOp::Focus {
                to: FocusTo::Module(m), ..
            } => Some(m.clone()),
            _ => None,
        })
        .collect()
}

// --- The pure rules moved from the shell.

/// `prefix Esc` is the prefix's own key, told to the window (which resets a container selection with it); a bare
/// `Esc` passes to the module with the keys and is not.
#[test]
fn only_the_prefixs_own_escape_is_told() {
    let t = std::time::Instant::now();
    let keymap = Keymap::defaults();
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    let mut p = Prefix::new(Rc::new(keymap), Rc::new(module_keys));
    assert_eq!(p.press(key("C-b", PREFIX).press, PREFIX, t), Outcome::Swallow);
    p.release(PREFIX);
    let was = p.waiting();
    assert_eq!(
        p.press(Press::Escape, K1, t),
        Outcome::Swallow,
        "the armed prefix takes it"
    );
    assert!(prefix_escape(true, was));
    assert_eq!(p.waiting(), Waiting::No);
    p.release(K1);
    let was = p.waiting();
    assert_eq!(
        p.press(Press::Escape, K1, t),
        Outcome::Pass,
        "a bare Esc is the module's"
    );
    assert!(!prefix_escape(true, was));
    assert!(
        !prefix_escape(false, Waiting::Command),
        "another key while armed is not Esc"
    );
}

#[test]
fn control_with_hjkl_moves_unless_shift_alt_super_or_meta_is_held() {
    assert_eq!(nav_direction(&key("C-h", K1)), Some(Direction::Left));
    assert_eq!(nav_direction(&key("C-j", K1)), Some(Direction::Down));
    assert_eq!(nav_direction(&key("C-k", K1)), Some(Direction::Up));
    assert_eq!(nav_direction(&key("C-l", K1)), Some(Direction::Right));
    assert_eq!(nav_direction(&key("k", K1)), None, "plain k is a letter");
    assert_eq!(nav_direction(&key("C-a", K1)), None, "Ctrl+a is the prefix's");
    let ctrl = Mods {
        ctrl: true,
        ..Mods::default()
    };
    // CapsLock delivers the capital with Control alone held; the prefix reads it lowercase.
    let capslock = KeyPress {
        keycode: K1,
        press: Press::Key(KeySpec::char('j', true, false)),
        key: Some(KeyName::Char('J')),
        mods: ctrl,
        super_or_hyper: false,
        super_or_meta: false,
    };
    assert_eq!(
        nav_direction(&capslock),
        Some(Direction::Down),
        "CapsLock is not a modifier here"
    );
    let shifted = KeyPress {
        press: Press::Other,
        mods: Mods { shift: true, ..ctrl },
        key: Some(KeyName::Char('K')),
        ..capslock.clone()
    };
    assert_eq!(nav_direction(&shifted), None, "Ctrl+Shift+K is the page's");
    let alt = KeyPress {
        press: Press::Key(KeySpec::char('j', true, true)),
        mods: Mods { alt: true, ..ctrl },
        key: Some(KeyName::Char('j')),
        ..capslock.clone()
    };
    assert_eq!(nav_direction(&alt), None, "Ctrl+Alt+j is the page's");
    let with_super = KeyPress {
        press: Press::Other,
        super_or_hyper: true,
        super_or_meta: true,
        key: Some(KeyName::Char('j')),
        ..capslock.clone()
    };
    assert_eq!(nav_direction(&with_super), None, "Super held: the page's");
    // Meta shares Super's flag here; the key has no prefix name either way.
    let meta = KeyPress {
        press: Press::Other,
        key: Some(KeyName::Char('j')),
        super_or_meta: true,
        ..capslock.clone()
    };
    assert_eq!(nav_direction(&meta), None, "Meta held: the page's");
    // Hyper alone gives the press no prefix name and is withheld from a page, but the chord still moves.
    let hyper = KeyPress {
        press: Press::Other,
        key: Some(KeyName::Char('j')),
        super_or_hyper: true,
        super_or_meta: false,
        ..capslock
    };
    assert_eq!(
        nav_direction(&hyper),
        Some(Direction::Down),
        "Hyper alone: still a move"
    );
}

#[test]
fn a_chord_with_nowhere_to_go_is_swallowed_only_where_the_module_says_so() {
    assert_eq!(nav_outcome(true, false), NavOutcome::Stop);
    assert_eq!(nav_outcome(true, true), NavOutcome::Stop);
    assert_eq!(nav_outcome(false, true), NavOutcome::Stop);
    assert_eq!(nav_outcome(false, false), NavOutcome::LetThrough);
}

#[test]
fn ctrl_a_ctrl_a_reaches_the_terminal_only_when_it_holds_the_keys() {
    assert_eq!(literal_target(Some(ModuleKind::Terminal)), LiteralTarget::Terminal);
    assert_eq!(
        literal_target(Some(ModuleKind::LuaWebview)),
        LiteralTarget::Neither,
        "a Lua panel, bottom or not"
    );
    assert_eq!(literal_target(Some(ModuleKind::Editor)), LiteralTarget::Editor);
    assert_eq!(literal_target(Some(ModuleKind::Agent)), LiteralTarget::Panel);
    assert_eq!(literal_target(Some(ModuleKind::Canvas)), LiteralTarget::Neither);
    assert_eq!(literal_target(None), LiteralTarget::Neither, "the top bar");
}

#[test]
fn copy_mode_entry_is_exactly_holding_the_keys() {
    assert!(copy_mode_entry(Some(ModuleKind::Terminal)));
    assert!(!copy_mode_entry(Some(ModuleKind::Editor)));
    assert!(!copy_mode_entry(Some(ModuleKind::Agent)));
    assert!(!copy_mode_entry(Some(ModuleKind::LuaWebview)));
    assert!(!copy_mode_entry(Some(ModuleKind::Canvas)));
    assert!(!copy_mode_entry(None), "the top bar");
}

// --- The module stage.

#[test]
fn ctrl_l_at_the_right_edge_of_a_lua_page_is_swallowed_and_of_the_chat_let_through() {
    let r = driven(pair(editor(), lua("notes"), lua("notes")));
    let mark = r.mark();
    assert_eq!(r.module_key(&lua("notes"), key("C-l", K1)), Verdict::Consumed);
    assert!(
        focus_to(&r.ops_since(mark)).is_empty(),
        "nothing that way: nothing moves"
    );

    let r = driven(pair(editor(), agent(), agent()));
    let mark = r.mark();
    assert_eq!(
        r.module_key(&agent(), key("C-l", K1)),
        Verdict::LetThrough,
        "the chat's own keys may use it at an edge"
    );
    assert!(focus_to(&r.ops_since(mark)).is_empty());
    assert_eq!(
        r.module_key(&agent(), key("C-h", K1)),
        Verdict::Consumed,
        "a chord that moves ends at the host"
    );
    assert_eq!(focus_to(&r.ops_since(mark)), vec![editor()]);
}

#[test]
fn a_super_chord_never_reaches_a_web_module() {
    let layout = Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::split(Axis::Column, 0.5, leaf(editor()), leaf(terminal())),
            Node::split(
                Axis::Column,
                0.5,
                leaf(agent()),
                Node::split(Axis::Column, 0.5, leaf(lua("notes")), leaf(lua("md"))),
            ),
        ),
        editor(),
    )
    .unwrap();
    let r = driven(layout);
    for web in [agent(), lua("notes"), lua("md")] {
        assert_eq!(r.module_key(&web, super_key('a', K1)), Verdict::Consumed, "{web}");
    }
    for native in [terminal(), editor()] {
        assert_eq!(
            r.module_key(&native, super_key('a', K1)),
            Verdict::PassThrough,
            "{native}"
        );
    }
    assert_eq!(
        r.module_key(&agent(), key("a", K1)),
        Verdict::PassThrough,
        "a plain key is the page's"
    );
}

#[test]
fn the_chat_claims_its_chord_and_gets_it_as_a_nav_key() {
    let r = driven(pair(editor(), agent(), agent()));
    r.host.set_chat_claims(ViewId::BASE, Direction::Left, true);
    let mark = r.mark();
    assert_eq!(r.module_key(&agent(), key("C-h", K1)), Verdict::Consumed);
    let ops = r.ops_since(mark);
    assert_eq!(
        ops,
        vec![HostOp::Chat(ChatOp::NavKey {
            win: WIN,
            view: ViewId::BASE,
            direction: Direction::Left,
        })],
        "the page gets the chord, and the keys stay where they are"
    );
}

#[test]
fn the_editor_without_nvim_navigates_as_the_shell() {
    let r = driven(pair(editor(), agent(), editor()));
    r.host.set_editor_running(false);
    let mark = r.mark();
    assert_eq!(r.module_key(&editor(), key("C-l", K1)), Verdict::Consumed);
    assert_eq!(focus_to(&r.ops_since(mark)), vec![agent()]);
}

#[test]
fn the_editor_with_nvim_running_keeps_its_chords() {
    let r = driven(pair(editor(), agent(), editor()));
    r.host.set_editor_running(true);
    let mark = r.mark();
    assert_eq!(r.module_key(&editor(), key("C-l", K1)), Verdict::PassThrough);
    assert!(r.ops_since(mark).is_empty(), "nvim's navigator decides");
}

#[test]
fn the_terminal_gives_up_all_four_chords_even_with_nothing_that_way() {
    let r = driven(pair(editor(), terminal(), terminal()));
    let mark = r.mark();
    assert_eq!(r.module_key(&terminal(), key("C-l", K1)), Verdict::Consumed);
    assert!(focus_to(&r.ops_since(mark)).is_empty());
    assert_eq!(r.module_key(&terminal(), key("C-h", K1)), Verdict::Consumed);
    assert_eq!(focus_to(&r.ops_since(mark)), vec![editor()]);
    assert_eq!(r.module_key(&terminal(), key("C-a", K1)), Verdict::PassThrough);
}

#[test]
fn a_pass_through_key_allocates_nothing_in_either_stage_or_on_release() {
    let r = driven(three(editor()));
    r.host.set_editor_running(true);
    let plain = key("a", K1);
    let window_stage = KeyStage::Window;
    let module_stage = KeyStage::Module(agent());
    let editor_stage = KeyStage::Module(editor());
    let driver = r.driver.clone();
    let (verdict, bytes) = bytes_allocated_by(|| driver.key(WIN, &window_stage, plain.clone()));
    assert_eq!((verdict, bytes), (Verdict::PassThrough, 0), "the window stage");
    let (verdict, bytes) = bytes_allocated_by(|| driver.key(WIN, &module_stage, plain.clone()));
    assert_eq!((verdict, bytes), (Verdict::PassThrough, 0), "the module stage");
    let chord = key("C-l", K2);
    let (verdict, bytes) = bytes_allocated_by(|| driver.key(WIN, &editor_stage, chord));
    assert_eq!((verdict, bytes), (Verdict::PassThrough, 0), "nvim's own chord");
    let (watched, bytes) = bytes_allocated_by(|| driver.query(|c| c.watches_release(WIN, K1)));
    assert_eq!((watched, bytes), (false, 0), "a release nobody watches");
}

// --- The window stage and the prefix's commands.

#[test]
fn the_prefix_runs_its_command_through_the_runner_with_the_keycode() {
    let r = driven(three(agent()));
    let mark = r.mark();
    assert_eq!(r.prefix_then(&bound(&Action::Zoom), K1), Verdict::Consumed);
    assert_eq!(r.layout.borrow().zoomed(), Some(&agent()), "the tiling verb ran here");
    assert!(r.shell_verbs_since(mark).is_empty());
    assert!(
        r.ops_since(mark).contains(&HostOp::Unmap {
            win: WIN,
            module: editor()
        }),
        "the zoom's own batch: {:?}",
        r.ops_since(mark)
    );

    let mark = r.mark();
    assert_eq!(r.prefix_then(&bound(&Action::FloatToggle), K2), Verdict::Consumed);
    assert_eq!(
        r.shell_verbs_since(mark),
        vec![ShellVerb {
            win: WIN,
            call: ShellCall::Action(Action::FloatToggle),
            keycode: K2,
        }],
        "an unmoved family goes to the shell with the key that ran it"
    );
}

#[test]
fn a_split_key_and_a_chooser_pick_go_to_the_shell_until_their_families_move() {
    let keymap = Keymap::defaults();
    let split = keymap
        .bindings()
        .iter()
        .find(|b| matches!(b.action, Action::Split { axis: Axis::Row, .. }))
        .expect("a default split.right key")
        .clone();
    let Action::Split { cwd, .. } = split.action else {
        unreachable!()
    };
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    assert_eq!(
        module_keys.module('t'),
        Some(&terminal()),
        "the premise: t is the terminal's key"
    );

    let r = driven(three(editor()));
    let mark = r.mark();
    assert_eq!(r.prefix_then(&split.key.to_string(), K1), Verdict::Consumed);
    assert_eq!(r.chrome().prefix.waiting, Waiting::Module(Axis::Row));
    assert_eq!(r.window_key(key("t", K2)), Verdict::Consumed);
    assert_eq!(
        r.shell_verbs_since(mark),
        vec![ShellVerb {
            win: WIN,
            call: ShellCall::Place {
                module: terminal(),
                axis: Axis::Row,
                cwd,
            },
            keycode: K2,
        }]
    );
    assert_eq!(r.chrome().prefix.waiting, Waiting::No);

    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K2 });
    r.driver.test_batch(|c| c.await_workspace(WIN));
    assert_eq!(r.chrome().prefix.waiting, Waiting::Workspace { deleting: false });
    let mark = r.mark();
    assert_eq!(r.window_key(key("2", K3)), Verdict::Consumed);
    assert_eq!(
        r.shell_verbs_since(mark),
        vec![ShellVerb {
            win: WIN,
            call: ShellCall::WorkspacePick(WorkspacePick::Switch(1)),
            keycode: K3,
        }]
    );
    assert_eq!(r.chrome().prefix.waiting, Waiting::No);
}

#[test]
fn a_waiting_change_bumps_the_chrome_revision_and_announces_it() {
    let r = driven(three(editor()));
    let mark = r.mark();
    assert_eq!(r.window_key(key("C-b", PREFIX)), Verdict::Consumed);
    assert_eq!(r.chrome().prefix.waiting, Waiting::Command);
    let ops = r.ops_since(mark);
    let revs: Vec<u64> = ops
        .iter()
        .filter_map(|op| match op {
            HostOp::Chrome { rev, .. } => Some(*rev),
            _ => None,
        })
        .collect();
    assert_eq!(revs, vec![1], "the first change is the window's first revision");
    let mark = r.mark();
    assert_eq!(r.window_key(escape_key(K1)), Verdict::Consumed);
    assert_eq!(r.chrome().prefix.waiting, Waiting::No);
    assert_eq!(
        r.ops_since(mark),
        vec![HostOp::Chrome { win: WIN, rev: 2 }],
        "no selection to put back: the change alone"
    );
}

#[test]
fn the_prefixs_escape_puts_a_selected_container_back_on_the_module() {
    let r = driven(three(agent()));
    assert!(crate::layout::focus_parent(&mut r.layout.borrow_mut()));
    r.arm();
    let mark = r.mark();
    assert_eq!(r.window_key(escape_key(K1)), Verdict::Consumed);
    assert!(matches!(
        r.layout.borrow().selection(),
        crate::layout::Selection::Module
    ));
    assert!(r.ops_since(mark).contains(&HostOp::ChromeHooks { win: WIN }));
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K1 });
    let mark = r.mark();
    assert_eq!(
        r.window_key(escape_key(K1)),
        Verdict::PassThrough,
        "a bare Esc is the module's"
    );
    assert!(r.ops_since(mark).is_empty());
}

#[test]
fn select_number_shows_the_digits_waits_and_ends_on_its_timer() {
    let r = driven(three(editor()));
    let mark = r.mark();
    assert_eq!(r.prefix_then(&bound(&Action::SelectNumber), K1), Verdict::Consumed);
    let plan = crate::layout::number_plan(&r.layout.borrow());
    let ops = r.ops_since(mark);
    assert!(
        ops.contains(&HostOp::Hint(HintOp::ShowDigits(vec![
            (editor(), 1),
            (agent(), 2),
            (terminal(), 3)
        ]))),
        "{ops:?}"
    );
    let shown = ops
        .iter()
        .position(|op| matches!(op, HostOp::Hint(HintOp::ShowDigits(_))))
        .unwrap();
    let last_chrome = ops.iter().rposition(|op| matches!(op, HostOp::Chrome { .. })).unwrap();
    assert!(shown < last_chrome, "the digits go up before the wait is announced");
    assert_eq!(r.chrome().prefix.waiting, Waiting::Number);
    assert_eq!(r.chrome().prefix.plan, plan);

    let mark = r.mark();
    r.scheduler.advance(DISPLAY_PANES_TIME - Duration::from_millis(1));
    assert!(r.ops_since(mark).is_empty(), "nothing before the deadline");
    assert_eq!(r.chrome().prefix.waiting, Waiting::Number);
    r.scheduler.advance(Duration::from_millis(1));
    let ops = r.ops_since(mark);
    assert_eq!(ops[0], HostOp::Hint(HintOp::Hide), "{ops:?}");
    assert_eq!(chrome_ops(&ops), 1);
    assert_eq!(r.chrome().prefix.waiting, Waiting::No);
    assert!(r
        .notes()
        .contains(&"[prefix] armed=false waiting=No (no digit in time)".to_string()));
    let mark = r.mark();
    assert_eq!(
        r.window_key(key("2", K2)),
        Verdict::PassThrough,
        "a digit after the wait is the module's"
    );
    assert!(r.ops_since(mark).is_empty());
}

#[test]
fn a_digit_on_the_zoomed_module_keeps_the_zoom_and_arrives_only_if_the_grab_took() {
    let mut layout = three(editor());
    layout.toggle_zoom(&agent());
    layout.set_focus(&agent()).unwrap();
    let r = driven(layout);
    r.prefix_then(&bound(&Action::SelectNumber), K1);
    assert_eq!(
        r.chrome().prefix.plan,
        vec![('1', agent())],
        "zoomed, only the zoomed module is numbered"
    );
    let mark = r.mark();
    assert_eq!(r.window_key(key("1", K2)), Verdict::Consumed);
    let applied = r.applied_since(mark);
    let arrive = HostOp::Chat(ChatOp::Arrive {
        win: WIN,
        view: ViewId::BASE,
        card_first: false,
    });
    assert!(applied.contains(&(
        HostOp::Focus {
            win: WIN,
            to: FocusTo::Module(agent()),
            on_grab: vec![arrive.clone()],
        },
        Applied::Done
    )));
    assert!(
        applied.contains(&(arrive.clone(), Applied::Done)),
        "the grab took: it arrives"
    );
    assert_eq!(r.layout.borrow().zoomed(), Some(&agent()), "the zoom stays");
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K2 });

    // The grab refused: the surface is not on screen as far as the host knows.
    r.host.with_surface(WIN, agent(), decl(&agent()).kind, false);
    r.prefix_then(&bound(&Action::SelectNumber), K1);
    let mark = r.mark();
    assert_eq!(r.window_key(key("1", K3)), Verdict::Consumed);
    let applied = r.applied_since(mark);
    assert!(applied
        .iter()
        .any(|(op, applied)| matches!(op, HostOp::Focus { .. }) && *applied == Applied::Refused));
    assert!(
        !applied.iter().any(|(op, _)| *op == arrive),
        "no arrival without the keys: {applied:?}"
    );
    assert_eq!(r.layout.borrow().zoomed(), Some(&agent()));
}

#[test]
fn a_digit_on_another_module_lands_as_ctrl_hjkl_land() {
    let r = driven(three(editor()));
    r.prefix_then(&bound(&Action::SelectNumber), K1);
    let mark = r.mark();
    assert_eq!(r.window_key(key("2", K2)), Verdict::Consumed);
    let ops = r.ops_since(mark);
    assert_eq!(focus_to(&ops), vec![agent()]);
    assert!(ops.contains(&HostOp::Chat(ChatOp::Arrive {
        win: WIN,
        view: ViewId::BASE,
        card_first: false,
    })));
    assert!(ops.contains(&HostOp::Hint(HintOp::Hide)), "the digit ended the wait");
}

#[test]
fn a_digit_no_module_had_does_nothing() {
    let r = driven(three(editor()));
    r.prefix_then(&bound(&Action::SelectNumber), K1);
    let mark = r.mark();
    assert_eq!(r.window_key(key("7", K2)), Verdict::Consumed);
    assert!(focus_to(&r.ops_since(mark)).is_empty());
    assert!(r.notes().contains(&"[prefix] number 7: no module has it".to_string()));
}

#[test]
fn a_digit_for_a_module_gone_during_the_wait_moves_nothing() {
    let root = Node::split(
        Axis::Row,
        0.5,
        leaf(editor()),
        Node::split(Axis::Column, 0.5, leaf(agent()), leaf(agent2())),
    );
    let r = driven(Layout::new(root, editor()).unwrap());
    r.prefix_then(&bound(&Action::SelectNumber), K1);
    assert_eq!(
        r.chrome().prefix.plan[2],
        ('3', agent2()),
        "the premise: the plan names agent#2"
    );
    {
        let layout = r.layout.clone();
        r.host.on_shell_verb(move |_, _, _, _| {
            layout.borrow_mut().remove(&agent2()).unwrap();
        });
    }
    // An agent's close while the digits show, through an arm the shell still runs.
    r.driver.run(WIN, Action::ModuleKill, Origin::Lua);
    assert!(!r.layout.borrow().contains(&agent2()));
    assert_eq!(
        r.chrome().prefix.waiting,
        Waiting::Number,
        "the kill does not end the wait"
    );
    let mark = r.mark();
    assert_eq!(r.window_key(key("3", K2)), Verdict::Consumed);
    let ops = r.ops_since(mark);
    assert!(focus_to(&ops).is_empty(), "nothing gets the keys: {ops:?}");
    assert!(!ops.iter().any(|op| matches!(op, HostOp::Chat(ChatOp::Arrive { .. }))));
    assert!(ops.contains(&HostOp::Hint(HintOp::Hide)));
    assert_eq!(r.chrome().prefix.waiting, Waiting::No, "the wait is over");
    assert!(!r.notes().iter().any(|line| line.contains("BUG")), "{:?}", r.notes());
}

#[test]
fn losing_activation_or_a_click_during_a_wait_ends_it_and_hides_the_digits() {
    let r = driven(three(editor()));
    r.prefix_then(&bound(&Action::SelectNumber), K1);
    assert_eq!(r.chrome().prefix.waiting, Waiting::Number);
    let mark = r.mark();
    r.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    let ops = r.ops_since(mark);
    assert!(ops.contains(&HostOp::Hint(HintOp::Hide)), "{ops:?}");
    assert_eq!(chrome_ops(&ops), 1);
    assert_eq!(r.chrome().prefix.waiting, Waiting::No);

    r.host.post(HostEvent::Active { win: WIN, active: true });
    r.driver.test_batch(|c| c.await_workspace(WIN));
    assert_eq!(r.chrome().prefix.waiting, Waiting::Workspace { deleting: false });
    let mark = r.mark();
    r.host.post(HostEvent::Pointer {
        win: WIN,
        on: PointerTarget::Window,
        ev: PointerEv::Press {
            x: 3.0,
            y: 4.0,
            button: 1,
        },
    });
    let ops = r.ops_since(mark);
    assert_eq!(chrome_ops(&ops), 1, "{ops:?}");
    assert!(!ops.contains(&HostOp::Hint(HintOp::Hide)), "the chooser drew no digits");
    assert_eq!(r.chrome().prefix.waiting, Waiting::No, "the chooser's wait is over");

    let mark = r.mark();
    r.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    assert_eq!(chrome_ops(&r.ops_since(mark)), 0, "nothing waits: nothing to end");
}

#[test]
fn a_key_released_is_watched_only_for_the_prefix_and_the_command_key() {
    let r = driven(three(agent()));
    let watches = |keycode| r.driver.query(move |c| c.watches_release(WIN, keycode));
    assert!(!watches(PREFIX));
    assert_eq!(r.window_key(key("C-b", PREFIX)), Verdict::Consumed);
    assert!(watches(PREFIX), "the prefix's own key while armed");
    assert!(!watches(K1));
    r.host.post(HostEvent::KeyReleased {
        win: WIN,
        keycode: PREFIX,
    });
    assert!(!watches(PREFIX));
    assert_eq!(r.window_key(key(&bound(&Action::Zoom), K1)), Verdict::Consumed);
    assert!(watches(K1), "the key that ran a command");
    assert_eq!(
        r.window_key(key(&bound(&Action::Zoom), K1)),
        Verdict::Consumed,
        "its auto-repeat is not a new key"
    );
    assert_eq!(r.layout.borrow().zoomed(), Some(&agent()), "the repeat did not unzoom");
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K1 });
    assert!(!watches(K1));
    assert_eq!(r.window_key(key("a", K2)), Verdict::PassThrough);
    assert!(!watches(K2), "a key that passed is nobody's");
}

// --- The keys verbs.

#[test]
fn hint_toggles_through_an_op_with_the_keycode() {
    let r = driven(three(editor()));
    let mark = r.mark();
    assert_eq!(r.prefix_then(&bound(&Action::Hint), K2), Verdict::Consumed);
    assert_eq!(
        r.ops_since(mark)
            .into_iter()
            .filter(|op| matches!(op, HostOp::Hint(_)))
            .collect::<Vec<_>>(),
        vec![HostOp::Hint(HintOp::Toggle { keycode: K2 })]
    );
    assert!(r.shell_verbs_since(mark).is_empty());
}

#[test]
fn send_prefix_reaches_the_module_with_the_keys() {
    let prefix = *Keymap::defaults().prefix();
    let root = Node::split(
        Axis::Row,
        0.5,
        leaf(editor()),
        Node::split(
            Axis::Column,
            0.5,
            Node::split(Axis::Row, 0.5, leaf(agent()), leaf(agent2())),
            leaf(terminal()),
        ),
    );
    let r = driven(Layout::new(root, editor()).unwrap());
    let sent = |module: &ModuleId| {
        r.native(module);
        let mark = r.mark();
        r.driver.run(WIN, Action::SendPrefix, Origin::Key { keycode: K1 });
        r.ops_since(mark)
    };
    assert_eq!(
        sent(&editor()),
        vec![HostOp::Editor(EditorCall::SendKeys {
            win: WIN,
            keys: prefix.to_vim(),
        })]
    );
    assert_eq!(
        sent(&agent()),
        vec![HostOp::Chat(ChatOp::LiteralKey {
            win: WIN,
            view: ViewId::BASE,
            key: prefix,
        })]
    );
    assert_eq!(
        sent(&agent2()),
        vec![HostOp::Chat(ChatOp::LiteralKey {
            win: WIN,
            view: ViewId::new(2).unwrap(),
            key: prefix,
        })],
        "the chat view that holds the keys"
    );
    assert_eq!(
        sent(&terminal()),
        vec![HostOp::Terminal(TermOp::LiteralKey {
            win: WIN,
            module: terminal(),
            key: prefix,
        })]
    );
    let literal = KeySpec::char('a', true, false);
    r.native(&terminal());
    let mark = r.mark();
    r.driver.run(WIN, Action::SendKeys(literal), Origin::Lua);
    assert_eq!(
        r.ops_since(mark),
        vec![HostOp::Terminal(TermOp::LiteralKey {
            win: WIN,
            module: terminal(),
            key: literal,
        })],
        "send-keys hands its own key"
    );
}

#[test]
fn a_literal_with_the_keys_in_no_module_goes_nowhere() {
    let r = driven(three(editor()));
    r.host.post(HostEvent::NativeFocus {
        win: WIN,
        owner: Some(FocusOwner::TopBar),
        chain: FocusChain(vec![]),
        active: true,
        op_index: None,
    });
    let mark = r.mark();
    r.driver.run(WIN, Action::SendPrefix, Origin::Lua);
    assert!(r.ops_since(mark).is_empty());
}

#[test]
fn copy_mode_outside_a_terminal_refuses() {
    let r = driven(three(editor()));
    r.native(&editor());
    let mark = r.mark();
    r.driver.run(WIN, Action::CopyMode { up: false }, Origin::Lua);
    assert_eq!(
        r.ops_since(mark),
        vec![HostOp::Refuse {
            win: WIN,
            text: "prefix [ scrolls the terminal".into(),
        }],
        "the refusal alone"
    );
}

#[test]
fn copy_mode_carries_its_scroll_up_entry() {
    let r = driven(three(terminal()));
    r.native(&terminal());
    for up in [true, false] {
        let mark = r.mark();
        r.driver.run(WIN, Action::CopyMode { up }, Origin::Lua);
        assert_eq!(
            r.ops_since(mark),
            vec![HostOp::Terminal(TermOp::CopyMode {
                win: WIN,
                module: terminal(),
                up,
            })]
        );
    }
}

#[test]
fn a_window_before_its_config_passes_every_key() {
    let host = FakeHost::new();
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let layout = three(editor());
    controller.open_window(
        WIN,
        WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::Owned,
            title: "project".into(),
        },
        WindowParts {
            layout: Rc::new(RefCell::new(layout)),
            workspaces: Rc::new(RefCell::new(crate::layout::Workspaces::single())),
            saver: None,
        },
    );
    assert_eq!(
        controller.key(WIN, &KeyStage::Window, &key("C-b", PREFIX)),
        (Verdict::PassThrough, Batch::default())
    );
    let keymap = Keymap::defaults();
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
    assert_eq!(
        controller.key(WIN, &KeyStage::Window, &key("C-b", PREFIX)).0,
        Verdict::Consumed,
        "the config's prefix arms"
    );
}
