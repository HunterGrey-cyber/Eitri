use super::*;
use crate::layout::{hide, kill, Axis, Frame, Layout, Node, Placement, Reopen, Size};
use crate::wm::fake::{FakeHost, FakeScheduler, FakeShell, Logged};
use crate::wm::verbs::rig::{decl, rig, rig_with, WIN};
use crate::wm::{Decide, Driver, HostSink};
use std::cell::RefCell;
use std::rc::Rc;

const PLUGIN: &str = "demo";
const DIR: &str = "/w/work/rust/ch04-02-fix-total-len";

fn terminal() -> ModuleId {
    ModuleId::terminal()
}

fn frame() -> Frame<'static> {
    Frame::new(Size { w: 1200, h: 800 }, 4)
}

/// The editor above the base terminal.
fn two() -> Layout {
    Layout::new(
        Node::split(
            Axis::Column,
            0.7,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(terminal()),
        ),
        ModuleId::editor(),
    )
    .unwrap()
}

/// The editor, with the base terminal declared but hidden: no shell has started.
fn two_hidden() -> Layout {
    let mut layout = two();
    hide(&mut layout, &terminal(), &frame()).unwrap();
    layout
}

/// The base terminal alone on screen.
fn alone() -> Layout {
    Layout::new(Node::Leaf(terminal()), terminal()).unwrap()
}

fn shell(generation: u64, pid: u32) -> ShellStart {
    ShellStart::of(generation, Some(pid))
}

struct T {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    scheduler: Rc<FakeScheduler>,
    layout: Rc<RefCell<Layout>>,
}

/// `layout` held by a driven controller over the fake host, its first apply carried out, in `PLUGIN`'s own window.
fn driven(layout: Layout) -> T {
    let mut r = rig_with(layout, |_, _| None);
    r.controller.window_mut(WIN).unwrap().own_window = Some(PLUGIN.into());
    let hosted: Vec<_> = r.layout.borrow().modules().iter().map(decl).collect();
    for d in hosted {
        r.host.with_surface(WIN, d.module, d.kind, false);
    }
    let driver = Driver::new(r.controller, r.host.clone(), r.scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    r.host.attach(Rc::downgrade(&sink));
    r.scheduler.attach(Rc::downgrade(&sink));
    for op in r.first.ops() {
        r.host.apply(op);
    }
    T {
        driver,
        host: r.host,
        scheduler: r.scheduler,
        layout: r.layout,
    }
}

/// `layout` with its base terminal running the shell `pid`, under the first start.
fn running(layout: Layout, pid: u32) -> T {
    let t = driven(layout);
    t.started(pid);
    t
}

impl T {
    fn at(&self, dir: &str, ticket: u64) -> Result<(), String> {
        self.driver.plugin_terminal_at(WIN, PLUGIN, PathBuf::from(dir), ticket)
    }

    /// Every op carried out so far but the chrome's announcements.
    fn ops(&self) -> Vec<HostOp> {
        self.host
            .log()
            .into_iter()
            .filter_map(|entry| match entry {
                Logged::Op(op, _) if !matches!(op, HostOp::Chrome { .. }) => Some(op),
                _ => None,
            })
            .collect()
    }

    fn since(&self, mark: usize) -> Vec<HostOp> {
        self.ops().into_iter().skip(mark).collect()
    }

    /// The ops since `mark` but the answers to pages and the "has the keys" signals.
    fn work_since(&self, mark: usize) -> Vec<HostOp> {
        self.since(mark)
            .into_iter()
            .filter(|op| !matches!(op, HostOp::Plugin(PluginOp::TerminalAtAnswer { .. })))
            .collect()
    }

    fn answers(&self) -> Vec<(u64, TerminalAtAnswer)> {
        self.ops()
            .into_iter()
            .filter_map(|op| match op {
                HostOp::Plugin(PluginOp::TerminalAtAnswer {
                    win,
                    plugin,
                    ticket,
                    answer,
                }) => {
                    assert_eq!(
                        (win, plugin.as_str()),
                        (WIN, PLUGIN),
                        "the answer goes to the asking plugin"
                    );
                    Some((ticket, answer))
                }
                _ => None,
            })
            .collect()
    }

    /// The reads asked for so far, in order.
    fn reads(&self) -> Vec<u64> {
        self.ops()
            .into_iter()
            .filter_map(|op| match op {
                HostOp::Terminal(TermOp::ReadJob { win, module, read }) => {
                    assert_eq!((win, module), (WIN, terminal()));
                    Some(read)
                }
                _ => None,
            })
            .collect()
    }

    fn started(&self, pid: u32) {
        self.driver.post(HostEvent::Terminal(TermEvent::Started {
            win: WIN,
            module: terminal(),
            pid,
        }));
    }

    fn exited(&self) {
        self.driver.post(HostEvent::Terminal(TermEvent::Exited {
            win: WIN,
            module: terminal(),
        }));
    }

    fn job(&self, read: u64, shell: ShellStart, cwd: Result<&str, &str>, busy: bool) {
        self.driver.post(HostEvent::Terminal(TermEvent::Job {
            win: WIN,
            module: terminal(),
            read,
            shell,
            cwd: cwd.map(PathBuf::from).map_err(str::to_owned),
            busy,
        }));
    }

    fn refuse_respawn(&self) {
        self.driver.post(HostEvent::Refused {
            what: OpTag::Respawn,
            why: "the pane no longer runs that shell".into(),
            surface: Some((WIN, terminal())),
        });
    }

    fn show(&self) {
        self.layout.borrow_mut().show(&terminal()).unwrap();
        self.driver.test_batch(|c| c.changed_in_test(WIN));
    }

    fn run(&self) -> Option<TermRun> {
        self.driver
            .query(|c| c.lifecycle(WIN).and_then(|l| l.terminal_run(&terminal()).cloned()))
    }

    fn state(&self) -> (Vec<PendingAt>, Option<PendingRestart>) {
        self.driver.query(|c| c.terminal_at_state(WIN))
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }

    fn shown(&self) -> bool {
        self.layout.borrow().is_shown(&terminal())
    }

    fn close_count(&self) -> usize {
        self.ops()
            .iter()
            .filter(|op| matches!(op, HostOp::CloseWindow { .. }))
            .count()
    }

    fn prompt(&self) -> Option<String> {
        self.driver.query(|c| c.chrome(WIN).prompt.map(|p| p.text))
    }

    /// A restart decided and sent: `terminal.at` under ticket 1, its read answered with an idle shell `pid`
    /// elsewhere.
    fn restarting(&self, pid: u32) {
        self.at(DIR, 1).unwrap();
        let read = *self.reads().last().unwrap();
        self.job(read, shell(1, pid), Ok("/w/work/rust/ch03-01"), false);
        assert!(self.state().1.is_some(), "the premise: a restart waits");
    }
}

/// `ops` holds `SetStartDir` for `dir` right before the terminal's `Start`, and nothing between them.
fn assert_start_dir_then_start(ops: &[HostOp], dir: &str) {
    let set = ops
        .iter()
        .position(|op| matches!(op, HostOp::Terminal(TermOp::SetStartDir { .. })))
        .unwrap_or_else(|| panic!("no SetStartDir: {ops:#?}"));
    assert_eq!(
        ops[set],
        HostOp::Terminal(TermOp::SetStartDir {
            win: WIN,
            module: terminal(),
            dir: dir.into(),
        })
    );
    assert_eq!(
        ops.get(set + 1),
        Some(&HostOp::Terminal(TermOp::Start {
            win: WIN,
            module: terminal()
        })),
        "the start comes right after its directory: {ops:#?}"
    );
}

fn set_start_dir(dir: &str) -> HostOp {
    HostOp::Terminal(TermOp::SetStartDir {
        win: WIN,
        module: terminal(),
        dir: dir.into(),
    })
}

/// The base terminal killed by its own shell's end, then shown again: the ops of that show.
fn killed_and_shown(t: &T) -> Vec<HostOp> {
    t.exited();
    assert!(!t.shown(), "the premise: its own end killed it");
    let mark = t.ops().len();
    t.show();
    t.since(mark)
}

fn terminal_ops(ops: &[HostOp]) -> Vec<HostOp> {
    ops.iter()
        .filter(|op| {
            matches!(
                op,
                HostOp::Terminal(
                    TermOp::Start { .. }
                        | TermOp::SetStartDir { .. }
                        | TermOp::HangUp { .. }
                        | TermOp::ReadJob { .. }
                        | TermOp::Respawn { .. }
                )
            )
        })
        .cloned()
        .collect()
}

#[test]
fn a_never_started_terminal_keeps_the_dir_and_set_start_dir_comes_just_before_its_first_start() {
    let t = driven(two_hidden());
    assert!(terminal_ops(&t.ops()).is_empty(), "the premise: no shell started");
    assert_eq!(t.run(), Some(TermRun::NotStarted));
    let mark = t.ops().len();
    assert_eq!(t.at(DIR, 1), Ok(()));
    assert_eq!(t.answers(), vec![(1, TerminalAtAnswer::StartDir)]);
    assert!(
        t.work_since(mark).is_empty(),
        "answered at once, nothing done: {:#?}",
        t.since(mark)
    );
    let mark = t.ops().len();
    t.show();
    assert_start_dir_then_start(&t.since(mark), DIR);
    // The directory goes with that one start only.
    let mark = t.ops().len();
    t.driver.test_batch(|c| c.changed_in_test(WIN));
    assert!(
        !t.since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::SetStartDir { .. }))),
        "{:#?}",
        t.since(mark)
    );
}

#[test]
fn a_killed_terminal_the_same() {
    let t = running(two(), 4100);
    t.exited();
    assert!(!t.shown(), "the premise: its own end killed it");
    assert_eq!(t.run(), Some(TermRun::NotStarted));
    let mark = t.ops().len();
    assert_eq!(t.at(DIR, 2), Ok(()));
    assert_eq!(t.answers(), vec![(2, TerminalAtAnswer::StartDir)]);
    assert!(t.work_since(mark).is_empty(), "{:#?}", t.since(mark));
    let mark = t.ops().len();
    t.show();
    assert_start_dir_then_start(&t.since(mark), DIR);
}

#[test]
fn an_exited_shell_keeps_the_dir_and_set_start_dir_comes_just_before_its_next_start() {
    let t = running(two(), 4100);
    // A shell that ended while its terminal stays (the controller could not kill it): the record says so and
    // nothing else happened.
    t.driver.test_batch(|c| {
        c.lifecycle_mut(WIN).unwrap().record_event(
            WIN,
            &HostEvent::Terminal(TermEvent::Exited {
                win: WIN,
                module: terminal(),
            }),
        );
        Batch::default()
    });
    assert_eq!(t.run(), Some(TermRun::Exited));
    let mark = t.ops().len();
    assert_eq!(t.at(DIR, 3), Ok(()));
    assert_eq!(t.answers(), vec![(3, TerminalAtAnswer::StartDir)]);
    assert_eq!(
        t.work_since(mark),
        vec![set_start_dir(DIR)],
        "the live pane takes the directory now, for a restart on Enter"
    );
    // Its next start may also be the one after a kill.
    let id = terminal();
    kill(
        &mut t.layout.borrow_mut(),
        &id,
        Reopen::At(Placement::BelowEditor),
        &frame(),
    )
    .unwrap();
    t.driver.post(HostEvent::SurfaceEnded {
        win: WIN,
        module: id,
        why: Ended::Killed {
            reopen: Reopen::At(Placement::BelowEditor),
        },
    });
    let mark = t.ops().len();
    t.show();
    assert_start_dir_then_start(&t.since(mark), DIR);
}

#[test]
fn an_exited_shell_restarted_in_place_spends_the_dir_and_a_later_start_gets_none() {
    let t = running(two(), 4100);
    t.driver.test_batch(|c| {
        c.lifecycle_mut(WIN).unwrap().record_event(
            WIN,
            &HostEvent::Terminal(TermEvent::Exited {
                win: WIN,
                module: terminal(),
            }),
        );
        Batch::default()
    });
    assert_eq!(t.at(DIR, 3), Ok(()));
    // Enter in the pane starts a shell with no `Start`.
    let mark = t.ops().len();
    t.started(4200);
    assert!(terminal_ops(&t.since(mark)).is_empty(), "{:#?}", t.since(mark));
    assert_eq!(t.run(), Some(TermRun::Running { pid: 4200 }));
    let shown = killed_and_shown(&t);
    assert!(
        !shown
            .iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::SetStartDir { .. }))),
        "the directory went with the restart, not with this start: {shown:#?}"
    );
    assert!(
        shown.contains(&HostOp::Terminal(TermOp::Start {
            win: WIN,
            module: terminal()
        })),
        "{shown:#?}"
    );
}

#[test]
fn a_shown_terminal_whose_spawn_waits_takes_the_dir_now_and_spends_it_on_starting() {
    let t = driven(two());
    assert!(
        terminal_ops(&t.ops()).contains(&HostOp::Terminal(TermOp::Start {
            win: WIN,
            module: terminal()
        })),
        "the premise: its start is out"
    );
    assert_eq!(t.run(), Some(TermRun::NotStarted), "the premise: no shell yet");
    let mark = t.ops().len();
    assert_eq!(t.at(DIR, 5), Ok(()));
    assert_eq!(t.answers(), vec![(5, TerminalAtAnswer::StartDir)]);
    assert_eq!(
        t.work_since(mark),
        vec![set_start_dir(DIR)],
        "the pane's spawn reads the directory when its size comes"
    );
    t.started(4100);
    let shown = killed_and_shown(&t);
    assert!(
        !shown
            .iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::SetStartDir { .. }))),
        "the directory went with the first shell: {shown:#?}"
    );
}

#[test]
fn a_running_shell_is_read_with_read_job_and_pending_at_is_recorded() {
    let t = running(two(), 4100);
    let mark = t.ops().len();
    assert_eq!(t.at(DIR, 4), Ok(()));
    assert!(t.answers().is_empty(), "no answer before the read comes back");
    assert_eq!(
        t.work_since(mark),
        vec![HostOp::Terminal(TermOp::ReadJob {
            win: WIN,
            module: terminal(),
            read: 0,
        })],
        "a read and nothing else"
    );
    let (pending, restart) = t.state();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (
            pending[0].ticket,
            pending[0].plugin.as_str(),
            pending[0].dir.as_path(),
            pending[0].read
        ),
        (4, PLUGIN, std::path::Path::new(DIR), 0)
    );
    assert_eq!(restart, None);
    // A second request reads again under its own number.
    t.at(DIR, 5).unwrap();
    assert_eq!(t.reads(), vec![0, 1]);
}

#[test]
fn a_moved_terminal_is_read_with_read_job_and_respawn_expects_the_captured_shell() {
    let t = driven(two());
    t.driver.test_batch(|c| {
        c.lifecycle_mut(WIN).unwrap().declare_moved(&decl(&terminal()));
        Batch::default()
    });
    assert_eq!(t.run(), Some(TermRun::Moved));
    t.at(DIR, 6).unwrap();
    assert_eq!(t.reads(), vec![0]);
    let mark = t.ops().len();
    let captured = shell(7, 9001);
    t.job(0, captured, Ok("/w/work/rust/ch03-01"), false);
    assert_eq!(
        t.work_since(mark),
        vec![HostOp::Terminal(TermOp::Respawn {
            win: WIN,
            module: terminal(),
            dir: DIR.into(),
            expect: captured,
        })]
    );
    assert_eq!(t.state().1.map(|r| r.shell), Some(captured));
}

#[test]
fn a_moved_terminal_whose_record_exits_before_the_job_is_the_terminal_changed() {
    let t = driven(two());
    t.driver.test_batch(|c| {
        c.lifecycle_mut(WIN).unwrap().declare_moved(&decl(&terminal()));
        Batch::default()
    });
    t.at(DIR, 7).unwrap();
    t.exited();
    let mark = t.ops().len();
    t.job(0, shell(7, 9001), Ok(DIR), false);
    assert_eq!(t.answers(), vec![(7, TerminalAtAnswer::Busy(CHANGED_TEXT.into()))]);
    assert!(terminal_ops(&t.since(mark)).is_empty(), "{:#?}", t.since(mark));
}

#[test]
fn a_cwd_inside_dir_is_already() {
    let t = running(two(), 4100);
    t.at(DIR, 8).unwrap();
    t.at(DIR, 9).unwrap();
    let mark = t.ops().len();
    t.job(0, shell(1, 4100), Ok(&format!("{DIR}/src")), false);
    t.job(1, shell(1, 4100), Ok(DIR), false);
    assert_eq!(
        t.answers(),
        vec![(8, TerminalAtAnswer::Already), (9, TerminalAtAnswer::Already)]
    );
    assert!(
        t.work_since(mark).is_empty(),
        "nothing is touched: {:#?}",
        t.since(mark)
    );
    assert_eq!(t.state(), (vec![], None));
}

#[test]
fn a_cwd_inside_dir_with_cargo_watch_running_is_already() {
    let t = running(two(), 4100);
    t.at(DIR, 10).unwrap();
    let mark = t.ops().len();
    t.job(0, shell(1, 4100), Ok(&format!("{DIR}/src")), true);
    assert_eq!(t.answers(), vec![(10, TerminalAtAnswer::Already)]);
    assert!(t.work_since(mark).is_empty(), "{:#?}", t.since(mark));
}

#[test]
fn a_busy_job_elsewhere_is_busy() {
    let t = running(two(), 4100);
    t.at(DIR, 11).unwrap();
    t.at(DIR, 12).unwrap();
    let mark = t.ops().len();
    t.job(0, shell(1, 4100), Ok("/w/work/rust/ch03-01"), true);
    // A sibling whose name starts with the directory's is not inside it.
    t.job(1, shell(1, 4100), Ok(&format!("{DIR}-old")), true);
    assert_eq!(
        t.answers(),
        vec![
            (11, TerminalAtAnswer::Busy(RUNNING_TEXT.into())),
            (12, TerminalAtAnswer::Busy(RUNNING_TEXT.into()))
        ]
    );
    assert!(t.work_since(mark).is_empty(), "{:#?}", t.since(mark));
}

#[test]
fn an_idle_job_elsewhere_respawns_and_answers_restarted_once_started() {
    let t = running(two(), 4100);
    t.at(DIR, 13).unwrap();
    let mark = t.ops().len();
    t.job(0, shell(1, 4100), Ok(&format!("{DIR}-old")), false);
    assert_eq!(
        t.work_since(mark),
        vec![HostOp::Terminal(TermOp::Respawn {
            win: WIN,
            module: terminal(),
            dir: DIR.into(),
            expect: shell(1, 4100),
        })]
    );
    assert!(t.answers().is_empty(), "answered only once the new shell runs");
    assert!(t.state().1.is_some());
    t.started(4101);
    assert_eq!(t.answers(), vec![(13, TerminalAtAnswer::Restarted)]);
    assert_eq!(t.state(), (vec![], None));
    assert_eq!(t.run(), Some(TermRun::Running { pid: 4101 }));
    assert!(t.shown(), "the terminal stays where it was");
}

#[test]
fn a_job_for_a_shell_the_record_no_longer_runs_is_the_terminal_changed() {
    // Another pid than the record's, with the directory elsewhere and with it inside `dir` alike.
    for cwd in ["/w/work/rust/ch03-01".to_string(), format!("{DIR}/src")] {
        let t = running(two(), 4100);
        t.at(DIR, 14).unwrap();
        let mark = t.ops().len();
        t.job(0, shell(1, 4200), Ok(&cwd), false);
        assert_eq!(t.answers(), vec![(14, TerminalAtAnswer::Busy(CHANGED_TEXT.into()))]);
        assert!(t.work_since(mark).is_empty(), "{:#?}", t.since(mark));
    }
    // A new shell started between the read and its answer, even one the capture names.
    let t = running(two(), 4100);
    t.at(DIR, 15).unwrap();
    t.started(4300);
    t.job(0, shell(2, 4300), Ok(&format!("{DIR}/src")), false);
    assert_eq!(t.answers(), vec![(15, TerminalAtAnswer::Busy(CHANGED_TEXT.into()))]);
}

#[test]
fn a_cwd_err_is_the_terminal_changed() {
    let t = running(two(), 4100);
    t.at(DIR, 16).unwrap();
    let mark = t.ops().len();
    t.job(0, shell(1, 4100), Err("No such file or directory"), false);
    assert_eq!(t.answers(), vec![(16, TerminalAtAnswer::Busy(CHANGED_TEXT.into()))]);
    assert!(t.work_since(mark).is_empty(), "{:#?}", t.since(mark));
}

#[test]
fn no_job_by_the_deadline_answers_busy_and_a_late_job_is_ignored() {
    let t = running(two(), 4100);
    t.at(DIR, 17).unwrap();
    t.scheduler.advance(ANSWER_WAIT - Duration::from_millis(1));
    assert_eq!(t.state().0.len(), 1, "still within the deadline");
    assert!(t.answers().is_empty());
    t.scheduler.advance(Duration::from_millis(1));
    assert_eq!(t.state(), (vec![], None));
    assert_eq!(t.answers(), vec![(17, TerminalAtAnswer::Busy(SLOW_TEXT.into()))]);
    let mark = t.ops().len();
    t.job(0, shell(1, 4100), Ok("/w/work/rust/ch03-01"), false);
    assert_eq!(t.answers().len(), 1, "the page is answered once");
    assert!(t.since(mark).is_empty(), "nothing restarts: {:#?}", t.since(mark));
    assert!(
        t.notes().iter().any(|n| n.contains("no request waits")),
        "{:?}",
        t.notes()
    );
}

#[test]
fn a_refused_respawn_is_the_terminal_changed_and_replays_a_held_exited() {
    let t = running(two(), 4100);
    t.restarting(4100);
    let mark = t.ops().len();
    t.exited();
    assert!(t.since(mark).is_empty(), "held, not a kill: {:#?}", t.since(mark));
    assert!(t.shown());
    t.refuse_respawn();
    assert_eq!(t.answers(), vec![(1, TerminalAtAnswer::Busy(CHANGED_TEXT.into()))]);
    assert!(
        terminal_ops(&t.since(mark)).contains(&HostOp::Terminal(TermOp::HangUp {
            win: WIN,
            module: terminal()
        })),
        "the held end is taken as a kill: {:#?}",
        t.since(mark)
    );
    assert!(!t.shown());
    assert_eq!(t.state(), (vec![], None));

    // Refused with nothing held: the answer alone.
    let t = running(two(), 4100);
    t.restarting(4100);
    let mark = t.ops().len();
    t.refuse_respawn();
    assert_eq!(t.answers(), vec![(1, TerminalAtAnswer::Busy(CHANGED_TEXT.into()))]);
    assert!(t.work_since(mark).is_empty(), "{:#?}", t.since(mark));
    assert!(t.shown());
}

#[test]
fn an_exited_while_pending_restart_stands_is_held_and_dropped_when_started_follows() {
    let t = running(two(), 4100);
    t.restarting(4100);
    let mark = t.ops().len();
    t.exited();
    assert!(t.since(mark).is_empty(), "{:#?}", t.since(mark));
    assert_eq!(
        t.run(),
        Some(TermRun::Running { pid: 4100 }),
        "a held end is not recorded"
    );
    assert!(t.state().1.unwrap().held.is_some());
    t.started(4101);
    assert_eq!(t.answers(), vec![(1, TerminalAtAnswer::Restarted)]);
    assert!(t.shown(), "never killed");
    assert!(
        !t.since(mark).iter().any(|op| matches!(
            op,
            HostOp::Terminal(TermOp::HangUp { .. }) | HostOp::CloseWindow { .. } | HostOp::Unmap { .. }
        )),
        "{:#?}",
        t.since(mark)
    );
    assert_eq!(t.run(), Some(TermRun::Running { pid: 4101 }));
    assert!(t.notes().iter().any(|n| n.contains("dropped")), "{:?}", t.notes());
    assert_eq!(t.state(), (vec![], None));
}

/// What the same `Exited` does to a twin window that never had a restart: the ops it carries out, whether it closed
/// and the question it asks.
fn as_taken(layout: Layout) -> (Vec<HostOp>, usize, Option<String>) {
    let twin = running(layout, 4100);
    let mark = twin.ops().len();
    twin.exited();
    (twin.work_since(mark), twin.close_count(), twin.prompt())
}

#[test]
fn a_held_exited_is_replayed_as_a_kill_when_respawn_is_refused() {
    let t = running(two(), 4100);
    t.restarting(4100);
    t.exited();
    let mark = t.ops().len();
    t.refuse_respawn();
    let (ops, closed, prompt) = as_taken(two());
    assert!(ops.contains(&HostOp::Terminal(TermOp::HangUp {
        win: WIN,
        module: terminal()
    })));
    assert_eq!(t.work_since(mark), ops, "taken exactly as it would have been");
    assert_eq!((t.close_count(), t.prompt()), (closed, prompt));
}

#[test]
fn a_held_exited_replayed_as_the_windows_close_when_the_terminal_is_the_last_module() {
    let t = running(alone(), 4100);
    t.restarting(4100);
    t.exited();
    assert_eq!((t.close_count(), t.prompt()), (0, None), "held: no close asked yet");
    let mark = t.ops().len();
    t.refuse_respawn();
    let (ops, closed, prompt) = as_taken(alone());
    assert!(
        t.notes()
            .iter()
            .any(|n| n.contains("the last module on screen closes Eitri")),
        "the held end is taken as the window's close: {:?}",
        t.notes()
    );
    assert_eq!(t.work_since(mark), ops);
    assert_eq!((t.close_count(), t.prompt()), (closed, prompt));
}

#[test]
fn a_pending_restart_past_5_s_replays_with_a_warning() {
    let t = running(two(), 4100);
    t.restarting(4100);
    t.exited();
    let mark = t.ops().len();
    t.scheduler.advance(ANSWER_WAIT - Duration::from_millis(1));
    assert!(t.state().1.is_some(), "still waiting");
    assert!(t.since(mark).is_empty());
    t.scheduler.advance(Duration::from_millis(1));
    assert_eq!(t.state(), (vec![], None));
    assert!(
        t.notes().iter().any(|n| n.contains("no answer within")),
        "{:?}",
        t.notes()
    );
    let (ops, _, _) = as_taken(two());
    assert_eq!(t.work_since(mark), ops, "the held end taken as it came");
    assert_eq!(
        t.answers(),
        vec![(1, TerminalAtAnswer::Busy(RESTART_LATE_TEXT.into()))],
        "the page hears the restart did not finish in time, from the window"
    );
}

/// The read and the restart share one deadline: a restart gets only what the read left of it, so the page is never
/// told the terminal did not answer while a restart it asked for is still on its way.
#[test]
fn a_restart_after_a_slow_read_waits_only_for_what_is_left_of_the_deadline() {
    let t = running(two(), 4100);
    t.at(DIR, 40).unwrap();
    t.scheduler.advance(Duration::from_secs(3));
    let mark = t.ops().len();
    t.job(0, shell(1, 4100), Ok("/w/work/rust/ch03-01"), false);
    assert!(
        t.since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::Terminal(TermOp::Respawn { .. }))),
        "two seconds left is enough for a restart: {:#?}",
        t.since(mark)
    );
    t.scheduler.advance(Duration::from_secs(2) - Duration::from_millis(1));
    assert!(t.state().1.is_some(), "still waiting for the new shell");
    t.scheduler.advance(Duration::from_millis(1));
    assert_eq!(t.state(), (vec![], None));
    assert_eq!(
        t.answers(),
        vec![(40, TerminalAtAnswer::Busy(RESTART_LATE_TEXT.into()))]
    );
}

/// A read that comes back with less than a restart's reserve left touches nothing: a restart then could finish
/// only after the request's deadline.
#[test]
fn a_read_back_with_too_little_time_left_restarts_nothing() {
    let t = running(two(), 4100);
    t.at(DIR, 41).unwrap();
    t.scheduler
        .advance(ANSWER_WAIT - RESTART_RESERVE + Duration::from_millis(1));
    let mark = t.ops().len();
    t.job(0, shell(1, 4100), Ok("/w/work/rust/ch03-01"), false);
    assert!(t.work_since(mark).is_empty(), "{:#?}", t.since(mark));
    assert_eq!(t.state(), (vec![], None));
    assert_eq!(t.answers(), vec![(41, TerminalAtAnswer::Busy(SLOW_TEXT.into()))]);
    assert!(t.notes().iter().any(|n| n.contains("not restarted")), "{:?}", t.notes());
}

/// The window decides whose page may move its terminal, not only the asking host: a plugin that is not this
/// window's own, or any plugin in a window that has none, is refused before anything is read.
#[test]
fn a_plugin_that_is_not_the_windows_own_is_refused_now() {
    let t = running(two(), 4100);
    let mark = t.ops().len();
    assert_eq!(
        t.driver.plugin_terminal_at(WIN, "other", PathBuf::from(DIR), 42),
        Err(NOT_OWN_WINDOW_TEXT.to_string())
    );
    t.driver.test_batch(|c| {
        c.window_mut(WIN).unwrap().own_window = None;
        Batch::default()
    });
    assert_eq!(t.at(DIR, 43), Err(NOT_OWN_WINDOW_TEXT.to_string()));
    assert!(t.since(mark).is_empty(), "{:#?}", t.since(mark));
    assert!(t.state().0.is_empty());
}

#[test]
fn the_batch_for_a_restart_holds_respawn_and_no_layout_focus_or_surface_op() {
    let mut r = rig(two());
    r.controller.window_mut(WIN).unwrap().own_window = Some(PLUGIN.into());
    r.controller.handle(HostEvent::Terminal(TermEvent::Started {
        win: WIN,
        module: terminal(),
        pid: 4100,
    }));
    let (taken, read) = r.controller.plugin_terminal_at(WIN, PLUGIN, DIR.into(), 1);
    assert_eq!(taken, Ok(()));
    assert_eq!(
        crate::wm::verbs::rig::ops(&read),
        vec![HostOp::Terminal(TermOp::ReadJob {
            win: WIN,
            module: terminal(),
            read: 0
        })]
    );
    let batch = r.controller.handle(HostEvent::Terminal(TermEvent::Job {
        win: WIN,
        module: terminal(),
        read: 0,
        shell: shell(1, 4100),
        cwd: Ok("/w/work/rust/ch03-01".into()),
        busy: false,
    }));
    assert_eq!(
        crate::wm::verbs::rig::ops(&batch),
        vec![HostOp::Terminal(TermOp::Respawn {
            win: WIN,
            module: terminal(),
            dir: DIR.into(),
            expect: shell(1, 4100),
        })]
    );
    assert!(crate::wm::verbs::rig::moves(&batch).is_empty());
    assert!(batch.lua().is_empty());
    // The new shell's start answers the page and changes nothing on screen either.
    let batch = r.controller.handle(HostEvent::Terminal(TermEvent::Started {
        win: WIN,
        module: terminal(),
        pid: 4101,
    }));
    assert!(crate::wm::verbs::rig::moves(&batch).is_empty());
    assert!(!batch.ops().any(|op| matches!(
        op,
        HostOp::Map { .. }
            | HostOp::Unmap { .. }
            | HostOp::Allocate { .. }
            | HostOp::CreateSurface { .. }
            | HostOp::DropSurface { .. }
            | HostOp::Terminal(TermOp::HangUp { .. } | TermOp::Start { .. })
    )));
}

#[test]
fn with_the_fake_hosts_shell_a_restart_runs_end_to_end() {
    let t = running(two(), 4100);
    t.host.set_shell(
        WIN,
        terminal(),
        Some(FakeShell {
            shell: shell(1, 4100),
            cwd: Ok("/w/work/rust/ch03-01".into()),
            busy: false,
        }),
    );
    t.at(DIR, 20).unwrap();
    assert_eq!(t.answers(), vec![(20, TerminalAtAnswer::Restarted)]);
    let now = t.host.shell(WIN, &terminal()).unwrap();
    assert_eq!((now.shell, now.cwd), (shell(2, 4101), Ok(PathBuf::from(DIR))));
    // Asked again, it is already there.
    t.at(DIR, 21).unwrap();
    assert_eq!(t.answers()[1], (21, TerminalAtAnswer::Already));
}

#[test]
fn the_fake_host_refuses_a_respawn_whose_shell_is_gone() {
    let t = running(two(), 4100);
    t.at(DIR, 22).unwrap();
    // The pane started another shell after the read captured this one.
    t.host.set_shell(
        WIN,
        terminal(),
        Some(FakeShell {
            shell: shell(2, 4200),
            cwd: Ok("/w/work".into()),
            busy: false,
        }),
    );
    t.job(0, shell(1, 4100), Ok("/w/work/rust/ch03-01"), false);
    assert_eq!(t.answers(), vec![(22, TerminalAtAnswer::Busy(CHANGED_TEXT.into()))]);
    assert_eq!(
        t.host.shell(WIN, &terminal()).unwrap().shell,
        shell(2, 4200),
        "nothing touched"
    );
}

/// A key typed into the shell after the read that found it idle may have started a command in that same shell, with
/// its pid unchanged: the restart is refused and the page told the terminal changed.
#[test]
fn the_fake_host_refuses_a_respawn_when_the_shell_was_typed_into_after_the_read() {
    let t = running(two(), 4100);
    t.at(DIR, 23).unwrap();
    let typed = FakeShell {
        shell: shell(1, 4100).typed(1),
        cwd: Ok("/w/work".into()),
        busy: true,
    };
    t.host.set_shell(WIN, terminal(), Some(typed.clone()));
    t.job(0, shell(1, 4100), Ok("/w/work/rust/ch03-01"), false);
    assert_eq!(t.answers(), vec![(23, TerminalAtAnswer::Busy(CHANGED_TEXT.into()))]);
    assert_eq!(t.host.shell(WIN, &terminal()), Some(typed), "nothing touched");
}

#[test]
fn a_request_while_a_restart_waits_or_with_no_terminal_is_refused_now() {
    let t = running(two(), 4100);
    t.restarting(4100);
    assert_eq!(t.at(DIR, 30), Err(RESTARTING_TEXT.to_string()));
    let t = driven(Layout::new(Node::Leaf(ModuleId::editor()), ModuleId::editor()).unwrap());
    let mark = t.ops().len();
    assert_eq!(t.at(DIR, 31), Err(NO_TERMINAL_TEXT.to_string()));
    assert!(t.since(mark).is_empty());
}
