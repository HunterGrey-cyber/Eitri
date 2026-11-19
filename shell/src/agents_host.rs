//! The agents layer in one window: its rows published, its control socket served, and its requests to other
//! windows sent.
//!
//! When `agents.enabled` is on, a window that holds chat tabs (window 1, window 2, an unpaired window, a chat
//! window of compositor mode, the companion panel) and the compositor hub start a publisher of their rows and bind
//! `ag-<instance>.sock`. Both make files, so they start on a thread of their own and reach the window through
//! the main loop; a failure to start (the directory, the lock, a socket path over the limit) is one `[agents]`
//! line and the window runs without the layer. Once started the layer keeps itself until [`AgentsHost::stop`], so a
//! window that never asks another one anything need not hold its host. The server's thread hands each request to
//! the main loop and waits for its answer there, so `ok` means the reveal ran; a request the main loop did not take
//! up within the deadline is answered `refused: busy` and is never carried out. A request to another window is
//! sent from a new thread and answered on the main loop, so a peer that never answers costs the window nothing. A
//! stop lets the publisher and the server go on a thread of their own: dropping the publisher waits for its last
//! write.
//!
//! The round keys (`prefix u`, `prefix B`) are carried out here too: the view and the shared round are read, the step
//! decided (`eitri_core::wm::agents_round`) and the round written on a thread of its own, and the stop is brought
//! forward from the main loop, in this window or another. So is the round's move after an answer: the panel's glue
//! hands this window's rows over when one stopped waiting, and only the window owning the round's current stop moves
//! it on, with this window's activation token for the next one. A process's round steps read and write the round one
//! at a time, and a step that comes back after a later one was carried out is dropped: an answer's advance can neither
//! bring back a round `prefix B` just left nor move the keys after it.
//!
//! Nothing here answers a card, a question or a trust prompt, sends text or changes a mode: a reveal brings a tab
//! forward, and `new` opens an empty one.

use crate::agent_panel::AgentPanelHandle;
use eitri_core::agents::control::{self, Reply, Request, Server, DEADLINE};
use eitri_core::agents::shared::{should_advance, Round, RoundPoint, SharedStore};
use eitri_core::agents::{
    new_instance, scan_once, Intent, ProjectRef, Publisher, Row, View, WindowRef, LABEL_MAX, SCHEMA_V,
};
use eitri_core::layout::ModuleId;
use eitri_core::tabs::TabId;
use eitri_core::wm::agents_round::{self, NextStep, RoundFile, RoundKey, RoundOutcome};
use eitri_core::wm::{RevealRefusal, RevealRequest};
use eitri_panel::agents_glue::{AgentsGlue, RoundHook};
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// How long the server's thread waits for the main loop's answer before it answers `refused: busy`: the client
/// stops listening after two deadlines.
const ANSWER_WAIT: std::time::Duration = DEADLINE;

/// Whether the layer runs, from `init.lua`'s `agents.enabled` (`None` when it is not set). A value the startup
/// checks refused never reaches a window; were it to, the default holds.
pub(crate) fn enabled(value: Option<&str>) -> bool {
    eitri_core::agent_prefs::parse_agents_enabled(value).unwrap_or(eitri_core::agent_prefs::AGENTS_ENABLED_DEFAULT)
}

/// The control directory every window of this user shares.
pub(crate) fn control_dir() -> PathBuf {
    eitri_core::panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir())
}

/// The project a window's rows name: the key the control sockets use and the folder's name, cut for display.
pub(crate) fn project_ref(project_root: &Path) -> ProjectRef {
    let name = project_root
        .file_name()
        .map_or_else(|| project_root.to_string_lossy(), |n| n.to_string_lossy());
    ProjectRef {
        key: eitri_core::state_paths::project_key(project_root),
        name: eitri_core::agents::wire::cut(&name, LABEL_MAX),
    }
}

/// What a request to this window does on the main loop: the window's own controller and its present.
pub(crate) trait AgentsTarget {
    /// Brings the tab of `req` forward (with no tab, the module that last had the keys) and, when that ran,
    /// presents the window with `token`.
    fn reveal(&self, req: &RevealRequest, token: Option<&str>) -> Reply;
    /// Opens an empty chat tab here, or in compositor mode's hub a new chat window.
    fn new_tab(&self, token: Option<&str>) -> Reply;
}

/// The words a refused reveal is answered with.
pub(crate) fn refusal(why: RevealRefusal) -> Reply {
    Reply::Refused(
        match why {
            RevealRefusal::Gone => "gone",
            RevealRefusal::Moving => "moving",
            RevealRefusal::Idle => "idle",
        }
        .to_owned(),
    )
}

/// Runs the controller's `tab.reveal` for a request.
pub(crate) type RevealFn = Box<dyn Fn(&RevealRequest) -> Result<(), RevealRefusal>>;

/// Brings the window forward with an activation token, when there is one.
pub(crate) type PresentFn = Box<dyn Fn(Option<&str>)>;

/// A window's controller as a target, through closures so a test can stand in for it: `reveal` runs the
/// controller's `tab.reveal`, `new` its `new` with the asking window's token, and `present` brings the window
/// forward with a token (a no-op where the window is already where the token would put it).
pub(crate) struct DriverTarget {
    pub(crate) reveal: RevealFn,
    pub(crate) new: PresentFn,
    pub(crate) present: PresentFn,
    /// Window 1 of compositor mode, which holds no tab: a new chat goes to a new chat window.
    pub(crate) hub: bool,
}

impl AgentsTarget for DriverTarget {
    fn reveal(&self, req: &RevealRequest, token: Option<&str>) -> Reply {
        match (self.reveal)(req) {
            Ok(()) => {
                (self.present)(token);
                Reply::Ok
            }
            Err(why) => refusal(why),
        }
    }

    fn new_tab(&self, token: Option<&str>) -> Reply {
        (self.new)(token);
        // The hub's new chat window comes forward by its own start, with the token; window 1 stays where it is.
        if !self.hub {
            (self.present)(token);
        }
        Reply::Ok
    }
}

/// One request from the server's thread and the way back to it. Its flag is taken by whichever side gets there first:
/// the main loop, which then carries the request out and answers, or the server's thread giving up on a busy loop,
/// after which the request is dropped unrun.
type Pending = (Request, std::sync::mpsc::SyncSender<Reply>, Arc<AtomicBool>);

/// What runs once the start on its thread succeeded.
struct Running {
    _server: Server,
    /// The hub's publisher, which no panel offers rows to; a window's publisher is inside its panel's glue, and a page
    /// window has none.
    _publisher: Option<Publisher>,
}

/// Where a window's rows come from: its panel, which takes the glue and gives back the one it held, or nothing (the
/// hub publishes no rows).
pub(crate) type RowsTo = Option<Rc<dyn Fn(Option<AgentsGlue>) -> Option<AgentsGlue>>>;

/// The agents layer of one window.
pub(crate) struct AgentsHost {
    instance: String,
    control_dir: PathBuf,
    target: Rc<dyn AgentsTarget>,
    /// An activation token from this window's display, asked on the main loop before a request goes out.
    token: Rc<dyn Fn() -> Option<String>>,
    rows: RowsTo,
    running: RefCell<Option<Running>>,
    stopped: Cell<bool>,
    /// Told when what a stop let go is gone (its files removed), for the application's end to wait on.
    released: RefCell<Option<std::sync::mpsc::Receiver<()>>>,
    /// The round steps carried out so far, by their place in the order they read and wrote the round.
    carried: StepOrder,
}

/// What a window starts the layer with.
pub(crate) struct Setup {
    pub(crate) enabled: bool,
    pub(crate) control_dir: PathBuf,
    pub(crate) project: ProjectRef,
    /// `1`, `2`, `u`, `c1`..`c9` or `companion`.
    pub(crate) window_name: String,
    pub(crate) focused: bool,
    /// Whether the window publishes a rows file: every window that shows chats and the hub do; a page window, which
    /// holds no tab and is no window a new chat could open in, only serves its socket.
    pub(crate) publish: bool,
}

impl AgentsHost {
    /// A host that has started nothing: what [`AgentsHost::start`] builds, and a test's sender.
    fn new(
        instance: String,
        control_dir: PathBuf,
        target: Rc<dyn AgentsTarget>,
        token: Rc<dyn Fn() -> Option<String>>,
        rows: RowsTo,
    ) -> Rc<AgentsHost> {
        Rc::new(AgentsHost {
            instance,
            control_dir,
            target,
            token,
            rows,
            running: RefCell::new(None),
            stopped: Cell::new(false),
            released: RefCell::new(None),
            carried: StepOrder::default(),
        })
    }

    /// Starts the layer for a window when `setup.enabled`: the publisher and the server start on a thread of their
    /// own, and once both stand the rows go to `rows` (the panel's glue) and requests are served on this thread's
    /// main loop. With the layer off nothing is made and `None` comes back.
    pub(crate) fn start(
        setup: Setup,
        target: Rc<dyn AgentsTarget>,
        token: Rc<dyn Fn() -> Option<String>>,
        rows: RowsTo,
    ) -> Option<Rc<AgentsHost>> {
        if !setup.enabled {
            println!("[agents] off (init.lua's agents.enabled): this window shares no agents");
            return None;
        }
        let host = AgentsHost::new(new_instance(), setup.control_dir.clone(), target, token, rows);
        host.begin(setup);
        Some(host)
    }

    /// This window's instance: the name of its rows file and its control socket.
    pub(crate) fn instance(&self) -> &str {
        &self.instance
    }

    fn begin(self: &Rc<Self>, setup: Setup) {
        let (requests, mut incoming) = futures_channel::mpsc::unbounded::<Pending>();
        let (started_tx, started) = futures_channel::oneshot::channel::<Result<(Option<Publisher>, Server), String>>();
        let (dir, instance) = (self.control_dir.clone(), self.instance.clone());
        let window = WindowRef {
            instance: self.instance.clone(),
            name: setup.window_name.clone(),
            focused: setup.focused,
        };
        let project = setup.project.clone();
        let publish = setup.publish;
        let spawned = std::thread::Builder::new()
            .name("eitri-agents-start".into())
            .spawn(move || {
                let published = if publish {
                    Publisher::start(&dir, &instance, project, window).map(Some)
                } else {
                    Ok(None)
                };
                let started = published
                    .map_err(|e| format!("could not publish this window's rows: {e}"))
                    .and_then(|publisher| {
                        let server = Server::bind(&dir, &instance, move |req| serve_on_main_loop(&requests, req))
                            .map_err(|e| format!("could not serve this window's control socket: {e}"))?;
                        Ok((publisher, server))
                    });
                let _ = started_tx.send(started);
            });
        if let Err(e) = spawned {
            println!("[agents] could not start: {e}; this window runs without the agents layer");
            return;
        }
        let weak = Rc::downgrade(self);
        let context = eitri_core::tab_set::RowContext {
            instance: self.instance.clone(),
            project: setup.project,
            window: WindowRef {
                instance: self.instance.clone(),
                name: setup.window_name,
                focused: setup.focused,
            },
        };
        glib::spawn_future_local(async move {
            let started = started.await;
            let Some(host) = weak.upgrade().filter(|host| !host.stopped.get()) else {
                if let Ok(Ok(started)) = started {
                    // Their files are removed on a thread: this window is going anyway.
                    let _ = std::thread::Builder::new()
                        .name("eitri-agents-stop".into())
                        .spawn(move || drop(started));
                }
                return;
            };
            match started {
                Ok(Ok((publisher, server))) => host.run(publisher, server, context),
                Ok(Err(why)) => println!("[agents] {why}; this window runs without the agents layer"),
                Err(_) => {
                    println!("[agents] the start ended without an answer; this window runs without the agents layer")
                }
            }
        });
        // The layer keeps itself: this loop holds the host for as long as anything can send it a request, which ends
        // when the server goes (a stop) or never stood (a failed start).
        let host = self.clone();
        glib::spawn_future_local(async move {
            while let Ok((req, answer, taken)) = incoming.recv().await {
                if taken.swap(true, Ordering::AcqRel) {
                    // The server's thread answered `busy` already: the asker has moved on.
                    continue;
                }
                let reply = if host.stopped.get() {
                    Reply::Refused("closing".to_owned())
                } else {
                    host.serve(req)
                };
                let _ = answer.send(reply);
            }
        });
    }

    fn run(self: &Rc<Self>, publisher: Option<Publisher>, server: Server, context: eitri_core::tab_set::RowContext) {
        let published = publisher.is_some();
        let publisher = match (&self.rows, publisher) {
            (Some(rows), Some(publisher)) => {
                let mut glue = AgentsGlue::new(context, Box::new(publisher));
                glue.set_round_hook(self.round_hook());
                rows(Some(glue));
                None
            }
            (_, publisher) => publisher,
        };
        *self.running.borrow_mut() = Some(Running {
            _server: server,
            _publisher: publisher,
        });
        if published {
            println!("[agents] this window shares its agents as {}", self.instance());
        } else {
            println!(
                "[agents] this page window publishes no agents; a round started here comes back through {}",
                self.instance()
            );
        }
    }

    /// One request read by the server, on the main loop.
    fn serve(&self, req: Request) -> Reply {
        match req {
            Request::Focus {
                tab,
                card_first,
                intent,
                token,
                ..
            } => self.target.reveal(
                &RevealRequest {
                    tab: tab.map(TabId),
                    card_first,
                    intent,
                },
                token.as_deref(),
            ),
            Request::New { token, .. } => self.target.new_tab(token.as_deref()),
        }
    }

    /// A reveal in this window, from a key or a page here: nothing crosses a socket.
    pub(crate) fn focus_local(&self, req: RevealRequest) -> Reply {
        self.target.reveal(&req, None)
    }

    /// An empty chat tab in this window, from a page here: nothing crosses a socket.
    pub(crate) fn new_local(&self) -> Reply {
        self.target.new_tab(None)
    }

    /// Sends `req` to the window `instance` from a new thread and calls `done` with its answer on the main loop.
    /// A request with no token takes one from this window's display first, so the other window may come forward.
    pub(crate) fn focus_remote(&self, instance: &str, req: Request, done: impl FnOnce(Reply) + 'static) {
        send_to(&self.control_dir, &*self.token, instance, req, done);
    }

    /// Whether the layer runs here: started, and not stopped.
    fn is_running(&self) -> bool {
        !self.stopped.get() && self.running.borrow().is_some()
    }

    /// A round key pressed in this window, the keys on `module` (and on its shown `tab`, in a chat): the step is read,
    /// decided and written on a thread of its own and carried out on the main loop; `done` is told what it came to.
    pub(crate) fn round_key(
        self: &Rc<Self>,
        step: RoundKey,
        module: String,
        tab: Option<u64>,
        done: impl FnOnce(RoundOutcome) + 'static,
    ) {
        let here = RoundPoint {
            instance: self.instance.clone(),
            module,
            tab,
        };
        let weak = Rc::downgrade(self);
        off_thread(
            self.control_dir.clone(),
            move |dir, scan| plan_key(dir, scan, step, &here, wall_clock_ms()),
            move |seq, planned| match weak.upgrade() {
                Some(host) => host.carry_out(seq, planned, done),
                None => done(RoundOutcome::Off),
            },
        );
    }

    /// Where the panel's glue hands this window's rows when one stopped waiting: queued, and taken up on the main
    /// loop, where the round is moved on if its current stop is one of them.
    fn round_hook(self: &Rc<Self>) -> RoundHook {
        let (rows_tx, mut rows_rx) = futures_channel::mpsc::unbounded::<Vec<Row>>();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(rows) = rows_rx.recv().await {
                let Some(host) = weak.upgrade() else {
                    break;
                };
                if host.is_running() {
                    host.answered(rows);
                }
            }
        });
        Box::new(move |rows| {
            let _ = rows_tx.unbounded_send(rows);
        })
    }

    /// One of this window's rows stopped waiting (`rows` are all of them): when it was the round's current stop, the
    /// round goes on to its next stop, brought forward with this window's token, or ends here.
    fn answered(self: &Rc<Self>, rows: Vec<Row>) {
        let own = self.instance.clone();
        let weak = Rc::downgrade(self);
        off_thread(
            self.control_dir.clone(),
            move |dir, scan| plan_answered(dir, scan, &own, rows, wall_clock_ms()),
            move |seq, planned| {
                if let Some(host) = weak.upgrade() {
                    host.carry_out(seq, planned, |outcome| {
                        if outcome != RoundOutcome::Went {
                            println!("[agents] the round after an answer: {outcome:?}");
                        }
                    });
                }
            },
        );
    }

    /// What a step decided off the main loop, carried out here, unless a step that read the round after it was carried
    /// out first: the round on disk is that one's, and so is where the keys go.
    fn carry_out(&self, seq: u64, planned: Planned, done: impl FnOnce(RoundOutcome) + 'static) {
        if !self.carried.take(seq) {
            println!("[agents] a round step overtaken by a later one: dropped ({planned:?})");
            return;
        }
        match planned {
            Planned::Nothing => {}
            Planned::Say(outcome) => done(outcome),
            Planned::Stop { instance, tab } => self.focus_stop(&instance, Some(tab), true, move |reply| {
                done(match reply {
                    Reply::Ok => RoundOutcome::Went,
                    Reply::Refused(why) => RoundOutcome::Refused(why),
                })
            }),
            // Back to a module of this window with no tab: the controller shows it and gives it the keys.
            Planned::Back(from) if from.instance == self.instance && from.tab.is_none() => {
                done(match ModuleId::parse(&from.module) {
                    Ok(module) => RoundOutcome::BackTo(module),
                    Err(_) => RoundOutcome::StartGone,
                })
            }
            Planned::Back(from) => self.focus_stop(&from.instance, from.tab, false, move |reply| {
                done(match reply {
                    Reply::Ok => RoundOutcome::Went,
                    Reply::Refused(why) if why == "gone" => RoundOutcome::StartGone,
                    Reply::Refused(why) => RoundOutcome::Refused(why),
                })
            }),
        }
    }

    /// Brings `tab` of the window `instance` forward, here or there; `done` gets the answer on the main loop.
    fn focus_stop(&self, instance: &str, tab: Option<u64>, card_first: bool, done: impl FnOnce(Reply) + 'static) {
        if instance == self.instance {
            done(self.focus_local(RevealRequest {
                tab: tab.map(TabId),
                card_first,
                intent: Intent::Show,
            }));
        } else {
            let req = Request::Focus {
                v: control::PROTOCOL_V,
                tab,
                card_first,
                intent: Intent::Show,
                token: None,
            };
            self.focus_remote(instance, req, done);
        }
    }

    /// Stops publishing and serving: the rows file, the lock and the socket are removed on a thread of their own, since
    /// the publisher's drop waits for its last write. Safe to call twice.
    pub(crate) fn stop(&self) {
        if self.stopped.replace(true) {
            return;
        }
        let glue = self.rows.as_ref().and_then(|rows| rows(None));
        let running = self.running.borrow_mut().take();
        if glue.is_none() && running.is_none() {
            return;
        }
        let (released, done) = std::sync::mpsc::channel();
        // A thread that cannot start drops what it was given here, which is where it went before.
        let _ = std::thread::Builder::new()
            .name("eitri-agents-stop".into())
            .spawn(move || {
                drop((glue, running));
                let _ = released.send(());
            });
        *self.released.borrow_mut() = Some(done);
    }

    /// Waits up to `within` for what [`AgentsHost::stop`] let go to be gone: at the application's end, so its files do
    /// not outlive the process. Returns at once when nothing is going.
    pub(crate) fn wait_released(&self, within: std::time::Duration) {
        if let Some(done) = self.released.borrow_mut().take() {
            let _ = done.recv_timeout(within);
        }
    }
}

/// Sends `req` to the window `instance` of `control_dir` from a new thread and calls `done` with its answer on the
/// main loop. A request with no token takes one from `token` (the asking window's display) first, so the other window
/// may come forward. A window that runs no layer of its own (a page window) sends through this too.
pub(crate) fn send_to(
    control_dir: &Path,
    token: &dyn Fn() -> Option<String>,
    instance: &str,
    req: Request,
    done: impl FnOnce(Reply) + 'static,
) {
    let req = match req {
        Request::Focus {
            v,
            tab,
            card_first,
            intent,
            token: None,
        } => Request::Focus {
            v,
            tab,
            card_first,
            intent,
            token: token(),
        },
        Request::New { v, token: None } => Request::New { v, token: token() },
        req => req,
    };
    let (answer_tx, answer) = futures_channel::oneshot::channel::<io::Result<Reply>>();
    let (dir, to) = (control_dir.to_path_buf(), instance.to_owned());
    let spawned = std::thread::Builder::new()
        .name("eitri-agents-send".into())
        .spawn(move || {
            let _ = answer_tx.send(control::send(&dir, &to, &req));
        });
    let to = instance.to_owned();
    let failed = spawned.err().map(|e| format!("could not start the request: {e}"));
    glib::spawn_future_local(async move {
        let reply = match (failed, answer.await) {
            (Some(why), _) => {
                println!("[agents] a request to {to}: {why}");
                Reply::Refused("unreachable".to_owned())
            }
            (None, Ok(Ok(reply))) => reply,
            (None, Ok(Err(e))) => {
                println!("[agents] a request to {to}: {e}");
                unanswered(&e)
            }
            (None, Err(_)) => Reply::Refused("unreachable".to_owned()),
        };
        done(reply);
    });
}

thread_local! {
    /// The agents layer of this process's window: a process holds one window, and so one layer.
    static THIS_WINDOW: RefCell<Weak<AgentsHost>> = RefCell::new(Weak::new());
}

/// The agents layer of this process's window, while it runs; `None` when it is off or did not start.
pub(crate) fn this_window() -> Option<Rc<AgentsHost>> {
    THIS_WINDOW
        .with(|this| this.borrow().upgrade())
        .filter(|host| host.is_running())
}

/// Makes `host` this process's window's layer, the one [`this_window`] and the round keys find.
pub(crate) fn serve_this_window(host: &Rc<AgentsHost>) {
    THIS_WINDOW.with(|this| *this.borrow_mut() = Rc::downgrade(host));
}

/// A round key pressed in this process's window, the keys on `module` (and its shown `tab`): carried out by the window's
/// agents layer, or answered [`RoundOutcome::Off`] at once when the layer does not run here.
pub(crate) fn round_key_here(
    step: RoundKey,
    module: String,
    tab: Option<u64>,
    done: impl FnOnce(RoundOutcome) + 'static,
) {
    match this_window() {
        Some(host) => host.round_key(step, module, tab, done),
        None => done(RoundOutcome::Off),
    }
}

/// The order a process's round steps read and wrote the round in. One step at a time holds the lock from its read to its
/// write, so two never interleave there.
static ROUND_STEPS: std::sync::Mutex<u64> = std::sync::Mutex::new(0);

/// Runs `step` holding the round's lock, and gives back its place in the order.
fn in_round_order<T>(step: impl FnOnce() -> T) -> (u64, T) {
    let mut last = ROUND_STEPS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *last += 1;
    (*last, step())
}

/// The latest round step carried out on the main loop.
#[derive(Default)]
struct StepOrder(Cell<u64>);

impl StepOrder {
    /// Whether the step at `seq` is carried out: not when a later one already was. A step at 0 read and wrote nothing
    /// (its thread did not start or did not answer) and only says so.
    fn take(&self, seq: u64) -> bool {
        if seq == 0 {
            return true;
        }
        if seq < self.0.get() {
            return false;
        }
        self.0.set(seq);
        true
    }
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What a round's step found off the main loop, for the main loop to carry out.
#[derive(Debug, Clone, PartialEq)]
enum Planned {
    /// Nothing to bring forward; the status line's outcome.
    Say(RoundOutcome),
    /// The row of `tab` in the window `instance`, its card first.
    Stop { instance: String, tab: u64 },
    /// Where the round started.
    Back(RoundPoint),
    /// Nothing at all.
    Nothing,
}

/// Reads every window's rows once, from the thread it is handed to.
type Scan<'a> = &'a dyn Fn() -> io::Result<View>;

/// Runs `work` on a thread of its own, in the round's order, with the control directory `dir` and a one-shot read of
/// its rows, and `then` with its place in the order and its answer on this thread's main loop.
fn off_thread(
    dir: PathBuf,
    work: impl FnOnce(&Path, Scan<'_>) -> Planned + Send + 'static,
    then: impl FnOnce(u64, Planned) + 'static,
) {
    let (answer_tx, answer) = futures_channel::oneshot::channel::<(u64, Planned)>();
    let spawned = std::thread::Builder::new()
        .name("eitri-agents-round".into())
        .spawn(move || {
            let scan = || scan_once(&dir, SCHEMA_V);
            let _ = answer_tx.send(in_round_order(|| work(&dir, &scan)));
        });
    if let Err(e) = spawned {
        // Nothing was read or written: any later step may go first.
        then(
            0,
            Planned::Say(RoundOutcome::Refused(format!("could not read the agents: {e}"))),
        );
        return;
    }
    glib::spawn_future_local(async move {
        let (seq, planned) = answer.await.unwrap_or_else(|_| {
            (
                0,
                Planned::Say(RoundOutcome::Refused("the read ended without an answer".to_owned())),
            )
        });
        then(seq, planned);
    });
}

/// Writes the round file, or removes it; a failure is a log line, and the step goes on.
fn write_round(store: &SharedStore, round: Option<&Round>) {
    if let Err(e) = store.write_round(round) {
        println!("[agents] the round could not be written: {e}");
    }
}

/// Leaves the round file as `file` says.
fn apply(store: &SharedStore, file: RoundFile) {
    match file {
        RoundFile::Keep => {}
        RoundFile::Write(round) => write_round(store, Some(&round)),
        RoundFile::Remove => write_round(store, None),
    }
}

/// A step's stop as the main loop brings it forward.
fn planned(step: &NextStep) -> Planned {
    match step {
        NextStep::Go { row_id, .. } => match agents_round::stop_of(row_id) {
            Some((instance, tab)) => Planned::Stop {
                instance: instance.to_owned(),
                tab,
            },
            None => Planned::Say(RoundOutcome::Refused("its row names no tab".to_owned())),
        },
        NextStep::Ended => Planned::Say(RoundOutcome::Ended),
        NextStep::NothingWaits => Planned::Say(RoundOutcome::NothingWaits),
    }
}

/// A round key's step from `here`, read and written in the control directory `dir`: off the main loop.
fn plan_key(dir: &Path, scan: Scan<'_>, step: RoundKey, here: &RoundPoint, now_ms: u64) -> Planned {
    let store = match SharedStore::open(dir) {
        Ok(store) => store,
        Err(e) => return Planned::Say(RoundOutcome::Refused(format!("the agents directory: {e}"))),
    };
    let round = store.read_round();
    match step {
        RoundKey::Back => {
            // Leaving the round, or finding it idle too long, removes it.
            if round.is_some() {
                write_round(&store, None);
            }
            match agents_round::on_back(round.as_ref(), now_ms) {
                None => Planned::Say(RoundOutcome::NoRound),
                Some(from) => Planned::Back(from),
            }
        }
        RoundKey::Next => {
            let view = match scan() {
                Ok(view) => view,
                Err(e) => return Planned::Say(RoundOutcome::Refused(format!("the agents could not be read: {e}"))),
            };
            let next = agents_round::on_next(&view, round.as_ref(), here, now_ms);
            apply(&store, next.round_file(round.as_ref(), now_ms));
            planned(&next)
        }
    }
}

/// The step the window `own` takes when one of its rows stopped waiting (`rows` are all of its rows, newer than its
/// file may be yet): off the main loop. Nothing unless the round's current stop is one of them.
fn plan_answered(dir: &Path, scan: Scan<'_>, own: &str, rows: Vec<Row>, now_ms: u64) -> Planned {
    let Ok(store) = SharedStore::open(dir) else {
        return Planned::Nothing;
    };
    let Some(round) = store.read_round() else {
        return Planned::Nothing;
    };
    let mine = View {
        rows,
        ..View::default()
    };
    if !should_advance(own, &mine, &round) {
        return Planned::Nothing;
    }
    let mut view = match scan() {
        Ok(view) => view,
        Err(e) => {
            println!("[agents] the round could not move on: {e}");
            return Planned::Nothing;
        }
    };
    // This window's own rows as it holds them now: its file may not carry the answer yet.
    view.rows.retain(|row| row.window.instance != own);
    view.rows.extend(mine.rows);
    match agents_round::after_answer(own, &view, &round, now_ms) {
        None => Planned::Nothing,
        Some(next) => {
            apply(&store, next.round_file(Some(&round), now_ms));
            match next {
                NextStep::Go { .. } => planned(&next),
                // The round ends where the person is: nothing comes forward.
                NextStep::Ended | NextStep::NothingWaits => Planned::Nothing,
            }
        }
    }
}

/// The refusal a failed send stands for: the window's socket gone, its answer late, or anything else.
fn unanswered(e: &io::Error) -> Reply {
    Reply::Refused(
        match e.kind() {
            io::ErrorKind::TimedOut => "timed out",
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::InvalidInput => "gone",
            _ => "unreachable",
        }
        .to_owned(),
    )
}

/// The server thread's handler: the request goes to the main loop, and the thread waits for its answer there.
fn serve_on_main_loop(requests: &futures_channel::mpsc::UnboundedSender<Pending>, req: Request) -> Reply {
    let (answer_tx, answer) = std::sync::mpsc::sync_channel(1);
    let taken = Arc::new(AtomicBool::new(false));
    if requests.unbounded_send((req, answer_tx, taken.clone())).is_err() {
        return Reply::Refused("closing".to_owned());
    }
    match answer.recv_timeout(ANSWER_WAIT) {
        Ok(reply) => reply,
        // Not taken up yet: it never will be.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !taken.swap(true, Ordering::AcqRel) => {
            Reply::Refused("busy".to_owned())
        }
        // The main loop took it up just now: its answer follows the reveal.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            answer.recv().unwrap_or_else(|_| Reply::Refused("closing".to_owned()))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Reply::Refused("closing".to_owned()),
    }
}

/// Of the windows `windows` names (each with its project), the one a new chat in `project_key` opens in: window 1
/// (the hub in compositor mode, else window 1 of the pair), else the lowest: window 2, then an unpaired window,
/// then the chat windows by number, then the companion panel; between equals, the lower instance.
pub(crate) fn new_target<'a>(windows: &'a [(ProjectRef, WindowRef)], project_key: &str) -> Option<&'a str> {
    fn rank(name: &str) -> u32 {
        match name {
            "1" => 0,
            "2" => 1,
            "u" => 2,
            "companion" => 30,
            chat => chat
                .strip_prefix('c')
                .and_then(|n| n.parse::<u32>().ok())
                .filter(|n| (1..=9).contains(n))
                .map_or(99, |n| 10 + n),
        }
    }
    windows
        .iter()
        .filter(|(project, _)| project.key == project_key)
        .min_by(|(_, a), (_, b)| (rank(&a.name), &a.instance).cmp(&(rank(&b.name), &b.instance)))
        .map(|(_, window)| window.instance.as_str())
}

/// Everything a full window, a chat window or the hub hands to [`start_for_window`].
pub(crate) struct AgentsWindow {
    pub(crate) app: gtk4::Application,
    pub(crate) window: gtk4::ApplicationWindow,
    pub(crate) driver: Weak<eitri_core::wm::Driver<eitri_core::wm::Controller>>,
    pub(crate) win: eitri_core::wm::WinId,
    pub(crate) raise: crate::linked_window_glue::focus::PairRaise,
    /// `None` for the hub, which holds no tab.
    pub(crate) panel: Option<AgentPanelHandle>,
    pub(crate) enabled: bool,
    pub(crate) project_root: PathBuf,
    pub(crate) window_name: String,
}

/// Starts the layer for a window of the full shell once it exists, and stops it when the window goes.
pub(crate) fn start_for_window(parts: AgentsWindow) -> Option<Rc<AgentsHost>> {
    use gtk4::prelude::*;
    let hub = parts.panel.is_none();
    let target = {
        let (driver, win) = (parts.driver.clone(), parts.win);
        let new_driver = parts.driver.clone();
        let (window, raise) = (parts.window.downgrade(), parts.raise.clone());
        DriverTarget {
            reveal: Box::new(move |req: &RevealRequest| match driver.upgrade() {
                Some(driver) => driver.tab_reveal(win, req),
                None => Err(RevealRefusal::Gone),
            }),
            new: Box::new(move |token: Option<&str>| {
                if let Some(driver) = new_driver.upgrade() {
                    driver.agents_new(win, token.map(str::to_owned));
                }
            }),
            present: Box::new(move |token: Option<&str>| {
                if let Some(window) = window.upgrade() {
                    if token.is_some() || !window.is_active() {
                        raise.present_and_focus(&window, token);
                    }
                }
            }),
            hub,
        }
    };
    let token: Rc<dyn Fn() -> Option<String>> = {
        let window = parts.window.downgrade();
        Rc::new(move || crate::window_services::activation_token(&window))
    };
    let rows: RowsTo = parts
        .panel
        .clone()
        .and_then(|panel| rows_to_panel(panel, &parts.window));
    let host = AgentsHost::start(
        Setup {
            enabled: parts.enabled,
            control_dir: control_dir(),
            project: project_ref(&parts.project_root),
            window_name: parts.window_name,
            focused: parts.window.is_active(),
            publish: true,
        },
        Rc::new(target),
        token,
        rows,
    )?;
    if let Some(panel) = parts.panel {
        parts
            .window
            .connect_is_active_notify(move |window| panel.set_agents_focused(window.is_active()));
    }
    stop_with(&parts.app, &parts.window, &host);
    serve_this_window(&host);
    Some(host)
}

/// A page window of a compositor group as a target: it holds no tab, so a focus of a tab and `new` are refused
/// `gone`; a focus with no tab (a round started from its page coming back) gives its page the keys through the
/// controller's `tab.reveal` and brings the window forward.
pub(crate) struct PageWindowTarget {
    pub(crate) reveal: RevealFn,
    pub(crate) present: PresentFn,
}

impl AgentsTarget for PageWindowTarget {
    fn reveal(&self, req: &RevealRequest, token: Option<&str>) -> Reply {
        if req.tab.is_some() {
            return refusal(RevealRefusal::Gone);
        }
        match (self.reveal)(req) {
            Ok(()) => {
                (self.present)(token);
                Reply::Ok
            }
            Err(why) => refusal(why),
        }
    }

    fn new_tab(&self, _token: Option<&str>) -> Reply {
        refusal(RevealRefusal::Gone)
    }
}

/// Starts the layer for a page window of a compositor group: no rows file, only the control socket, so a round
/// started from its page has a way back here, and the round keys run here as in any window. Stopped when the
/// window goes.
pub(crate) fn start_for_page_window(parts: AgentsWindow) -> Option<Rc<AgentsHost>> {
    use gtk4::prelude::*;
    let target = {
        let (driver, win) = (parts.driver.clone(), parts.win);
        let (window, raise) = (parts.window.downgrade(), parts.raise.clone());
        PageWindowTarget {
            reveal: Box::new(move |req: &RevealRequest| match driver.upgrade() {
                Some(driver) => driver.tab_reveal(win, req),
                None => Err(RevealRefusal::Gone),
            }),
            present: Box::new(move |token: Option<&str>| {
                if let Some(window) = window.upgrade() {
                    if token.is_some() || !window.is_active() {
                        raise.present_and_focus(&window, token);
                    }
                }
            }),
        }
    };
    let token: Rc<dyn Fn() -> Option<String>> = {
        let window = parts.window.downgrade();
        Rc::new(move || crate::window_services::activation_token(&window))
    };
    let host = AgentsHost::start(
        Setup {
            enabled: parts.enabled,
            control_dir: control_dir(),
            project: project_ref(&parts.project_root),
            window_name: parts.window_name,
            focused: parts.window.is_active(),
            publish: false,
        },
        Rc::new(target),
        token,
        None,
    )?;
    stop_with(&parts.app, &parts.window, &host);
    serve_this_window(&host);
    Some(host)
}

/// Rows that go to `panel`'s glue, which is told at once whether `window` has the keys: the window may have taken or
/// lost them while the layer started.
pub(crate) fn rows_to_panel(panel: AgentPanelHandle, window: &gtk4::ApplicationWindow) -> RowsTo {
    use gtk4::prelude::*;
    let window = window.downgrade();
    Some(Rc::new(move |glue: Option<AgentsGlue>| {
        let installed = glue.is_some();
        let held = panel.set_agents(glue);
        if let Some(window) = window.upgrade().filter(|_| installed) {
            panel.set_agents_focused(window.is_active());
        }
        held
    }))
}

/// Stops `host` when `window` is destroyed or the application shuts down, whichever comes first.
pub(crate) fn stop_with(app: &gtk4::Application, window: &gtk4::ApplicationWindow, host: &Rc<AgentsHost>) {
    use gtk4::prelude::*;
    let weak = Rc::downgrade(host);
    window.connect_destroy(move |_| {
        if let Some(host) = weak.upgrade() {
            host.stop();
        }
    });
    let weak = Rc::downgrade(host);
    app.connect_shutdown(move |_| {
        if let Some(host) = weak.upgrade() {
            host.stop();
            // The process ends after this: its files go first, unless the disk keeps them past the deadline (a
            // reader then removes them once this process is gone).
            host.wait_released(DEADLINE);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::agents::Intent;
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// How long any one step may take on a loaded machine; a test that passes never waits it out.
    const PATIENCE: Duration = Duration::from_secs(20);

    /// A private directory standing in for the control directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            static SERIAL: AtomicU32 = AtomicU32::new(0);
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(std::env::temp_dir);
            let path = base.join(format!(
                "ah-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Runs `body` with a main context of its own as this thread's default, so the host's futures run on it.
    fn on_own_loop<T>(body: impl FnOnce(&glib::MainContext) -> T) -> T {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| body(&context))
            .expect("the test thread owns its new context")
    }

    /// Turns `context` until `done` holds.
    fn turn_until(context: &glib::MainContext, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !done() {
            assert!(Instant::now() < deadline, "{what} never happened");
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn setup(dir: &Path, enabled: bool, name: &str) -> Setup {
        Setup {
            enabled,
            control_dir: dir.to_path_buf(),
            project: ProjectRef {
                key: "fedcba9876543210".into(),
                name: "eitri".into(),
            },
            window_name: name.into(),
            focused: false,
            publish: true,
        }
    }

    /// What a fake controller saw, and where.
    #[derive(Default)]
    struct Seen {
        reveals: Vec<RevealRequest>,
        /// Each `new`, with the token it came with.
        news: Vec<Option<String>>,
        presents: Vec<Option<String>>,
        reveal_thread: Option<std::thread::ThreadId>,
    }

    fn fake_target(seen: &Rc<RefCell<Seen>>, revealed: &Arc<AtomicBool>, hub: bool) -> Rc<dyn AgentsTarget> {
        let (s1, s2, s3) = (seen.clone(), seen.clone(), seen.clone());
        let revealed = revealed.clone();
        Rc::new(DriverTarget {
            reveal: Box::new(move |req: &RevealRequest| {
                let mut seen = s1.borrow_mut();
                seen.reveals.push(req.clone());
                seen.reveal_thread = Some(std::thread::current().id());
                revealed.store(true, Ordering::SeqCst);
                if req.tab == Some(TabId(99)) {
                    Err(RevealRefusal::Gone)
                } else {
                    Ok(())
                }
            }),
            new: Box::new(move |token: Option<&str>| s2.borrow_mut().news.push(token.map(str::to_owned))),
            present: Box::new(move |token: Option<&str>| s3.borrow_mut().presents.push(token.map(str::to_owned))),
            hub,
        })
    }

    fn no_token() -> Rc<dyn Fn() -> Option<String>> {
        Rc::new(|| None)
    }

    /// Sends `req` to `instance` from a worker while the loop turns, and returns the reply and whether the reveal
    /// had run when the reply came.
    fn send_while_turning(
        context: &glib::MainContext,
        dir: &Path,
        instance: &str,
        req: Request,
        revealed: &Arc<AtomicBool>,
    ) -> (Reply, bool) {
        let (dir, instance, revealed) = (dir.to_path_buf(), instance.to_owned(), revealed.clone());
        let worker = std::thread::spawn(move || {
            let reply = control::send(&dir, &instance, &req).expect("the window answers");
            (reply, revealed.load(Ordering::SeqCst))
        });
        turn_until(context, "the reply", || worker.is_finished());
        worker.join().unwrap()
    }

    fn rows_file(dir: &Path, instance: &str) -> PathBuf {
        eitri_core::agents::agents_dir(dir).join(format!("{instance}.json"))
    }

    fn waiting(instance: &str, tab: u64, key: &str, class: eitri_core::agents::Class, since: u64) -> Row {
        Row {
            id: format!("{instance}.{tab}"),
            key: key.into(),
            window: WindowRef {
                instance: instance.into(),
                name: "1".into(),
                focused: false,
            },
            class,
            since,
            waiting_since: (class == eitri_core::agents::Class::Needs).then_some(since),
            ..Row::default()
        }
    }

    fn view_of(rows: Vec<Row>) -> View {
        View {
            rows,
            ..View::default()
        }
    }

    #[test]
    fn a_round_key_writes_the_round_and_names_the_stop() {
        use eitri_core::agents::Class;
        let scratch = Scratch::new();
        let store = SharedStore::open(&scratch.0).unwrap();
        let here = RoundPoint {
            instance: "cccc".into(),
            module: "editor".into(),
            tab: None,
        };
        let busy = || -> io::Result<View> { Ok(view_of(vec![waiting("bbbb", 4, "k-b", Class::Needs, 1_000)])) };
        assert_eq!(
            plan_key(&scratch.0, &busy, RoundKey::Next, &here, 5_000),
            Planned::Stop {
                instance: "bbbb".into(),
                tab: 4
            }
        );
        let round = store
            .read_round()
            .expect("the round is written before the stop comes forward");
        assert_eq!(round.current.as_deref(), Some("k-b"));
        assert_eq!(round.from, here);

        let quiet = || -> io::Result<View> { Ok(view_of(vec![waiting("bbbb", 4, "k-b", Class::Working, 1_000)])) };
        let at_stop = RoundPoint {
            instance: "bbbb".into(),
            module: "agent".into(),
            tab: Some(4),
        };
        assert_eq!(
            plan_key(&scratch.0, &quiet, RoundKey::Next, &at_stop, 6_000),
            Planned::Say(RoundOutcome::NothingWaits)
        );
        let ended = store.read_round().expect("the ended round keeps where it started");
        assert_eq!(ended.current, None, "nothing waits: the running round ends");
        assert_eq!(
            plan_key(&scratch.0, &quiet, RoundKey::Back, &at_stop, 7_000),
            Planned::Back(here.clone())
        );
        assert_eq!(store.read_round(), None, "leaving the round removes it");
        assert_eq!(
            plan_key(&scratch.0, &quiet, RoundKey::Back, &at_stop, 7_500),
            Planned::Say(RoundOutcome::NoRound)
        );

        assert!(matches!(
            plan_key(&scratch.0, &busy, RoundKey::Next, &here, 8_000),
            Planned::Stop { .. }
        ));
        assert_eq!(
            plan_key(&scratch.0, &quiet, RoundKey::Back, &at_stop, 9_000),
            Planned::Back(here)
        );
        assert_eq!(store.read_round(), None, "leaving the round removes it");
    }

    #[test]
    fn an_answer_moves_the_round_on_once_and_only_from_the_stops_window() {
        use eitri_core::agents::Class;
        let scratch = Scratch::new();
        let store = SharedStore::open(&scratch.0).unwrap();
        // The files still show the answered card waiting: its window's rows are newer than its file.
        let files = || -> io::Result<View> {
            Ok(view_of(vec![
                waiting("aaaa", 1, "k-a", Class::Needs, 1_000),
                waiting("bbbb", 2, "k-b", Class::Needs, 2_000),
            ]))
        };
        let here = RoundPoint {
            instance: "cccc".into(),
            module: "editor".into(),
            tab: None,
        };
        assert!(matches!(
            plan_key(&scratch.0, &files, RoundKey::Next, &here, 3_000),
            Planned::Stop { .. }
        ));
        let mut answered = waiting("aaaa", 1, "k-a", Class::Working, 4_000);
        answered.window.focused = true;
        let answered = vec![answered];
        assert_eq!(
            plan_answered(&scratch.0, &files, "bbbb", answered.clone(), 4_000),
            Planned::Nothing,
            "another window's rows never move the round"
        );
        assert_eq!(
            plan_answered(&scratch.0, &files, "aaaa", answered.clone(), 4_000),
            Planned::Stop {
                instance: "bbbb".into(),
                tab: 2
            }
        );
        assert_eq!(store.read_round().unwrap().current.as_deref(), Some("k-b"));
        assert_eq!(
            plan_answered(&scratch.0, &files, "aaaa", answered, 4_100),
            Planned::Nothing,
            "the round already moved on: once"
        );

        // The last stop answered: the person stays there, and prefix B still goes back to the start.
        let mut last = waiting("bbbb", 2, "k-b", Class::Working, 5_000);
        last.window.focused = true;
        let after = || -> io::Result<View> {
            Ok(view_of(vec![
                waiting("aaaa", 1, "k-a", Class::Working, 4_000),
                waiting("bbbb", 2, "k-b", Class::Needs, 2_000),
            ]))
        };
        assert_eq!(
            plan_answered(&scratch.0, &after, "bbbb", vec![last], 5_000),
            Planned::Nothing
        );
        assert_eq!(store.read_round().expect("kept").current, None);
        let at_last = RoundPoint {
            instance: "bbbb".into(),
            module: "agent".into(),
            tab: Some(2),
        };
        assert_eq!(
            plan_key(&scratch.0, &after, RoundKey::Back, &at_last, 6_000),
            Planned::Back(here)
        );
    }

    #[test]
    fn a_round_step_overtaken_by_a_later_one_moves_nothing() {
        let seen = Rc::new(RefCell::new(Seen::default()));
        let host = AgentsHost::new(
            "aaaa".into(),
            PathBuf::from("/nonexistent"),
            fake_target(&seen, &Arc::default(), false),
            no_token(),
            None,
        );
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let record = |outcomes: &Rc<RefCell<Vec<RoundOutcome>>>| {
            let outcomes = outcomes.clone();
            move |outcome| outcomes.borrow_mut().push(outcome)
        };
        let stop = || Planned::Stop {
            instance: "aaaa".into(),
            tab: 1,
        };
        host.carry_out(5, Planned::Nothing, |_| {});
        host.carry_out(3, stop(), record(&outcomes));
        assert!(
            seen.borrow().reveals.is_empty(),
            "read before the step carried out: dropped"
        );
        assert!(outcomes.borrow().is_empty());
        host.carry_out(0, Planned::Say(RoundOutcome::Refused("x".into())), record(&outcomes));
        assert_eq!(outcomes.borrow().len(), 1, "a step that read nothing still says so");
        host.carry_out(6, stop(), record(&outcomes));
        assert_eq!(seen.borrow().reveals.len(), 1, "a later step goes");
    }

    #[test]
    fn round_steps_of_a_process_read_and_write_one_at_a_time() {
        let inside = Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let inside = inside.clone();
                std::thread::spawn(move || {
                    (0..25)
                        .map(|_| {
                            in_round_order(|| {
                                assert!(!inside.swap(true, Ordering::SeqCst), "two steps at once");
                                std::thread::yield_now();
                                inside.store(false, Ordering::SeqCst);
                            })
                            .0
                        })
                        .collect::<Vec<u64>>()
                })
            })
            .collect();
        let mut seqs: Vec<u64> = workers.into_iter().flat_map(|w| w.join().unwrap()).collect();
        let n = seqs.len();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), n, "each step has a place of its own");
    }

    #[test]
    fn with_agents_enabled_false_the_round_keys_say_agents_are_off() {
        let scratch = Scratch::new();
        let seen = Rc::new(RefCell::new(Seen::default()));
        on_own_loop(|context| {
            let start = |value: &str| {
                let host = AgentsHost::start(
                    setup(&scratch.0, enabled(Some(value)), "1"),
                    fake_target(&seen, &Arc::default(), false),
                    no_token(),
                    None,
                );
                if let Some(host) = &host {
                    serve_this_window(host);
                }
                host
            };
            let press = || {
                let outcome = Rc::new(RefCell::new(None));
                let put = outcome.clone();
                round_key_here(RoundKey::Next, "editor".into(), None, move |o| {
                    *put.borrow_mut() = Some(o)
                });
                turn_until(context, "the round key's answer", || outcome.borrow().is_some());
                outcome.take().unwrap()
            };
            assert!(start("false").is_none(), "agents.enabled = false: no layer");
            assert_eq!(press(), RoundOutcome::Off);

            let host = start("true").expect("agents.enabled = true: the layer starts");
            turn_until(context, "the layer running", || host.is_running());
            assert_eq!(
                press(),
                RoundOutcome::NothingWaits,
                "with the layer on the round is read"
            );
            host.stop();
            host.wait_released(PATIENCE);
        });
    }

    #[test]
    fn agents_disabled_starts_nothing() {
        let scratch = Scratch::new();
        let seen = Rc::new(RefCell::new(Seen::default()));
        let installed = Rc::new(Cell::new(false));
        on_own_loop(|context| {
            let rows: RowsTo = {
                let installed = installed.clone();
                Some(Rc::new(move |_: Option<AgentsGlue>| {
                    installed.set(true);
                    None
                }))
            };
            let host = AgentsHost::start(
                setup(&scratch.0, false, "1"),
                fake_target(&seen, &Arc::default(), false),
                no_token(),
                rows,
            );
            assert!(host.is_none(), "with the layer off no host runs");
            for _ in 0..20 {
                context.iteration(false);
            }
        });
        assert!(!installed.get(), "no publisher reached the panel");
        let left: Vec<_> = std::fs::read_dir(&scratch.0).unwrap().collect();
        assert!(left.is_empty(), "no rows directory, file or socket: {left:?}");
    }

    #[test]
    fn the_hub_publishes_no_rows_and_serves_new() {
        let scratch = Scratch::new();
        let seen = Rc::new(RefCell::new(Seen::default()));
        let revealed = Arc::new(AtomicBool::new(false));
        on_own_loop(|context| {
            let host = AgentsHost::start(
                setup(&scratch.0, true, "1"),
                fake_target(&seen, &revealed, true),
                no_token(),
                None,
            )
            .expect("the layer is on");
            let instance = host.instance().to_owned();
            turn_until(context, "the hub's server", || host.running.borrow().is_some());
            turn_until(context, "the hub's rows file", || {
                rows_file(&scratch.0, &instance).exists()
            });
            let file = std::fs::read_to_string(rows_file(&scratch.0, &instance)).unwrap();
            let file: eitri_core::agents::ProcessFile = serde_json::from_str(&file).unwrap();
            assert!(file.rows.is_empty(), "the hub holds no tab: {file:?}");
            assert_eq!(file.window.name, "1");

            let (reply, _) = send_while_turning(
                context,
                &scratch.0,
                &instance,
                Request::New {
                    v: 1,
                    token: Some("tok".into()),
                },
                &revealed,
            );
            assert_eq!(reply, Reply::Ok);
            assert_eq!(
                seen.borrow().news,
                [Some("tok".to_owned())],
                "the hub's new carries the asking window's token"
            );
            assert!(seen.borrow().presents.is_empty(), "window 1 stays where it is");

            let weak = Rc::downgrade(&host);
            host.stop();
            host.wait_released(PATIENCE);
            drop(host);
            assert!(
                !rows_file(&scratch.0, &instance).exists(),
                "a stop removes the rows file"
            );
            assert!(!control::socket_path(&scratch.0, &instance).unwrap().exists());
            turn_until(context, "the stopped layer to let itself go", || {
                weak.upgrade().is_none()
            });
        });
    }

    #[test]
    fn a_page_window_publishes_no_rows_and_takes_a_round_back_to_its_page() {
        let scratch = Scratch::new();
        let seen = Rc::new(RefCell::new(Seen::default()));
        let revealed = Arc::new(AtomicBool::new(false));
        on_own_loop(|context| {
            let (s1, s2) = (seen.clone(), seen.clone());
            let flag = revealed.clone();
            let target = Rc::new(PageWindowTarget {
                reveal: Box::new(move |req: &RevealRequest| {
                    s1.borrow_mut().reveals.push(req.clone());
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                }),
                present: Box::new(move |token: Option<&str>| s2.borrow_mut().presents.push(token.map(str::to_owned))),
            });
            let host = AgentsHost::start(
                Setup {
                    publish: false,
                    ..setup(&scratch.0, true, "p")
                },
                target,
                no_token(),
                None,
            )
            .expect("the layer is on");
            let instance = host.instance().to_owned();
            turn_until(context, "the page window's server", || host.running.borrow().is_some());
            assert!(
                !rows_file(&scratch.0, &instance).exists(),
                "a page window is no window a row or a new chat could name"
            );
            let back = Request::Focus {
                v: control::PROTOCOL_V,
                tab: None,
                card_first: false,
                intent: Intent::Show,
                token: Some("tok".into()),
            };
            let (reply, ran) = send_while_turning(context, &scratch.0, &instance, back, &revealed);
            assert_eq!(reply, Reply::Ok);
            assert!(ran, "the page took the keys before the answer");
            assert_eq!(seen.borrow().presents, [Some("tok".to_owned())]);
            for req in [
                Request::Focus {
                    v: control::PROTOCOL_V,
                    tab: Some(1),
                    card_first: true,
                    intent: Intent::Show,
                    token: None,
                },
                Request::New { v: 1, token: None },
            ] {
                let (reply, _) = send_while_turning(context, &scratch.0, &instance, req, &revealed);
                assert_eq!(reply, Reply::Refused("gone".into()));
            }
            assert_eq!(
                seen.borrow().reveals.len(),
                1,
                "only the way back reached the controller"
            );
            let weak = Rc::downgrade(&host);
            host.stop();
            host.wait_released(PATIENCE);
            drop(host);
            assert!(!control::socket_path(&scratch.0, &instance).unwrap().exists());
            // The server's thread lets its sender go last; the loop that holds the host must end on this context.
            turn_until(context, "the stopped layer to let itself go", || {
                weak.upgrade().is_none()
            });
        });
    }

    #[test]
    fn a_focus_request_runs_reveal_on_the_main_loop_and_answers_after() {
        let scratch = Scratch::new();
        let seen = Rc::new(RefCell::new(Seen::default()));
        let revealed = Arc::new(AtomicBool::new(false));
        let glue = Rc::new(RefCell::new(None::<AgentsGlue>));
        on_own_loop(|context| {
            let rows: RowsTo = {
                let glue = glue.clone();
                Some(Rc::new(move |g: Option<AgentsGlue>| {
                    std::mem::replace(&mut *glue.borrow_mut(), g)
                }))
            };
            let host = AgentsHost::start(
                setup(&scratch.0, true, "2"),
                fake_target(&seen, &revealed, false),
                no_token(),
                rows,
            )
            .expect("the layer is on");
            let instance = host.instance().to_owned();
            turn_until(context, "the window's server", || host.running.borrow().is_some());
            assert!(glue.borrow().is_some(), "the panel was given its rows' glue");

            let (reply, revealed_first) = send_while_turning(
                context,
                &scratch.0,
                &instance,
                Request::Focus {
                    v: 1,
                    tab: Some(7),
                    card_first: true,
                    intent: Intent::Show,
                    token: Some("tok-7".into()),
                },
                &revealed,
            );
            assert_eq!(reply, Reply::Ok);
            assert!(revealed_first, "the reply was sent after the reveal ran");
            {
                let seen = seen.borrow();
                assert_eq!(
                    seen.reveals,
                    [RevealRequest {
                        tab: Some(TabId(7)),
                        card_first: true,
                        intent: Intent::Show,
                    }]
                );
                assert_eq!(
                    seen.reveal_thread,
                    Some(std::thread::current().id()),
                    "on the main loop's thread"
                );
                assert_eq!(
                    seen.presents,
                    [Some("tok-7".to_owned())],
                    "presented with the caller's token"
                );
            }

            let (reply, _) = send_while_turning(
                context,
                &scratch.0,
                &instance,
                Request::Focus {
                    v: 1,
                    tab: Some(99),
                    card_first: false,
                    intent: Intent::Show,
                    token: None,
                },
                &revealed,
            );
            assert_eq!(reply, Reply::Refused("gone".into()));
            assert_eq!(seen.borrow().presents.len(), 1, "a refused reveal presents nothing");

            host.stop();
            assert!(glue.borrow().is_none(), "a stop takes the glue back from the panel");
        });
    }

    #[test]
    fn the_layer_runs_after_the_callers_handle_is_dropped() {
        let scratch = Scratch::new();
        let seen = Rc::new(RefCell::new(Seen::default()));
        let revealed = Arc::new(AtomicBool::new(false));
        let glue = Rc::new(RefCell::new(None::<AgentsGlue>));
        on_own_loop(|context| {
            let rows: RowsTo = {
                let glue = glue.clone();
                Some(Rc::new(move |g: Option<AgentsGlue>| {
                    std::mem::replace(&mut *glue.borrow_mut(), g)
                }))
            };
            let host = AgentsHost::start(
                setup(&scratch.0, true, "u"),
                fake_target(&seen, &revealed, false),
                no_token(),
                rows,
            )
            .expect("the layer is on");
            let instance = host.instance().to_owned();
            let weak = Rc::downgrade(&host);
            drop(host);

            turn_until(context, "the panel's glue", || glue.borrow().is_some());
            let (reply, revealed_first) = send_while_turning(
                context,
                &scratch.0,
                &instance,
                Request::Focus {
                    v: 1,
                    tab: Some(4),
                    card_first: false,
                    intent: Intent::Show,
                    token: None,
                },
                &revealed,
            );
            assert_eq!(reply, Reply::Ok, "the window still serves its socket");
            assert!(revealed_first);

            let host = weak.upgrade().expect("the running layer holds itself");
            host.stop();
            host.wait_released(PATIENCE);
            drop(host);
            assert!(glue.borrow().is_none(), "the stop took the glue back");
            assert!(!rows_file(&scratch.0, &instance).exists());
            turn_until(context, "the stopped layer to let itself go", || {
                weak.upgrade().is_none()
            });
        });
    }

    #[test]
    fn a_request_the_loop_took_too_long_for_is_refused_busy_and_never_carried_out() {
        let scratch = Scratch::new();
        let seen = Rc::new(RefCell::new(Seen::default()));
        let revealed = Arc::new(AtomicBool::new(false));
        on_own_loop(|context| {
            let host = AgentsHost::start(
                setup(&scratch.0, true, "2"),
                fake_target(&seen, &revealed, false),
                no_token(),
                None,
            )
            .expect("the layer is on");
            let instance = host.instance().to_owned();
            turn_until(context, "the window's server", || host.running.borrow().is_some());

            // The loop does not turn while the request waits: a window busy past the deadline.
            let (dir, to) = (scratch.0.clone(), instance.clone());
            let worker = std::thread::spawn(move || {
                control::send(
                    &dir,
                    &to,
                    &Request::Focus {
                        v: 1,
                        tab: Some(5),
                        card_first: true,
                        intent: Intent::Show,
                        token: Some("tok".into()),
                    },
                )
            });
            let reply = worker.join().unwrap().expect("the window answers");
            assert_eq!(reply, Reply::Refused("busy".into()));

            // The loop turns again and finds the request queued: it is dropped, not carried out late.
            for _ in 0..50 {
                context.iteration(false);
            }
            assert!(seen.borrow().reveals.is_empty(), "{:?}", seen.borrow().reveals);
            assert!(seen.borrow().presents.is_empty());
            assert!(!revealed.load(Ordering::SeqCst));
            host.stop();
            host.wait_released(PATIENCE);
        });
    }

    #[test]
    fn new_targets_the_hub_then_window_1_then_the_lowest() {
        let window = |project: &str, instance: &str, name: &str| {
            (
                ProjectRef {
                    key: project.into(),
                    name: project.into(),
                },
                WindowRef {
                    instance: instance.into(),
                    name: name.into(),
                    focused: false,
                },
            )
        };
        // Compositor mode: the hub is window 1, beside its chat windows.
        let compositor = [
            window("p", "00000000000000c2", "c2"),
            window("p", "00000000000000c1", "c1"),
            window("p", "00000000000000a1", "1"),
        ];
        assert_eq!(new_target(&compositor, "p"), Some("00000000000000a1"));
        // A pair: window 1 before window 2.
        let pair = [
            window("p", "0000000000000002", "2"),
            window("p", "0000000000000001", "1"),
        ];
        assert_eq!(new_target(&pair, "p"), Some("0000000000000001"));
        // No window 1: the lowest, and of two unpaired windows the lower instance.
        let rest = [
            window("p", "00000000000000ff", "companion"),
            window("p", "00000000000000b2", "u"),
            window("p", "00000000000000b1", "u"),
            window("p", "00000000000000c3", "c3"),
            window("q", "0000000000000001", "1"),
        ];
        assert_eq!(new_target(&rest, "p"), Some("00000000000000b1"));
        assert_eq!(new_target(&rest[..1], "p"), Some("00000000000000ff"));
        assert_eq!(
            new_target(&rest, "none"),
            None,
            "a project with no window has no target"
        );
    }

    #[test]
    fn a_peer_that_never_answers_keeps_the_loop_turning() {
        let scratch = Scratch::new();
        let instance = "0123456789abcdef";
        // Accepts (the backlog does) and never reads or answers.
        let _silent = std::os::unix::net::UnixListener::bind(control::socket_path(&scratch.0, instance).unwrap())
            .expect("a silent peer");
        let seen = Rc::new(RefCell::new(Seen::default()));
        let token_asked = Rc::new(Cell::new(0u32));
        on_own_loop(|context| {
            let token: Rc<dyn Fn() -> Option<String>> = {
                let asked = token_asked.clone();
                Rc::new(move || {
                    asked.set(asked.get() + 1);
                    Some("from-this-display".into())
                })
            };
            let host = AgentsHost::new(
                "fedcba9876543210".into(),
                scratch.0.clone(),
                fake_target(&seen, &Arc::default(), false),
                token,
                None,
            );
            // Another source of the same loop, which has to keep running while the request waits.
            let turns = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let source = {
                let turns = turns.clone();
                glib::timeout_source_new(Duration::from_millis(5), None, glib::Priority::DEFAULT, move || {
                    turns.fetch_add(1, Ordering::SeqCst);
                    glib::ControlFlow::Continue
                })
            };
            source.attach(Some(context));
            let answer = Rc::new(RefCell::new(None::<Reply>));
            {
                let answer = answer.clone();
                host.focus_remote(
                    instance,
                    Request::Focus {
                        v: 1,
                        tab: Some(3),
                        card_first: true,
                        intent: Intent::Show,
                        token: None,
                    },
                    move |reply| *answer.borrow_mut() = Some(reply),
                );
            }
            assert_eq!(
                token_asked.get(),
                1,
                "the token is asked of this window's display before sending"
            );
            assert!(answer.borrow().is_none(), "nothing is answered on the calling thread");
            let deadline = Instant::now() + PATIENCE;
            while answer.borrow().is_none() {
                assert!(Instant::now() < deadline, "the request was never answered");
                context.iteration(true);
            }
            source.destroy();
            assert_eq!(*answer.borrow(), Some(Reply::Refused("timed out".into())));
            let turns = turns.load(Ordering::SeqCst);
            assert!(turns > 1, "other sources ran while the peer was silent ({turns} turns)");
        });
    }
}
