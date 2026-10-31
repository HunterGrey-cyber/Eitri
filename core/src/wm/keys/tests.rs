use super::*;
use crate::alloc_count::bytes_allocated_by;
use crate::keymap::prefix::REPEAT_TIME;
use crate::keymap::{Action, KeySpec, Keymap, KeymapOp};
use crate::layout::{Axis, Layout, ModuleKeys, Node, Size};
use crate::wm::chrome::ChromeModel;
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::prefix_panel::PrefixPanel;
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
    driven_with(layout, None)
}

/// [`driven`] with `keymap.which_key_delay` given to the controller before the window is made.
fn driven_with(layout: Layout, hint_delay: Option<Duration>) -> Rig {
    driven_keyed(layout, Keymap::defaults(), hint_delay)
}

/// [`driven_with`] over a given keymap.
fn driven_keyed(layout: Layout, keymap: Keymap, hint_delay: Option<Duration>) -> Rig {
    let host = FakeHost::new();
    host.set_grid_size(GRID);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    controller.set_keymap(WIN, Rc::new(keymap), Rc::new(module_keys));
    controller.set_hint_delay(WIN, hint_delay);
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
        self.driver.key(WIN, &KeyStage::Module(module), press)
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

    /// What the chrome was last told the prefix waits for.
    fn waiting(&self) -> Waiting {
        self.driver.query(|c| c.prefix_shown(WIN)).expect("the window").0
    }

    /// The digits the last `select.number` drew.
    fn plan(&self) -> Vec<(char, ModuleId)> {
        self.driver.query(|c| c.prefix_shown(WIN)).expect("the window").1
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }

    /// The hint panel the chrome shows now.
    fn panel(&self) -> Option<PrefixPanel> {
        self.chrome().prefix_panel
    }

    /// How many times the hint panel's field was derived.
    fn panel_derivations(&self) -> u64 {
        self.driver.query(|c| c.chrome_derivations(WIN)).prefix_panel
    }

    /// `Esc` pressed and released.
    fn escape(&self) -> Verdict {
        let verdict = self.window_key(escape_key(K3));
        self.host.post(HostEvent::KeyReleased { win: WIN, keycode: K3 });
        verdict
    }
}

const HINT_DELAY: Duration = Duration::from_millis(200);

fn split_right_key() -> String {
    Keymap::defaults()
        .bindings()
        .iter()
        .find(|b| matches!(b.action, Action::Split { axis: Axis::Row, .. }))
        .expect("a default split.right key")
        .key
        .to_string()
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
fn ctrl_hyper_l_at_the_chats_right_edge_is_let_through_as_every_move_there_is() {
    // Hyper alone hides nothing a move reads, so the chord is a move, and at the edge a move is the page's.
    let hyper_l = KeyPress {
        press: Press::Other,
        super_or_hyper: true,
        super_or_meta: false,
        ..key("C-l", K1)
    };
    let r = driven(pair(editor(), agent(), agent()));
    let mark = r.mark();
    assert_eq!(r.module_key(&agent(), hyper_l.clone()), Verdict::LetThrough);
    assert!(
        focus_to(&r.ops_since(mark)).is_empty(),
        "nothing that way: nothing moves"
    );
    let hyper_h = KeyPress {
        key: Some(KeyName::Char('h')),
        ..hyper_l
    };
    assert_eq!(
        r.module_key(&agent(), hyper_h),
        Verdict::Consumed,
        "a move that goes ends at the host"
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
    let r = driven_with(three(editor()), Some(HINT_DELAY));
    r.host.set_editor_running(true);
    let plain = key("a", K1);
    // Each stage is built inside the measured call, as a host builds it for every key: a stage that copied the
    // module's name would allocate there.
    let (agent, editor) = (agent(), editor());
    let driver = r.driver.clone();
    let (verdict, bytes) = bytes_allocated_by(|| driver.key(WIN, &KeyStage::Window, plain.clone()));
    assert_eq!((verdict, bytes), (Verdict::PassThrough, 0), "the window stage");
    let (verdict, bytes) = bytes_allocated_by(|| driver.key(WIN, &KeyStage::Module(&agent), plain.clone()));
    assert_eq!((verdict, bytes), (Verdict::PassThrough, 0), "the module stage");
    let chord = key("C-l", K2);
    let (verdict, bytes) = bytes_allocated_by(|| driver.key(WIN, &KeyStage::Module(&editor), chord));
    assert_eq!((verdict, bytes), (Verdict::PassThrough, 0), "nvim's own chord");
    let (watched, bytes) = bytes_allocated_by(|| driver.query(|c| c.watches_release(WIN, K1)));
    assert_eq!((watched, bytes), (false, 0), "a release nobody watches");
    let (watched, bytes) = bytes_allocated_by(|| driver.query(|c| c.watches_ctrl(WIN)));
    assert_eq!((watched, bytes), (false, 0), "a Ctrl release nobody watches");
}

// --- The window stage and the prefix's commands.

#[test]
fn the_prefix_runs_its_command_through_the_runner_with_the_keycode() {
    let r = driven(three(editor()));
    let mark = r.mark();
    assert_eq!(r.prefix_then(&bound(&Action::Zoom), K1), Verdict::Consumed);
    assert_eq!(r.layout.borrow().zoomed(), Some(&editor()), "the tiling verb ran here");
    assert!(
        r.ops_since(mark).contains(&HostOp::Unmap {
            win: WIN,
            module: agent()
        }),
        "the zoom's own batch: {:?}",
        r.ops_since(mark)
    );

    // Text size is decided by the same runner when a hardware key invokes it.
    r.driver.set_text_size(WIN, 14.0, 1.0);
    r.native(&editor());
    let action = Action::Text(crate::keymap::action::TextChange::Larger);
    let mark = r.mark();
    assert_eq!(r.prefix_then(&bound(&action), K2), Verdict::Consumed);
    let writes: Vec<_> = r
        .ops_since(mark)
        .into_iter()
        .filter(|op| {
            matches!(
                op,
                HostOp::Editor(EditorCall::SetScale { .. }) | HostOp::Chat(ChatOp::SetFontSize { .. })
            )
        })
        .collect();
    assert_eq!(
        writes,
        [
            HostOp::Editor(EditorCall::SetScale {
                win: WIN,
                scale: 1.1_f32 as f64
            }),
            HostOp::Chat(ChatOp::SetFontSize { win: WIN, px: 14.0 }),
        ]
    );
}

#[test]
fn a_split_key_places_here_and_a_chooser_pick_is_answered_here() {
    let keymap = Keymap::defaults();
    let split = keymap
        .bindings()
        .iter()
        .find(|b| matches!(b.action, Action::Split { axis: Axis::Row, .. }))
        .expect("a default split.right key")
        .clone();
    let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
    assert_eq!(
        module_keys.module('t'),
        Some(&terminal()),
        "the premise: t is the terminal's key"
    );

    let r = driven(three(editor()));
    assert_eq!(r.prefix_then(&split.key.to_string(), K1), Verdict::Consumed);
    assert_eq!(r.waiting(), Waiting::Module(Axis::Row));
    assert_eq!(r.window_key(key("t", K2)), Verdict::Consumed);
    {
        let layout = r.layout.borrow();
        assert_eq!(layout.focus(), &terminal(), "the placed module takes the keys");
        assert_eq!(
            crate::layout::navigate(
                &layout,
                &editor(),
                Direction::Right,
                &crate::layout::Frame::new(GRID, 1)
            ),
            crate::layout::Nav::Module(terminal()),
            "the terminal is placed right of the module that had the keys"
        );
    }
    assert_eq!(r.waiting(), Waiting::No);

    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K2 });
    r.driver.test_batch(|c| c.await_workspace(WIN));
    assert_eq!(r.waiting(), Waiting::Workspace { deleting: false });
    assert_eq!(r.window_key(key("2", K3)), Verdict::Consumed);
    assert_eq!(r.chrome().toast.unwrap().text, "no workspace 2");
    assert!(r.notes().contains(&"[modules] refused: no workspace 2".into()));
    assert_eq!(r.waiting(), Waiting::No);
}

/// `prefix d` then a module key hides that module wherever the keys are: the keys stay put unless the hidden module
/// held them, and then go to its neighbour; `Esc` after `d` hides nothing.
#[test]
fn the_hide_key_hides_the_named_module_and_moves_the_keys_only_off_it() {
    let r = driven(three(editor()));
    let hide = bound(&Action::ModuleHideByKey);
    assert_eq!(hide, "d");
    assert_eq!(r.prefix_then(&hide, K1), Verdict::Consumed);
    assert_eq!(r.waiting(), Waiting::Hide);
    let mark = r.mark();
    assert_eq!(r.window_key(key("t", K2)), Verdict::Consumed);
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K2 });
    assert_eq!(r.waiting(), Waiting::No);
    {
        let layout = r.layout.borrow();
        assert!(!layout.is_shown(&terminal()), "the terminal is hidden");
        assert_eq!(layout.focus(), &editor(), "the keys stay where they were");
    }
    assert!(focus_to(&r.ops_since(mark)).is_empty(), "no module is given the keys");

    assert_eq!(r.prefix_then(&hide, K1), Verdict::Consumed);
    assert_eq!(r.window_key(escape_key(K2)), Verdict::Consumed);
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K2 });
    assert_eq!(r.waiting(), Waiting::No, "Esc ends the wait");
    assert!(r.layout.borrow().is_shown(&editor()) && r.layout.borrow().is_shown(&agent()));

    assert_eq!(r.prefix_then(&hide, K1), Verdict::Consumed);
    let mark = r.mark();
    assert_eq!(r.window_key(key("e", K2)), Verdict::Consumed);
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K2 });
    {
        let layout = r.layout.borrow();
        assert!(!layout.is_shown(&editor()), "the module with the keys is hidden");
        assert_eq!(layout.focus(), &agent(), "and its neighbour has the keys");
    }
    assert_eq!(focus_to(&r.ops_since(mark)), vec![agent()]);
}

/// While the hide key waits, the chrome's strip dims a kind by the instance its key would hide: the base terminal and
/// the base chat hidden, `terminal#2` and `agent#2` shown, so `t` and `a` are not dimmed, and `t` hides `terminal#2`.
#[test]
fn the_hide_keys_strip_dims_by_the_instance_the_key_hides() {
    use crate::layout::{Frame, ModuleDecl, Placement};
    use crate::wm::prefix_strip::StripPiece;
    let (t2, a2) = (
        ModuleId::parse("terminal#2").unwrap(),
        ModuleId::parse("agent#2").unwrap(),
    );
    let decl = |id: &ModuleId, placement| ModuleDecl {
        id: id.clone(),
        placement,
    };
    let mut layout = Layout::initial(&[
        decl(&terminal(), Placement::BelowEditor),
        decl(&t2, Placement::BelowEditor),
        decl(&a2, Placement::RightOfRoot),
    ])
    .unwrap();
    let frame = Frame::new(GRID, 1);
    layout.set_focus(&editor()).unwrap();
    crate::layout::hide(&mut layout, &terminal(), &frame).unwrap();
    crate::layout::hide(&mut layout, &agent(), &frame).unwrap();
    let r = driven(layout);
    assert_eq!(r.prefix_then(&bound(&Action::ModuleHideByKey), K1), Verdict::Consumed);
    let strip = r.chrome().prefix_strip.expect("the strip while the hide key waits");
    let run = |key: &str| {
        strip
            .iter()
            .find_map(|piece| match piece {
                StripPiece::Run { text, dimmed } if text.starts_with(&format!("{key} ")) => Some(*dimmed),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {key} run: {strip:?}"))
    };
    assert!(!run("t"), "t hides terminal#2, which is shown: {strip:?}");
    assert!(!run("a"), "a hides agent#2, which is shown: {strip:?}");
    assert_eq!(r.window_key(key("t", K2)), Verdict::Consumed);
    assert!(!r.layout.borrow().is_shown(&t2), "t hid terminal#2");
}

/// The hide key refuses, with the flash and a toast saying why: a module the layout does not hold, one hidden already,
/// the last module on screen, and any module while a container is selected.
#[test]
fn the_hide_key_refuses_what_it_cannot_hide() {
    let r = driven(pair(editor(), agent(), editor()));
    let hide = bound(&Action::ModuleHideByKey);
    let hide_then = |text: &str| {
        assert_eq!(r.prefix_then(&hide, K1), Verdict::Consumed);
        assert_eq!(r.window_key(key(text, K2)), Verdict::Consumed);
        r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K2 });
    };
    let toast = || r.chrome().toast.map(|toast| toast.text);

    hide_then("t");
    assert_eq!(toast().as_deref(), Some("module 'terminal' is not in the layout"));

    r.arm();
    assert_eq!(r.window_key(key(&bound(&Action::FocusParent), K3)), Verdict::Consumed);
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K3 });
    assert!(matches!(
        r.layout.borrow().selection(),
        crate::layout::Selection::Con(_)
    ));
    hide_then("a");
    assert_eq!(toast().as_deref(), Some("acts on one module: Ctrl+b Alt+d"));
    assert!(
        r.layout.borrow().is_shown(&agent()),
        "nothing is hidden under a container selection"
    );
    r.arm();
    assert_eq!(r.window_key(key(&bound(&Action::FocusChild), K3)), Verdict::Consumed);
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K3 });

    hide_then("a");
    assert!(!r.layout.borrow().is_shown(&agent()));
    hide_then("a");
    assert_eq!(toast().as_deref(), Some("agent is hidden already"));
    hide_then("e");
    assert_eq!(toast().as_deref(), Some("'editor' is the last module on screen"));
    assert!(r.layout.borrow().is_shown(&editor()));
}

#[test]
fn a_waiting_change_bumps_the_chrome_revision_and_announces_it() {
    let r = driven(three(editor()));
    let mark = r.mark();
    assert_eq!(r.window_key(key("C-b", PREFIX)), Verdict::Consumed);
    assert_eq!(r.waiting(), Waiting::Command);
    let ops = r.ops_since(mark);
    let revs: Vec<u64> = ops
        .iter()
        .filter_map(|op| match op {
            HostOp::Chrome { rev, .. } => Some(*rev),
            _ => None,
        })
        .collect();
    assert_eq!(revs.len(), 1, "one announcement for the change: {ops:?}");
    let mark = r.mark();
    assert_eq!(r.window_key(escape_key(K1)), Verdict::Consumed);
    assert_eq!(r.waiting(), Waiting::No);
    assert_eq!(
        r.ops_since(mark),
        vec![HostOp::Chrome {
            win: WIN,
            rev: revs[0] + 1
        }],
        "no selection to put back: the change alone, the next revision"
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
    assert!(chrome_ops(&r.ops_since(mark)) > 0, "the selection's text went");
    assert_eq!(r.chrome().top_bar.selection, None);
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
    assert_eq!(r.waiting(), Waiting::Number);
    assert_eq!(r.plan(), plan);

    let mark = r.mark();
    r.scheduler.advance(DISPLAY_PANES_TIME - Duration::from_millis(1));
    assert!(r.ops_since(mark).is_empty(), "nothing before the deadline");
    assert_eq!(r.waiting(), Waiting::Number);
    r.scheduler.advance(Duration::from_millis(1));
    let ops = r.ops_since(mark);
    assert_eq!(ops[0], HostOp::Hint(HintOp::Hide), "{ops:?}");
    assert_eq!(chrome_ops(&ops), 1);
    assert_eq!(r.waiting(), Waiting::No);
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
        r.plan(),
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
    assert_eq!(r.plan()[2], ('3', agent2()), "the premise: the plan names agent#2");
    // A module can leave the layout while the digit plan is waiting.
    r.layout.borrow_mut().remove(&agent2()).unwrap();
    r.driver.test_batch(|c| c.changed_in_test(WIN));
    assert!(!r.layout.borrow().contains(&agent2()));
    assert_eq!(
        r.waiting(),
        Waiting::Number,
        "removing the module does not end the wait"
    );
    let mark = r.mark();
    assert_eq!(r.window_key(key("3", K2)), Verdict::Consumed);
    let ops = r.ops_since(mark);
    assert!(focus_to(&ops).is_empty(), "nothing gets the keys: {ops:?}");
    assert!(!ops.iter().any(|op| matches!(op, HostOp::Chat(ChatOp::Arrive { .. }))));
    assert!(ops.contains(&HostOp::Hint(HintOp::Hide)));
    assert_eq!(r.waiting(), Waiting::No, "the wait is over");
    assert!(!r.notes().iter().any(|line| line.contains("BUG")), "{:?}", r.notes());
}

#[test]
fn losing_activation_or_a_click_during_a_wait_ends_it_and_hides_the_digits() {
    let r = driven(three(editor()));
    r.prefix_then(&bound(&Action::SelectNumber), K1);
    assert_eq!(r.waiting(), Waiting::Number);
    let mark = r.mark();
    r.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    let ops = r.ops_since(mark);
    assert!(ops.contains(&HostOp::Hint(HintOp::Hide)), "{ops:?}");
    assert_eq!(chrome_ops(&ops), 1);
    assert_eq!(r.waiting(), Waiting::No);

    r.host.post(HostEvent::Active { win: WIN, active: true });
    r.driver.test_batch(|c| c.await_workspace(WIN));
    assert_eq!(r.waiting(), Waiting::Workspace { deleting: false });
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
    assert_eq!(r.waiting(), Waiting::No, "the chooser's wait is over");

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

/// `prefix C-=` stays on the pane while Ctrl is down, however long, and the same chord goes on to the accelerator
/// once Ctrl went up and the repeat time ran out: a host hears of Ctrl only while a run waits on it.
#[test]
fn a_text_size_run_ends_through_the_reported_ctrl_release() {
    let r = driven(three(agent()));
    r.driver.set_text_size(WIN, 14.0, 1.0);
    r.native(&agent());
    let watches = || r.driver.query(|c| c.watches_ctrl(WIN));
    assert!(!watches());
    assert_eq!(r.prefix_then("C-=", K1), Verdict::Consumed);
    assert!(watches());
    r.scheduler.advance(Duration::from_secs(5));
    assert_eq!(r.window_key(key("C-=", K1)), Verdict::Consumed, "Ctrl is still down");
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K1 });
    assert_eq!(r.driver.query(|c| c.panel_scale_for_test(WIN)), Some(1.2));
    r.host.post(HostEvent::CtrlReleased { win: WIN });
    assert!(!watches());
    r.scheduler.advance(REPEAT_TIME + Duration::from_millis(1));
    assert_eq!(
        r.window_key(key("C-=", K1)),
        Verdict::PassThrough,
        "the accelerator's now"
    );
}

/// `Ctrl++` with nothing armed is the accelerators' step, run by the window stage.
#[test]
fn ctrl_plus_at_the_window_stage_moves_every_module() {
    let r = driven(three(editor()));
    r.driver.set_text_size(WIN, 14.0, 1.0);
    r.native(&editor());
    let mark = r.mark();
    assert_eq!(r.window_key(key("C-+", K1)), Verdict::Consumed);
    let ops = r.ops_since(mark);
    assert!(
        ops.contains(&HostOp::Editor(EditorCall::SetScale {
            win: WIN,
            scale: 1.1_f32 as f64
        })),
        "{ops:?}"
    );
    assert_eq!(r.driver.query(|c| c.panel_scale_for_test(WIN)), Some(1.1));
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
    let ops = r.ops_since(mark);
    assert!(
        ops.iter().all(|op| matches!(op, HostOp::Chrome { .. })),
        "the refusal moves nothing: {ops:#?}"
    );
    assert_eq!(chrome_ops(&ops), 1, "and announces the chrome it flashed");
    assert_eq!(
        r.chrome().toast.map(|toast| toast.text).as_deref(),
        Some("prefix [ scrolls the terminal")
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

// --- The hint panel.

#[test]
fn armed_with_delay_200_shows_no_panel_before_the_timer_and_the_panel_after() {
    let r = driven(three(editor()));
    r.driver.set_hint_delay(WIN, Some(HINT_DELAY));
    r.arm();
    assert_eq!(r.waiting(), Waiting::Command);
    assert_eq!(r.panel(), None, "nothing at once");
    r.scheduler.advance(HINT_DELAY - Duration::from_millis(1));
    assert_eq!(r.panel(), None, "nothing before the delay");
    let mark = r.mark();
    r.scheduler.advance(Duration::from_millis(1));
    let panel = r.panel().expect("the panel is up at the delay");
    assert_eq!(panel.heading, "Ctrl+b \u{2026}");
    assert!(panel.groups.iter().any(|group| group.title == "Modules"), "{panel:?}");
    assert_eq!(chrome_ops(&r.ops_since(mark)), 1, "announced once");
    assert_eq!(r.waiting(), Waiting::Command, "the wait goes on");
}

#[test]
fn a_key_before_the_timer_means_the_panel_is_never_shown() {
    let r = driven_with(three(editor()), Some(HINT_DELAY));
    r.arm();
    r.scheduler.advance(HINT_DELAY / 2);
    assert_eq!(r.window_key(key(&bound(&Action::Zoom), K1)), Verdict::Consumed);
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K1 });
    r.scheduler.advance(Duration::from_secs(5));
    assert_eq!(r.panel(), None);
    assert_eq!(r.panel_derivations(), 0, "a panel that never went up was never derived");
}

#[test]
fn an_older_arms_timer_after_a_rearm_does_nothing() {
    let r = driven_with(three(editor()), Some(HINT_DELAY));
    r.arm();
    let first: Vec<TimerToken> = r.scheduler.armed().into_iter().map(|(token, _)| token).collect();
    assert_eq!(first.len(), 1, "one timer for the wait");
    assert_eq!(r.escape(), Verdict::Consumed);
    r.scheduler.advance(HINT_DELAY / 2);
    r.arm();
    // The first wait's timer fires anyway, as a timer already queued would.
    r.host.post(HostEvent::Timer(first[0]));
    assert_eq!(r.panel(), None, "the timer of an earlier wait shows nothing");
    r.scheduler.advance(HINT_DELAY - Duration::from_millis(1));
    assert_eq!(r.panel(), None, "the new wait has its own full delay");
    r.scheduler.advance(Duration::from_millis(1));
    assert!(r.panel().is_some());
}

#[test]
fn esc_focus_loss_and_a_pointer_press_hide_it() {
    let r = driven_with(three(editor()), Some(HINT_DELAY));
    let up = |r: &Rig| {
        r.arm();
        r.scheduler.advance(HINT_DELAY);
        assert!(r.panel().is_some(), "the premise: the panel is up");
    };
    up(&r);
    assert_eq!(r.escape(), Verdict::Consumed);
    assert_eq!(r.panel(), None, "Esc");
    assert_eq!(r.waiting(), Waiting::No);

    up(&r);
    r.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    assert_eq!(r.panel(), None, "focus loss");
    r.host.post(HostEvent::Active { win: WIN, active: true });

    up(&r);
    r.host.post(HostEvent::Pointer {
        win: WIN,
        on: PointerTarget::Window,
        ev: PointerEv::Press {
            x: 3.0,
            y: 4.0,
            button: 1,
        },
    });
    assert_eq!(r.panel(), None, "a pointer press");
    r.scheduler.advance(Duration::from_secs(5));
    assert_eq!(r.panel(), None, "and no timer brings it back");
}

#[test]
fn repeat_mode_shows_nothing() {
    let r = driven_with(three(editor()), Some(HINT_DELAY));
    let repeating = Keymap::defaults()
        .bindings()
        .iter()
        .find(|b| b.repeatable)
        .expect("a default repeats")
        .key
        .to_string();
    r.prefix_then(&repeating, K1);
    assert_eq!(
        r.waiting(),
        Waiting::No,
        "repeat mode waits for nothing the chrome shows"
    );
    r.scheduler.advance(Duration::from_secs(5));
    assert_eq!(r.panel(), None);
}

#[test]
fn a_delay_set_before_the_window_is_made_still_shows_the_panel() {
    let r = driven_with(three(editor()), Some(HINT_DELAY));
    r.arm();
    r.scheduler.advance(HINT_DELAY);
    assert!(r.panel().is_some());
}

/// A window with nothing around it, to drive the keys' own functions.
fn bare_window(scheduler: &Rc<FakeScheduler>) -> Window {
    Window::new(
        WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::Owned,
            title: "project".into(),
        },
        WindowParts {
            layout: Rc::new(RefCell::new(three(editor()))),
            workspaces: Rc::new(RefCell::new(crate::layout::Workspaces::single())),
            saver: None,
        },
        Timers {
            scheduler: scheduler.clone(),
            next: Rc::new(std::cell::Cell::new(1)),
        },
    )
}

#[test]
fn a_wait_for_a_module_reached_straight_from_no_wait_arms_the_timer() {
    let scheduler = FakeScheduler::new();
    let mut w = bare_window(&scheduler);
    w.keys.hint_delay = Some(HINT_DELAY);
    let mut batch = Batch::default();
    waiting_changed(&mut w, Waiting::No, Waiting::Module(Axis::Row), &mut batch);
    let armed = scheduler.armed();
    assert_eq!(armed.len(), 1, "{armed:?}");
    assert!(!w.keys.hints_shown);
    assert!(
        !hint_timer_fired(&mut w, TimerToken(u64::MAX - 1)),
        "another window's timer is not its"
    );
    assert!(hint_timer_fired(&mut w, armed[0].0));
    assert!(w.keys.hints_shown, "the module wait shows the panel at the delay");
    waiting_changed(&mut w, Waiting::Module(Axis::Row), Waiting::No, &mut batch);
    assert!(!w.keys.hints_shown);

    // A wait that ends before the delay takes its timer down.
    let before = scheduler.armed().len();
    waiting_changed(&mut w, Waiting::No, Waiting::Module(Axis::Column), &mut batch);
    assert!(w.keys.hint_timer.is_some());
    waiting_changed(&mut w, Waiting::Module(Axis::Column), Waiting::No, &mut batch);
    assert!(w.keys.hint_timer.is_none());
    assert_eq!(
        scheduler.armed().len(),
        before,
        "the second wait's timer is gone from the scheduler"
    );
}

#[test]
fn after_a_split_key_the_panel_lists_the_module_keys() {
    let r = driven_with(three(editor()), Some(Duration::ZERO));
    r.arm();
    assert_eq!(r.panel().expect("shown at once").heading, "Ctrl+b \u{2026}");
    assert_eq!(r.window_key(key(&split_right_key(), K1)), Verdict::Consumed);
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K1 });
    assert_eq!(r.waiting(), Waiting::Module(Axis::Row));
    let panel = r.panel().expect("the panel stays up and switches");
    assert_eq!(panel.heading, "right of editor:");
    assert_eq!(panel.groups.len(), 1);
    assert_eq!(panel.groups[0].title, "Modules");
    assert!(panel.groups[0].rows.iter().any(|row| row.keys == "t"), "{panel:?}");
    assert_eq!(r.window_key(key("t", K2)), Verdict::Consumed);
    assert_eq!(r.panel(), None, "the placement ends the wait");

    // With a delay, the timer started by the prefix keeps running through the split key.
    let r = driven_with(three(editor()), Some(HINT_DELAY));
    r.arm();
    r.scheduler.advance(HINT_DELAY / 2);
    r.window_key(key(&split_right_key(), K1));
    r.host.post(HostEvent::KeyReleased { win: WIN, keycode: K1 });
    assert_eq!(r.panel(), None);
    r.scheduler.advance(HINT_DELAY / 2);
    assert_eq!(r.panel().expect("the first wait's delay").heading, "right of editor:");
}

#[test]
fn delay_off_never_derives_the_field() {
    let r = driven(three(editor()));
    r.arm();
    r.scheduler.advance(Duration::from_secs(10));
    r.escape();
    r.prefix_then(&bound(&Action::Zoom), K1);
    r.prefix_then(&split_right_key(), K2);
    assert_eq!(r.panel(), None);
    assert_eq!(r.panel_derivations(), 0);
    assert!(r.scheduler.armed().is_empty(), "no timer is armed with the panel off");
}

#[test]
fn delay_zero_shows_it_on_the_arming_key() {
    let r = driven_with(three(editor()), Some(Duration::ZERO));
    assert_eq!(r.window_key(key("C-b", PREFIX)), Verdict::Consumed);
    assert!(r.panel().is_some());
    assert!(r.scheduler.armed().is_empty(), "no timer for a delay of nothing");
}

#[test]
fn the_panel_changes_no_verdict() {
    fn run(r: &Rig) -> Vec<(Verdict, Verdict)> {
        let mut verdicts = Vec::new();
        let mut keycode = 60;
        let mut press = |text: &str| {
            keycode += 1;
            let at_window = r.window_key(key(text, keycode));
            let at_module = r.module_key(&editor(), key(text, keycode + 100));
            r.host.post(HostEvent::KeyReleased { win: WIN, keycode });
            verdicts.push((at_window, at_module));
        };
        for text in [
            "a",
            "C-b",
            "C-b",
            &bound(&Action::Zoom),
            "k",
            "C-b",
            &split_right_key(),
            "t",
            "C-l",
            "C-b",
        ] {
            press(text);
        }
        verdicts.push((r.escape(), r.window_key(key("a", 90))));
        verdicts
    }
    let off = run(&driven(three(editor())));
    let shown = driven_with(three(editor()), Some(Duration::ZERO));
    let with_panel = run(&shown);
    assert_eq!(off, with_panel);
    assert!(
        off.iter().any(|(window, _)| *window == Verdict::Consumed),
        "the premise: keys were taken"
    );
}

/// A root key runs once however long it is held, and a window that lost focus while it was down never reports
/// its release, which must not make the same key's next press an auto-repeat.
#[test]
fn a_held_root_key_runs_once_and_focus_lost_while_it_was_down_frees_its_next_press() {
    let keymap = Keymap::apply_user(
        &[KeymapOp::Set {
            table: "root".into(),
            key: "M-y".into(),
            action: "zoom".into(),
            opts: vec![],
        }],
        &[],
    )
    .unwrap();
    let r = driven_keyed(three(agent()), keymap, None);
    let zoomed = || r.layout.borrow().zoomed().is_some();
    assert!(!zoomed());
    assert_eq!(r.window_key(key("M-y", K1)), Verdict::Consumed);
    assert!(zoomed(), "the root key ran its action with no prefix");
    assert_eq!(
        r.window_key(key("M-y", K1)),
        Verdict::Consumed,
        "the auto-repeat is swallowed"
    );
    assert!(zoomed(), "and ran nothing");
    r.host.post(HostEvent::Active {
        win: WIN,
        active: false,
    });
    r.host.post(HostEvent::Active { win: WIN, active: true });
    assert_eq!(r.window_key(key("M-y", K1)), Verdict::Consumed);
    assert!(
        !zoomed(),
        "the press after the focus change is a new press, not a repeat"
    );
    assert_eq!(r.waiting(), Waiting::No, "no wait was announced for it");
}
