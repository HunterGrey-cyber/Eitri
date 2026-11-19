//! A plugin page's `terminal.at`: the base terminal is put in a directory only while nothing but its shell runs.
//!
//! A terminal with no shell running (never started, or killed, or its shell ended) keeps the directory for its next
//! start; a pane that can start a shell unasked (an ended shell restarts on Enter, a shown terminal's spawn waits for
//! its size) is given it at once too. A running shell is read first, off the toolkit's thread
//! ([`TermOp::ReadJob`]), and the answer is decided on what comes back ([`TermEvent::Job`]): a shell already in the
//! directory or below it is left as it is, whatever runs in it; a shell with anything else in its session is left
//! alone; an idle shell elsewhere is ended and a fresh one starts in the directory in the same pane
//! ([`TermOp::Respawn`]), with no layout, focus or surface op.
//!
//! A request has one deadline, [`ANSWER_WAIT`] from when it is made, which the read and a restart share: whatever is
//! still waiting then is answered by this module, so the page hears what happened from the window, before the plugin
//! host's own later deadline for the verb. A read that comes back with too little of it left for a restart touches
//! nothing.
//!
//! The kill and show paths are not used: a hang-up of the base terminal is a kill there (the keys leave, the
//! terminal comes back below the editor, the window closes when it is the last module), which is none of what a
//! restart in place wants. So while a restart waits for its new shell, an `Exited` for that terminal is held rather
//! than taken as a kill: dropped when the new shell starts, and replayed as it would have been taken when the
//! restart is refused or waits too long.

use super::host::*;
use super::module::TermRun;
use super::window::{WinId, Window};
use crate::layout::ModuleId;
use crate::plugin::host::{TerminalAtAnswer, NOT_OWN_WINDOW_TEXT, TERMINAL_AT_WAIT};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long a request may take, from when it is made: a running shell's read and the restart it may lead to share
/// it, and whatever still waits at its end is answered `busy` then (and a held `Exited` replayed).
pub const ANSWER_WAIT: Duration = TERMINAL_AT_WAIT;
/// The least of [`ANSWER_WAIT`] a restart needs left when its read comes back: with less, the new shell could start
/// after the page had been told the terminal did not answer, so the shell is left alone and the page told that.
pub const RESTART_RESERVE: Duration = Duration::from_secs(1);

/// The answer for a read that describes a shell the terminal no longer runs, or could not be read.
pub const CHANGED_TEXT: &str = "the terminal changed";
/// The answer for a shell with anything but itself running in its session.
pub const RUNNING_TEXT: &str = "a program is running in the terminal";
/// The refusal for a request made while a restart waits for its new shell.
pub const RESTARTING_TEXT: &str = "the terminal is being restarted";
/// The refusal for a window that holds no base terminal.
pub const NO_TERMINAL_TEXT: &str = "this window has no terminal";
/// The refusal for a base terminal the other window of the pair holds now.
pub const AWAY_TEXT: &str = "the terminal is in the other window";
/// The answer for a read that did not come back in time, or came back with too little time left for a restart:
/// nothing was touched.
pub const SLOW_TEXT: &str = "the terminal did not answer within 5 s";
/// The answer for a restart whose new shell had not started by the request's deadline: the old shell was ended, and
/// the new one may still start in the directory.
pub const RESTART_LATE_TEXT: &str = "the terminal's new shell did not start within 5 s";

const _: () = assert!(ANSWER_WAIT.as_secs() == 5, "the answers' texts name the deadline");

/// A running shell's read out for a page's `terminal.at`, matched to its answer by `read`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAt {
    pub ticket: u64,
    pub plugin: String,
    pub dir: PathBuf,
    pub read: u64,
    /// The terminal reported a shell starting or ending since the read was asked for: its answer describes a shell
    /// the terminal may no longer run.
    pub(super) stale: bool,
    /// When the request must be answered by: a restart it leads to gets what is left of it.
    pub(super) deadline: Instant,
    pub(super) timer: TimerToken,
}

/// A restart in place sent to the host and not yet answered: `shell` is the shell it ends, `held` an `Exited` for
/// the terminal that came meanwhile.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRestart {
    pub module: ModuleId,
    pub shell: ShellStart,
    pub held: Option<TermEvent>,
    pub since: Instant,
    pub(super) ticket: u64,
    pub(super) plugin: String,
    pub(super) timer: TimerToken,
}

/// What a timer of this module's did.
pub(super) enum Fired {
    /// The timer is none of these.
    No,
    /// A request was let go.
    Done,
    /// A restart waited too long: its held `Exited`, to be taken now as it would have been.
    Replay(Option<TermEvent>),
}

fn answer(win: WinId, plugin: &str, ticket: u64, answer: TerminalAtAnswer, batch: &mut Batch) {
    batch.push_effect(HostOp::Plugin(PluginOp::TerminalAtAnswer {
        win,
        plugin: plugin.to_owned(),
        ticket,
        answer,
    }));
}

/// A page's `terminal.at {dir}` in `win`, `dir` already checked by the plugin host: answered at once with the
/// directory kept for a terminal with no shell running, or a read of the running shell asked for. `Err` is a
/// refusal now, the first for a plugin that is not this window's own: the window decides that here, whatever the
/// asking host believes.
pub(super) fn request(
    w: &mut Window,
    win: WinId,
    plugin: &str,
    dir: PathBuf,
    ticket: u64,
    batch: &mut Batch,
) -> Result<(), String> {
    let module = ModuleId::terminal();
    if w.own_window.as_deref() != Some(plugin) {
        return Err(NOT_OWN_WINDOW_TEXT.to_string());
    }
    if w.pending_restart.is_some() {
        return Err(RESTARTING_TEXT.to_string());
    }
    if matches!(w.lifecycle.state(&module), Some(super::module::ModuleState::Away)) {
        return Err(AWAY_TEXT.to_string());
    }
    let run = w.lifecycle.terminal_run(&module).cloned();
    match run {
        None => Err(NO_TERMINAL_TEXT.to_string()),
        Some(run @ (TermRun::NotStarted | TermRun::Exited)) => {
            w.lifecycle.keep_start_dir(&module, dir.clone());
            // A pane that can start its shell with no further `Start` takes the directory now as well: an ended
            // shell starts again in place on Enter, and a shown terminal's `Start` is already out, its spawn waiting
            // for the pane's first size. What is kept then goes with whichever start comes first.
            let starts_unasked =
                run == TermRun::Exited || matches!(w.lifecycle.state(&module), Some(super::module::ModuleState::Shown));
            if starts_unasked {
                batch.push_effect(HostOp::Terminal(TermOp::SetStartDir {
                    win,
                    module: module.clone(),
                    dir: dir.clone(),
                }));
            }
            w.say(format!(
                "[terminal] {module}: no shell runs; its next one starts in {}",
                dir.display()
            ));
            answer(win, plugin, ticket, TerminalAtAnswer::StartDir, batch);
            Ok(())
        }
        Some(TermRun::Running { .. } | TermRun::Moved) => {
            let read = w.next_job_read;
            w.next_job_read += 1;
            let deadline = w.now() + ANSWER_WAIT;
            let timer = w.arm_after(ANSWER_WAIT);
            batch.push_effect(HostOp::Terminal(TermOp::ReadJob {
                win,
                module: module.clone(),
                read,
            }));
            w.pending_at.push(PendingAt {
                ticket,
                plugin: plugin.to_owned(),
                dir,
                read,
                stale: false,
                deadline,
                timer,
            });
            Ok(())
        }
    }
}

/// Whether the shell a read captured is the one the terminal's record runs now. A record that left running since
/// the read, or runs a shell with another identity, describes another shell. A moved terminal's identity is
/// checked by the host against the capture before a restart.
fn same_shell(w: &Window, pending: &PendingAt, module: &ModuleId, shell: &ShellStart) -> bool {
    if pending.stale || w.pending_restart.is_some() {
        return false;
    }
    match w.lifecycle.terminal_run(module) {
        Some(TermRun::Running { identity }) => shell.identity.as_ref() == Some(identity),
        Some(TermRun::Moved) => true,
        _ => false,
    }
}

/// A running shell's read came back: the decision, in its order. A read about another shell, or one whose
/// directory could not be read, is "the terminal changed"; then a directory that is `dir` or inside it is
/// `already`, whatever runs in the session; then anything running is `busy`; else the idle shell is restarted in
/// `dir`, answered once its new shell has started, unless less than [`RESTART_RESERVE`] of the request's time is
/// left, which leaves the shell alone and answers that the terminal did not answer in time.
#[allow(clippy::too_many_arguments)]
pub(super) fn job(
    w: &mut Window,
    win: WinId,
    module: &ModuleId,
    read: u64,
    shell: ShellStart,
    cwd: Result<PathBuf, String>,
    busy: bool,
    batch: &mut Batch,
) {
    let Some(at) = w.pending_at.iter().position(|p| p.read == read) else {
        w.say(format!(
            "[terminal] {module}: a job read came back that no request waits for any more; ignored"
        ));
        return;
    };
    let pending = w.pending_at.remove(at);
    w.cancel_timer(pending.timer);
    let decided = if *module != ModuleId::terminal() || !same_shell(w, &pending, module, &shell) {
        Some(TerminalAtAnswer::Busy(CHANGED_TEXT.to_string()))
    } else {
        match cwd {
            Err(why) => {
                w.say(format!("[terminal] {module}: its directory could not be read: {why}"));
                Some(TerminalAtAnswer::Busy(CHANGED_TEXT.to_string()))
            }
            Ok(cwd) if cwd.starts_with(&pending.dir) => Some(TerminalAtAnswer::Already),
            Ok(_) if busy => Some(TerminalAtAnswer::Busy(RUNNING_TEXT.to_string())),
            Ok(_) => None,
        }
    };
    if let Some(decided) = decided {
        answer(win, &pending.plugin, pending.ticket, decided, batch);
        return;
    }
    let since = w.now();
    let left = pending.deadline.saturating_duration_since(since);
    if left < RESTART_RESERVE {
        w.say(format!(
            "[terminal] {module}: its read came back with {left:?} of the request's time left; not restarted"
        ));
        answer(
            win,
            &pending.plugin,
            pending.ticket,
            TerminalAtAnswer::Busy(SLOW_TEXT.to_string()),
            batch,
        );
        return;
    }
    w.say(format!(
        "[terminal] {module}: restarting its idle shell in {}",
        pending.dir.display()
    ));
    let timer = w.arm_after(left);
    batch.push_effect(HostOp::Terminal(TermOp::Respawn {
        win,
        module: module.clone(),
        dir: pending.dir,
        expect: shell,
    }));
    w.pending_restart = Some(PendingRestart {
        module: module.clone(),
        shell,
        held: None,
        since,
        ticket: pending.ticket,
        plugin: pending.plugin,
        timer,
    });
}

/// An `Exited` for `module` while a restart of it waits for its new shell: held, not taken as a kill. Whether it
/// was held.
pub(super) fn hold_exited(w: &mut Window, module: &ModuleId, event: &TermEvent) -> bool {
    let Some(restart) = w.pending_restart.as_mut().filter(|r| r.module == *module) else {
        return false;
    };
    if restart.held.is_none() {
        restart.held = Some(event.clone());
    }
    w.say(format!(
        "[terminal] {module}: its shell ended while it was being restarted; held until the restart is answered"
    ));
    true
}

/// The terminal `module` reported a shell starting (`started`) or ending, or its surface ended: a read out for it
/// may describe a shell it no longer runs, and a restart waiting for its new shell has it.
pub(super) fn shell_changed(w: &mut Window, win: WinId, module: &ModuleId, started: bool, batch: &mut Batch) {
    if *module != ModuleId::terminal() {
        return;
    }
    for pending in &mut w.pending_at {
        pending.stale = true;
    }
    if !started {
        return;
    }
    let Some(restart) = w.pending_restart.take_if(|r| r.module == *module) else {
        return;
    };
    w.cancel_timer(restart.timer);
    if restart.held.is_some() {
        w.say(format!(
            "[terminal] {module}: the end of the shell it replaced is dropped; a new shell runs"
        ));
    }
    answer(win, &restart.plugin, restart.ticket, TerminalAtAnswer::Restarted, batch);
}

/// The host refused a restart in place: the pane no longer runs the shell it would have ended. The page is told
/// the terminal changed, and a held `Exited` is handed back, to be taken as it would have been.
pub(super) fn respawn_refused(
    w: &mut Window,
    win: WinId,
    module: &ModuleId,
    why: &str,
    batch: &mut Batch,
) -> Option<TermEvent> {
    let restart = w.pending_restart.take_if(|r| r.module == *module)?;
    w.cancel_timer(restart.timer);
    w.say(format!("[terminal] {module}: the restart was refused: {why}"));
    answer(
        win,
        &restart.plugin,
        restart.ticket,
        TerminalAtAnswer::Busy(CHANGED_TEXT.to_string()),
        batch,
    );
    restart.held
}

/// `token` fired, at a request's deadline: a read nothing answered is let go, so a late answer is ignored, and the
/// page told the terminal did not answer; a restart whose new shell has not started is cleared with a warning, the
/// page told so, and its held `Exited` handed back.
pub(super) fn timer(w: &mut Window, win: WinId, token: TimerToken, batch: &mut Batch) -> Fired {
    if let Some(at) = w.pending_at.iter().position(|p| p.timer == token) {
        let pending = w.pending_at.remove(at);
        w.say(format!(
            "[terminal] terminal.at: no answer from the terminal's read {} within {:?}; let go",
            pending.read, ANSWER_WAIT
        ));
        answer(
            win,
            &pending.plugin,
            pending.ticket,
            TerminalAtAnswer::Busy(SLOW_TEXT.to_string()),
            batch,
        );
        return Fired::Done;
    }
    if let Some(restart) = w.pending_restart.take_if(|r| r.timer == token) {
        w.warn(format!(
            "[terminal] {}: the restart had no answer within {:?}; what it held is taken as it came",
            restart.module, ANSWER_WAIT
        ));
        answer(
            win,
            &restart.plugin,
            restart.ticket,
            TerminalAtAnswer::Busy(RESTART_LATE_TEXT.to_string()),
            batch,
        );
        return Fired::Replay(restart.held);
    }
    Fired::No
}

#[cfg(test)]
mod tests;
