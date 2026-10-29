//! The GTK-side half of companion focus: it turns `eitri_core::compositor`'s argv lists into commands run
//! on a worker thread. Nothing here waits for a command on the GTK thread: a key that moves focus
//! is claimed at once and the worker finishes the move (or finds nothing to move to) afterwards.
//!
//! The commands themselves are injected (`Exec`), so the logic below runs in tests without a
//! compositor and without starting a process.
//!
//! Hyprland takes its dispatches in the form its config manager reads, legacy or Lua. The first dispatch a
//! runner sends probes which one that is, on the worker, and the answer is kept for the runner (the main
//! window and the companion window each have a runner, so each probes once); a failure whose reply names the
//! other form probes once more (a user who switched config files mid-session). Every Hyprland dispatch's reply
//! is read: anything but `ok`, for each part of a batch, is a failure, logged once per kind.
//!
//! GNOME is the one adapter with no command to run: a client there cannot list or focus windows, so
//! its Shell extension does, and the calls to it go through `gnome_shell` (async gio calls, no
//! worker thread, no child). Behind the `GnomeCalls` trait the routing below is tested with a fake.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use eitri_core::compositor::{self, HyprForm, WindowRef, Wm};
use eitri_core::layout::Direction;
use eitri_core::proc_chain;

use super::gnome_shell::{GnomeCalls, GnomeShell};

/// Runs one argv and returns what it printed. `Err` means the program could not be run at all.
type Exec = Arc<dyn Fn(Vec<String>) -> Result<String, String> + Send + Sync>;
/// `/proc/<pid>/stat` text, or nothing.
type ReadStat = Arc<dyn Fn(u32) -> Option<String> + Send + Sync>;

/// How many ancestors of the editor's process are looked at for a window owner.
const CHAIN_MAX: usize = 32;

pub(crate) struct WmRunner {
    wm: Wm,
    notice_shown: Cell<bool>,
    /// The commands, shared with the worker threads that run them.
    commands: Commands,
    read_stat: ReadStat,
    /// The Eitri extension's service, on GNOME only.
    gnome: Option<Rc<dyn GnomeCalls>>,
    /// What the extension was last told about its partner.
    partner: Rc<Partner>,
}

/// The partner the extension was last asked to hold. Shared with the callbacks of calls that are
/// still waiting for an answer.
#[derive(Default)]
struct Partner {
    /// The chain last sent. Not empty means the extension has something a detach must clear, and is
    /// what is sent again to an extension that was disabled and enabled in between (a screen lock
    /// does that, and the enabled one starts with no partner).
    chain: RefCell<Vec<u32>>,
    /// Counts every change of partner. A call that was started under one partner and finishes under
    /// another must not put its own back.
    generation: Cell<u64>,
}

impl Partner {
    /// The partner is now `chain`; returns the generation of that change.
    fn change_to(&self, chain: &[u32]) -> u64 {
        *self.chain.borrow_mut() = chain.to_vec();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        generation
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.get() == generation
    }
}

impl WmRunner {
    pub(crate) fn new(wm: Wm) -> Rc<Self> {
        println!("[companion] window manager adapter: {}", wm_name(wm));
        let gnome = (wm == Wm::Gnome).then(|| {
            let shell = GnomeShell::new();
            shell.probe();
            shell as Rc<dyn GnomeCalls>
        });
        let runner = Rc::new(Self::with_adapters(
            wm,
            real_exec(wm),
            Arc::new(proc_chain::proc_stat),
            gnome,
        ));
        runner.resend_partner_when_the_extension_returns();
        runner
    }

    /// An extension that was disabled and enabled again (a screen lock) has no partner. It is told
    /// the one this panel last asked for, if there is one.
    fn resend_partner_when_the_extension_returns(self: &Rc<Self>) {
        let Some(gnome) = &self.gnome else { return };
        let runner = Rc::downgrade(self);
        gnome.on_appeared(Box::new(move || {
            if let Some(runner) = runner.upgrade() {
                runner.resend_partner();
            }
        }));
    }

    fn resend_partner(&self) {
        let chain = self.partner.chain.borrow().clone();
        if chain.is_empty() {
            return;
        }
        if let Some(gnome) = self.gnome() {
            println!("[companion] telling the GNOME Shell extension its partner again");
            gnome.set_partner(chain, Box::new(|_| {}));
        }
    }

    #[cfg(test)]
    fn with_commands(wm: Wm, exec: Exec, read_stat: ReadStat) -> Self {
        Self::with_adapters(wm, exec, read_stat, None)
    }

    fn with_adapters(wm: Wm, exec: Exec, read_stat: ReadStat, gnome: Option<Rc<dyn GnomeCalls>>) -> Self {
        WmRunner {
            wm,
            notice_shown: Cell::new(false),
            commands: Commands::new(wm, exec),
            read_stat,
            gnome,
            partner: Rc::default(),
        }
    }

    /// The extension's service when this is GNOME and it is not known to be missing. While the
    /// first answer is outstanding it is assumed to be there, so a key typed in that moment is not
    /// lost to a probe that is still in flight.
    fn gnome(&self) -> Option<&Rc<dyn GnomeCalls>> {
        self.gnome.as_ref().filter(|gnome| gnome.present() != Some(false))
    }

    /// The notice for a window manager that cannot move focus right now, or `None` when one can.
    /// GNOME without its extension says how to get it.
    fn no_adapter_notice(&self) -> Option<&'static str> {
        match self.wm {
            Wm::None => Some(compositor::NO_ADAPTER_NOTICE),
            Wm::Gnome if self.gnome().is_none() => Some(compositor::GNOME_NO_EXTENSION_NOTICE),
            _ => None,
        }
    }

    fn notice_once(&self, text: &'static str, on_none: &dyn Fn(&str)) {
        if !self.notice_shown.replace(true) {
            on_none(text);
        }
    }

    /// `Ctrl+h/j/k/l` out of the panel, the prefix's Select, and an edge letter from nvim. `false`
    /// with no adapter: the key then goes on to the page, and `on_none` says why, once. With an
    /// adapter the key is claimed at once and a worker does the rest. On sway it first asks
    /// whether any window lies that way, because sway wraps around to the far side; at an edge the
    /// key is therefore consumed with nothing moved, as tmux consumes `select-pane` there (the
    /// GTK side has to claim or release the key before the worker can answer, and never waits).
    pub(crate) fn move_focus(&self, dir: Direction, on_none: &dyn Fn(&str)) -> bool {
        if let Some(text) = self.no_adapter_notice() {
            self.notice_once(text, on_none);
            return false;
        }
        if let Some(gnome) = self.gnome() {
            // The extension moves focus only when the focused window is this panel's and has just
            // had a key; a `false` answer is the extension finding nothing that way, as at sway's
            // edge, and the key is consumed either way.
            gnome.focus_direction(dir, Box::new(|_| {}));
            return true;
        }
        let commands = self.commands.clone();
        thread::spawn(move || commands.move_focus_blocking(dir, &|line| println!("{line}")));
        true
    }

    /// An edge letter from nvim's own navigator: the cursor was at nvim's last window and the user
    /// pressed on toward the panel's neighbour. Everywhere but GNOME this is the same move as the
    /// panel's own key. On GNOME the extension is asked to move focus to the panel if the panel lies
    /// that way from the editor's window, because the key went to the editor and the focused window
    /// is therefore the editor's. Returns whether the key was claimed.
    pub(crate) fn editor_edge(&self, dir: Direction, on_none: &dyn Fn(&str)) -> bool {
        if self.wm != Wm::Gnome {
            return self.move_focus(dir, on_none);
        }
        if let Some(text) = self.no_adapter_notice() {
            self.notice_once(text, on_none);
            return false;
        }
        if let Some(gnome) = self.gnome() {
            gnome.focus_self_if_neighbour(dir, Box::new(|_| {}));
        }
        true
    }

    /// Brings the window owned by this process (`std::process::id()`) or by the editor forward.
    /// On GNOME the extension identifies the caller by its own pid, so only this process's own
    /// window can be asked for, by `ActivateOwn`.
    pub(crate) fn raise_pid(&self, pid: u32) {
        if self.wm == Wm::Gnome {
            if pid == std::process::id() {
                if let Some(gnome) = self.gnome() {
                    gnome.activate_own(Box::new(|_| {}));
                }
            }
            return;
        }
        self.raise(move |_| vec![pid]);
    }

    /// The editor's window: the first window owned by `nvim_pid` or one of its ancestors (a nvim
    /// run in a terminal has the terminal emulator as an ancestor). On GNOME the extension keeps
    /// the editor window as this panel's partner (`set_partner_from`) and raises that one.
    pub(crate) fn raise_editor(&self, nvim_pid: u32) {
        if self.wm == Wm::Gnome {
            if let Some(gnome) = self.gnome() {
                gnome.activate_partner(Box::new(|_| {}));
            }
            return;
        }
        self.raise(move |read_stat| proc_chain::ppid_chain(nvim_pid, read_stat, CHAIN_MAX));
    }

    /// Tells the GNOME extension which window is the editor's, by nvim's own pid and its
    /// ancestors (the extension picks the first that owns a window, which a client cannot do
    /// itself there), or clears it with `None` once the panel has no editor. Nothing elsewhere.
    pub(crate) fn set_partner_from(&self, nvim_pid: Option<u32>) {
        if self.gnome.is_none() {
            return;
        }
        let chain = match nvim_pid {
            Some(pid) => proc_chain::ppid_chain(pid, &*self.read_stat, CHAIN_MAX),
            None => Vec::new(),
        };
        if chain.is_empty() && self.partner.chain.borrow().is_empty() {
            return;
        }
        // Remembered whether or not the extension can be called now: one that is off may be enabled
        // later, and is then told what the panel's partner is.
        self.partner.change_to(&chain);
        if let Some(gnome) = self.gnome() {
            gnome.set_partner(chain, Box::new(|_| {}));
        }
    }

    /// What a request that brought this panel forward does. Elsewhere that is the window manager's
    /// own raise at once. On GNOME the panel first names the sender's window as its partner
    /// (`chain`, when the request was an attach) and brings itself forward only once that is
    /// recorded, since the extension raises the panel only when the focused window is its partner.
    ///
    /// The raise waits for the extension to say the partner was recorded: when the call fails, the
    /// extension may still hold the earlier partner, and a raise then would rest on that window's
    /// input instead of the sender's.
    ///
    /// `editor_pid` is nvim's pid when the request was for the editor this panel is already
    /// attached to. The link does not change then, so nothing else would put the editor back as
    /// the partner after the raise; without it the window that sent the request -- another terminal
    /// running `eitri panel --nvim` with the same address -- would stay the partner, and handing
    /// focus back to the editor would go to that terminal instead.
    pub(crate) fn raise_for_request(&self, chain: Option<Vec<u32>>, editor_pid: Option<u32>) {
        if self.wm != Wm::Gnome {
            self.raise_pid(std::process::id());
            return;
        }
        if self.gnome.is_none() {
            return;
        }
        // The partner is remembered even when the extension cannot be called now (see
        // `set_partner_from`); only the calls are skipped.
        let generation = chain.as_deref().map(|chain| self.partner.change_to(chain));
        let Some(gnome) = self.gnome().cloned() else {
            // No raise can happen now, so remember the partner the raise would have left behind: the editor
            // already attached when the request named it (the link does not change then, so nothing else would
            // put it back), else the sender.
            if chain.is_some() {
                if let Some(pid) = editor_pid {
                    let editor_chain = proc_chain::ppid_chain(pid, &*self.read_stat, CHAIN_MAX);
                    if !editor_chain.is_empty() {
                        self.partner.change_to(&editor_chain);
                    }
                }
            }
            return;
        };
        match chain {
            Some(chain) => {
                let generation = generation.expect("counted for an attach");
                let editor_chain = editor_pid.map(|pid| proc_chain::ppid_chain(pid, &*self.read_stat, CHAIN_MAX));
                let (after, partner) = (gnome.clone(), self.partner.clone());
                gnome.set_partner(
                    chain,
                    Box::new(move |recorded| {
                        if recorded != Some(true) {
                            println!("[companion] the partner was not recorded; not raising the panel");
                            return;
                        }
                        let restore = after.clone();
                        after.activate_own(Box::new(move |_| {
                            // Another editor may have become the partner while the raise was
                            // pending; giving this one back then would undo that.
                            if !partner.is_current(generation) {
                                return;
                            }
                            if let Some(editor_chain) = editor_chain.filter(|chain| !chain.is_empty()) {
                                partner.change_to(&editor_chain);
                                restore.set_partner(editor_chain, Box::new(|_| {}));
                            }
                        }));
                    }),
                );
            }
            None => gnome.activate_own(Box::new(|_| {})),
        }
    }

    fn raise(&self, chain: impl FnOnce(&dyn Fn(u32) -> Option<String>) -> Vec<u32> + Send + 'static) {
        if matches!(self.wm, Wm::None | Wm::Gnome) {
            return;
        }
        let (commands, read_stat) = (self.commands.clone(), self.read_stat.clone());
        thread::spawn(move || {
            let chain = chain(&*read_stat);
            commands.raise_blocking(&chain, &|line| println!("{line}"));
        });
    }
}

fn wm_name(wm: Wm) -> &'static str {
    match wm {
        Wm::Hyprland => "hyprland",
        Wm::Sway => "sway",
        Wm::Niri => "niri",
        Wm::Gnome => "gnome",
        Wm::None => "none",
    }
}

/// The real commands. A program that cannot be started is logged once per window: a compositor's
/// tool that is missing is missing for every key, and a line per key press would bury the log.
fn real_exec(wm: Wm) -> Exec {
    let logged = Arc::new(AtomicBool::new(false));
    let exits: Arc<Mutex<HashSet<(String, String)>>> = Arc::default();
    Arc::new(move |argv: Vec<String>| {
        let (program, args) = argv.split_first().ok_or_else(|| "empty command".to_string())?;
        match Command::new(program).args(args).output() {
            Ok(output) => {
                if let Some(line) = exit_note(wm, &exits, program, &output) {
                    println!("{line}");
                }
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Err(err) => {
                if !logged.swap(true, Ordering::Relaxed) {
                    println!("[companion] {} adapter: could not run {program}: {err}", wm_name(wm));
                }
                Err(err.to_string())
            }
        }
    })
}

/// The log line for a command that exited non-zero, or `None` when it needs none. `hyprctl` exits non-zero
/// when a reply starts with `error:` and has already put that reply on stdout, where the caller reads and logs
/// it once per kind, so a Hyprland exit with output is not noted here. Any other non-zero exit is noted once
/// per program and status: a failing tool fails the same way on every key press.
fn exit_note(
    wm: Wm,
    seen: &Mutex<HashSet<(String, String)>>,
    program: &str,
    output: &std::process::Output,
) -> Option<String> {
    if output.status.success() || (wm == Wm::Hyprland && !output.stdout.trim_ascii().is_empty()) {
        return None;
    }
    let status = output.status.to_string();
    if !locked(seen).insert((program.to_string(), status.clone())) {
        return None;
    }
    Some(format!(
        "[companion] {} adapter: {program} exited with {status}: {}",
        wm_name(wm),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// What one runner's Hyprland adapter has learned: the dispatch form the running config manager takes
/// (`None` until the first dispatch needs it), whether the one probe after a failure was spent, and how often each
/// kind of failure was seen. Only worker threads touch it, so a probe never runs on the GTK thread.
#[derive(Default)]
struct HyprState {
    form: Mutex<Option<HyprForm>>,
    reprobed: AtomicBool,
    failures: Mutex<HashMap<String, u64>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Everything a worker thread needs to run window-manager commands: the adapter, the way to run a command, and
/// what this runner learned about Hyprland.
#[derive(Clone)]
struct Commands {
    wm: Wm,
    exec: Exec,
    hypr: Arc<HyprState>,
}

impl Commands {
    fn new(wm: Wm, exec: Exec) -> Commands {
        Commands {
            wm,
            exec,
            hypr: Arc::default(),
        }
    }

    /// The form Hyprland's dispatches are written in, probed on the first call. Other adapters have one form
    /// and nothing is run for them.
    fn form(&self, log: &dyn Fn(&str)) -> HyprForm {
        if self.wm != Wm::Hyprland {
            return HyprForm::Legacy;
        }
        // Held through the probe, so two first dispatches on two workers probe once. A worker waits here for as
        // long as the probe runs; no timeout wraps `hyprctl`, whose own 5 s receive timeout bounds a compositor
        // that does not answer.
        let mut form = locked(&self.hypr.form);
        *form.get_or_insert_with(|| self.probe(log))
    }

    /// `hyprctl eval 'return 1'`: `ok` means the Lua config manager; any other answer, or a `hyprctl` that cannot
    /// run, means the legacy dispatchers. One log line says which and why.
    fn probe(&self, log: &dyn Fn(&str)) -> HyprForm {
        match (self.exec)(compositor::hyprland_probe_argv()) {
            Ok(reply) => {
                let form = HyprForm::from_probe_reply(&reply);
                log(&format!(
                    "[companion] hyprland adapter: {} dispatch (the config-manager probe answered {:?})",
                    form.name(),
                    compositor::first_line(&reply)
                ));
                form
            }
            Err(err) => {
                log(&format!(
                    "[companion] hyprland adapter: the config-manager probe could not run ({err}); using the legacy dispatch"
                ));
                HyprForm::Legacy
            }
        }
    }

    /// Runs the command `build` gives for the current form and reads its reply. A Hyprland failure whose reply
    /// names the other form probes again, once per runner; when that finds the other form, the command is sent
    /// once more in it, since a failure of that kind ran nothing. `false` when there is no command to run.
    fn dispatch(&self, operation: &str, build: &dyn Fn(HyprForm) -> Option<Vec<String>>, log: &dyn Fn(&str)) -> bool {
        let form = self.form(log);
        let Some(argv) = build(form) else {
            return false;
        };
        let Some(failure) = self.run(operation, argv, log) else {
            return true;
        };
        if self.wm != Wm::Hyprland
            || !compositor::names_other_form(&failure)
            || self.hypr.reprobed.swap(true, Ordering::SeqCst)
        {
            return true;
        }
        let now = {
            let mut current = locked(&self.hypr.form);
            let now = self.probe(log);
            *current = Some(now);
            now
        };
        if now != form {
            if let Some(argv) = build(now) {
                self.run(operation, argv, log);
            }
        }
        true
    }

    /// Runs one command; the failing reply when the compositor said it failed. A command that could not be run
    /// at all was logged by `exec` and is no reply to judge.
    fn run(&self, operation: &str, argv: Vec<String>, log: &dyn Fn(&str)) -> Option<String> {
        let reply = (self.exec)(argv.clone()).ok()?;
        let failure = compositor::dispatch_reply_failure(self.wm, &argv, &reply)?;
        self.note_failure(operation, &failure, log);
        Some(failure)
    }

    /// The first failure of a kind (the operation and the reply's first line) is logged; later ones are counted.
    fn note_failure(&self, operation: &str, reply: &str, log: &dyn Fn(&str)) {
        let kind = compositor::failure_kind(operation, reply);
        let mut failures = locked(&self.hypr.failures);
        let seen = failures.entry(kind).or_insert(0);
        *seen += 1;
        if *seen == 1 {
            log(&format!(
                "[companion] {} adapter: {operation} failed: {} (later failures like it are counted, not logged)",
                wm_name(self.wm),
                compositor::first_line(reply)
            ));
        }
    }

    #[cfg(test)]
    fn failure_counts(&self) -> HashMap<String, u64> {
        locked(&self.hypr.failures).clone()
    }

    fn move_focus_blocking(&self, dir: Direction, log: &dyn Fn(&str)) {
        let wm = self.wm;
        if wm == Wm::Sway {
            let windows = compositor::list_windows_argv(wm)
                .and_then(|argv| (self.exec)(argv).ok())
                .and_then(|json| compositor::parse_windows(wm, &json).ok());
            match windows
                .as_deref()
                .and_then(|windows| compositor::has_neighbour(windows, dir))
            {
                Some(false) => {
                    log(&format!(
                        "[companion] sway: no window {dir:?} of the panel; the key stops here"
                    ));
                    return;
                }
                // Not knowing is not a reason to eat the key: sway's own answer is the better guess.
                None => log("[companion] sway: could not read the window tree; moving focus anyway"),
                Some(true) => {}
            }
        }
        self.dispatch(
            "focus by direction",
            &|form| compositor::move_focus_argv(wm, form, dir),
            log,
        );
    }

    /// Focuses the first window owned by one of `chain`'s processes, nearest first. With none found
    /// nothing is run and one line names the chain: inside tmux it ends at the tmux server, which owns
    /// no window.
    fn raise_blocking(&self, chain: &[u32], log: &dyn Fn(&str)) {
        let wm = self.wm;
        let Some(list) = compositor::list_windows_argv(wm) else {
            return;
        };
        let Ok(json) = (self.exec)(list) else { return };
        let windows: Vec<WindowRef> = match compositor::parse_windows(wm, &json) {
            Ok(windows) => windows,
            Err(err) => {
                log(&format!("[companion] {} adapter: {err}", wm_name(wm)));
                return;
            }
        };
        let Some(target) = compositor::first_window_owner(chain, &windows).copied() else {
            log(&format!(
                "[companion] raise: no window owned by any process in {chain:?}; nothing raised"
            ));
            return;
        };
        if !self.dispatch(
            "focus by pid",
            &|form| compositor::focus_window_argv(wm, form, &target),
            log,
        ) {
            log(&format!(
                "[companion] raise: {} cannot focus pid {} without a window id",
                wm_name(wm),
                target.pid
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    fn fake(answers: &'static str) -> (Exec, Arc<Mutex<Vec<Vec<String>>>>) {
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let record = seen.clone();
        let exec: Exec = Arc::new(move |argv| {
            record.lock().unwrap().push(argv);
            Ok(answers.to_string())
        });
        (exec, seen)
    }

    fn sway_tree(panel_x: i64, other_x: i64) -> &'static str {
        let json = format!(
            r#"{{"type":"root","nodes":[
                {{"type":"con","visible":true,"focused":true,"pid":100,"id":1,
                  "rect":{{"x":{panel_x},"y":0,"width":500,"height":800}}}},
                {{"type":"con","visible":true,"focused":false,"pid":200,"id":2,
                  "rect":{{"x":{other_x},"y":0,"width":500,"height":800}}}}]}}"#
        );
        Box::leak(json.into_boxed_str())
    }

    #[test]
    fn none_shows_its_notice_once_and_never_claims_the_key() {
        let runner = WmRunner::with_commands(Wm::None, fake("").0, Arc::new(|_| None));
        let notices = RefCell::new(Vec::new());
        for dir in [Direction::Left, Direction::Right, Direction::Left] {
            assert!(!runner.move_focus(dir, &|text| notices.borrow_mut().push(text.to_string())));
        }
        assert_eq!(*notices.borrow(), vec![compositor::NO_ADAPTER_NOTICE.to_string()]);
    }

    #[test]
    fn a_chain_that_reaches_no_window_raises_nothing() {
        // 300's parent is 250, whose parent is 1; no window belongs to either.
        let stat = |pid: u32| match pid {
            300 => Some("300 (nvim) S 250 0 0".to_string()),
            250 => Some("250 (zsh) S 1 0 0".to_string()),
            _ => None,
        };
        let chain = proc_chain::ppid_chain(300, &stat, CHAIN_MAX);
        assert_eq!(chain, vec![300, 250]);
        let (exec, seen) = fake(sway_tree(0, 500));
        let lines = RefCell::new(Vec::new());
        Commands::new(Wm::Sway, exec).raise_blocking(&chain, &|line| lines.borrow_mut().push(line.to_string()));
        // Only the window list was asked for: no focus command ran.
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(seen.lock().unwrap()[0], vec!["swaymsg", "-t", "get_tree"]);
        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0].contains("[300, 250]"), "{:?}", lines.borrow());
    }

    #[test]
    fn a_chain_that_reaches_a_window_focuses_it() {
        let (exec, seen) = fake(sway_tree(0, 500));
        Commands::new(Wm::Sway, exec).raise_blocking(&[300, 200], &|_| {});
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], vec!["swaymsg".to_string(), "[pid=200] focus".to_string()]);
    }

    #[test]
    fn sway_stops_at_an_edge_and_moves_when_a_window_lies_that_way() {
        // The panel (focused) is on the left, the other window on the right.
        let (exec, seen) = fake(sway_tree(0, 500));
        Commands::new(Wm::Sway, exec).move_focus_blocking(Direction::Left, &|_| {});
        assert_eq!(seen.lock().unwrap().len(), 1, "an edge runs only the tree query");
        let (exec, seen) = fake(sway_tree(0, 500));
        Commands::new(Wm::Sway, exec).move_focus_blocking(Direction::Right, &|_| {});
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], vec!["swaymsg", "focus", "right"]);
    }

    #[test]
    fn hyprland_moves_without_asking() {
        let (exec, seen) = hyprland(|_| LEGACY_PROBE, |_| "ok");
        Commands::new(Wm::Hyprland, exec).move_focus_blocking(Direction::Up, &|_| {});
        assert_eq!(
            *seen.lock().unwrap(),
            vec![probe(), strings(&["hyprctl", "dispatch", "movefocus", "u"])]
        );
    }

    const LEGACY_PROBE: &str = "eval is only supported with the lua config manager";
    /// What the Lua manager answers to legacy text: hyprctl 0.57 starts a Lua error reply with `error: <code> ` (3
    /// is the syntax error code), which is also why it exits non-zero. Read from its source, not run.
    const LUA_UPDATE_NOTE: &str =
        "error: 3 [string \"return hl.dispatch(movefocus l)\"]:1: ')' expected near 'l'\n\n → \
        Note: dispatch in lua is a shorthand for hl.dispatch(...), your syntax might need to be updated.";

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    fn probe() -> Vec<String> {
        strings(&["hyprctl", "eval", "return 1"])
    }

    /// A fake `hyprctl`: `probe_reply` answers the n-th probe (from 0), `dispatch_reply` every other command.
    fn hyprland(
        probe_reply: impl Fn(usize) -> &'static str + Send + Sync + 'static,
        dispatch_reply: impl Fn(&[String]) -> &'static str + Send + Sync + 'static,
    ) -> (Exec, Arc<Mutex<Vec<Vec<String>>>>) {
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let record = seen.clone();
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let exec: Exec = Arc::new(move |argv: Vec<String>| {
            record.lock().unwrap().push(argv.clone());
            if argv == probe() {
                return Ok(probe_reply(probes.fetch_add(1, Ordering::SeqCst)).to_string());
            }
            Ok(dispatch_reply(&argv).to_string())
        });
        (exec, seen)
    }

    fn logged() -> (Rc<RefCell<Vec<String>>>, impl Fn(&str)) {
        let lines: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = lines.clone();
        (lines, move |line: &str| sink.borrow_mut().push(line.to_string()))
    }

    #[test]
    fn a_probe_answering_ok_gives_the_lua_form_and_is_not_repeated() {
        let (exec, seen) = hyprland(|_| "ok\n", |_| "ok\n");
        let commands = Commands::new(Wm::Hyprland, exec);
        let (lines, log) = logged();
        commands.move_focus_blocking(Direction::Left, &log);
        commands.move_focus_blocking(Direction::Down, &log);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                probe(),
                strings(&["hyprctl", "dispatch", "hl.dsp.focus({ direction = 'l' })"]),
                strings(&["hyprctl", "dispatch", "hl.dsp.focus({ direction = 'd' })"]),
            ]
        );
        assert_eq!(lines.borrow().len(), 1, "{:?}", lines.borrow());
        assert!(lines.borrow()[0].contains("lua dispatch"), "{:?}", lines.borrow());
        assert!(commands.failure_counts().is_empty());
    }

    #[test]
    fn a_legacy_manager_or_a_hyprland_without_eval_gives_the_legacy_form() {
        for reply in [LEGACY_PROBE, "unknown request", ""] {
            let (exec, seen) = hyprland(move |_| reply, |_| "ok");
            let commands = Commands::new(Wm::Hyprland, exec);
            let (lines, log) = logged();
            commands.move_focus_blocking(Direction::Right, &log);
            assert_eq!(
                *seen.lock().unwrap(),
                vec![probe(), strings(&["hyprctl", "dispatch", "movefocus", "r"])],
                "{reply:?}"
            );
            assert_eq!(lines.borrow().len(), 1, "{:?}", lines.borrow());
            assert!(lines.borrow()[0].contains("legacy dispatch"), "{:?}", lines.borrow());
        }
    }

    /// A Lua dispatch that fails for each window it is asked about, with a reply that echoes the pid: one log line,
    /// every later failure counted under the same kind.
    #[test]
    fn a_repeated_failing_lua_dispatch_logs_once() {
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let record = seen.clone();
        let exec: Exec = Arc::new(move |argv: Vec<String>| {
            record.lock().unwrap().push(argv.clone());
            if argv == probe() {
                return Ok("ok\n".to_string());
            }
            let pid: String = argv[2].chars().filter(char::is_ascii_digit).collect();
            Ok(format!(
                "error: 3 [string \"return hl.dispatch(hl.dsp.focus({{ window = 'pid:{pid}' }}))\"]:1: no window pid:{pid}\n"
            ))
        });
        let commands = Commands::new(Wm::Hyprland, exec);
        let (lines, log) = logged();
        for pid in [11, 12, 13, 12, 11, 4000] {
            let target = WindowRef {
                pid,
                id: None,
                focused: false,
                visible: true,
                rect: None,
            };
            commands.dispatch(
                "focus by pid",
                &|form| compositor::focus_window_argv(Wm::Hyprland, form, &target),
                &log,
            );
        }
        let failed: Vec<String> = lines
            .borrow()
            .iter()
            .filter(|line| line.contains("failed"))
            .cloned()
            .collect();
        assert_eq!(failed.len(), 1, "{:?}", lines.borrow());
        assert!(failed[0].contains("error: 3"), "{failed:?}");
        assert_eq!(commands.failure_counts().values().copied().collect::<Vec<_>>(), vec![6]);
        // The reply named no form, so nothing probed again.
        assert_eq!(seen.lock().unwrap().iter().filter(|argv| **argv == probe()).count(), 1);
    }

    /// Two workers making the first dispatch at the same moment probe once between them.
    #[test]
    fn two_first_dispatches_at_once_probe_once() {
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = probes.clone();
        let exec: Exec = Arc::new(move |argv: Vec<String>| {
            if argv == probe() {
                count.fetch_add(1, Ordering::SeqCst);
                // Long enough that the other worker reaches the form while this probe is running.
                thread::sleep(Duration::from_millis(100));
            }
            Ok("ok".to_string())
        });
        let commands = Commands::new(Wm::Hyprland, exec);
        let start = Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = [Direction::Left, Direction::Right]
            .into_iter()
            .map(|dir| {
                let (commands, start) = (commands.clone(), start.clone());
                thread::spawn(move || {
                    start.wait();
                    commands.move_focus_blocking(dir, &|_| {});
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    fn exit_output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// `hyprctl` exits 7 on an `error:` reply and has put the reply on stdout: the reply's reader logs that, so
    /// the exit is not logged per call. An exit with nothing on stdout is logged once per program and status.
    #[test]
    fn a_failing_exit_is_logged_once_and_never_for_a_reply_that_is_read() {
        let seen: Mutex<HashSet<(String, String)>> = Mutex::default();
        let reply = exit_output(7, "error: 3 something\n", "");
        for _ in 0..5 {
            assert_eq!(exit_note(Wm::Hyprland, &seen, "hyprctl", &reply), None);
        }
        assert!(exit_note(Wm::Hyprland, &seen, "hyprctl", &exit_output(0, "", "")).is_none());
        let silent = exit_output(1, "", "no socket\n");
        let first = exit_note(Wm::Hyprland, &seen, "hyprctl", &silent).expect("the first is logged");
        assert!(
            first.contains("hyprctl exited with") && first.ends_with("no socket"),
            "{first}"
        );
        for _ in 0..5 {
            assert_eq!(exit_note(Wm::Hyprland, &seen, "hyprctl", &silent), None);
        }
        // Another status, or another program, is another kind.
        assert!(exit_note(Wm::Hyprland, &seen, "hyprctl", &exit_output(2, "", "")).is_some());
        assert!(exit_note(Wm::Sway, &seen, "swaymsg", &silent).is_some());
        // Only Hyprland's replies are read by the caller; sway's exit is logged even with output.
        assert!(exit_note(Wm::Sway, &seen, "swaymsg", &exit_output(3, "[]", "")).is_some());
    }

    #[test]
    fn a_probe_that_cannot_run_gives_the_legacy_form_and_one_log_line() {
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let record = seen.clone();
        let exec: Exec = Arc::new(move |argv: Vec<String>| {
            record.lock().unwrap().push(argv.clone());
            if argv == probe() {
                Err("No such file or directory (os error 2)".to_string())
            } else {
                Ok("ok".to_string())
            }
        });
        let commands = Commands::new(Wm::Hyprland, exec);
        let (lines, log) = logged();
        commands.move_focus_blocking(Direction::Left, &log);
        commands.move_focus_blocking(Direction::Up, &log);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                probe(),
                strings(&["hyprctl", "dispatch", "movefocus", "l"]),
                strings(&["hyprctl", "dispatch", "movefocus", "u"]),
            ]
        );
        assert_eq!(lines.borrow().len(), 1, "{:?}", lines.borrow());
        assert!(lines.borrow()[0].contains("could not run"), "{:?}", lines.borrow());
    }

    /// Each failure that names the other form is logged once for its kind, counted after that, and probes once
    /// more; the one probe after a failure is spent for the runner.
    #[test]
    fn a_failure_naming_the_other_form_is_logged_once_and_probes_once_more() {
        let cases: [(&str, &'static str, &'static str); 3] = [
            ("lua", "ok", "Invalid dispatcher"),
            ("legacy", LEGACY_PROBE, LUA_UPDATE_NOTE),
            (
                "legacy",
                LEGACY_PROBE,
                "current config provider doesn't support dispatch",
            ),
        ];
        for (name, probe_reply, failure) in cases {
            let (exec, seen) = hyprland(move |_| probe_reply, move |_| failure);
            let commands = Commands::new(Wm::Hyprland, exec);
            let (lines, log) = logged();
            for _ in 0..3 {
                commands.move_focus_blocking(Direction::Left, &log);
            }
            let seen = seen.lock().unwrap();
            assert_eq!(
                seen.iter().filter(|argv| **argv == probe()).count(),
                2,
                "{name}: the first probe and one more: {seen:?}"
            );
            // The form did not change, so nothing is sent again: three dispatches in all.
            assert_eq!(seen.len(), 5, "{seen:?}");
            let failed = lines.borrow().iter().filter(|line| line.contains("failed")).count();
            assert_eq!(failed, 1, "{:?}", lines.borrow());
            assert_eq!(
                commands
                    .failure_counts()
                    .get(&compositor::failure_kind("focus by direction", failure)),
                Some(&3)
            );
            // Two probe lines and one failure line, whatever the reply's later lines held.
            assert_eq!(lines.borrow().len(), 3, "{:?}", lines.borrow());
            assert!(lines.borrow().iter().all(|line| !line.contains('\n')));
        }
    }

    /// A user who switched from `hyprland.conf` to `hyprland.lua` mid-session: the legacy command fails with the
    /// update note, the probe now answers `ok`, and the move is sent once more in the Lua form.
    #[test]
    fn a_probe_that_finds_the_other_form_sends_the_command_again_in_it() {
        let (exec, seen) = hyprland(
            |n| if n == 0 { LEGACY_PROBE } else { "ok" },
            |argv| {
                if argv[2].starts_with("hl.dsp") {
                    "ok"
                } else {
                    LUA_UPDATE_NOTE
                }
            },
        );
        let commands = Commands::new(Wm::Hyprland, exec);
        let (lines, log) = logged();
        commands.move_focus_blocking(Direction::Left, &log);
        commands.move_focus_blocking(Direction::Right, &log);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                probe(),
                strings(&["hyprctl", "dispatch", "movefocus", "l"]),
                probe(),
                strings(&["hyprctl", "dispatch", "hl.dsp.focus({ direction = 'l' })"]),
                strings(&["hyprctl", "dispatch", "hl.dsp.focus({ direction = 'r' })"]),
            ]
        );
        assert_eq!(commands.failure_counts().values().sum::<u64>(), 1);
        assert_eq!(lines.borrow().len(), 3, "{:?}", lines.borrow());
    }

    #[test]
    fn a_failure_that_names_no_form_probes_nothing_more() {
        let (exec, seen) = hyprland(|_| "ok", |_| "No such window found");
        let commands = Commands::new(Wm::Hyprland, exec);
        let (lines, log) = logged();
        commands.move_focus_blocking(Direction::Left, &log);
        commands.move_focus_blocking(Direction::Left, &log);
        assert_eq!(seen.lock().unwrap().iter().filter(|argv| **argv == probe()).count(), 1);
        assert_eq!(
            lines.borrow().len(),
            2,
            "the probe and one failure: {:?}",
            lines.borrow()
        );
        // Another operation failing the same way is another kind.
        let (exec, _) = hyprland(
            |_| "ok",
            |argv| {
                if argv[0] == "hyprctl" && argv[1] == "clients" {
                    r#"[{"mapped":true,"pid":9,"focusHistoryID":1}]"#
                } else {
                    "No such window found"
                }
            },
        );
        let commands = Commands::new(Wm::Hyprland, exec);
        let (lines, log) = logged();
        commands.move_focus_blocking(Direction::Left, &log);
        commands.raise_blocking(&[9], &log);
        commands.raise_blocking(&[9], &log);
        assert_eq!(commands.failure_counts().len(), 2);
        assert_eq!(lines.borrow().iter().filter(|line| line.contains("failed")).count(), 2);
    }

    #[test]
    fn a_batch_succeeds_only_when_every_part_answers_ok() {
        let target = WindowRef {
            pid: 42,
            id: None,
            focused: false,
            visible: true,
            rect: None,
        };
        for (reply, fails) in [
            ("ok\n\n\nok", false),
            ("ok\n\n\nok\n", false),
            ("ok\n\n\nInvalid dispatcher", true),
            ("Invalid dispatcher\n\n\nok", true),
            ("ok", true),
        ] {
            let (exec, seen) = hyprland(|_| LEGACY_PROBE, move |_| reply);
            let commands = Commands::new(Wm::Hyprland, exec);
            let (lines, log) = logged();
            assert!(commands.dispatch(
                "move to output",
                &|form| compositor::move_to_output_argv(Wm::Hyprland, form, &target, "DP-1"),
                &log,
            ));
            assert_eq!(
                seen.lock().unwrap()[1],
                strings(&[
                    "hyprctl",
                    "--batch",
                    "dispatch focuswindow pid:42 ; dispatch movewindow mon:DP-1"
                ])
            );
            assert_eq!(!commands.failure_counts().is_empty(), fails, "{reply:?}");
            assert_eq!(
                lines.borrow().iter().any(|line| line.contains("failed")),
                fails,
                "{reply:?}"
            );
        }
        // The Lua move is one command.
        let (exec, seen) = hyprland(|_| "ok", |_| "ok");
        let commands = Commands::new(Wm::Hyprland, exec);
        commands.dispatch(
            "move to output",
            &|form| compositor::move_to_output_argv(Wm::Hyprland, form, &target, "DP-1"),
            &|_| {},
        );
        assert_eq!(
            seen.lock().unwrap()[1],
            strings(&[
                "hyprctl",
                "dispatch",
                "hl.dsp.window.move({ window = 'pid:42', monitor = 'DP-1' })"
            ])
        );
        assert!(commands.failure_counts().is_empty());
    }

    #[test]
    fn a_hyprland_raise_reads_the_window_list_then_probes_then_focuses_by_pid() {
        let (exec, seen) = hyprland(
            |_| "ok",
            |argv| {
                if argv[1] == "clients" {
                    r#"[{"mapped":true,"pid":300,"focusHistoryID":1},{"mapped":true,"pid":250,"focusHistoryID":0}]"#
                } else {
                    "ok"
                }
            },
        );
        let (lines, log) = logged();
        Commands::new(Wm::Hyprland, exec).raise_blocking(&[300, 250], &log);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                strings(&["hyprctl", "clients", "-j"]),
                probe(),
                strings(&["hyprctl", "dispatch", "hl.dsp.focus({ window = 'pid:300' })"]),
            ]
        );
        assert_eq!(lines.borrow().len(), 1, "{:?}", lines.borrow());
    }

    /// The runner's own methods run on the GTK thread. They reach Hyprland's probe and its dispatches only
    /// through the two worker functions, and only from inside a spawned thread.
    #[test]
    fn the_probe_and_every_dispatch_run_on_a_worker_thread() {
        use eitri_core::source_scan::{braced, code_only, modules, read_required, without};
        let text = read_required("shell/src/companion/wm_runner.rs").text;
        let code = code_only(&text);
        let code = without(&code, &modules(&code, "tests"));
        let at = code.find("impl WmRunner {").expect("the runner's impl");
        let runner = braced(&code, at + "impl WmRunner ".len());
        // Of its `Commands`, the runner only clones them and hands them to a worker.
        for (at, _) in runner.match_indices("commands.") {
            let method: String = runner[at + "commands.".len()..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            assert!(
                ["clone", "move_focus_blocking", "raise_blocking"].contains(&method.as_str()),
                "the runner calls `commands.{method}` on its own thread"
            );
        }
        let mut workers = 0;
        for name in [".move_focus_blocking(", ".raise_blocking("] {
            for (at, _) in runner.match_indices(name) {
                workers += 1;
                let before = &runner[at.saturating_sub(160)..at];
                assert!(
                    before.contains("thread::spawn(move ||"),
                    "`{name}` is called outside a spawned thread: {before}"
                );
            }
        }
        assert_eq!(workers, 2, "the scan saw the two worker calls");
    }

    /// sway and niri have one form and no reply rule: nothing is probed, nothing judged.
    #[test]
    fn other_compositors_are_never_probed() {
        let (exec, seen) = fake(sway_tree(0, 500));
        let (lines, log) = logged();
        let commands = Commands::new(Wm::Sway, exec);
        commands.move_focus_blocking(Direction::Right, &log);
        assert!(seen.lock().unwrap().iter().all(|argv| argv[0] == "swaymsg"));
        assert!(lines.borrow().is_empty(), "{:?}", lines.borrow());
        let (exec, seen) = fake("anything");
        Commands::new(Wm::Niri, exec).move_focus_blocking(Direction::Up, &log);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![strings(&["niri", "msg", "action", "focus-window-up"])]
        );
        assert!(lines.borrow().is_empty(), "{:?}", lines.borrow());
    }

    use crate::companion::gnome_shell::Done;

    /// Records what the runner asked of the extension. A `set_partner` is held until the test
    /// answers it, to see what waits for that answer.
    struct FakeGnome {
        present: Cell<Option<bool>>,
        calls: RefCell<Vec<String>>,
        set_partner_done: RefCell<Option<Done>>,
        activate_own_done: RefCell<Option<Done>>,
        appeared: RefCell<Option<Box<dyn Fn()>>>,
    }

    impl FakeGnome {
        fn new(present: Option<bool>) -> Rc<Self> {
            Rc::new(FakeGnome {
                present: Cell::new(present),
                calls: RefCell::default(),
                set_partner_done: RefCell::default(),
                activate_own_done: RefCell::default(),
                appeared: RefCell::default(),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn answer_set_partner(&self) {
            self.answer_set_partner_with(Some(true));
        }

        fn answer_set_partner_with(&self, reply: Option<bool>) {
            let done = self
                .set_partner_done
                .borrow_mut()
                .take()
                .expect("a set_partner is waiting");
            done(reply);
        }

        /// The extension's name got an owner.
        fn appear(&self) {
            (self.appeared.borrow().as_ref().expect("the runner registered a hook"))();
        }

        fn answer_activate_own(&self) {
            let done = self
                .activate_own_done
                .borrow_mut()
                .take()
                .expect("an activate_own is waiting");
            done(Some(true));
        }
    }

    impl GnomeCalls for FakeGnome {
        fn probe(&self) {
            self.calls.borrow_mut().push("probe".into());
        }
        fn present(&self) -> Option<bool> {
            self.present.get()
        }
        fn focus_direction(&self, dir: Direction, _: Done) {
            self.calls.borrow_mut().push(format!("focus_direction:{dir:?}"));
        }
        fn focus_self_if_neighbour(&self, dir: Direction, _: Done) {
            self.calls.borrow_mut().push(format!("focus_self_if_neighbour:{dir:?}"));
        }
        fn set_partner(&self, pids: Vec<u32>, done: Done) {
            self.calls.borrow_mut().push(format!("set_partner:{pids:?}"));
            *self.set_partner_done.borrow_mut() = Some(done);
        }
        fn activate_own(&self, done: Done) {
            self.calls.borrow_mut().push("activate_own".into());
            *self.activate_own_done.borrow_mut() = Some(done);
        }
        fn activate_partner(&self, _: Done) {
            self.calls.borrow_mut().push("activate_partner".into());
        }
        fn on_appeared(&self, hook: Box<dyn Fn()>) {
            *self.appeared.borrow_mut() = Some(hook);
        }
    }

    fn gnome_runner(present: Option<bool>) -> (Rc<WmRunner>, Rc<FakeGnome>, Arc<Mutex<Vec<Vec<String>>>>) {
        let shell = FakeGnome::new(present);
        let (exec, seen) = fake("");
        let stat = |pid: u32| match pid {
            300 => Some("300 (nvim) S 250 0 0".to_string()),
            250 => Some("250 (kitty) S 1 0 0".to_string()),
            _ => None,
        };
        let runner = Rc::new(WmRunner::with_adapters(
            Wm::Gnome,
            exec,
            Arc::new(stat),
            Some(shell.clone() as Rc<dyn GnomeCalls>),
        ));
        runner.resend_partner_when_the_extension_returns();
        (runner, shell, seen)
    }

    fn no_notice(_: &str) {}

    #[test]
    fn gnome_routes_every_event_to_its_extension_method_and_runs_no_command() {
        let (runner, fake, seen) = gnome_runner(Some(true));
        assert!(runner.move_focus(Direction::Left, &no_notice), "the key is claimed");
        assert!(runner.editor_edge(Direction::Up, &no_notice));
        runner.raise_pid(std::process::id());
        runner.raise_editor(300);
        assert_eq!(
            fake.calls(),
            vec![
                "focus_direction:Left",
                "focus_self_if_neighbour:Up",
                "activate_own",
                "activate_partner"
            ]
        );
        // Another process's window cannot be asked for: the extension knows only the caller.
        runner.raise_pid(std::process::id() + 1);
        assert_eq!(fake.calls().len(), 4);
        assert!(seen.lock().unwrap().is_empty(), "no child, no window-manager tool");
    }

    #[test]
    fn gnome_names_the_editors_chain_on_attach_and_clears_it_only_when_there_is_something_to_clear() {
        let (runner, fake, _) = gnome_runner(Some(true));
        // Nothing was ever set: a start-up state with no editor sends nothing.
        runner.set_partner_from(None);
        assert!(fake.calls().is_empty());
        runner.set_partner_from(Some(300));
        assert_eq!(fake.calls(), vec!["set_partner:[300, 250]"]);
        runner.set_partner_from(None);
        runner.set_partner_from(None);
        assert_eq!(fake.calls(), vec!["set_partner:[300, 250]", "set_partner:[]"]);
    }

    #[test]
    fn a_forwarded_raise_waits_for_its_partner_to_be_recorded() {
        let (runner, fake, _) = gnome_runner(Some(true));
        runner.raise_for_request(Some(vec![40, 30]), None);
        assert_eq!(fake.calls(), vec!["set_partner:[40, 30]"], "no raise before the reply");
        fake.answer_set_partner();
        assert_eq!(fake.calls(), vec!["set_partner:[40, 30]", "activate_own"]);
        fake.answer_activate_own();
        assert_eq!(fake.calls().len(), 2, "no editor to put back");
        // The chain stands, so a later detach clears it.
        runner.set_partner_from(None);
        assert_eq!(fake.calls().last().unwrap(), "set_partner:[]");
        // A raise-only request leaves the partner alone and raises at once.
        let (runner, fake, _) = gnome_runner(Some(true));
        runner.raise_for_request(None, Some(300));
        assert_eq!(fake.calls(), vec!["activate_own"]);
    }

    /// A partner the extension did not record may leave the earlier one in place: no raise then.
    #[test]
    fn a_failed_set_partner_raises_nothing() {
        for reply in [None, Some(false)] {
            let (runner, fake, _) = gnome_runner(Some(true));
            runner.raise_for_request(Some(vec![40, 30]), None);
            fake.answer_set_partner_with(reply);
            assert_eq!(fake.calls(), vec!["set_partner:[40, 30]"], "{reply:?}");
        }
    }

    /// A request for the editor already attached changes no link, so the editor is put back as the
    /// partner once the panel is raised, not left to the window that sent the request.
    #[test]
    fn a_same_editor_request_puts_the_editor_back_as_the_partner_after_the_raise() {
        let (runner, fake, _) = gnome_runner(Some(true));
        runner.raise_for_request(Some(vec![900, 800]), Some(300));
        fake.answer_set_partner();
        fake.answer_activate_own();
        assert_eq!(
            fake.calls(),
            vec!["set_partner:[900, 800]", "activate_own", "set_partner:[300, 250]"]
        );
    }

    /// The raise of an earlier same-editor request finishing after the panel moved on to another
    /// editor must not hand the partner back to the earlier one.
    #[test]
    fn a_raise_that_finishes_after_another_editor_became_the_partner_restores_nothing() {
        let (runner, fake, _) = gnome_runner(Some(true));
        runner.raise_for_request(Some(vec![900, 800]), Some(300));
        fake.answer_set_partner();
        // The raise is still pending when the panel attaches to another editor.
        runner.set_partner_from(Some(250));
        fake.answer_activate_own();
        assert_eq!(
            fake.calls(),
            vec!["set_partner:[900, 800]", "activate_own", "set_partner:[250]"],
            "no restore of the first editor's chain"
        );
        // The same when the newer change is another request.
        let (runner, fake, _) = gnome_runner(Some(true));
        runner.raise_for_request(Some(vec![900, 800]), Some(300));
        fake.answer_set_partner();
        runner.raise_for_request(Some(vec![70]), None);
        fake.answer_activate_own();
        assert_eq!(
            fake.calls(),
            vec!["set_partner:[900, 800]", "activate_own", "set_partner:[70]"]
        );
    }

    /// A screen lock disables the extension and unlocking enables it with no partner.
    #[test]
    fn an_extension_that_returns_is_told_the_partner_again() {
        let (runner, fake, _) = gnome_runner(Some(true));
        // Nothing to tell yet.
        fake.appear();
        assert!(fake.calls().is_empty());
        runner.set_partner_from(Some(300));
        fake.appear();
        assert_eq!(fake.calls(), vec!["set_partner:[300, 250]", "set_partner:[300, 250]"]);
        // The chain of a request is the one remembered, and a detach makes it empty again.
        runner.raise_for_request(Some(vec![40, 30]), None);
        fake.answer_set_partner();
        fake.appear();
        assert_eq!(fake.calls().last().unwrap(), "set_partner:[40, 30]");
        runner.set_partner_from(None);
        let sent = fake.calls().len();
        fake.appear();
        assert_eq!(fake.calls().len(), sent, "a cleared partner is not sent");
    }

    /// The panel attached while the extension was off, and the user then enabled it.
    #[test]
    fn a_partner_named_while_the_extension_was_off_is_sent_when_it_appears() {
        let (runner, fake, _) = gnome_runner(Some(false));
        runner.set_partner_from(Some(300));
        assert!(fake.calls().is_empty(), "nothing can be called while it is absent");
        fake.present.set(Some(true));
        fake.appear();
        assert_eq!(fake.calls(), vec!["set_partner:[300, 250]"]);
        // The same for the chain of a request.
        let (runner, fake, _) = gnome_runner(Some(false));
        runner.raise_for_request(Some(vec![40, 30]), None);
        assert!(fake.calls().is_empty());
        fake.present.set(Some(true));
        fake.appear();
        assert_eq!(fake.calls(), vec!["set_partner:[40, 30]"]);
        // A request for the editor already attached, from another terminal, while the extension is off: the
        // editor stays the partner, not the terminal that sent it.
        let (runner, fake, _) = gnome_runner(Some(false));
        runner.raise_for_request(Some(vec![900, 800]), Some(300));
        assert!(fake.calls().is_empty());
        fake.present.set(Some(true));
        fake.appear();
        assert_eq!(fake.calls(), vec!["set_partner:[300, 250]"]);
    }

    #[test]
    fn other_adapters_raise_at_once_and_have_no_partner_to_set() {
        let (exec, seen) = fake("[]");
        let runner = WmRunner::with_commands(Wm::Hyprland, exec, Arc::new(|_| None));
        runner.set_partner_from(Some(300));
        runner.set_partner_from(None);
        runner.raise_for_request(Some(vec![1, 2]), None);
        let waited = Instant::now();
        while seen.lock().unwrap().is_empty() && waited.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(seen.lock().unwrap()[0], vec!["hyprctl", "clients", "-j"]);
    }

    #[test]
    fn an_edge_letter_is_the_ordinary_move_where_gnome_is_not_in_play() {
        let (exec, seen) = hyprland(|_| LEGACY_PROBE, |_| "ok");
        let runner = WmRunner::with_commands(Wm::Hyprland, exec, Arc::new(|_| None));
        assert!(runner.editor_edge(Direction::Right, &no_notice));
        let waited = Instant::now();
        while seen.lock().unwrap().len() < 2 && waited.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec![probe(), strings(&["hyprctl", "dispatch", "movefocus", "r"])]
        );
        let runner = WmRunner::with_commands(Wm::None, fake("").0, Arc::new(|_| None));
        let notices = RefCell::new(Vec::new());
        assert!(!runner.editor_edge(Direction::Left, &|t| notices.borrow_mut().push(t.to_string())));
        assert_eq!(*notices.borrow(), vec![compositor::NO_ADAPTER_NOTICE.to_string()]);
    }

    #[test]
    fn a_known_absent_extension_behaves_as_no_adapter_with_the_gnome_notice_once() {
        let (runner, fake, seen) = gnome_runner(Some(false));
        let notices = RefCell::new(Vec::new());
        let say = |text: &str| notices.borrow_mut().push(text.to_string());
        assert!(!runner.move_focus(Direction::Left, &say), "the key goes on to the page");
        assert!(!runner.editor_edge(Direction::Right, &say));
        assert!(!runner.move_focus(Direction::Down, &say));
        assert_eq!(
            *notices.borrow(),
            vec![compositor::GNOME_NO_EXTENSION_NOTICE.to_string()]
        );
        runner.raise_pid(std::process::id());
        runner.raise_editor(300);
        runner.set_partner_from(Some(300));
        runner.raise_for_request(Some(vec![40]), None);
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn an_extension_not_yet_heard_from_is_assumed_present() {
        let (runner, fake, _) = gnome_runner(None);
        let notices = RefCell::new(Vec::new());
        assert!(runner.move_focus(Direction::Left, &|t| notices.borrow_mut().push(t.to_string())));
        assert!(notices.borrow().is_empty());
        assert_eq!(fake.calls(), vec!["focus_direction:Left"]);
        // Once the bus says nobody owns the name, the same runner stops calling.
        fake.present.set(Some(false));
        assert!(!runner.move_focus(Direction::Left, &|t| notices.borrow_mut().push(t.to_string())));
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(
            *notices.borrow(),
            vec![compositor::GNOME_NO_EXTENSION_NOTICE.to_string()]
        );
    }
}
