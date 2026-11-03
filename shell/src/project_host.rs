//! Project reads run off the window thread; serialled answers cross back through channels awaited on the main loop.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use eitri_core::project_chooser::read::{READ_SLOW, READ_STUCK};
use eitri_core::project_chooser::{LinkKind, Listing, Row, READ_DEBOUNCE};
use eitri_core::recent_projects::Sources;
use eitri_core::wm::{Applied, ChooserEnv, HostEvent, HostSink, ProjectEvent, ProjectOp, WinId};
use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use gtk4::glib;
use gtk4::prelude::*;

use crate::gtk_host::project_chooser::ProjectChooserView;
use crate::project_reader::{Desk, ReadAnswer, ReadJob, Reader};
use crate::project_start::{self, How};

#[derive(Debug)]
pub(crate) enum WorkerAnswer {
    Rows {
        serial: u64,
        rows: Vec<Row>,
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
    /// Starts still being watched.
    opens: usize,
}

impl Carrier {
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
}

pub(crate) struct ProjectWorkers {
    inner: Rc<Workers>,
}

struct Workers {
    sources: Arc<Sources>,
    answer: Rc<dyn Fn(WorkerAnswer)>,
    /// Rows and starts' ends, awaited on the main loop.
    answers: UnboundedSender<WorkerAnswer>,
    /// The folder reader's answers, awaited on the main loop.
    reads: UnboundedSender<ReadAnswer>,
    /// The two receivers until the first request starts their futures.
    listening: RefCell<Option<(UnboundedReceiver<WorkerAnswer>, UnboundedReceiver<ReadAnswer>)>>,
    reader: Reader,
    desk: RefCell<Desk>,
    carrier: RefCell<Carrier>,
}

impl ProjectWorkers {
    pub(crate) fn new(sources: Sources, answer: Rc<dyn Fn(WorkerAnswer)>) -> ProjectWorkers {
        ProjectWorkers::with_reader(sources, answer, Reader::global().clone())
    }

    fn with_reader(sources: Sources, answer: Rc<dyn Fn(WorkerAnswer)>, reader: Reader) -> ProjectWorkers {
        let (answers, answers_rx) = unbounded();
        let (reads, reads_rx) = unbounded();
        ProjectWorkers {
            inner: Rc::new(Workers {
                sources: Arc::new(sources),
                answer,
                answers,
                reads,
                listening: RefCell::new(Some((answers_rx, reads_rx))),
                reader,
                desk: RefCell::new(Desk::default()),
                carrier: RefCell::new(Carrier::default()),
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
}

/// What the window hears of a worker's answer. A start that went well says nothing: the window moving is the answer.
fn project_event(win: WinId, answer: WorkerAnswer) -> Option<ProjectEvent> {
    Some(match answer {
        WorkerAnswer::Rows { serial, rows } => ProjectEvent::Rows { win, serial, rows },
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
    use std::time::Instant;

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
    fn a_start_is_heard_once_and_keeps_no_poll() {
        let mut carrier = Carrier {
            opens: 2,
            ..Carrier::default()
        };
        let quiet = || WorkerAnswer::Started { failed: None };
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
        let until = Instant::now() + Duration::from_secs(30);
        while heard.borrow().is_empty() && Instant::now() < until {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(
            &heard.borrow()[..],
            [WorkerAnswer::Started { failed: Some(StartFailed { path, code: None, gone: true }) }] if path == Path::new("/work/app")
        ));
    }

    #[test]
    fn rows_arrive_once_and_leave_the_context_idle() {
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
        assert_eq!(listings(&context, &heard), [(2, vec!["two".to_owned()])]);

        // Typing: the older job is replaced while it waits, and only the newer one is read.
        workers.read(job(3, "a"), true);
        workers.read(job(4, "b"), true);
        heard.borrow_mut().clear();
        assert_eq!(listings(&context, &heard), [(4, vec!["two".to_owned()])]);
    }
}
