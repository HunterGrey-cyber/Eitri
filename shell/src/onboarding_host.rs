//! The first-start screen's work, carried out for the controller: the checks that decide whether the screen opens, the
//! detection rows, the replay and the write of `init.lua`, the marker, and the two things the screen starts in the
//! editor (the lines it would write, the tutor).
//!
//! Nothing blocks the GTK thread. Each job (the trigger or the check, the detection, the write) runs on one
//! `std::thread` of its own and answers over one channel; a GLib timer polls the channel only while a job is out, as
//! `root_conflicts` does. The write has no cap on the wait (it ends); a detection job is given up on after
//! [`DETECT_WAIT`], and a row it never answered stays `looking…`. Every answer goes to the controller as an event
//! carrying the generation the job was started under, and the controller drops one for a screen that is gone.
//!
//! What a worker reads or writes is its own: the screen's data comes in by value, and nothing a worker finds is
//! turned into a command or into the generated file here.
//!
//! `EITRI_ONBOARDING_LOG` (a non-empty path) gets one JSON line per change of what the screen shows and one per key
//! pressed while it shows; see [`ViewLog`]. Off by default, and nothing else reads it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::keymap::tmux::TmuxEnv;
use eitri_core::keymap::{KeySpec, Keymap, LuaClaims, TmuxImport};
use eitri_core::nvim_keys::{findings::findings, nvim_leader_token, rescan::rescan, NvimReport};
use eitri_core::nvim_rpc::RpcError;
use eitri_core::onboarding::screen::{CheckWhy, MarkerEvent};
use eitri_core::onboarding::{
    check, detect, display::display_text, marker, presets, trigger, write, Build, Found, Generated, Lang,
    OnboardingView, OpenMode, Opening, ShowWhy, ViewKey, WriteOutcome,
};
use eitri_core::wm::{
    Applied, Controller, Driver, HostEvent, KeyPress, KeyStage, Mods, OnboardingEvent, OnboardingOp, WinId,
};
use gtk4::glib;
use gtk4::prelude::*;

use crate::gtk_host::onboarding::OnboardingScreen;
use crate::onboarding_detect::{self, DetectContext};

/// How long the GTK thread waits for the detection rows before it stops listening for them.
const DETECT_WAIT: Duration = Duration::from_secs(5);
const POLL_EVERY: Duration = Duration::from_millis(100);
/// How often a rescan asked of the editor is looked at, and how long the screen waits for the editor to take it.
const RESCAN_POLL: Duration = Duration::from_millis(50);
const RESCAN_GIVE_UP: Duration = Duration::from_secs(5);
/// The title of the scratch buffer `o` opens.
const LINES_TITLE: &str = "init.lua lines";
/// What `t` runs in the editor: a constant, so nothing detected can become Lua source there.
const TUTOR_SNIPPET: &str = "vim.cmd('EitriTutor')";

/// What the host needs of the window.
pub(crate) struct Deps {
    pub(crate) driver: Weak<Driver<Controller>>,
    pub(crate) win: WinId,
    pub(crate) screen: Rc<OnboardingScreen>,
    /// The directory `init.lua` is read from and written in.
    pub(crate) config_dir: PathBuf,
    /// The marker file; `None` when the state home cannot be told.
    pub(crate) marker: Option<PathBuf>,
    /// The keymap the window runs: the screen presses against it.
    pub(crate) keymap: Rc<Keymap>,
    pub(crate) lang: Lang,
    /// What the window's own tmux import found, for the tmux row.
    pub(crate) tmux: Option<TmuxImport>,
    /// Whether a trust question holds the keys now.
    pub(crate) trust_open: Rc<dyn Fn() -> bool>,
    /// The Lua panels `init.lua` registered and what they claim: what the keymap check meets besides the file.
    pub(crate) panels: Rc<dyn Fn() -> (Vec<String>, LuaClaims)>,
    /// Shows a text in a scratch buffer of the editor: the panel's, which owns the window's scratch directory.
    pub(crate) view_in_editor: Rc<dyn Fn(&str, &str) -> Result<(), String>>,
    pub(crate) log: Option<ViewLog>,
}

/// What a worker tells the GTK thread.
enum Msg {
    /// The check said the screen opens, in `mode`.
    Open {
        generation: u64,
        mode: OpenMode,
        show_why: Option<ShowWhy>,
        today: String,
    },
    Found {
        generation: u64,
        found: Found,
    },
    Written {
        generation: u64,
        outcome: WriteOutcome,
    },
    CheckFailed {
        generation: u64,
        why: CheckWhy,
    },
    /// The job `0` ended: nothing more comes from it.
    Ended(u64),
}

pub(crate) struct OnboardingHost {
    deps: Deps,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    /// The jobs out, each with the time after which the GTK thread stops waiting for it.
    jobs: RefCell<HashMap<u64, Option<Instant>>>,
    next_job: Cell<u64>,
    timer: RefCell<Option<glib::SourceId>>,
    /// The last report of the editor's mappings, for the Neovim row.
    report: RefCell<Option<NvimReport>>,
    editor: RefCell<Option<Rc<dyn EditorRpc>>>,
    /// What the start knew when it started the trigger, for a check that is not asked for.
    inputs: RefCell<Option<trigger::Inputs>>,
    /// The view last drawn, for the log.
    shown: RefCell<Option<OnboardingView>>,
}

impl OnboardingHost {
    pub(crate) fn new(deps: Deps) -> Rc<OnboardingHost> {
        let (tx, rx) = mpsc::channel();
        Rc::new(OnboardingHost {
            deps,
            tx,
            rx,
            jobs: RefCell::new(HashMap::new()),
            next_job: Cell::new(1),
            timer: RefCell::new(None),
            report: RefCell::new(None),
            editor: RefCell::new(None),
            inputs: RefCell::new(None),
            shown: RefCell::new(None),
        })
    }

    /// The window's one editor handle.
    pub(crate) fn set_editor(&self, editor: Rc<dyn EditorRpc>) {
        *self.editor.borrow_mut() = Some(editor);
    }

    /// Carries out one op. Answers `Done` once the work is started; the result comes back as an event.
    pub(crate) fn apply(self: &Rc<Self>, op: &OnboardingOp) -> Applied {
        match op {
            OnboardingOp::Check {
                generation, requested, ..
            } => self.check(*generation, *requested),
            OnboardingOp::Detect { generation, .. } => self.detect(*generation),
            OnboardingOp::Rescan { generation, extra, .. } => {
                let extra: Vec<String> = extra.iter().map(KeySpec::to_vim).collect();
                self.rescan(*generation, &extra);
                Applied::Done
            }
            OnboardingOp::Scroll { by, .. } => {
                self.deps.screen.scroll(*by);
                Applied::Done
            }
            OnboardingOp::Write {
                generation, generated, ..
            } => self.write(*generation, generated.clone()),
            OnboardingOp::Marker { event, today, .. } => self.marker(*event, today.clone()),
            OnboardingOp::OpenLines { lines, .. } => {
                self.open_lines(lines);
                Applied::Done
            }
            OnboardingOp::OpenTutor { .. } => {
                self.open_tutor();
                Applied::Done
            }
        }
    }

    /// Starts the first-start decision: whether this launch opens the screen by itself. Called once the config phase
    /// has run, with what the start knows.
    pub(crate) fn start_trigger(self: &Rc<Self>, inputs: trigger::Inputs) {
        *self.inputs.borrow_mut() = Some(inputs.clone());
        self.decide(0, inputs);
    }

    fn decide(self: &Rc<Self>, generation: u64, inputs: trigger::Inputs) {
        self.spawn("onboarding-trigger", None, move |tx| {
            let (decision, log) = trigger::decide(&inputs);
            for line in log {
                eprintln!("{}", line.escape_debug());
            }
            if decision == trigger::Decision::Open {
                let _ = tx.send(Msg::Open {
                    generation,
                    mode: OpenMode::First,
                    show_why: None,
                    today: eitri_core::onboarding::today(),
                });
            }
        });
    }

    fn check(self: &Rc<Self>, generation: u64, requested: bool) -> Applied {
        let inputs = self.inputs.borrow().clone();
        match (requested, inputs) {
            // The start's own question again, as it was asked then.
            (false, Some(inputs)) => {
                self.decide(generation, inputs);
                Applied::Done
            }
            _ => {
                let dir = self.deps.config_dir.clone();
                self.spawn("onboarding-check", None, move |tx| {
                    let (mode, show_why) = trigger::reopen_mode(&dir);
                    let _ = tx.send(Msg::Open {
                        generation,
                        mode,
                        show_why,
                        today: eitri_core::onboarding::today(),
                    });
                });
                Applied::Done
            }
        }
    }

    fn detect(self: &Rc<Self>, generation: u64) -> Applied {
        let tmux = self.deps.tmux.clone();
        self.spawn("onboarding-detect", Some(Instant::now() + DETECT_WAIT), move |tx| {
            let ctx = DetectContext::live(tmux, Build::THIS_BINARY);
            onboarding_detect::run(ctx, |found| {
                let _ = tx.send(Msg::Found { generation, found });
            });
        });
        Applied::Done
    }

    /// Asks the editor for a report of its mappings now, unchanged or not: the feed sends only a report that differs
    /// from the last, and a mapping a plugin makes on `InsertEnter`, or a buffer-local one, may be missing from it.
    /// The answer comes back through the feed as any report does ([`OnboardingHost::nvim_report`]). The request is
    /// looked at every [`RESCAN_POLL`] without waiting on the GTK thread; an editor that is not there, refuses, or has
    /// not taken it within [`RESCAN_GIVE_UP`] leaves the row with the last report there is, or reading when there is
    /// none. `extra` is probed as well, for that report only: the prefixes in play may not be among the fixed chords.
    fn rescan(self: &Rc<Self>, generation: u64, extra: &[String]) {
        let editor = self.editor.borrow().clone();
        let Some(editor) = editor else {
            self.replay(generation);
            return;
        };
        let pending = rescan(&*editor, extra);
        let started = Instant::now();
        let weak = Rc::downgrade(self);
        let look = move || {
            let Some(host) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            match rescan_poll(pending.try_take(), started.elapsed()) {
                RescanPoll::Waiting => glib::ControlFlow::Continue,
                RescanPoll::Asked => glib::ControlFlow::Break,
                RescanPoll::Unanswered => {
                    host.replay(generation);
                    glib::ControlFlow::Break
                }
            }
        };
        if look() == glib::ControlFlow::Continue {
            glib::timeout_add_local(RESCAN_POLL, look);
        }
    }

    /// The Neovim row from the last report, for a screen the editor could not be asked for a fresh one; with none,
    /// nothing is sent and the row keeps reading.
    fn replay(&self, generation: u64) {
        let report = self.report.borrow().clone();
        if let Some(report) = report {
            self.post(OnboardingEvent::Found {
                win: self.deps.win,
                generation,
                found: neovim_found(&report, self.deps.lang, self.deps.keymap.prefix()),
            });
        }
    }

    /// The editor's mappings were reported: remembered, and handed to the screen if one is up, whatever its
    /// generation (the screen the start opens by itself has generation `0`).
    pub(crate) fn nvim_report(&self, report: NvimReport) {
        let found = neovim_found(&report, self.deps.lang, self.deps.keymap.prefix());
        *self.report.borrow_mut() = Some(report);
        let Some(driver) = self.deps.driver.upgrade() else {
            return;
        };
        let Some(generation) = driver.onboarding_generation(self.deps.win) else {
            return;
        };
        self.post(OnboardingEvent::Found {
            win: self.deps.win,
            generation,
            found,
        });
    }

    /// The trust gate's question opened or closed: the screen gives the keys up, or takes them back. `0` names
    /// whatever screen is up.
    pub(crate) fn trust_changed(&self, open: bool) {
        self.post(OnboardingEvent::Found {
            win: self.deps.win,
            generation: 0,
            found: Found::TrustOpen(open),
        });
    }

    /// Checks the file the user chose, then writes it, on one worker: the replay, the data check, the writer, in that
    /// order. A file either fails its check and nothing is written, or it is written once. The screen reads no key
    /// while it writes, so the worker always answers: a panic answers through [`AnswerGuard`], and a worker that
    /// cannot start is answered for here.
    fn write(self: &Rc<Self>, generation: u64, generated: Generated) -> Applied {
        let (panels, claims) = (self.deps.panels)();
        let (dir, lang) = (self.deps.config_dir.clone(), self.deps.lang);
        let started = self.spawn("onboarding-write", None, move |tx| {
            let answer = AnswerGuard::new(
                tx,
                Msg::CheckFailed {
                    generation,
                    why: CheckWhy::Unverified,
                },
            );
            let message = match check::replay(&generated) {
                Err(failure) => {
                    eprintln!("[onboarding] the generated file failed its replay: {failure:?}");
                    Msg::CheckFailed {
                        generation,
                        why: CheckWhy::Unverified,
                    }
                }
                Ok(()) => match check::data_check(&generated, &panels, &claims, TmuxEnv::from_process) {
                    Err(message) => Msg::CheckFailed {
                        generation,
                        why: CheckWhy::Refused(message),
                    },
                    Ok(()) => Msg::Written {
                        generation,
                        outcome: write::write_init(&dir, &generated.text(), lang),
                    },
                },
            };
            answer.send(message);
        });
        if !started {
            self.post(OnboardingEvent::CheckFailed {
                win: self.deps.win,
                generation,
                why: CheckWhy::Unverified,
            });
        }
        Applied::Done
    }

    /// Records a "not now" or a "don't ask again" on a worker; the file system may be slow. A failure is one log
    /// line: the screen is gone either way and opens again next start.
    fn marker(&self, event: MarkerEvent, today: String) -> Applied {
        let Some(path) = self.deps.marker.clone() else {
            eprintln!("[onboarding] no state directory to remember the choice in");
            return Applied::Done;
        };
        let spawned = std::thread::Builder::new()
            .name("onboarding-marker".into())
            .spawn(move || {
                let result = match event {
                    MarkerEvent::NotNow => marker::not_now(&path, &today),
                    MarkerEvent::Declined => marker::decline(&path, &today),
                };
                if let Err(err) = result {
                    eprintln!("[onboarding] could not record the choice in {path:?} ({})", err.kind());
                }
            });
        match spawned {
            Ok(_) => Applied::Done,
            Err(err) => {
                eprintln!("[onboarding] could not start the marker write ({})", err.kind());
                Applied::Refused
            }
        }
    }

    /// Shows what the file would hold in a scratch buffer, after the screen has gone: the editor takes the keys then.
    fn open_lines(self: &Rc<Self>, lines: &[String]) {
        let mut text = lines.join("\n");
        text.push('\n');
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(host) = weak.upgrade() else { return };
            if let Err(why) = (host.deps.view_in_editor)(LINES_TITLE, &text) {
                host.refused(&why);
            }
        });
    }

    /// Starts `:EitriTutor` in the editor, after the screen has gone. The editor is shown and given the keys first (a
    /// restored layout may have it hidden or tabbed away, the lesson would open out of sight), and the answer is looked
    /// at every [`RESCAN_POLL`] without waiting on the GTK thread: an error (no tutor installed in this session) is a
    /// toast. An editor that has not answered within [`RESCAN_GIVE_UP`] is busy, not failing: the call stays queued.
    fn open_tutor(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(host) = weak.upgrade() else { return };
            let editor = host.editor.borrow().clone();
            let Some(editor) = editor else {
                host.refused("the editor is not connected to this window");
                return;
            };
            let Some(driver) = host.deps.driver.upgrade() else {
                return;
            };
            if let Err(why) = driver.editor_request(host.deps.win, String::new()) {
                host.refused(&why);
                return;
            }
            let pending = editor.exec_lua(TUTOR_SNIPPET, Vec::new());
            let started = Instant::now();
            let weak = Rc::downgrade(&host);
            let look = move || {
                let Some(host) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                match tutor_poll(pending.try_take(), started.elapsed()) {
                    TutorPoll::Waiting => glib::ControlFlow::Continue,
                    TutorPoll::Started => glib::ControlFlow::Break,
                    TutorPoll::Failed(why) => {
                        host.refused(&format!("the tutor could not be started: {why}"));
                        glib::ControlFlow::Break
                    }
                }
            };
            if look() == glib::ControlFlow::Continue {
                glib::timeout_add_local(RESCAN_POLL, look);
            }
        });
    }

    /// The screen is gone, so what could not be done is a toast.
    fn refused(&self, why: &str) {
        if let Some(driver) = self.deps.driver.upgrade() {
            driver.nvim_refused(self.deps.win, why);
        }
    }

    /// A click on a button: the key it stands for, posted as the window stage reads every key.
    pub(crate) fn pressed(&self, key: ViewKey) {
        let Some(driver) = self.deps.driver.upgrade() else {
            return;
        };
        let (press, name) = key.press();
        let press = KeyPress {
            keycode: 0,
            press,
            key: name,
            mods: Mods::default(),
            super_or_hyper: false,
            super_or_meta: false,
        };
        driver.key(self.deps.win, &KeyStage::Window, press);
    }

    fn post(&self, event: OnboardingEvent) {
        if let Some(driver) = self.deps.driver.upgrade() {
            driver.post(HostEvent::Onboarding(event));
        }
    }

    // --- the log -------------------------------------------------------------------------------------------

    /// The chrome drew `view` (or took the screen down): one log line per change.
    pub(crate) fn view_drawn(&self, view: Option<&OnboardingView>) {
        *self.shown.borrow_mut() = view.cloned();
        if let (Some(log), Some(view)) = (&self.deps.log, view) {
            log.view(view, self.deps.screen.has_keys());
        }
    }

    /// Writes a line for every key pressed while the screen shows, after the key has been decided. A passive capture:
    /// it stops nothing and reads nothing but the fact that a key came.
    pub(crate) fn watch_keys(self: &Rc<Self>, window: &gtk4::Window) {
        if self.deps.log.is_none() {
            return;
        }
        let controller = gtk4::EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        controller.connect_key_pressed(move |_, _, _, _| {
            let weak = weak.clone();
            // After the key's own decision has been applied.
            glib::idle_add_local_once(move || {
                let Some(host) = weak.upgrade() else { return };
                let view = host.shown.borrow().clone();
                if let (Some(log), Some(view)) = (&host.deps.log, view) {
                    log.key(&view, host.deps.screen.has_keys());
                }
            });
            glib::Propagation::Proceed
        });
        window.add_controller(controller);
    }

    // --- jobs ----------------------------------------------------------------------------------------------

    /// Runs `work` on a thread of its own and polls for what it sends until it ends or, when it has a deadline, until
    /// that passes. The job's end is sent when `work` returns or panics. `false` when the thread could not start.
    fn spawn(
        self: &Rc<Self>,
        name: &str,
        deadline: Option<Instant>,
        work: impl FnOnce(&mpsc::Sender<Msg>) + Send + 'static,
    ) -> bool {
        let id = self.next_job.get();
        self.next_job.set(id + 1);
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new().name(name.to_string()).spawn(move || {
            let _end = EndGuard { id, tx: tx.clone() };
            work(&tx);
        });
        match spawned {
            Ok(_) => {
                self.jobs.borrow_mut().insert(id, deadline);
                self.poll_while_out();
                true
            }
            Err(err) => {
                eprintln!("[onboarding] could not start {name} ({})", err.kind());
                false
            }
        }
    }

    /// Arms the poll timer if it is not running. It stops itself when no job is out.
    fn poll_while_out(self: &Rc<Self>) {
        if self.timer.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local(POLL_EVERY, move || match weak.upgrade() {
            Some(host) => host.poll(),
            None => glib::ControlFlow::Break,
        });
        *self.timer.borrow_mut() = Some(id);
    }

    fn poll(&self) -> glib::ControlFlow {
        while let Ok(msg) = self.rx.try_recv() {
            self.deliver(msg);
        }
        let now = Instant::now();
        let mut jobs = self.jobs.borrow_mut();
        // A job whose time is up is not waited for: its row stays as it is, and its late answer is dropped by the
        // controller if the screen has gone.
        jobs.retain(|_, deadline| deadline.is_none_or(|at| now < at));
        if jobs.is_empty() {
            *self.timer.borrow_mut() = None;
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    }

    fn deliver(&self, msg: Msg) {
        let win = self.deps.win;
        match msg {
            Msg::Ended(id) => {
                self.jobs.borrow_mut().remove(&id);
            }
            Msg::Open {
                generation,
                mode,
                show_why,
                today,
            } => {
                let opening = self.opening(generation, mode, show_why, today);
                self.post(OnboardingEvent::Open { win, opening });
            }
            Msg::Found { generation, found } => self.post(OnboardingEvent::Found { win, generation, found }),
            Msg::Written { generation, outcome } => self.post(OnboardingEvent::Written {
                win,
                generation,
                outcome,
            }),
            Msg::CheckFailed { generation, why } => self.post(OnboardingEvent::CheckFailed { win, generation, why }),
        }
    }

    /// What the controller opens the screen with. Built here, on the GTK thread, because the keymap and the trust gate
    /// are this thread's.
    fn opening(&self, generation: u64, mode: OpenMode, show_why: Option<ShowWhy>, today: String) -> Opening {
        let keymap = &self.deps.keymap;
        Opening {
            generation,
            mode,
            show_why,
            trust_open: (self.deps.trust_open)(),
            lang: self.deps.lang,
            build: Build::THIS_BINARY,
            running_prefix: *keymap.prefix(),
            keymap_check: keymap.bindings().iter().map(|b| (b.key, b.action.name())).collect(),
            today,
            config_path: self.deps.config_dir.display().to_string(),
        }
    }
}

/// Sends the end of its job when it is dropped: at the end of the worker, or while a panic unwinds it.
struct EndGuard {
    id: u64,
    tx: mpsc::Sender<Msg>,
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Ended(self.id));
    }
}

/// A worker's one answer, with the one to give if the worker never gets to its own (a panic unwinding it).
struct AnswerGuard<'a> {
    tx: &'a mpsc::Sender<Msg>,
    fallback: Option<Msg>,
}

impl<'a> AnswerGuard<'a> {
    fn new(tx: &'a mpsc::Sender<Msg>, fallback: Msg) -> AnswerGuard<'a> {
        AnswerGuard {
            tx,
            fallback: Some(fallback),
        }
    }

    fn send(mut self, answer: Msg) {
        self.fallback = None;
        let _ = self.tx.send(answer);
    }
}

impl Drop for AnswerGuard<'_> {
    fn drop(&mut self) {
        if let Some(fallback) = self.fallback.take() {
            let _ = self.tx.send(fallback);
        }
    }
}

/// What a look at the editor's answer to the rescan request found.
#[derive(Debug, PartialEq, Eq)]
enum RescanPoll {
    /// Not answered yet, and not yet given up on.
    Waiting,
    /// The feed took the request: the report comes through it.
    Asked,
    /// No feed, no editor, an error, or too long: the row makes do with the last report.
    Unanswered,
}

fn rescan_poll(answer: Option<Result<rmpv::Value, RpcError>>, waited: Duration) -> RescanPoll {
    match answer {
        Some(Ok(rmpv::Value::Boolean(true))) => RescanPoll::Asked,
        Some(_) => RescanPoll::Unanswered,
        None if waited >= RESCAN_GIVE_UP => RescanPoll::Unanswered,
        None => RescanPoll::Waiting,
    }
}

/// What a look at the editor's answer to `:EitriTutor` found.
#[derive(Debug, PartialEq, Eq)]
enum TutorPoll {
    /// Not answered yet, and not yet given up on.
    Waiting,
    /// It ran, or it is still queued behind whatever keeps the editor busy.
    Started,
    /// The editor answered with an error.
    Failed(String),
}

fn tutor_poll(answer: Option<Result<rmpv::Value, RpcError>>, waited: Duration) -> TutorPoll {
    match answer {
        Some(Ok(_)) => TutorPoll::Started,
        Some(Err(err)) => TutorPoll::Failed(err.to_string()),
        None if waited >= RESCAN_GIVE_UP => TutorPoll::Started,
        None => TutorPoll::Waiting,
    }
}

/// The Neovim row from a report, with every piece of text the probe supplied cut and escaped as it fills a template.
fn neovim_found(report: &NvimReport, lang: Lang, prefix: &KeySpec) -> Found {
    let mut row = detect::neovim_row_with(&findings(report), lang, &display_text, prefix);
    let lhs: Vec<String> = report.maps.iter().map(|map| map.lhs.clone()).collect();
    row.nvim_lhs = presets::free_nvim_lhs(&lhs, nvim_leader_token(report));
    Found::Neovim(row)
}

/// The environment's reading of `EITRI_ONBOARDING_LOG`, as the start hands it over.
pub(crate) fn log_from_env(env: &dyn Fn(&str) -> Option<OsString>) -> Option<ViewLog> {
    env("EITRI_ONBOARDING_LOG")
        .filter(|path| !path.is_empty())
        .map(|path| ViewLog::new(PathBuf::from(path)))
}

/// The log a pass reads: one JSON line per change of what the screen shows, one per key pressed while it shows.
/// A line is `{"step", "title", "rows": [{"label", "text", "note"}], "items": [{"number", "name", "selected"}],
/// "question", "body", "status", "buttons", "footer", "focus"}`, `focus` being `"screen"` while the keys are on the
/// screen and `"module"` while a module holds them.
pub(crate) struct ViewLog {
    path: PathBuf,
    last: RefCell<Option<OnboardingView>>,
}

impl ViewLog {
    pub(crate) fn new(path: PathBuf) -> ViewLog {
        ViewLog {
            path,
            last: RefCell::new(None),
        }
    }

    /// `view` was drawn: a line unless it is what was drawn last.
    pub(crate) fn view(&self, view: &OnboardingView, on_screen: bool) {
        if self.last.borrow().as_ref() == Some(view) {
            return;
        }
        *self.last.borrow_mut() = Some(view.clone());
        self.append(&line(view, on_screen));
    }

    /// A key came while `view` shows: a line, whether or not the view changed.
    pub(crate) fn key(&self, view: &OnboardingView, on_screen: bool) {
        self.append(&line(view, on_screen));
    }

    fn append(&self, line: &str) {
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(err) = written {
            eprintln!("[onboarding] could not write the log {:?} ({})", self.path, err.kind());
        }
    }
}

fn line(view: &OnboardingView, on_screen: bool) -> String {
    serde_json::json!({
        "step": view.step,
        "title": view.title,
        "rows": view
            .rows
            .iter()
            .map(|r| serde_json::json!({ "label": r.label, "text": r.text, "note": r.note }))
            .collect::<Vec<_>>(),
        "items": view
            .items
            .iter()
            .map(|i| serde_json::json!({ "number": i.number, "name": i.name, "selected": i.selected }))
            .collect::<Vec<_>>(),
        "question": view.question,
        "body": view.body,
        "status": view.status,
        "buttons": view.buttons.iter().map(|b| format!("{}", b.key)).collect::<Vec<_>>(),
        "footer": view.footer,
        "focus": if on_screen { "screen" } else { "module" },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use eitri_core::onboarding::{ViewItem, ViewRow};

    use super::*;

    fn view(step: &str) -> OnboardingView {
        OnboardingView {
            title: "Set up".into(),
            step: step.into(),
            rows: vec![ViewRow {
                label: "tmux".into(),
                text: "~/.tmux.conf: prefix Ctrl+a".into(),
                note: String::new(),
            }],
            question: None,
            items: vec![ViewItem {
                number: 1,
                name: "Neovim + tmux".into(),
                summary: String::new(),
                tag: None,
                selected: true,
            }],
            body: vec!["-- line".into()],
            status: Vec::new(),
            footer: "Enter next".into(),
            buttons: Vec::new(),
            help: None,
        }
    }

    #[test]
    fn the_log_is_one_json_line_per_change() {
        let dir = eitri_core::plugin::test_support::Scratch::new("onboarding-log");
        let path = dir.0.join("log");
        let log = ViewLog::new(path.clone());
        log.view(&view("1 / 2"), true);
        log.view(&view("1 / 2"), true);
        log.view(&view("2 / 2"), true);
        log.key(&view("2 / 2"), false);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(lines[0]["step"], "1 / 2");
        assert_eq!(lines[0]["focus"], "screen");
        assert_eq!(lines[0]["rows"][0]["text"], "~/.tmux.conf: prefix Ctrl+a");
        assert_eq!(lines[0]["body"][0], "-- line");
        assert_eq!(lines[0]["footer"], "Enter next");
        assert_eq!(lines[1]["step"], "2 / 2");
        assert_eq!(lines[2]["focus"], "module", "a key's line says where the keys are");
    }

    #[test]
    fn a_rescan_is_done_with_when_the_feed_took_it_or_cannot_and_waits_otherwise() {
        let soon = Duration::from_millis(10);
        assert_eq!(
            rescan_poll(Some(Ok(rmpv::Value::Boolean(true))), soon),
            RescanPoll::Asked
        );
        assert_eq!(
            rescan_poll(Some(Ok(rmpv::Value::Boolean(false))), soon),
            RescanPoll::Unanswered,
            "no feed is running"
        );
        assert_eq!(
            rescan_poll(Some(Err(RpcError::Unavailable("not ready".into()))), soon),
            RescanPoll::Unanswered
        );
        assert_eq!(
            rescan_poll(None, soon),
            RescanPoll::Waiting,
            "nvim may be busy, not failed"
        );
        assert_eq!(rescan_poll(None, RESCAN_GIVE_UP), RescanPoll::Unanswered);
    }

    #[test]
    fn a_tutor_the_editor_refuses_is_said_and_a_busy_editor_is_not_a_failure() {
        let soon = Duration::from_millis(10);
        assert_eq!(
            tutor_poll(Some(Ok(rmpv::Value::Nil)), soon),
            TutorPoll::Started,
            "the command ran"
        );
        let refused = tutor_poll(Some(Err(RpcError::Unavailable("E492".into()))), soon);
        let TutorPoll::Failed(why) = refused else {
            panic!("{refused:?}");
        };
        assert!(why.contains("E492"), "{why}");
        assert_eq!(tutor_poll(None, soon), TutorPoll::Waiting);
        assert_eq!(
            tutor_poll(None, RESCAN_GIVE_UP),
            TutorPoll::Started,
            "still queued: nvim runs it when it is free"
        );
    }

    #[test]
    fn a_rescan_request_reaches_the_editor_and_its_answer_is_read() {
        use eitri_core::nvim_keys::rescan::{rescan, KEYS_RESCAN_LUA};
        use eitri_core::nvim_rpc::Pending;
        struct Editor {
            calls: RefCell<Vec<&'static str>>,
        }
        impl EditorRpc for Editor {
            fn exec_lua(&self, code: &'static str, _args: Vec<rmpv::Value>) -> Pending {
                self.calls.borrow_mut().push(code);
                let (answer, pending) = Pending::pair();
                answer.send(Ok(rmpv::Value::Boolean(true)));
                pending
            }

            fn target(&self) -> Option<u64> {
                Some(1)
            }
        }
        let editor = Editor {
            calls: RefCell::default(),
        };
        let pending = rescan(&editor, &[]);
        assert_eq!(*editor.calls.borrow(), [KEYS_RESCAN_LUA]);
        assert_eq!(rescan_poll(pending.try_take(), Duration::ZERO), RescanPoll::Asked);
    }

    #[test]
    fn a_worker_that_panics_still_answers_and_ends() {
        let (tx, rx) = mpsc::channel();
        let sent = tx.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _end = EndGuard {
                id: 7,
                tx: sent.clone(),
            };
            let _answer = AnswerGuard::new(
                &sent,
                Msg::CheckFailed {
                    generation: 3,
                    why: CheckWhy::Unverified,
                },
            );
            panic!("the worker broke");
        }));
        assert!(result.is_err());
        let first = rx.try_recv().expect("the answer");
        assert!(matches!(first, Msg::CheckFailed { generation: 3, .. }));
        assert!(matches!(rx.try_recv(), Ok(Msg::Ended(7))));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_worker_that_answers_sends_the_one_answer_only() {
        let (tx, rx) = mpsc::channel();
        {
            let _end = EndGuard { id: 1, tx: tx.clone() };
            AnswerGuard::new(
                &tx,
                Msg::CheckFailed {
                    generation: 3,
                    why: CheckWhy::Unverified,
                },
            )
            .send(Msg::Written {
                generation: 3,
                outcome: WriteOutcome::Written(PathBuf::from("/x")),
            });
        }
        assert!(matches!(rx.try_recv(), Ok(Msg::Written { .. })));
        assert!(matches!(rx.try_recv(), Ok(Msg::Ended(1))));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_log_is_off_with_the_variable_unset_or_empty() {
        let set =
            |value: &'static str| move |name: &str| (name == "EITRI_ONBOARDING_LOG").then(|| OsString::from(value));
        assert!(log_from_env(&|_| None).is_none());
        assert!(log_from_env(&set("")).is_none());
        let log = log_from_env(&set("/scratch/log")).expect("a path turns it on");
        assert_eq!(log.path, PathBuf::from("/scratch/log"));
    }

    #[test]
    fn a_log_that_cannot_be_written_is_a_line_on_stderr_and_nothing_more() {
        let log = ViewLog::new(PathBuf::from("/nonexistent/eitri-onboarding/log"));
        log.view(&view("1 / 2"), true);
    }
}
