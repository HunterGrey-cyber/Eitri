//! A terminal moving to the other window of the pair, as far as the shell's part of it needs no display.
//!
//! The controller (`eitri_core::wm::moves`) decides every step of a move; the shell carries out the terminal's three
//! of them on its session (`eitri_terminal::TerminalSession`): the freeze (`TermOp::Detach`), which the session
//! answers on a channel with what the other window needs or why it stays; the forget once the other window holds it
//! (`TermOp::Forget`); and the resume when the move did not happen (`TermOp::Resume`). The destination's
//! `AdoptMoved` builds a session over what `move` carried. This module holds what those steps have in common
//! wherever they run, the GTK pane's services or a test's two controllers:
//!
//! - [`Freezes`]: the freeze each module's session was last asked for. A refusal names no move when the host posts it
//!   (`HostEvent::Refused` carries the module only), and the controller gives up whichever move of that module is
//!   freezing now, so a refusal that comes back after its move ended and a retry is freezing would give the retry up.
//!   Only the refusal of the freeze still asked for is posted; an older one is dropped and logged.
//! - [`wait_frozen`]: the session's answer, read off the GTK thread (the screen's replay can be large), as the record
//!   and descriptors `move` carries, or as the refusal's words.
//! - [`arrival`] and [`adopt`]: the destination's reading of that record and those descriptors. They come from the
//!   other window's process and are read as its input: bounded, checked, and refused as a whole when anything is off,
//!   before any session exists. The shell's pid is trusted only once a pidfd opened for it names a process that
//!   started when the record says (`agent::child::PidFd::open`).
//!
//! The record is a JSON object: `kind` (`"terminal"`), the shell's `pid` and `start` time, the screen's `replay`, the
//! `tail` of output the source read and the replay does not show, the `outbox` of input and answers not yet written to
//! the PTY, and the `view` (scroll and selection). The one descriptor is the PTY's controller side.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use eitri_core::layout::ModuleId;
use eitri_core::window_link::moves::MoveId;
use eitri_core::wm::{HostEvent, OpTag, TermEvent, WinId};
use eitri_terminal::replay::screen_replay::ScreenReplay;
use eitri_terminal::{DetachRefused, Detached, SessionConfig, Tail, TerminalSession, ViewState};
use serde_json::Value;

/// The record's `kind`.
pub(crate) const KIND: &str = "terminal";

/// How long a freeze's answer is waited for, on its worker thread. A session answers within a second (a cut
/// sequence's wait and a synchronized update's few rounds); past this its thread is taken to be stuck, and the move is
/// refused as the session would have refused it.
pub(crate) const ANSWER_WAIT: Duration = Duration::from_secs(10);

/// The widest and tallest screen an arriving record may ask for. A real one is a window's size in cells; a larger
/// one is not a screen any window shows, and building it would take memory the record's sender chose.
const MAX_CELLS_PER_SIDE: u16 = 2000;

/// The freeze each module's session was last asked for, by move.
#[derive(Default)]
pub(crate) struct Freezes(RefCell<BTreeMap<ModuleId, MoveId>>);

impl Freezes {
    /// `module`'s session is asked to freeze for `mv`: from now on only that freeze's answer is outstanding.
    pub(crate) fn asked(&self, module: &ModuleId, mv: MoveId) {
        self.0.borrow_mut().insert(module.clone(), mv);
    }

    /// Whether `mv` is the freeze `module` still waits on; if it is, it is answered now.
    pub(crate) fn answered(&self, module: &ModuleId, mv: MoveId) -> bool {
        let mut outstanding = self.0.borrow_mut();
        if outstanding.get(module) == Some(&mv) {
            outstanding.remove(module);
            true
        } else {
            false
        }
    }

    /// `module` no longer waits on any freeze: it was resumed, forgotten or ended.
    pub(crate) fn forget(&self, module: &ModuleId) {
        self.0.borrow_mut().remove(module);
    }

    /// The move `module`'s session is freezing for, if any.
    #[cfg(test)]
    pub(crate) fn outstanding(&self, module: &ModuleId) -> Option<MoveId> {
        self.0.borrow().get(module).copied()
    }
}

/// What a session answered a freeze with, read for the link.
#[derive(Debug)]
pub(crate) enum Frozen {
    /// The terminal as `move` carries it.
    Record { record: Value, fds: Vec<OwnedFd> },
    /// The terminal stays: why, as the user is told.
    Refused(String),
}

/// What the host does with a freeze's answer.
#[derive(Debug)]
pub(crate) enum Answer {
    /// Posted as `TermEvent::Detached` with the freeze's own move: the controller matches it to its move by that id,
    /// so even an answer for a move that has ended goes to it, and it resumes or ignores it.
    Detached { record: Value, fds: Vec<OwnedFd> },
    /// Posted as the refusal of the freeze, naming the module.
    Refused(String),
    /// A refusal of a freeze no longer asked for: nothing is posted.
    Dropped(String),
}

/// The words a refused freeze is shown with.
pub(crate) fn refusal_text(refused: DetachRefused) -> String {
    match refused {
        DetachRefused::RawTooLong => "the terminal is in the middle of a long sequence; try again".into(),
        DetachRefused::TooDeep => "terminal state too deep to move".into(),
        DetachRefused::NoDescriptor => "no descriptor was left to hand the terminal over".into(),
        DetachRefused::ServerTerminal => "a terminal on the server does not move to another window yet".into(),
    }
}

/// Waits at most `wait` for the session's answer to a freeze, and reads it for the link. Blocks: never on the GTK
/// thread.
pub(crate) fn wait_frozen(answer: Receiver<Result<Detached, DetachRefused>>, wait: Duration) -> Frozen {
    match answer.recv_timeout(wait) {
        Ok(answer) => frozen_of(answer),
        Err(RecvTimeoutError::Timeout) => Frozen::Refused("the terminal did not freeze in time".into()),
        Err(RecvTimeoutError::Disconnected) => Frozen::Refused("the terminal's session has ended".into()),
    }
}

/// The session's answer, read for the link.
pub(crate) fn frozen_of(answer: Result<Detached, DetachRefused>) -> Frozen {
    match answer {
        Ok(detached) => record_of(detached),
        Err(refused) => Frozen::Refused(refusal_text(refused)),
    }
}

/// A frozen terminal as `move` carries it.
pub(crate) fn record_of(detached: Detached) -> Frozen {
    let Detached {
        master,
        pid,
        start,
        replay,
        tail,
        outbox,
        view,
    } = detached;
    let fields = (
        serde_json::to_value(&replay),
        serde_json::to_value(&tail),
        serde_json::to_value(&view),
    );
    let (Ok(replay), Ok(tail), Ok(view)) = fields else {
        // Nothing of the terminal has left: the descriptor closes as `master` drops, and the session resumes.
        return Frozen::Refused("the terminal's screen could not be written down".into());
    };
    let record = serde_json::json!({
        "kind": KIND,
        "pid": pid,
        "start": start,
        "replay": replay,
        "tail": tail,
        "outbox": Value::from(outbox),
        "view": view,
    });
    Frozen::Record {
        record,
        fds: vec![master],
    }
}

/// What the host does with `frozen`, the answer to `module`'s freeze for `mv`.
pub(crate) fn answer_for(freezes: &Freezes, module: &ModuleId, mv: MoveId, frozen: Frozen) -> Answer {
    let outstanding = freezes.answered(module, mv);
    match frozen {
        Frozen::Record { record, fds } => Answer::Detached { record, fds },
        Frozen::Refused(why) if outstanding => Answer::Refused(why),
        Frozen::Refused(why) => Answer::Dropped(format!(
            "{module}: a refused freeze for move {}, which no longer waits on it ({why}); dropped",
            mv.0
        )),
    }
}

/// The event the host posts for `answer`, the answer to `module`'s freeze for `mv` in `win`; `None` for a dropped one,
/// which is logged.
pub(crate) fn event_for(win: WinId, module: ModuleId, mv: MoveId, answer: Answer) -> Option<HostEvent> {
    match answer {
        Answer::Detached { record, fds } => Some(HostEvent::Terminal(TermEvent::Detached {
            win,
            module,
            mv,
            record,
            handoff: crate::gtk_host::peer::handoff_of(fds),
        })),
        Answer::Refused(why) => Some(HostEvent::Refused {
            what: OpTag::Terminal,
            why,
            surface: Some((win, module)),
        }),
        Answer::Dropped(note) => {
            eprintln!("[terminal] {note}");
            None
        }
    }
}

/// What a `move`'s record and descriptors come to here: the terminal to adopt, and a pidfd naming its shell, opened
/// and checked against the record's start time. Refused as a whole when anything is off.
pub(crate) fn arrival(record: &Value, fds: Vec<OwnedFd>) -> Result<(Detached, OwnedFd), String> {
    let object = record.as_object().ok_or("the terminal's record is not an object")?;
    if object.get("kind").and_then(Value::as_str) != Some(KIND) {
        return Err("the record is not a terminal's".into());
    }
    let mut fds = fds.into_iter();
    let (Some(master), None) = (fds.next(), fds.next()) else {
        return Err("a terminal comes with exactly one descriptor".into());
    };
    // SAFETY: `isatty` only asks the kernel about a descriptor this function owns.
    if unsafe { libc::isatty(master.as_raw_fd()) } != 1 {
        return Err("the terminal's descriptor is not a terminal".into());
    }
    let number = |name: &str| object.get(name).and_then(Value::as_u64);
    let pid = number("pid")
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 1)
        .ok_or("the record names no shell")?;
    let start = number("start").ok_or("the record gives no start time for the shell")?;
    let field = |name: &str| object.get(name).cloned().ok_or(format!("the record has no {name}"));
    let replay: ScreenReplay =
        serde_json::from_value(field("replay")?).map_err(|e| format!("the record's screen: {e}"))?;
    if !(1..=MAX_CELLS_PER_SIDE).contains(&replay.columns) || !(1..=MAX_CELLS_PER_SIDE).contains(&replay.rows) {
        return Err(format!(
            "the record's screen of {}x{} cells is not one a window shows",
            replay.columns, replay.rows
        ));
    }
    let tail: Tail = serde_json::from_value(field("tail")?).map_err(|e| format!("the record's tail: {e}"))?;
    let outbox: Vec<u8> = serde_json::from_value(field("outbox")?).map_err(|e| format!("the record's outbox: {e}"))?;
    let view: ViewState = serde_json::from_value(field("view")?).map_err(|e| format!("the record's view: {e}"))?;
    let pidfd = agent::child::PidFd::open(pid, start).map_err(|e| format!("the shell {pid} cannot be named: {e}"))?;
    let detached = Detached {
        master,
        pid,
        start,
        replay,
        tail,
        outbox,
        view,
    };
    Ok((detached, pidfd.into_fd()))
}

/// The destination's session over what a committed move carried: the screen rebuilt, then committed at once, since
/// the controller asks this only once its `move_done` is on the socket, so the source has let the terminal go and the
/// outbox it carried may be written now. With it, whether the view arrives in copy mode: the session's screen keeps
/// it, and the pane's own key routing must too, or copy mode's letters would reach the shell.
pub(crate) fn adopt(
    record: &Value,
    fds: Vec<OwnedFd>,
    config: SessionConfig,
    wake: impl Fn() + Send + 'static,
) -> Result<(TerminalSession, bool), String> {
    let (detached, pidfd) = arrival(record, fds)?;
    let copy_mode = detached.view.copy_mode;
    let session = TerminalSession::adopt(detached, pidfd, config, wake)
        .map_err(|e| format!("the terminal could not be taken over: {e}"))?;
    session.commit();
    Ok((session, copy_mode))
}

#[cfg(test)]
mod tests;
