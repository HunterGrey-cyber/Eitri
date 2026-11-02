//! Project reads run off the window thread; only serialled answers cross back to the view.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eitri_core::project_chooser::path::{ANSWER_WITHIN, DEBOUNCE};
use eitri_core::project_chooser::{PathAnswer, Row};
use eitri_core::recent_projects::Sources;
use eitri_core::wm::{Applied, ChooserEnv, HostEvent, HostSink, ProjectEvent, ProjectOp, WinId};
use gtk4::glib;
use gtk4::prelude::*;

use crate::gtk_host::project_chooser::ProjectChooserView;

const POLL_EVERY: Duration = Duration::from_millis(10);
static PATH_BUSY: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub(crate) enum WorkerAnswer {
    Rows { serial: u64, rows: Vec<Row> },
    Path { serial: u64, answer: PathAnswer },
    Folder(Option<PathBuf>),
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

#[derive(Default)]
struct Carrier {
    lists: BTreeSet<u64>,
    path: Option<PendingPath>,
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

    fn outstanding(&self) -> bool {
        !self.lists.is_empty() || self.path.is_some()
    }

    fn tick(
        &mut self,
        now: Instant,
        received: impl IntoIterator<Item = WorkerAnswer>,
    ) -> (Vec<WorkerAnswer>, Option<PathJob>) {
        let mut answers = Vec::new();
        for answer in received {
            match &answer {
                WorkerAnswer::Rows { serial, .. } if self.lists.remove(serial) => answers.push(answer),
                WorkerAnswer::Path { serial, .. } if self.path.as_ref().is_some_and(|p| p.job.serial == *serial) => {
                    self.path = None;
                    answers.push(answer);
                }
                _ => {}
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
    sender: mpsc::Sender<WorkerAnswer>,
    receiver: mpsc::Receiver<WorkerAnswer>,
    carrier: RefCell<Carrier>,
    poll: RefCell<Option<glib::SourceId>>,
}

impl ProjectWorkers {
    pub(crate) fn new(sources: Sources, answer: Rc<dyn Fn(WorkerAnswer)>) -> ProjectWorkers {
        let (sender, receiver) = mpsc::channel();
        ProjectWorkers {
            inner: Rc::new(Workers {
                sources: Arc::new(sources),
                answer,
                sender,
                receiver,
                carrier: RefCell::new(Carrier::default()),
                poll: RefCell::new(None),
            }),
        }
    }

    pub(crate) fn list(&self, serial: u64) {
        self.inner.carrier.borrow_mut().lists.insert(serial);
        let sources = self.inner.sources.clone();
        let sender = self.inner.sender.clone();
        if std::thread::Builder::new()
            .name("project-list".into())
            .spawn(move || {
                let rows = eitri_core::recent_projects::read(&sources);
                let _ = sender.send(WorkerAnswer::Rows { serial, rows });
            })
            .is_err()
        {
            let _ = self.inner.sender.send(WorkerAnswer::Rows {
                serial,
                rows: Vec::new(),
            });
        }
        Workers::arm(&self.inner);
    }

    pub(crate) fn path(&self, serial: u64, text: String, base: PathBuf) {
        let replaced = self.inner.carrier.borrow_mut().path(serial, text, base, Instant::now());
        Workers::arm(&self.inner);
        if let Some(answer) = replaced {
            (self.inner.answer)(answer);
        }
    }

    pub(crate) fn folder_dialog(&self, parent: &gtk4::Window) {
        let answer = self.inner.answer.clone();
        gtk4::FileDialog::new().select_folder(Some(parent), None::<&gtk4::gio::Cancellable>, move |result| {
            answer(WorkerAnswer::Folder(result.ok().and_then(|file| file.path())));
        });
    }
}

impl Workers {
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
                let event = match answer {
                    WorkerAnswer::Rows { serial, rows } => ProjectEvent::Rows { win, serial, rows },
                    WorkerAnswer::Path { serial, answer } => ProjectEvent::PathAnswer { win, serial, answer },
                    WorkerAnswer::Folder(path) => ProjectEvent::Folder { win, path },
                };
                sink.post(HostEvent::Project(event));
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
            ProjectOp::FolderDialog { win } if *win == self.win => {
                let Some(window) = self.window.upgrade() else {
                    return Applied::Refused;
                };
                self.workers.folder_dialog(&window);
            }
            _ => return Applied::Refused,
        }
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
        carrier.path(4, "./new".into(), "/base".into(), now);
        assert!(carrier.outstanding());
        let (answers, _) = carrier.tick(
            now,
            [WorkerAnswer::Rows {
                serial: 3,
                rows: vec![],
            }],
        );
        assert_eq!(answers.len(), 1);
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
            .sender
            .send(WorkerAnswer::Rows {
                serial: 8,
                rows: vec![],
            })
            .unwrap();
        Workers::arm(&workers.inner);
        assert!(workers.inner.poll.borrow().is_some());
        context.iteration(true);
        assert_eq!(heard.borrow().len(), 1);
        assert!(
            workers.inner.poll.borrow().is_none(),
            "the finished source is forgotten"
        );
        assert!(!workers.inner.carrier.borrow().outstanding());
        assert!(!context.pending(), "no timer keeps an idle window polling");
    }
}
