//! The review flow: what a window does with the panel's revert, undo, comment, send and recovery
//! requests. It owns every request still in flight and answers each one exactly once.
//!
//! GTK-free, and it never waits. Disk and git work (a write, an anchor, a message check, the
//! journal) runs on a worker thread of its own, and the editor's answers are [`Pending`]s taken
//! with `try_take` on the window's tick, so the GTK thread is only ever asked to read memory.
//!
//! **What this file adds to the engine's own safety checks.** A write job cannot be built without
//! a presence guard, the editor's word for that very path and a flag that says the request is still
//! wanted ([`super::revert`]). This file makes those three truthfully and on time:
//!
//! - the flag is a [`ReviewTicket`], cancelled the moment its tab closes and checked on every
//!   tick, and again when the editor's answer arrives, because that answer may belong to a tab,
//!   a session or an editor that is no longer the one asked;
//! - one write is in flight at a time, from the editor question until the worker has returned,
//!   so two questions cannot each clear a different write;
//! - nothing here reads review state on a permission or an ordinary send path: the only message
//!   this file ever sends is the confirmed review, in [`confirmed_send`].
//!
//! **The editor overlay.** The flow also drives the window's [`EditorOverlay`]: `o` on a file opens
//! it in the user's nvim with the turn's hunks drawn over it, a hunk the user reverts there comes
//! back as a revert of the draft, and the panel showing another turn for a drawn file redraws it.
//! The flow never types into the editor: everything it asks is one of the overlay's own calls.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::agent_bridge::{
    serialize_command_ok_with_message_for_js, serialize_command_result_for_js, serialize_review_draft_for_js,
    serialize_review_recovery_for_js, serialize_review_send_preview_for_js, InboundMessage, ReviewRecoverAnswerWire,
    ReviewRevertTargetWire,
};
use crate::editor_rpc::EditorRpc;
use crate::nvim_rpc::Pending;
use crate::review_editor::{
    overlay_hunks, EditorOverlay, EditorRevertEvent, OffEvent, OverlayCall, OverlayOutcome, Owner, ShowHunk, ShowMeta,
};
use crate::tab_set::{Flush, ReviewTicket, Tab, TabBackend, TabSet};
use crate::tabs::TabId;

use super::draft::{check_reverts, NewRevert, RevertShape, RevertSource, ReviewDraft, UndoData, UndoState};
use super::journal::JournalEntry;
use super::lifecycle::{ReviewError, Scope};
use super::message::{compose, Preview};
use super::presence::{Blocked, PresenceGuard};
use super::revert::{
    buffer_state_args, parse_buffer_state, AppliedRevert, EditorCheck, EditorClear, ExactHunk, ExactHunks,
    RecoverAnswer, Refusal, RevertKind, RevertTarget, Saved, BUFFER_STATE_LUA,
};

/// How long the editor has to say what a buffer holds for a message's check. Past it the revert is
/// treated as not loaded, which keeps it out of the message rather than guessing.
const BUFFER_ANSWER_WAIT: Duration = Duration::from_secs(5);

/// The most comments being quoted at once.
const MAX_ANCHORS: usize = 4;

const TAB_CLOSED: &str = "the tab closed; nothing was written";
const SESSION_CHANGED: &str = "the tab's session changed; nothing was written";
const EDITOR_CHANGED: &str = "the editor changed while it was asked; nothing was written";
const WRITE_BUSY: &str = "a revert is still being written";
/// The most file preparations (for an open, or for a redraw) running at once.
const MAX_EDITOR_JOBS: usize = 4;

const NO_EDITOR_OVERLAY: &str = "this window has no editor to show the changes in";
const OVERLAY_TOO_LARGE: &str = "too large for the editor overlay";
const OVERLAY_BINARY: &str = "no overlay for a binary file";
const MESSAGE_BUSY: &str = "the review message is still being prepared; try again in a moment";

/// What the shell owes the panel after a call into the flow, in order.
pub enum Out {
    /// A JSON envelope to dispatch to the panel.
    Envelope(String),
    /// A confirmed review went out (or was refused by the backend and left queued): the shell
    /// applies it as it does any flush of a tab's queue, and adds nothing to the prompt history.
    Flushed(TabId, Flush),
    /// A confirmed review was put in the tab's queue behind its running turn: the shell sends the
    /// queue to the page, as it does when a typed message is queued, so the queue strip shows it.
    QueueChanged(TabId),
    /// The editor opened a file for the user: the shell shows it and gives it the keys.
    EditorOpened,
}

/// One window's review requests in flight.
#[derive(Default)]
pub struct ReviewFlow {
    /// The one revert, undo or recovery being written, from its editor question until its worker
    /// has returned.
    write: Option<WriteWork>,
    anchors: Vec<AnchorWork>,
    sends: Vec<SendWork>,
    /// The interrupted reverts last found, which a `review_recover` must name one of.
    recoveries: Vec<JournalEntry>,
    recovery_job: Option<mpsc::Receiver<Vec<JournalEntry>>>,
    recovery_asked: bool,
    jobs_started: usize,
    /// The window's editor overlay, once the shell has said who owns the module in it.
    overlay: Option<EditorOverlay>,
    overlay_owner: Option<Owner>,
    /// Files being read (hunks and path) for an open or for a redraw.
    opens: Vec<OpenWork>,
    redraws: Vec<RedrawWork>,
    /// The opens the editor has not answered yet, each with what to add to its answer.
    awaiting: BTreeMap<String, Option<String>>,
    /// What each file drawn over in the editor shows, to know when the panel moves to another turn.
    shown: BTreeMap<PathBuf, Shown>,
    /// Answers decided between two calls (an owner change failing the opens it dropped).
    early: Vec<Out>,
}

/// A file prepared on a worker: its resolved absolute path and the turn's hunks over it.
type Prepared = Result<(PathBuf, Result<ExactHunks, ReviewError>), String>;

struct OpenWork {
    request_id: String,
    tab: TabId,
    session: String,
    turn: u32,
    scope: Scope,
    line: Option<u32>,
    rx: mpsc::Receiver<Prepared>,
}

struct RedrawWork {
    /// The project-relative path the panel asked about: a newer request for it replaces this one.
    path: String,
    tab: TabId,
    session: String,
    turn: u32,
    scope: Scope,
    rx: mpsc::Receiver<Prepared>,
}

struct Shown {
    meta: ShowMeta,
    /// Each drawn hunk's `@@` header, which the editor does not carry back.
    headers: BTreeMap<u32, String>,
}

struct WriteWork {
    request_id: String,
    /// `None` for a recovery, which belongs to the project and to no tab.
    ticket: Option<ReviewTicket>,
    /// The editor the question went to: an answer is used only while it is still this one.
    target_at_ask: Option<u64>,
    guard: PresenceGuard,
    op: Op,
    stage: Stage,
}

enum Op {
    Revert {
        turn: u32,
        scope: Scope,
        path: String,
        target: RevertTarget,
    },
    Undo {
        record: u32,
        path: String,
        pre: Saved,
        post: Saved,
    },
    Restore {
        entry: JournalEntry,
    },
    Dismiss {
        entry: JournalEntry,
    },
}

enum Stage {
    Checking(EditorCheck),
    Running(mpsc::Receiver<Done>),
}

enum Done {
    Revert(Result<AppliedRevert, Refusal>),
    Undo(Result<(), Refusal>),
    Recover(Result<(), Refusal>),
}

struct AnchorWork {
    request_id: String,
    tab: TabId,
    session: String,
    turn: u32,
    path: String,
    from: u32,
    to: u32,
    text: String,
    rx: mpsc::Receiver<Result<Vec<String>, ReviewError>>,
}

struct SendWork {
    request_id: String,
    tab: TabId,
    session: String,
    /// The digest the user confirmed, if this is the confirmation.
    confirm: Option<String>,
    /// The draft as it was when the request came in: what the message is made from, and what the
    /// tab's draft must still equal when the answer is used.
    draft: ReviewDraft,
    stage: SendStage,
}

enum SendStage {
    /// Waiting for what the editor holds of each revert made there.
    Asking {
        waiting: Vec<(u32, u32, Pending)>,
        known: BTreeMap<u32, Option<Vec<u8>>>,
        started: Instant,
        root: std::path::PathBuf,
        latest_turn: u32,
    },
    /// The statuses are being read from the disk and the message composed, on a worker.
    Composing(mpsc::Receiver<Preview>),
}

impl ReviewFlow {
    pub fn new() -> ReviewFlow {
        ReviewFlow::default()
    }

    /// Nothing is in flight, so the tick has nothing to poll.
    pub fn is_idle(&self) -> bool {
        self.write.is_none()
            && self.anchors.is_empty()
            && self.sends.is_empty()
            && self.recovery_job.is_none()
            && self.opens.is_empty()
            && self.redraws.is_empty()
            && self.early.is_empty()
            && !self.overlay.as_ref().is_some_and(EditorOverlay::wants_ticks)
    }

    /// Who owns the review module in this window's editor: the integrated window's own nvim, or the
    /// companion panel's channel. A different owner is a different editor, so the driver that
    /// served the old one is dropped and the opens still waiting on it fail. Called before every
    /// request and tick, because a companion's channel changes with each attach.
    pub fn set_editor_owner(&mut self, owner: Option<Owner>) {
        if owner == self.overlay_owner {
            return;
        }
        self.overlay_owner = owner;
        self.overlay = owner.map(EditorOverlay::new);
        self.shown.clear();
        self.redraws.clear();
        // A file still being read was asked for the old editor; sending it to the new one would show
        // a request nobody made of it.
        self.fail_opens("the editor changed before the file was ready; nothing was opened");
        self.fail_awaiting("the editor changed before it answered; nothing was opened");
    }

    /// The editor is gone, or its panel's drafts were dropped: the driver forgets what it installed
    /// and drew, and every open that was waiting for an answer is told it will not come.
    pub fn editor_lost(&mut self) -> Vec<Out> {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.editor_lost();
        }
        self.shown.clear();
        self.redraws.clear();
        self.fail_opens("the editor went away before the file was ready; nothing was opened");
        self.fail_awaiting("the editor went away before it answered; nothing was opened");
        std::mem::take(&mut self.early)
    }

    /// Fails the opens whose file is still being read. Their workers finish into a closed channel.
    fn fail_opens(&mut self, why: &str) {
        for work in std::mem::take(&mut self.opens) {
            self.early.push(failed(&work.request_id, why));
        }
    }

    fn fail_awaiting(&mut self, why: &str) {
        for (request_id, _) in std::mem::take(&mut self.awaiting) {
            self.early.push(failed(&request_id, why));
        }
    }

    /// The panel asked for another turn's diff of `path`. When the editor draws that file's review,
    /// the new turn replaces the old one; nothing is read when no file is drawn.
    pub fn panel_shows(&mut self, tabs: &TabSet, tab: TabId, turn: u32, scope: Scope, path: &str) {
        let drawn = self.overlay.as_ref().is_some_and(|o| !o.active_paths().is_empty());
        if !drawn || check_relative(path).is_err() {
            return;
        }
        // The newest request for a file is the one the panel shows; an older one still being read
        // must not land after it and draw the turn the panel has left.
        self.redraws.retain(|work| work.path != path);
        if self.redraws.len() >= MAX_EDITOR_JOBS {
            return;
        }
        let Ok((review, session)) = tabs.review_session(tab) else {
            return;
        };
        let root = review.project_root().to_path_buf();
        let job = review.exact_hunks_job(&session, turn, scope, path);
        let worker_path = path.to_string();
        if let Ok(rx) = spawn("eitri-review-redraw", move || prepare_file(&root, &worker_path, job)) {
            self.jobs_started += 1;
            self.redraws.push(RedrawWork {
                path: path.to_string(),
                tab,
                session,
                turn,
                scope,
                rx,
            });
        }
    }

    /// How many workers this flow has started: a request refused before its worker was built
    /// leaves it unchanged.
    pub fn jobs_started(&self) -> usize {
        self.jobs_started
    }

    /// One of the panel's review messages. `rpc` is the window's editor, if it has one. The answer
    /// to a request that could be refused at once comes back in the result; the rest follows on
    /// later [`tick`](Self::tick)s.
    pub fn handle(
        &mut self,
        message: InboundMessage,
        tabs: &mut TabSet,
        rpc: Option<&dyn EditorRpc>,
        now: Instant,
    ) -> Vec<Out> {
        let mut out = Vec::new();
        let request_id = message.request_id().to_string();
        // Resolved again here although the shell did it: every tab-scoped request names its tab, and
        // a flow that is called without the shell must not fall back to "the active one".
        let tab = match tabs.resolve(message.tab_ref()) {
            Ok(tab) => tab,
            Err(why) => {
                out.push(failed(&request_id, &why));
                return out;
            }
        };
        let named = |tab: Option<TabId>| tab.ok_or_else(|| "protocol: this command names no tab".to_string());
        let result = match message {
            InboundMessage::ReviewRevert {
                turn,
                scope,
                path,
                target,
                ..
            } => named(tab).and_then(|tab| {
                let target = match target {
                    ReviewRevertTargetWire::Hunk { id, header } => RevertTarget::Hunk { id, header },
                    ReviewRevertTargetWire::File => RevertTarget::File,
                };
                self.start_revert(tabs, rpc, now, &request_id, tab, turn, scope.into(), path, target)
            }),
            InboundMessage::ReviewUndo { .. } => {
                named(tab).and_then(|tab| self.start_undo(tabs, rpc, now, &request_id, tab))
            }
            InboundMessage::ReviewCommentAdd {
                turn,
                scope,
                path,
                from,
                to,
                text,
                ..
            } => named(tab)
                .and_then(|tab| self.start_comment(tabs, &request_id, tab, turn, scope.into(), path, from, to, text)),
            InboundMessage::ReviewCommentRemove { id, .. } => named(tab).and_then(|tab| {
                tabs.review_session(tab)?;
                tabs.review_draft_mut(tab)?.remove_comment(id)?;
                out.extend(draft_envelope(Some(&request_id), tab, tabs));
                Ok(())
            }),
            InboundMessage::ReviewSend { confirm, .. } => {
                named(tab).and_then(|tab| self.start_send(tabs, rpc, now, &request_id, tab, confirm))
            }
            InboundMessage::ReviewRecover { entry, answer, .. } => {
                self.start_recover(tabs, rpc, now, &request_id, &entry, answer)
            }
            InboundMessage::OpenInEditor {
                turn,
                scope,
                path,
                line,
                ..
            } => {
                named(tab).and_then(|tab| self.start_open(tabs, rpc, &request_id, tab, turn, scope.into(), path, line))
            }
            _ => {
                debug_assert!(false, "a message that is not the review flow's reached it");
                Err("this is not a review request".to_string())
            }
        };
        if let Err(why) = result {
            out.push(failed(&request_id, &why));
        }
        // A request that can already be answered (no editor, a refusal) is, without waiting for the
        // next tick.
        out.extend(self.tick(tabs, rpc, now));
        out
    }

    /// Moves every request in flight as far as it can go without waiting.
    pub fn tick(&mut self, tabs: &mut TabSet, rpc: Option<&dyn EditorRpc>, now: Instant) -> Vec<Out> {
        let mut out = std::mem::take(&mut self.early);
        self.advance_write(tabs, rpc, now, &mut out);
        self.advance_anchors(tabs, &mut out);
        self.advance_sends(tabs, now, &mut out);
        self.advance_recovery_job(&mut out);
        self.advance_opens(tabs, rpc, now, &mut out);
        self.advance_redraws(tabs, rpc, now);
        self.advance_overlay(tabs, rpc, now, &mut out);
        out
    }

    /// The panel's document is ready (the first time, or after a reload): offers the interrupted
    /// reverts. The journal is read once per window, on a worker; a reload re-sends what was found
    /// and reads nothing, because the page is only a view.
    pub fn document_ready(&mut self, tabs: &TabSet) -> Vec<Out> {
        if !self.recovery_asked {
            // Asked for only once a review exists: a window whose review is not installed yet (or
            // is off) reads no journal, and asks at the first ready after one is.
            if let Some(review) = tabs.turn_review() {
                self.recovery_asked = true;
                let job = review.recoveries_job();
                if let Ok(rx) = spawn("eitri-review-journal", move || job.run()) {
                    self.jobs_started += 1;
                    self.recovery_job = Some(rx);
                }
            }
            return Vec::new();
        }
        if self.recoveries.is_empty() {
            Vec::new()
        } else {
            vec![Out::Envelope(serialize_review_recovery_for_js(&self.recoveries))]
        }
    }

    // ---- the write slot ------------------------------------------------------------------------

    /// Refuses a write while another is out or a message is being made from the disk.
    fn ensure_free(&self) -> Result<(), String> {
        if self.write.is_some() {
            return Err(WRITE_BUSY.to_string());
        }
        if !self.sends.is_empty() {
            return Err(MESSAGE_BUSY.to_string());
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn start_revert(
        &mut self,
        tabs: &mut TabSet,
        rpc: Option<&dyn EditorRpc>,
        now: Instant,
        request_id: &str,
        tab: TabId,
        turn: u32,
        scope: Scope,
        path: String,
        target: RevertTarget,
    ) -> Result<(), String> {
        let root = tabs.review_session(tab)?.0.project_root().to_path_buf();
        self.ensure_free()?;
        check_relative(&path)?;
        let guard = may_write(tabs)?;
        let ticket = tabs.review_ticket(tab)?;
        let check = EditorCheck::start(rpc, &root.join(&path), now);
        self.write = Some(WriteWork {
            request_id: request_id.to_string(),
            ticket: Some(ticket),
            target_at_ask: rpc.and_then(|rpc| rpc.target()),
            guard,
            op: Op::Revert {
                turn,
                scope,
                path,
                target,
            },
            stage: Stage::Checking(check),
        });
        Ok(())
    }

    fn start_undo(
        &mut self,
        tabs: &mut TabSet,
        rpc: Option<&dyn EditorRpc>,
        now: Instant,
        request_id: &str,
        tab: TabId,
    ) -> Result<(), String> {
        let root = tabs.review_session(tab)?.0.project_root().to_path_buf();
        self.ensure_free()?;
        let record = tabs
            .review_draft(tab)?
            .last_undoable()
            .ok_or_else(|| "nothing to undo".to_string())?;
        let id = record.id;
        let path = record.new.path.clone();
        // A panel revert always carries what undoes it; a record that does not is refused rather
        // than guessed at.
        let undo = record
            .new
            .undo
            .clone()
            .ok_or_else(|| "this revert cannot be undone from here".to_string())?;
        check_relative(&path)?;
        let guard = may_write(tabs)?;
        let ticket = tabs.review_ticket(tab)?;
        let check = EditorCheck::start(rpc, &root.join(&path), now);
        self.write = Some(WriteWork {
            request_id: request_id.to_string(),
            ticket: Some(ticket),
            target_at_ask: rpc.and_then(|rpc| rpc.target()),
            guard,
            op: Op::Undo {
                record: id,
                path,
                pre: undo_to_saved(undo.pre),
                post: undo_to_saved(undo.post),
            },
            stage: Stage::Checking(check),
        });
        Ok(())
    }

    fn start_recover(
        &mut self,
        tabs: &mut TabSet,
        rpc: Option<&dyn EditorRpc>,
        now: Instant,
        request_id: &str,
        entry_id: &str,
        answer: ReviewRecoverAnswerWire,
    ) -> Result<(), String> {
        if let Some(why) = tabs.review_unavailable() {
            return Err(why.to_string());
        }
        let entry = self
            .recoveries
            .iter()
            .find(|e| e.id == entry_id)
            .cloned()
            .ok_or_else(|| "this interrupted revert is not on offer".to_string())?;
        self.ensure_free()?;
        match answer {
            ReviewRecoverAnswerWire::Dismiss => {
                let guard = tabs
                    .presence_guard()
                    .ok_or_else(|| Blocked::NotHeld("no state directory".to_string()).to_string())?;
                // A dismissal writes no file, so there is no editor to ask: it runs at once.
                let op = Op::Dismiss { entry };
                let rx = self.build_job(&op, &guard, None, None, tabs)?;
                self.jobs_started += 1;
                self.write = Some(WriteWork {
                    request_id: request_id.to_string(),
                    ticket: None,
                    target_at_ask: None,
                    guard,
                    op,
                    stage: Stage::Running(rx),
                });
            }
            ReviewRecoverAnswerWire::Restore => {
                let root = tabs
                    .turn_review()
                    .ok_or_else(|| TURN_REVIEW_UNAVAILABLE.to_string())?
                    .project_root()
                    .to_path_buf();
                check_relative(&entry.path.to_string_lossy())?;
                let guard = may_write(tabs)?;
                let check = EditorCheck::start(rpc, &root.join(&entry.path), now);
                self.write = Some(WriteWork {
                    request_id: request_id.to_string(),
                    ticket: None,
                    target_at_ask: rpc.and_then(|rpc| rpc.target()),
                    guard,
                    op: Op::Restore { entry },
                    stage: Stage::Checking(check),
                });
            }
        }
        Ok(())
    }

    /// Polls the write in flight. The ticket is looked at first, every time: a tab that closed or
    /// changed session cancels the request wherever it is.
    fn advance_write(&mut self, tabs: &mut TabSet, rpc: Option<&dyn EditorRpc>, now: Instant, out: &mut Vec<Out>) {
        let Some(mut work) = self.write.take() else {
            return;
        };
        let stale = work.ticket.as_ref().and_then(|ticket| stale_reason(tabs, ticket));
        if stale.is_some() {
            if let Some(ticket) = &work.ticket {
                ticket.cancel();
            }
        }
        enum Polled {
            Check(Option<Result<EditorClear, Refusal>>),
            Run(Result<Done, mpsc::TryRecvError>),
        }
        let polled = match &mut work.stage {
            // Never built when stale: there is no job to cancel, and nothing may be asked of one.
            Stage::Checking(_) if stale.is_some() => {
                out.push(failed(&work.request_id, stale.unwrap_or(TAB_CLOSED)));
                return;
            }
            Stage::Checking(check) => Polled::Check(check.poll(now)),
            // A request that went stale while its worker runs is answered by the worker's own
            // result: if its last check came before the cancel, the write landed and must be said.
            Stage::Running(rx) => Polled::Run(rx.try_recv()),
        };
        match polled {
            Polled::Check(None) => self.write = Some(work),
            Polled::Check(Some(Err(refusal))) => out.push(failed(&work.request_id, &refusal.to_string())),
            Polled::Check(Some(Ok(clear))) => {
                if let Some(why) = cleared_but_not_wanted(&work, tabs, rpc) {
                    out.push(failed(&work.request_id, &why));
                    return;
                }
                match self.build_job(&work.op, &work.guard, work.ticket.as_ref(), Some(clear), tabs) {
                    Ok(rx) => {
                        self.jobs_started += 1;
                        work.stage = Stage::Running(rx);
                        self.write = Some(work);
                    }
                    Err(why) => out.push(failed(&work.request_id, &why)),
                }
            }
            Polled::Run(Err(mpsc::TryRecvError::Empty)) => self.write = Some(work),
            Polled::Run(Err(mpsc::TryRecvError::Disconnected)) => out.push(failed(
                &work.request_id,
                "the write stopped before it said how it ended; check the file before trying again",
            )),
            Polled::Run(Ok(done)) => self.finish_write(work, done, tabs, out),
        }
    }

    /// Builds the job for `op` and starts its worker. `clear` is the editor's word for the file; a
    /// dismissal has none and a write cannot be built without one.
    fn build_job(
        &self,
        op: &Op,
        guard: &PresenceGuard,
        ticket: Option<&ReviewTicket>,
        clear: Option<EditorClear>,
        tabs: &TabSet,
    ) -> Result<mpsc::Receiver<Done>, String> {
        let no_clear = || "internal: a write was started without the editor's word".to_string();
        let no_ticket = || "internal: a write was started without a ticket".to_string();
        match op {
            Op::Revert {
                turn,
                scope,
                path,
                target,
            } => {
                let (ticket, clear) = (ticket.ok_or_else(no_ticket)?, clear.ok_or_else(no_clear)?);
                let (review, _) = tabs.review_session(ticket.tab)?;
                let job = review.revert_job(
                    guard.clone(),
                    ticket.wanted.clone(),
                    &ticket.session,
                    *turn,
                    *scope,
                    path,
                    target.clone(),
                    clear,
                );
                spawn("eitri-review-write", move || Done::Revert(job.run()))
            }
            Op::Undo { path, pre, post, .. } => {
                let (ticket, clear) = (ticket.ok_or_else(no_ticket)?, clear.ok_or_else(no_clear)?);
                let (review, _) = tabs.review_session(ticket.tab)?;
                let job = review.undo_job(
                    guard.clone(),
                    ticket.wanted.clone(),
                    &ticket.session,
                    path,
                    pre.clone(),
                    post.clone(),
                    clear,
                );
                spawn("eitri-review-write", move || Done::Undo(job.run()))
            }
            Op::Restore { entry } => {
                let clear = clear.ok_or_else(no_clear)?;
                let review = tabs.turn_review().ok_or_else(|| TURN_REVIEW_UNAVAILABLE.to_string())?;
                let job = review.recover_job(guard.clone(), &entry.id, RecoverAnswer::Restore(clear));
                spawn("eitri-review-write", move || Done::Recover(job.run()))
            }
            Op::Dismiss { entry } => {
                let review = tabs.turn_review().ok_or_else(|| TURN_REVIEW_UNAVAILABLE.to_string())?;
                let job = review.recover_job(guard.clone(), &entry.id, RecoverAnswer::Dismiss);
                spawn("eitri-review-write", move || Done::Recover(job.run()))
            }
        }
    }

    /// What the worker returned: the answer to the request, and the draft's record of it. Reached
    /// for every worker that ran, including one whose request went stale meanwhile.
    fn finish_write(&mut self, work: WriteWork, done: Done, tabs: &mut TabSet, out: &mut Vec<Out>) {
        let id = work.request_id.as_str();
        // The tab is still the one asked about: it exists and holds the session of the ticket.
        let same_tab = work.ticket.as_ref().is_some_and(|ticket| {
            tabs.get(ticket.tab)
                .and_then(Tab::provider_session_id)
                .is_some_and(|session| session == ticket.session)
        });
        let tab = work.ticket.as_ref().map(|ticket| ticket.tab);
        match (work.op, done) {
            (Op::Revert { .. }, Done::Revert(Ok(applied))) => match tab {
                Some(tab) if same_tab => {
                    let record = NewRevert {
                        turn: applied.turn,
                        path: applied.path.clone(),
                        hunk: applied.hunk.clone(),
                        shape: shape_of(&applied),
                        source: RevertSource::Panel,
                        at_line: applied.at_line,
                        reverted_to: applied.reverted_to.clone(),
                        replaced: applied.replaced.clone(),
                        undo: Some(UndoData {
                            pre: saved_to_undo(applied.pre.clone()),
                            post: saved_to_undo(applied.post.clone()),
                        }),
                    };
                    match tabs.review_draft_mut(tab) {
                        Ok(draft) => {
                            draft.record_revert(record);
                            out.extend(draft_envelope(None, tab, tabs));
                            out.push(ok(id));
                        }
                        Err(_) => out.push(failed(id, NOT_RECORDED)),
                    }
                }
                _ => {
                    eprintln!(
                        "[review] {} was reverted, but its tab closed or changed session before the revert was recorded",
                        applied.path
                    );
                    out.push(failed(id, NOT_RECORDED));
                }
            },
            (Op::Undo { record, .. }, Done::Undo(Ok(()))) => {
                match tab {
                    Some(tab) if same_tab => {
                        if let Ok(draft) = tabs.review_draft_mut(tab) {
                            draft.mark_undone(record);
                        }
                        out.extend(draft_envelope(None, tab, tabs));
                        out.push(ok(id));
                    }
                    _ => {
                        eprintln!("[review] a revert was undone, but its tab closed or changed session before it was recorded");
                        out.push(failed(id, UNDO_NOT_RECORDED));
                    }
                }
            }
            (Op::Restore { entry } | Op::Dismiss { entry }, Done::Recover(Ok(()))) => {
                self.recoveries.retain(|e| e.id != entry.id);
                // Sent even when nothing is left: an empty list takes the notice away.
                out.push(Out::Envelope(serialize_review_recovery_for_js(&self.recoveries)));
                out.push(ok(id));
            }
            (_, Done::Revert(Err(refusal))) | (_, Done::Undo(Err(refusal))) | (_, Done::Recover(Err(refusal))) => {
                let why = match refusal {
                    // Its own text says the tab closed, which is not so when the tab is still here.
                    Refusal::Cancelled if work.ticket.is_some() && tab.is_some_and(|t| tabs.get(t).is_some()) => {
                        SESSION_CHANGED.to_string()
                    }
                    other => other.to_string(),
                };
                out.push(failed(id, &why));
            }
            _ => out.push(failed(id, "internal: a worker answered a different request")),
        }
    }

    // ---- the editor overlay --------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn start_open(
        &mut self,
        tabs: &mut TabSet,
        rpc: Option<&dyn EditorRpc>,
        request_id: &str,
        tab: TabId,
        turn: u32,
        scope: Scope,
        path: String,
        line: Option<u32>,
    ) -> Result<(), String> {
        let (review, session) = tabs.review_session(tab)?;
        check_openable(&path)?;
        if self.overlay.is_none() || rpc.is_none() {
            return Err(NO_EDITOR_OVERLAY.to_string());
        }
        if self.opens.len() >= MAX_EDITOR_JOBS {
            return Err("files are still being prepared for the editor; try again in a moment".to_string());
        }
        let root = review.project_root().to_path_buf();
        let job = review.exact_hunks_job(&session, turn, scope, &path);
        // What this open draws replaces whatever a panel redraw of the file was about to.
        self.redraws.retain(|work| work.path != path);
        let rx = spawn("eitri-review-open", move || prepare_file(&root, &path, job))?;
        self.jobs_started += 1;
        self.opens.push(OpenWork {
            request_id: request_id.to_string(),
            tab,
            session,
            turn,
            scope,
            line,
            rx,
        });
        Ok(())
    }

    /// Opens whose file is ready go to the editor, as one call each.
    fn advance_opens(&mut self, tabs: &TabSet, rpc: Option<&dyn EditorRpc>, now: Instant, out: &mut Vec<Out>) {
        let mut waiting = Vec::new();
        for work in std::mem::take(&mut self.opens) {
            let prepared = match work.rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {
                    waiting.push(work);
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    out.push(failed(
                        &work.request_id,
                        "the file could not be prepared for the editor",
                    ));
                    continue;
                }
                Ok(prepared) => prepared,
            };
            let id = work.request_id.as_str();
            let (abs, exact) = match prepared {
                Ok(found) => found,
                Err(why) => {
                    out.push(failed(id, &why));
                    continue;
                }
            };
            if let Some(why) = tab_gone(tabs, work.tab, &work.session, "nothing was opened") {
                out.push(failed(id, &why));
                continue;
            }
            let (Some(rpc), Some(overlay)) = (rpc, self.overlay.as_mut()) else {
                out.push(failed(id, NO_EDITOR_OVERLAY));
                continue;
            };
            let drawing = draw_plan(exact);
            let meta = ShowMeta {
                tab: work.tab.0,
                session: work.session.clone(),
                turn: work.turn,
                scope: work.scope,
            };
            match &drawing.hunks {
                Some(_) => {
                    self.shown.insert(
                        abs.clone(),
                        Shown {
                            meta: meta.clone(),
                            headers: drawing.headers,
                        },
                    );
                }
                None => {
                    self.shown.remove(&abs);
                }
            }
            overlay.call(
                rpc,
                OverlayCall::OpenAndShow {
                    request_id: work.request_id.clone(),
                    path: abs,
                    line: work.line,
                    meta,
                    hunks: drawing.hunks,
                },
                now,
            );
            self.awaiting.insert(work.request_id, drawing.note);
        }
        self.opens = waiting;
    }

    /// Files the panel moved to another turn: the new turn's hunks replace what the editor draws.
    fn advance_redraws(&mut self, tabs: &TabSet, rpc: Option<&dyn EditorRpc>, now: Instant) {
        let mut waiting = Vec::new();
        for work in std::mem::take(&mut self.redraws) {
            let prepared = match work.rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {
                    waiting.push(work);
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => continue,
                Ok(prepared) => prepared,
            };
            // A file that could not be read keeps what the editor draws: that is still true of the
            // turn it was drawn for.
            let Ok((abs, exact)) = prepared else { continue };
            if tab_gone(tabs, work.tab, &work.session, "").is_some() {
                continue;
            }
            let (Some(rpc), Some(overlay)) = (rpc, self.overlay.as_mut()) else {
                continue;
            };
            if !overlay.active_paths().contains(&abs) {
                continue;
            }
            let meta = ShowMeta {
                tab: work.tab.0,
                session: work.session,
                turn: work.turn,
                scope: work.scope,
            };
            if self.shown.get(&abs).is_some_and(|shown| shown.meta == meta) {
                continue;
            }
            let drawing = draw_plan(exact);
            match drawing.hunks {
                Some(hunks) => {
                    self.shown.insert(
                        abs.clone(),
                        Shown {
                            meta: meta.clone(),
                            headers: drawing.headers,
                        },
                    );
                    overlay.call(rpc, OverlayCall::Show { path: abs, meta, hunks }, now);
                }
                None => {
                    self.shown.remove(&abs);
                    overlay.call(rpc, OverlayCall::Clear { path: abs }, now);
                }
            }
        }
        self.redraws = waiting;
    }

    /// What the editor reported since the last tick: the opens it answered, the hunks the user
    /// reverted there, and the buffers that stopped showing a review.
    fn advance_overlay(&mut self, tabs: &mut TabSet, rpc: Option<&dyn EditorRpc>, now: Instant, out: &mut Vec<Out>) {
        let (Some(overlay), Some(rpc)) = (self.overlay.as_mut(), rpc) else {
            return;
        };
        if !overlay.wants_ticks() {
            return;
        }
        for outcome in overlay.tick(rpc, now) {
            match outcome {
                OverlayOutcome::Answered { request_id, result } => {
                    let Some(note) = self.awaiting.remove(&request_id) else {
                        continue;
                    };
                    match result {
                        Ok(mut message) => {
                            if let Some(note) = note {
                                message.push_str("; ");
                                message.push_str(&note);
                            }
                            out.push(ok_with(&request_id, &message));
                            out.push(Out::EditorOpened);
                        }
                        Err(why) => out.push(failed(&request_id, &why)),
                    }
                }
                OverlayOutcome::Reverts(events) => self.record_editor_reverts(tabs, events, out),
                OverlayOutcome::Unreverts(events) => self.forget_editor_reverts(tabs, events, out),
                OverlayOutcome::Off(offs) => {
                    for OffEvent { path, .. } in offs {
                        self.shown.remove(Path::new(&path));
                    }
                }
            }
        }
    }

    /// Hunks reverted in the editor become reverts of their tab's draft, in the order they
    /// happened. The draft is pushed once per tab after the batch.
    fn record_editor_reverts(&mut self, tabs: &mut TabSet, events: Vec<EditorRevertEvent>, out: &mut Vec<Out>) {
        let mut touched: Vec<TabId> = Vec::new();
        for event in events {
            match self.record_editor_revert(tabs, &event) {
                Ok(tab) => {
                    if !touched.contains(&tab) {
                        touched.push(tab);
                    }
                }
                Err(why) => eprintln!(
                    "[review] a revert made in the editor on {} was dropped: {why}",
                    event.path
                ),
            }
        }
        for tab in touched {
            out.extend(draft_envelope(None, tab, tabs));
        }
    }

    /// Hunks whose reverted text is back in the editor stop being reverts of their tab's draft, in
    /// the order the editor said so. The draft is pushed once per tab after the batch.
    fn forget_editor_reverts(&mut self, tabs: &mut TabSet, events: Vec<EditorRevertEvent>, out: &mut Vec<Out>) {
        let mut touched: Vec<TabId> = Vec::new();
        for event in events {
            match self.forget_editor_revert(tabs, &event) {
                Ok(Some(tab)) => {
                    if !touched.contains(&tab) {
                        touched.push(tab);
                    }
                }
                // Already gone from the draft (sent, or never recorded): nothing to take back.
                Ok(None) => {}
                Err(why) => eprintln!(
                    "[review] an undone revert made in the editor on {} was not taken back: {why}",
                    event.path
                ),
            }
        }
        for tab in touched {
            out.extend(draft_envelope(None, tab, tabs));
        }
    }

    fn forget_editor_revert(&self, tabs: &mut TabSet, event: &EditorRevertEvent) -> Result<Option<TabId>, String> {
        let (tab, rel, header) = self.editor_revert_target(tabs, event, "the undo was not recorded")?;
        let draft = tabs.review_draft_mut(tab)?;
        // The revert was recorded under the header the panel showed then; if the drawing has been
        // replaced since, the header the hunk's own counts give is the other one it can carry.
        let removed = draft.forget_editor_revert(event.meta.turn, &rel, event.hunk.id, &header)
            || draft.forget_editor_revert(event.meta.turn, &rel, event.hunk.id, &header_of(&event.hunk));
        Ok(removed.then_some(tab))
    }

    /// The tab, the project-relative path and the header of the hunk an editor event is about.
    fn editor_revert_target(
        &self,
        tabs: &mut TabSet,
        event: &EditorRevertEvent,
        consequence: &str,
    ) -> Result<(TabId, String, String), String> {
        let tab = TabId(event.meta.tab);
        if let Some(why) = tab_gone(tabs, tab, &event.meta.session, consequence) {
            return Err(why);
        }
        let root = tabs.review_session(tab)?.0.project_root().to_path_buf();
        let abs = Path::new(&event.path);
        let rel = abs
            .strip_prefix(&root)
            .ok()
            .and_then(Path::to_str)
            .filter(|rel| check_relative(rel).is_ok())
            .ok_or_else(|| "the file is not inside the project".to_string())?
            .to_string();
        let header = self
            .shown
            .get(abs)
            .and_then(|shown| shown.headers.get(&event.hunk.id))
            .cloned()
            .unwrap_or_else(|| header_of(&event.hunk));
        Ok((tab, rel, header))
    }

    fn record_editor_revert(&self, tabs: &mut TabSet, event: &EditorRevertEvent) -> Result<TabId, String> {
        let (tab, rel, header) = self.editor_revert_target(tabs, event, "the revert was not recorded")?;
        // The bytes are the file's own, rebuilt from the lines and endings the editor echoed, never
        // by joining text with a terminator of this file's own choosing.
        let reverted_to = event.hunk.old_bytes();
        let replaced = event.hunk.new_bytes();
        let record = NewRevert {
            turn: event.meta.turn,
            path: rel,
            hunk: Some((event.hunk.id, header)),
            shape: lines_shape(event.at_line, &reverted_to),
            source: RevertSource::Editor,
            at_line: event.at_line,
            reverted_to,
            replaced,
            undo: None,
        };
        tabs.review_draft_mut(tab)?.record_revert(record);
        Ok(tab)
    }

    // ---- comments ------------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn start_comment(
        &mut self,
        tabs: &mut TabSet,
        request_id: &str,
        tab: TabId,
        turn: u32,
        scope: Scope,
        path: String,
        from: u32,
        to: u32,
        text: String,
    ) -> Result<(), String> {
        let (review, session) = tabs.review_session(tab)?;
        if self.anchors.len() >= MAX_ANCHORS {
            return Err("comments are still being quoted; try again in a moment".to_string());
        }
        let job = review.anchor_job(&session, turn, scope, &path, from, to);
        let rx = spawn("eitri-review-flow", move || job.run())?;
        self.jobs_started += 1;
        self.anchors.push(AnchorWork {
            request_id: request_id.to_string(),
            tab,
            session,
            turn,
            path,
            from,
            to,
            text,
            rx,
        });
        Ok(())
    }

    fn advance_anchors(&mut self, tabs: &mut TabSet, out: &mut Vec<Out>) {
        let mut waiting = Vec::new();
        for work in std::mem::take(&mut self.anchors) {
            let polled = work.rx.try_recv();
            let id = work.request_id.as_str();
            match polled {
                Err(mpsc::TryRecvError::Empty) => {
                    waiting.push(work);
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => out.push(failed(id, "the comment's lines could not be read")),
                Ok(Err(why)) => out.push(failed(id, &why.to_string())),
                Ok(Ok(anchor)) => {
                    if let Some(why) = tab_gone(tabs, work.tab, &work.session, "the comment was not added") {
                        out.push(failed(id, &why));
                        continue;
                    }
                    let added = tabs.review_draft_mut(work.tab).and_then(|draft| {
                        draft.add_comment(work.turn, &work.path, work.from, work.to, anchor, &work.text)
                    });
                    match added {
                        Ok(_) => out.extend(draft_envelope(Some(id), work.tab, tabs)),
                        Err(why) => out.push(failed(id, &why)),
                    }
                }
            }
        }
        self.anchors = waiting;
    }

    // ---- send ----------------------------------------------------------------------------------

    fn start_send(
        &mut self,
        tabs: &mut TabSet,
        rpc: Option<&dyn EditorRpc>,
        now: Instant,
        request_id: &str,
        tab: TabId,
        confirm: Option<String>,
    ) -> Result<(), String> {
        let (review, session) = tabs.review_session(tab)?;
        let root = review.project_root().to_path_buf();
        let latest_turn = review.turns(&session).iter().map(|r| r.n).max().unwrap_or(0);
        // A preview computed while a write is out describes a disk that is about to change.
        if self.write.is_some() {
            return Err(format!("{WRITE_BUSY}; try again in a moment"));
        }
        if self.sends.iter().any(|send| send.tab == tab) {
            return Err("this review's message is already being prepared".to_string());
        }
        let draft = tabs.review_draft(tab)?.clone();
        if draft.is_empty() {
            return Err("the review draft is empty; there is nothing to send".to_string());
        }
        // What the editor holds of each revert made there, so that one not yet saved is named
        // for what it is. With no editor the answer is "not loaded" at once.
        let mut waiting = Vec::new();
        let mut known = BTreeMap::new();
        for record in draft.reverts() {
            if record.new.source != RevertSource::Editor || record.undone {
                continue;
            }
            let lines = lines_in(&record.new.reverted_to);
            let asked = match rpc {
                Some(rpc) if rpc.target().is_some() => Some(rpc.exec_lua(
                    BUFFER_STATE_LUA,
                    buffer_state_args(&root.join(&record.new.path), Some((record.new.at_line, lines))),
                )),
                _ => None,
            };
            match asked {
                Some(pending) => waiting.push((record.id, record.new.at_line, pending)),
                None => {
                    known.insert(record.id, None);
                }
            }
        }
        self.sends.push(SendWork {
            request_id: request_id.to_string(),
            tab,
            session,
            confirm,
            draft,
            stage: SendStage::Asking {
                waiting,
                known,
                started: now,
                root,
                latest_turn,
            },
        });
        Ok(())
    }

    fn advance_sends(&mut self, tabs: &mut TabSet, now: Instant, out: &mut Vec<Out>) {
        let mut still = Vec::new();
        for mut work in std::mem::take(&mut self.sends) {
            if let Some(why) = tab_gone(tabs, work.tab, &work.session, "nothing was sent") {
                out.push(failed(&work.request_id, &why));
                continue;
            }
            let mut composed: Option<Result<Preview, mpsc::TryRecvError>> = None;
            match &mut work.stage {
                SendStage::Asking {
                    waiting,
                    known,
                    started,
                    root,
                    latest_turn,
                } => {
                    let timed_out = now.saturating_duration_since(*started) >= BUFFER_ANSWER_WAIT;
                    waiting.retain(|(id, at_line, pending)| match pending.try_take() {
                        Some(Ok(value)) => {
                            let bytes = parse_buffer_state(&value)
                                .ok()
                                .and_then(|state| state.range_bytes(*at_line));
                            known.insert(*id, bytes);
                            false
                        }
                        Some(Err(_)) => {
                            known.insert(*id, None);
                            false
                        }
                        None if timed_out => {
                            known.insert(*id, None);
                            false
                        }
                        None => true,
                    });
                    if waiting.is_empty() {
                        let (root, draft, known, latest) =
                            (root.clone(), work.draft.clone(), known.clone(), *latest_turn);
                        match spawn("eitri-review-flow", move || {
                            let statuses = check_reverts(&root, &draft, &known);
                            compose(&draft, &statuses, latest)
                        }) {
                            Ok(rx) => {
                                self.jobs_started += 1;
                                work.stage = SendStage::Composing(rx);
                            }
                            Err(why) => {
                                out.push(failed(&work.request_id, &why));
                                continue;
                            }
                        }
                    }
                }
                SendStage::Composing(rx) => composed = Some(rx.try_recv()),
            }
            match composed {
                None | Some(Err(mpsc::TryRecvError::Empty)) => still.push(work),
                Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    out.push(failed(&work.request_id, "the review message could not be prepared"))
                }
                Some(Ok(preview)) => self.finish_send(work, preview, tabs, out),
            }
        }
        self.sends = still;
    }

    /// The message is composed: show it, or, when the user confirmed a digest, send it only if
    /// that is still exactly the message they saw.
    fn finish_send(&mut self, work: SendWork, preview: Preview, tabs: &mut TabSet, out: &mut Vec<Out>) {
        let id = work.request_id.as_str();
        let tab = work.tab;
        let Some(confirmed) = work.confirm.as_deref() else {
            let queued = tabs.get(tab).is_some_and(would_wait);
            out.push(Out::Envelope(serialize_review_send_preview_for_js(
                id,
                tab,
                &work.draft,
                &preview,
                queued,
            )));
            return;
        };
        if confirmed != preview.digest {
            out.push(failed(
                id,
                "the draft or the disk changed since the preview; press s again",
            ));
            return;
        }
        // The message was made from the draft as it was; a comment added or a revert recorded since
        // is not in it, and clearing the draft would lose it unsent.
        if tabs.review_draft(tab).map_or(true, |now| *now != work.draft) {
            out.push(failed(
                id,
                "the draft changed while the message was prepared; press s again",
            ));
            return;
        }
        if work.draft.comments().is_empty() && preview.not_on_disk.len() == work.draft.reverts().len() {
            out.push(failed(
                id,
                "nothing in the draft can be sent: every revert is undone, changed or only in the editor",
            ));
            return;
        }
        match confirmed_send(tabs, tab, &preview.text, now_ms()) {
            Err(why) => out.push(failed(id, &why)),
            Ok(sent) => {
                let went_out = sent.is_some();
                let refused = sent
                    .as_ref()
                    .and_then(|flush| flush.outcome.as_ref().err())
                    .map(|e| e.message.clone());
                match sent {
                    Some(flush) => out.push(Out::Flushed(tab, flush)),
                    None => out.push(Out::QueueChanged(tab)),
                }
                // Cleared whatever the backend said: a refused message stays in the tab's queue,
                // so keeping the draft too would send it twice.
                if let Ok(draft) = tabs.review_draft_mut(tab) {
                    draft.clear();
                }
                out.extend(draft_envelope(None, tab, tabs));
                match refused {
                    Some(why) => out.push(failed(id, &format!("not sent: {why}; it stays in the queue"))),
                    None if went_out => out.push(ok_with(id, "sent")),
                    None => out.push(ok_with(id, "queued: it is sent when the running turn ends")),
                }
            }
        }
    }

    // ---- the journal ---------------------------------------------------------------------------

    fn advance_recovery_job(&mut self, out: &mut Vec<Out>) {
        let Some(rx) = &self.recovery_job else {
            return;
        };
        match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.recovery_job = None,
            Ok(entries) => {
                self.recovery_job = None;
                self.recoveries = entries;
                // Whatever `review.hint` says: a write that did not finish is not a hint.
                if !self.recoveries.is_empty() {
                    out.push(Out::Envelope(serialize_review_recovery_for_js(&self.recoveries)));
                }
            }
        }
    }
}

/// The one place this file sends a message: the review the user confirmed, queued and flushed as
/// their own message. Everything that reaches it has been checked against the digest they saw.
fn confirmed_send(tabs: &mut TabSet, tab: TabId, text: &str, now_ms: u64) -> Result<Option<Flush>, String> {
    tabs.send_review(tab, text, now_ms)
}

const TURN_REVIEW_UNAVAILABLE: &str = "turn review is not available in this window";
const NOT_RECORDED: &str =
    "reverted, but the tab closed before it was recorded; the replaced bytes are kept in the review store";
const UNDO_NOT_RECORDED: &str = "undone, but the tab closed before it was recorded";

fn failed(request_id: &str, why: &str) -> Out {
    Out::Envelope(serialize_command_result_for_js(request_id, Err(why)))
}

fn ok(request_id: &str) -> Out {
    Out::Envelope(serialize_command_result_for_js(request_id, Ok(())))
}

fn ok_with(request_id: &str, message: &str) -> Out {
    Out::Envelope(serialize_command_ok_with_message_for_js(request_id, message))
}

/// The tab's draft as the panel draws it; `None` when the tab is gone.
fn draft_envelope(request_id: Option<&str>, tab: TabId, tabs: &TabSet) -> Option<Out> {
    let draft = tabs.review_draft(tab).ok()?;
    Some(Out::Envelope(serialize_review_draft_for_js(request_id, tab, draft)))
}

/// Why `ticket` is no longer wanted, if it is not: its tab closed, or the tab holds another session.
fn stale_reason(tabs: &TabSet, ticket: &ReviewTicket) -> Option<&'static str> {
    if ticket.is_current(tabs) {
        None
    } else if tabs.get(ticket.tab).is_none() {
        Some(TAB_CLOSED)
    } else {
        Some(SESSION_CHANGED)
    }
}

/// Why a request about `tab` and `session` cannot be finished now, with `consequence` said last.
fn tab_gone(tabs: &TabSet, tab: TabId, session: &str, consequence: &str) -> Option<String> {
    match tabs.get(tab) {
        None => Some(format!("the tab closed; {consequence}")),
        Some(found) if found.provider_session_id().as_deref() != Some(session) => {
            Some(format!("the tab's session changed; {consequence}"))
        }
        Some(_) => None,
    }
}

/// The checks a cleared editor question must pass on the GTK thread before a job is built, in
/// this order: still wanted, no turn running in this window, and still the same editor.
fn cleared_but_not_wanted(work: &WriteWork, tabs: &TabSet, rpc: Option<&dyn EditorRpc>) -> Option<String> {
    if let Some(ticket) = &work.ticket {
        if let Some(why) = stale_reason(tabs, ticket) {
            return Some(why.to_string());
        }
    }
    if tabs.running_count() > 0 {
        return Some(Blocked::TurnRunningHere.to_string());
    }
    if rpc.and_then(|rpc| rpc.target()) != work.target_at_ask {
        return Some(EDITOR_CHANGED.to_string());
    }
    None
}

/// This window's guard, if no turn runs in it: what every write is asked at the door. The worker
/// asks again, against every other window too.
fn may_write(tabs: &TabSet) -> Result<PresenceGuard, String> {
    if tabs.running_count() > 0 {
        return Err(Blocked::TurnRunningHere.to_string());
    }
    tabs.presence_guard()
        .ok_or_else(|| Blocked::NotHeld("no state directory".to_string()).to_string())
}

/// A path from the panel names a file inside the project: relative, with no `..`, no root and no
/// NUL. The write checks it again from the project root; this keeps a path that cannot be one out
/// of the editor's question.
fn check_relative(path: &str) -> Result<(), String> {
    let inside = !path.is_empty()
        && !path.contains('\0')
        && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)));
    if inside {
        Ok(())
    } else {
        Err(format!("{path} is not a path inside the project"))
    }
}

/// Whether a send from `tab` waits instead of going out: the checks `send_review` ends with.
fn would_wait(tab: &Tab) -> bool {
    match &tab.backend {
        TabBackend::Live(backend) => {
            tab.turn_running() || tab.pending_handoff.is_some() || !backend.projection().pending_permissions.is_empty()
        }
        _ => true,
    }
}

/// How many lines `bytes` has, each counted with its terminator.
fn lines_in(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.split_inclusive(|b| *b == b'\n').count()).unwrap_or(u32::MAX)
}

/// The lines `reverted_to` covers from `at_line` on.
fn lines_shape(at_line: u32, reverted_to: &[u8]) -> RevertShape {
    let lines = lines_in(reverted_to);
    let to = if lines == 0 {
        at_line
    } else {
        at_line.saturating_add(lines - 1)
    };
    RevertShape::Lines { from: at_line, to }
}

/// What a revert looks like to the draft: the lines it now covers, or the file's fate.
fn shape_of(applied: &AppliedRevert) -> RevertShape {
    match applied.kind {
        RevertKind::Hunk { .. } => lines_shape(applied.at_line, &applied.reverted_to),
        RevertKind::Delete => RevertShape::Deleted,
        RevertKind::Restore => RevertShape::Restored,
        RevertKind::Replace => RevertShape::WholeFile,
    }
}

/// A path the editor may be asked to open: one inside the project (see [`check_relative`]) that
/// holds no control character, because the editor reports a revert with the path as text.
fn check_openable(path: &str) -> Result<(), String> {
    check_relative(path)?;
    if path.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
        return Err(format!("{} is not a path inside the project", path.escape_default()));
    }
    Ok(())
}

/// On a worker: `rel` resolved from the project root, its directory canonicalized and required to
/// stay inside the root, and the turn's hunks of it.
fn prepare_file(root: &Path, rel: &str, job: super::revert::ExactHunksJob) -> Prepared {
    let rel_path = Path::new(rel);
    let leaf = rel_path
        .file_name()
        .ok_or_else(|| format!("{rel} is not a path inside the project"))?;
    let directory = root
        .join(rel_path.parent().unwrap_or(Path::new("")))
        .canonicalize()
        .map_err(|e| format!("{rel}: its directory cannot be reached: {e}"))?;
    if !directory.starts_with(root) {
        return Err(format!("{rel} is not inside the project"));
    }
    Ok((directory.join(leaf), job.run()))
}

/// What the editor is asked to draw for a file, and what its answer is told besides.
struct DrawPlan {
    /// `None`: open the file with no overlay.
    hunks: Option<Vec<ShowHunk>>,
    headers: BTreeMap<u32, String>,
    note: Option<String>,
}

fn draw_plan(exact: Result<ExactHunks, ReviewError>) -> DrawPlan {
    let none = |note: String| DrawPlan {
        hunks: None,
        headers: BTreeMap::new(),
        note: Some(note),
    };
    match exact {
        Ok(exact) => {
            let binary = exact.binary;
            let headers = exact
                .hunks
                .iter()
                .flatten()
                .map(|hunk| (hunk.id, hunk.header.clone()))
                .collect();
            let shown = exact.hunks.map(|hunks| hunks.iter().map(show_hunk).collect());
            match overlay_hunks(binary, shown) {
                Some(hunks) => DrawPlan {
                    hunks: Some(hunks),
                    headers,
                    note: None,
                },
                None if binary => none(OVERLAY_BINARY.to_string()),
                None => none(OVERLAY_TOO_LARGE.to_string()),
            }
        }
        Err(ReviewError::TooLarge(_)) => none(OVERLAY_TOO_LARGE.to_string()),
        Err(other) => none(format!("no overlay: {other}")),
    }
}

/// A hunk as the editor gets it. The one place an [`ExactHunk`] becomes a [`ShowHunk`]; the
/// terminators are split off here and rebuilt, byte for byte, from what the editor echoes back.
pub fn show_hunk(hunk: &ExactHunk) -> ShowHunk {
    ShowHunk::from_file_lines(
        hunk.id,
        (hunk.old_start, hunk.old_len),
        (hunk.new_start, hunk.new_len),
        &hunk.old_lines,
        &hunk.new_lines,
    )
}

/// A hunk's `@@` header from its counts, written as git writes it, for a hunk whose own header was
/// not kept.
fn header_of(hunk: &ShowHunk) -> String {
    let range = |start: u32, len: u32| {
        if len == 1 {
            start.to_string()
        } else {
            format!("{start},{len}")
        }
    };
    format!(
        "@@ -{} +{} @@",
        range(hunk.old_start, hunk.old_len),
        range(hunk.new_start, hunk.new_len)
    )
}

/// The revert engine's account of a path, as the draft keeps it. The draft depends on no engine,
/// so this and [`undo_to_saved`] are the only places the two types meet.
pub fn saved_to_undo(saved: Saved) -> UndoState {
    match saved {
        Saved::Regular { blob, mode } => UndoState::Regular { blob, mode },
        Saved::Symlink { blob } => UndoState::Symlink { blob },
        Saved::Absent => UndoState::Absent,
    }
}

/// The inverse of [`saved_to_undo`].
pub fn undo_to_saved(state: UndoState) -> Saved {
    match state {
        UndoState::Regular { blob, mode } => Saved::Regular { blob, mode },
        UndoState::Symlink { blob } => Saved::Symlink { blob },
        UndoState::Absent => Saved::Absent,
    }
}

/// Runs `work` on a thread of its own and returns the receiver of its result.
fn spawn<T: Send + 'static>(
    name: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<mpsc::Receiver<T>, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            // The flow may be gone (the window closed); the result has nowhere to go.
            let _ = tx.send(work());
        })
        .map_err(|e| format!("could not start a worker: {e}"))?;
    Ok(rx)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
