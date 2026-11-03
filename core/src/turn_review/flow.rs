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
//!   so two questions cannot each clear a different write; across windows the project's write
//!   slot ([`super::write_slot`]) does the same, taken by the write's worker and, shared, by a
//!   review message's read of the disk;
//! - nothing here reads review state on a permission or an ordinary send path: the only message
//!   this file ever sends is the confirmed review, in [`confirmed_send`].
//!
//! **The editor overlay.** The flow also drives the window's [`EditorOverlay`]: `o` on a file opens
//! it in the user's nvim with the turn's hunks drawn over it, a hunk the user reverts there comes
//! back as a revert of the draft, and the panel showing another turn for a drawn file redraws it.
//! The flow never types into the editor: everything it asks is one of the overlay's own calls.
//!
//! **Two windows.** Window 1's flow keeps the one driver nvim's review module has; window 2's calls
//! are queued on it and what it finds for each window waits in [`ReviewFlow::take_linked_events`] for
//! the link. Window 2's flow drives a [`RemoteOverlay`] instead, and asks window 1's editor about a
//! buffer through a [`RemoteAsk`]; everything else here is the same in both windows. An editor
//! event finds its tab by the session it names, since tab numbers are each window's own.
//!
//! **Live drawings.** With `review.live` on, [`ReviewFlow::live_tick`] takes the edits the tab set's
//! pump listed and works them through the window's [`LiveDraws`]: it asks the editor which edited
//! files a window shows, runs a worker per shown file, and turns each answer into a live draw or
//! clear on the same driver. Window 1 asks its own editor and notes each turn's edits in its own
//! driver's contested-paths table; window 2 does both through window 1 ([`LiveAsk`]).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
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
    live_shown_args, overlay_hunks, parse_live_shown, EditorOverlay, EditorRevertEvent, LinkedEvent, LiveMeta,
    OffEvent, Overlay, OverlayCall, OverlayOutcome, Owner, RemoteOverlay, Routed, ShowHunk, ShowMeta, Window,
    LIVE_SHOWN_LUA,
};
use crate::tab_set::{Flush, LiveEdit, ReviewTicket, Tab, TabBackend, TabSet};
use crate::tabs::TabId;
use crate::wm::PeerId;
use serde::{Deserialize, Serialize};

use super::draft::{check_reverts, NewRevert, RevertShape, RevertSource, ReviewDraft, UndoData, UndoState};
use super::journal::JournalEntry;
use super::lifecycle::{ReviewError, Scope};
use super::live_contest::{LiveContest, TurnKey};
use super::live_draws::{LiveDraws, LiveRecords, LiveStep};
use super::live_job::{LiveHunks, LiveRecord, LiveSettle, LiveSkip};
use super::message::{compose, Preview};
use super::presence::{Blocked, PresenceGuard};
use super::revert::{
    parse_buffer_state, AppliedRevert, BufferAsk, EditorCheck, EditorClear, EmbeddedAsk, ExactHunk, ExactHunks,
    RecoverAnswer, Recovered, Refusal, RemoteAsk, RevertKind, RevertTarget, Saved,
};
use super::write_slot::{SlotBusy, WriteSlot};

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
    /// A JSON envelope for the window as a whole (the interrupted reverts on offer), not for any one
    /// request or tab.
    Envelope(String),
    /// A JSON envelope that answers, or is part of the answer to, the panel request `request_id`:
    /// it goes to the page that sent that request.
    Answer { request_id: String, payload: String },
    /// A tab's review draft as it is now, changed by no request of its own: it goes to the page that
    /// shows the tab.
    Draft(TabId, String),
    /// A confirmed review went out (or was refused by the backend and left queued): the shell
    /// applies it as it does any flush of a tab's queue, and adds nothing to the prompt history.
    Flushed(TabId, Flush),
    /// A confirmed review was put in the tab's queue behind its running turn: the shell sends the
    /// queue to the page, as it does when a typed message is queued, so the queue strip shows it.
    QueueChanged(TabId),
    /// The editor opened a file for the user: the shell shows it and gives it the keys.
    EditorOpened,
}

/// A linked window's way to window 1 for its live drawings: window 1's driver keeps the one
/// contested-paths table and window 1's editor answers which files are shown.
pub trait LiveAsk {
    /// Tells window 1 that the turn `key`, running from `started_ns` (until `ended_ns` once it
    /// ended), edited `abs_paths`. While the turn runs window 1 also asks its editor which of them
    /// a window shows: the operation's id is returned, and [`take_shown`](Self::take_shown) gives
    /// the answer. `None`: nothing was asked (the turn ended, or there is no link).
    fn note(&self, key: &TurnKey, started_ns: u64, ended_ns: Option<u64>, abs_paths: &[PathBuf]) -> Option<u64>;
    /// The answer to the note `op_id`, one entry per path, once window 1 gave it.
    fn take_shown(&self, op_id: u64) -> Option<Result<Vec<bool>, String>>;
}

/// Where a follow run that opened a file tells the live drawings it ended: `(tab, path relative to
/// the project)`. A file not shown at the edit may be shown now.
#[derive(Clone, Default)]
pub struct FollowEnds(Rc<RefCell<Vec<(u64, PathBuf)>>>);

impl FollowEnds {
    pub fn ended(&self, tab: u64, rel: PathBuf) {
        self.0.borrow_mut().push((tab, rel));
    }

    /// Every end told since the last take, in order.
    pub fn take(&self) -> Vec<(u64, PathBuf)> {
        std::mem::take(&mut *self.0.borrow_mut())
    }

    fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }
}

/// The one shown question out for the live drawings.
enum LiveQuestion {
    /// To this window's own editor.
    Local {
        asked: Vec<PathBuf>,
        pending: Pending,
        since: Instant,
    },
    /// Through window 1, one note per turn the paths were last named by.
    Remote {
        asked: Vec<PathBuf>,
        parts: Vec<RemotePart>,
        since: Instant,
    },
}

struct RemotePart {
    paths: Vec<PathBuf>,
    /// `None`: nothing could be asked, so the paths count as not shown.
    op: Option<u64>,
    shown: Option<Vec<bool>>,
}

/// A live drawing's worker job.
struct LiveJob {
    key: TurnKey,
    tab: u64,
    path: PathBuf,
    rx: LiveRx,
}

enum LiveRx {
    Live(mpsc::Receiver<Result<Result<LiveHunks, LiveSkip>, ReviewError>>),
    Settle(mpsc::Receiver<Result<LiveSettle, ReviewError>>),
}

/// The records of a window with no review: every turn is gone.
struct NoRecords;

impl LiveRecords for NoRecords {
    fn live_record(&self, _session: &str, _turn_id: &str) -> Option<LiveRecord> {
        None
    }
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
    /// The window's editor overlay, once the shell has said who owns the module in it, or window
    /// 2's, which reaches window 1's driver.
    overlay: Option<Overlay>,
    overlay_owner: Option<Owner>,
    /// Window 2's way to ask window 1's editor about a buffer; `None` in a window with its own.
    remote_ask: Option<Rc<RemoteAsk>>,
    /// What window 1's driver found for each linked window, until its link takes it.
    linked_out: BTreeMap<PeerId, Vec<LinkedEvent>>,
    /// Files being read (hunks and path) for an open or for a redraw.
    opens: Vec<OpenWork>,
    redraws: Vec<RedrawWork>,
    /// The opens the editor has not answered yet, each with what to add to its answer.
    awaiting: BTreeMap<String, Option<String>>,
    /// What each file drawn over in the editor shows, to know when the panel moves to another turn.
    shown: BTreeMap<PathBuf, Shown>,
    /// Answers decided between two calls (an owner change failing the opens it dropped).
    early: Vec<Out>,
    /// What a write finished with for a session whose chat moves, or moved, to another window: its
    /// record belongs in the draft the chat took along.
    for_holder: BTreeMap<String, VecDeque<serde_json::Value>>,
    /// The window's live drawings and what is out for them.
    live: LiveDraws,
    live_question: Option<LiveQuestion>,
    live_jobs: Vec<LiveJob>,
    /// The session each tab that edited live held when last seen: a tab that closes, moves away or
    /// changes session takes its drawings with it.
    live_tabs: BTreeMap<u64, String>,
    /// A settled drawing's meta and its hunks' headers, from the draw until the editor's answer.
    live_headers: BTreeMap<PathBuf, (ShowMeta, BTreeMap<u32, String>)>,
    /// Window 2's way to window 1 for its live drawings; `None` in any other window.
    live_ask: Option<Box<dyn LiveAsk>>,
    /// The contested-paths table while the window has no editor: the next driver takes it over.
    parked_contest: LiveContest,
    /// Window 1: the running turns each linked window noted, so their ends are noted when its link ends.
    linked_live: HashMap<TurnKey, PeerId>,
    /// Window 2: the turn that last named each path (with its start), and each ended turn's end,
    /// so a shown question can name the turn its paths belong to.
    live_named: BTreeMap<PathBuf, (TurnKey, u64)>,
    live_ends: HashMap<TurnKey, u64>,
    /// Window 2: notes whose shown answer nobody waits for, taken when they come.
    live_unread: Vec<(u64, Instant)>,
    /// Edits whose path goes through `..`, being resolved against the disk on a worker.
    live_resolving: Vec<mpsc::Receiver<Vec<LiveEdit>>>,
    follow_ends: FollowEnds,
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
    Recover(Result<Recovered, Refusal>),
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
    /// The review directory whose write slot is taken, shared, around the read of the disk.
    slot_dir: PathBuf,
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
    /// The statuses are being read from the disk and the message composed, on a worker. An error is
    /// the slot another window holds.
    Composing(mpsc::Receiver<Result<Preview, String>>),
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
            && self.linked_out.is_empty()
            && !self.overlay.as_ref().is_some_and(Overlay::wants_ticks)
            && self.live_is_idle()
    }

    /// The live drawings have nothing to follow, ask, run or hand out.
    pub fn live_is_idle(&self) -> bool {
        !self.live.wants_ticks()
            && self.live_question.is_none()
            && self.live_jobs.is_empty()
            && self.live_unread.is_empty()
            && self.follow_ends.is_empty()
            && self.live_resolving.is_empty()
    }

    /// Whether the live drawings still follow the turn `key`: it runs, or its drawings are not
    /// settled yet.
    pub fn live_follows(&self, key: &TurnKey) -> bool {
        self.live.follows(key)
    }

    /// Who owns the review module in this window's editor: the integrated window's own nvim, or the
    /// companion panel's channel. A different owner is a different editor, so the driver that
    /// served the old one is dropped and the opens still waiting on it fail. Called before every
    /// request and tick, because a companion's channel changes with each attach.
    pub fn set_editor_owner(&mut self, owner: Option<Owner>) {
        // Window 2 has no editor of its own to change: its overlay is window 1's for its whole life.
        if owner == self.overlay_owner || matches!(self.overlay, Some(Overlay::Remote(_))) {
            return;
        }
        self.overlay_owner = owner;
        // Which paths two running turns both edited is decided from their edits, not from the
        // editor: the table goes on to the next driver, so a turn that edits again stays contested.
        let contest = match self.overlay.take() {
            Some(Overlay::Local(overlay)) => overlay.into_contest(),
            _ => std::mem::take(&mut self.parked_contest),
        };
        match owner {
            Some(owner) => self.overlay = Some(Overlay::Local(EditorOverlay::with_contest(owner, contest))),
            None => self.parked_contest = contest,
        }
        self.shown.clear();
        self.redraws.clear();
        self.live_editor_lost();
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
        self.live_editor_lost();
        self.fail_opens("the editor went away before the file was ready; nothing was opened");
        self.fail_awaiting("the editor went away before it answered; nothing was opened");
        std::mem::take(&mut self.early)
    }

    /// Makes this the flow of window 2, which reaches window 1's editor: its overlay calls go to
    /// window 1's driver through `overlay`, and its questions about a buffer through `ask`.
    pub fn link_to_window_1(&mut self, overlay: RemoteOverlay, ask: RemoteAsk) {
        self.overlay_owner = None;
        self.overlay = Some(Overlay::Remote(overlay));
        self.remote_ask = Some(Rc::new(ask));
        // Window 1's driver keeps the one table now.
        self.parked_contest = LiveContest::new();
        self.linked_live.clear();
        self.shown.clear();
        self.redraws.clear();
        self.live_editor_lost();
        self.fail_opens("the editor changed before the file was ready; nothing was opened");
        self.fail_awaiting("the editor changed before it answered; nothing was opened");
    }

    /// Window 1's driver, through which the shell queues window 2's calls and holds or releases a
    /// moving session's events; `None` in window 2 and in a window without an editor.
    pub fn local_overlay(&mut self) -> Option<&mut EditorOverlay> {
        match self.overlay.as_mut() {
            Some(Overlay::Local(overlay)) => Some(overlay),
            _ => None,
        }
    }

    /// Window 2's driver, which takes what window 1 reports (`review_drawn`, `review_idle`,
    /// `review_events`, the answers to its opens); `None` in any other window.
    pub fn remote_overlay(&mut self) -> Option<&mut RemoteOverlay> {
        match self.overlay.as_mut() {
            Some(Overlay::Remote(overlay)) => Some(overlay),
            _ => None,
        }
    }

    /// What window 1's driver found for each linked window since this was last asked, each in order.
    pub fn take_linked_events(&mut self) -> BTreeMap<PeerId, Vec<LinkedEvent>> {
        std::mem::take(&mut self.linked_out)
    }

    /// The editor question of a write: through window 1 in window 2, else this window's own editor.
    fn start_check(&self, rpc: Option<&dyn EditorRpc>, abs_path: &Path, now: Instant) -> EditorCheck {
        match self.remote_ask.as_deref() {
            Some(ask) => EditorCheck::start(ask, abs_path, now),
            None => EditorCheck::start(&EmbeddedAsk(rpc), abs_path, now),
        }
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
        let Some(overlay) = self.overlay.as_ref().filter(|o| !o.active_paths().is_empty()) else {
            return;
        };
        if check_relative(path).is_err() {
            return;
        }
        let Ok((review, session)) = tabs.review_session(tab) else {
            return;
        };
        let root = review.project_root().to_path_buf();
        // The newest request for a file is the one the panel shows; an older one still being read
        // must not land after it and draw the turn the panel has left, even when this one draws
        // nothing.
        self.redraws.retain(|work| work.path != path);
        // A live drawing is the agent's edits as they land: the panel looking at another turn of the
        // file leaves it, and only the panel's own turn of it may replace it.
        if live_of_another_turn(overlay, &root.join(path), &session, turn) {
            return;
        }
        if self.redraws.len() >= MAX_EDITOR_JOBS {
            return;
        }
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

    /// Whether the panel request `request_id` may still be answered: it is being worked out, waits
    /// for the editor, or its answer is decided and not yet handed out.
    pub fn owes_answer(&self, request_id: &str) -> bool {
        self.write.as_ref().is_some_and(|w| w.request_id == request_id)
            || self.anchors.iter().any(|w| w.request_id == request_id)
            || self.sends.iter().any(|w| w.request_id == request_id)
            || self.opens.iter().any(|w| w.request_id == request_id)
            || self.awaiting.contains_key(request_id)
            || self
                .early
                .iter()
                .any(|out| matches!(out, Out::Answer { request_id: id, .. } if id == request_id))
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
                let remote = self.remote_ask.clone();
                let embedded = EmbeddedAsk(rpc);
                let ask: &dyn BufferAsk = match remote.as_deref() {
                    Some(remote) => remote,
                    None => &embedded,
                };
                named(tab).and_then(|tab| self.start_send(tabs, ask, now, &request_id, tab, confirm))
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
        let check = self.start_check(rpc, &root.join(&path), now);
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
        let check = self.start_check(rpc, &root.join(&path), now);
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
                let check = self.start_check(rpc, &root.join(&entry.path), now);
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
        // A session moving to another window took its draft along as it was; a session that moved
        // has none here. Either way the holder records what the write did.
        let (moving, moved) = work.ticket.as_ref().map_or((false, false), |ticket| {
            tabs.turn_review().map_or((false, false), |review| {
                (review.handing_over(&ticket.session), review.moved_away(&ticket.session))
            })
        });
        if moving || (moved && !same_tab) {
            if let (Some(ticket), Some(result)) = (work.ticket.as_ref(), WriteResult::of(&work.op, &done)) {
                if let Ok(value) = serde_json::to_value(result) {
                    self.for_holder
                        .entry(ticket.session.clone())
                        .or_default()
                        .push_back(value);
                }
            }
        }
        if moved && !same_tab {
            match &done {
                Done::Revert(Ok(_)) | Done::Undo(Ok(())) => {
                    out.push(ok_with(id, HELD_ELSEWHERE));
                    return;
                }
                _ => {}
            }
        }
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
            (Op::Restore { entry } | Op::Dismiss { entry }, Done::Recover(Ok(recovered))) => {
                self.recoveries.retain(|e| e.id != entry.id);
                // Sent even when nothing is left: an empty list takes the notice away.
                out.push(Out::Envelope(serialize_review_recovery_for_js(&self.recoveries)));
                match recovered {
                    // Not a restore: the user is told that nothing was put back, and why.
                    Recovered::AlreadyWritten => out.push(ok_with(
                        id,
                        &format!(
                            "{} already held what the interrupted revert was writing; nothing was changed, the entry is forgotten",
                            entry.path.display()
                        ),
                    )),
                    Recovered::Restored | Recovered::Dismissed => out.push(ok(id)),
                }
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

    // ---- results for the window a chat moved to -----------------------------------------------

    /// The oldest result of a job this window finished for `session` after its chat started moving
    /// to another window, for the holder there (a `review_result` on the link): a write's record
    /// first, then the turn review's snapshots. Taken once the move is done.
    pub fn job_result_for_holder(&mut self, tabs: &TabSet, session: &str) -> Option<serde_json::Value> {
        if let Some(queue) = self.for_holder.get_mut(session) {
            let value = queue.pop_front();
            if queue.is_empty() {
                self.for_holder.remove(session);
            }
            if value.is_some() {
                return value;
            }
        }
        tabs.turn_review()?.result_for_holder(session)
    }

    /// The move of `session` did not happen: nothing kept for another window is sent. This drops
    /// only the writes' records kept here; the turn review keeps its own (and its own mark that the
    /// session is being handed over), so [`TurnReview::move_aborted`] is owed as well, or its
    /// snapshots stay kept for a holder that never comes.
    ///
    /// [`TurnReview::move_aborted`]: crate::turn_review::TurnReview::move_aborted
    pub fn move_aborted(&mut self, session: &str) {
        self.for_holder.remove(session);
    }

    /// A result the window `session` came from finished for it: a write is recorded in the draft of
    /// the tab that holds the session here, a snapshot goes to the turn review.
    pub fn review_result(
        &mut self,
        tabs: &mut TabSet,
        session: &str,
        result: serde_json::Value,
    ) -> Result<Vec<Out>, String> {
        if result.get("write").is_none() {
            let review = tabs.turn_review().ok_or_else(|| TURN_REVIEW_UNAVAILABLE.to_string())?;
            review.accept_result(session, result)?;
            return Ok(Vec::new());
        }
        let result: WriteResult =
            serde_json::from_value(result).map_err(|e| format!("protocol: not a review result: {e}"))?;
        let tab = tabs
            .tabs()
            .iter()
            .find(|t| t.provider_session_id().as_deref() == Some(session))
            .map(|t| t.id)
            .ok_or_else(|| format!("no tab holds session {session} here"))?;
        let draft = tabs.review_draft_mut(tab)?;
        match result.into_record()? {
            HolderRecord::Revert(record) => {
                draft.record_revert(record);
            }
            HolderRecord::Undone(record) => draft.mark_undone(record),
        }
        Ok(draft_envelope(None, tab, tabs).into_iter().collect())
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
        if !self.overlay.as_ref().is_some_and(|overlay| overlay.reachable(rpc)) {
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
            let Some(overlay) = self.overlay.as_mut().filter(|overlay| overlay.reachable(rpc)) else {
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
            let Some(overlay) = self.overlay.as_mut().filter(|overlay| overlay.reachable(rpc)) else {
                continue;
            };
            if !overlay.active_paths().contains(&abs) {
                continue;
            }
            // Drawn live for another turn while the file was being read: the live drawing stays.
            if live_of_another_turn(overlay, &abs, &work.session, work.turn) {
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
                // Nothing to put in its place (a running turn has no end snapshot yet): the agent's
                // edits stay drawn as they landed.
                None if overlay.live_drawings().contains_key(&abs) => {}
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
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        if !overlay.wants_ticks() {
            return;
        }
        let Routed { mine, linked } = overlay.tick(rpc, now);
        for (peer, events) in linked {
            self.linked_out.entry(peer).or_default().extend(events);
        }
        for outcome in mine {
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
                        self.live.off(Path::new(&path));
                    }
                }
                OverlayOutcome::LiveDrawn {
                    path,
                    meta,
                    drawn,
                    left,
                } => {
                    let headers = match self.live_headers.get(&path) {
                        Some((shown, _))
                            if !meta.running && shown.session == meta.session && shown.turn == meta.turn =>
                        {
                            self.live_headers.remove(&path)
                        }
                        _ => None,
                    };
                    // Only a drawing that is up, and is still this turn's live one, says what the
                    // file shows: an `o` drawing that took the file meanwhile keeps its own headers,
                    // and so does one still on its way, whose headers are in `shown` already.
                    let live_now = (meta.session.clone(), meta.turn);
                    let up = drawn > 0
                        && !left
                        && self.overlay.as_ref().is_some_and(|o| {
                            o.live_drawings().get(&path) == Some(&live_now) && !o.draws_pending(&path)
                        });
                    if !up {
                        continue;
                    }
                    match headers {
                        Some((meta, headers)) => {
                            self.shown.insert(path, Shown { meta, headers });
                        }
                        // A running turn's drawing has no hunk to revert, so no header to keep.
                        None => {
                            self.shown.remove(&path);
                        }
                    }
                }
            }
        }
    }

    // ---- live drawings -------------------------------------------------------------------------

    /// Makes window 2's live drawings go through window 1: its notes and shown questions are `ask`'s.
    pub fn set_live_ask(&mut self, ask: Box<dyn LiveAsk>) {
        self.live_ask = Some(ask);
        self.live_editor_lost();
    }

    /// Where a follow run tells this window's live drawings that it opened a file.
    pub fn follow_ends(&self) -> FollowEnds {
        self.follow_ends.clone()
    }

    /// The editor the drawings were made in is gone or replaced: nothing out for it will be used.
    fn live_editor_lost(&mut self) {
        self.live.editor_lost();
        self.live_question = None;
        self.live_jobs.clear();
        self.live_headers.clear();
    }

    /// One round of the live drawings: the tabs' new edits, the editor's and the workers' answers,
    /// and what follows from them, each a call on the driver, a question or a worker. Reads memory
    /// only; never waits.
    pub fn live_tick(&mut self, tabs: &mut TabSet, rpc: Option<&dyn EditorRpc>, now: Instant) {
        let edits = tabs.take_live_edits();
        let mut resolved = Vec::new();
        self.live_resolving.retain(|rx| match rx.try_recv() {
            Ok(batch) => {
                resolved.extend(batch);
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        });
        let follows = self.follow_ends.take();
        let gone: Vec<u64> = self
            .live_tabs
            .iter()
            .filter(|(tab, session)| !holds_session(tabs, **tab, session))
            .map(|(tab, _)| *tab)
            .collect();
        if edits.is_empty() && resolved.is_empty() && follows.is_empty() && gone.is_empty() && self.live_is_idle() {
            return;
        }
        let review = tabs.turn_review();
        let records: &dyn LiveRecords = match review {
            Some(review) => review,
            None => &NoRecords,
        };
        let root = review.map(|review| review.project_root().to_path_buf());

        for tab in gone {
            self.live_tabs.remove(&tab);
            self.live.tab_gone(tab);
            // Their answers would be dropped; the slots are freed now.
            self.live_jobs.retain(|job| job.tab != tab);
        }
        let (dotted, plain): (Vec<LiveEdit>, Vec<LiveEdit>) = edits
            .into_iter()
            .filter(|edit| holds_session(tabs, edit.tab.0, &edit.session))
            .partition(|edit| edit.path.components().any(|c| c == Component::ParentDir));
        let mut edits: Vec<LiveEdit> = plain
            .into_iter()
            .filter_map(|mut edit| {
                edit.path = live_path(root.as_deref()?, &edit.path)?;
                Some(edit)
            })
            .collect();
        if let (false, Some(root)) = (dotted.is_empty(), root.clone()) {
            // `..` after a link leads where the link leads, which only the disk says: read off the GTK
            // thread, taken up on a later tick.
            let batch = dotted.clone();
            let worker_root = root.clone();
            match spawn("live-path", move || {
                batch
                    .into_iter()
                    .filter_map(|mut edit| {
                        edit.path = resolve_live_path(&worker_root, &edit.path)?;
                        Some(edit)
                    })
                    .collect()
            }) {
                Ok(rx) => self.live_resolving.push(rx),
                Err(_) => edits.extend(dotted.into_iter().filter_map(|mut edit| {
                    edit.path = live_path(&root, &edit.path)?;
                    Some(edit)
                })),
            }
        }
        edits.extend(
            resolved
                .into_iter()
                .filter(|edit| holds_session(tabs, edit.tab.0, &edit.session)),
        );
        for edit in &edits {
            self.live_tabs.insert(edit.tab.0, edit.session.clone());
        }
        if !edits.is_empty() {
            self.live.edits(edits, records, now);
        }
        for (tab, rel) in follows {
            if let Some(abs) = root.as_deref().and_then(|root| live_path(root, &rel)) {
                self.live.follow_ended(tab, &abs, now);
            }
        }

        self.poll_live_question(now);
        self.poll_live_jobs();
        self.poll_live_unread(now);
        let steps = self.live.steps(records, self.live_jobs.len(), now);
        for step in steps {
            match step {
                LiveStep::AskShown { paths } => self.ask_shown(paths, rpc, now),
                LiveStep::Note {
                    key,
                    started_ns,
                    ended_ns,
                    paths,
                } => self.note_live(&key, started_ns, ended_ns, &paths, now),
                LiveStep::RunLive { key, tab, path } => {
                    let job = root.as_deref().zip(review).and_then(|(root, review)| {
                        let rel = path.strip_prefix(root).ok()?.to_str()?;
                        Some(review.live_hunks_job(&key.session, &key.turn_id, rel))
                    });
                    match job.map(|job| spawn("eitri-review-live", move || job.run())) {
                        Some(Ok(rx)) => {
                            self.jobs_started += 1;
                            self.live_jobs.push(LiveJob {
                                key,
                                tab,
                                path,
                                rx: LiveRx::Live(rx),
                            });
                        }
                        _ => self.live.live_done(&key, &path, Err(no_worker())),
                    }
                }
                LiveStep::RunSettle { key, tab, turn, path } => {
                    let job = root.as_deref().zip(review).and_then(|(root, review)| {
                        let rel = path.strip_prefix(root).ok()?.to_str()?;
                        Some(review.live_settle_job(&key.session, turn, rel))
                    });
                    match job.map(|job| spawn("eitri-review-settle", move || job.run())) {
                        Some(Ok(rx)) => {
                            self.jobs_started += 1;
                            self.live_jobs.push(LiveJob {
                                key,
                                tab,
                                path,
                                rx: LiveRx::Settle(rx),
                            });
                        }
                        _ => self.live.settle_done(&key, &path, Err(no_worker())),
                    }
                }
                LiveStep::Draw {
                    path,
                    key,
                    tab,
                    turn,
                    running,
                    read_lines,
                    hunks,
                    headers,
                } => {
                    let meta = LiveMeta {
                        tab,
                        session: key.session,
                        turn_id: key.turn_id,
                        turn,
                        running,
                        read_lines,
                    };
                    self.live_draw(path, meta, &hunks, headers, rpc, now);
                }
                LiveStep::Clear { path } => self.live_clear(path, rpc, now),
            }
        }
        if self.live_ask.is_some() && !self.live.wants_ticks() && self.live_question.is_none() {
            // Nothing will be asked for these turns' paths again.
            self.live_named.clear();
            self.live_ends.clear();
        }
    }

    /// Asks which of `paths` a window shows: this window's editor, or window 1's for window 2. With
    /// no editor to ask, none is.
    fn ask_shown(&mut self, paths: Vec<PathBuf>, rpc: Option<&dyn EditorRpc>, now: Instant) {
        if let Some(ask) = self.live_ask.as_deref() {
            // Grouped by the turn that last named each path: window 1 answers a turn's note.
            let mut groups: Vec<(TurnKey, u64, Vec<PathBuf>)> = Vec::new();
            let mut parts = Vec::new();
            for path in &paths {
                match self.live_named.get(path) {
                    Some((key, started_ns)) => match groups.iter_mut().find(|(k, _, _)| k == key) {
                        Some((_, _, group)) => group.push(path.clone()),
                        None => groups.push((key.clone(), *started_ns, vec![path.clone()])),
                    },
                    None => parts.push(RemotePart {
                        paths: vec![path.clone()],
                        op: None,
                        shown: Some(vec![false]),
                    }),
                }
            }
            for (key, started_ns, group) in groups {
                let op = ask.note(&key, started_ns, None, &group);
                // A turn that already ended is told so again, so the question does not reopen it.
                if let Some(&ended_ns) = self.live_ends.get(&key) {
                    if let Some(extra) = ask.note(&key, started_ns, Some(ended_ns), &group) {
                        self.live_unread.push((extra, now));
                    }
                }
                let shown = op.is_none().then(|| vec![false; group.len()]);
                parts.push(RemotePart {
                    paths: group,
                    op,
                    shown,
                });
            }
            self.live_question = Some(LiveQuestion::Remote {
                asked: paths,
                parts,
                since: now,
            });
            return;
        }
        match (self.overlay.as_ref(), rpc) {
            (Some(Overlay::Local(_)), Some(rpc)) => {
                let pending = rpc.exec_lua(LIVE_SHOWN_LUA, live_shown_args(&paths));
                self.live_question = Some(LiveQuestion::Local {
                    asked: paths,
                    pending,
                    since: now,
                });
            }
            _ => self.live.shown(paths, Vec::new(), now),
        }
    }

    /// The shown question's answer, once it is in; past [`BUFFER_ANSWER_WAIT`] what is missing
    /// counts as not shown.
    fn poll_live_question(&mut self, now: Instant) {
        let Some(question) = self.live_question.take() else {
            return;
        };
        match question {
            LiveQuestion::Local { asked, pending, since } => match pending.try_take() {
                None if now.saturating_duration_since(since) < BUFFER_ANSWER_WAIT => {
                    self.live_question = Some(LiveQuestion::Local { asked, pending, since });
                }
                Some(Ok(answer)) => {
                    let shown = parse_live_shown(&answer, asked.len()).unwrap_or_default();
                    self.live.shown(asked, shown, now);
                }
                _ => self.live.shown(asked, Vec::new(), now),
            },
            LiveQuestion::Remote {
                asked,
                mut parts,
                since,
            } => {
                if let Some(ask) = self.live_ask.as_deref() {
                    for part in parts.iter_mut().filter(|part| part.shown.is_none()) {
                        let Some(op) = part.op else { continue };
                        match ask.take_shown(op) {
                            Some(Ok(shown)) if shown.len() == part.paths.len() => part.shown = Some(shown),
                            Some(_) => part.shown = Some(vec![false; part.paths.len()]),
                            None => {}
                        }
                    }
                }
                let waiting = parts.iter().any(|part| part.shown.is_none());
                if waiting && now.saturating_duration_since(since) < BUFFER_ANSWER_WAIT {
                    self.live_question = Some(LiveQuestion::Remote { asked, parts, since });
                    return;
                }
                for part in &parts {
                    if let (None, Some(op)) = (&part.shown, part.op) {
                        self.live_unread.push((op, now));
                    }
                }
                let answered: BTreeMap<&PathBuf, bool> = parts
                    .iter()
                    .filter_map(|part| Some(part.paths.iter().zip(part.shown.as_ref()?.iter().copied())))
                    .flatten()
                    .collect();
                let shown = asked
                    .iter()
                    .map(|path| answered.get(path).copied().unwrap_or(false))
                    .collect();
                self.live.shown(asked, shown, now);
            }
        }
    }

    /// Workers that finished hand their answers to the live drawings.
    fn poll_live_jobs(&mut self) {
        let mut waiting = Vec::new();
        for job in std::mem::take(&mut self.live_jobs) {
            match &job.rx {
                LiveRx::Live(rx) => match rx.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => waiting.push(job),
                    Err(mpsc::TryRecvError::Disconnected) => self.live.live_done(&job.key, &job.path, Err(no_worker())),
                    Ok(result) => self.live.live_done(&job.key, &job.path, result),
                },
                LiveRx::Settle(rx) => match rx.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => waiting.push(job),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.live.settle_done(&job.key, &job.path, Err(no_worker()))
                    }
                    Ok(result) => self.live.settle_done(&job.key, &job.path, result),
                },
            }
        }
        self.live_jobs = waiting;
    }

    /// Window 2: takes the answers to notes nobody waits for, so none is kept; one that never comes
    /// is let go after [`BUFFER_ANSWER_WAIT`].
    fn poll_live_unread(&mut self, now: Instant) {
        let Some(ask) = self.live_ask.as_deref() else {
            self.live_unread.clear();
            return;
        };
        self.live_unread.retain(|(op, since)| {
            ask.take_shown(*op).is_none() && now.saturating_duration_since(*since) < BUFFER_ANSWER_WAIT
        });
    }

    /// Tells the contested-paths table that `key` edited `paths`: window 1's own driver, or window 1
    /// through window 2's link.
    fn note_live(&mut self, key: &TurnKey, started_ns: u64, ended_ns: Option<u64>, paths: &[PathBuf], now: Instant) {
        if let Some(ask) = self.live_ask.as_deref() {
            for path in paths {
                self.live_named.insert(path.clone(), (key.clone(), started_ns));
            }
            if let Some(ended_ns) = ended_ns {
                self.live_ends.insert(key.clone(), ended_ns);
            }
            // A running turn's note also asks what is shown; the question for these paths is asked
            // on its own, so this answer is only taken out of the way.
            if let Some(op) = ask.note(key, started_ns, ended_ns, paths) {
                self.live_unread.push((op, now));
            }
        } else {
            // A turn a linked window noted is this window's now that its session is here: its end
            // comes from this window, and the link's end must not move it.
            self.linked_live.remove(key);
            self.note_contest(Window::One, key, started_ns, ended_ns, paths);
        }
    }

    /// Window 1: a linked window's note, into the driver's table, or the parked one while the window
    /// has no editor. False in window 2, which has no table.
    pub fn note_linked_live(
        &mut self,
        peer: PeerId,
        key: &TurnKey,
        started_ns: u64,
        ended_ns: Option<u64>,
        paths: &[PathBuf],
    ) -> bool {
        if !self.note_contest(Window::Linked(peer), key, started_ns, ended_ns, paths) {
            return false;
        }
        // The window that noted a running turn last holds its session: a session that moved between
        // linked windows takes the turn along.
        match ended_ns {
            None => {
                self.linked_live.insert(key.clone(), peer);
            }
            Some(_) => {
                self.linked_live.remove(key);
            }
        }
        true
    }

    /// Window 1: the link to `peer` ended, so nobody will say when the turns it noted as running end;
    /// they end at `at_ns`.
    pub fn linked_live_gone(&mut self, peer: PeerId, at_ns: u64) {
        let gone: Vec<TurnKey> = self
            .linked_live
            .iter()
            .filter(|(_, held_by)| **held_by == peer)
            .map(|(key, _)| key.clone())
            .collect();
        for key in gone {
            self.linked_live.remove(&key);
            self.note_contest(Window::Linked(peer), &key, 0, Some(at_ns), &[]);
        }
    }

    /// Window 1's table: its driver's, or the parked one while it has no editor (nothing is drawn to
    /// clear, and the next driver takes the table). False in window 2.
    fn note_contest(
        &mut self,
        from: Window,
        key: &TurnKey,
        started_ns: u64,
        ended_ns: Option<u64>,
        paths: &[PathBuf],
    ) -> bool {
        match self.overlay.as_mut() {
            Some(Overlay::Local(overlay)) => overlay.note_live(from, key, started_ns, ended_ns, paths),
            None => {
                if ended_ns.is_none() {
                    self.parked_contest.reopen(key);
                }
                for path in paths {
                    self.parked_contest.note_edit(key, started_ns, path);
                }
                if let Some(ended_ns) = ended_ns {
                    self.parked_contest.note_end(key, ended_ns);
                }
            }
            Some(Overlay::Remote(_)) => return false,
        }
        true
    }

    /// A live draw of `path`. A settled draw's headers wait for the editor's answer: only a drawing
    /// that went up is what a revert in the editor is recorded under.
    fn live_draw(
        &mut self,
        path: PathBuf,
        meta: LiveMeta,
        hunks: &[ExactHunk],
        headers: BTreeMap<u32, String>,
        rpc: Option<&dyn EditorRpc>,
        now: Instant,
    ) {
        let Some(overlay) = self.overlay.as_mut().filter(|overlay| overlay.reachable(rpc)) else {
            return;
        };
        let Some(hunks) = overlay_hunks(false, Some(hunks.iter().map(show_hunk).collect())) else {
            // Longer than the overlay draws: what was drawn of the file is no longer this turn's.
            overlay.call(rpc, OverlayCall::LiveClear { path }, now);
            return;
        };
        if !meta.running {
            let shown = ShowMeta {
                tab: meta.tab,
                session: meta.session.clone(),
                turn: meta.turn,
                scope: Scope::Turn,
            };
            self.live_headers.insert(path.clone(), (shown, headers));
        }
        overlay.call(rpc, OverlayCall::LiveShow { path, meta, hunks }, now);
    }

    /// Takes `path`'s live drawing off, and with it what the flow kept of that drawing.
    fn live_clear(&mut self, path: PathBuf, rpc: Option<&dyn EditorRpc>, now: Instant) {
        let Some(overlay) = self.overlay.as_mut().filter(|overlay| overlay.reachable(rpc)) else {
            return;
        };
        if let Some((session, turn)) = overlay.live_drawings().get(&path) {
            if self
                .shown
                .get(&path)
                .is_some_and(|shown| shown.meta.session == *session && shown.meta.turn == *turn)
            {
                self.shown.remove(&path);
            }
        }
        overlay.call(rpc, OverlayCall::LiveClear { path }, now);
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
        // By the session, never by the tab number the overlay was drawn with: numbers are each
        // window's own, and a chat that moved between the windows took its session along.
        let held_by = tabs
            .tabs()
            .iter()
            .find(|tab| tab.provider_session_id().as_deref() == Some(event.meta.session.as_str()))
            .map(|tab| tab.id);
        let Some(tab) = held_by else {
            return Err(tab_gone(tabs, TabId(event.meta.tab), &event.meta.session, consequence)
                .unwrap_or_else(|| format!("no tab holds the session; {consequence}")));
        };
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
        ask: &dyn BufferAsk,
        now: Instant,
        request_id: &str,
        tab: TabId,
        confirm: Option<String>,
    ) -> Result<(), String> {
        let (review, session) = tabs.review_session(tab)?;
        let root = review.project_root().to_path_buf();
        let latest_turn = review.turns(&session).iter().map(|r| r.n).max().unwrap_or(0);
        // The slot is where this window's writes take it; a window that holds no presence still
        // reads the disk, so it takes the slot in its review's own store, which is the same
        // directory. With neither there is no slot to take, and no read goes ahead without one.
        let slot_dir = tabs
            .presence_guard()
            .map(|guard| guard.review_dir().to_path_buf())
            .or_else(|| review.store_dir().map(Path::to_path_buf))
            .ok_or_else(|| {
                SlotBusy::Unavailable("there is no state directory to keep it in".to_string()).to_string()
            })?;
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
            let asked = ask.buffer_state(&root.join(&record.new.path), Some((record.new.at_line, lines)));
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
            slot_dir,
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
            let mut composed: Option<Result<Result<Preview, String>, mpsc::TryRecvError>> = None;
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
                        let slot_dir = work.slot_dir.clone();
                        match spawn("eitri-review-flow", move || -> Result<Preview, String> {
                            // Shared: other reads may run beside it, a write in any window may not.
                            let slot = WriteSlot::open_or_busy(&slot_dir)
                                .and_then(|slot| slot.try_shared())
                                .map_err(|busy| busy.to_string())?;
                            let statuses = check_reverts(&root, &draft, &known);
                            drop(slot);
                            Ok(compose(&draft, &statuses, latest))
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
                Some(Ok(Err(why))) => out.push(failed(&work.request_id, &why)),
                Some(Ok(Ok(preview))) => self.finish_send(work, preview, tabs, out),
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
            out.push(Out::Answer {
                request_id: id.to_string(),
                payload: serialize_review_send_preview_for_js(id, tab, &work.draft, &preview, queued),
            });
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
/// a host-composed message, without marking it as text the person typed. Everything that reaches it
/// has been checked against the digest they saw.
fn confirmed_send(tabs: &mut TabSet, tab: TabId, text: &str, now_ms: u64) -> Result<Option<Flush>, String> {
    tabs.send_review(tab, text, now_ms)
}

const TURN_REVIEW_UNAVAILABLE: &str = "turn review is not available in this window";
/// A write that finished after its chat moved to another window: the record is that window's.
const HELD_ELSEWHERE: &str = "written; it is recorded in the other window, where the chat is now";

/// What a write did, for the window that holds its session now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "write", rename_all = "snake_case")]
enum WriteResult {
    Revert {
        turn: u32,
        path: String,
        hunk: Option<(u32, String)>,
        shape: ShapeWire,
        at_line: u32,
        /// Hex, as are the next two: the bytes need not be text.
        reverted_to: String,
        replaced: String,
        pre: SavedWire,
        post: SavedWire,
    },
    Undo {
        record: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
enum ShapeWire {
    Lines { from: u32, to: u32 },
    Deleted,
    Restored,
    WholeFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SavedWire {
    Regular { blob: String, mode: u32 },
    Symlink { blob: String },
    Absent,
}

/// What a holder records from a [`WriteResult`].
enum HolderRecord {
    Revert(NewRevert),
    Undone(u32),
}

fn hex_of(bytes: &[u8]) -> String {
    super::shadow::hex_encode(bytes)
}

fn bytes_of(text: &str) -> Result<Vec<u8>, String> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    super::shadow::hex_decode(text).ok_or_else(|| "protocol: a review result's bytes are not hex".to_string())
}

impl SavedWire {
    fn of(saved: &Saved) -> SavedWire {
        match saved {
            Saved::Regular { blob, mode } => SavedWire::Regular {
                blob: blob.clone(),
                mode: *mode,
            },
            Saved::Symlink { blob } => SavedWire::Symlink { blob: blob.clone() },
            Saved::Absent => SavedWire::Absent,
        }
    }

    fn into_undo(self) -> UndoState {
        match self {
            SavedWire::Regular { blob, mode } => UndoState::Regular { blob, mode },
            SavedWire::Symlink { blob } => UndoState::Symlink { blob },
            SavedWire::Absent => UndoState::Absent,
        }
    }
}

impl WriteResult {
    /// `None` for a write that wrote nothing: there is nothing to record.
    fn of(op: &Op, done: &Done) -> Option<WriteResult> {
        match (op, done) {
            (Op::Revert { .. }, Done::Revert(Ok(applied))) => Some(WriteResult::Revert {
                turn: applied.turn,
                path: applied.path.clone(),
                hunk: applied.hunk.clone(),
                shape: match shape_of(applied) {
                    RevertShape::Lines { from, to } => ShapeWire::Lines { from, to },
                    RevertShape::Deleted => ShapeWire::Deleted,
                    RevertShape::Restored => ShapeWire::Restored,
                    RevertShape::WholeFile => ShapeWire::WholeFile,
                },
                at_line: applied.at_line,
                reverted_to: hex_of(&applied.reverted_to),
                replaced: hex_of(&applied.replaced),
                pre: SavedWire::of(&applied.pre),
                post: SavedWire::of(&applied.post),
            }),
            (Op::Undo { record, .. }, Done::Undo(Ok(()))) => Some(WriteResult::Undo { record: *record }),
            _ => None,
        }
    }

    fn into_record(self) -> Result<HolderRecord, String> {
        match self {
            WriteResult::Revert {
                turn,
                path,
                hunk,
                shape,
                at_line,
                reverted_to,
                replaced,
                pre,
                post,
            } => {
                check_relative(&path)?;
                Ok(HolderRecord::Revert(NewRevert {
                    turn,
                    path,
                    hunk,
                    shape: match shape {
                        ShapeWire::Lines { from, to } => RevertShape::Lines { from, to },
                        ShapeWire::Deleted => RevertShape::Deleted,
                        ShapeWire::Restored => RevertShape::Restored,
                        ShapeWire::WholeFile => RevertShape::WholeFile,
                    },
                    source: RevertSource::Panel,
                    at_line,
                    reverted_to: bytes_of(&reverted_to)?,
                    replaced: bytes_of(&replaced)?,
                    undo: Some(UndoData {
                        pre: pre.into_undo(),
                        post: post.into_undo(),
                    }),
                }))
            }
            WriteResult::Undo { record } => Ok(HolderRecord::Undone(record)),
        }
    }
}

const NOT_RECORDED: &str =
    "reverted, but the tab closed before it was recorded; the replaced bytes are kept in the review store";
const UNDO_NOT_RECORDED: &str = "undone, but the tab closed before it was recorded";

fn answer(request_id: &str, payload: String) -> Out {
    Out::Answer {
        request_id: request_id.to_string(),
        payload,
    }
}

fn failed(request_id: &str, why: &str) -> Out {
    answer(request_id, serialize_command_result_for_js(request_id, Err(why)))
}

fn ok(request_id: &str) -> Out {
    answer(request_id, serialize_command_result_for_js(request_id, Ok(())))
}

fn ok_with(request_id: &str, message: &str) -> Out {
    answer(
        request_id,
        serialize_command_ok_with_message_for_js(request_id, message),
    )
}

/// The tab's draft as the panel draws it, as the answer to `request_id` or on its own; `None` when
/// the tab is gone.
fn draft_envelope(request_id: Option<&str>, tab: TabId, tabs: &TabSet) -> Option<Out> {
    let draft = tabs.review_draft(tab).ok()?;
    let payload = serialize_review_draft_for_js(request_id, tab, draft);
    Some(match request_id {
        Some(request_id) => answer(request_id, payload),
        None => Out::Draft(tab, payload),
    })
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

/// Whether `tab` is still here, staying, and holding `session`: otherwise its live drawings go.
fn holds_session(tabs: &TabSet, tab: u64, session: &str) -> bool {
    !tabs.is_moving(TabId(tab))
        && tabs
            .get(TabId(tab))
            .is_some_and(|found| found.provider_session_id().as_deref() == Some(session))
}

/// The absolute path of a file an edit call named (absolute, or relative to the project root), when
/// it names one inside the project in UTF-8; decided from the text alone, on the GTK thread. The
/// worker's read checks the file itself from the project root.
fn live_path(root: &Path, named: &Path) -> Option<PathBuf> {
    // A tool's path is the model's text: `dir/../project/a.rs` names the same file as `project/a.rs`
    // when no link is on the way, so it is read by its text (`.` gone, `..` taking the name before it)
    // and must then lie under the root. Reading the disk here would wait on the GTK thread.
    let mut clean = PathBuf::new();
    for component in root.join(named).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !clean.pop() {
                    return None;
                }
            }
            other => clean.push(other.as_os_str()),
        }
    }
    let rel = clean.strip_prefix(root).ok()?;
    check_relative(rel.to_str()?).ok()?;
    Some(root.join(rel))
}

/// The absolute path of the file a path with `..` in it names, as the disk resolves it: the deepest
/// part of its directory that exists is resolved with its links, the rest (names not there yet) is
/// read by its text, and the file's own name is kept. Only a path that ends under the project root,
/// both resolved, is kept; when nothing of it exists, its text decides as [`live_path`] does.
/// Reads the disk: never on the GTK thread.
fn resolve_live_path(root: &Path, named: &Path) -> Option<PathBuf> {
    let full = root.join(named);
    let mut parts: Vec<Component> = full.components().collect();
    let Some(Component::Normal(leaf)) = parts.pop() else {
        return None;
    };
    let Ok(real_root) = std::fs::canonicalize(root) else {
        return live_path(root, named);
    };
    for n in (1..=parts.len()).rev() {
        if parts[..n]
            .iter()
            .all(|c| matches!(c, Component::RootDir | Component::Prefix(_)))
        {
            break;
        }
        let prefix: PathBuf = parts[..n].iter().collect();
        let Ok(mut real) = std::fs::canonicalize(&prefix) else {
            continue;
        };
        for part in &parts[n..] {
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !real.pop() {
                        return None;
                    }
                }
                Component::Normal(name) => real.push(name),
                _ => return None,
            }
        }
        real.push(leaf);
        let rel = real.strip_prefix(&real_root).ok()?;
        check_relative(rel.to_str()?).ok()?;
        return Some(root.join(rel));
    }
    live_path(root, named)
}

/// Whether `abs` is drawn live for another turn than `session`'s `turn`.
fn live_of_another_turn(overlay: &Overlay, abs: &Path, session: &str, turn: u32) -> bool {
    overlay
        .live_drawings()
        .get(abs)
        .is_some_and(|(drawn_session, drawn_turn)| drawn_session != session || *drawn_turn != turn)
}

fn no_worker() -> ReviewError {
    ReviewError::Unavailable("the worker could not be started or stopped".to_string())
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
    match tab.backend() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotted_live_edit_path_is_resolved_against_the_disk() {
        let dir = crate::test_scratch_dir::ScratchDir::new("live-path", "dotted");
        let root = dir.join("project");
        std::fs::create_dir_all(root.join("other/sub")).unwrap();
        std::fs::write(root.join("other/a.txt"), "x").unwrap();
        std::fs::write(root.join("a.txt"), "y").unwrap();
        std::os::unix::fs::symlink(root.join("other/sub"), root.join("alias")).unwrap();
        std::fs::create_dir_all(dir.join("run")).unwrap();
        // `alias/..` is `other`, where the link leads: not the root, as the text alone would say.
        assert_eq!(live_path(&root, Path::new("alias/../a.txt")), Some(root.join("a.txt")));
        assert_eq!(
            resolve_live_path(&root, Path::new("alias/../a.txt")),
            Some(root.join("other/a.txt"))
        );
        assert_eq!(
            resolve_live_path(&root, &root.join("alias/../a.txt")),
            Some(root.join("other/a.txt"))
        );
        // A directory beside the root, reached through `..`, as the window pass's tool names its file.
        assert_eq!(
            resolve_live_path(&root, &dir.join("run/../project/a.txt")),
            Some(root.join("a.txt"))
        );
        // A file not there yet, under a directory not there yet: the missing part is read by its text.
        assert_eq!(
            resolve_live_path(&root, Path::new("new/../other/b.txt")),
            Some(root.join("other/b.txt"))
        );
        // Out of the root, through the link or past it.
        assert_eq!(resolve_live_path(&root, Path::new("alias/../../../x.txt")), None);
        assert_eq!(resolve_live_path(&root, Path::new("../x.txt")), None);
        assert_eq!(resolve_live_path(&root, Path::new("other/..")), None);
    }

    #[test]
    fn a_live_edit_path_is_read_by_its_text_and_kept_inside_the_root() {
        let root = Path::new("/w/project");
        for (named, want) in [
            ("/w/project/a.txt", Some("/w/project/a.txt")),
            ("/w/run/../project/a.txt", Some("/w/project/a.txt")),
            ("/w/project/./src/../a.txt", Some("/w/project/a.txt")),
            ("src/a.rs", Some("/w/project/src/a.rs")),
            ("src/../a.txt", Some("/w/project/a.txt")),
            ("../other/a.txt", None),
            ("/w/project/../other/a.txt", None),
            ("/w/project", None),
            ("/w/projectx/a.txt", None),
            ("/../../a.txt", None),
        ] {
            assert_eq!(live_path(root, Path::new(named)), want.map(PathBuf::from), "{named}");
        }
    }

    fn applied(reverted_to: &[u8], replaced: &[u8]) -> AppliedRevert {
        AppliedRevert {
            session: "s1".into(),
            turn: 3,
            scope: Scope::Turn,
            path: "src/a b.rs".into(),
            kind: RevertKind::Hunk {
                base: (4, 2),
                end: (4, 1),
            },
            hunk: Some((2, "@@ -4,2 +4,1 @@".into())),
            reverted_to: reverted_to.to_vec(),
            replaced: replaced.to_vec(),
            at_line: 4,
            pre: Saved::Regular {
                blob: "aaaa".into(),
                mode: 0o644,
            },
            post: Saved::Symlink { blob: "bbbb".into() },
            at_ms: 7,
        }
    }

    fn revert_op() -> Op {
        Op::Revert {
            turn: 3,
            scope: Scope::Turn,
            path: "src/a b.rs".into(),
            target: RevertTarget::File,
        }
    }

    #[test]
    fn a_revert_finished_for_a_moved_chat_is_recorded_by_its_holder_as_written() {
        // Bytes that are not text, and an empty side, cross the link unchanged.
        let applied = applied(b"\xff\x00line\r\n", b"");
        let result = WriteResult::of(&revert_op(), &Done::Revert(Ok(applied.clone()))).unwrap();
        let value = serde_json::to_value(&result).unwrap();
        assert!(value.get("write").is_some(), "{value}");
        let back: WriteResult = serde_json::from_value(value).unwrap();
        let HolderRecord::Revert(record) = back.into_record().unwrap() else {
            panic!("a revert came back as something else");
        };
        assert_eq!(
            record,
            NewRevert {
                turn: 3,
                path: "src/a b.rs".into(),
                hunk: Some((2, "@@ -4,2 +4,1 @@".into())),
                shape: shape_of(&applied),
                source: RevertSource::Panel,
                at_line: 4,
                reverted_to: b"\xff\x00line\r\n".to_vec(),
                replaced: Vec::new(),
                undo: Some(UndoData {
                    pre: saved_to_undo(applied.pre.clone()),
                    post: saved_to_undo(applied.post.clone()),
                }),
            }
        );
    }

    #[test]
    fn a_write_that_wrote_nothing_has_nothing_for_the_holder() {
        assert!(WriteResult::of(&revert_op(), &Done::Revert(Err(Refusal::Cancelled))).is_none());
        let undo = Op::Undo {
            record: 5,
            path: "a".into(),
            pre: Saved::Absent,
            post: Saved::Absent,
        };
        assert_eq!(
            WriteResult::of(&undo, &Done::Undo(Ok(()))),
            Some(WriteResult::Undo { record: 5 })
        );
    }

    #[test]
    fn a_holder_result_naming_a_path_outside_the_project_is_refused() {
        let value = serde_json::json!({
            "write": "revert", "turn": 1, "path": "../outside", "hunk": null,
            "shape": { "shape": "deleted" }, "at_line": 1, "reverted_to": "", "replaced": "",
            "pre": { "kind": "absent" }, "post": { "kind": "absent" }
        });
        let result: WriteResult = serde_json::from_value(value).unwrap();
        assert!(result.into_record().is_err());
    }
}
