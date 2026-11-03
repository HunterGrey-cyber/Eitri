//! Project reads run off the window thread; only serialled answers cross back to the view. Every answer but the typed
//! path's comes through a channel that a future on the main loop awaits; the typed path's worker keeps its poll.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eitri_core::project_chooser::path::{ANSWER_WITHIN, DEBOUNCE};
use eitri_core::project_chooser::read::{READ_SLOW, READ_STUCK};
use eitri_core::project_chooser::{LinkKind, Listing, PathAnswer, Row, READ_DEBOUNCE};
use eitri_core::recent_projects::Sources;
use eitri_core::wm::{Applied, ChooserEnv, HostEvent, HostSink, ProjectEvent, ProjectOp, WinId};
use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use gtk4::glib;
use gtk4::prelude::*;

use crate::gtk_host::project_chooser::ProjectChooserView;
use crate::project_reader::{Desk, ReadAnswer, ReadJob, Reader};
use crate::project_start::{self, How};

const POLL_EVERY: Duration = Duration::from_millis(10);
static PATH_BUSY: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub(crate) enum WorkerAnswer {
    Rows {
        serial: u64,
        rows: Vec<Row>,
    },
    Path {
        serial: u64,
        answer: PathAnswer,
    },
    Listing {
        serial: u64,
        dir: PathBuf,
        listing: Listing,
    },
    Later {
        serial: u64,
        git: Vec<OsString>,
        links: Vec<(OsString, LinkKind)>,
    },
    /// The read of `serial` has taken longer than `READ_SLOW`.
    Slow {
        serial: u64,
    },
    /// The read of `serial` has taken `READ_STUCK`; its listing may still come.
    Stuck {
        serial: u64,
    },
    Freed {
        mount: PathBuf,
    },
    Folder(Option<PathBuf>),
    /// A start's watch has ended; `failed` holds what the window has to say about it.
    Started {
        failed: Option<StartFailed>,
    },
}

/// A project that could not be started, the child's exit code when it got as far as having one, and whether the folder
/// was gone by the time the start ended.
#[derive(Debug, PartialEq)]
pub(crate) struct StartFailed {
    path: PathBuf,
    code: Option<i32>,
    gone: bool,
}

struct PathJob {
    serial: u64,
    text: String,
    base: PathBuf,
}

struct PendingPath {
    job: PathJob,
    ready: Instant,
    deadline: Instant,
    started: bool,
}

impl From<ReadAnswer> for WorkerAnswer {
    fn from(answer: ReadAnswer) -> WorkerAnswer {
        match answer {
            ReadAnswer::Listing { serial, dir, listing } => WorkerAnswer::Listing { serial, dir, listing },
            ReadAnswer::Later { serial, git, links } => WorkerAnswer::Later { serial, git, links },
            ReadAnswer::Freed { mount } => WorkerAnswer::Freed { mount },
        }
    }
}

#[derive(Default)]
struct Carrier {
    lists: BTreeSet<u64>,
    path: Option<PendingPath>,
    /// Starts still being watched.
    opens: usize,
}

impl Carrier {
    fn path(&mut self, serial: u64, text: String, base: PathBuf, now: Instant) -> Option<WorkerAnswer> {
        self.path
            .replace(PendingPath {
                job: PathJob { serial, text, base },
                ready: now + DEBOUNCE,
                // Set when the worker starts: its bound counts from there, not from the keystroke.
                deadline: now,
                started: false,
            })
            .map(|old| WorkerAnswer::Path {
                serial: old.job.serial,
                answer: PathAnswer::Stuck,
            })
    }

    /// Whether the typed path's poll has anything to wait for.
    fn outstanding(&self) -> bool {
        self.path.is_some()
    }

    /// A list's rows or a start's end, from the channel: each is owed once.
    fn accept(&mut self, answer: WorkerAnswer) -> Option<WorkerAnswer> {
        match &answer {
            WorkerAnswer::Rows { serial, .. } if self.lists.remove(serial) => Some(answer),
            WorkerAnswer::Started { .. } if self.opens > 0 => {
                self.opens -= 1;
                Some(answer)
            }
            _ => None,
        }
    }

    fn tick(
        &mut self,
        now: Instant,
        received: impl IntoIterator<Item = WorkerAnswer>,
    ) -> (Vec<WorkerAnswer>, Option<PathJob>) {
        let mut answers = Vec::new();
        for answer in received {
            if let WorkerAnswer::Path { serial, .. } = &answer {
                if self.path.as_ref().is_some_and(|p| p.job.serial == *serial) {
                    self.path = None;
                    answers.push(answer);
                }
            }
        }
        // An answer already received is the answer; only a worker that has run its whole bound is stuck.
        if self.path.as_ref().is_some_and(|p| p.started && now >= p.deadline) {
            let pending = self.path.take().unwrap();
            answers.push(WorkerAnswer::Path {
                serial: pending.job.serial,
                answer: PathAnswer::Stuck,
            });
        }
        let job = self.path.as_mut().filter(|p| !p.started && now >= p.ready).map(|p| {
            p.started = true;
            p.deadline = now + ANSWER_WITHIN;
            PathJob {
                serial: p.job.serial,
                text: p.job.text.clone(),
                base: p.job.base.clone(),
            }
        });
        (answers, job)
    }
}

pub(crate) struct ProjectWorkers {
    inner: Rc<Workers>,
}

struct Workers {
    sources: Arc<Sources>,
    answer: Rc<dyn Fn(WorkerAnswer)>,
    /// The typed path's worker, read by the poll.
    sender: mpsc::Sender<WorkerAnswer>,
    receiver: mpsc::Receiver<WorkerAnswer>,
    /// Rows and starts' ends, awaited on the main loop.
    answers: UnboundedSender<WorkerAnswer>,
    /// The folder reader's answers, awaited on the main loop.
    reads: UnboundedSender<ReadAnswer>,
    /// The two receivers until the first request starts their futures.
    listening: RefCell<Option<(UnboundedReceiver<WorkerAnswer>, UnboundedReceiver<ReadAnswer>)>>,
    reader: Reader,
    desk: RefCell<Desk>,
    carrier: RefCell<Carrier>,
    poll: RefCell<Option<glib::SourceId>>,
}

impl ProjectWorkers {
    pub(crate) fn new(sources: Sources, answer: Rc<dyn Fn(WorkerAnswer)>) -> ProjectWorkers {
        ProjectWorkers::with_reader(sources, answer, Reader::global().clone())
    }

    fn with_reader(sources: Sources, answer: Rc<dyn Fn(WorkerAnswer)>, reader: Reader) -> ProjectWorkers {
        let (sender, receiver) = mpsc::channel();
        let (answers, answers_rx) = unbounded();
        let (reads, reads_rx) = unbounded();
        ProjectWorkers {
            inner: Rc::new(Workers {
                sources: Arc::new(sources),
                answer,
                sender,
                receiver,
                answers,
                reads,
                listening: RefCell::new(Some((answers_rx, reads_rx))),
                reader,
                desk: RefCell::new(Desk::default()),
                carrier: RefCell::new(Carrier::default()),
                poll: RefCell::new(None),
            }),
        }
    }

    pub(crate) fn list(&self, serial: u64) {
        Workers::listen(&self.inner);
        self.inner.carrier.borrow_mut().lists.insert(serial);
        let sources = self.inner.sources.clone();
        let sender = self.inner.answers.clone();
        if std::thread::Builder::new()
            .name("project-list".into())
            .spawn(move || {
                let rows = eitri_core::recent_projects::read(&sources);
                let _ = sender.unbounded_send(WorkerAnswer::Rows { serial, rows });
            })
            .is_err()
        {
            let _ = self.inner.answers.unbounded_send(WorkerAnswer::Rows {
                serial,
                rows: Vec::new(),
            });
        }
    }

    /// Reads `job.dir` on the process's reader. A newer read supersedes this one; one from typing waits
    /// `READ_DEBOUNCE` first.
    pub(crate) fn read(&self, job: ReadJob, debounce: bool) {
        let serial = job.serial;
        let now = self.inner.desk.borrow_mut().ask(job, debounce);
        match now {
            Some(job) => Workers::start_read(&self.inner, job),
            None => {
                let weak = Rc::downgrade(&self.inner);
                glib::timeout_add_local_once(READ_DEBOUNCE, move || {
                    let Some(this) = weak.upgrade() else { return };
                    let due = this.desk.borrow_mut().due(serial);
                    if let Some(job) = due {
                        Workers::start_read(&this, job);
                    }
                });
            }
        }
    }

    pub(crate) fn path(&self, serial: u64, text: String, base: PathBuf) {
        let replaced = self.inner.carrier.borrow_mut().path(serial, text, base, Instant::now());
        Workers::arm(&self.inner);
        if let Some(answer) = replaced {
            (self.inner.answer)(answer);
        }
    }

    /// Starts `target`'s window off this thread, and answers once the start has failed or its window has passed.
    pub(crate) fn open(&self, target: PathBuf, token: Option<String>) {
        let exe = || project_start::exe_to_start(std::env::current_exe(), &|path| path.exists());
        self.open_with(exe, target, token, project_start::FAILED_WITHIN);
    }

    fn open_with(
        &self,
        exe: impl FnOnce() -> PathBuf + Send + 'static,
        target: PathBuf,
        token: Option<String>,
        window: Duration,
    ) {
        Workers::listen(&self.inner);
        self.inner.carrier.borrow_mut().opens += 1;
        let path = target.clone();
        let sender = self.inner.answers.clone();
        if std::thread::Builder::new()
            .name("project-open".into())
            .spawn(move || {
                let start = project_start::start(&exe(), &[], How::Raise(target.clone()), token.as_deref());
                let failed = project_start::failure(start, window).map(|code| StartFailed {
                    gone: project_start::not_there(&target),
                    path: target,
                    code,
                });
                let _ = sender.unbounded_send(WorkerAnswer::Started { failed });
            })
            .is_err()
        {
            let failed = Some(StartFailed {
                path,
                code: None,
                gone: false,
            });
            let _ = self.inner.answers.unbounded_send(WorkerAnswer::Started { failed });
        }
    }

    pub(crate) fn folder_dialog(&self, parent: &gtk4::Window, at: Option<&Path>) {
        let answer = self.inner.answer.clone();
        let dialog = gtk4::FileDialog::new();
        if let Some(at) = at {
            dialog.set_initial_folder(Some(&gtk4::gio::File::for_path(at)));
        }
        dialog.select_folder(Some(parent), None::<&gtk4::gio::Cancellable>, move |result| {
            answer(WorkerAnswer::Folder(result.ok().and_then(|file| file.path())));
        });
    }
}

impl Workers {
    /// Starts the two futures that deliver answers on the main loop, once. Each holds the workers weakly and ends with
    /// its channel; a send to a channel whose future has gone is dropped.
    fn listen(this: &Rc<Self>) {
        let Some((mut answers, mut reads)) = this.listening.borrow_mut().take() else {
            return;
        };
        let weak = Rc::downgrade(this);
        glib::spawn_future_local(async move {
            while let Ok(answer) = answers.recv().await {
                let Some(this) = weak.upgrade() else { break };
                let accepted = this.carrier.borrow_mut().accept(answer);
                if let Some(answer) = accepted {
                    (this.answer)(answer);
                }
            }
        });
        let weak = Rc::downgrade(this);
        glib::spawn_future_local(async move {
            while let Ok(answer) = reads.recv().await {
                let Some(this) = weak.upgrade() else { break };
                // The desk's borrow ends before the answer goes on: the chooser may ask for the next read at once.
                let taken = this.desk.borrow_mut().take(answer);
                if let Some((answer, lines)) = taken {
                    for line in lines {
                        println!("{line}");
                    }
                    (this.answer)(answer.into());
                }
            }
        });
    }

    /// Hands `job` to the reader and arms its one-shot slow and stuck timers.
    fn start_read(this: &Rc<Self>, job: ReadJob) {
        Workers::listen(this);
        let serial = job.serial;
        let ticket = this.reader.request(job, this.reads.clone());
        this.desk.borrow_mut().started(serial, ticket);
        let weak = Rc::downgrade(this);
        glib::timeout_add_local_once(READ_SLOW, move || {
            let Some(this) = weak.upgrade() else { return };
            let slow = this.desk.borrow().slow(serial);
            if slow {
                (this.answer)(WorkerAnswer::Slow { serial });
            }
        });
        let weak = Rc::downgrade(this);
        glib::timeout_add_local_once(READ_STUCK, move || {
            let Some(this) = weak.upgrade() else { return };
            let stuck = this.desk.borrow_mut().stuck(serial);
            if let Some(line) = stuck {
                println!("{line}");
                (this.answer)(WorkerAnswer::Stuck { serial });
            }
        });
    }

    fn arm(this: &Rc<Self>) {
        if this.poll.borrow().is_some() || !this.carrier.borrow().outstanding() {
            return;
        }
        let weak = Rc::downgrade(this);
        let source = glib::timeout_add_local(POLL_EVERY, move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let (answers, job) = this.carrier.borrow_mut().tick(Instant::now(), this.receiver.try_iter());
            if let Some(job) = job {
                this.start_path(job);
            }
            // Forget a finished source before delivering callbacks: a callback may request more work.
            let keep = this.carrier.borrow().outstanding();
            if !keep {
                this.poll.borrow_mut().take();
            }
            for answer in answers {
                (this.answer)(answer);
            }
            if keep {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
        *this.poll.borrow_mut() = Some(source);
    }

    fn start_path(&self, job: PathJob) {
        let serial = job.serial;
        if PATH_BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            let _ = self.sender.send(WorkerAnswer::Path {
                serial,
                answer: PathAnswer::Stuck,
            });
            return;
        }
        let busy = PathBusy;
        let sender = self.sender.clone();
        let home = self.sources.home.clone();
        if std::thread::Builder::new()
            .name("project-path".into())
            .spawn(move || {
                let _busy = busy;
                let answer = eitri_core::project_chooser::path::answer(&job.text, &job.base, home.as_deref());
                let _ = sender.send(WorkerAnswer::Path { serial, answer });
            })
            .is_err()
        {
            let _ = self.sender.send(WorkerAnswer::Path {
                serial,
                answer: PathAnswer::Stuck,
            });
        }
    }
}

struct PathBusy;
impl Drop for PathBusy {
    fn drop(&mut self) {
        PATH_BUSY.store(false, Ordering::Release);
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        if let Some(source) = self.poll.get_mut().take() {
            source.remove();
        }
    }
}

/// What the window hears of a worker's answer. A start that went well says nothing: the window moving is the answer.
fn project_event(win: WinId, answer: WorkerAnswer) -> Option<ProjectEvent> {
    Some(match answer {
        WorkerAnswer::Rows { serial, rows } => ProjectEvent::Rows { win, serial, rows },
        WorkerAnswer::Path { serial, answer } => ProjectEvent::PathAnswer { win, serial, answer },
        WorkerAnswer::Listing { serial, dir, listing } => ProjectEvent::Listing {
            win,
            serial,
            dir,
            listing,
        },
        WorkerAnswer::Later { serial, git, links } => ProjectEvent::Later {
            win,
            serial,
            git,
            links,
        },
        WorkerAnswer::Slow { serial } => ProjectEvent::Slow { win, serial },
        WorkerAnswer::Stuck { serial } => ProjectEvent::Stuck { win, serial },
        WorkerAnswer::Freed { mount } => ProjectEvent::Freed { win, mount },
        WorkerAnswer::Folder(path) => ProjectEvent::Folder { win, path },
        WorkerAnswer::Started { failed } => {
            let StartFailed { path, code, gone } = failed?;
            ProjectEvent::OpenFailed { win, path, code, gone }
        }
    })
}

/// The window's carrier: it keeps native objects here and only posts facts to the controller.
pub(crate) struct WindowProjects {
    win: WinId,
    window: glib::WeakRef<gtk4::Window>,
    view: Rc<ProjectChooserView>,
    workers: ProjectWorkers,
    env: ChooserEnv,
}

impl WindowProjects {
    pub(crate) fn new(
        win: WinId,
        window: &gtk4::Window,
        view: Rc<ProjectChooserView>,
        sources: Sources,
        sink: Weak<dyn HostSink>,
    ) -> Rc<Self> {
        let env = ChooserEnv {
            home: sources.home.clone(),
            lang: eitri_core::onboarding::Lang::from_env(&|name| std::env::var_os(name)),
        };
        let workers = ProjectWorkers::new(
            sources,
            Rc::new(move |answer| {
                let Some(sink) = sink.upgrade() else {
                    return;
                };
                if let Some(event) = project_event(win, answer) {
                    sink.post(HostEvent::Project(event));
                }
            }),
        );
        Rc::new(Self {
            win,
            window: window.downgrade(),
            view,
            workers,
            env,
        })
    }
}

impl crate::gtk_host::ProjectHost for WindowProjects {
    fn apply(&self, op: &ProjectOp) -> Applied {
        match op {
            ProjectOp::List { win, serial } if *win == self.win => self.workers.list(*serial),
            ProjectOp::Path {
                win,
                serial,
                text,
                base,
            } if *win == self.win => self.workers.path(*serial, text.clone(), base.clone()),
            ProjectOp::Read {
                win,
                serial,
                dir,
                filter,
                debounce,
                near,
            } if *win == self.win => self.workers.read(
                ReadJob {
                    serial: *serial,
                    dir: dir.clone(),
                    filter: filter.clone(),
                    near: near.clone(),
                },
                *debounce,
            ),
            ProjectOp::FolderDialog { win, at } if *win == self.win => {
                let Some(window) = self.window.upgrade() else {
                    return Applied::Refused;
                };
                self.workers.folder_dialog(&window, at.as_deref());
            }
            _ => return Applied::Refused,
        }
        Applied::Done
    }

    fn open(&self, win: WinId, target: &Path, token: Option<String>) -> Applied {
        if win != self.win {
            return Applied::Refused;
        }
        self.workers.open(target.to_path_buf(), token);
        Applied::Done
    }

    fn focus(&self, win: WinId) -> bool {
        if win != self.win {
            return false;
        }
        self.view.focus();
        self.window
            .upgrade()
            .and_then(|window| GtkWindowExt::focus(&window))
            .is_some_and(|focus| focus == *self.view.widget() || focus.is_ancestor(self.view.widget()))
    }

    fn env(&self, _win: WinId) -> ChooserEnv {
        self.env.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_dialog_starts_at_the_given_folder() {
        use eitri_core::source_scan::{code_only, functions, read_required};
        let source = code_only(&read_required("shell/src/project_host.rs").text);
        let bodies = functions(&source, "folder_dialog");
        assert_eq!(bodies.len(), 1);
        let compact: String = bodies[0].chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("ifletSome(at)=at"));
        assert!(compact.contains("dialog.set_initial_folder(Some(&gtk4::gio::File::for_path(at)))"));
    }

    #[test]
    fn a_path_op_always_answers() {
        let now = Instant::now();
        let mut carrier = Carrier::default();
        carrier.path(7, "./held".into(), "/base".into(), now);
        let (answers, job) = carrier.tick(now + DEBOUNCE, []);
        assert!(answers.is_empty());
        assert_eq!(job.unwrap().serial, 7);
        let (answers, job) = carrier.tick(now + DEBOUNCE + ANSWER_WITHIN - Duration::from_millis(1), []);
        assert!(answers.is_empty() && job.is_none(), "the worker has its whole bound");
        let (answers, job) = carrier.tick(now + DEBOUNCE + ANSWER_WITHIN, []);
        assert!(job.is_none());
        assert!(matches!(
            &answers[..],
            [WorkerAnswer::Path {
                serial: 7,
                answer: PathAnswer::Stuck
            }]
        ));
        assert!(!carrier.outstanding());
        let (answers, _) = carrier.tick(
            now + ANSWER_WITHIN * 2,
            [WorkerAnswer::Path {
                serial: 7,
                answer: PathAnswer::Missing,
            }],
        );
        assert!(answers.is_empty(), "a late worker never answers twice");
    }

    #[test]
    fn a_stalled_window_thread_does_not_fail_a_worker_that_never_ran() {
        let now = Instant::now();
        let mut carrier = Carrier::default();
        carrier.path(5, "./slow".into(), "/base".into(), now);
        // The thread comes back long after the debounce and the bound: the worker starts, it is not called stuck.
        let late = now + DEBOUNCE + ANSWER_WITHIN * 3;
        let (answers, job) = carrier.tick(late, []);
        assert!(answers.is_empty());
        assert_eq!(job.unwrap().serial, 5);
        assert!(carrier.outstanding());
        let (answers, _) = carrier.tick(late + ANSWER_WITHIN, []);
        assert!(matches!(
            &answers[..],
            [WorkerAnswer::Path {
                serial: 5,
                answer: PathAnswer::Stuck
            }]
        ));
    }

    #[test]
    fn a_reply_already_received_when_the_bound_passes_is_the_answer() {
        let now = Instant::now();
        let mut carrier = Carrier::default();
        carrier.path(8, "./held".into(), "/base".into(), now);
        assert!(carrier.tick(now + DEBOUNCE, []).1.is_some());
        let (answers, _) = carrier.tick(
            now + DEBOUNCE + ANSWER_WITHIN,
            [WorkerAnswer::Path {
                serial: 8,
                answer: PathAnswer::Missing,
            }],
        );
        assert!(
            matches!(
                &answers[..],
                [WorkerAnswer::Path {
                    serial: 8,
                    answer: PathAnswer::Missing
                }]
            ),
            "the worker answered; the poll only noticed late"
        );
    }

    #[test]
    fn a_second_path_worker_is_refused_while_one_runs_and_the_flag_clears_when_it_ends() {
        let workers = ProjectWorkers::new(
            Sources {
                layout: None,
                tabs: None,
                control: "/unused".into(),
                home: None,
            },
            Rc::new(|_| {}),
        );
        let job = |serial| PathJob {
            serial,
            text: "/".into(),
            base: "/".into(),
        };
        PATH_BUSY.store(true, Ordering::Release);
        workers.inner.start_path(job(1));
        assert!(matches!(
            workers.inner.receiver.try_recv(),
            Ok(WorkerAnswer::Path {
                serial: 1,
                answer: PathAnswer::Stuck
            })
        ));
        assert!(
            PATH_BUSY.load(Ordering::Acquire),
            "a refused start leaves the running worker's flag alone"
        );
        PATH_BUSY.store(false, Ordering::Release);
        workers.inner.start_path(job(2));
        let answer = workers.inner.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(answer, WorkerAnswer::Path { serial: 2, .. }));
        let end = Instant::now() + Duration::from_secs(10);
        while PATH_BUSY.load(Ordering::Acquire) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !PATH_BUSY.load(Ordering::Acquire),
            "the flag clears when the worker ends"
        );
    }

    #[test]
    fn a_start_is_heard_once_and_keeps_no_poll() {
        let mut carrier = Carrier {
            opens: 2,
            ..Carrier::default()
        };
        let quiet = || WorkerAnswer::Started { failed: None };
        assert!(!carrier.outstanding(), "a start's end comes through the channel");
        assert!(carrier.accept(quiet()).is_some());
        assert_eq!(carrier.opens, 1, "the other start is still watched");
        assert!(carrier.accept(quiet()).is_some());
        assert!(carrier.accept(quiet()).is_none(), "nothing is owed");
        let rows = |serial| WorkerAnswer::Rows { serial, rows: vec![] };
        carrier.lists.insert(3);
        assert!(carrier.accept(rows(4)).is_none(), "rows nobody asked for");
        assert!(carrier.accept(rows(3)).is_some());
        assert!(carrier.accept(rows(3)).is_none(), "rows are owed once");
    }

    #[test]
    fn a_failed_start_reaches_the_window_and_one_that_went_well_says_nothing() {
        let win = WinId::MAIN;
        let failed = Some(StartFailed {
            path: "/work/app".into(),
            code: Some(3),
            gone: true,
        });
        assert_eq!(
            project_event(win, WorkerAnswer::Started { failed }),
            Some(ProjectEvent::OpenFailed {
                win,
                path: "/work/app".into(),
                code: Some(3),
                gone: true,
            })
        );
        assert_eq!(project_event(win, WorkerAnswer::Started { failed: None }), None);
    }

    #[test]
    fn a_start_answers_on_the_window_thread_with_no_poll() {
        let _turn = crate::layout_state::default_context_turn();
        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let heard = Rc::new(RefCell::new(Vec::new()));
        let workers = ProjectWorkers::new(
            Sources {
                layout: None,
                tabs: None,
                control: "/unused".into(),
                home: None,
            },
            Rc::new({
                let heard = heard.clone();
                move |answer| heard.borrow_mut().push(answer)
            }),
        );
        // The exit is known at once, so this wide a window would hang here if the worker sat it out.
        workers.open_with(
            || "/nonexistent/eitri".into(),
            "/work/app".into(),
            Some("token".into()),
            Duration::from_secs(3600),
        );
        assert!(workers.inner.poll.borrow().is_none(), "a start needs no poll");
        let until = Instant::now() + Duration::from_secs(30);
        while heard.borrow().is_empty() && Instant::now() < until {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(
            &heard.borrow()[..],
            [WorkerAnswer::Started { failed: Some(StartFailed { path, code: None, gone: true }) }] if path == Path::new("/work/app")
        ));
        assert!(workers.inner.poll.borrow().is_none(), "nothing polled for it");
    }

    #[test]
    fn only_the_newest_path_serial_runs() {
        let now = Instant::now();
        let mut carrier = Carrier::default();
        carrier.path(1, "./old".into(), "/base".into(), now);
        let replaced = carrier.path(2, "./new".into(), "/base".into(), now + DEBOUNCE / 2);
        assert!(matches!(
            replaced,
            Some(WorkerAnswer::Path {
                serial: 1,
                answer: PathAnswer::Stuck
            })
        ));
        assert!(carrier.tick(now + DEBOUNCE, []).1.is_none());
        let (_, job) = carrier.tick(now + DEBOUNCE + DEBOUNCE / 2, []);
        let job = job.unwrap();
        assert_eq!(job.serial, 2);
        assert_eq!(job.text, "./new");
        assert!(carrier.tick(now + DEBOUNCE + DEBOUNCE / 2, []).1.is_none());
    }

    #[test]
    fn the_poll_stops_with_nothing_outstanding() {
        let _turn = crate::layout_state::default_context_turn();
        let now = Instant::now();
        let mut carrier = Carrier::default();
        assert!(!carrier.outstanding());
        carrier.lists.insert(3);
        assert!(!carrier.outstanding(), "rows come through the channel");
        carrier.path(4, "./new".into(), "/base".into(), now);
        assert!(carrier.outstanding());
        assert!(carrier.tick(now + DEBOUNCE, []).1.is_some());
        let (answers, _) = carrier.tick(now + DEBOUNCE + ANSWER_WITHIN, []);
        assert_eq!(answers.len(), 1);
        assert!(!carrier.outstanding());

        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let heard = Rc::new(RefCell::new(Vec::new()));
        let workers = ProjectWorkers::new(
            Sources {
                layout: None,
                tabs: None,
                control: "/unused".into(),
                home: None,
            },
            Rc::new({
                let heard = heard.clone();
                move |answer| heard.borrow_mut().push(answer)
            }),
        );
        Workers::arm(&workers.inner);
        assert!(workers.inner.poll.borrow().is_none());
        workers.inner.carrier.borrow_mut().lists.insert(8);
        workers
            .inner
            .answers
            .unbounded_send(WorkerAnswer::Rows {
                serial: 8,
                rows: vec![],
            })
            .unwrap();
        Workers::listen(&workers.inner);
        Workers::arm(&workers.inner);
        assert!(workers.inner.poll.borrow().is_none(), "rows need no poll");
        let until = Instant::now() + Duration::from_secs(30);
        while heard.borrow().is_empty() && Instant::now() < until {
            context.iteration(true);
        }
        assert_eq!(heard.borrow().len(), 1);
        assert!(!context.pending(), "no timer keeps an idle window polling");
    }

    fn sources() -> Sources {
        Sources {
            layout: None,
            tabs: None,
            control: "/unused".into(),
            home: None,
        }
    }

    /// Iterates the default context until `heard` holds a listing, and returns the listings heard.
    fn listings(context: &glib::MainContext, heard: &RefCell<Vec<WorkerAnswer>>) -> Vec<(u64, Vec<String>)> {
        let listed = |heard: &RefCell<Vec<WorkerAnswer>>| {
            heard
                .borrow()
                .iter()
                .filter_map(|answer| match answer {
                    WorkerAnswer::Listing {
                        serial,
                        listing: Listing::Read { entries, .. },
                        ..
                    } => Some((*serial, entries.iter().map(|e| e.name.clone()).collect())),
                    WorkerAnswer::Listing { listing, .. } => panic!("{listing:?}"),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let until = Instant::now() + Duration::from_secs(30);
        while listed(heard).is_empty() && Instant::now() < until {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        listed(heard)
    }

    #[test]
    fn a_read_answers_on_the_window_thread_and_a_newer_one_supersedes_it() {
        use eitri_core::plugin::test_support::Scratch;
        let _turn = crate::layout_state::default_context_turn();
        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let scratch = Scratch::new("project-host-read");
        std::fs::create_dir_all(scratch.0.join("a/one")).unwrap();
        std::fs::create_dir_all(scratch.0.join("b/two")).unwrap();
        let heard = Rc::new(RefCell::new(Vec::new()));
        let workers = ProjectWorkers::new(
            sources(),
            Rc::new({
                let heard = heard.clone();
                move |answer| heard.borrow_mut().push(answer)
            }),
        );
        let job = |serial, dir: &str| ReadJob {
            serial,
            dir: scratch.0.join(dir),
            filter: None,
            near: None,
        };
        workers.read(job(1, "a"), false);
        workers.read(job(2, "b"), false);
        assert!(workers.inner.poll.borrow().is_none(), "a read needs no poll");
        assert_eq!(listings(&context, &heard), [(2, vec!["two".to_owned()])]);

        // Typing: the older job is replaced while it waits, and only the newer one is read.
        workers.read(job(3, "a"), true);
        workers.read(job(4, "b"), true);
        heard.borrow_mut().clear();
        assert_eq!(listings(&context, &heard), [(4, vec!["two".to_owned()])]);
    }
}
