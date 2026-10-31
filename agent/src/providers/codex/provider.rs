//! `CodexProvider`: one `codex app-server` child per tab, driven over [`JsonRpcLink`].
//!
//! The provider is an [`AgentProvider`] for the ingestion thread, which calls [`pump`](AgentProvider::pump)
//! and gets events, and an [`ExternalProvider`] for the host, which starts a session and sends turns under
//! the policy of the tab's mode. The plain Claude-shaped calls refuse.
//!
//! **Nothing blocks the thread that pumps.** Only `start` and `send_turn_with` wait for an answer, each for
//! a bounded time and without holding either lock; `turn/interrupt` is queued and never waited on. The
//! link's reader thread files an answer in its own slot, so a pump that runs while a caller waits sees
//! only notifications.
//!
//! **A turn starts when Codex says so.** `TurnStarted` is emitted from the `turn/start` answer, which is
//! where the turn id is. Notifications that arrive before the caller has seen that answer are held and
//! released after it, in order, so a delta never reaches the projection ahead of its turn.
//!
//! **Every request from Codex is answered.** An approval Eitri can show becomes a card and waits for the
//! user (or bypass) through `resolve_permission`; one it cannot show, or must never grant, is answered at
//! once with a note on the call's row (`approvals`). Anything else gets "method not found": an unanswered
//! request would hang the agent. Nothing is ever granted by default, and no answer is remembered.

use super::approvals::{self, AnswerForm, CardAnswer, Handling, HeldItems, HeldItemsRecord};
use super::config::{expected_echo, session_config};
use super::family::{Family, FamilyEdge, Full, MEMBERS_MAX, UNREAD_TURN};
use super::resume;
use super::self_check::{ensure_checked, BinaryKey, SelfCheckError};
use super::translate::{self, parse_user_agent_version, version_text, SIGNED_OUT};
use super::tripwire::{self, PolicyMismatch, TrustWatch};
use super::wire::{
    self, method, AccountReadParams, AccountReadResponse, InitializeParams, InitializeResponse, ItemDelta,
    ItemNotification, ServerNotification, SubAgentKind, ThreadItem, ThreadResumeParams, ThreadStartParams,
    ThreadStartResponse, ThreadUnsubscribeParams, TurnInterruptParams, TurnNotification, TurnStartParams,
    TurnStartResponse, TurnStatus, TurnSteerParams,
};
use crate::external::{ExternalProvider, ExternalStart, Reviewer, TurnPolicy, ANSWERED_BY_EITRI};
use crate::external_link::{self, lock, provider_error, ClosingNotice};
use crate::jsonrpc_link::{Framing, Incoming, JsonRpcLink, LinkError, LinkHandoff, LinkSpec, Pending, Skipped};
use crate::projection::{
    thinking_open_after, ContentKind, PermissionOutcome, ResumeStatus, TurnEndDetail, TurnOutcome,
};
use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, PermissionDecision,
    ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest,
    ResumeSessionRequest, SendTurnRequest, SteerTurnRequest,
};
use crate::provider_move::{at_offset, forget_unclaimed, handed_ids, offset_ms, MovedAnswer, START_WAIT};
use crate::steer_gate::{SteerEntry, SteerOrder};
use crate::{AgentDomainEvent, MoveRefusedWhy, UNARY_RPC_TIMEOUT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The oldest Codex Eitri drives. Below it a start is refused, naming both versions.
pub const MIN_CODEX: (u32, u32, u32) = (0, 160, 0);

/// The newest Codex whose traffic was recorded against this code and whose scripted suite and self-check
/// passed. A newer one runs, with a note.
pub const HIGHEST_TESTED_CODEX: (u32, u32, u32) = (0, 161, 0);

/// How long the handshake and the session start may take.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long reading the history of a resumed thread may take in all; one page waits as long as a turn start.
const HISTORY_BUDGET: Duration = Duration::from_secs(30);
/// How often a history page's answer is looked for, moving aside what Codex says meanwhile.
const HISTORY_POLL: Duration = Duration::from_millis(2);
/// The `turn/start` answer is immediate when Codex is healthy.
const TURN_START_TIMEOUT: Duration = UNARY_RPC_TIMEOUT;
/// How long a close waits for Codex to confirm the unsubscribe before it closes the pipe anyway.
const UNSUBSCRIBE_WAIT: Duration = Duration::from_secs(3);
/// Notifications and requests read per pump, so one call cannot run away with a flood.
const PUMP_BATCH: usize = 4096;
/// Batches a send reads before its turn goes out; past them the send is refused for now.
const SEND_DRAIN_BATCHES: usize = 8;
/// Unknown method names remembered so each is logged once.
const UNKNOWN_METHODS_KEPT: usize = 64;
/// Turns of threads not (yet) in the family remembered at most, for a child announced after its turn began.
const EARLY_TURNS_KEPT: usize = 64;
/// How long after an interrupt the tab's turn waits for the calls Codex was running to end. Codex ends an
/// interrupted turn at once but lets a running command go on to its own end, so a command or file change
/// still running when this has passed is stopped with the whole session.
pub const INTERRUPT_SETTLE: Duration = Duration::from_secs(3);
/// How long after Codex refused a `turn/interrupt` it is sent again, while the turn still runs. Codex refuses
/// one that comes the moment `turn/start` was answered ("no active turn to interrupt") and runs the turn on.
const INTERRUPT_RETRY: Duration = Duration::from_millis(100);
/// How many times one turn's refused interrupt is sent again: as many as fit in [`INTERRUPT_SETTLE`], after
/// which a tab's turn is stopped anyway and a turn with no tab's turn around it (one Codex started after its
/// start timed out) is not asked forever.
const INTERRUPT_RESENDS: u32 = 30;
/// Running calls remembered at most; past it something always counts as still running.
const RUNNING_CALLS_KEPT: usize = 4096;

/// JSON-RPC's "method not found".
const METHOD_NOT_FOUND: i64 = -32601;

/// The prefix of the id of a row Eitri adds for a request it answered that has no row of its own.
const ANSWERED_ROW_PREFIX: &str = "eitri-answered-";
/// What `PermissionDenied.reason_type` says when Codex's own reviewer refused a call, or ran out of time,
/// which Codex treats the same.
const REVIEW_DENIED: &str = "auto_review";
/// What `PermissionDenied.reason_type` says when Codex's own reviewer let a call through. The event is the
/// one note a call's row carries about who decided it, whichever way.
const REVIEW_APPROVED: &str = "auto_review_approved";
/// The prefix of the id of a row Eitri adds for a review that names no call (a network request).
const REVIEW_ROW_PREFIX: &str = "eitri-review-";
/// The longest reviewer's rationale a note carries.
const REVIEW_RATIONALE_CHARS: usize = 2000;

/// The notice for a Codex that is not installed.
pub const NOT_FOUND: &str = "codex not found: install it and run `codex login`, or set EITRI_CODEX";

/// Where and how a Codex child is started.
#[derive(Debug, Clone)]
pub struct CodexLaunch {
    /// Absolute: the child runs in the untrusted project root, where a relative name would resolve.
    pub program: PathBuf,
    /// The project root, canonical. The child's working directory.
    pub cwd: PathBuf,
    /// Eitri's own state directory, where the per-binary check keeps its verdicts.
    pub state_home: PathBuf,
}

/// What belongs to one running turn.
struct ActiveTurn {
    /// `None` between sending `turn/start` and reading its answer.
    id: Option<String>,
    /// The user asked to interrupt before the turn had an id.
    interrupt_wanted: bool,
    /// The turns of the family that have been sent their one `turn/interrupt`, or whose waiting card was
    /// answered `cancel`, which ends a turn by itself.
    interrupted: HashSet<String>,
    /// The turns in `interrupted` only because a card of theirs was cancelled. Should one still run when the
    /// user presses interrupt again, `cancel` did not end it, and it is sent a `turn/interrupt` after all.
    cancelled: HashSet<String>,
    /// The user pressed interrupt after the tab's own turn had ended, so only subagents were left to stop.
    interrupt_asked: bool,
    /// The user pressed interrupt: a family turn learnt about later is sent its interrupt too.
    interrupting: bool,
    /// How the tab's own thread's turn ended, once it has; the family may still be working then.
    own_end: Option<(TurnOutcome, TurnEndDetail)>,
    /// When the first interrupt was pressed, plus [`INTERRUPT_SETTLE`]: whatever still changes things after
    /// it stops the session.
    settle_until: Option<Instant>,
    /// When each turn in `interrupted` was first told to stop. One still running [`INTERRUPT_SETTLE`] later
    /// stops the session, whatever Codex answered.
    told_to_stop: HashMap<String, Instant>,
    /// Every turn of the family has ended after an interrupt, and the tab's turn waits for the calls still
    /// running before it ends.
    settling: bool,
    /// The last whole assistant message of the turn: the turn's result text.
    last_message: String,
    /// The thread's running token total as of the last report.
    usage: Option<wire::TokenUsage>,
    /// The reviewer this turn's `turn/start` named; `None` for a turn a subagent opened while the tab was
    /// idle, which Eitri did not start.
    reviewer: Option<Reviewer>,
}

impl ActiveTurn {
    fn new(id: Option<String>) -> ActiveTurn {
        ActiveTurn {
            id,
            interrupt_wanted: false,
            interrupted: HashSet::new(),
            cancelled: HashSet::new(),
            interrupt_asked: false,
            interrupting: false,
            own_end: None,
            settle_until: None,
            told_to_stop: HashMap::new(),
            settling: false,
            last_message: String::new(),
            usage: None,
            reviewer: None,
        }
    }
}

/// Item types that run nothing and write nothing (a search, a plan, a look at an image, a pause, review
/// markers, a compaction, a hook's prompt, an output record), so an interrupt does not wait for them.
const QUIET_ITEM_TYPES: &[&str] = &[
    "webSearch",
    "plan",
    "imageView",
    "sleep",
    "enteredReviewMode",
    "exitedReviewMode",
    "contextCompaction",
    "hookPrompt",
    "functionCallOutput",
];

/// What a call Codex started and has not reported ended is, for deciding what an interrupt left running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Running {
    Command,
    FileChange,
    /// An item whose kind or start Eitri could not read, which may be either.
    Unread,
    /// A call Eitri does not stop the session for: a subagent call, which ends with its child, or a tool
    /// server's call, which may be a long read as well as anything else. It is waited on only up to the
    /// settle time, so what a tool server writes after that is not in the turn's end snapshot.
    Other,
}

impl Running {
    fn of(item: &ThreadItem) -> Option<Running> {
        match item {
            ThreadItem::CommandExecution(_) => Some(Running::Command),
            ThreadItem::FileChange(_) => Some(Running::FileChange),
            // What Codex 0.161 names in its schema that runs nothing and writes nothing: not waited on.
            ThreadItem::Unknown { item_type, .. } if QUIET_ITEM_TYPES.contains(&item_type.as_str()) => None,
            // Anything else it may add, an image it saves, a tool Eitri never offered, a known type that
            // did not read: may be writing.
            ThreadItem::Unknown { .. } | ThreadItem::Malformed { .. } => Some(Running::Unread),
            ThreadItem::McpToolCall(_) | ThreadItem::CollabAgentToolCall(_) => Some(Running::Other),
            ThreadItem::UserMessage(_)
            | ThreadItem::AgentMessage(_)
            | ThreadItem::Reasoning(_)
            | ThreadItem::SubAgentActivity(_) => None,
        }
    }

    /// What the tab says Codex kept doing, or `None` for a call that changes nothing by itself.
    fn kept_doing(self) -> Option<&'static str> {
        match self {
            Running::Command => Some("kept running a command"),
            Running::FileChange => Some("kept applying a file change"),
            Running::Unread => Some("kept running something Eitri could not read"),
            Running::Other => None,
        }
    }
}

/// What the tab says when an interrupt left `kept_doing` running and the session was stopped for it.
pub fn stopped_after_interrupt(kept_doing: &str) -> String {
    format!(
        "Codex {kept_doing} after the interrupt; its session was stopped so nothing more runs. \
         The conversation can be resumed."
    )
}

/// What the tab says when the user stopped the calls Codex left running in the background.
pub const STOPPED_BACKGROUND: &str =
    "What Codex left running in the background was stopped with its session. The conversation can be resumed.";

/// A `turn/interrupt` sent, and its answer.
struct SentInterrupt {
    thread_id: String,
    turn_id: String,
    /// The answer, until it is read: a refusal is read once, and the interrupt is then sent again.
    answer: Option<Pending>,
    /// Codex refused it: it is sent again then, should the turn still run.
    again_at: Option<Instant>,
    /// How many times this turn's interrupt was sent again before this one.
    resent: u32,
}

/// The session being stopped: something kept running after an interrupt, or the user stopped what was left
/// running in the background.
struct Stopping {
    /// Set once the child and everything it started are gone.
    done: Arc<AtomicBool>,
    /// The thread doing it, which a close waits for: a window that went before the group was killed would
    /// leave the command running.
    worker: Option<std::thread::JoinHandle<()>>,
    /// What the session's end says.
    reason: String,
}

/// Sets its flag when dropped, so a stop that panics, or a thread that could not start, still ends the turn.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A card waiting for its answer.
#[derive(Debug, Serialize, Deserialize)]
struct WaitingCard {
    /// Codex's id of the request, which the answer goes back under.
    request_id: Value,
    form: AnswerForm,
    /// The call the card sits on.
    item_id: String,
    /// The thread that asked: the tab's, or a subagent's.
    thread_id: String,
    /// The row Eitri added for the card because its call had none, which ends with the card's answer.
    added_row: Option<AddedRow>,
}

/// A row Eitri drew for a card whose call had no row of its own (a network request names Codex's own key,
/// and no item is ever started for it).
#[derive(Debug, Serialize, Deserialize)]
struct AddedRow {
    row: String,
    /// The tab turn the row was filed under.
    turn_id: String,
    /// What the card asked about, which the row's result repeats with the answer.
    subject: String,
}

#[derive(Default)]
struct State {
    thread_id: Option<String>,
    /// The tab's thread and the subagents announced from it; set when the session starts.
    family: Option<Family>,
    model: Option<String>,
    events: VecDeque<AgentDomainEvent>,
    /// Messages read while a `turn/start` answer was awaited.
    held: VecDeque<Incoming>,
    turn: Option<ActiveTurn>,
    /// Bytes already emitted for each streaming item of the running turn.
    items: HashMap<String, usize>,
    /// The text already emitted for each assistant message of the running turn, so its final text can be
    /// told to extend it or not.
    message_text: HashMap<String, String>,
    /// Rows of the running turn's tool calls that have started and not completed, by row id
    /// (`translate::row_id`).
    open_tools: HashSet<String>,
    /// The calls of the running turn a request may name, as Codex announced them.
    calls: HeldItems,
    /// Cards waiting for an answer, by permission id, in the order they were drawn.
    cards: Vec<(String, WaitingCard)>,
    /// Numbers the cards of this connection.
    next_card: u64,
    /// Numbers the rows Eitri adds for requests it answered that had no row of their own.
    next_answered_row: u64,
    /// Rows Eitri added for a card and ended with its answer. Should Codex report that call after all, its
    /// start draws no second row and its end gives the row Codex's own result.
    answered_rows: HashSet<String>,
    seen_unknown: HashSet<String>,
    /// The running turn of each thread outside the family that started one, by thread. A child can start
    /// its turn before the message that makes it a member is read; it is followed from that turn once it
    /// is.
    early_turns: HashMap<String, String>,
    last_limits: Option<translate::LimitFigure>,
    /// What the session's start reported about the project configuration it loaded, and what to look for
    /// while it runs.
    watch: TrustWatch,
    /// Why the session may not take turns any more: Codex reported a policy other than the one asked for,
    /// loaded what an untrusted session may not, or refused the resume.
    blocked: Option<&'static str>,
    /// A resumed thread's history is being read. What arrives meanwhile is held, so the history reaches the
    /// projection first.
    history_loading: bool,
    /// The resumed thread's history, until the host takes it (`take_history`). Never pumped: a past turn
    /// delivered live would read to the host as one that just ran.
    history: Vec<AgentDomainEvent>,
    /// The calls Codex has started and not reported ended, on any thread of this connection, by thread and
    /// item. Kept across turns: a command can outlive the turn that started it.
    running: HashMap<(String, String), Running>,
    /// A call started that could not be told apart, or more were running than are remembered: something
    /// counts as running for as long as this connection lasts.
    running_unread: bool,
    /// Something kept running after an interrupt and the child is being stopped for it.
    stopping: Option<Stopping>,
    /// The `turn/interrupt` requests whose answers are still read: a refused one is sent again while its
    /// turn runs.
    interrupts: Vec<SentInterrupt>,
    /// The child was stopped by Eitri and is gone, the conversation whole: it can be continued.
    stopped_whole: bool,
    /// The calls of `running` that were still running when the tab's turn last ended: put in the background
    /// by the agent. A running turn's own calls join them only when it ends.
    background: HashSet<(String, String)>,
    /// Open rows of calls left running by a completed turn, with the turn that drew each row.
    background_rows: HashMap<(String, String), (String, String)>,
    /// `running_unread` was set when a turn ended: something unreadable may be running in the background.
    background_unread: bool,
    /// The count last sent as `BackgroundCalls`.
    background_reported: u32,
    /// A `turn/start` got no answer in time and the user was told the turn did not start. Should Codex
    /// start it after all, it is interrupted at once.
    start_abandoned: bool,
    /// The turns, by thread and turn id, that run under reviewer `user`: each one Eitri started naming it,
    /// and each subagent turn that began inside one. Codex's own reviewer has no business in any of them;
    /// a turn that started before (an Auto turn, or one Codex keeps running after an interrupt) is never
    /// in here, so what its reviewer does later is not mistaken for this.
    user_reviewed: HashSet<(String, String)>,
    /// Messages sent into the running turn with `turn/steer` whose fate Codex has not told yet, oldest
    /// first. Each ends as `UserMessageDelivered` (its `userMessage` item came) or `UserMessageDropped`
    /// (refused, or the turn or session ended first: Codex forgets a message it had not read when the turn
    /// is interrupted, measured on 0.162.1).
    steers: SteerOrder<SentSteer>,
    /// The child ended on its own and its notice waits for the last of its stderr: the reason the link
    /// gave, and when to give up waiting. Finished by `pump`, so nothing sleeps while holding this state.
    closing_notice: Option<ClosingNotice>,
    /// Closed by the host.
    closed: bool,
    /// The child ended on its own, or was stopped.
    dead: bool,
    /// Whether the projection's thinking block is open after the events queued so far
    /// (`thinking_open_after`), followed through `events` up to `thought_scanned`.
    thought_open: bool,
    thought_scanned: usize,
    /// The reasoning item and part the last thought sent came from.
    thought_part: Option<(String, ThoughtPart)>,
    /// When a chat taken up from another process was adopted, until its move is committed. Codex's
    /// output is not read before the commit, so nothing is pumped then and no deadline may run out: one
    /// that did would stop a session whose turn may well have ended in what is still unread, and the
    /// parked link would be dropped without the child it does not own yet.
    adopted_at: Option<Instant>,
}

/// Which part of a reasoning item a thought came from. Codex writes a reasoning item as summary parts
/// (or raw content parts) and a past turn's item joins them with a blank line; a thought from another
/// part, or from another item, is given the same blank line live, so the text reads the same live and
/// resumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum ThoughtPart {
    Summary(Option<i64>),
    Content(Option<i64>),
    /// An item that streamed nothing and arrived whole.
    Whole,
}

/// A message sent into the running turn.
struct SentSteer {
    client_id: String,
    /// What the user typed, for the transcript row once Codex has read it.
    as_typed: String,
    /// The answer to `turn/steer`, until it is read.
    answer: Option<Pending>,
    /// The request, not sent yet because an earlier message's answer has not come: Codex may still refuse
    /// that one, and a later message must never reach the model before an earlier one.
    waiting: Option<Value>,
}

/// A Codex tab's provider state as data, taken when its chat moves to another process: every field of the
/// provider's state is here, rebuilt there, or named where it is taken as the reason it does not move.
/// The connection's own state crosses beside it as the link's [`LinkHandoff`].
#[derive(Debug, Serialize, Deserialize)]
pub struct CodexProviderHandoff {
    version: (u32, u32, u32),
    state_home: PathBuf,
    codex_home: PathBuf,
    steer_unsupported: bool,
    thread_id: Option<String>,
    family: Option<Family>,
    model: Option<String>,
    /// Folded already and not pumped yet when the chat froze.
    events: VecDeque<AgentDomainEvent>,
    held: VecDeque<Incoming>,
    turn: Option<MovedTurn>,
    items: HashMap<String, usize>,
    message_text: HashMap<String, String>,
    open_tools: HashSet<String>,
    calls: HeldItemsRecord,
    cards: Vec<(String, WaitingCard)>,
    next_card: u64,
    next_answered_row: u64,
    answered_rows: HashSet<String>,
    seen_unknown: HashSet<String>,
    early_turns: HashMap<String, String>,
    last_limits: Option<translate::LimitFigure>,
    watch: TrustWatch,
    blocked: Option<String>,
    history: Vec<AgentDomainEvent>,
    /// By thread and item: a list, since a map with a pair for its key has no JSON form.
    running: Vec<((String, String), Running)>,
    running_unread: bool,
    interrupts: Vec<MovedInterrupt>,
    stopped_whole: bool,
    background: HashSet<(String, String)>,
    /// By thread and item, as a list so the pair keys have a JSON form.
    background_rows: Vec<((String, String), (String, String))>,
    background_unread: bool,
    background_reported: u32,
    start_abandoned: bool,
    user_reviewed: HashSet<(String, String)>,
    steers: Vec<MovedSteer>,
    /// The reason, and the time left to wait for the last of stderr, in milliseconds from the freeze.
    closing_notice: Option<(String, i64)>,
    thought_open: bool,
    thought_scanned: usize,
    thought_part: Option<(String, ThoughtPart)>,
}

/// [`ActiveTurn`] as data: its moments as milliseconds from the freeze.
#[derive(Debug, Serialize, Deserialize)]
struct MovedTurn {
    id: Option<String>,
    interrupt_wanted: bool,
    interrupted: HashSet<String>,
    cancelled: HashSet<String>,
    interrupt_asked: bool,
    interrupting: bool,
    own_end: Option<(TurnOutcome, TurnEndDetail)>,
    settle_until_ms: Option<i64>,
    told_to_stop_ms: HashMap<String, i64>,
    settling: bool,
    last_message: String,
    usage: Option<wire::TokenUsage>,
    /// Whether the turn's reviewer is the user (`true`) or the agent (`false`).
    reviewed_by_user: Option<bool>,
}

/// [`SentInterrupt`] as data: its answer by request id, or the answer itself if it had come.
#[derive(Debug, Serialize, Deserialize)]
struct MovedInterrupt {
    thread_id: String,
    turn_id: String,
    answer: Option<MovedAnswer>,
    again_at_ms: Option<i64>,
    resent: u32,
}

/// [`SentSteer`] as data: its answer by request id, or the answer itself if it had come.
#[derive(Debug, Serialize, Deserialize)]
struct MovedSteer {
    client_id: String,
    as_typed: String,
    answer: Option<MovedAnswer>,
    waiting: Option<Value>,
}

fn invalid(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

impl ActiveTurn {
    fn into_moved(self, now: Instant) -> MovedTurn {
        let ActiveTurn {
            id,
            interrupt_wanted,
            interrupted,
            cancelled,
            interrupt_asked,
            interrupting,
            own_end,
            settle_until,
            told_to_stop,
            settling,
            last_message,
            usage,
            reviewer,
        } = self;
        MovedTurn {
            id,
            interrupt_wanted,
            interrupted,
            cancelled,
            interrupt_asked,
            interrupting,
            own_end,
            settle_until_ms: settle_until.map(|at| offset_ms(at, now)),
            told_to_stop_ms: told_to_stop
                .into_iter()
                .map(|(turn, at)| (turn, offset_ms(at, now)))
                .collect(),
            settling,
            last_message,
            usage,
            // Matched in full, so a reviewer added later fails to build here instead of crossing as the
            // agent, the side that asks the user less.
            reviewed_by_user: reviewer.map(|r| match r {
                Reviewer::User => true,
                Reviewer::Agent => false,
            }),
        }
    }

    fn from_moved(moved: MovedTurn, now: Instant) -> ActiveTurn {
        let MovedTurn {
            id,
            interrupt_wanted,
            interrupted,
            cancelled,
            interrupt_asked,
            interrupting,
            own_end,
            settle_until_ms,
            told_to_stop_ms,
            settling,
            last_message,
            usage,
            reviewed_by_user,
        } = moved;
        ActiveTurn {
            id,
            interrupt_wanted,
            interrupted,
            cancelled,
            interrupt_asked,
            interrupting,
            own_end,
            settle_until: settle_until_ms.map(|ms| at_offset(ms, now)),
            told_to_stop: told_to_stop_ms
                .into_iter()
                .map(|(turn, ms)| (turn, at_offset(ms, now)))
                .collect(),
            settling,
            last_message,
            usage,
            reviewer: reviewed_by_user.map(|user| if user { Reviewer::User } else { Reviewer::Agent }),
        }
    }
}

impl SentInterrupt {
    fn into_moved(self, now: Instant) -> MovedInterrupt {
        let SentInterrupt {
            thread_id,
            turn_id,
            mut answer,
            again_at,
            resent,
        } = self;
        MovedInterrupt {
            thread_id,
            turn_id,
            answer: answer.as_mut().map(MovedAnswer::of),
            again_at_ms: again_at.map(|at| offset_ms(at, now)),
            resent,
        }
    }

    fn from_moved(moved: MovedInterrupt, link: &JsonRpcLink, now: Instant) -> io::Result<SentInterrupt> {
        let MovedInterrupt {
            thread_id,
            turn_id,
            answer,
            again_at_ms,
            resent,
        } = moved;
        Ok(SentInterrupt {
            thread_id,
            turn_id,
            answer: answer.map(|a| a.rebind(link)).transpose()?,
            again_at: again_at_ms.map(|ms| at_offset(ms, now)),
            resent,
        })
    }
}

impl SteerEntry for SentSteer {
    fn client_id(&self) -> &str {
        &self.client_id
    }

    fn waiting(&self) -> bool {
        self.waiting.is_some()
    }

    fn in_flight(&self) -> bool {
        self.answer.is_some()
    }
}

impl SentSteer {
    fn into_moved(self) -> MovedSteer {
        let SentSteer {
            client_id,
            as_typed,
            mut answer,
            waiting,
        } = self;
        MovedSteer {
            client_id,
            as_typed,
            answer: answer.as_mut().map(MovedAnswer::of),
            waiting,
        }
    }

    fn from_moved(moved: MovedSteer, link: &JsonRpcLink) -> io::Result<SentSteer> {
        let MovedSteer {
            client_id,
            as_typed,
            answer,
            waiting,
        } = moved;
        Ok(SentSteer {
            client_id,
            as_typed,
            answer: answer.map(|a| a.rebind(link)).transpose()?,
            waiting,
        })
    }
}

/// Why a session takes no more turns: Codex runs under another policy than the one asked for, or loaded
/// what an untrusted session may not.
const BLOCKED_POLICY: &str = "Codex is not running under the policy Eitri asked for";
/// Why a session takes no more turns: Codex refused to resume the conversation.
const BLOCKED_RESUME: &str = "Codex would not resume this conversation";
/// Every reason a session can be blocked for, so a moved session's reason is read back as the same one.
const BLOCKED_REASONS: [&str; 2] = [BLOCKED_POLICY, BLOCKED_RESUME];

/// The start of the id Eitri gives a turn whose start it could not read.
const UNREAD_TURN_PREFIX: &str = "eitri-unread-turn-";

/// Why an adopted chat refuses what needs its link running: its move is not committed yet.
const NOT_COMMITTED: &str = "this chat's move here is not committed yet";

/// Moves every deadline the state holds on by `by`.
fn later_deadlines(st: &mut State, by: Duration) {
    if let Some(turn) = st.turn.as_mut() {
        if let Some(until) = turn.settle_until.as_mut() {
            *until += by;
        }
        for at in turn.told_to_stop.values_mut() {
            *at += by;
        }
    }
    for sent in &mut st.interrupts {
        if let Some(at) = sent.again_at.as_mut() {
            *at += by;
        }
    }
    if let Some(notice) = st.closing_notice.as_mut() {
        notice.until += by;
    }
}

/// The longest reason a dropped message carries of Codex's own words, in characters.
const MAX_STEER_REASON_CHARS: usize = 200;

/// Why Codex did not take a message into the running turn, from its answer to `turn/steer`.
fn steer_refusal(e: &LinkError) -> String {
    match e {
        LinkError::Rpc { code, .. } if *code == METHOD_NOT_FOUND => {
            "this Codex cannot take messages while a turn runs".to_string()
        }
        LinkError::Rpc { message, .. }
            if message.contains("no active turn") || message.contains("expected active turn id") =>
        {
            "the turn had ended when it reached Codex".to_string()
        }
        LinkError::Rpc { message, .. } => {
            let words: String = message.chars().take(MAX_STEER_REASON_CHARS).collect();
            let cut = if message.chars().count() > MAX_STEER_REASON_CHARS {
                "…"
            } else {
                ""
            };
            format!("Codex did not take it: {words}{cut}")
        }
        other => format!("Codex did not take it: {other}"),
    }
}

pub struct CodexProvider {
    link: Mutex<Option<JsonRpcLink>>,
    state: Mutex<State>,
    pid: u32,
    version: (u32, u32, u32),
    state_home: PathBuf,
    /// Where Codex keeps the user's own configuration, as it reported.
    codex_home: PathBuf,
    /// Codex does not know `turn/steer`: every later message waits for the turn's end. Outside `state` so
    /// `capabilities` never waits for a pump.
    steer_unsupported: AtomicBool,
}

/// A failed call on the link as the error the host shows.
fn link_error(link: &JsonRpcLink, what: &str, e: LinkError) -> ProviderError {
    external_link::link_error("Codex", link, what, e)
}

/// The directory Codex keeps the user's own configuration in: what it reported, else what it would have
/// used. Only a file inside it is exempt from the check on instruction files, so a guess errs toward
/// reporting more, never less.
fn effective_codex_home(reported: Option<&str>) -> PathBuf {
    if let Some(dir) = reported.filter(|d| Path::new(d).is_absolute()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|d| Path::new(d).is_absolute()) {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(|home| Path::new(&home).join(".codex"))
        .unwrap_or_else(|| PathBuf::from("/nonexistent/.codex"))
}

/// The `CODEX_HOME` a child started with `env` and `env_remove` sees: what `env` sets wins, a removed one is
/// gone, and otherwise it is Eitri's own.
fn child_codex_home(env: &[(OsString, OsString)], env_remove: &[OsString]) -> Option<OsString> {
    child_var("CODEX_HOME", env, env_remove)
}

/// The value of `name` a child started with `env` and `env_remove` sees.
fn child_var(name: &str, env: &[(OsString, OsString)], env_remove: &[OsString]) -> Option<OsString> {
    if let Some((_, value)) = env.iter().rev().find(|(n, _)| n == name) {
        return Some(value.clone());
    }
    if env_remove.iter().any(|n| n == name) {
        return None;
    }
    std::env::var_os(name)
}

/// Refuses a relative `CODEX_HOME`. Codex resolves it against its working directory, which is the project
/// root, so a directory the project itself holds would be read as the user's own configuration: its MCP
/// servers, hooks, rules and skills would load whatever the user said about trusting the project. No
/// child is started under it. An empty value is Codex's own "unset"; an absolute one is judged by
/// [`refuse_home_in_project`].
fn refuse_relative_home(value: Option<&OsStr>) -> Result<(), ProviderError> {
    let Some(value) = value.filter(|v| !v.is_empty() && !Path::new(v).is_absolute()) else {
        return Ok(());
    };
    let shown = value.to_string_lossy();
    let shown = shown.escape_debug();
    Err(provider_error(
        ProviderErrorCode::InvalidConfiguration,
        format!(
            "CODEX_HOME is relative ({shown}): Codex would read the project's own {shown} as your configuration; \
             set it to an absolute path"
        ),
    ))
}

/// Refuses a Codex start under a relative `CODEX_HOME` in Eitri's own environment, the one every Codex child
/// inherits. Part of [`refuse_codex_home`], which the connect worker asks before it looks for the binary,
/// since probing its version runs a child too.
pub fn refuse_relative_codex_home() -> Result<(), ProviderError> {
    refuse_relative_home(child_codex_home(&[], &[]).as_deref())
}

/// Refuses a Codex start whose `CODEX_HOME`, as Eitri's own environment gives it to every Codex child, is
/// relative ([`refuse_relative_codex_home`]) or lies in the project `root` (see [`refuse_home_in_project`]).
/// A tab's connect worker asks before it looks for the binary; [`CodexProvider::connect`] asks again.
pub fn refuse_codex_home(root: &Path) -> Result<(), ProviderError> {
    refuse_relative_codex_home()?;
    refuse_home_in_project(
        child_codex_home(&[], &[]).as_deref(),
        std::env::var_os("HOME").as_deref(),
        root,
    )
}

/// Refuses an absolute `CODEX_HOME` (or, when it is unset or empty, `~/.codex`) that is the project root or
/// lies in it. Codex reads that directory as the user's own configuration layer whatever the user said about
/// trusting the project, and trust discovery leaves the effective `CODEX_HOME` out, so a `.codex/` the
/// project ships there would load unasked.
///
/// A root that is the user's home or holds it is not a boundary: what lies in the user's own home is not
/// something a project ships. "In the project" is judged on the path as written and on the path with its
/// links resolved, and either is enough: a link in the project that leads elsewhere is the project's to
/// change, so it never makes a home count as outside.
fn refuse_home_in_project(value: Option<&OsStr>, user_home: Option<&OsStr>, root: &Path) -> Result<(), ProviderError> {
    let user_home = user_home.map(Path::new).filter(|h| h.is_absolute());
    let (home, named) = match value.filter(|v| !v.is_empty()) {
        // A relative one is refused on its own; it is not judged here.
        Some(v) if !Path::new(v).is_absolute() => return Ok(()),
        Some(v) => (PathBuf::from(v), None),
        None => match user_home {
            Some(h) => (h.join(".codex"), Some("~/.codex")),
            None => return Ok(()),
        },
    };
    let roots = path_forms(root);
    if let Some(user_home) = user_home {
        let homes = path_forms(user_home);
        if homes.iter().any(|h| roots.iter().any(|r| h.starts_with(r))) {
            return Ok(());
        }
    }
    let homes = path_forms(&home);
    let inside = |h: &PathBuf| roots.iter().any(|r| h.starts_with(r));
    if !homes.iter().any(inside) {
        return Ok(());
    }
    // Only its links lead it into the project: a CODEX_HOME naming the same place would be refused too.
    let by_link = !homes.first().is_some_and(inside);
    let shown_home = home.to_string_lossy();
    let shown_home = shown_home.escape_debug();
    let shown_root = root.to_string_lossy();
    let shown_root = shown_root.escape_debug();
    let what = match named {
        Some(default) => format!("CODEX_HOME is unset, so Codex reads {default} ({shown_home}), which"),
        None => format!("CODEX_HOME ({shown_home})"),
    };
    let (place, advice) = if by_link {
        (
            "links into the project",
            "set CODEX_HOME to a directory whose real path is outside the project",
        )
    } else {
        (
            "is inside the project",
            "set CODEX_HOME to a directory outside the project",
        )
    };
    Err(provider_error(
        ProviderErrorCode::InvalidConfiguration,
        format!(
            "{what} {place} {shown_root}: Codex would read files the project holds as your configuration; \
             {advice}"
        ),
    ))
}

/// A path as written with `.` and `..` taken out by name, and the same path with every link of its longest
/// existing ancestor resolved and the rest put back.
fn path_forms(path: &Path) -> Vec<PathBuf> {
    let mut lexical = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                lexical.pop();
            }
            other => lexical.push(other),
        }
    }
    let mut forms = vec![lexical];
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            let mut resolved = resolved;
            for part in rest.iter().rev() {
                resolved.push(part);
            }
            forms.push(resolved);
            break;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => break,
        }
    }
    forms
}

fn to_json<T: Serialize>(params: &T) -> Result<Value, ProviderError> {
    serde_json::to_value(params).map_err(|e| ProviderError::Transport(format!("a request could not be written: {e}")))
}

fn from_json<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Result<T, ProviderError> {
    serde_json::from_value(value).map_err(|e| {
        provider_error(
            ProviderErrorCode::ProviderProtocolError,
            format!("Codex's answer to {what} could not be read: {e}"),
        )
    })
}

impl CodexProvider {
    /// Starts `codex app-server` on stdio, runs the handshake, checks the version, and checks that Codex has
    /// an account when it needs one. Every failure is a sentence for the tab, and no child outlives it.
    ///
    /// A binary Eitri has not checked yet first goes through the per-binary self-check, which takes a few
    /// seconds once; while it has not passed, no Codex start is allowed.
    pub fn connect(launch: CodexLaunch) -> Result<CodexProvider, ProviderError> {
        // Before the self-check too: nothing is started under a home the project could supply.
        refuse_codex_home(&launch.cwd)?;
        let checked = match ensure_checked(&launch.program, &launch.state_home) {
            Ok(key) => key,
            Err(SelfCheckError::NotFound) => {
                return Err(provider_error(ProviderErrorCode::ProviderUnavailable, NOT_FOUND));
            }
            Err(e @ SelfCheckError::TooOld { .. }) => {
                return Err(provider_error(ProviderErrorCode::UnsupportedCliVersion, e.to_string()));
            }
            Err(e) => return Err(provider_error(ProviderErrorCode::ProviderUnavailable, e.to_string())),
        };
        let args = ["app-server", "--listen", "stdio://"]
            .iter()
            .map(OsString::from)
            .collect();
        Self::connect_child(launch, args, Vec::new(), Vec::new(), Some(&checked))
    }

    /// [`connect`](Self::connect) with the self-check's answer given and the child's arguments given: the
    /// tests start a scripted stand-in and say which version the check passed.
    #[doc(hidden)]
    pub fn connect_checked_with_args(
        launch: CodexLaunch,
        args: Vec<OsString>,
        checked: &BinaryKey,
    ) -> Result<CodexProvider, ProviderError> {
        Self::connect_child(launch, args, Vec::new(), Vec::new(), Some(checked))
    }

    /// [`connect`](Self::connect) with the child's arguments given: the tests start a scripted stand-in.
    #[doc(hidden)]
    pub fn connect_with_args(launch: CodexLaunch, args: Vec<OsString>) -> Result<CodexProvider, ProviderError> {
        Self::connect_with_env(launch, args, Vec::new(), Vec::new())
    }

    /// [`connect_with_args`](Self::connect_with_args) with variables set on and taken from the child: the
    /// tests that run a real Codex give it a private home and no multiplexer.
    #[doc(hidden)]
    pub fn connect_with_env(
        launch: CodexLaunch,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        env_remove: Vec<OsString>,
    ) -> Result<CodexProvider, ProviderError> {
        Self::connect_child(launch, args, env, env_remove, None)
    }

    /// Starts the child and runs the handshake. `checked` is the binary the self-check passed, whose version
    /// the child must report; only the tests' own doors start a child with none.
    fn connect_child(
        launch: CodexLaunch,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        env_remove: Vec<OsString>,
        checked: Option<&BinaryKey>,
    ) -> Result<CodexProvider, ProviderError> {
        let child_home = child_codex_home(&env, &env_remove);
        refuse_relative_home(child_home.as_deref())?;
        refuse_home_in_project(
            child_home.as_deref(),
            child_var("HOME", &env, &env_remove).as_deref(),
            &launch.cwd,
        )?;
        let link = JsonRpcLink::spawn(LinkSpec {
            program: launch.program.clone(),
            args,
            cwd: launch.cwd.clone(),
            framing: Framing::Bare,
            env,
            env_remove,
            over_cap: approvals::over_cap_reply,
        })
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => provider_error(ProviderErrorCode::ProviderUnavailable, NOT_FOUND),
            _ => ProviderError::Transport(format!("Codex could not be started: {e}")),
        })?;
        let (version, codex_home) = Self::handshake(&link, checked)?;
        let pid = link.pid();
        Ok(CodexProvider {
            link: Mutex::new(Some(link)),
            state: Mutex::new(State::default()),
            pid,
            version,
            state_home: launch.state_home,
            codex_home,
            steer_unsupported: AtomicBool::new(false),
        })
    }

    fn handshake(link: &JsonRpcLink, checked: Option<&BinaryKey>) -> Result<((u32, u32, u32), PathBuf), ProviderError> {
        let params = to_json(&InitializeParams::eitri(env!("CARGO_PKG_VERSION")))?;
        let answer = link
            .request(method::INITIALIZE, params)
            .wait(Instant::now() + START_TIMEOUT)
            .map_err(|e| link_error(link, "the handshake", e))?;
        let init: InitializeResponse = from_json(answer, "the handshake")?;
        let Some(version) = parse_user_agent_version(&init.user_agent) else {
            return Err(provider_error(
                ProviderErrorCode::UnsupportedCliVersion,
                "Codex did not report a version Eitri can read, so it cannot be checked",
            ));
        };
        // The binary is found again for the tab's own child; one replaced after the check passed (an
        // upgrade under way) would run unchecked.
        if checked.is_some_and(|key| !key.is_version(version)) {
            return Err(provider_error(
                ProviderErrorCode::ProviderUnavailable,
                "Codex changed while Eitri was checking it; start the tab again",
            ));
        }
        if version < MIN_CODEX {
            return Err(provider_error(
                ProviderErrorCode::UnsupportedCliVersion,
                format!(
                    "Codex {} is older than the oldest Eitri works with ({}): update Codex",
                    version_text(version),
                    version_text(MIN_CODEX)
                ),
            ));
        }
        link.notify(method::INITIALIZED, Value::Null);

        // The answer carries the account's email; it is read for one fact and never kept or logged.
        let answer = link
            .request(method::ACCOUNT_READ, to_json(&AccountReadParams::new())?)
            .wait(Instant::now() + START_TIMEOUT)
            .map_err(|e| match e {
                LinkError::Rpc { .. } => provider_error(
                    ProviderErrorCode::ProviderProtocolError,
                    "Codex could not tell whether it is signed in",
                ),
                other => link_error(link, "the account check", other),
            })?;
        let account: AccountReadResponse = from_json(answer, "the account check")?;
        if account.signed_out() {
            return Err(provider_error(ProviderErrorCode::InvalidConfiguration, SIGNED_OUT));
        }
        Ok((version, effective_codex_home(init.codex_home.as_deref())))
    }

    /// The child's pid, captured at spawn.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The version `initialize` reported.
    pub fn version(&self) -> (u32, u32, u32) {
        self.version
    }

    /// Eitri's state directory, as the launch gave it.
    pub fn state_home(&self) -> &Path {
        &self.state_home
    }

    /// Sends a request on the link, or says the link is gone.
    fn request(&self, method: &str, params: Value) -> Result<Pending, ProviderError> {
        match lock(&self.link).as_ref() {
            Some(link) => Ok(link.request(method, params)),
            None => Err(provider_error(
                ProviderErrorCode::ProviderUnavailable,
                "Codex is no longer running",
            )),
        }
    }

    /// Waits for an answer outside every lock but the link's own. The link is only needed to describe a
    /// closed connection, so a missing link is a plain closed error.
    fn wait(&self, pending: Pending, within: Duration, what: &str) -> Result<Value, ProviderError> {
        pending.wait(Instant::now() + within).map_err(|e| self.failure(e, what))
    }

    /// A failed call as the error the host shows.
    fn failure(&self, e: LinkError, what: &str) -> ProviderError {
        match lock(&self.link).as_ref() {
            Some(link) => link_error(link, what, e),
            None => ProviderError::Transport(format!("Codex ended before it answered {what}")),
        }
    }

    fn require_open(st: &State) -> Result<(), ProviderError> {
        if st.closed || st.dead {
            return Err(provider_error(
                ProviderErrorCode::ProviderUnavailable,
                "Codex is no longer running",
            ));
        }
        Ok(())
    }

    // ------------------------------------------------------------ interrupt

    /// Queues the one `turn/interrupt` each running turn of the family gets. Never waits: the answers are
    /// read by `follow_interrupts`, and Codex answers a second interrupt of an interrupted turn only when the
    /// turn ends. Interrupting the parent alone does not stop a child, so every family turn is named.
    fn send_interrupts(&self, st: &mut State) {
        let Some(family) = st.family.as_ref() else { return };
        let Some(turn) = st.turn.as_mut() else { return };
        if turn.id.is_none() {
            return;
        }
        for (thread_id, turn_id) in family.running_turns() {
            // A turn whose start could not be read has no id to name yet; it is sent its interrupt once a
            // message names it (`running_turn`).
            if turn_id == UNREAD_TURN || !turn.interrupted.insert(turn_id.clone()) {
                continue;
            }
            turn.told_to_stop.entry(turn_id.clone()).or_insert_with(Instant::now);
            if let Some(sent) = self.request_interrupt(thread_id, turn_id, 0) {
                st.interrupts.push(sent);
            }
        }
    }

    /// Sends one family turn its `turn/interrupt`, unless it had one. The tab's turn goes on for the rest of
    /// the family and ends with the outcome Codex reports for its own.
    fn interrupt_one(&self, st: &mut State, thread_id: &str, turn_id: &str) {
        if let Some(turn) = st.turn.as_mut() {
            if !turn.interrupted.insert(turn_id.to_string()) {
                return;
            }
            turn.told_to_stop
                .entry(turn_id.to_string())
                .or_insert_with(Instant::now);
        }
        if let Some(sent) = self.request_interrupt(thread_id.to_string(), turn_id.to_string(), 0) {
            st.interrupts.push(sent);
        }
    }

    fn request_interrupt(&self, thread_id: String, turn_id: String, resent: u32) -> Option<SentInterrupt> {
        let params = to_json(&TurnInterruptParams {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
        })
        .ok()?;
        let answer = lock(&self.link).as_ref()?.request(method::TURN_INTERRUPT, params);
        Some(SentInterrupt {
            thread_id,
            turn_id,
            answer: Some(answer),
            again_at: None,
            resent,
        })
    }

    /// Reads the answers to the interrupts sent. Codex refuses one that comes the moment `turn/start` was
    /// answered and runs the turn on, so a refused interrupt of a turn that still runs is sent again a
    /// moment later. An interrupt of a turn that is over is forgotten, its answer unread.
    fn follow_interrupts(&self, st: &mut State) {
        if st.interrupts.is_empty() || st.dead || st.closed {
            return;
        }
        let now = Instant::now();
        let running = st.family.as_ref().map(Family::running_turns).unwrap_or_default();
        let mut again = Vec::new();
        st.interrupts.retain_mut(|sent| {
            if !running
                .iter()
                .any(|(thread, turn)| *thread == sent.thread_id && *turn == sent.turn_id)
            {
                return false;
            }
            if sent.again_at.is_none() {
                match sent.answer.as_mut().and_then(Pending::try_take) {
                    None => return true,
                    Some(Ok(_)) => return false,
                    Some(Err(e)) if sent.resent < INTERRUPT_RESENDS => {
                        eprintln!("agent: Codex did not take an interrupt ({e}); it is sent again");
                        sent.again_at = Some(now + INTERRUPT_RETRY);
                        sent.answer = None;
                    }
                    Some(Err(e)) => {
                        eprintln!("agent: Codex did not take an interrupt ({e}); it is not sent again");
                        return false;
                    }
                }
            }
            if sent.again_at.is_some_and(|at| now >= at) {
                again.push((sent.thread_id.clone(), sent.turn_id.clone(), sent.resent + 1));
                return false;
            }
            true
        });
        for (thread_id, turn_id, resent) in again {
            if let Some(sent) = self.request_interrupt(thread_id, turn_id, resent) {
                st.interrupts.push(sent);
            }
        }
    }

    /// A turn of the family still running [`INTERRUPT_SETTLE`] after it was told to stop, whatever Codex
    /// answered, stops the session as a command left running does, so the tab is never left busy for good.
    /// The tab's turn then ends interrupted.
    fn stop_an_overdue_interrupt(&self, st: &mut State) {
        if st.dead || st.closed || st.stopping.is_some() {
            return;
        }
        let now = Instant::now();
        let overdue = match (st.turn.as_ref(), st.family.as_ref()) {
            (Some(turn), Some(family)) if turn.interrupting => family.running_turns().iter().any(|(_, id)| {
                turn.told_to_stop
                    .get(id)
                    .is_some_and(|at| now >= *at + INTERRUPT_SETTLE)
            }),
            _ => false,
        };
        if !overdue {
            return;
        }
        eprintln!("agent: Codex did not end a turn after its interrupt; its session is stopped");
        // The key stopped what was still running, whatever the tab's own turn reported before.
        if let Some(turn) = st.turn.as_mut() {
            turn.own_end = Some(translate::turn_ending(&TurnStatus::Interrupted, None));
        }
        st.interrupts.clear();
        self.stop_session(st, stopped_after_interrupt("did not end the turn"));
    }

    /// Interrupts every running turn of the family. A card that waits is answered first: a command or
    /// file-change card with `cancel`, which ends that thread's turn by itself, so that turn is not sent an
    /// interrupt; a tool card with a decline, after which its turn still needs one.
    fn interrupt_running(&self, st: &mut State) {
        let own_running = st
            .family
            .as_ref()
            .is_some_and(|f| f.running_on(f.tab_thread()).is_some());
        if let (Some(turn), false) = (st.turn.as_mut(), own_running) {
            turn.interrupt_asked = true;
        }
        if let Some(turn) = st.turn.as_mut() {
            if turn.interrupting {
                // Pressed again: a turn that a cancelled card was to end and that still runs gets its
                // interrupt now.
                for ended in std::mem::take(&mut turn.cancelled) {
                    turn.interrupted.remove(&ended);
                }
            }
            turn.interrupting = true;
            turn.settle_until
                .get_or_insert_with(|| Instant::now() + INTERRUPT_SETTLE);
        }
        for (permission_id, card) in std::mem::take(&mut st.cards) {
            if card.form.ends_the_turn(CardAnswer::Interrupt) {
                let ended = st
                    .family
                    .as_ref()
                    .and_then(|f| f.running_on(&card.thread_id))
                    .map(str::to_string);
                if let (Some(turn), Some(ended)) = (st.turn.as_mut(), ended) {
                    if turn.interrupted.insert(ended.clone()) {
                        // A stand-in for a turn whose start could not be read has no id of its own: a time
                        // kept for it would stand for whichever such turn runs next.
                        if ended != UNREAD_TURN {
                            turn.told_to_stop.entry(ended.clone()).or_insert_with(Instant::now);
                        }
                        turn.cancelled.insert(ended);
                    }
                }
            }
            self.respond(card.request_id.clone(), card.form.reply(CardAnswer::Interrupt));
            st.events.push_back(AgentDomainEvent::PermissionResolved {
                permission_id,
                outcome: PermissionOutcome::CancelledByInterrupt,
            });
            end_added_row(st, card, PermissionOutcome::CancelledByInterrupt);
        }
        self.send_interrupts(st);
        if forget_unnamed_turns(st) {
            self.close_family_turn(st);
        }
    }

    // ------------------------------------------------------------ messages into the running turn

    /// Reads the answers to `turn/steer`. An accepted message waits for its `userMessage` item; a refused
    /// one is dropped with Codex's reason.
    ///
    /// In order: a message waits unsent until every earlier one's answer has come, and a refusal drops the
    /// messages waiting behind it too, so they reach the model in the order they were typed or not at all.
    /// `send_waiting` false (a turn ending) reads answers and sends nothing.
    fn follow_steers(&self, st: &mut State, send_waiting: bool) {
        let mut at = 0;
        while at < st.steers.len() {
            if let Some(answer) = st.steers[at].answer.as_mut() {
                match answer.try_take() {
                    None => return,
                    Some(Ok(_)) => {
                        st.steers[at].answer = None;
                        at += 1;
                    }
                    Some(Err(e)) => {
                        if matches!(&e, LinkError::Rpc { code, .. } if *code == METHOD_NOT_FOUND) {
                            self.steer_unsupported.store(true, Ordering::Release);
                        }
                        eprintln!("agent: Codex did not take a message into the running turn ({e})");
                        st.events.extend(st.steers.refused(at, &steer_refusal(&e)));
                        return;
                    }
                }
            } else if st.steers[at].waiting.is_some() {
                if !send_waiting || st.steers.next_to_send() != Some(at) {
                    return;
                }
                let params = st.steers[at].waiting.take().expect("checked above");
                match self.request(method::TURN_STEER, params) {
                    Ok(answer) => st.steers[at].answer = Some(answer),
                    Err(_) => {
                        let lost: Vec<SentSteer> = st.steers.drain(at..).collect();
                        for later in lost {
                            st.events.push_back(AgentDomainEvent::UserMessageDropped {
                                client_message_id: later.client_id,
                                reason: "Codex ended before it read it".to_string(),
                            });
                        }
                    }
                }
                return;
            } else {
                at += 1;
            }
        }
    }

    /// A `userMessage` item on the tab's thread carrying the id of a message sent into the turn: Codex
    /// has read it into the turn.
    fn note_delivered(&self, st: &mut State, thread_id: &str, turn_id: &str, item: &ThreadItem) {
        let ThreadItem::UserMessage(message) = item else { return };
        let Some(client_id) = message.client_id.as_deref() else {
            return;
        };
        if !is_tab(st, thread_id) {
            return;
        }
        let Some(at) = st.steers.iter().position(|s| s.client_id == client_id) else {
            return;
        };
        let sent = st.steers.remove(at);
        st.events.push_back(AgentDomainEvent::UserMessageDelivered {
            client_message_id: sent.client_id,
            turn_id: turn_id.to_string(),
            text: sent.as_typed,
        });
    }

    /// Every message sent into the turn that Codex has not read is dropped with `reason`. An answer already
    /// read off the pipe goes first: a refusal names its own reason, and an unknown method is remembered.
    fn drop_steers(&self, st: &mut State, reason: &str) {
        self.follow_steers(st, false);
        for sent in std::mem::take(&mut st.steers) {
            st.events.push_back(AgentDomainEvent::UserMessageDropped {
                client_message_id: sent.client_id,
                reason: reason.to_string(),
            });
        }
    }

    fn respond(&self, id: Value, result: Value) {
        if let Some(link) = lock(&self.link).as_ref() {
            link.respond(id, result);
        }
    }

    /// A card's answer, sent or refused as a whole: `Err` when it was not queued, so the card can stay.
    fn respond_to_card(&self, id: Value, result: Value) -> Result<(), ProviderError> {
        let sent = match lock(&self.link).as_ref() {
            Some(link) => link.try_respond(id, result),
            None => Err(LinkError::Closed),
        };
        sent.map_err(|e| {
            provider_error(
                ProviderErrorCode::ProviderUnavailable,
                format!("the answer did not reach Codex: {e}; the card is still waiting"),
            )
        })
    }

    // ------------------------------------------------------------ requests

    fn on_request(&self, st: &mut State, id: Value, method: String, params: Value) {
        let request = wire::parse_server_request(&method, params);
        let handling = {
            let cards = &st.cards;
            let carded =
                |thread: &str, item: &str| cards.iter().any(|(_, c)| c.thread_id == thread && c.item_id == item);
            approvals::classify(request, &st.calls, &carded)
        };
        if let Handling::Card {
            thread_id,
            turn_id: Some(turn_id),
            ..
        } = &handling
        {
            self.adopt_turn(st, thread_id, turn_id);
        }
        // A request about a thread outside the tab's family is not shown; it is answered the way a deny
        // would be.
        let ours = handling.thread_id().is_none_or(|t| in_family(st, t));
        match handling {
            Handling::NotHandled => {
                note_unknown(st, "a request", &method);
                if let Some(link) = lock(&self.link).as_ref() {
                    link.respond_error(id, METHOD_NOT_FOUND, "method not found");
                }
            }
            // A request about a thread outside the family, after a tripwire, or from a turn that is not running
            // falls to the arm below, which declines it without drawing a row.
            Handling::Card {
                ref thread_id,
                ref turn_id,
                ref item_id,
                ref card,
                ref form,
            } if ours
                && st.blocked.is_none()
                && st.turn.as_ref().is_some_and(|t| t.interrupting)
                && Self::asks_from_a_running_turn(st, thread_id, turn_id.as_deref()) =>
            {
                let said = match form {
                    AnswerForm::Decision => "cancelled it",
                    AnswerForm::ToolForm => "declined it",
                    AnswerForm::ToolQuestion { .. } => "left it unanswered",
                };
                let note = format!("Codex asked while the turn was being interrupted; Eitri {said}");
                eprintln!("agent: {note} ({method})");
                st.calls.mark_asked(thread_id, item_id);
                self.respond(id, form.reply(CardAnswer::Interrupt));
                Self::note_answered(st, Some(thread_id), Some(item_id.clone()), card.tool_name(), note);
            }
            // Nor is a card for a turn that is not running: none at all, or an earlier one of that thread.
            Handling::Card {
                form,
                ref thread_id,
                ref turn_id,
                ..
            } if !ours
                || st.blocked.is_some()
                || !Self::asks_from_a_running_turn(st, thread_id, turn_id.as_deref()) =>
            {
                eprintln!("agent: Codex asked about a call Eitri does not take ({method}); declined");
                self.respond(id, form.reply(CardAnswer::Deny));
            }
            Handling::Card {
                thread_id,
                item_id,
                card,
                form,
                ..
            } => {
                st.calls.mark_asked(&thread_id, &item_id);
                let row = row_of(st, &thread_id, &item_id);
                let input = labelled(st, &thread_id, card.input());
                // A card whose call has no row gets one, or the card, the host's note on it and the
                // answer would sit on nothing the panel draws.
                let added_row = match Self::tab_turn_id(st) {
                    Some(turn_id) if !st.open_tools.contains(&row) && !st.answered_rows.contains(&row) => {
                        st.open_tools.insert(row.clone());
                        st.events.push_back(AgentDomainEvent::ToolCallStarted {
                            turn_id: turn_id.clone(),
                            tool_use_id: row.clone(),
                            name: card.tool_name().to_string(),
                            input: input.clone(),
                        });
                        Some(AddedRow {
                            row: row.clone(),
                            turn_id,
                            subject: card.subject(),
                        })
                    }
                    _ => None,
                };
                st.next_card += 1;
                let permission_id = format!("codex-{}", st.next_card);
                st.cards.push((
                    permission_id.clone(),
                    WaitingCard {
                        request_id: id,
                        form,
                        item_id: item_id.clone(),
                        thread_id,
                        added_row,
                    },
                ));
                st.events.push_back(AgentDomainEvent::PermissionRequested {
                    permission_id,
                    tool_use_id: Some(row),
                    tool_name: card.tool_name().to_string(),
                    input,
                    provider_prompt: None,
                });
            }
            Handling::Auto {
                thread_id,
                item_id,
                tool_name,
                answer,
                reply,
            } => {
                self.respond(id, reply);
                if !ours {
                    eprintln!("agent: Codex asked about a thread outside this tab's family ({method}); declined");
                    return;
                }
                let note = answer.note();
                eprintln!("agent: {note}");
                Self::note_answered(st, thread_id.as_deref(), item_id, tool_name, note);
            }
        }
    }

    /// Whether a card asked from `thread_id` (and its `turn_id`, when the request named one) belongs to a
    /// turn of the family that is running now.
    fn asks_from_a_running_turn(st: &State, thread_id: &str, turn_id: Option<&str>) -> bool {
        if Self::tab_turn_id(st).is_none() {
            return false;
        }
        let Some(family) = st.family.as_ref() else {
            return false;
        };
        match turn_id {
            Some(turn) => family.is_running(thread_id, turn),
            None => family.running_on(thread_id).is_some(),
        }
    }

    /// Says on a row why Eitri answered a request itself. The note goes on the call's own row when it has
    /// one; otherwise on a row Eitri adds for it, since a note with no row to sit on would never be seen
    /// and the user would not learn that something was answered for them. The note is the row's one
    /// statement of it: an added row's input names only the subagent and its result is empty. With no turn
    /// running there is nowhere to add a row, and the log line is all that is left.
    fn note_answered(
        st: &mut State,
        thread_id: Option<&str>,
        item_id: Option<String>,
        tool_name: &'static str,
        note: String,
    ) {
        let denied = |tool_use_id: Option<String>, note: String| AgentDomainEvent::PermissionDenied {
            tool_use_id,
            tool_name: tool_name.to_string(),
            reason_type: Some(ANSWERED_BY_EITRI.to_string()),
            reason: Some(note),
        };
        let on_its_row = match (thread_id, item_id) {
            (Some(thread), Some(item)) => Some(row_of(st, thread, &item)).filter(|row| st.open_tools.contains(row)),
            _ => None,
        };
        if let Some(row) = on_its_row {
            st.events.push_back(denied(Some(row), note));
            return;
        }
        let Some(turn_id) = Self::tab_turn_id(st) else {
            return;
        };
        st.next_answered_row += 1;
        // The turn's id keeps the row's id unique in a projection that outlives this connection.
        let row = format!("{ANSWERED_ROW_PREFIX}{turn_id}-{}", st.next_answered_row);
        let input = match thread_id {
            Some(thread) => labelled(st, thread, json!({})),
            None => json!({}),
        };
        st.events.push_back(AgentDomainEvent::ToolCallStarted {
            turn_id: turn_id.clone(),
            tool_use_id: row.clone(),
            name: tool_name.to_string(),
            input,
        });
        st.events.push_back(denied(Some(row.clone()), note));
        st.events.push_back(AgentDomainEvent::ToolCallCompleted {
            turn_id,
            tool_use_id: row,
            content: Value::String(String::new()),
            is_error: false,
        });
    }

    /// Codex's own reviewer decided an escalation (Auto). The verdict is a note on the call it reviewed. A
    /// network review names no call, since the command that wanted the network is not what was decided, so
    /// it gets a row of its own naming the host. A review stopped by an interrupt decided nothing.
    fn on_auto_review(&self, st: &mut State, review: wire::AutoReviewNotification) {
        if !self.running_turn(st, &review.thread_id, &review.turn_id) {
            return;
        }
        let Some(turn_id) = Self::tab_turn_id(st) else { return };
        let rationale = review
            .review
            .rationale
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(|r| r.chars().take(REVIEW_RATIONALE_CHARS).collect::<String>());
        let (reason_type, reason, allowed) = match review.review.status {
            wire::ReviewStatus::Approved => (REVIEW_APPROVED, rationale, true),
            wire::ReviewStatus::Denied => (REVIEW_DENIED, rationale, false),
            wire::ReviewStatus::TimedOut => (
                REVIEW_DENIED,
                Some(rationale.unwrap_or_else(|| "the reviewer did not decide in time".to_string())),
                false,
            ),
            wire::ReviewStatus::InProgress | wire::ReviewStatus::Aborted | wire::ReviewStatus::Unknown(_) => return,
        };
        let note = |row: String| AgentDomainEvent::PermissionDenied {
            tool_use_id: Some(row),
            tool_name: "autoApprovalReview".to_string(),
            reason_type: Some(reason_type.to_string()),
            reason: reason.clone(),
        };
        if let Some(item) = review.target_item_id.as_deref() {
            let row = row_of(st, &review.thread_id, item);
            st.events.push_back(note(row));
            return;
        }
        let Some((host, protocol)) = review.network() else {
            return;
        };
        st.next_answered_row += 1;
        let row = format!("{REVIEW_ROW_PREFIX}{turn_id}-{}", st.next_answered_row);
        let input = labelled(
            st,
            &review.thread_id,
            json!({ "command": null, "cwd": null, "network": { "host": host, "protocol": protocol } }),
        );
        let said = if allowed {
            "allowed by auto review"
        } else {
            "blocked by auto review"
        };
        st.events.push_back(AgentDomainEvent::ToolCallStarted {
            turn_id: turn_id.clone(),
            tool_use_id: row.clone(),
            name: "commandExecution".to_string(),
            input,
        });
        st.events.push_back(note(row.clone()));
        st.events.push_back(AgentDomainEvent::ToolCallCompleted {
            turn_id,
            tool_use_id: row,
            content: Value::String(format!("network access to {host} ({protocol}): {said}")),
            is_error: !allowed,
        });
    }

    /// Codex says a request is settled. A card still waiting for it was settled without Eitri's answer
    /// (an interrupt resolves what waits), so it goes.
    fn on_request_resolved(&self, st: &mut State, resolved: wire::ServerRequestResolved) {
        if !in_family(st, &resolved.thread_id) {
            return;
        }
        let Some(at) = st.cards.iter().position(|(_, c)| c.request_id == resolved.request_id) else {
            return;
        };
        let (permission_id, card) = st.cards.remove(at);
        st.events.push_back(AgentDomainEvent::PermissionResolved {
            permission_id,
            outcome: PermissionOutcome::CancelledByInterrupt,
        });
        end_added_row(st, card, PermissionOutcome::CancelledByInterrupt);
    }

    /// Every card still waiting goes, with `outcome`: what it asked about can no longer happen. Codex is
    /// told no for each while it still runs: a request left unanswered would hold its thread's turn open
    /// for good.
    fn drop_cards(&self, st: &mut State, outcome: PermissionOutcome) {
        self.drop_cards_with(st, outcome, CardAnswer::Deny);
    }

    /// [`drop_cards`](Self::drop_cards), telling Codex `answer` for each card.
    fn drop_cards_with(&self, st: &mut State, outcome: PermissionOutcome, answer: CardAnswer) {
        for (permission_id, card) in std::mem::take(&mut st.cards) {
            if !st.dead {
                self.respond(card.request_id.clone(), card.form.reply(answer));
            }
            st.events
                .push_back(AgentDomainEvent::PermissionResolved { permission_id, outcome });
            end_added_row(st, card, outcome);
        }
    }

    // ------------------------------------------------------------ the stream

    /// Reads what the link has, folding it into events. Holds the incoming messages back while a
    /// `turn/start` answer is awaited.
    fn drain(&self, st: &mut State) -> bool {
        let mut handled = 0;
        while handled < PUMP_BATCH {
            let awaiting_turn_id = st.turn.as_ref().is_some_and(|t| t.id.is_none());
            if awaiting_turn_id || st.history_loading {
                let mut link = lock(&self.link);
                if let Some(link) = link.as_mut() {
                    // Never past the cap: what does not fit stays in the link's queue. A history read stops
                    // once the cap is reached (`read_history`), and a turn start's wait ends on its timeout.
                    while st.held.len() < PUMP_BATCH {
                        let Some(message) = link.try_recv() else { break };
                        st.held.push_back(message);
                    }
                }
                return true;
            }
            let next = match st.held.pop_front() {
                Some(message) => Some(message),
                None => external_link::try_recv(&self.link),
            };
            let Some(message) = next else { return true };
            handled += 1;
            self.fold(st, message);
        }
        false
    }

    fn fold(&self, st: &mut State, message: Incoming) {
        match message {
            Incoming::Notification { method, params } => {
                self.on_notification(st, wire::parse_notification(&method, params))
            }
            Incoming::Request { id, method, params } => self.on_request(st, id, method, params),
            Incoming::Skipped(skipped) => self.on_skipped(st, skipped),
            Incoming::Closed { reason } => self.on_closed(st, &reason),
        }
    }

    fn on_closed(&self, st: &mut State, reason: &str) {
        if st.closed || st.dead {
            return;
        }
        st.dead = true;
        end_background_rows(st, "Codex ended while it ran");
        // Nothing can answer what waited any more.
        self.drop_cards(st, PermissionOutcome::ProviderFailed);
        self.drop_steers(st, "Codex ended before it read it");
        // The end of the output stream can be read a moment before the last of stderr; the notice waits a
        // short while for it so it says why the child went. `pump` finishes it: waiting here would hold this
        // state, which the UI thread's calls take.
        st.closing_notice = Some(ClosingNotice::after(reason, Instant::now(), &self.link));
        self.finish_closing_notice(st);
    }

    /// Sends the child's end once its stderr says why, or once the wait is over without it.
    fn finish_closing_notice(&self, st: &mut State) {
        if let Some(reason) = external_link::finish_closing_notice(&mut st.closing_notice, "Codex", &self.link) {
            st.events.push_back(AgentDomainEvent::SessionUnavailable { reason });
        }
    }

    fn on_skipped(&self, st: &mut State, skipped: Skipped) {
        match skipped {
            Skipped::TurnTooLarge {
                method: name,
                thread_id,
            } => {
                eprintln!("agent: a {name} message from Codex was larger than 16 MiB");
                if name == method::TURN_STARTED {
                    self.unreadable_turn_start(st, thread_id.as_deref());
                } else {
                    self.unreadable_turn_end(
                        st,
                        thread_id.as_deref(),
                        "a message from Codex was too large",
                        "message_too_large",
                    );
                }
            }
            Skipped::ItemTooLarge {
                method: name,
                item_id,
                thread_id,
            } => {
                eprintln!("agent: a {name} message from Codex was larger than 16 MiB; the item is not shown");
                if name == method::ITEM_STARTED {
                    match (&thread_id, &item_id) {
                        (Some(thread), Some(id)) => note_running(st, thread, id, Running::Unread),
                        _ => st.running_unread = true,
                    }
                    match &thread_id {
                        Some(thread) => st.calls.mark_missed(thread),
                        None => st.calls.mark_missed_anywhere(),
                    }
                } else if name == method::ITEM_COMPLETED {
                    if let (Some(thread), Some(id)) = (&thread_id, &item_id) {
                        let key = (thread.clone(), id.clone());
                        st.running.remove(&key);
                        // The call is over even though its end could not be read; its row says so rather
                        // than reading as running until the turn ends.
                        let row = row_of(st, thread, id);
                        let ended = st.background_rows.remove(&key).or_else(|| {
                            Self::tab_turn_id(st)
                                .filter(|_| st.open_tools.remove(&row))
                                .map(|turn_id| (row, turn_id))
                        });
                        if let Some((row, turn_id)) = ended {
                            st.events.push_back(AgentDomainEvent::ToolCallCompleted {
                                turn_id,
                                tool_use_id: row,
                                content: Value::String(
                                    "ended; its result was larger than 16 MiB and is not shown".to_string(),
                                ),
                                is_error: false,
                            });
                        }
                    }
                }
                // What is held may no longer be what Codex will do; a request naming it is declined.
                match (item_id, thread_id) {
                    (Some(id), Some(thread)) => st.calls.forget(&thread, &id),
                    (Some(id), None) => st.calls.forget_everywhere(&id),
                    (None, _) => {}
                }
            }
            Skipped::RequestAnswered {
                method,
                thread_id,
                item_id,
                ..
            } => {
                eprintln!("agent: a {method} request from Codex was larger than 16 MiB and was answered at once");
                let Some((tool_name, note)) = approvals::over_cap_note(&method) else {
                    return;
                };
                // As with any request: one about a thread outside the tab's family is not shown.
                if thread_id.as_deref().is_some_and(|thread| !in_family(st, thread)) {
                    return;
                }
                Self::note_answered(st, thread_id.as_deref(), item_id, tool_name, note);
            }
            Skipped::ResponseFailed { id } => {
                eprintln!("agent: Codex's answer to request {id} was larger than 16 MiB");
            }
        }
    }

    fn on_notification(&self, st: &mut State, notification: ServerNotification) {
        use ServerNotification as N;
        match notification {
            N::ItemStarted(n) => self.on_item_started(st, n),
            N::ItemCompleted(n) => self.on_item_completed(st, n),
            N::AgentMessageDelta(d) => self.on_delta(st, d, ContentKind::Text, None),
            N::ReasoningSummaryDelta(d) => {
                let part = ThoughtPart::Summary(d.summary_index);
                self.on_delta(st, d, ContentKind::Thinking, Some(part))
            }
            N::ReasoningTextDelta(d) => {
                let part = ThoughtPart::Content(d.content_index);
                self.on_delta(st, d, ContentKind::Thinking, Some(part))
            }
            N::TurnCompleted(n) => self.on_turn_completed(st, n),
            N::TokenUsageUpdated(u) => {
                // The tab's own figure; a subagent's tokens are its own thread's.
                if is_tab(st, &u.thread_id) {
                    if let Some(turn) = st.turn.as_mut() {
                        turn.usage = Some(u.token_usage);
                    }
                }
            }
            N::RateLimitsUpdated(limits) => {
                if let Some(figure) = translate::account_limits(&limits.rate_limits) {
                    if st.last_limits.as_ref() != Some(&figure) {
                        st.events.push_back(AgentDomainEvent::AccountLimits {
                            used_percent: figure.used_percent,
                            window_minutes: figure.window_minutes,
                            resets_at: figure.resets_at,
                            plan: figure.plan.clone(),
                        });
                        st.last_limits = Some(figure);
                    }
                }
            }
            N::TurnStarted(n) => self.on_turn_started(st, n),
            N::ThreadStarted(_) => {}
            // Output is read from the finished item, which carries all of it.
            N::CommandOutputDelta(_) => {}
            // A review under way has nothing to say yet; its verdict is what the row needs.
            N::AutoReviewStarted(review) => {
                self.reviewer_not_honoured(st, &review, method::AUTO_REVIEW_STARTED);
            }
            N::AutoReviewCompleted(review) => {
                if !self.reviewer_not_honoured(st, &review, method::AUTO_REVIEW_COMPLETED) {
                    self.on_auto_review(st, review)
                }
            }
            N::McpStartupStatus(status) => {
                if let Err(mismatch) = st.watch.mcp_started(&status.name) {
                    self.trip(st, mismatch);
                }
            }
            N::HookStarted(hook) => {
                if let Err(mismatch) = st
                    .watch
                    .hook_started(hook.run.source.as_deref(), hook.run.source_path.as_deref())
                {
                    self.trip(st, mismatch);
                }
            }
            N::HookCompleted(_) => {}
            N::ServerRequestResolved(resolved) => self.on_request_resolved(st, resolved),
            N::Error(_) => {}
            N::Unknown { method, .. } => note_unknown(st, "a notification", &method),
            N::Malformed {
                method: name,
                error,
                thread_id,
            } => {
                eprintln!("agent: a {name} notification from Codex could not be read: {error}");
                // What cannot be read cannot be shown to be the user's own.
                let unplaced = match name.as_str() {
                    method::HOOK_STARTED => st.watch.unreadable_hook().err(),
                    method::MCP_STARTUP_STATUS => st.watch.unreadable_mcp_status().err(),
                    _ => None,
                };
                if let Some(mismatch) = unplaced {
                    self.trip(st, mismatch);
                } else if name == method::TURN_COMPLETED {
                    // A turn whose end cannot be read would otherwise run forever.
                    self.unreadable_turn_end(
                        st,
                        thread_id.as_deref(),
                        "Codex sent the end of the turn in a form Eitri could not read",
                        "unreadable_turn_end",
                    );
                } else if name == method::TURN_STARTED {
                    self.unreadable_turn_start(st, thread_id.as_deref());
                } else if name == method::ITEM_STARTED {
                    // A call whose start cannot be read may be a command that is running now, or the tool
                    // call a later approval is for.
                    st.running_unread = true;
                    match thread_id.as_deref() {
                        Some(thread) => st.calls.mark_missed(thread),
                        None => st.calls.mark_missed_anywhere(),
                    }
                }
            }
        }
    }

    /// Whether a message names a turn of the family that is running. Anything else is about something this
    /// tab does not follow, and a thread outside the family is logged once.
    fn running_turn(&self, st: &mut State, thread_id: &str, turn_id: &str) -> bool {
        if !in_family(st, thread_id) {
            note_foreign(st, thread_id);
            return false;
        }
        if !st.turn.as_ref().is_some_and(|t| t.id.is_some()) {
            return false;
        }
        self.adopt_turn(st, thread_id, turn_id);
        st.family.as_ref().is_some_and(|f| f.is_running(thread_id, turn_id))
    }

    /// A message named `turn_id` on `thread_id`: when that thread's running turn is one whose start could
    /// not be read, this is it. An interrupt the user asked for meanwhile goes out to it now.
    fn adopt_turn(&self, st: &mut State, thread_id: &str, turn_id: &str) {
        let adopted = st.family.as_mut().is_some_and(|f| f.adopt(thread_id, turn_id));
        // A stand-in is followed only by the tab's turn that is running now, so the turn it turns out to be
        // began inside that one.
        if adopted {
            Self::inherit_reviewer(st, thread_id, turn_id);
        }
        if adopted && st.turn.as_ref().is_some_and(|t| t.interrupting) {
            self.send_interrupts(st);
        }
    }

    /// A turn of the family started in a message Eitri could not read. A thread already running a turn
    /// (the tab's, begun by the `turn/start` answer) loses nothing. Any other member's turn is followed
    /// under a stand-in until a readable message names it, so the tab's turn still lasts as long as it
    /// runs; when it opens the tab's turn, that turn gets an id of Eitri's own.
    fn unreadable_turn_start(&self, st: &mut State, thread_id: Option<&str>) {
        let Some(thread_id) = thread_id else {
            eprintln!("agent: a turn started that Codex did not say the thread of; it is not followed");
            return;
        };
        if !in_family(st, thread_id) {
            // A child may start its turn before the message that makes it a member is read; it is kept
            // under the stand-in like a readable early start, so the thread is followed once it joins.
            note_foreign(st, thread_id);
            self.remember_early_turn(st, thread_id, UNREAD_TURN.to_string());
            return;
        }
        let edge = match st.family.as_mut() {
            Some(family) if family.running_on(thread_id).is_none() => family.turn_started(thread_id, UNREAD_TURN),
            _ => return,
        };
        if edge == FamilyEdge::Opened && st.turn.is_none() {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let turn_id = format!("{UNREAD_TURN_PREFIX}{thread_id}-{stamp}");
            st.turn = Some(ActiveTurn::new(Some(turn_id.clone())));
            st.events.push_back(AgentDomainEvent::TurnStarted { turn_id });
        }
    }

    /// A turn of the family ended in a message Eitri could not read, so its end is a failure. Only that
    /// thread's turn ends: the tab's turn still waits for every other turn of the family. When the message
    /// did not say whose it was, it is taken for the tab's own turn if that runs, else for the one family
    /// turn running; with several running and none of them the tab's, nothing is guessed.
    fn unreadable_turn_end(&self, st: &mut State, thread_id: Option<&str>, text: &str, reason: &str) {
        if let Some(thread) = thread_id.filter(|thread| !in_family(st, thread)) {
            // Not a member (yet): whatever turn was kept for it is over, so the message that makes it one
            // must not bring that turn back, or the tab would wait for an end that already came.
            st.early_turns.remove(thread);
            note_foreign(st, thread);
            return;
        }
        if !st.turn.as_ref().is_some_and(|t| t.id.is_some()) {
            return;
        }
        let Some(family) = st.family.as_ref() else { return };
        let thread = match thread_id {
            Some(thread) => thread.to_string(),
            None if family.running_on(family.tab_thread()).is_some() => family.tab_thread().to_string(),
            None => match family.running_turns().as_slice() {
                [(thread, _)] => thread.clone(),
                _ => {
                    eprintln!("agent: a turn ended that Codex did not say the thread of; the tab waits on");
                    return;
                }
            },
        };
        let Some(turn) = family.running_on(&thread).map(str::to_string) else {
            return;
        };
        let detail = TurnEndDetail::from_result(TurnOutcome::Failed, true, text, Some(reason.to_string()), None, &[]);
        self.family_turn_ended(st, &thread, &turn, (TurnOutcome::Failed, detail));
    }

    /// The id of the tab's running turn, which every event of a family turn is filed under.
    /// A subagent's turn that begins inside a tab's turn started with reviewer `user` runs under it too.
    fn inherit_reviewer(st: &mut State, thread: &str, turn: &str) {
        let inside_user_turn = st
            .turn
            .as_ref()
            .is_some_and(|t| t.id.is_some() && t.reviewer == Some(Reviewer::User));
        if inside_user_turn && !is_tab(st, thread) && turn != UNREAD_TURN {
            st.user_reviewed.insert((thread.to_string(), turn.to_string()));
        }
    }

    /// Codex's own reviewer took up a request in a turn that runs under reviewer `user`: the reviewer the
    /// turn's start named was not honoured, so what the tab's mode says about who decides is not true. The
    /// session is failed like any other policy difference. A review in a turn that began under Codex's
    /// reviewer (an Auto turn still winding down) is that turn's business and passes.
    fn reviewer_not_honoured(&self, st: &mut State, review: &wire::AutoReviewNotification, method: &str) -> bool {
        if !st
            .user_reviewed
            .contains(&(review.thread_id.clone(), review.turn_id.clone()))
        {
            return false;
        }
        self.trip(
            st,
            PolicyMismatch {
                requested: "approvalsReviewer \"user\" for this turn".to_string(),
                reported: format!("Codex's own reviewer took up a request in it ({method})"),
            },
        );
        true
    }

    fn tab_turn_id(st: &State) -> Option<String> {
        st.turn.as_ref().and_then(|t| t.id.clone())
    }

    /// A turn started on a thread of the family. The tab's own turn is begun by the `turn/start` answer and
    /// this repeats it; a subagent's turn joins the tab's turn, or opens one when the tab is idle (a child
    /// keeps working after its parent's turn ended).
    fn on_turn_started(&self, st: &mut State, n: TurnNotification) {
        if !in_family(st, &n.thread_id) {
            note_foreign(st, &n.thread_id);
            self.remember_early_turn(st, &n.thread_id, n.turn.id);
            return;
        }
        self.join_family_turn(st, &n.thread_id, n.turn.id);
    }

    /// `turn` of member `thread` is running: it joins the tab's turn, or opens one when the tab is idle. An
    /// interrupt the user asked for that this turn missed goes out to it now.
    fn join_family_turn(&self, st: &mut State, thread: &str, turn: String) {
        Self::inherit_reviewer(st, thread, &turn);
        let edge = match st.family.as_mut() {
            Some(family) => family.turn_started(thread, &turn),
            None => return,
        };
        // The tab's own turn after a `turn/start` that timed out is the one the user was told did not start,
        // whether it opens the tab's turn or joins one a subagent opened first.
        let abandoned = is_tab(st, thread) && std::mem::take(&mut st.start_abandoned);
        let started = turn.clone();
        if edge == FamilyEdge::Opened && st.turn.is_none() {
            st.turn = Some(ActiveTurn::new(Some(turn.clone())));
            st.events.push_back(AgentDomainEvent::TurnStarted { turn_id: turn });
        } else if st.turn.as_ref().is_some_and(|t| t.interrupting) {
            // A turn begun while the tab's turn waited for its calls: the family is running again.
            if let Some(active) = st.turn.as_mut() {
                active.settling = false;
            }
            self.send_interrupts(st);
        }
        // Only that turn is stopped: a subagent's turn that runs beside it was not part of the send.
        if abandoned {
            eprintln!("agent: Codex started a turn whose start had timed out; it is interrupted");
            self.interrupt_one(st, thread, &started);
        }
    }

    /// A thread that just joined the family may already be running a turn; it is followed from now on. What
    /// it started before is not held, so a tool approval on it must name its tool to be matched.
    fn joined(&self, st: &mut State, thread: &str) {
        if st.early_turns.contains_key(thread) {
            st.calls.mark_missed(thread);
        }
        match st.early_turns.remove(thread) {
            // Its start could not be read: followed under the stand-in until a message names the turn.
            Some(turn) if turn == UNREAD_TURN => self.unreadable_turn_start(st, Some(thread)),
            Some(turn) => self.join_family_turn(st, thread, turn),
            None => {}
        }
    }

    /// Takes what an item says about the family into account. A `subAgentActivity` announces a subagent
    /// and is nothing else, so the caller drops it (returns true). A `collabAgentToolCall` on a member
    /// thread makes its receivers members, which is how a spawn is announced under a model with the plain
    /// tool set; it is still the call's row (returns false).
    fn announced(&self, st: &mut State, thread_id: &str, item: &ThreadItem) -> bool {
        match item {
            // A child announced without a thread id could never be followed, like one that does not read.
            ThreadItem::SubAgentActivity(activity)
                if activity.kind == SubAgentKind::Started
                    && activity.agent_thread_id.is_empty()
                    && in_family(st, thread_id) =>
            {
                let mismatch = PolicyMismatch {
                    requested: "every subagent announced with its thread".to_string(),
                    reported: "Codex announced a subagent with no thread id".to_string(),
                };
                self.trip(st, mismatch);
                true
            }
            ThreadItem::SubAgentActivity(activity) => {
                match st.family.as_mut().map(|f| f.announce(thread_id, activity)) {
                    Some(Ok(true)) => self.joined(st, &activity.agent_thread_id),
                    Some(Err(Full)) => self.trip(st, family_full()),
                    _ if activity.kind == SubAgentKind::Started => note_foreign(st, thread_id),
                    _ => {}
                }
                true
            }
            ThreadItem::CollabAgentToolCall(call) => {
                let joined = match st
                    .family
                    .as_mut()
                    .map(|f| f.announce_receivers(thread_id, &call.receiver_thread_ids))
                {
                    Some(Ok(joined)) => joined,
                    Some(Err(Full)) => {
                        self.trip(st, family_full());
                        Vec::new()
                    }
                    None => Vec::new(),
                };
                for thread in joined {
                    self.joined(st, &thread);
                }
                false
            }
            // An announcement that does not read may name a child, which would then never be followed:
            // its turn could write files while the tab reads as idle. The session is failed instead.
            ThreadItem::Malformed { item_type, .. }
                if (item_type == "subAgentActivity" || item_type == "collabAgentToolCall")
                    && in_family(st, thread_id) =>
            {
                let mismatch = PolicyMismatch {
                    requested: "every subagent announced in a form Eitri can read".to_string(),
                    reported: format!("Codex sent a {item_type} item Eitri could not read"),
                };
                self.trip(st, mismatch);
                item_type == "subAgentActivity"
            }
            _ => false,
        }
    }

    fn on_item_started(&self, st: &mut State, n: ItemNotification) {
        self.note_delivered(st, &n.thread_id, &n.turn_id, &n.item);
        // Before anything decides whether the item is shown: what runs is counted whoever runs it.
        if let Some(kind) = Running::of(&n.item) {
            match n.item.id() {
                Some(id) => note_running(st, &n.thread_id, id, kind),
                None if kind == Running::Other => {}
                None => st.running_unread = true,
            }
        }
        if self.announced(st, &n.thread_id, &n.item) {
            return;
        }
        if !self.running_turn(st, &n.thread_id, &n.turn_id) {
            return;
        }
        let Some(turn_id) = Self::tab_turn_id(st) else { return };
        let from_tab = is_tab(st, &n.thread_id);
        st.calls.hold(&n.thread_id, &n.item);
        match &n.item {
            // A subagent's words are its own; only what it ran is shown in the tab's turn.
            ThreadItem::AgentMessage(_) | ThreadItem::Reasoning(_) if !from_tab => {}
            ThreadItem::AgentMessage(m) => {
                st.events.push_back(AgentDomainEvent::AssistantMessageBoundary {
                    turn_id: turn_id.clone(),
                });
                let mut emitted = 0;
                if !m.text.is_empty() {
                    emitted = m.text.len();
                    st.events.push_back(AgentDomainEvent::ContentDelta {
                        turn_id,
                        kind: ContentKind::Text,
                        text: m.text.clone(),
                    });
                }
                st.items.insert(m.id.clone(), emitted);
                st.message_text.insert(m.id.clone(), m.text.clone());
            }
            ThreadItem::Reasoning(r) => {
                st.items.insert(r.id.clone(), 0);
            }
            other => {
                if let (Some((name, input)), Some(id)) = (translate::tool_start(other), other.id()) {
                    let input = labelled(st, &n.thread_id, input);
                    let row = row_of(st, &n.thread_id, id);
                    if !st.answered_rows.contains(&row) && st.open_tools.insert(row.clone()) {
                        st.events.push_back(AgentDomainEvent::ToolCallStarted {
                            turn_id,
                            tool_use_id: row,
                            name: name.to_string(),
                            input,
                        });
                    }
                }
            }
        }
    }

    fn on_item_completed(&self, st: &mut State, n: ItemNotification) {
        self.note_delivered(st, &n.thread_id, &n.turn_id, &n.item);
        if let Some(id) = n.item.id() {
            let key = (n.thread_id.clone(), id.to_string());
            st.running.remove(&key);
            if let Some((row, turn_id)) = st.background_rows.remove(&key) {
                st.calls.forget(&n.thread_id, id);
                if let Some((content, is_error)) = translate::tool_result(&n.item) {
                    st.events.push_back(AgentDomainEvent::ToolCallCompleted {
                        turn_id,
                        tool_use_id: row,
                        content,
                        is_error,
                    });
                }
                return;
            }
        }
        if self.announced(st, &n.thread_id, &n.item) {
            return;
        }
        if st.turn.as_ref().is_some_and(|t| t.settling) {
            // A call the interrupted turn left running has ended; its row gets its result.
            self.end_settled_row(st, &n);
            return;
        }
        if !self.running_turn(st, &n.thread_id, &n.turn_id) {
            return;
        }
        let Some(turn_id) = Self::tab_turn_id(st) else { return };
        let from_tab = is_tab(st, &n.thread_id);
        if let Some(id) = n.item.id() {
            st.calls.forget(&n.thread_id, id);
        }
        match &n.item {
            ThreadItem::AgentMessage(_) | ThreadItem::Reasoning(_) if !from_tab => {}
            ThreadItem::AgentMessage(m) => {
                let emitted = st.items.remove(&m.id);
                let shown = st.message_text.remove(&m.id).unwrap_or_default();
                if emitted.is_none() {
                    st.events.push_back(AgentDomainEvent::AssistantMessageBoundary {
                        turn_id: turn_id.clone(),
                    });
                }
                if m.text.starts_with(shown.as_str()) {
                    // What streamed is a prefix of the final text; only the rest is new.
                    if m.text.len() > shown.len() {
                        st.events.push_back(AgentDomainEvent::ContentDelta {
                            turn_id,
                            kind: ContentKind::Text,
                            text: m.text[shown.len()..].to_string(),
                        });
                    }
                } else if !m.text.is_empty() {
                    // The final text is not what streamed with more after it. What was shown stays as Codex
                    // sent it, and the final text follows whole as a message of its own, rather than the end
                    // of one version spliced onto the start of the other.
                    st.events.push_back(AgentDomainEvent::AssistantMessageBoundary {
                        turn_id: turn_id.clone(),
                    });
                    st.events.push_back(AgentDomainEvent::ContentDelta {
                        turn_id,
                        kind: ContentKind::Text,
                        text: m.text.clone(),
                    });
                }
                if !m.text.is_empty() {
                    if let Some(turn) = st.turn.as_mut() {
                        turn.last_message = m.text.clone();
                    }
                }
            }
            ThreadItem::Reasoning(r) => {
                let streamed = st.items.remove(&r.id).unwrap_or(0);
                if streamed == 0 {
                    let parts = if r.summary.is_empty() { &r.content } else { &r.summary };
                    let text = parts.join("\n\n");
                    if !text.is_empty() {
                        let text = thought_text(st, r.id.clone(), ThoughtPart::Whole, text);
                        st.events.push_back(AgentDomainEvent::ContentDelta {
                            turn_id,
                            kind: ContentKind::Thinking,
                            text,
                        });
                    }
                }
            }
            other => {
                let (Some((name, input)), Some((content, is_error)), Some(id)) =
                    (translate::tool_start(other), translate::tool_result(other), other.id())
                else {
                    return;
                };
                let row = row_of(st, &n.thread_id, id);
                if st.open_tools.remove(&row) || st.answered_rows.remove(&row) {
                    // Started earlier: only the result is new.
                } else {
                    let input = labelled(st, &n.thread_id, input);
                    st.events.push_back(AgentDomainEvent::ToolCallStarted {
                        turn_id: turn_id.clone(),
                        tool_use_id: row.clone(),
                        name: name.to_string(),
                        input,
                    });
                }
                st.events.push_back(AgentDomainEvent::ToolCallCompleted {
                    turn_id,
                    tool_use_id: row,
                    content,
                    is_error,
                });
            }
        }
    }

    /// `part` is which part of a reasoning item a thinking delta writes; `None` for a message's text.
    fn on_delta(&self, st: &mut State, delta: ItemDelta, kind: ContentKind, part: Option<ThoughtPart>) {
        if delta.delta.is_empty() || !self.running_turn(st, &delta.thread_id, &delta.turn_id) {
            return;
        }
        let Some(turn_id) = Self::tab_turn_id(st) else { return };
        if !is_tab(st, &delta.thread_id) {
            return;
        }
        *st.items.entry(delta.item_id.clone()).or_insert(0) += delta.delta.len();
        if kind == ContentKind::Text {
            st.message_text
                .entry(delta.item_id.clone())
                .or_default()
                .push_str(&delta.delta);
        }
        let text = match part {
            Some(part) => thought_text(st, delta.item_id, part, delta.delta),
            None => delta.delta,
        };
        st.events
            .push_back(AgentDomainEvent::ContentDelta { turn_id, kind, text });
    }

    /// A turn of the family ended. The tab's turn ends with the last of them, with the outcome of the tab's
    /// own turn: a child's end is not the tab's failure.
    fn on_turn_completed(&self, st: &mut State, n: TurnNotification) {
        // An early turn whose start could not be read is the one the next end on its thread names.
        if st
            .early_turns
            .get(&n.thread_id)
            .is_some_and(|early| *early == n.turn.id || early == UNREAD_TURN)
        {
            st.early_turns.remove(&n.thread_id);
        }
        if !self.running_turn(st, &n.thread_id, &n.turn.id) {
            return;
        }
        let ending = translate::turn_ending(&n.turn.status, n.turn.error.as_ref());
        self.family_turn_ended(st, &n.thread_id, &n.turn.id, ending);
    }

    /// `turn` of `thread` ended with `ending`. The tab's turn ends with the last of the family's, with the
    /// outcome of the tab's own turn.
    fn family_turn_ended(&self, st: &mut State, thread: &str, turn: &str, ending: (TurnOutcome, TurnEndDetail)) {
        if is_tab(st, thread) {
            if let Some(active) = st.turn.as_mut() {
                active.own_end = Some(ending);
            }
        }
        let edge = match st.family.as_mut() {
            Some(family) => family.turn_completed(thread, turn),
            None => return,
        };
        let closed = match edge {
            FamilyEdge::Closed => true,
            FamilyEdge::Still => forget_unnamed_turns(st),
            _ => false,
        };
        if closed {
            self.close_family_turn(st);
        }
    }

    /// The family has no turn running: the tab's turn ends, with the outcome of the tab's own turn. After an
    /// interrupt it first waits for the calls still running (`settle`).
    fn close_family_turn(&self, st: &mut State) {
        let something_runs = st.running_unread || !st.running.is_empty();
        let Some(turn) = st.turn.as_mut() else { return };
        if turn.interrupting && something_runs {
            turn.settling = true;
            return;
        }
        self.end_family_turn(st);
    }

    /// Ends the tab's turn, the family being idle, with the outcome of the tab's own turn.
    fn end_family_turn(&self, st: &mut State) {
        let (own, asked) = match st.turn.as_mut() {
            Some(turn) => (turn.own_end.take(), turn.interrupt_asked),
            None => return,
        };
        // A turn that finished before the user pressed interrupt, with a child still working, ends
        // interrupted: the key was what stopped the child.
        let (outcome, detail) = match own {
            Some((outcome, detail)) if !(asked && outcome == TurnOutcome::Completed) => (outcome, detail),
            None if !asked => (TurnOutcome::Completed, TurnEndDetail::default()),
            _ => translate::turn_ending(&TurnStatus::Interrupted, None),
        };
        self.end_turn(st, outcome, detail);
    }

    /// Ends the running turn with an event and forgets its per-turn bookkeeping.
    fn end_turn(&self, st: &mut State, outcome: TurnOutcome, detail: TurnEndDetail) {
        let Some(turn) = st.turn.take() else { return };
        let Some(turn_id) = turn.id else { return };
        let usage = turn.usage.as_ref().map(|u| translate::usage_of(u, st.model.as_deref()));
        // A call can finish after this turn, so keep the row's original turn until its result arrives.
        let left: Vec<(String, String)> = st
            .running
            .iter()
            .filter(|(_, kind)| kind.kept_doing().is_some())
            .map(|(key, _)| key.clone())
            .collect();
        for (thread, item) in &left {
            let row = row_of(st, thread, item);
            if st.open_tools.contains(&row) && st.background_rows.len() < RUNNING_CALLS_KEPT {
                st.background_rows
                    .entry((thread.clone(), item.clone()))
                    .or_insert_with(|| (row, turn_id.clone()));
            }
        }
        st.items.clear();
        st.message_text.clear();
        st.open_tools.clear();
        st.answered_rows.clear();
        st.calls.clear();
        if let Some(family) = st.family.as_mut() {
            family.reset_turns();
        }
        self.drop_cards(st, PermissionOutcome::CancelledByInterrupt);
        // What is still running now outlives the turn: a command the model put in the background.
        st.background.extend(left);
        st.background_unread |= st.running_unread;
        report_background(st);
        // Before the end, so the host holds them again by the time it sees the turn over.
        self.drop_steers(st, "the turn ended before Codex read it");
        st.events.push_back(AgentDomainEvent::TurnCompleted {
            turn_id,
            outcome,
            result_text: turn.last_message,
            stop_reason: None,
            usage,
            detail,
        });
    }
}

impl CodexProvider {
    /// After an interrupt, once every turn of the family has ended: the tab's turn ends when nothing Codex
    /// started is running any more. A command or file change still running past [`INTERRUPT_SETTLE`] (or
    /// a call Eitri could not read) stops the session, and the turn ends only once the child is gone, so
    /// no command Codex ran is still writing when the turn is reported over. A subagent's or tool server's
    /// call is not waited on past the settle time.
    fn settle(&self, st: &mut State) {
        if let Some(stopping) = st.stopping.as_ref() {
            if stopping.done.load(Ordering::Acquire) {
                let reason = std::mem::take(&mut st.stopping.as_mut().expect("checked above").reason);
                st.stopping = None;
                self.finish_stop(st, reason);
            }
            return;
        }
        if st.dead || st.closed {
            return;
        }
        if !st.turn.as_ref().is_some_and(|t| t.settling) {
            return;
        }
        // A family turn began meanwhile (one whose start could not be read included): the tab's turn waits
        // for it to end again, which settles anew. One that no message has named is not waited on, as
        // everywhere after an interrupt.
        forget_unnamed_turns(st);
        if st.family.as_ref().is_some_and(|f| !f.running_turns().is_empty()) {
            if let Some(turn) = st.turn.as_mut() {
                turn.settling = false;
            }
            return;
        }
        if !st.running_unread && st.running.is_empty() {
            self.end_family_turn(st);
            return;
        }
        let until = st.turn.as_ref().and_then(|t| t.settle_until);
        if until.is_some_and(|until| Instant::now() < until) {
            return;
        }
        let unread = st.running_unread.then_some(Running::Unread);
        let worst = unread
            .into_iter()
            .chain(st.running.values().copied())
            .find_map(|kind| kind.kept_doing());
        match worst {
            Some(kept_doing) => {
                eprintln!("agent: Codex {kept_doing} after the interrupt; its session is stopped");
                self.stop_session(st, stopped_after_interrupt(kept_doing));
            }
            None => self.end_family_turn(st),
        }
    }

    /// Stops the child, and with it everything it runs: stdin is closed, then its process group is killed.
    /// That waits, so it happens on a thread of its own; the pump learns from `Stopping::done` that it is
    /// over. Nothing is sent to Codex from here on.
    fn stop_session(&self, st: &mut State, reason: String) {
        st.dead = true;
        self.drop_cards(st, PermissionOutcome::CancelledByInterrupt);
        let done = Arc::new(AtomicBool::new(false));
        let mut worker = None;
        if let Some(link) = lock(&self.link).take() {
            let set = SetOnDrop(Arc::clone(&done));
            // Without a thread the closure, and the link in it, is dropped inside `spawn`, which kills the
            // group at once; the flag is set as it goes. The pump cannot read it before: it holds `st`.
            worker = std::thread::Builder::new()
                .name("eitri-codex-stop".to_string())
                .spawn(move || {
                    let _set = set;
                    external_link::stop_child(link);
                })
                .ok();
        } else {
            done.store(true, Ordering::Release);
        }
        st.stopping = Some(Stopping { done, worker, reason });
    }

    /// The child is gone: the rows still open end, the turn ends interrupted, and the session is over.
    fn finish_stop(&self, st: &mut State, reason: String) {
        if let Some(turn_id) = Self::tab_turn_id(st) {
            let mut rows: Vec<String> = st.open_tools.drain().collect();
            rows.sort();
            for row in rows {
                st.events.push_back(AgentDomainEvent::ToolCallCompleted {
                    turn_id: turn_id.clone(),
                    tool_use_id: row,
                    content: Value::String("stopped with the session after the interrupt".to_string()),
                    is_error: true,
                });
            }
        }
        st.running.clear();
        end_background_rows(st, "stopped with the session");
        st.running_unread = false;
        self.end_family_turn(st);
        report_background(st);
        st.stopped_whole = true;
        st.events.push_back(AgentDomainEvent::SessionUnavailable { reason });
    }

    /// A call that the interrupted turn left running ended while the tab's turn waited for it: its row, if
    /// the tab drew one, gets the result.
    fn end_settled_row(&self, st: &mut State, n: &ItemNotification) {
        let Some(turn_id) = Self::tab_turn_id(st) else { return };
        let (Some((_, _)), Some((content, is_error)), Some(id)) = (
            translate::tool_start(&n.item),
            translate::tool_result(&n.item),
            n.item.id(),
        ) else {
            return;
        };
        st.calls.forget(&n.thread_id, id);
        let row = row_of(st, &n.thread_id, id);
        if st.open_tools.remove(&row) {
            st.events.push_back(AgentDomainEvent::ToolCallCompleted {
                turn_id,
                tool_use_id: row,
                content,
                is_error,
            });
        }
    }
}

/// Sends `BackgroundCalls` when the number of calls left running after a turn changed. A session that is
/// over has none: its child and everything it started are gone.
fn report_background(st: &mut State) {
    let running = &st.running;
    st.background.retain(|key| running.contains_key(key));
    st.background_rows.retain(|key, _| st.background.contains(key));
    if st.dead || st.closed {
        st.background.clear();
        // A session being stopped is dead before its child is gone, and no completion is read after that.
        end_background_rows(st, "stopped with the session");
        st.background_unread = false;
    }
    let count = u32::try_from(st.background.len())
        .unwrap_or(u32::MAX)
        .saturating_add(u32::from(st.background_unread));
    if count != st.background_reported {
        st.background_reported = count;
        st.events
            .push_back(AgentDomainEvent::BackgroundCalls { running: count });
    }
}

/// Ends the rows of calls a completed turn left running, under the turn that drew each, when the session ends
/// before they do: no completion comes for them any more, and a row left open would read as abandoned.
fn end_background_rows(st: &mut State, said: &str) {
    let mut rows: Vec<(String, String)> = st.background_rows.drain().map(|(_, row)| row).collect();
    rows.sort();
    for (row, turn_id) in rows {
        st.events.push_back(AgentDomainEvent::ToolCallCompleted {
            turn_id,
            tool_use_id: row,
            content: Value::String(said.to_string()),
            is_error: true,
        });
    }
}

/// Counts a call Codex started as running until it is reported ended.
fn note_running(st: &mut State, thread: &str, item: &str, kind: Running) {
    let key = (thread.to_string(), item.to_string());
    if st.running.len() >= RUNNING_CALLS_KEPT && !st.running.contains_key(&key) {
        st.running_unread = true;
        return;
    }
    st.running.insert(key, kind);
}

/// Ends the row Eitri added for `card`, if it added one and Codex has not reported the call since, with the
/// card's answer as its result.
fn end_added_row(st: &mut State, card: WaitingCard, outcome: PermissionOutcome) {
    let Some(added) = card.added_row else { return };
    if !st.open_tools.remove(&added.row) {
        return;
    }
    let (said, is_error) = match outcome {
        PermissionOutcome::Allowed => ("allowed once", false),
        PermissionOutcome::Denied => ("declined", true),
        _ => ("not answered", true),
    };
    st.answered_rows.insert(added.row.clone());
    st.events.push_back(AgentDomainEvent::ToolCallCompleted {
        turn_id: added.turn_id,
        tool_use_id: added.row,
        content: Value::String(format!("{}: {said}", added.subject)),
        is_error,
    });
}

/// The row id of item `item` of `thread` (`translate::row_id`).
fn row_of(st: &State, thread: &str, item: &str) -> String {
    translate::row_id(st.thread_id.as_deref().unwrap_or_default(), thread, item)
}

/// Whether a thread is the tab's own.
fn is_tab(st: &State, thread_id: &str) -> bool {
    st.thread_id.as_deref() == Some(thread_id)
}

/// Whether a thread is the tab's or one announced from it.
fn in_family(st: &State, thread_id: &str) -> bool {
    st.family.as_ref().is_some_and(|f| f.contains(thread_id))
}

/// `input` of a call made on a subagent's thread, naming the subagent so its rows and cards say whose they
/// are. The key is Eitri's alone: one the item carried itself is dropped, so the tab's own call never
/// reads as a subagent's.
fn labelled(st: &State, thread_id: &str, mut input: Value) -> Value {
    let Some(object) = input.as_object_mut() else {
        return input;
    };
    object.remove("subagent");
    if let Some(label) = st.family.as_ref().and_then(|f| f.label(thread_id)) {
        object.insert("subagent".to_string(), Value::String(label.to_string()));
    }
    input
}

/// A family turn whose start could not be read has no id, so an interrupt cannot be sent to it, and should
/// its end never be readable either the tab would stay busy for good. Once the user has asked to interrupt
/// and only such turns are left running, they are not waited on: they are forgotten, the tab's own among
/// them ending interrupted. Should one of them go on, what it sends later names no running turn and is
/// dropped like any other message about a turn the tab does not follow. Returns whether this left the
/// family idle.
fn forget_unnamed_turns(st: &mut State) -> bool {
    if !st.turn.as_ref().is_some_and(|t| t.interrupting) {
        return false;
    }
    let Some(family) = st.family.as_mut() else { return false };
    let running = family.running_turns();
    if running.is_empty() || running.iter().any(|(_, turn)| turn != UNREAD_TURN) {
        return false;
    }
    let own = running.iter().any(|(thread, _)| thread == family.tab_thread());
    let mut edge = FamilyEdge::Ignored;
    for (thread, _) in &running {
        edge = family.turn_completed(thread, UNREAD_TURN);
    }
    if own {
        if let Some(turn) = st.turn.as_mut() {
            turn.own_end = Some(translate::turn_ending(&TurnStatus::Interrupted, None));
        }
    }
    eprintln!("agent: an interrupt cannot name a turn whose start Codex sent unreadably; it is no longer waited on");
    edge == FamilyEdge::Closed
}

/// Logs a thread outside the tab's family, once per thread, and drops what it sent.
fn note_foreign(st: &mut State, thread_id: &str) {
    let key = format!("thread {thread_id}");
    if st.seen_unknown.len() < UNKNOWN_METHODS_KEPT && st.seen_unknown.insert(key) {
        eprintln!("agent: Codex sent something about a thread outside this tab's family; dropped");
    }
}

/// The session follows only so many subagents. One it would have to leave out could write files while the
/// tab reads as idle and the turn's review is taken, so the session is failed instead.
fn family_full() -> PolicyMismatch {
    PolicyMismatch {
        requested: format!("at most {MEMBERS_MAX} subagents followed in one tab"),
        reported: "Codex started more subagents than Eitri can follow".to_string(),
    }
}

/// Logs a method Eitri does not handle, once per name, without its payload.
fn note_unknown(st: &mut State, what: &str, method: &str) {
    if st.seen_unknown.len() < UNKNOWN_METHODS_KEPT && st.seen_unknown.insert(method.to_string()) {
        eprintln!("agent: Codex sent {what} Eitri does not handle: {method}");
    }
}

// ------------------------------------------------------------ moving the chat to another process

impl CodexProvider {
    /// Whether a running turn moves with the chat. Codex keeps its turn when its stdio changes hands, so
    /// a chat moves mid-turn as well as between turns.
    pub fn live_move(&self) -> bool {
        true
    }

    /// Why the provider cannot freeze now, if it cannot.
    fn move_blocker(&self, st: &State) -> Option<MoveRefusedWhy> {
        if st.adopted_at.is_some() {
            Some(MoveRefusedWhy::Link(NOT_COMMITTED.to_string()))
        } else if st.stopping.is_some() {
            Some(MoveRefusedWhy::Stopping)
        } else if st.closed || st.dead {
            Some(MoveRefusedWhy::Ended)
        } else if st.history_loading {
            Some(MoveRefusedWhy::HistoryLoading)
        } else if st.thread_id.is_none() || st.turn.as_ref().is_some_and(|turn| turn.id.is_none()) {
            Some(MoveRefusedWhy::Starting)
        } else if st.turn.is_some() && !self.live_move() {
            Some(MoveRefusedWhy::TurnRunning)
        } else {
            None
        }
    }

    /// Stops the provider where it stands so that another process can take the chat up with
    /// [`adopt`](Self::adopt): the link stops (its reader at once, its writer once what was queued is
    /// written), and the provider's state comes out as data, every waiting answer by its request id or, if
    /// it had come, as the answer itself. Nothing is written to Codex and nothing is ended: the child, its
    /// stdin, stdout and stderr go with the returned descriptors. From here on this provider is closed and
    /// pumps nothing.
    ///
    /// Refused, with the provider left as it was, while the session is being stopped, a resumed
    /// conversation's history is being read, the session or a turn is still starting 2 s on, an adopted
    /// chat's own move is not committed yet, or the link cannot stop (a caller waiting on an answer that
    /// does not come within 2 s, a write Codex does not take).
    ///
    /// The provider stays locked while the link stops, so that nothing is pumped from a half-stopped
    /// link: its other calls wait for up to those 2 s, and a caller on a UI thread freezes from another.
    pub fn freeze_for_move(&self) -> Result<(CodexProviderHandoff, LinkHandoff, [OwnedFd; 3]), MoveRefusedWhy> {
        let deadline = Instant::now() + START_WAIT;
        let mut st = loop {
            let st = lock(&self.state);
            match self.move_blocker(&st) {
                None => break st,
                // The start's caller waits outside this lock and takes it again once its answer comes.
                Some(MoveRefusedWhy::Starting) if Instant::now() < deadline => {
                    drop(st);
                    std::thread::sleep(Duration::from_millis(10));
                }
                Some(why) => return Err(why),
            }
        };
        let Some(link) = lock(&self.link).take() else {
            return Err(MoveRefusedWhy::Ended);
        };
        let (link_handoff, fds) = match link.stop_for_move() {
            Ok(stopped) => stopped,
            Err(refused) => {
                let why = refused.reason;
                *lock(&self.link) = Some(*refused.link);
                return Err(MoveRefusedWhy::Link(why));
            }
        };
        let gone = State {
            closed: true,
            dead: true,
            ..State::default()
        };
        let state = std::mem::replace(&mut *st, gone);
        Ok((self.handoff_of(state, Instant::now()), link_handoff, fds))
    }

    fn handoff_of(&self, state: State, now: Instant) -> CodexProviderHandoff {
        let CodexProvider {
            // Stopped and handed over as the link's own record.
            link: _,
            // Taken apart below.
            state: _,
            // The link's record carries the child's pid.
            pid: _,
            version,
            state_home,
            codex_home,
            steer_unsupported,
        } = self;
        let State {
            thread_id,
            family,
            model,
            events,
            held,
            turn,
            items,
            message_text,
            open_tools,
            calls,
            cards,
            next_card,
            next_answered_row,
            answered_rows,
            seen_unknown,
            early_turns,
            last_limits,
            watch,
            blocked,
            // A freeze is refused while the history is read, so it is false.
            history_loading: _,
            history,
            running,
            running_unread,
            // A freeze is refused while the child is being stopped: the thread doing it stays here.
            stopping: _,
            interrupts,
            stopped_whole,
            background,
            background_rows,
            background_unread,
            background_reported,
            start_abandoned,
            user_reviewed,
            steers,
            closing_notice,
            // A freeze is refused once the session is closed or its child gone, so both are false.
            closed: _,
            dead: _,
            thought_open,
            thought_scanned,
            thought_part,
            // A freeze is refused until an adopted chat's move is committed, so it is `None`.
            adopted_at: _,
        } = state;
        CodexProviderHandoff {
            version: *version,
            state_home: state_home.clone(),
            codex_home: codex_home.clone(),
            steer_unsupported: steer_unsupported.load(Ordering::Acquire),
            thread_id,
            family,
            model,
            events,
            held,
            turn: turn.map(|turn| turn.into_moved(now)),
            items,
            message_text,
            open_tools,
            calls: calls.into_record(),
            cards,
            next_card,
            next_answered_row,
            answered_rows,
            seen_unknown,
            early_turns,
            last_limits,
            watch,
            blocked: blocked.map(str::to_string),
            history,
            running: running.into_iter().collect(),
            running_unread,
            interrupts: interrupts.into_iter().map(|sent| sent.into_moved(now)).collect(),
            stopped_whole,
            background,
            background_rows: background_rows.into_iter().collect(),
            background_unread,
            background_reported,
            start_abandoned,
            user_reviewed,
            steers: steers.into_iter().map(SentSteer::into_moved).collect(),
            closing_notice: closing_notice.map(|notice| (notice.reason, offset_ms(notice.until, now))),
            thought_open,
            thought_scanned,
            thought_part,
        }
    }

    /// Takes up a chat another process froze with [`freeze_for_move`](Self::freeze_for_move). The link is
    /// adopted parked and every waiting answer is bound again by its request id, so nothing is read from or
    /// written to Codex until [`commit`](Self::commit); dropping the provider before then only closes this
    /// process's copies of the descriptors.
    pub fn adopt(handoff: CodexProviderHandoff, link: LinkHandoff, fds: [OwnedFd; 3]) -> io::Result<CodexProvider> {
        let pid = link.pid;
        let handed = handed_ids(&link);
        let link = JsonRpcLink::adopt(link, fds, approvals::over_cap_reply)?;
        Self::rebuilt(handoff, link, pid, &handed)
    }

    /// Starts the adopted link once the move is committed: what was carried is read first, then Codex's
    /// stdout, and the answers given meanwhile are written. Nothing to do for a running link.
    ///
    /// Until then [`pump`](AgentProvider::pump) reports nothing and acts on no deadline. The deadlines
    /// carried as time remaining start running here: they are moved on by the time the chat waited for its
    /// commit, while nothing of Codex's could be read.
    pub fn commit(&self) -> io::Result<()> {
        let mut st = lock(&self.state);
        let started = match lock(&self.link).as_mut() {
            Some(link) => link.start_after_commit(),
            None => Ok(()),
        };
        // Pumping resumes even when the link's threads could not start: the tab then learns that the
        // session ended, instead of waiting for good.
        if let Some(adopted_at) = st.adopted_at.take() {
            later_deadlines(&mut st, adopted_at.elapsed());
        }
        started
    }

    /// The source's way back when a move is given up before its commit: this provider takes its own
    /// frozen state up again and reads on at once, as it would have without the move.
    pub fn resume_after_move(
        &self,
        handoff: CodexProviderHandoff,
        link: LinkHandoff,
        fds: [OwnedFd; 3],
    ) -> io::Result<()> {
        let pid = link.pid;
        let handed = handed_ids(&link);
        let link = JsonRpcLink::resume_after_move(link, fds, approvals::over_cap_reply)?;
        let CodexProvider {
            link,
            state,
            // The source's own, unchanged by the move.
            pid: _,
            version: _,
            state_home: _,
            codex_home: _,
            steer_unsupported,
        } = Self::rebuilt(handoff, link, pid, &handed)?;
        self.steer_unsupported
            .store(steer_unsupported.load(Ordering::Acquire), Ordering::Release);
        let mut st = lock(&self.state);
        *st = state.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
        // The source's link reads on at once, so there is no commit to wait for.
        st.adopted_at = None;
        *lock(&self.link) = link.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(())
    }

    fn rebuilt(
        handoff: CodexProviderHandoff,
        link: JsonRpcLink,
        pid: u32,
        handed: &[u64],
    ) -> io::Result<CodexProvider> {
        let now = Instant::now();
        let CodexProviderHandoff {
            version,
            state_home,
            codex_home,
            steer_unsupported,
            thread_id,
            family,
            model,
            events,
            held,
            turn,
            items,
            message_text,
            open_tools,
            calls,
            cards,
            next_card,
            next_answered_row,
            answered_rows,
            seen_unknown,
            early_turns,
            last_limits,
            watch,
            blocked,
            history,
            running,
            running_unread,
            interrupts,
            stopped_whole,
            background,
            background_rows,
            background_unread,
            background_reported,
            start_abandoned,
            user_reviewed,
            steers,
            closing_notice,
            thought_open,
            thought_scanned,
            thought_part,
        } = handoff;
        let blocked = match blocked {
            None => None,
            Some(why) => Some(
                BLOCKED_REASONS
                    .iter()
                    .copied()
                    .find(|known| *known == why)
                    .ok_or_else(|| {
                        invalid(format!(
                            "the session was blocked for a reason this build does not know: {why}"
                        ))
                    })?,
            ),
        };
        let calls = HeldItems::from_record(calls).map_err(invalid)?;
        let interrupts = interrupts
            .into_iter()
            .map(|moved| SentInterrupt::from_moved(moved, &link, now))
            .collect::<io::Result<Vec<_>>>()?;
        let steers = steers
            .into_iter()
            .map(|moved| SentSteer::from_moved(moved, &link))
            .collect::<io::Result<SteerOrder<_>>>()?;
        forget_unclaimed(&link, handed);
        let state = State {
            thread_id,
            family,
            model,
            events,
            held,
            turn: turn.map(|moved| ActiveTurn::from_moved(moved, now)),
            items,
            message_text,
            open_tools,
            calls,
            cards,
            next_card,
            next_answered_row,
            answered_rows,
            seen_unknown,
            early_turns,
            last_limits,
            watch,
            blocked,
            history_loading: false,
            history,
            running: running.into_iter().collect(),
            running_unread,
            stopping: None,
            interrupts,
            stopped_whole,
            background,
            background_rows: background_rows.into_iter().collect(),
            background_unread,
            background_reported,
            start_abandoned,
            user_reviewed,
            steers,
            // What the child said before its end is not carried; the moved notice goes at its first
            // stderr or its deadline, as one made with no earlier stderr does.
            closing_notice: closing_notice.map(|(reason, ms)| ClosingNotice {
                reason,
                until: at_offset(ms, now),
                stderr_at_close: String::new(),
            }),
            closed: false,
            dead: false,
            thought_open,
            thought_scanned,
            thought_part,
            adopted_at: Some(now),
        };
        Ok(CodexProvider {
            link: Mutex::new(Some(link)),
            state: Mutex::new(state),
            pid,
            version,
            state_home,
            codex_home,
            steer_unsupported: AtomicBool::new(steer_unsupported),
        })
    }
}

impl AgentProvider for CodexProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            interrupt: true,
            // Until Codex answers `turn/steer` with an unknown method: the tab then says Enter holds.
            steer: !self.steer_unsupported.load(Ordering::Acquire),
            ..Default::default()
        }
    }

    fn info(&self) -> ProviderInfo {
        let untested = self.version > HIGHEST_TESTED_CODEX;
        let version = version_text(self.version);
        let tested = format!(
            "tested {} to {}",
            version_text(MIN_CODEX),
            version_text(HIGHEST_TESTED_CODEX)
        );
        ProviderInfo {
            // A version inside the tested range says so; one above it is named as untested beside the
            // newest one that was tested.
            build_description: Some(if untested {
                format!("Codex {version}")
            } else {
                format!("Codex {version} ({tested})")
            }),
            startup_diagnostics: if untested {
                vec![format!(
                    "untested Codex {version} (tested up to {})",
                    version_text(HIGHEST_TESTED_CODEX)
                )]
            } else {
                Vec::new()
            },
            configuration_home: Some(self.codex_home.display().to_string()),
            ..Default::default()
        }
    }

    fn create_session(&self, _: CreateSessionRequest) -> Result<String, ProviderError> {
        Err(ProviderError::UnsupportedCapability("external"))
    }

    fn resume_session(&self, _: ResumeSessionRequest) -> Result<String, ProviderError> {
        Err(ProviderError::UnsupportedCapability("external"))
    }

    fn send_turn(&self, _: SendTurnRequest) -> Result<String, ProviderError> {
        Err(ProviderError::UnsupportedCapability("external"))
    }

    /// Stops the session for what its turns left running, as an interrupt does for what a turn keeps
    /// running: stdin's end, then the process group. Only between turns.
    fn stop_background(&self, session_id: &str, shown: u32, _tasks: &[String]) -> Result<(), ProviderError> {
        let mut st = lock(&self.state);
        if st.thread_id.as_deref() != Some(session_id) {
            return Err(provider_error(
                ProviderErrorCode::SessionNotFound,
                "no such Codex session",
            ));
        }
        Self::require_open(&st)?;
        // Before the commit the link does not hold the child yet, so stopping it here would end nothing.
        if st.adopted_at.is_some() {
            return Err(provider_error(ProviderErrorCode::NoActiveTurn, NOT_COMMITTED));
        }
        if st.turn.is_some() {
            return Err(provider_error(
                ProviderErrorCode::TurnAlreadyActive,
                "a turn is running: interrupt it, which also stops what is left running",
            ));
        }
        report_background(&mut st);
        if st.background_reported == 0 {
            return Err(provider_error(
                ProviderErrorCode::NoActiveTurn,
                "nothing is running in the background",
            ));
        }
        // The user said yes to a number the panel had; one that moved since is asked about again.
        if st.background_reported != shown {
            return Err(provider_error(
                ProviderErrorCode::NoActiveTurn,
                format!(
                    "{} running in the background now, not {shown}: nothing was stopped; ask again",
                    st.background_reported
                ),
            ));
        }
        eprintln!("agent: the user stopped what Codex left running in the background; its session is stopped");
        self.stop_session(&mut st, STOPPED_BACKGROUND.to_string());
        Ok(())
    }

    fn interrupt_turn(&self, request: InterruptTurnRequest) -> Result<(), ProviderError> {
        let mut st = lock(&self.state);
        if st.thread_id.as_deref() != Some(request.session_id.as_str()) {
            return Err(provider_error(
                ProviderErrorCode::SessionNotFound,
                "no such Codex session",
            ));
        }
        Self::require_open(&st)?;
        let Some(turn) = st.turn.as_mut() else {
            return Err(provider_error(ProviderErrorCode::NoActiveTurn, "no turn is running"));
        };
        if turn.id.is_none() {
            // The answer that carries the turn id has not come: the interrupt goes out when it does.
            turn.interrupt_wanted = true;
            return Ok(());
        }
        self.interrupt_running(&mut st);
        Ok(())
    }

    /// Never waits: the answer is read by `pump` (`follow_steers`), and what became of the message arrives
    /// as an event. A message behind one whose answer has not come waits unsent, so Codex sees them in
    /// order. Refused at once, nothing sent, when the turn Codex runs is not the one the host saw, it has no
    /// id yet, an interrupt was asked for (Codex would forget the message), the tab's own turn is over while
    /// its subagents finish, only a subagent's turn runs, or this Codex does not know the method.
    fn steer_turn(&self, request: SteerTurnRequest) -> Result<(), ProviderError> {
        let mut st = lock(&self.state);
        if st.thread_id.as_deref() != Some(request.session_id.as_str()) {
            return Err(provider_error(
                ProviderErrorCode::SessionNotFound,
                "no such Codex session",
            ));
        }
        Self::require_open(&st)?;
        if let Some(why) = st.blocked {
            return Err(provider_error(ProviderErrorCode::ProviderUnavailable, why));
        }
        if self.steer_unsupported.load(Ordering::Acquire) {
            return Err(ProviderError::UnsupportedCapability("steer"));
        }
        let no_turn = |why: &str| provider_error(ProviderErrorCode::NoActiveTurn, why);
        let Some(turn) = st.turn.as_ref() else {
            return Err(no_turn("no turn is running"));
        };
        if turn.id.as_deref() != Some(request.expected_turn_id.as_str()) {
            return Err(no_turn("the running turn is not the one the message was typed in"));
        }
        if turn.interrupting || turn.own_end.is_some() || turn.settling {
            return Err(no_turn("the turn is ending"));
        }
        // Codex steers the tab thread's own turn: a turn a subagent opened, or one whose start could not be
        // read, has nothing on the tab's thread to take the message.
        let own = st
            .family
            .as_ref()
            .and_then(|f| f.running_on(f.tab_thread()))
            .map(str::to_string);
        match own.as_deref() {
            Some(id) if id == request.expected_turn_id && id != UNREAD_TURN && !id.starts_with(UNREAD_TURN_PREFIX) => {}
            Some(_) => return Err(no_turn("Eitri could not read the turn's start")),
            None => return Err(no_turn("only a subagent is running")),
        }
        let thread_id = st.thread_id.clone().unwrap_or_default();
        let params = to_json(&TurnSteerParams::new(
            thread_id,
            request.client_message_id.clone(),
            request.text,
            request.expected_turn_id,
        ))?;
        st.steers.open(&request.client_message_id, "Codex", |behind| {
            let (answer, waiting) = if behind {
                (None, Some(params))
            } else {
                (Some(self.request(method::TURN_STEER, params)?), None)
            };
            Ok(SentSteer {
                client_id: request.client_message_id.clone(),
                as_typed: request.as_typed,
                answer,
                waiting,
            })
        })
    }

    fn resolve_permission(&self, request: ResolvePermissionRequest) -> Result<(), ProviderError> {
        let mut st = lock(&self.state);
        if st.thread_id.as_deref() != Some(request.session_id.as_str()) {
            return Err(provider_error(
                ProviderErrorCode::SessionNotFound,
                "no such Codex session",
            ));
        }
        Self::require_open(&st)?;
        let answer = match request.decision {
            PermissionDecision::Allow => CardAnswer::Approve,
            // Codex takes no reason with a deny; the decline alone is sent.
            PermissionDecision::Deny { .. } => CardAnswer::Deny,
            // Codex has no handling of its own to hand a request back to.
            PermissionDecision::Defer => return Err(ProviderError::UnsupportedCapability("defer")),
        };
        let Some(at) = st.cards.iter().position(|(id, _)| *id == request.permission_id) else {
            return Err(provider_error(
                ProviderErrorCode::PermissionNotFound,
                "no permission request is waiting",
            ));
        };
        // Sent before the card goes, as in the ACP provider: an answer the link did not take leaves the
        // card waiting, to be answered again.
        self.respond_to_card(st.cards[at].1.request_id.clone(), st.cards[at].1.form.reply(answer))?;
        let (permission_id, card) = st.cards.remove(at);
        let outcome = match answer {
            CardAnswer::Approve => PermissionOutcome::Allowed,
            _ => PermissionOutcome::Denied,
        };
        st.events
            .push_back(AgentDomainEvent::PermissionResolved { permission_id, outcome });
        end_added_row(&mut st, card, outcome);
        Ok(())
    }

    fn close_session(&self, request: CloseSessionRequest) -> Result<(), ProviderError> {
        let stop_worker;
        let thread_id = {
            let mut st = lock(&self.state);
            if st.closed {
                return Ok(());
            }
            if st.thread_id.is_some() && st.thread_id.as_deref() != Some(request.session_id.as_str()) {
                return Err(provider_error(
                    ProviderErrorCode::SessionNotFound,
                    "no such Codex session",
                ));
            }
            if !st.dead {
                // A running turn is interrupted first, so Codex has not left a command running.
                self.interrupt_running(&mut st);
            }
            self.drop_cards(&mut st, PermissionOutcome::CancelledBySessionClose);
            self.drop_steers(&mut st, "the session was closed before Codex read it");
            st.closed = true;
            end_background_rows(&mut st, "the session was closed while it ran");
            st.events.push_back(AgentDomainEvent::SessionClosed {
                reason: "closed_by_host".to_string(),
            });
            // A stop under way is the close's own shutdown now: waited for below, and the session's end
            // is this close rather than the stop's.
            stop_worker = st.stopping.take().and_then(|stopping| stopping.worker);
            st.thread_id.clone()
        };
        if let Some(worker) = stop_worker {
            let _ = worker.join();
        }
        let dead = lock(&self.state).dead;
        if let (Some(thread_id), false) = (thread_id, dead) {
            if let Ok(params) = to_json(&ThreadUnsubscribeParams { thread_id }) {
                if let Ok(pending) = self.request(method::THREAD_UNSUBSCRIBE, params) {
                    // Whatever the answer, the pipe closes next; a failure only means Codex was already going.
                    let _ = pending.wait(Instant::now() + UNSUBSCRIBE_WAIT);
                }
            }
        }
        external_link::stop_linked_child(&self.link);
        Ok(())
    }

    fn pump(&self) -> Vec<AgentDomainEvent> {
        let mut st = lock(&self.state);
        if st.adopted_at.is_some() {
            return Vec::new();
        }
        let caught_up = st.closed || self.drain(&mut st);
        self.follow_interrupts(&mut st);
        self.follow_steers(&mut st, true);
        self.finish_closing_notice(&mut st);
        // Only once everything Codex wrote has been read: a turn end waiting behind a backlog is not overdue.
        if caught_up {
            self.stop_an_overdue_interrupt(&mut st);
        }
        self.settle(&mut st);
        report_background(&mut st);
        follow_thought(&mut st);
        st.thought_scanned = 0;
        st.events.drain(..).collect()
    }
    #[cfg(target_os = "linux")]
    fn mover(&self) -> Option<&dyn crate::chat_mover::ChatMover> {
        Some(self)
    }
}

/// Whether the projection's thinking block is open once the queued events are folded: carried across
/// pumps in `thought_open`, and followed through each event once.
fn follow_thought(st: &mut State) -> bool {
    for event in st.events.iter().skip(st.thought_scanned) {
        st.thought_open = thinking_open_after(st.thought_open, event);
    }
    st.thought_scanned = st.events.len();
    st.thought_open
}

/// The text to send for a thought from `part` of reasoning item `item`. While a thought is open, one from
/// another part or another item starts after a blank line: the projection and the page fold back-to-back
/// thinking deltas into one block with nothing between them, and a resumed turn joins the same parts with
/// a blank line (`resume::turn_events`).
fn thought_text(st: &mut State, item: String, part: ThoughtPart, text: String) -> String {
    let open = follow_thought(st);
    let here = (item, part);
    let text = if open && st.thought_part.as_ref().is_some_and(|last| *last != here) {
        format!("\n\n{text}")
    } else {
        text
    };
    st.thought_part = Some(here);
    text
}

impl CodexProvider {
    /// Remembers `turn` as the running turn of `thread`, not (yet) in the family, so a child announced after
    /// its turn began is followed from that turn. Past `EARLY_TURNS_KEPT` threads the session is failed: a
    /// turn not kept would leave its child unfollowed once it is announced.
    fn remember_early_turn(&self, st: &mut State, thread: &str, turn: String) {
        if st.early_turns.len() < EARLY_TURNS_KEPT || st.early_turns.contains_key(thread) {
            st.early_turns.insert(thread.to_string(), turn);
            return;
        }
        self.trip(
            st,
            PolicyMismatch {
                requested: format!("at most {EARLY_TURNS_KEPT} subagent turns kept before their announcement"),
                reported: "Codex started more turns on threads it had not announced".to_string(),
            },
        );
    }

    /// Fails the session: Codex did something other than what Eitri asked for. The host fails the tab on the
    /// event and answers nothing on its own afterwards; here every card is declined and no turn starts.
    fn trip(&self, st: &mut State, mismatch: PolicyMismatch) {
        if st.blocked.is_some() {
            return;
        }
        eprintln!("agent: {mismatch}");
        st.blocked = Some(BLOCKED_POLICY);
        // Cancelled, not declined: a cancel ends the asking thread's turn at once, where a decline would let
        // it go on with what needs no request until the host shuts the session down.
        self.drop_cards_with(st, PermissionOutcome::ProviderFailed, CardAnswer::Interrupt);
        st.events.push_back(mismatch.into_event());
    }

    /// `thread/start`, or `thread/resume` and the history after it.
    fn open_session(&self, req: ExternalStart, resume: Option<&str>) -> Result<String, ProviderError> {
        let config = session_config(&req.plan)
            .map_err(|e| provider_error(ProviderErrorCode::InvalidConfiguration, e.to_string()))?;
        let TurnPolicy { reviewer, sandbox } = req.policy;
        let (verb, pending) = match resume {
            None => (
                "the session start",
                self.request(
                    method::THREAD_START,
                    to_json(&ThreadStartParams::new(req.cwd.clone(), reviewer, sandbox, config))?,
                )?,
            ),
            Some(id) => (
                "the session resume",
                self.request(
                    method::THREAD_RESUME,
                    to_json(&ThreadResumeParams::new(
                        id.to_string(),
                        req.cwd.clone(),
                        reviewer,
                        sandbox,
                        config,
                    ))?,
                )?,
            ),
        };
        let answer = match pending.wait(Instant::now() + START_TIMEOUT) {
            Ok(answer) => answer,
            // Codex refuses a thread another client holds open, among other things. Its own words are
            // what the user needs, shown as the verdict on the resume.
            Err(LinkError::Rpc { message, .. }) if resume.is_some() => {
                eprintln!("agent: Codex refused to resume a session: {message}");
                return Ok(self.resume_not_continued(
                    &req,
                    resume.unwrap_or_default(),
                    ResumeStatus::Rejected,
                    None,
                    Some(message),
                ));
            }
            Err(e) => return Err(self.failure(e, verb)),
        };
        let started: ThreadStartResponse = from_json(answer, verb)?;
        if let Some(requested) = resume.filter(|requested| *requested != started.thread.id) {
            // Another conversation than the one asked for: none of it is shown, and no turn goes to it.
            eprintln!("agent: Codex resumed another conversation than the one asked for");
            return Ok(self.resume_not_continued(
                &req,
                requested,
                ResumeStatus::Attached,
                Some(started.thread.id),
                None,
            ));
        }

        let thread_id = started.thread.id.clone();
        let model = started.model.clone().unwrap_or_default();
        let effort = started.reasoning_effort.clone();
        let mismatch = tripwire::check_echo(&expected_echo(reviewer, sandbox), &started.echo())
            .and_then(|()| {
                tripwire::check_instruction_sources(
                    &started.instruction_sources,
                    &tripwire::dirs_not_trusted(&req.plan),
                    &self.codex_home,
                )
            })
            .err();
        {
            let mut st = lock(&self.state);
            st.thread_id = Some(thread_id.clone());
            st.family = Some(Family::new(thread_id.clone()));
            st.model = started.model;
            st.watch = TrustWatch::for_plan(&req.plan, &self.codex_home);
            st.events.push_back(AgentDomainEvent::SessionOpened {
                session_id: thread_id.clone(),
                provider_session_id: thread_id.clone(),
                model,
                cwd: started.cwd.unwrap_or(req.cwd),
            });
            if let Some(requested) = resume {
                st.events.push_back(AgentDomainEvent::ResumeOutcome {
                    requested_provider_session_id: requested.to_string(),
                    status: ResumeStatus::Attached,
                    attached_provider_session_id: Some(thread_id.clone()),
                    forked: false,
                    detail: None,
                });
            }
            // After a resume's verdict, which ends what was known about the process before it.
            if effort.is_some() {
                st.events.push_back(AgentDomainEvent::EffortReported { effort });
            }
            if let Some(mismatch) = mismatch {
                self.trip(&mut st, mismatch);
            }
        }
        if resume.is_some() {
            self.read_history(&thread_id);
        }
        Ok(thread_id)
    }

    /// A resume that did not continue the conversation asked for (Codex refused it, or continued another),
    /// as events: the session opens under the requested id so there is something to say the verdict on, and
    /// what Codex says about any other thread is not the tab's.
    fn resume_not_continued(
        &self,
        req: &ExternalStart,
        requested: &str,
        status: ResumeStatus,
        attached: Option<String>,
        detail: Option<String>,
    ) -> String {
        let mut st = lock(&self.state);
        st.history_loading = false;
        st.blocked = Some(BLOCKED_RESUME);
        st.thread_id = Some(requested.to_string());
        st.family = Some(Family::new(requested.to_string()));
        st.events.push_back(AgentDomainEvent::SessionOpened {
            session_id: requested.to_string(),
            provider_session_id: requested.to_string(),
            model: String::new(),
            cwd: req.cwd.clone(),
        });
        st.events.push_back(AgentDomainEvent::ResumeOutcome {
            requested_provider_session_id: requested.to_string(),
            status,
            attached_provider_session_id: attached,
            forked: false,
            detail,
        });
        requested.to_string()
    }

    /// Reads the thread's history page by page and keeps it for the host, which folds it ahead of
    /// everything Codex said meanwhile.
    fn read_history(&self, thread_id: &str) {
        let skip = lock(&self.state).blocked.is_some();
        let mut events = Vec::new();
        if !skip {
            let mut ask = |name: &str, params: Value| -> Result<Value, resume::AskError> {
                let mut pending = match lock(&self.link).as_ref() {
                    Some(link) => link.request(name, params),
                    None => return Err(LinkError::Closed.into()),
                };
                let until = Instant::now() + TURN_START_TIMEOUT;
                loop {
                    if let Some(answer) = pending.try_take() {
                        return answer.map_err(Into::into);
                    }
                    // What Codex says meanwhile is moved aside while the answer is awaited, so the link's
                    // reader is never left blocked with the answer behind it. Once as much is held as can
                    // be, the history stops: the rest would only wait behind it.
                    {
                        let mut st = lock(&self.state);
                        self.drain(&mut st);
                        if st.held.len() >= PUMP_BATCH {
                            return Err(resume::AskError::Flooded);
                        }
                    }
                    let now = Instant::now();
                    if now >= until {
                        return Err(LinkError::Timeout.into());
                    }
                    std::thread::sleep(HISTORY_POLL.min(until - now));
                }
            };
            events = resume::page_history(thread_id, Instant::now() + HISTORY_BUDGET, &mut ask);
        }
        let mut st = lock(&self.state);
        st.history = events;
        st.history_loading = false;
    }
}

impl ExternalProvider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    /// A Codex thread is continued with `thread/resume`.
    fn resumes(&self) -> bool {
        true
    }

    fn start(&self, req: ExternalStart) -> Result<String, ProviderError> {
        let resuming = req.resume.clone();
        {
            let mut st = lock(&self.state);
            Self::require_open(&st)?;
            if st.thread_id.is_some() {
                return Err(provider_error(
                    ProviderErrorCode::InvalidConfiguration,
                    "this Codex connection already has a session",
                ));
            }
            // Anything Codex says from here on waits until the history, if there is one, is in.
            st.history_loading = resuming.is_some();
        }
        let result = self.open_session(req, resuming.as_deref());
        if result.is_err() {
            lock(&self.state).history_loading = false;
        }
        result
    }

    fn take_history(&self) -> Vec<AgentDomainEvent> {
        std::mem::take(&mut lock(&self.state).history)
    }

    fn stopped_resumable(&self) -> bool {
        let st = lock(&self.state);
        st.stopped_whole && st.blocked.is_none()
    }

    fn send_turn_with(&self, req: SendTurnRequest, policy: TurnPolicy) -> Result<String, ProviderError> {
        let (thread_id, pending) = {
            let mut st = lock(&self.state);
            let busy = || provider_error(ProviderErrorCode::TurnAlreadyActive, "a turn is already running");
            Self::require_open(&st)?;
            if st.turn.is_some() {
                return Err(busy());
            }
            // What Codex already said is read first: a tripwire it holds must stop this turn, not the next.
            // Should it say more than a few batches hold, the turn waits rather than go out past an unread
            // message.
            let mut read_all = false;
            for _ in 0..SEND_DRAIN_BATCHES {
                if self.drain(&mut st) {
                    read_all = true;
                    break;
                }
            }
            if !read_all {
                return Err(busy());
            }
            Self::require_open(&st)?;
            if let Some(why) = st.blocked {
                return Err(provider_error(ProviderErrorCode::ProviderUnavailable, why));
            }
            if st.thread_id.as_deref() != Some(req.session_id.as_str()) {
                return Err(provider_error(
                    ProviderErrorCode::SessionNotFound,
                    "no such Codex session",
                ));
            }
            // A subagent's turn read just now may have opened one.
            if st.turn.is_some() {
                return Err(busy());
            }
            // Sent under the same lock as the check above: the pump cannot fold a tripwire in between.
            let params = to_json(&TurnStartParams::new(
                req.session_id.clone(),
                req.text,
                policy.reviewer,
                policy.sandbox,
            ))?;
            let pending = self.request(method::TURN_START, params)?;
            let mut turn = ActiveTurn::new(None);
            turn.reviewer = Some(policy.reviewer);
            st.turn = Some(turn);
            (req.session_id.clone(), pending)
        };
        // The user is told the turn did not start, so one Codex starts after all is not left running.
        let abandon_unanswered = |this: &Self| {
            let mut st = lock(&this.state);
            st.turn = None;
            st.start_abandoned = true;
        };
        let abandon = |this: &Self| lock(&this.state).turn = None;
        let answer = match self.wait(pending, TURN_START_TIMEOUT, "the turn start") {
            Ok(answer) => answer,
            Err(e @ ProviderError::Timeout) => {
                abandon_unanswered(self);
                return Err(e);
            }
            Err(e) => {
                abandon(self);
                return Err(e);
            }
        };
        let started: TurnStartResponse = match from_json(answer, "the turn start") {
            Ok(started) => started,
            Err(e) => {
                abandon(self);
                return Err(e);
            }
        };

        let turn_id = started.turn.id;
        let mut st = lock(&self.state);
        st.start_abandoned = false;
        if policy.reviewer == Reviewer::User {
            st.user_reviewed.insert((thread_id.clone(), turn_id.clone()));
        }
        let wanted = match st.turn.as_mut() {
            Some(turn) => {
                turn.id = Some(turn_id.clone());
                turn.interrupt_wanted
            }
            None => false,
        };
        if let Some(family) = st.family.as_mut() {
            family.turn_started(&thread_id, &turn_id);
        }
        st.events.push_back(AgentDomainEvent::TurnStarted {
            turn_id: turn_id.clone(),
        });
        if wanted {
            self.interrupt_running(&mut st);
        }
        Ok(turn_id)
    }
}
