//! The GTK-side half of companion focus: it turns `eitri_core::wm`'s argv lists into commands run
//! on a worker thread. Nothing here waits for a command on the GTK thread: a key that moves focus
//! is claimed at once and the worker finishes the move (or finds nothing to move to) afterwards.
//!
//! The commands themselves are injected (`Exec`), so the logic below runs in tests without a
//! compositor and without starting a process.
//!
//! GNOME is the one adapter with no command to run: a client there cannot list or focus windows, so
//! its Shell extension does, and the calls to it go through `gnome_shell` (async gio calls, no
//! worker thread, no child). Behind the `GnomeCalls` trait the routing below is tested with a fake.

use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use eitri_core::layout::Direction;
use eitri_core::wm::{self, WindowRef, Wm};

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
    exec: Exec,
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
        let runner = Rc::new(Self::with_adapters(wm, real_exec(wm), Arc::new(wm::proc_stat), gnome));
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
            exec,
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
            Wm::None => Some(wm::NO_ADAPTER_NOTICE),
            Wm::Gnome if self.gnome().is_none() => Some(wm::GNOME_NO_EXTENSION_NOTICE),
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
        let (wm, exec) = (self.wm, self.exec.clone());
        thread::spawn(move || move_focus_blocking(wm, &*exec, dir, &|line| println!("{line}")));
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
        self.raise(move |read_stat| wm::ppid_chain(nvim_pid, read_stat, CHAIN_MAX));
    }

    /// Tells the GNOME extension which window is the editor's, by nvim's own pid and its
    /// ancestors (the extension picks the first that owns a window, which a client cannot do
    /// itself there), or clears it with `None` once the panel has no editor. Nothing elsewhere.
    pub(crate) fn set_partner_from(&self, nvim_pid: Option<u32>) {
        if self.gnome.is_none() {
            return;
        }
        let chain = match nvim_pid {
            Some(pid) => wm::ppid_chain(pid, &*self.read_stat, CHAIN_MAX),
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
                    let editor_chain = wm::ppid_chain(pid, &*self.read_stat, CHAIN_MAX);
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
                let editor_chain = editor_pid.map(|pid| wm::ppid_chain(pid, &*self.read_stat, CHAIN_MAX));
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
        let (wm, exec, read_stat) = (self.wm, self.exec.clone(), self.read_stat.clone());
        thread::spawn(move || {
            let chain = chain(&*read_stat);
            raise_blocking(wm, &*exec, &chain, &|line| println!("{line}"));
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
    Arc::new(move |argv: Vec<String>| {
        let (program, args) = argv.split_first().ok_or_else(|| "empty command".to_string())?;
        match Command::new(program).args(args).output() {
            Ok(output) => {
                if !output.status.success() {
                    println!(
                        "[companion] {} adapter: {program} exited with {}: {}",
                        wm_name(wm),
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
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

fn move_focus_blocking(
    wm: Wm,
    exec: &dyn Fn(Vec<String>) -> Result<String, String>,
    dir: Direction,
    log: &dyn Fn(&str),
) {
    if wm == Wm::Sway {
        let windows = wm::list_windows_argv(wm)
            .and_then(|argv| exec(argv).ok())
            .and_then(|json| wm::parse_windows(wm, &json).ok());
        match windows.as_deref().and_then(|windows| wm::has_neighbour(windows, dir)) {
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
    if let Some(argv) = wm::move_focus_argv(wm, dir) {
        let _ = exec(argv);
    }
}

/// Focuses the first window owned by one of `chain`'s processes, nearest first. With none found
/// nothing is run and one line names the chain: inside tmux it ends at the tmux server, which owns
/// no window.
fn raise_blocking(wm: Wm, exec: &dyn Fn(Vec<String>) -> Result<String, String>, chain: &[u32], log: &dyn Fn(&str)) {
    let Some(list) = wm::list_windows_argv(wm) else { return };
    let Ok(json) = exec(list) else { return };
    let windows: Vec<WindowRef> = match wm::parse_windows(wm, &json) {
        Ok(windows) => windows,
        Err(err) => {
            log(&format!("[companion] {} adapter: {err}", wm_name(wm)));
            return;
        }
    };
    let Some(target) = wm::first_window_owner(chain, &windows) else {
        log(&format!(
            "[companion] raise: no window owned by any process in {chain:?}; nothing raised"
        ));
        return;
    };
    match wm::focus_window_argv(wm, target) {
        Some(argv) => {
            let _ = exec(argv);
        }
        None => log(&format!(
            "[companion] raise: {} cannot focus pid {} without a window id",
            wm_name(wm),
            target.pid
        )),
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
        assert_eq!(*notices.borrow(), vec![wm::NO_ADAPTER_NOTICE.to_string()]);
    }

    #[test]
    fn a_chain_that_reaches_no_window_raises_nothing() {
        // 300's parent is 250, whose parent is 1; no window belongs to either.
        let stat = |pid: u32| match pid {
            300 => Some("300 (nvim) S 250 0 0".to_string()),
            250 => Some("250 (zsh) S 1 0 0".to_string()),
            _ => None,
        };
        let chain = wm::ppid_chain(300, &stat, CHAIN_MAX);
        assert_eq!(chain, vec![300, 250]);
        let (exec, seen) = fake(sway_tree(0, 500));
        let lines = RefCell::new(Vec::new());
        raise_blocking(Wm::Sway, &*exec, &chain, &|line| {
            lines.borrow_mut().push(line.to_string())
        });
        // Only the window list was asked for: no focus command ran.
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(seen.lock().unwrap()[0], vec!["swaymsg", "-t", "get_tree"]);
        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0].contains("[300, 250]"), "{:?}", lines.borrow());
    }

    #[test]
    fn a_chain_that_reaches_a_window_focuses_it() {
        let (exec, seen) = fake(sway_tree(0, 500));
        raise_blocking(Wm::Sway, &*exec, &[300, 200], &|_| {});
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], vec!["swaymsg".to_string(), "[pid=200] focus".to_string()]);
    }

    #[test]
    fn sway_stops_at_an_edge_and_moves_when_a_window_lies_that_way() {
        // The panel (focused) is on the left, the other window on the right.
        let (exec, seen) = fake(sway_tree(0, 500));
        move_focus_blocking(Wm::Sway, &*exec, Direction::Left, &|_| {});
        assert_eq!(seen.lock().unwrap().len(), 1, "an edge runs only the tree query");
        let (exec, seen) = fake(sway_tree(0, 500));
        move_focus_blocking(Wm::Sway, &*exec, Direction::Right, &|_| {});
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], vec!["swaymsg", "focus", "right"]);
    }

    #[test]
    fn hyprland_moves_without_asking() {
        let (exec, seen) = fake("");
        move_focus_blocking(Wm::Hyprland, &*exec, Direction::Up, &|_| {});
        assert_eq!(
            *seen.lock().unwrap(),
            vec![vec!["hyprctl", "dispatch", "movefocus", "u"]]
        );
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
        let (exec, seen) = fake("");
        let runner = WmRunner::with_commands(Wm::Hyprland, exec, Arc::new(|_| None));
        assert!(runner.editor_edge(Direction::Right, &no_notice));
        let waited = Instant::now();
        while seen.lock().unwrap().is_empty() && waited.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec![vec!["hyprctl", "dispatch", "movefocus", "r"]]
        );
        let runner = WmRunner::with_commands(Wm::None, fake("").0, Arc::new(|_| None));
        let notices = RefCell::new(Vec::new());
        assert!(!runner.editor_edge(Direction::Left, &|t| notices.borrow_mut().push(t.to_string())));
        assert_eq!(*notices.borrow(), vec![wm::NO_ADAPTER_NOTICE.to_string()]);
    }

    #[test]
    fn a_known_absent_extension_behaves_as_no_adapter_with_the_gnome_notice_once() {
        let (runner, fake, seen) = gnome_runner(Some(false));
        let notices = RefCell::new(Vec::new());
        let say = |text: &str| notices.borrow_mut().push(text.to_string());
        assert!(!runner.move_focus(Direction::Left, &say), "the key goes on to the page");
        assert!(!runner.editor_edge(Direction::Right, &say));
        assert!(!runner.move_focus(Direction::Down, &say));
        assert_eq!(*notices.borrow(), vec![wm::GNOME_NO_EXTENSION_NOTICE.to_string()]);
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
        assert_eq!(*notices.borrow(), vec![wm::GNOME_NO_EXTENSION_NOTICE.to_string()]);
    }
}
