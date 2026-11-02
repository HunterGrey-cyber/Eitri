//! The window's half of reopening the editor's files: what it heard from the embedded nvim about its restore, when it
//! asks nvim for the record of what is open, and what it tells the person. No I/O happens here: the record, its marker
//! and nvim's input are the record writer's (`crate::editor_record::RecordWriter`), whose thread wakes the controller
//! through the scheduler's waker, and the capture is a call the shell makes and answers with
//! [`EditorEvent::Captured`](super::host::EditorEvent::Captured).
//!
//! Each nvim the window starts has a token, and the lines it sends move that token's state only forward:
//! `Started` -> `Aside` (final), or `Started` -> `Began` -> `Ended` (final). A line in any other order, or with
//! another nvim's token, is dropped and logged, so a leftover descendant of an earlier nvim cannot speak for this one
//! and a late `B` cannot set the marker again.
//!
//! Once the restore has ended the window captures at once, then every [`CAPTURE_EVERY`] and when the window loses
//! the keyboard (at most once per [`FOCUS_CAPTURE_EVERY`]); never two at a time, and never for an nvim that stood
//! aside. The restore marker goes only when a capture answers with a record after the restore's end: an nvim that
//! answers is out of its restore, which a line from anyone else cannot prove.

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmpv::Value;

use super::chrome;
use super::host::{Batch, CleanupOp, EditorCall, HostOp, TimerToken, Waker};
use super::toasts::ToastKind;
use super::window::{EditorRun, WinId, Window};
use crate::editor_record::{self, RecordWriter, WriterAnswer};
use crate::editor_reopen_start::Prepared;
use crate::pane_switch::ReopenLine;

/// How often the record is taken while the editor runs.
pub const CAPTURE_EVERY: Duration = Duration::from_secs(30);
/// The least time between two captures the window's losing the keyboard asks for.
pub const FOCUS_CAPTURE_EVERY: Duration = Duration::from_secs(5);
/// How long a restart of nvim waits for the writer's answer about the ended nvim's last record, and then for the new
/// nvim's input, before it goes without them.
pub const RESTART_HOLD: Duration = Duration::from_millis(1500);

/// Where one nvim's restore stands, as its lines have said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    /// Nothing heard yet.
    Started,
    /// It left reopening to something else, or had nothing to work from: nothing is captured for it.
    Aside,
    /// The restore began and has not said it ended.
    Began,
    /// The restore ended: captures run from here on.
    Ended,
}

/// A restart of nvim the window holds back until the new nvim's input is written from the newest record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
    /// Waiting for the writer's answer about the last record the nvim of `ended` left, at most until `timer`.
    Handoff { ended: String, timer: TimerToken },
    /// Waiting for the writer to say the input of `token`'s nvim is in place, at most until `timer`.
    Input { token: String, timer: TimerToken },
}

impl Held {
    fn timer(&self) -> TimerToken {
        match self {
            Held::Handoff { timer, .. } | Held::Input { timer, .. } => *timer,
        }
    }
}

/// What the restore's end counted, and the first name of each kind the editor sent after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Told {
    gone: u32,
    failed: u32,
    squeezed: u32,
    gone_name: Option<String>,
    failed_name: Option<String>,
}

/// One window's reopening, from its start's reopen step on.
pub struct Reopen {
    pub writer: RecordWriter,
    /// The current nvim's token.
    pub token: String,
    pub state: TokenState,
    /// This nvim's restore was skipped because the last one never finished: its marker stays until a record is
    /// written.
    pub skip: bool,
    /// The sequence number of the last record asked for; every capture asked takes the next.
    pub seq: u64,
    /// A capture is out and has not answered.
    pub capture_out: bool,
    /// The timer of the next timed capture, while one is armed.
    pub capture_timer: Option<TimerToken>,
    /// When the window's losing the keyboard last asked for a capture.
    pub last_focus_capture: Option<Instant>,
    /// What the start had to say, until the window is shown.
    pub held_toast: Option<String>,
    /// The tokens whose handoff read the writer was already told of.
    pub told_handoffs: BTreeSet<String>,
    /// The tokens whose handoff read the writer has answered.
    pub heard_handoffs: BTreeSet<String>,
    /// A restart of nvim waiting for the ended nvim's last record or the new one's input.
    pub restart_after_handoff: Option<Held>,
    /// A record was written since the current nvim started.
    pub wrote: bool,
    /// The sequence number when the current nvim's input was written: a record written with a higher one is this
    /// nvim's.
    session_from: u64,
    pub started_with_files: bool,
    pub channel_dir: Option<PathBuf>,
    pub root: PathBuf,
    pub prefixes: Vec<PathBuf>,
    /// The sequence number of the capture out.
    out_seq: u64,
    /// The first record a capture answered after the restore's end has come: the marker was let go then.
    answered_once: bool,
    told: Told,
    /// What the window took from the writer, for tests.
    #[cfg(test)]
    pub(super) answered: Vec<WriterAnswer>,
}

impl Reopen {
    /// The window's reopening from what its start prepared; `None` when the start reopens nothing.
    fn new(prepared: Prepared) -> Option<Reopen> {
        let (Some(token), Some(writer)) = (prepared.token, prepared.writer) else {
            return None;
        };
        Some(Reopen {
            writer,
            token,
            state: TokenState::Started,
            skip: prepared.skip,
            seq: 0,
            capture_out: false,
            capture_timer: None,
            last_focus_capture: None,
            held_toast: prepared.toast,
            told_handoffs: BTreeSet::new(),
            heard_handoffs: BTreeSet::new(),
            restart_after_handoff: None,
            wrote: false,
            session_from: 0,
            started_with_files: prepared.started_with_files,
            channel_dir: prepared.channel_dir,
            root: prepared.root,
            prefixes: prepared.prefixes,
            out_seq: 0,
            answered_once: false,
            told: Told::default(),
            #[cfg(test)]
            answered: Vec::new(),
        })
    }

    /// The read of the current nvim's last record, when it is still to be asked: nvim arms its handoff only as its
    /// restore ends, just before it says so, so an nvim that stood aside or never said its restore ended leaves none;
    /// and each token is read once. It takes the next sequence number, so no record asked for before it replaces what
    /// it reads.
    fn ask_handoff(&mut self) -> Option<(String, u64)> {
        if self.state != TokenState::Ended || !self.told_handoffs.insert(self.token.clone()) {
            return None;
        }
        self.seq += 1;
        Some((self.token.clone(), self.seq))
    }

    /// The capture's arguments: the token it answers for, and the root and the prefixes as base64 of their bytes.
    fn capture_args(&self) -> Value {
        let encode = |path: &PathBuf| Value::from(editor_record::encode_name(path.as_os_str().as_bytes()));
        Value::Map(vec![
            (Value::from("token"), Value::from(self.token.as_str())),
            (Value::from("root"), encode(&self.root)),
            (
                Value::from("prefixes"),
                Value::Array(self.prefixes.iter().map(encode).collect()),
            ),
        ])
    }
}

/// The start's reopen step: the window keeps what its start prepared, and the writer wakes the controller.
pub(super) fn install(w: &mut Window, prepared: Prepared, waker: Arc<dyn Waker>) {
    if let Some(reopen) = Reopen::new(prepared) {
        reopen.writer.set_waker(waker);
        w.reopen = Some(reopen);
    }
}

/// The window is shown: what the start had to say goes on its toast. Whether anything was shown.
pub(super) fn present(w: &mut Window) -> bool {
    let Some(reopen) = w.reopen.as_mut() else {
        return false;
    };
    let Some(text) = reopen.held_toast.take() else {
        return false;
    };
    // A skipped restore is said because files may seem lost; a record set aside is only a notice.
    let kind = if reopen.skip {
        ToastKind::Warning
    } else {
        ToastKind::Other
    };
    chrome::show_toast(w, text, kind);
    true
}

/// A line the editor sent about its restore.
pub(super) fn line(w: &mut Window, win: WinId, line: ReopenLine, batch: &mut Batch) {
    let Some(reopen) = w.reopen.as_mut() else {
        return;
    };
    let (kind, token) = match &line {
        ReopenLine::Began { token } => ("B", token),
        ReopenLine::Ended { token, .. } => ("O", token),
        ReopenLine::Name { token, .. } => ("N", token),
        ReopenLine::Aside { token, .. } => ("A", token),
    };
    if *token != reopen.token {
        w.say(format!("[editor] reopen: dropped {kind} for an earlier nvim"));
        return;
    }
    match (line, reopen.state) {
        (ReopenLine::Began { token }, TokenState::Started) => {
            reopen.state = TokenState::Began;
            if !reopen.skip {
                reopen.writer.set_marker(token);
            }
        }
        (ReopenLine::Aside { word, .. }, TokenState::Started) => {
            reopen.state = TokenState::Aside;
            w.say(format!("[editor] reopen: standing aside for {}", word.as_str()));
        }
        (
            ReopenLine::Ended {
                opened,
                gone,
                failed,
                squeezed,
                micros,
                ..
            },
            TokenState::Began,
        ) => {
            reopen.state = TokenState::Ended;
            reopen.told = Told {
                gone,
                failed,
                squeezed,
                ..Told::default()
            };
            w.say(format!(
                "[editor] reopen: {opened} opened, {gone} gone, {failed} failed, {squeezed} did not fit, in {micros} us"
            ));
            show_told(w);
            ask_capture(w, win, batch);
        }
        (ReopenLine::Name { gone, name, .. }, TokenState::Ended) => {
            let told = &mut reopen.told;
            let (count, slot) = if gone {
                (told.gone, &mut told.gone_name)
            } else {
                (told.failed, &mut told.failed_name)
            };
            if count == 0 || slot.is_some() {
                w.say(format!(
                    "[editor] reopen: dropped N {name:?}: no such file was counted, or one is named"
                ));
                return;
            }
            *slot = Some(name);
            show_told(w);
        }
        (line, state) => {
            let kind = match line {
                ReopenLine::Began { .. } => "B",
                ReopenLine::Ended { .. } => "O",
                ReopenLine::Name { .. } => "N",
                ReopenLine::Aside { .. } => "A",
            };
            w.say(format!(
                "[editor] reopen: dropped {kind} while the restore is {state:?}"
            ));
        }
    }
}

/// Shows what the restore's end said, whole, on one toast: each part as its count and its first name have come.
fn show_told(w: &mut Window) {
    let Some(reopen) = w.reopen.as_ref() else {
        return;
    };
    let told = &reopen.told;
    let parts: Vec<String> = [
        told.gone_name
            .as_deref()
            .map(|name| super::feedback::reopen_gone_text(told.gone, name)),
        told.failed_name.as_deref().map(super::feedback::reopen_failed_text),
        (told.squeezed > 0).then(|| super::feedback::reopen_squeezed_text(told.squeezed)),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !parts.is_empty() {
        chrome::show_toast(w, parts.join("; "), ToastKind::Other);
    }
}

/// Asks for a capture when the restore has ended, nvim runs and none is out. Whether one was asked.
fn ask_capture(w: &mut Window, win: WinId, batch: &mut Batch) -> bool {
    if w.editor_run != EditorRun::Running {
        return false;
    }
    let Some(reopen) = w.reopen.as_mut() else {
        return false;
    };
    if reopen.state != TokenState::Ended || reopen.capture_out {
        return false;
    }
    reopen.seq += 1;
    reopen.out_seq = reopen.seq;
    reopen.capture_out = true;
    batch.push_effect(HostOp::Editor(EditorCall::Capture {
        win,
        token: reopen.token.clone(),
        args: reopen.capture_args(),
    }));
    // The next timed capture is armed when this one answers.
    if let Some(armed) = reopen.capture_timer.take() {
        w.cancel_timer(armed);
    }
    true
}

/// The answer to the capture asked with `token`. Whatever it is, the next timed capture is armed from now.
pub(super) fn captured(w: &mut Window, token: String, reply: Result<Option<(String, bool)>, String>) {
    let Some(reopen) = w.reopen.as_mut() else {
        return;
    };
    if token != reopen.token || !reopen.capture_out {
        w.say("[editor] reopen: dropped a capture's answer for an earlier nvim".into());
        return;
    }
    reopen.capture_out = false;
    let seq = reopen.out_seq;
    let refused = match reply {
        Ok(Some((json, same))) => {
            if !std::mem::replace(&mut reopen.answered_once, true) && !reopen.skip {
                reopen.writer.clear_marker_if(token);
            }
            match editor_record::parse(&json, &reopen.root) {
                Ok(record) => {
                    reopen.writer.offer(seq, record, same);
                    None
                }
                Err(e) => Some(format!("[editor] reopen: the capture's record was refused: {e}")),
            }
        }
        Ok(None) => None,
        Err(why) => Some(format!("[editor] reopen: the capture failed: {why}")),
    };
    if let Some(line) = refused {
        w.warn(line);
    }
    let next = w.arm_after(CAPTURE_EVERY);
    if let Some(reopen) = w.reopen.as_mut() {
        reopen.capture_timer = Some(next);
    }
}

/// A timer fired: when it is the timed capture's, the capture is asked; when it is a held restart's, the restart
/// goes on without what it waited for. `false` when `token` is neither.
pub(super) fn timer(w: &mut Window, win: WinId, token: TimerToken, batch: &mut Batch) -> bool {
    let Some(reopen) = w.reopen.as_mut() else {
        return false;
    };
    if reopen.capture_timer == Some(token) {
        reopen.capture_timer = None;
        ask_capture(w, win, batch);
        return true;
    }
    let held = match reopen.restart_after_handoff.take() {
        Some(held) if held.timer() == token => held,
        other => {
            reopen.restart_after_handoff = other;
            return false;
        }
    };
    match held {
        Held::Handoff { ended, .. } => {
            w.say(format!(
                "[editor] reopen: no word of nvim {ended}'s last record in time; restarting from the newest one held"
            ));
            restart_input(w);
        }
        Held::Input { token, .. } => {
            w.warn(format!(
                "[editor] reopen: the input of nvim {token} was not written in time; it restarts without it"
            ));
            // The ended nvim's input is still there, and would hand the new one a token the window no longer hears.
            batch.push_effect(HostOp::Editor(EditorCall::DropInput { win }));
            release_restart(w, win, batch);
        }
    }
    true
}

/// The window lost the keyboard: a capture, unless one was asked for this within [`FOCUS_CAPTURE_EVERY`].
pub(super) fn focus_out(w: &mut Window, win: WinId, batch: &mut Batch) {
    let now = w.now();
    let Some(reopen) = w.reopen.as_ref() else {
        return;
    };
    if reopen
        .last_focus_capture
        .is_some_and(|at| now.saturating_duration_since(at) < FOCUS_CAPTURE_EVERY)
    {
        return;
    }
    if ask_capture(w, win, batch) {
        if let Some(reopen) = w.reopen.as_mut() {
            reopen.last_focus_capture = Some(now);
        }
    }
}

/// nvim ended and the window lives on (`:qa`, `ZZ`, a crash, a kill of the editor alone): the writer is told to read
/// the last record it left, so that record is kept now rather than at the next restart or the close.
pub(super) fn nvim_ended(w: &mut Window, win: WinId, batch: &mut Batch) {
    let Some(reopen) = w.reopen.as_mut() else {
        return;
    };
    if let Some((token, seq)) = reopen.ask_handoff() {
        batch.push_effect(HostOp::Editor(EditorCall::ReadHandoff { win, token, seq }));
    }
}

/// The teardown's op for the editor's record, between nvim's shutdown and the channel directory's removal: it reads
/// the last nvim's record unless that was asked already, and waits for the writer. `None` when the window keeps no
/// record. A restart still held goes with the window.
pub(super) fn teardown_op(w: &mut Window) -> Option<HostOp> {
    let reopen = w.reopen.as_mut()?;
    let held = reopen.restart_after_handoff.take();
    // The input was written for an nvim that never started, which leaves nothing to read.
    if matches!(held, Some(Held::Input { .. })) {
        reopen.told_handoffs.insert(reopen.token.clone());
    }
    let read = reopen.ask_handoff();
    let seq = read.as_ref().map_or(reopen.seq, |(_, seq)| *seq);
    if let Some(held) = held {
        w.cancel_timer(held.timer());
    }
    Some(HostOp::Cleanup(CleanupOp::EditorRecord {
        token: read.map(|(token, _)| token),
        seq,
    }))
}

/// A restart of an ended nvim: held until the writer has answered about the ended nvim's last record and has written
/// the new nvim's input from the newest record it holds, each wait at most [`RESTART_HOLD`]. Whether it is held;
/// when not (the window keeps no record), the restart goes now.
pub(super) fn hold_restart(w: &mut Window, win: WinId, batch: &mut Batch) -> bool {
    let Some(reopen) = w.reopen.as_mut() else {
        return false;
    };
    let ended = reopen.token.clone();
    if let Some((token, seq)) = reopen.ask_handoff() {
        batch.push_effect(HostOp::Editor(EditorCall::ReadHandoff { win, token, seq }));
    }
    // An nvim whose restore never ended left nothing to wait for.
    let heard = reopen.state != TokenState::Ended || reopen.heard_handoffs.contains(&ended);
    if let Some(stale) = reopen.restart_after_handoff.take() {
        w.cancel_timer(stale.timer());
    }
    if heard {
        restart_input(w);
    } else {
        let timer = w.arm_after(RESTART_HOLD);
        if let Some(reopen) = w.reopen.as_mut() {
            reopen.restart_after_handoff = Some(Held::Handoff { ended, timer });
        }
    }
    true
}

/// The ended nvim's last record is with the writer, or its wait is over: the new nvim gets a token and its input,
/// written by the writer from the newest record once everything handed to it before is done. Its restore is skipped
/// when the ended nvim's never said it ended, or when the last one that did not finish has had nothing written since.
fn restart_input(w: &mut Window) {
    let Some(reopen) = w.reopen.as_mut() else {
        return;
    };
    let skip = reopen.state == TokenState::Began || (reopen.skip && !reopen.wrote);
    let token = crate::editor_reopen::new_token();
    reopen.token = token.clone();
    reopen.state = TokenState::Started;
    reopen.skip = skip;
    reopen.wrote = false;
    reopen.session_from = reopen.seq;
    reopen.capture_out = false;
    reopen.answered_once = false;
    reopen.told = Told::default();
    let capture_timer = reopen.capture_timer.take();
    reopen.writer.begin_session(token.clone(), skip);
    if let Some(channel_dir) = &reopen.channel_dir {
        reopen
            .writer
            .write_input(channel_dir.clone(), token.clone(), reopen.prefixes.clone(), skip);
    }
    if let Some(armed) = capture_timer {
        w.cancel_timer(armed);
    }
    let timer = w.arm_after(RESTART_HOLD);
    if let Some(reopen) = w.reopen.as_mut() {
        reopen.restart_after_handoff = Some(Held::Input { token, timer });
    }
    if skip {
        w.say("[editor] reopen: the last restore did not finish; the restarted nvim restores nothing".into());
        chrome::show_toast(w, super::feedback::reopen_unfinished_text(), ToastKind::Warning);
    }
}

/// The held restart goes, unless the editor stopped restarting meanwhile (killed, or the window closing).
fn release_restart(w: &mut Window, win: WinId, batch: &mut Batch) {
    if w.editor_run == EditorRun::Restarting {
        super::verbs::modules::send_restart(w, win, batch);
    } else {
        w.say(format!(
            "[editor] reopen: a held restart was dropped; the editor is {:?} now",
            w.editor_run
        ));
    }
}

/// The writer woke the controller: takes what it answered. A write that failed is the writer's to report, through
/// the save health the panel shows.
pub(super) fn collect(w: &mut Window, win: WinId, batch: &mut Batch) {
    let Some(reopen) = w.reopen.as_ref() else {
        return;
    };
    for answer in reopen.writer.take_answers() {
        let Some(reopen) = w.reopen.as_mut() else {
            return;
        };
        #[cfg(test)]
        reopen.answered.push(answer.clone());
        match answer {
            WriterAnswer::Written { seq } => {
                if seq > reopen.session_from {
                    reopen.wrote = true;
                }
            }
            WriterAnswer::Handoff { token, .. } => {
                reopen.heard_handoffs.insert(token.clone());
                if let Some(Held::Handoff { ended, timer }) = &reopen.restart_after_handoff {
                    if *ended == token {
                        let timer = *timer;
                        reopen.restart_after_handoff = None;
                        w.cancel_timer(timer);
                        restart_input(w);
                    }
                }
            }
            WriterAnswer::InputWritten { token } => {
                if let Some(Held::Input { token: held, timer }) = &reopen.restart_after_handoff {
                    if *held == token {
                        let timer = *timer;
                        reopen.restart_after_handoff = None;
                        w.cancel_timer(timer);
                        release_restart(w, win, batch);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
