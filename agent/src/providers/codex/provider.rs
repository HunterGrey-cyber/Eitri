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

use super::approvals::{self, AnswerForm, CardAnswer, Handling, HeldItems};
use super::config::{expected_echo, session_config};
use super::family::{Family, FamilyEdge, UNREAD_TURN};
use super::resume;
use super::self_check::{ensure_checked, SelfCheckError};
use super::translate::{self, parse_user_agent_version, version_text, SIGNED_OUT};
use super::tripwire::{self, PolicyMismatch, TrustWatch};
use super::wire::{
    self, method, AccountReadParams, AccountReadResponse, InitializeParams, InitializeResponse, ItemDelta,
    ItemNotification, ServerNotification, SubAgentKind, ThreadItem, ThreadResumeParams, ThreadStartParams,
    ThreadStartResponse, ThreadUnsubscribeParams, TurnInterruptParams, TurnNotification, TurnStartParams,
    TurnStartResponse, TurnStatus,
};
use crate::external::{ExternalProvider, ExternalStart, TurnPolicy};
use crate::jsonrpc_link::{Framing, Incoming, JsonRpcLink, LinkError, LinkSpec, Pending, Skipped};
use crate::projection::{ContentKind, PermissionOutcome, ResumeStatus, TurnEndDetail, TurnOutcome};
use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, PermissionDecision,
    ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest,
    ResumeSessionRequest, SendTurnRequest,
};
use crate::{AgentDomainEvent, SIDECAR_EXIT_GRACE, UNARY_RPC_TIMEOUT};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The oldest Codex Eitri drives. Below it a start is refused, naming both versions.
pub const MIN_CODEX: (u32, u32, u32) = (0, 160, 0);

/// The newest Codex whose traffic was recorded against this code. A newer one runs, with a note.
pub const HIGHEST_TESTED_CODEX: (u32, u32, u32) = (0, 160, 0);

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
/// Unknown method names remembered so each is logged once.
const UNKNOWN_METHODS_KEPT: usize = 64;
/// Turns of threads not (yet) in the family remembered at most, for a child announced after its turn began.
const EARLY_TURNS_KEPT: usize = 64;
/// The longest stderr tail put in a notice.
const NOTICE_TAIL_CHARS: usize = 400;

/// JSON-RPC's "method not found".
const METHOD_NOT_FOUND: i64 = -32601;

/// What `PermissionDenied.reason_type` says when Eitri itself answered a request.
const ANSWERED_BY_EITRI: &str = "eitri";
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
    /// The turns of the family that have been sent their one `turn/interrupt`.
    interrupted: HashSet<String>,
    /// The user pressed interrupt after the tab's own turn had ended, so only subagents were left to stop.
    interrupt_asked: bool,
    /// The user pressed interrupt: a family turn learnt about later is sent its interrupt too.
    interrupting: bool,
    /// How the tab's own thread's turn ended, once it has; the family may still be working then.
    own_end: Option<(TurnOutcome, TurnEndDetail)>,
    /// The last whole assistant message of the turn: the turn's result text.
    last_message: String,
    /// The thread's running token total as of the last report.
    usage: Option<wire::TokenUsage>,
}

impl ActiveTurn {
    fn new(id: Option<String>) -> ActiveTurn {
        ActiveTurn {
            id,
            interrupt_wanted: false,
            interrupted: HashSet::new(),
            interrupt_asked: false,
            interrupting: false,
            own_end: None,
            last_message: String::new(),
            usage: None,
        }
    }
}

/// A card waiting for its answer.
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
    /// Closed by the host.
    closed: bool,
    /// The child ended on its own.
    dead: bool,
}

pub struct CodexProvider {
    link: Mutex<Option<JsonRpcLink>>,
    state: Mutex<State>,
    pid: u32,
    version: (u32, u32, u32),
    state_home: PathBuf,
    /// Where Codex keeps the user's own configuration, as it reported.
    codex_home: PathBuf,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn provider_error(code: ProviderErrorCode, message: impl Into<String>) -> ProviderError {
    ProviderError::Provider {
        code,
        message: message.into(),
    }
}

/// The end of the child's stderr, short enough for a notice. Email addresses are already replaced.
fn notice_tail(tail: &str) -> String {
    let tail = tail.trim();
    let count = tail.chars().count();
    if count <= NOTICE_TAIL_CHARS {
        return tail.to_string();
    }
    tail.chars().skip(count - NOTICE_TAIL_CHARS).collect()
}

/// A failed call on the link as the error the host shows.
fn link_error(link: &JsonRpcLink, what: &str, e: LinkError) -> ProviderError {
    match e {
        LinkError::Timeout => ProviderError::Timeout,
        LinkError::Closed => {
            let tail = notice_tail(&link.stderr_tail());
            let suffix = if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            };
            ProviderError::Transport(format!("Codex ended before it answered {what}{suffix}"))
        }
        LinkError::Busy => ProviderError::Transport("Codex is not reading its input".to_string()),
        LinkError::TooLarge => provider_error(
            ProviderErrorCode::ProviderProtocolError,
            format!("Codex's answer to {what} was larger than 16 MiB"),
        ),
        LinkError::Rpc { message, .. } => provider_error(
            ProviderErrorCode::ProviderProtocolError,
            format!("Codex refused {what}: {message}"),
        ),
    }
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
    if let Some((_, value)) = env.iter().rev().find(|(name, _)| name == "CODEX_HOME") {
        return Some(value.clone());
    }
    if env_remove.iter().any(|name| name == "CODEX_HOME") {
        return None;
    }
    std::env::var_os("CODEX_HOME")
}

/// Refuses a relative `CODEX_HOME`. Codex resolves it against its working directory, which is the project
/// root, so a directory the project itself holds would be read as the user's own configuration: its MCP
/// servers, hooks, rules and skills would load whatever the user said about trusting the project. No
/// child is started under it. An empty value is Codex's own "unset" and an absolute one is the user's.
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
/// inherits. A tab's connect worker asks before it looks for the binary, since probing its version runs a
/// child too; [`CodexProvider::connect`] asks again.
pub fn refuse_relative_codex_home() -> Result<(), ProviderError> {
    refuse_relative_home(child_codex_home(&[], &[]).as_deref())
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
        refuse_relative_codex_home()?;
        match ensure_checked(&launch.program, &launch.state_home) {
            Ok(()) => {}
            Err(SelfCheckError::NotFound) => {
                return Err(provider_error(ProviderErrorCode::ProviderUnavailable, NOT_FOUND));
            }
            Err(e @ SelfCheckError::TooOld { .. }) => {
                return Err(provider_error(ProviderErrorCode::UnsupportedCliVersion, e.to_string()));
            }
            Err(e) => return Err(provider_error(ProviderErrorCode::ProviderUnavailable, e.to_string())),
        }
        let args = ["app-server", "--listen", "stdio://"]
            .iter()
            .map(OsString::from)
            .collect();
        Self::connect_with_args(launch, args)
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
        refuse_relative_home(child_codex_home(&env, &env_remove).as_deref())?;
        let link = JsonRpcLink::spawn(LinkSpec {
            program: launch.program.clone(),
            args,
            cwd: launch.cwd.clone(),
            framing: Framing::Bare,
            env,
            env_remove,
        })
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => provider_error(ProviderErrorCode::ProviderUnavailable, NOT_FOUND),
            _ => ProviderError::Transport(format!("Codex could not be started: {e}")),
        })?;
        let (version, codex_home) = Self::handshake(&link)?;
        let pid = link.pid();
        Ok(CodexProvider {
            link: Mutex::new(Some(link)),
            state: Mutex::new(State::default()),
            pid,
            version,
            state_home: launch.state_home,
            codex_home,
        })
    }

    fn handshake(link: &JsonRpcLink) -> Result<((u32, u32, u32), PathBuf), ProviderError> {
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

    /// Queues the one `turn/interrupt` each running turn of the family gets. Never waits: Codex answers a
    /// second interrupt of an interrupted turn only when the turn ends, so the answers are dropped unread.
    /// Interrupting the parent alone does not stop a child, so every family turn is named.
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
            let Ok(params) = to_json(&TurnInterruptParams { thread_id, turn_id }) else {
                continue;
            };
            if let Some(link) = lock(&self.link).as_ref() {
                drop(link.request(method::TURN_INTERRUPT, params));
            }
        }
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
            turn.interrupting = true;
        }
        for (permission_id, card) in std::mem::take(&mut st.cards) {
            if card.form.ends_the_turn(CardAnswer::Interrupt) {
                let ended = st
                    .family
                    .as_ref()
                    .and_then(|f| f.running_on(&card.thread_id))
                    .map(str::to_string);
                if let (Some(turn), Some(ended)) = (st.turn.as_mut(), ended) {
                    turn.interrupted.insert(ended);
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

    fn respond(&self, id: Value, result: Value) {
        if let Some(link) = lock(&self.link).as_ref() {
            link.respond(id, result);
        }
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

    /// Every card still waiting goes, with `outcome`: what it asked about can no longer happen.
    fn drop_cards(st: &mut State, outcome: PermissionOutcome) {
        for (permission_id, card) in std::mem::take(&mut st.cards) {
            st.events
                .push_back(AgentDomainEvent::PermissionResolved { permission_id, outcome });
            end_added_row(st, card, outcome);
        }
    }

    // ------------------------------------------------------------ the stream

    /// Reads what the link has, folding it into events. Holds the incoming messages back while a
    /// `turn/start` answer is awaited.
    fn drain(&self, st: &mut State) {
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
                return;
            }
            let next = match st.held.pop_front() {
                Some(message) => Some(message),
                None => lock(&self.link).as_ref().and_then(JsonRpcLink::try_recv),
            };
            let Some(message) = next else { return };
            handled += 1;
            self.fold(st, message);
        }
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
        // Nothing can answer what waited any more.
        Self::drop_cards(st, PermissionOutcome::ProviderFailed);
        // The end of the output stream can be read a moment before the last of stderr; give it a short
        // chance so the notice says why the child went.
        let mut tail = String::new();
        for _ in 0..5 {
            tail = lock(&self.link)
                .as_ref()
                .map(|l| notice_tail(&l.stderr_tail()))
                .unwrap_or_default();
            if !tail.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let suffix = if tail.is_empty() {
            String::new()
        } else {
            format!(": {tail}")
        };
        st.events.push_back(AgentDomainEvent::SessionUnavailable {
            reason: format!("Codex ended unexpectedly ({reason}){suffix}"),
        });
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
                method,
                item_id,
                thread_id,
            } => {
                eprintln!("agent: a {method} message from Codex was larger than 16 MiB; the item is not shown");
                // What is held may no longer be what Codex will do; a request naming it is declined.
                match (item_id, thread_id) {
                    (Some(id), Some(thread)) => st.calls.forget(&thread, &id),
                    (Some(id), None) => st.calls.forget_everywhere(&id),
                    (None, _) => {}
                }
            }
            Skipped::RequestAnswered { method } => {
                eprintln!("agent: a {method} request from Codex was larger than 16 MiB and was answered at once");
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
            N::AgentMessageDelta(d) => self.on_delta(st, d, ContentKind::Text),
            N::ReasoningSummaryDelta(d) | N::ReasoningTextDelta(d) => self.on_delta(st, d, ContentKind::Thinking),
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
            N::AutoReviewStarted(_) => {}
            N::AutoReviewCompleted(review) => self.on_auto_review(st, review),
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
            remember_early_turn(st, thread_id, UNREAD_TURN.to_string());
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
            let turn_id = format!("eitri-unread-turn-{thread_id}-{stamp}");
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
    fn tab_turn_id(st: &State) -> Option<String> {
        st.turn.as_ref().and_then(|t| t.id.clone())
    }

    /// A turn started on a thread of the family. The tab's own turn is begun by the `turn/start` answer and
    /// this repeats it; a subagent's turn joins the tab's turn, or opens one when the tab is idle (a child
    /// keeps working after its parent's turn ended).
    fn on_turn_started(&self, st: &mut State, n: TurnNotification) {
        if !in_family(st, &n.thread_id) {
            note_foreign(st, &n.thread_id);
            remember_early_turn(st, &n.thread_id, n.turn.id);
            return;
        }
        self.join_family_turn(st, &n.thread_id, n.turn.id);
    }

    /// `turn` of member `thread` is running: it joins the tab's turn, or opens one when the tab is idle. An
    /// interrupt the user asked for that this turn missed goes out to it now.
    fn join_family_turn(&self, st: &mut State, thread: &str, turn: String) {
        let edge = match st.family.as_mut() {
            Some(family) => family.turn_started(thread, &turn),
            None => return,
        };
        if edge == FamilyEdge::Opened && st.turn.is_none() {
            st.turn = Some(ActiveTurn::new(Some(turn.clone())));
            st.events.push_back(AgentDomainEvent::TurnStarted { turn_id: turn });
        } else if st.turn.as_ref().is_some_and(|t| t.interrupting) {
            self.send_interrupts(st);
        }
    }

    /// A thread that just joined the family may already be running a turn; it is followed from now on.
    fn joined(&self, st: &mut State, thread: &str) {
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
            ThreadItem::SubAgentActivity(activity) => {
                let member = st.family.as_mut().is_some_and(|f| f.announce(thread_id, activity));
                if member {
                    self.joined(st, &activity.agent_thread_id);
                } else if activity.kind == SubAgentKind::Started {
                    note_foreign(st, thread_id);
                }
                true
            }
            ThreadItem::CollabAgentToolCall(call) => {
                let joined = st
                    .family
                    .as_mut()
                    .map(|f| f.announce_receivers(thread_id, &call.receiver_thread_ids))
                    .unwrap_or_default();
                for thread in joined {
                    self.joined(st, &thread);
                }
                false
            }
            _ => false,
        }
    }

    fn on_item_started(&self, st: &mut State, n: ItemNotification) {
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
        if self.announced(st, &n.thread_id, &n.item) {
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
                if emitted.is_none() {
                    st.events.push_back(AgentDomainEvent::AssistantMessageBoundary {
                        turn_id: turn_id.clone(),
                    });
                }
                let already = emitted.unwrap_or(0);
                // What streamed is a prefix of the final text; only the rest is new.
                if m.text.len() > already && m.text.is_char_boundary(already) {
                    st.events.push_back(AgentDomainEvent::ContentDelta {
                        turn_id,
                        kind: ContentKind::Text,
                        text: m.text[already..].to_string(),
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

    fn on_delta(&self, st: &mut State, delta: ItemDelta, kind: ContentKind) {
        if delta.delta.is_empty() || !self.running_turn(st, &delta.thread_id, &delta.turn_id) {
            return;
        }
        let Some(turn_id) = Self::tab_turn_id(st) else { return };
        if !is_tab(st, &delta.thread_id) {
            return;
        }
        *st.items.entry(delta.item_id).or_insert(0) += delta.delta.len();
        st.events.push_back(AgentDomainEvent::ContentDelta {
            turn_id,
            kind,
            text: delta.delta,
        });
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

    /// The family has no turn running: the tab's turn ends, with the outcome of the tab's own turn.
    fn close_family_turn(&self, st: &mut State) {
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
        st.items.clear();
        st.open_tools.clear();
        st.answered_rows.clear();
        st.calls.clear();
        if let Some(family) = st.family.as_mut() {
            family.reset_turns();
        }
        Self::drop_cards(st, PermissionOutcome::CancelledByInterrupt);
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
/// are. The tab's own calls are left as they are.
fn labelled(st: &State, thread_id: &str, mut input: Value) -> Value {
    if let (Some(label), Some(object)) = (
        st.family.as_ref().and_then(|f| f.label(thread_id)),
        input.as_object_mut(),
    ) {
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

/// Remembers `turn` as the running turn of `thread`, not (yet) in the family, so a child announced after
/// its turn began is followed from that turn. At most `EARLY_TURNS_KEPT` threads are kept.
fn remember_early_turn(st: &mut State, thread: &str, turn: String) {
    if st.early_turns.len() < EARLY_TURNS_KEPT || st.early_turns.contains_key(thread) {
        st.early_turns.insert(thread.to_string(), turn);
    }
}

/// Logs a method Eitri does not handle, once per name, without its payload.
fn note_unknown(st: &mut State, what: &str, method: &str) {
    if st.seen_unknown.len() < UNKNOWN_METHODS_KEPT && st.seen_unknown.insert(method.to_string()) {
        eprintln!("agent: Codex sent {what} Eitri does not handle: {method}");
    }
}

impl AgentProvider for CodexProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            interrupt: true,
            ..Default::default()
        }
    }

    fn info(&self) -> ProviderInfo {
        let untested = self.version > HIGHEST_TESTED_CODEX;
        ProviderInfo {
            build_description: Some(format!("Codex {}", version_text(self.version))),
            startup_diagnostics: if untested {
                vec![format!("untested Codex {}", version_text(self.version))]
            } else {
                Vec::new()
            },
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
        let (permission_id, card) = st.cards.remove(at);
        self.respond(card.request_id.clone(), card.form.reply(answer));
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
            Self::drop_cards(&mut st, PermissionOutcome::CancelledBySessionClose);
            st.closed = true;
            st.events.push_back(AgentDomainEvent::SessionClosed {
                reason: "closed_by_host".to_string(),
            });
            st.thread_id.clone()
        };
        let dead = lock(&self.state).dead;
        if let (Some(thread_id), false) = (thread_id, dead) {
            if let Ok(params) = to_json(&ThreadUnsubscribeParams { thread_id }) {
                if let Ok(pending) = self.request(method::THREAD_UNSUBSCRIBE, params) {
                    // Whatever the answer, the pipe closes next; a failure only means Codex was already going.
                    let _ = pending.wait(Instant::now() + UNSUBSCRIBE_WAIT);
                }
            }
        }
        let link = lock(&self.link).take();
        if let Some(link) = link {
            link.shutdown(SIDECAR_EXIT_GRACE);
        }
        Ok(())
    }

    fn pump(&self) -> Vec<AgentDomainEvent> {
        let mut st = lock(&self.state);
        if !st.closed {
            self.drain(&mut st);
        }
        st.events.drain(..).collect()
    }
}

impl CodexProvider {
    /// Fails the session: Codex did something other than what Eitri asked for. The host fails the tab on the
    /// event and answers nothing on its own afterwards; here every card is declined and no turn starts.
    fn trip(&self, st: &mut State, mismatch: PolicyMismatch) {
        if st.blocked.is_some() {
            return;
        }
        eprintln!("agent: {mismatch}");
        st.blocked = Some("Codex is not running under the policy Eitri asked for");
        Self::drop_cards(st, PermissionOutcome::ProviderFailed);
        st.events.push_back(mismatch.into_event());
    }

    /// `thread/start`, or `thread/resume` and the history after it.
    fn open_session(&self, req: ExternalStart, resume: Option<&str>) -> Result<String, ProviderError> {
        let config = session_config(&req.plan)
            .map_err(|e| provider_error(ProviderErrorCode::InvalidConfiguration, e.to_string()))?;
        let reviewer = req.policy.reviewer;
        let (verb, pending) = match resume {
            None => (
                "the session start",
                self.request(
                    method::THREAD_START,
                    to_json(&ThreadStartParams::new(req.cwd.clone(), reviewer, config))?,
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
                return Ok(self.resume_refused(&req, resume.unwrap_or_default(), message));
            }
            Err(e) => return Err(self.failure(e, verb)),
        };
        let started: ThreadStartResponse = from_json(answer, verb)?;

        let thread_id = started.thread.id.clone();
        let model = started.model.clone().unwrap_or_default();
        let mismatch = tripwire::check_echo(&expected_echo(reviewer), &started.echo())
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
            if let Some(mismatch) = mismatch {
                self.trip(&mut st, mismatch);
            }
        }
        if resume.is_some() {
            self.read_history(&thread_id);
        }
        Ok(thread_id)
    }

    /// The resume Codex refused, as events: the session opens so there is something to say the verdict on.
    fn resume_refused(&self, req: &ExternalStart, requested: &str, message: String) -> String {
        eprintln!("agent: Codex refused to resume a session: {message}");
        let mut st = lock(&self.state);
        st.history_loading = false;
        st.blocked = Some("Codex would not resume this conversation");
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
            status: ResumeStatus::Rejected,
            attached_provider_session_id: None,
            forked: false,
            detail: Some(message),
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

    fn send_turn_with(&self, req: SendTurnRequest, policy: TurnPolicy) -> Result<String, ProviderError> {
        let thread_id = {
            let mut st = lock(&self.state);
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
            if st.turn.is_some() {
                return Err(provider_error(
                    ProviderErrorCode::TurnAlreadyActive,
                    "a turn is already running",
                ));
            }
            st.turn = Some(ActiveTurn::new(None));
            req.session_id.clone()
        };
        let abandon = |this: &Self| lock(&this.state).turn = None;

        let params = match to_json(&TurnStartParams::new(thread_id.clone(), req.text, policy.reviewer)) {
            Ok(params) => params,
            Err(e) => {
                abandon(self);
                return Err(e);
            }
        };
        let pending = match self.request(method::TURN_START, params) {
            Ok(pending) => pending,
            Err(e) => {
                abandon(self);
                return Err(e);
            }
        };
        let answer = match self.wait(pending, TURN_START_TIMEOUT, "the turn start") {
            Ok(answer) => answer,
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
            self.send_interrupts(&mut st);
        }
        Ok(turn_id)
    }
}
