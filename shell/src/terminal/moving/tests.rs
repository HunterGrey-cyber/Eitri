//! A terminal's move with nothing faked but the display: two window controllers, each on the fake host, linked by a
//! real link over a socketpair (`LinkDriver::attach_controller`, as the shell attaches it), each window's terminal
//! service carrying out the freeze, the forget, the resume and the adoption on real sessions over real PTYs with this
//! module's own code. The two windows share this test's process, as two processes would share the shell's pid and
//! the PTY: the descriptors still cross the socket.

use super::*;
use crate::linked_window::driver::LinkDriver;
use eitri_core::keymap::Action;
use eitri_core::layout::{Axis, Float, FloatRect, Layout, ModuleId, ModuleKind, Node, Size, Workspaces};
use eitri_core::tabs::ViewId;
use eitri_core::window_link::fdpass;
use eitri_core::window_link::link::{park, LinkHandle, PresenceFlag};
use eitri_core::wm::fake::{FakeHost, FakeScheduler, Logged};
use eitri_core::wm::module::SurfaceDecl;
use eitri_core::wm::{
    Controller, Driver, EditorPresence, EditorSpawn, HostOp, HostSink, LinkSide, Origin, PeerId, SurfaceKind, TermOp,
    WindowParts, WindowRole, WindowSpec,
};
use eitri_terminal::{PtySize, SessionCommand, SpawnSpec, TerminalColors};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use terminal_input::NormalizedInput;

const WIN: WinId = WinId::MAIN;
/// A flood through a debug build and two link threads: generous, reached only when something is stuck.
const LONG: Duration = Duration::from_secs(120);

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn agent() -> ModuleId {
    ModuleId::agent()
}
fn terminal() -> ModuleId {
    ModuleId::terminal()
}
fn numbered(n: u8) -> ModuleId {
    ModuleId::instance(ModuleKind::Terminal, n).unwrap()
}

fn decl(module: &ModuleId) -> SurfaceDecl {
    let kind = match module.kind() {
        ModuleKind::Editor => SurfaceKind::Editor {
            spawn: EditorSpawn {
                env: vec![],
                args: vec![],
                cwd: None,
                clean: true,
                background: None,
                open: vec![],
            },
        },
        ModuleKind::Agent => SurfaceKind::Chat {
            view: ViewId::of_module(module).unwrap(),
        },
        _ => SurfaceKind::Terminal { cwd: ".".into() },
    };
    SurfaceDecl {
        module: module.clone(),
        kind,
        deferred: false,
    }
}

fn sh(script: &str) -> SpawnSpec {
    let mut spec = SpawnSpec::shell("/bin/sh".into(), ".".into());
    spec.args = vec!["-c".into(), script.into()];
    spec
}

/// Every number below `lines`, one per line.
fn counter(lines: u32) -> SpawnSpec {
    sh(&format!(
        "i=0; while [ $i -lt {lines} ]; do echo $i; i=$((i+1)); done; sleep 30"
    ))
}

/// A program that turns bracketed paste on, then reads nothing for a second, so a paste waits in the session's
/// outbox, then copies exactly `bytes` of its input to `file`.
fn paste_reader(file: &std::path::Path, bytes: usize) -> SpawnSpec {
    sh(&format!(
        "stty raw -echo; printf '\\033[?2004h'; sleep 1; head -c {bytes} > '{}'; sleep 30",
        file.display()
    ))
}

/// About a quarter of a mebibyte of pasted lines, and the bytes a program with bracketed paste on reads for it.
fn paste() -> (NormalizedInput, Vec<u8>) {
    let mut text = String::new();
    let mut n = 0;
    while text.len() < 256 * 1024 {
        text.push_str(&format!("pasted line {n:06}\n"));
        n += 1;
    }
    let mut sent = b"\x1b[200~".to_vec();
    sent.extend_from_slice(text.as_bytes());
    sent.extend_from_slice(b"\x1b[201~");
    (NormalizedInput::Paste { text, bracketed: true }, sent)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A scratch directory of the test's own, removed when the test ends.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "eitri-terminal-moving-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Tapped = Arc<Mutex<Vec<u8>>>;

/// Where a session answers one freeze.
type FreezeAnswer = Receiver<Result<Detached, DetachRefused>>;

fn config(tap: &Tapped) -> SessionConfig {
    let into = tap.clone();
    SessionConfig {
        spawn: sh("exit"),
        size: PtySize {
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
        },
        colors: TerminalColors::default(),
        focused: false,
        tap: Some(Box::new(move |chunk: &[u8]| {
            into.lock().unwrap().extend_from_slice(chunk)
        })),
    }
}

/// One window's terminal service, over real sessions: what the shell's services do for the move's ops, with the
/// answers it would post queued for the test's loop to post.
#[derive(Default)]
struct Terms {
    scripts: RefCell<BTreeMap<ModuleId, SpawnSpec>>,
    sessions: RefCell<BTreeMap<ModuleId, TerminalSession>>,
    taps: RefCell<BTreeMap<ModuleId, Tapped>>,
    freezes: Freezes,
    asked: RefCell<Vec<(ModuleId, MoveId, FreezeAnswer)>>,
    /// The outbox each freeze carried, by move.
    outboxes: RefCell<BTreeMap<u64, usize>>,
}

impl Terms {
    fn tap(&self, module: &ModuleId) -> Tapped {
        self.taps.borrow_mut().entry(module.clone()).or_default().clone()
    }

    fn apply(&self, op: &HostOp) {
        match op {
            HostOp::Terminal(TermOp::Start { module, .. }) => {
                let Some(spawn) = self.scripts.borrow().get(module).cloned() else {
                    return;
                };
                let mut config = config(&self.tap(module));
                config.spawn = spawn;
                let session = TerminalSession::spawn(config, || {}).expect("the shell starts");
                self.sessions.borrow_mut().insert(module.clone(), session);
            }
            HostOp::Terminal(TermOp::Detach { module, mv, .. }) => {
                let (answer, answered) = std::sync::mpsc::channel();
                self.sessions.borrow()[module].send(SessionCommand::Detach(answer));
                self.freezes.asked(module, *mv);
                self.asked.borrow_mut().push((module.clone(), *mv, answered));
            }
            HostOp::Terminal(TermOp::Forget { module, .. }) => {
                self.freezes.forget(module);
                if let Some(session) = self.sessions.borrow_mut().remove(module) {
                    session.send(SessionCommand::Committed);
                }
            }
            HostOp::Terminal(TermOp::Resume { module, .. }) => {
                self.freezes.forget(module);
                self.sessions.borrow()[module].send(SessionCommand::Resume);
            }
            HostOp::AdoptMoved {
                module, record, fds, ..
            } => {
                let (session, _) =
                    adopt(record, fds.take(), config(&self.tap(module)), || {}).expect("the terminal is taken over");
                self.sessions.borrow_mut().insert(module.clone(), session);
            }
            _ => {}
        }
    }

    /// The events the freezes answered since the last call, as the services post them.
    fn answers(&self) -> Vec<HostEvent> {
        let mut events = Vec::new();
        self.asked
            .borrow_mut()
            .retain(|(module, mv, answered)| match answered.try_recv() {
                Ok(answer) => {
                    let frozen = frozen_of(answer);
                    if let Frozen::Record { record, .. } = &frozen {
                        let outbox = record["outbox"].as_array().map_or(0, Vec::len);
                        self.outboxes.borrow_mut().insert(mv.0, outbox);
                    }
                    let answer = answer_for(&self.freezes, module, *mv, frozen);
                    events.extend(event_for(WIN, module.clone(), *mv, answer));
                    false
                }
                Err(_) => true,
            });
        events
    }
}

/// One window of the pair.
struct End {
    driver: Rc<Driver<Controller>>,
    host: Rc<FakeHost>,
    link: LinkDriver,
    layout: eitri_core::wm::LayoutView,
    terms: Rc<Terms>,
}

impl End {
    fn open(layout: Layout, role: WindowRole, link: LinkDriver, grid: Size, scripts: &[(ModuleId, SpawnSpec)]) -> End {
        let host = FakeHost::new();
        host.set_grid_size(grid);
        let scheduler = FakeScheduler::new();
        let mut controller = Controller::new(scheduler.clone());
        controller.set_reads(host.clone());
        let keymap = eitri_core::keymap::Keymap::defaults();
        let modules = eitri_core::layout::ModuleKeys::build(&[], &keymap).unwrap();
        controller.set_keymap(WIN, Rc::new(keymap), Rc::new(modules));
        let hosted: Vec<_> = layout.modules().iter().map(decl).collect();
        for d in &hosted {
            host.with_surface(WIN, d.module.clone(), d.kind.clone(), false);
        }
        let presence = match &role {
            WindowRole::Linked { side: LinkSide::Second } => EditorPresence::Proxy,
            _ => EditorPresence::Owned,
        };
        let peer = PeerId(if presence == EditorPresence::Proxy { 1 } else { 2 });
        let parts = WindowParts::new(layout, Workspaces::single(), None);
        let layout = parts.views().0;
        controller.open_window(
            WIN,
            WindowSpec {
                role,
                presence,
                title: "project".into(),
            },
            parts,
        );
        let terms = Rc::new(Terms::default());
        terms.scripts.borrow_mut().extend(scripts.iter().cloned());
        {
            let terms = terms.clone();
            host.on_apply(move |_, _, op| terms.apply(op));
        }
        let driver = Driver::new(controller, host.clone(), scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        driver.first_apply(WIN, hosted);
        link.attach_controller(WIN, peer, &driver);
        End {
            driver,
            host,
            link,
            layout,
            terms,
        }
    }

    fn ops(&self) -> Vec<HostOp> {
        self.host
            .log()
            .into_iter()
            .filter_map(|entry| match entry {
                Logged::Op(op, _) => Some(op),
                _ => None,
            })
            .collect()
    }

    fn adopted(&self) -> bool {
        self.ops().iter().any(|op| matches!(op, HostOp::AdoptMoved { .. }))
    }

    fn forgot(&self, module: &ModuleId) -> bool {
        self.ops()
            .iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::Forget { module: m, .. }) if m == module))
    }

    fn resumed(&self, module: &ModuleId) -> bool {
        self.ops()
            .iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::Resume { module: m, .. }) if m == module))
    }

    fn move_to_other(&self, module: &ModuleId) {
        self.driver.run(
            WIN,
            Action::MoveToWindow {
                module: Some(module.clone()),
                follow: false,
            },
            Origin::Key { keycode: 7 },
        );
    }

    /// One turn of each window's main loop: the link drained, the freezes' answers posted.
    fn turn(&self) {
        self.link.pump();
        for event in self.terms.answers() {
            self.driver.post(event);
        }
    }
}

/// Turns both windows' loops until `done` holds; false past `limit`.
fn until(ends: [&End; 2], limit: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        for end in ends {
            end.turn();
        }
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

fn links() -> (LinkDriver, LinkDriver) {
    let (a, b) = fdpass::seqpacket_pair().unwrap();
    let (one, one_in) = LinkHandle::start(a, PresenceFlag::default()).unwrap();
    let (two, two_in) = LinkHandle::start(b, PresenceFlag::default()).unwrap();
    (LinkDriver::new(one, one_in), LinkDriver::new(two, two_in))
}

/// Window 2's end held just before it writes `move_done`, failing that write once released when `fail`.
fn links_parked(fail: bool) -> (LinkDriver, LinkDriver, Arc<park::ParkHook>) {
    let (a, b) = fdpass::seqpacket_pair().unwrap();
    let (one, one_in) = LinkHandle::start(a, PresenceFlag::default()).unwrap();
    let (two, two_in, hook) = if fail {
        park::start_parked_failing(b, PresenceFlag::default(), "move_done").unwrap()
    } else {
        park::start_parked(b, PresenceFlag::default(), "move_done").unwrap()
    };
    (LinkDriver::new(one, one_in), LinkDriver::new(two, two_in), hook)
}

const GRID: Size = Size { w: 1200, h: 800 };

/// Window 1, `editor | (agent / <terminals>)`, the keys on the editor, and window 2 holding `window_2`.
fn pair(
    terminals: &[ModuleId],
    window_2: Layout,
    scripts: &[(ModuleId, SpawnSpec)],
    links: (LinkDriver, LinkDriver),
) -> (End, End) {
    let mut right = Node::Leaf(agent());
    for t in terminals {
        right = Node::split(Axis::Column, 0.5, right, Node::Leaf(t.clone()));
    }
    let one = Layout::new(Node::split(Axis::Row, 0.5, Node::Leaf(editor()), right), editor()).unwrap();
    let one = End::open(one, WindowRole::Main, links.0, GRID, scripts);
    for t in terminals {
        one.host.set_module_size(t.clone(), Size { w: 600, h: 400 });
    }
    let two = End::open(
        window_2,
        WindowRole::Linked { side: LinkSide::Second },
        links.1,
        Size { w: 1000, h: 600 },
        &[],
    );
    (one, two)
}

fn chat_alone() -> Layout {
    Layout::new(Node::Leaf(agent()), agent()).unwrap()
}

/// Reaps `pid`, killed first if it still runs, so no test leaves a child behind.
fn end_child(pid: u32) {
    // SAFETY: a signal and a wait for this test's own child, by its pid.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
        libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0);
    }
}

fn pid_of(end: &End, module: &ModuleId) -> u32 {
    end.terms.sessions.borrow()[module].pid()
}

fn file_len(file: &std::path::Path) -> usize {
    std::fs::metadata(file).map_or(0, |m| m.len() as usize)
}

#[test]
fn a_terminal_with_a_running_counter_moves_and_nothing_is_lost_or_doubled() {
    // A counter that never ends is mid-flood whenever the freeze comes, however fast this machine reads it.
    let forever = sh("i=0; while :; do echo $i; i=$((i+1)); done");
    let (one, two) = pair(&[terminal()], chat_alone(), &[(terminal(), forever)], links());
    let source_tap = one.terms.tap(&terminal());
    assert!(
        until([&one, &two], LONG, || contains(
            &source_tap.lock().unwrap(),
            b"\n2000\r\n"
        )),
        "the counter gets going"
    );
    let pid = pid_of(&one, &terminal());
    one.move_to_other(&terminal());
    assert!(
        until([&one, &two], LONG, || two.adopted() && one.forgot(&terminal())),
        "the move commits: {:#?}",
        one.ops()
    );
    assert!(
        one.layout.read().is_away(&terminal()),
        "window 1 keeps an away placeholder"
    );
    assert!(two.layout.read().is_shown(&terminal()), "window 2 shows the arrival");
    let read_by_source = source_tap.lock().unwrap().len();
    let at_the_move = complete_numbers(&source_tap.lock().unwrap());
    let destination_tap = two.terms.tap(&terminal());
    assert!(
        until([&one, &two], LONG, || {
            let mut all = source_tap.lock().unwrap().clone();
            all.extend_from_slice(&destination_tap.lock().unwrap());
            complete_numbers(&all) > at_the_move + 20_000
        }),
        "the destination reads on where the source stopped"
    );
    end_child(pid);
    assert_eq!(
        source_tap.lock().unwrap().len(),
        read_by_source,
        "the source read nothing once the move was committed"
    );
    let mut all = source_tap.lock().unwrap().clone();
    all.extend_from_slice(&destination_tap.lock().unwrap());
    // Every number from 0 once and in order, across both windows' reads; only the last line may be cut short.
    complete_numbers(&all);
}

/// How many numbers `bytes` holds, each exactly once and in order from 0; a last line with no line end yet is not
/// counted.
fn complete_numbers(bytes: &[u8]) -> u32 {
    let text = String::from_utf8_lossy(bytes);
    let complete = match text.rfind('\n') {
        Some(at) => &text[..at],
        None => "",
    };
    let mut expected = 0u32;
    for line in complete.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let n: u32 = line
            .parse()
            .unwrap_or_else(|_| panic!("not a number: {line:?} (expected {expected})"));
        assert_eq!(n, expected, "a number was lost, doubled or reordered");
        expected += 1;
    }
    expected
}

#[test]
fn the_source_resumes_after_an_abort_and_writes_its_kept_outbox() {
    let dir = Scratch::new("resume");
    let file = dir.0.join("read.bin");
    let (input, sent) = paste();
    let (a, b, hook) = links_parked(true);
    let (one, two) = pair(
        &[terminal()],
        chat_alone(),
        &[(terminal(), paste_reader(&file, sent.len()))],
        (a, b),
    );
    let tap = one.terms.tap(&terminal());
    assert!(until([&one, &two], LONG, || contains(
        &tap.lock().unwrap(),
        b"\x1b[?2004h"
    )));
    let pid = pid_of(&one, &terminal());
    one.terms.sessions.borrow()[&terminal()].send(SessionCommand::Input(input));
    one.move_to_other(&terminal());
    assert!(until([&one, &two], LONG, || hook.wait_parked(Duration::ZERO)));
    assert!(
        one.terms.outboxes.borrow().get(&1).is_some_and(|n| *n > sent.len() / 2),
        "the premise: most of the paste was still unwritten at the freeze ({:?} of {})",
        one.terms.outboxes.borrow(),
        sent.len()
    );
    // Window 2's `move_done` never reaches the socket: its link ends, and with it the move.
    hook.release();
    assert!(
        until([&one, &two], LONG, || one.resumed(&terminal())),
        "the source resumes: {:#?}",
        one.ops()
    );
    assert!(!two.adopted(), "window 2 never built it");
    assert!(!one.layout.read().is_away(&terminal()));
    assert!(
        until([&one, &two], LONG, || file_len(&file) >= sent.len()),
        "the program reads the paste"
    );
    assert_eq!(std::fs::read(&file).unwrap(), sent, "every byte once, in order");
    end_child(pid);
}

#[test]
fn the_destination_writes_the_outbox_only_after_move_done() {
    let dir = Scratch::new("after-done");
    let file = dir.0.join("read.bin");
    let (input, sent) = paste();
    let (a, b, hook) = links_parked(false);
    let (one, two) = pair(
        &[terminal()],
        chat_alone(),
        &[(terminal(), paste_reader(&file, sent.len()))],
        (a, b),
    );
    let tap = one.terms.tap(&terminal());
    assert!(until([&one, &two], LONG, || contains(
        &tap.lock().unwrap(),
        b"\x1b[?2004h"
    )));
    let pid = pid_of(&one, &terminal());
    one.terms.sessions.borrow()[&terminal()].send(SessionCommand::Input(input));
    one.move_to_other(&terminal());
    assert!(until([&one, &two], LONG, || hook.wait_parked(Duration::ZERO)));
    // The program reads again after a second; what it was owed may not reach it while `move_done` is unwritten.
    until([&one, &two], Duration::from_millis(1500), || false);
    let read = file_len(&file);
    assert!(
        read < sent.len() / 2,
        "the paste waits while the move is undecided ({read} of {} bytes read)",
        sent.len()
    );
    assert!(!two.adopted(), "nothing is built before move_done is written");
    hook.release();
    assert!(until([&one, &two], LONG, || two.adopted() && one.forgot(&terminal())));
    assert!(
        until([&one, &two], LONG, || file_len(&file) >= sent.len()),
        "the destination writes what the source owed"
    );
    assert_eq!(std::fs::read(&file).unwrap(), sent, "every byte once, in order");
    end_child(pid);
}

#[test]
fn no_kind_commits_before_move_done_is_confirmed_written() {
    let (a, b, hook) = links_parked(false);
    let (one, two) = pair(&[terminal()], chat_alone(), &[(terminal(), counter(1_000))], (a, b));
    let tap = one.terms.tap(&terminal());
    assert!(until([&one, &two], LONG, || contains(
        &tap.lock().unwrap(),
        b"\n999\r\n"
    )));
    let pid = pid_of(&one, &terminal());
    one.move_to_other(&terminal());
    assert!(until([&one, &two], LONG, || hook.wait_parked(Duration::ZERO)));
    until([&one, &two], Duration::from_millis(300), || false);
    assert!(
        !two.adopted(),
        "the destination builds nothing while its move_done is unwritten"
    );
    assert!(!one.forgot(&terminal()), "the source keeps its copy");
    assert!(!one.layout.read().is_away(&terminal()));
    assert!(
        !two.layout.read().contains(&terminal()),
        "and the arrival is not placed"
    );
    hook.release();
    assert!(until([&one, &two], LONG, || two.adopted() && one.forgot(&terminal())));
    assert!(two.layout.read().is_shown(&terminal()));
    end_child(pid);
}

#[test]
fn a_float_keeps_its_relative_rectangle() {
    let one_layout = Layout::from_parts_floating(
        Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(agent())),
        vec![Float {
            id: terminal(),
            rect: FloatRect {
                cx: 0.5,
                cy: 0.4,
                w: 600,
                h: 400,
            },
            scratch: false,
        }],
        Default::default(),
        editor(),
        Default::default(),
    )
    .unwrap();
    let (a, b) = links();
    let one = End::open(one_layout, WindowRole::Main, a, GRID, &[(terminal(), sh("sleep 30"))]);
    let two = End::open(
        chat_alone(),
        WindowRole::Linked { side: LinkSide::Second },
        b,
        Size { w: 1000, h: 600 },
        &[],
    );
    assert!(until([&one, &two], LONG, || one
        .terms
        .sessions
        .borrow()
        .contains_key(&terminal())));
    let pid = pid_of(&one, &terminal());
    one.move_to_other(&terminal());
    assert!(until([&one, &two], LONG, || two.adopted()));
    let layout = two.layout.read();
    let float = layout.float_of(&terminal()).expect("it floats in window 2 too");
    // Half the grid's width and height, centred where it was: on window 2's 1000x600 grid.
    assert_eq!((float.rect.w, float.rect.h), (500, 300));
    assert!(
        (float.rect.cx - 0.5).abs() < 1e-6 && (float.rect.cy - 0.4).abs() < 1e-6,
        "{float:?}"
    );
    drop(layout);
    end_child(pid);
}

#[test]
fn the_moved_module_keeps_its_id_or_takes_the_lowest_free_number() {
    let window_2 = Layout::new(
        Node::split(Axis::Row, 0.5, Node::Leaf(agent()), Node::Leaf(terminal())),
        agent(),
    )
    .unwrap();
    let scripts = [(terminal(), sh("sleep 30")), (numbered(2), sh("sleep 30"))];
    let (one, two) = pair(&[terminal(), numbered(2)], window_2, &scripts, links());
    assert!(until([&one, &two], LONG, || one.terms.sessions.borrow().len() == 2));
    let pids = [pid_of(&one, &terminal()), pid_of(&one, &numbered(2))];

    // Free in window 2: it keeps its id.
    one.move_to_other(&numbered(2));
    assert!(until([&one, &two], LONG, || one.forgot(&numbered(2))));
    assert!(two.layout.read().is_shown(&numbered(2)));

    // Window 2 has a `terminal` of its own and now `terminal#2`: the next arrival takes the lowest free number.
    one.move_to_other(&terminal());
    assert!(until([&one, &two], LONG, || one.forgot(&terminal())));
    assert!(
        two.layout.read().is_shown(&numbered(3)),
        "{:?}",
        two.layout.read().modules()
    );
    assert!(two.terms.sessions.borrow().contains_key(&numbered(3)));
    for pid in pids {
        end_child(pid);
    }
}

#[test]
fn a_detach_is_answered_with_its_own_move_id() {
    let tap = Tapped::default();
    let mut config = config(&tap);
    config.spawn = sh("echo ready; sleep 30");
    let session = TerminalSession::spawn(config, || {}).unwrap();
    let deadline = Instant::now() + LONG;
    while !contains(&tap.lock().unwrap(), b"ready") {
        assert!(Instant::now() < deadline, "the shell starts");
        std::thread::sleep(Duration::from_millis(5));
    }
    let freezes = Freezes::default();
    let (answer, answered) = std::sync::mpsc::channel();
    session.send(SessionCommand::Detach(answer));
    freezes.asked(&terminal(), MoveId(7));
    let frozen = wait_frozen(answered, ANSWER_WAIT);
    let event = event_for(
        WIN,
        terminal(),
        MoveId(7),
        answer_for(&freezes, &terminal(), MoveId(7), frozen),
    );
    let Some(HostEvent::Terminal(TermEvent::Detached {
        module,
        mv,
        record,
        handoff,
        ..
    })) = event
    else {
        panic!("a freeze is answered with the terminal: {event:?}");
    };
    assert_eq!(
        (module, mv),
        (terminal(), MoveId(7)),
        "the move the freeze was asked for"
    );
    assert_eq!(record["kind"], KIND);
    assert_eq!(record["pid"], session.pid());
    let fds = handoff
        .inner()
        .downcast_ref::<eitri_core::wm::Descriptors>()
        .expect("the handoff holds the descriptors the link passes");
    assert_eq!(fds.len(), 1, "the PTY's controller side");
    assert_eq!(freezes.outstanding(&terminal()), None, "answered");
    session.send(SessionCommand::Resume);
    end_child(session.pid());
}

#[test]
fn a_late_freeze_refusal_never_gives_up_the_retry() {
    let freezes = Freezes::default();
    freezes.asked(&terminal(), MoveId(1));
    // Move 1 ended while frozen; its retry, move 3, froze the module again before move 1's refusal came back.
    freezes.asked(&terminal(), MoveId(3));
    let late = answer_for(&freezes, &terminal(), MoveId(1), Frozen::Refused("busy".into()));
    assert!(matches!(late, Answer::Dropped(_)), "{late:?}");
    assert_eq!(
        event_for(WIN, terminal(), MoveId(1), late),
        None,
        "nothing is posted for it"
    );
    assert_eq!(
        freezes.outstanding(&terminal()),
        Some(MoveId(3)),
        "the retry still waits"
    );
    let own = answer_for(&freezes, &terminal(), MoveId(3), Frozen::Refused("busy".into()));
    assert!(
        matches!(
            event_for(WIN, terminal(), MoveId(3), own),
            Some(HostEvent::Refused {
                what: OpTag::Terminal,
                surface: Some((WIN, ref m)),
                ..
            }) if *m == terminal()
        ),
        "the retry's own refusal names its module"
    );
}

#[test]
fn an_arriving_record_that_is_not_a_terminal_is_refused_whole() {
    let (a, _b) = fdpass::seqpacket_pair().unwrap();
    let refused = |record: serde_json::Value, fds: Vec<OwnedFd>| arrival(&record, fds).err().unwrap_or_default();
    assert!(refused(serde_json::json!([1]), vec![]).contains("not an object"));
    assert!(refused(serde_json::json!({ "kind": "chat" }), vec![]).contains("not a terminal's"));
    assert!(refused(serde_json::json!({ "kind": "terminal" }), vec![]).contains("exactly one descriptor"));
    assert!(
        refused(serde_json::json!({ "kind": "terminal", "pid": 5 }), vec![a]).contains("not a terminal"),
        "a socket is no PTY"
    );
}
