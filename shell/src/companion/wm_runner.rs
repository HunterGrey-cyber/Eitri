//! The GTK-side half of companion focus: it turns `eitri_core::wm`'s argv lists into commands run
//! on a worker thread. Nothing here waits for a command on the GTK thread: a key that moves focus
//! is claimed at once and the worker finishes the move (or finds nothing to move to) afterwards.
//!
//! The commands themselves are injected (`Exec`), so the logic below runs in tests without a
//! compositor and without starting a process.

use std::cell::Cell;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use eitri_core::layout::Direction;
use eitri_core::wm::{self, WindowRef, Wm};

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
}

impl WmRunner {
    pub(crate) fn new(wm: Wm) -> Rc<Self> {
        println!("[companion] window manager adapter: {}", wm_name(wm));
        Rc::new(Self::with_commands(wm, real_exec(wm), Arc::new(wm::proc_stat)))
    }

    fn with_commands(wm: Wm, exec: Exec, read_stat: ReadStat) -> Self {
        WmRunner {
            wm,
            notice_shown: Cell::new(false),
            exec,
            read_stat,
        }
    }

    /// `Ctrl+h/j/k/l` out of the panel, the prefix's Select, and an edge letter from nvim. `false`
    /// with no adapter: the key then goes on to the page, and `on_none` says why, once. With an
    /// adapter the key is claimed at once and a worker does the rest. On sway it first asks
    /// whether any window lies that way, because sway wraps around to the far side; at an edge the
    /// key is therefore consumed with nothing moved, as tmux consumes `select-pane` there (the
    /// GTK side has to claim or release the key before the worker can answer, and never waits).
    pub(crate) fn move_focus(&self, dir: Direction, on_none: &dyn Fn(&str)) -> bool {
        if self.wm == Wm::None {
            if !self.notice_shown.replace(true) {
                on_none(wm::NO_ADAPTER_NOTICE);
            }
            return false;
        }
        let (wm, exec) = (self.wm, self.exec.clone());
        thread::spawn(move || move_focus_blocking(wm, &*exec, dir, &|line| println!("{line}")));
        true
    }

    /// Brings the window owned by this process (`std::process::id()`) or by the editor forward.
    pub(crate) fn raise_pid(&self, pid: u32) {
        self.raise(move |_| vec![pid]);
    }

    /// The editor's window: the first window owned by `nvim_pid` or one of its ancestors (a nvim
    /// run in a terminal has the terminal emulator as an ancestor).
    pub(crate) fn raise_editor(&self, nvim_pid: u32) {
        self.raise(move |read_stat| wm::ppid_chain(nvim_pid, read_stat, CHAIN_MAX));
    }

    fn raise(&self, chain: impl FnOnce(&dyn Fn(u32) -> Option<String>) -> Vec<u32> + Send + 'static) {
        if self.wm == Wm::None {
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
}
