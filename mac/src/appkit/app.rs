//! The app from start to exit: `main`'s order, the folder chooser, waiting for Neovide's nvim before the link
//! is aimed at it, the question before a quit while an agent is at work, and the shutdown that every way out
//! runs exactly once.
//!
//! **Every way out runs the shutdown.** When nvim exits, Neovide's loop ends and `neovide::embed::run` returns;
//! `run_main` then shuts down. A quit that ends in AppKit's `terminate:` (the Dock's Quit, a logout, a script's
//! `quit app`) may end the process without returning from `run`, so the same shutdown also runs synchronously
//! on `NSApplicationWillTerminateNotification`. Whichever comes first does it; the other finds it done.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::time::Instant;

use block2::RcBlock;
use eitri_panel::panel_page::PanelHost;
use neovide::embed::EmbedConfig;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSApplicationWillTerminateNotification, NSButton, NSModalResponse, NSModalResponseOK, NSOpenPanel,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSString};

use super::page::{repeat, start_editor_tick, PANEL_DOCUMENT};
use super::window::{host_for_later, on_next_turn, Host, MacEmbedder};
use crate::assembly::{assemble, Assembly};
use crate::editor::{AttachStep, AttachWait, Editor};
use crate::quit::{QuitAfter, QuitDecision, QuitGate, QuitSource};
use crate::startup::{self, ProjectStart};

/// What starting and leaving have done, for the log and the acceptance build.
#[derive(Default)]
pub struct Lifecycle {
    quit: RefCell<QuitGate>,
    /// Each quit request and answer, in order.
    quit_log: RefCell<Vec<String>>,
    /// Where waiting for nvim stands.
    attach: RefCell<String>,
    finished: Cell<bool>,
    finished_via: Cell<Option<&'static str>>,
    menu_insertions: Cell<u32>,
    new_window_items_removed: Cell<usize>,
    quit_items_retargeted: Cell<usize>,
    /// The open quit question's Cancel button, for Escape (`Host::cancel_quit_question`).
    quit_cancel: RefCell<Option<Retained<NSButton>>>,
}

impl Lifecycle {
    pub(super) fn menu_inserted(&self) {
        self.menu_insertions.set(self.menu_insertions.get() + 1);
    }

    pub(super) fn new_window_removed(&self, count: usize) {
        self.new_window_items_removed
            .set(self.new_window_items_removed.get() + count);
    }

    pub(super) fn quit_item_retargeted(&self, count: usize) {
        self.quit_items_retargeted.set(self.quit_items_retargeted.get() + count);
    }

    /// Quit items pointed at the window's question so far.
    pub fn quit_items_retargeted(&self) -> usize {
        self.quit_items_retargeted.get()
    }

    /// New Window items taken out of Neovide's main menu so far.
    pub fn new_window_items_removed(&self) -> usize {
        self.new_window_items_removed.get()
    }

    fn log_quit(&self, line: String) {
        crate::log_line!("[mac] quit: {line}");
        self.quit_log.borrow_mut().push(line);
    }

    pub fn quit_log(&self) -> Vec<String> {
        self.quit_log.borrow().clone()
    }

    pub fn attach_phase(&self) -> String {
        self.attach.borrow().clone()
    }

    pub fn finished_via(&self) -> Option<&'static str> {
        self.finished_via.get()
    }

    /// How many times the Edit menu was put into a main menu (once more each time Neovide replaced it).
    pub fn menu_insertions(&self) -> u32 {
        self.menu_insertions.get()
    }

    pub fn quit_asking(&self) -> bool {
        self.quit.borrow().is_asking()
    }
}

/// The panel's process side: links open in the user's handler; in an acceptance build they are recorded.
fn panel_host() -> Rc<dyn PanelHost> {
    #[cfg(feature = "acceptance")]
    return super::acceptance::panel_host();
    #[cfg(not(feature = "acceptance"))]
    Rc::new(super::page::MacPanelHost)
}

/// `main` on macOS, in this order: the environment and standard input first (before any thread), the
/// project, the working directory and the paths, the editor's sockets, the panel, then Neovide's window; the
/// shutdown once Neovide's loop has ended.
pub fn run_main() -> ExitCode {
    startup::scrub_environment();
    if let Err(e) = startup::detach_stdin_if_redirected() {
        crate::log_error!("eitri: standard input: {e}");
        return ExitCode::FAILURE;
    }
    // Before any thread: it sets `PATH` and the sidecar's `claude` for a start from the Dock or Finder.
    let command_env = crate::user_env::apply();
    let Some(mtm) = MainThreadMarker::new() else {
        crate::log_error!("eitri: not started on the main thread");
        return ExitCode::FAILURE;
    };
    // Read before the start moves stderr to the log: whether a failure's message would have reached anyone.
    let stderr = startup::output_goes(2).unwrap_or(startup::OutputGoes::Kept);
    match start(mtm, &command_env) {
        Ok(Some((host, config))) => {
            // A panic in Neovide's loop unwinds through here; the shutdown still runs on the way out.
            let guard = FinishOnUnwind(host.clone());
            let code = neovide::embed::run(config, Rc::new(MacEmbedder::new(host.clone())));
            host.finish("Neovide's loop ended and run returned", true);
            drop(guard);
            code
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(why) => {
            crate::log_error!("eitri: {why}");
            show_start_failure(mtm, &why, stderr);
            ExitCode::FAILURE
        }
    }
}

/// Runs the shutdown if `run` is left by a panic (a normal return has run it already, so this does nothing).
struct FinishOnUnwind(Rc<Host>);

impl Drop for FinishOnUnwind {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.finish("a panic is unwinding out of Neovide's loop", false);
        }
    }
}

/// A start that fails before the window says why on stderr; from the Dock or Finder that goes nowhere, so an
/// alert says it too (`startup::start_failure_needs_an_alert`). `stderr` is where it went when the app started:
/// the log that takes it over later is not someone reading.
fn show_start_failure(mtm: MainThreadMarker, why: &str, stderr: startup::OutputGoes) {
    // SAFETY: `getppid` takes nothing and cannot fail.
    let parent = unsafe { libc::getppid() };
    if !startup::start_failure_needs_an_alert(parent, stderr) {
        return;
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.activate();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Eitri could not start"));
    alert.setInformativeText(&NSString::from_str(why));
    alert.runModal();
}

/// Everything before Neovide's window. `Ok(None)`: the user cancelled the folder chooser.
/// `command_env` is what `user_env::apply` put in place, logged once the log is.
fn start(mtm: MainThreadMarker, command_env: &[String]) -> Result<Option<(Rc<Host>, EmbedConfig)>, String> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let cwd = std::env::current_dir().map_err(|e| format!("the working directory: {e}"))?;
    let root = match startup::project_start(&args, std::env::var_os("EITRI_PROJECT_DIR"), &cwd)? {
        ProjectStart::Root(root) => root,
        ProjectStart::Choose => match choose_project(mtm) {
            // The folder chosen goes through the same rules as one named on the command line (never `/`).
            Some(chosen) => match startup::project_start(&[chosen.into_os_string()], None, &cwd)? {
                ProjectStart::Root(root) => root,
                ProjectStart::Choose => return Err("/ is never a project directory".into()),
            },
            None => return Ok(None),
        },
    };
    std::env::set_current_dir(&root).map_err(|e| format!("cannot enter {}: {e}", root.display()))?;
    let home = std::env::home_dir().ok_or("no home directory")?;
    // From here on every line, this crate's and every other's, lands somewhere a write cannot fail.
    let log_dir = startup::log_dir(std::env::var_os("XDG_STATE_HOME").as_deref(), &home);
    let moved = startup::move_output_to_log(1, 2, log_dir.as_deref(), &root, &|log| match log {
        Some(log) => crate::log_error!("eitri: output goes to {}", log.display()),
        None => crate::log_error!("eitri: output goes nowhere (no log file could be made)"),
    });
    crate::log_line!(
        "[mac] eitri-mac {} (pid {}) for {}{}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        root.display(),
        match &moved.log {
            Some(log) => format!("; this log is {}", log.display()),
            None => String::new(),
        }
    );
    if moved.unlocked {
        crate::log_line!("[mac] could not take the log's shared lock; a later lone start may empty this log");
    }
    for line in command_env {
        crate::log_line!("{line}");
    }
    let paths = startup::paths(&|key| std::env::var_os(key), &home, &std::env::temp_dir())?;
    let editor = Rc::new(Editor::new(paths.nvim_listen.clone())?);

    let host = Host::new(mtm);
    let assembly = assemble(
        &root,
        &paths.config_dir,
        Some(host.page()),
        panel_host(),
        &PANEL_DOCUMENT,
        editor,
        host.clone(),
    )?;
    host.attach(Rc::new(assembly));
    let config = EmbedConfig {
        data_dir: Some(paths.neovide_data),
        config_file: Some(paths.neovide_config),
        args: startup::neovide_args(&paths.nvim_listen),
    };
    Ok(Some((host, config)))
}

/// A start from the Dock or Finder named no project: the user picks a folder. `None` on cancel.
fn choose_project(mtm: MainThreadMarker) -> Option<PathBuf> {
    let app = NSApplication::sharedApplication(mtm);
    // A start outside a bundle is not a regular app until told, and its panel would open behind others.
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.activate();
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setAllowsMultipleSelection(false);
    panel.setPrompt(Some(&NSString::from_str("Open")));
    panel.setMessage(Some(&NSString::from_str("Choose the project folder to open in Eitri")));
    if panel.runModal() != NSModalResponseOK {
        crate::log_line!("[mac] no project chosen");
        return None;
    }
    Some(PathBuf::from(panel.URL()?.path()?.to_string()))
}

/// Removes nvim's socket and the directory made for it, once nvim is gone. Only that file and an empty
/// directory: `remove_dir` refuses anything else.
fn remove_socket_dir(socket: &std::path::Path) {
    let _ = std::fs::remove_file(socket);
    if let Some(dir) = socket.parent() {
        if let Err(e) = std::fs::remove_dir(dir) {
            crate::log_line!("[mac] the editor's socket directory {} stays: {e}", dir.display());
        }
    }
}

/// Whether `path` is a listening socket's file (nvim creates it when it starts listening).
fn is_socket(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket())
}

impl Host {
    pub(super) fn observe_termination(self: &Rc<Self>) {
        let weak: Weak<Host> = Rc::downgrade(self);
        let block = RcBlock::new(move |_note: NonNull<NSNotification>| {
            if let Some(host) = weak.upgrade() {
                host.finish("AppKit is terminating the application", false);
            }
        });
        // SAFETY: AppKit's notification name; the observer runs on the posting (main) thread, and the token is
        // kept for the process's life.
        let token = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSApplicationWillTerminateNotification),
                None,
                None,
                &block,
            )
        };
        self.observers.borrow_mut().push(token);
    }

    /// Looks for nvim's socket every [`AttachWait::POLL`]; once it is there the link is aimed at it and the
    /// editor's tick starts. The band says it is waiting meanwhile, and says so if it gives up.
    pub(super) fn wait_for_the_editor(self: &Rc<Self>, assembly: Rc<Assembly>) {
        let mtm = self.mtm();
        // The clock starts at the first tick, when AppKit is running and Neovide is starting nvim, not now.
        let wait: Cell<Option<AttachWait>> = Cell::new(None);
        let reported_slow = Cell::new(false);
        let (state, text) = AttachWait::WAITING;
        assembly.report_editor_link(state, text);
        *self.life.attach.borrow_mut() = "waiting for nvim's socket".into();
        let started = Instant::now();
        let weak = Rc::downgrade(self);
        let timer = repeat(mtm, AttachWait::POLL, move || {
            let Some(host) = weak.upgrade() else { return false };
            let listen = assembly.editor.nvim_listen().to_path_buf();
            let now = Instant::now();
            let wait = wait.get().unwrap_or_else(|| {
                let started = AttachWait::new(now);
                wait.set(Some(started));
                started
            });
            match wait.step(now, is_socket(&listen)) {
                AttachStep::Wait => true,
                AttachStep::Attach => {
                    let after = started.elapsed().as_millis();
                    crate::log_line!("[mac] nvim listens after {after} ms; attaching");
                    *host.life.attach.borrow_mut() = format!("attach requested after {after} ms");
                    assembly.editor.attach();
                    let tick = start_editor_tick(mtm, assembly.clone());
                    host.timers.borrow_mut().push(tick);
                    false
                }
                AttachStep::GiveUp => {
                    // Said once; the looking goes on (one `lstat` a poll), so a late nvim still gets its link.
                    if !reported_slow.replace(true) {
                        crate::log_line!("[mac] nvim has not listened on {} yet; still looking", listen.display());
                        *host.life.attach.borrow_mut() = "nvim has not listened yet; still looking".into();
                        let (state, text) = AttachWait::GAVE_UP;
                        assembly.report_editor_link(state, text);
                    }
                    true
                }
            }
        });
        self.timers.borrow_mut().push(timer);
    }

    /// A quit the window sees before nvim does. `true`: it goes on now. `false`: it is held (a question is
    /// put, on the next turn) or dropped (one is already open).
    pub fn quit_requested(&self, source: QuitSource) -> bool {
        self.quit_decided(source, |gate, running, queued| {
            gate.request(source, running, queued, Instant::now())
        })
    }

    /// The quit chord from the key monitor, `repeat` for the key's auto-repeat. `true`: it goes on (to the
    /// menu's Quit) without the prefix seeing it.
    pub fn quit_chord(&self, repeat: bool) -> bool {
        self.quit_decided(QuitSource::Key, |gate, running, queued| {
            gate.request_chord(repeat, running, queued, Instant::now())
        })
    }

    fn quit_decided(
        &self,
        source: QuitSource,
        decide: impl FnOnce(&mut QuitGate, usize, usize) -> QuitDecision,
    ) -> bool {
        let Some(assembly) = self.assembly() else { return true };
        if self.life.finished.get() {
            return true;
        }
        let (running, queued) = (assembly.panel.running_count(), assembly.panel.queued_count());
        let decision = decide(&mut self.life.quit.borrow_mut(), running, queued);
        self.life.log_quit(format!("{source:?}: {decision:?}"));
        match decision {
            QuitDecision::Proceed => true,
            QuitDecision::AlreadyAsking => false,
            QuitDecision::Ask { what } => {
                if let Some(host) = host_for_later() {
                    on_next_turn(self.mtm(), move || host.ask_before_quitting(&what));
                }
                false
            }
        }
    }

    /// The question, as a sheet on the window, with Cancel the default button.
    fn ask_before_quitting(self: &Rc<Self>, what: &str) {
        let Some(window) = self.ns_window() else {
            // No window to ask in: the request is dropped, which is the cautious answer.
            let _ = self.life.quit.borrow_mut().answer(false, Instant::now());
            return;
        };
        let (alert, cancel) = quit_alert(self.mtm(), what);
        *self.life.quit_cancel.borrow_mut() = Some(cancel);
        let weak = Rc::downgrade(self);
        let handler = RcBlock::new(move |response: NSModalResponse| {
            if let Some(host) = weak.upgrade() {
                host.quit_answered(quit_chosen(response));
            }
        });
        alert.beginSheetModalForWindow_completionHandler(&window, Some(&handler));
    }

    /// Whether the quit question's sheet is up (it has the keys).
    pub fn quit_question_open(&self) -> bool {
        self.life.quit_cancel.borrow().is_some()
    }

    /// Escape while the quit question is open: Cancel, as a click on it. `false`: no question is open.
    pub fn cancel_quit_question(&self) -> bool {
        click_held_button(&self.life.quit_cancel)
    }

    fn quit_answered(&self, quit: bool) {
        self.life.quit_cancel.borrow_mut().take();
        let after = self.life.quit.borrow_mut().answer(quit, Instant::now());
        self.life
            .log_quit(format!("answered {}", if quit { "Quit" } else { "Cancel" }));
        match after {
            None => {}
            Some(QuitAfter::SendQuitKey) => {
                // The key Neovide's own Quit sends; nvim decides from there (its own prompt for an unsaved buffer).
                let sent = match self.assembly() {
                    Some(assembly) => assembly.editor.send_keys("<D-q>"),
                    None => Err("no editor".to_owned()),
                };
                if let Err(why) = sent {
                    self.life.log_quit(format!(
                        "the quit key did not reach nvim ({why}); closing the window instead"
                    ));
                    let _ = self.life.quit.borrow_mut().close_again(Instant::now());
                    self.close_window();
                }
            }
            Some(QuitAfter::CloseAgain) => self.close_window(),
        }
    }

    fn close_window(&self) {
        if let Some(window) = self.ns_window() {
            window.performClose(None);
        }
    }

    /// The shutdown, exactly once: the page is cut off, the timers and the panel's tick stop, the editor's link
    /// and sockets go, and the panel's own shutdown is waited for until nothing is held (sessions closed, the
    /// sidecar stopped, the saved tabs' last write on disk, or their backstops passed: `quit::wait_for_the_close`). `via` says which way out got here first.
    /// `nvim_gone`: Neovide's loop ended with its nvim, whose socket directory can go now.
    pub fn finish(&self, via: &'static str, nvim_gone: bool) {
        if self.life.finished.replace(true) {
            crate::log_line!(
                "[mac] shutdown already done ({:?}); {via}",
                self.life.finished_via.get()
            );
            return;
        }
        self.life.finished_via.set(Some(via));
        crate::log_line!("[mac] shutting down: {via}");
        self.panel_view().disconnect();
        self.teardown();
        let Some(assembly) = self.assembly() else { return };
        assembly.editor.shutdown();
        if nvim_gone {
            remove_socket_dir(assembly.editor.nvim_listen());
        }
        let took = crate::quit::wait_for_the_close(assembly.panel.shutdown());
        crate::log_line!("[mac] shutdown done in {} ms ({via})", took.as_millis());
    }
}

/// The quit question for `what` is at work: Cancel first, the default button, answering Return (AppKit gives a
/// button titled Cancel Escape in place of the first button's Return, which left Return answering nothing; the key
/// monitor turns Escape into a click on it, `click_held_button`), then Quit, with no key of its own.
pub fn quit_alert(mtm: MainThreadMarker, what: &str) -> (Retained<NSAlert>, Retained<NSButton>) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Quit Eitri while an agent is at work?"));
    alert.setInformativeText(&NSString::from_str(&format!(
        "{what}. Quitting ends the running turns and drops the queued messages."
    )));
    let cancel = alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    cancel.setKeyEquivalent(&NSString::from_str("\r"));
    alert.addButtonWithTitle(&NSString::from_str("Quit"));
    (alert, cancel)
}

/// Whether the question's answer is Quit: anything but its first button, Cancel.
pub fn quit_chosen(response: NSModalResponse) -> bool {
    response != NSAlertFirstButtonReturn
}

/// Clicks the button held in `slot`, if any. Cloned out first: the click runs the answer, which takes the button
/// out of the slot and must find no borrow held.
pub fn click_held_button(slot: &RefCell<Option<Retained<NSButton>>>) -> bool {
    let button = slot.borrow().clone();
    match button {
        Some(button) => {
            // SAFETY: a click on a live button of ours, on the main thread, with no sender.
            unsafe { button.performClick(None) };
            true
        }
        None => false,
    }
}
