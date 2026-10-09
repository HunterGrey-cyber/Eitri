//! `AcpProvider`: one ACP agent child per tab, driven over [`JsonRpcLink`] with the `jsonrpc` member on.
//!
//! The provider is an [`AgentProvider`] for the ingestion thread, which calls [`pump`](AgentProvider::pump)
//! and gets events, and an [`ExternalProvider`] for the host, which starts a session and sends turns. The
//! plain Claude-shaped calls refuse.
//!
//! **Nothing blocks the thread that pumps.** Only `connect` and `start` wait for an answer, each for a
//! bounded time, and nothing waits while a lock is held. A turn is different from Codex's: `session/prompt`
//! answers when the turn *ends*, so the request is kept as the turn's own pending answer and `pump` takes it
//! once it has arrived. Everything the agent said before that answer is folded first.
//!
//! **Every request from the agent is answered.** A permission request becomes a card and waits for the
//! user (or bypass) through `resolve_permission`, or is answered `cancelled` when nobody can decide it any
//! more; any other request gets "method not found", because an unanswered one would hang the agent. The
//! options of a card stay inside the provider: only `allow_once` and `reject_once` are ever shown or
//! selected, so no answer can grant more than this one call.
//!
//! **What Eitri sends is small.** `initialize` offers no file system and no terminal; a session gets no
//! tool servers; `authenticate` is never sent (an agent's login method may open a browser); and the only
//! mode change is to the one mode the agent was registered with. If the agent reports any other mode, the
//! session is stopped.

use super::history::History;
use super::translate::{self, Fold};
use super::wire::{self, method, ChunkKind, InitializeResponse, OptionKind, PermissionRequest, Update};
use crate::external::{ContextGauge, ExternalProvider, ExternalStart, TurnPolicy};
use crate::jsonrpc_link::{resolve_program, Framing, Incoming, JsonRpcLink, LinkError, LinkSpec, Pending, Skipped};
use crate::projection::{AgentDomainEvent, PermissionOutcome, ResumeStatus, TurnEndDetail, TurnOutcome};
use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, PermissionDecision,
    ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest,
    ResumeSessionRequest, SendTurnRequest,
};
use crate::SIDECAR_EXIT_GRACE;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long the handshake may take. An agent started through a package runner may fetch itself first.
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a session start or a mode change may take.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long loading a saved session may take: the replay of a long conversation comes before the answer.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a close waits for the agent to confirm before the pipe is closed anyway.
const CLOSE_WAIT: Duration = Duration::from_secs(3);
/// How often a wait during the start looks for the answer and for what the agent said meanwhile.
const POLL: Duration = Duration::from_millis(5);
/// Messages read per pump, so one call cannot run away with a flood.
const PUMP_BATCH: usize = 4096;
/// Unknown names remembered so each is logged once.
const UNKNOWN_KEPT: usize = 64;
/// The longest stderr tail put in a notice.
const NOTICE_TAIL_CHARS: usize = 400;

/// JSON-RPC's "method not found".
const METHOD_NOT_FOUND: i64 = -32601;

/// What an ACP agent is, as the user registered it.
#[derive(Debug, Clone)]
pub struct AcpAgentSpec {
    /// The registered name (`^[A-Za-z0-9][A-Za-z0-9._-]*$`); the provider is called `acp:<name>`.
    pub name: String,
    /// The program and its arguments. A bare program name is looked up on `PATH`, never relative to the
    /// project root.
    pub argv: Vec<OsString>,
    /// The mode id set at the start of every session, in both Eitri modes. Eitri never sets another. With
    /// none, the agent's own default mode stays and Eitri does not know what it runs without asking.
    pub auto_mode: Option<String>,
}

/// The provider name for an agent: leaked once per distinct name, since [`ExternalProvider::name`] hands
/// out a `'static` string and the set of registered agents is small and fixed by the user's `init.lua`.
fn interned_name(agent: &str) -> &'static str {
    static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let full = format!("acp:{agent}");
    let mut names = lock(&NAMES);
    if let Some(known) = names.iter().find(|n| **n == full) {
        return known;
    }
    let leaked: &'static str = Box::leak(full.into_boxed_str());
    names.push(leaked);
    leaked
}

/// A card waiting for its answer.
struct WaitingCard {
    /// The agent's id of the request, which the answer goes back under.
    request_id: Value,
    /// The agent's first `allow_once` option, if it offered one. Never shown.
    allow: Option<String>,
    /// The agent's first `reject_once` option, if it offered one. Never shown.
    reject: Option<String>,
}

/// What belongs to one running turn.
struct ActiveTurn {
    id: String,
    /// The `session/prompt` answer, until it arrives.
    pending: Option<Pending>,
    /// The answer, taken and waiting for everything the agent said before it to be folded.
    response: Option<Result<Value, LinkError>>,
    fold: Fold,
    /// The user pressed interrupt: the agent has been told to cancel and nothing new is carded.
    interrupting: bool,
}

#[derive(Default)]
struct State {
    session_id: Option<String>,
    model: Option<String>,
    events: VecDeque<AgentDomainEvent>,
    turn: Option<ActiveTurn>,
    turn_seq: u64,
    /// Cards waiting for an answer, by permission id, in the order they were drawn.
    cards: Vec<(String, WaitingCard)>,
    next_card: u64,
    seen_unknown: HashSet<String>,
    context: Option<ContextGauge>,
    /// Why the session may not take turns any more: the agent reported a mode other than the one asked
    /// for, or would not load the saved session.
    blocked: Option<&'static str>,
    /// A saved session is being loaded: what arrives meanwhile is left in the link, so the replay is read
    /// in one place.
    loading: bool,
    /// The loaded session's history, until the host takes it. Never pumped.
    history: Vec<AgentDomainEvent>,
    closed: bool,
    dead: bool,
}

/// What a wait during the start collects: the mode the agent last reported, and the replay.
#[derive(Default)]
struct Boot {
    mode: Option<String>,
    history: Option<History>,
}

pub struct AcpProvider {
    name: &'static str,
    agent: String,
    auto_mode: Option<String>,
    link: Mutex<Option<JsonRpcLink>>,
    state: Mutex<State>,
    pid: u32,
    load_session: bool,
    close_session: bool,
    /// What the agent calls itself and its version, for the panel.
    label: Option<String>,
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

/// A mode as a sentence for the tab.
fn mode_words(mode: &str) -> String {
    format!("mode {mode}")
}

fn note_unknown(st: &mut State, what: &str, name: &str) {
    if st.seen_unknown.len() < UNKNOWN_KEPT && st.seen_unknown.insert(format!("{what} {name}")) {
        eprintln!("agent: an ACP agent sent {what} Eitri does not use ({name}); dropped");
    }
}

impl AcpProvider {
    /// Starts the agent on stdio in `cwd` and runs the handshake. Every failure is a sentence for the tab,
    /// and no child outlives it.
    pub fn connect(spec: AcpAgentSpec, cwd: PathBuf) -> Result<AcpProvider, ProviderError> {
        let Some(first) = spec.argv.first() else {
            return Err(provider_error(
                ProviderErrorCode::InvalidConfiguration,
                format!("the agent {} has no command to run", spec.name),
            ));
        };
        let program =
            resolve_program(first, std::env::var_os("PATH").as_deref(), |p| p.is_file()).ok_or_else(|| {
                provider_error(
                    ProviderErrorCode::ProviderUnavailable,
                    format!("{}: {} not found on PATH", spec.name, first.to_string_lossy()),
                )
            })?;
        let link = JsonRpcLink::spawn(LinkSpec {
            program,
            args: spec.argv[1..].to_vec(),
            cwd,
            framing: Framing::WithJsonrpcMember,
            env: Vec::new(),
            env_remove: Vec::new(),
        })
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => provider_error(
                ProviderErrorCode::ProviderUnavailable,
                format!("{} could not be started: {e}", spec.name),
            ),
            _ => ProviderError::Transport(format!("{} could not be started: {e}", spec.name)),
        })?;
        let init = match Self::handshake(&spec.name, &link) {
            Ok(init) => init,
            Err(e) => {
                link.shutdown(SIDECAR_EXIT_GRACE);
                return Err(e);
            }
        };
        let pid = link.pid();
        Ok(AcpProvider {
            name: interned_name(&spec.name),
            agent: spec.name,
            auto_mode: spec.auto_mode,
            link: Mutex::new(Some(link)),
            state: Mutex::new(State::default()),
            pid,
            load_session: init.load_session,
            close_session: init.close_session,
            label: init.agent_label,
        })
    }

    fn handshake(agent: &str, link: &JsonRpcLink) -> Result<InitializeResponse, ProviderError> {
        let params = serde_json::to_value(wire::InitializeParams::eitri(env!("CARGO_PKG_VERSION")))
            .map_err(|e| ProviderError::Transport(format!("a request could not be written: {e}")))?;
        let answer = link
            .request(method::INITIALIZE, params)
            .wait(Instant::now() + INIT_TIMEOUT)
            .map_err(|e| link_error(agent, link, "the handshake", e))?;
        let init = InitializeResponse::parse(&answer);
        if init.protocol_version != Some(wire::PROTOCOL_VERSION) {
            let said = match init.protocol_version {
                Some(v) => format!("version {v}"),
                None => "no version".to_string(),
            };
            return Err(provider_error(
                ProviderErrorCode::IncompatibleProtocol,
                format!(
                    "{agent} answered with ACP {said}; Eitri speaks version {}",
                    wire::PROTOCOL_VERSION
                ),
            ));
        }
        Ok(init)
    }

    /// The child's pid, captured at spawn.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The mode set at the start of every session, if the agent was registered with one.
    pub fn auto_mode(&self) -> Option<&str> {
        self.auto_mode.as_deref()
    }

    /// How full the agent last said its context window is. Never counted as tokens.
    pub fn context_gauge(&self) -> Option<ContextGauge> {
        lock(&self.state).context
    }

    /// Whether the agent takes `session/load`, which is what makes a saved session resumable.
    pub fn can_load_session(&self) -> bool {
        self.load_session
    }

    /// Sends a request on the link, or says the link is gone.
    fn request(&self, method: &str, params: Value) -> Result<Pending, ProviderError> {
        match lock(&self.link).as_ref() {
            Some(link) => Ok(link.request(method, params)),
            None => Err(provider_error(
                ProviderErrorCode::ProviderUnavailable,
                format!("{} is no longer running", self.agent),
            )),
        }
    }

    /// A failed call as the error the host shows.
    fn failure(&self, e: LinkError, what: &str) -> ProviderError {
        match lock(&self.link).as_ref() {
            Some(link) => link_error(&self.agent, link, what, e),
            None => ProviderError::Transport(format!("{} ended before it answered {what}", self.agent)),
        }
    }

    fn require_open(&self, st: &State) -> Result<(), ProviderError> {
        if st.closed || st.dead {
            return Err(provider_error(
                ProviderErrorCode::ProviderUnavailable,
                format!("{} is no longer running", self.agent),
            ));
        }
        Ok(())
    }

    fn respond(&self, id: Value, result: Value) {
        if let Some(link) = lock(&self.link).as_ref() {
            link.respond(id, result);
        }
    }

    fn require_session(&self, st: &State, session_id: &str) -> Result<(), ProviderError> {
        if st.session_id.as_deref() != Some(session_id) {
            return Err(provider_error(
                ProviderErrorCode::SessionNotFound,
                format!("no such {} session", self.agent),
            ));
        }
        Ok(())
    }

    // ------------------------------------------------------------ the start

    /// Waits for `pending` while serving what the agent says meanwhile (a replay, a mode report, a request
    /// it makes), so its queue never fills behind the wait.
    fn serve(&self, mut pending: Pending, within: Duration, boot: &mut Boot) -> Result<Value, LinkError> {
        let deadline = Instant::now() + within;
        loop {
            // The answer first: the reader files every message before it ahead of the answer, so reading
            // the queue afterwards leaves nothing of the replay behind.
            let answer = pending.try_take();
            loop {
                let next = lock(&self.link).as_ref().and_then(JsonRpcLink::try_recv);
                let Some(message) = next else { break };
                self.fold_boot(message, boot);
            }
            if let Some(answer) = answer {
                return answer;
            }
            if Instant::now() >= deadline {
                return Err(LinkError::Timeout);
            }
            std::thread::sleep(POLL);
        }
    }

    fn fold_boot(&self, message: Incoming, boot: &mut Boot) {
        match message {
            Incoming::Notification { method, params } if method == method::SESSION_UPDATE => {
                let Some((_, update)) = wire::parse_update(&params) else {
                    return;
                };
                match &update {
                    Update::Mode(mode) => boot.mode = Some(mode.clone()),
                    Update::Options { mode: Some(mode) } => boot.mode = Some(mode.clone()),
                    _ => {}
                }
                if let Some(history) = boot.history.as_mut() {
                    history.apply(&update);
                }
            }
            Incoming::Request { id, method, .. } => {
                // Nobody can decide a permission before the session exists.
                if method == method::REQUEST_PERMISSION {
                    self.respond(id, wire::cancelled_outcome());
                } else if let Some(link) = lock(&self.link).as_ref() {
                    link.respond_error(id, METHOD_NOT_FOUND, "method not found");
                }
            }
            _ => {}
        }
    }

    fn open_new(&self, req: ExternalStart) -> Result<String, ProviderError> {
        let mut boot = Boot::default();
        let pending = self.request(method::SESSION_NEW, wire::session_new_params(&req.cwd))?;
        let answer = self
            .serve(pending, START_TIMEOUT, &mut boot)
            .map_err(|e| self.failure(e, "the session start"))?;
        let setup = wire::SessionSetup::parse(&answer);
        let Some(session_id) = setup.session_id.clone().filter(|id| !id.is_empty()) else {
            return Err(provider_error(
                ProviderErrorCode::ProviderProtocolError,
                format!("{} started a session without naming it", self.agent),
            ));
        };
        self.finish_open(req.cwd, session_id, setup, boot, None)
    }

    fn open_saved(&self, req: ExternalStart, saved: String) -> Result<String, ProviderError> {
        if !self.load_session {
            return Ok(self.resume_refused(&req, &saved, format!("{} cannot load a saved session", self.agent)));
        }
        let mut boot = Boot {
            mode: None,
            history: Some(History::default()),
        };
        let pending = self.request(method::SESSION_LOAD, wire::session_load_params(&saved, &req.cwd))?;
        let answer = match self.serve(pending, LOAD_TIMEOUT, &mut boot) {
            Ok(answer) => answer,
            // The agent's own words are what the user needs, shown as the verdict on the resume.
            Err(LinkError::Rpc { message, .. }) => return Ok(self.resume_refused(&req, &saved, message)),
            Err(e) => return Err(self.failure(e, "loading the session")),
        };
        let setup = wire::SessionSetup::parse(&answer);
        self.finish_open(req.cwd, saved.clone(), setup, boot, Some(saved))
    }

    /// The session exists: sets and checks its mode, then opens it in the projection. `resumed` is the id a
    /// load asked for.
    fn finish_open(
        &self,
        cwd: String,
        session_id: String,
        setup: wire::SessionSetup,
        mut boot: Boot,
        resumed: Option<String>,
    ) -> Result<String, ProviderError> {
        let mut mismatch = None;
        if let Some(want) = &self.auto_mode {
            // A report that came after the answer is the newer one.
            let reported = boot.mode.take().or_else(|| setup.current_mode.clone());
            if reported.as_deref() == Some(want.as_str()) {
                // Already in the mode asked for.
            } else {
                let pending = self.request(method::SESSION_SET_MODE, wire::set_mode_params(&session_id, want))?;
                match self.serve(pending, START_TIMEOUT, &mut boot) {
                    // codex-acp sends no `current_mode_update` after `session/set_mode`, so an agent saying
                    // nothing has taken the change; a report that arrived says otherwise or agrees.
                    Ok(_) => {
                        let now = boot.mode.take().unwrap_or_else(|| want.clone());
                        if now != *want {
                            mismatch = Some((mode_words(want), mode_words(&now)));
                        }
                    }
                    Err(LinkError::Rpc { message, .. }) => {
                        let was = reported.as_deref().map_or("no mode reported".to_string(), mode_words);
                        mismatch = Some((
                            mode_words(want),
                            format!("{was} (the agent refused to change it: {message})"),
                        ));
                    }
                    Err(e) => return Err(self.failure(e, "the mode change")),
                }
            }
        }
        let history = boot.history.take().map(History::finish).unwrap_or_default();
        let mut st = lock(&self.state);
        st.loading = false;
        st.session_id = Some(session_id.clone());
        st.model = setup.current_model.clone();
        st.history = history;
        st.events.push_back(AgentDomainEvent::SessionOpened {
            session_id: session_id.clone(),
            provider_session_id: session_id.clone(),
            model: setup.current_model.unwrap_or_default(),
            cwd,
        });
        if let Some(requested) = resumed {
            st.events.push_back(AgentDomainEvent::ResumeOutcome {
                requested_provider_session_id: requested,
                status: ResumeStatus::Attached,
                attached_provider_session_id: Some(session_id.clone()),
                forked: false,
                detail: None,
            });
        }
        if let Some((requested, reported)) = mismatch {
            self.trip(&mut st, requested, reported);
        }
        Ok(session_id)
    }

    /// The saved session the agent would not load, as events: the session opens so there is something to
    /// say the verdict on.
    fn resume_refused(&self, req: &ExternalStart, requested: &str, message: String) -> String {
        eprintln!("agent: {} would not load a saved session: {message}", self.agent);
        let mut st = lock(&self.state);
        st.loading = false;
        st.blocked = Some("the agent would not load this conversation");
        st.session_id = Some(requested.to_string());
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

    /// Fails the session: the agent runs in a mode other than the one Eitri asked for. The host fails the
    /// tab on the event; here every card is cancelled and no turn starts.
    fn trip(&self, st: &mut State, requested: String, reported: String) {
        if st.blocked.is_some() {
            return;
        }
        eprintln!("agent: asked for {requested}, {} reported {reported}", self.agent);
        st.blocked = Some("the agent is not running in the mode Eitri asked for");
        self.drop_cards(st, PermissionOutcome::ProviderFailed, true);
        st.events
            .push_back(AgentDomainEvent::PolicyMismatch { requested, reported });
    }

    /// The agent reported its mode: it must be the one asked for. With no mode registered, nothing is asked
    /// and nothing is checked.
    fn check_mode(&self, st: &mut State, reported: &str) {
        let Some(want) = &self.auto_mode else { return };
        if reported != want {
            self.trip(st, mode_words(want), mode_words(reported));
        }
    }

    // ------------------------------------------------------------ interrupt and cards

    /// Tells the agent to cancel the running turn, then answers every open request `cancelled`. The turn
    /// ends when the agent answers its prompt.
    fn interrupt_running(&self, st: &mut State) {
        let Some(session) = st.session_id.clone() else { return };
        let already = match st.turn.as_mut() {
            Some(turn) => std::mem::replace(&mut turn.interrupting, true),
            None => false,
        };
        if !already && st.turn.is_some() {
            if let Some(link) = lock(&self.link).as_ref() {
                link.notify(method::SESSION_CANCEL, wire::session_only_params(&session));
            }
        }
        self.drop_cards(st, PermissionOutcome::CancelledByInterrupt, true);
    }

    /// Every card still waiting goes with `outcome`; the agent is told `cancelled` unless it is gone.
    fn drop_cards(&self, st: &mut State, outcome: PermissionOutcome, answer: bool) {
        for (permission_id, card) in std::mem::take(&mut st.cards) {
            if answer {
                self.respond(card.request_id, wire::cancelled_outcome());
            }
            st.events
                .push_back(AgentDomainEvent::PermissionResolved { permission_id, outcome });
        }
    }

    // ------------------------------------------------------------ the stream

    /// Reads what the link has, folding it into events. Returns whether the queue was read to its end.
    fn drain(&self, st: &mut State) -> bool {
        if st.loading {
            return true;
        }
        for _ in 0..PUMP_BATCH {
            let next = lock(&self.link).as_ref().and_then(JsonRpcLink::try_recv);
            let Some(message) = next else { return true };
            self.fold(st, message);
        }
        false
    }

    fn fold(&self, st: &mut State, message: Incoming) {
        match message {
            Incoming::Notification { method, params } => {
                if method == method::SESSION_UPDATE {
                    self.on_update(st, params);
                } else {
                    // Only the name is logged: `_auth/*` notifications carry the account.
                    note_unknown(st, "a notification", &method);
                }
            }
            Incoming::Request { id, method, params } => {
                if method == method::REQUEST_PERMISSION {
                    self.on_permission(st, id, params);
                } else {
                    note_unknown(st, "a request", &method);
                    if let Some(link) = lock(&self.link).as_ref() {
                        link.respond_error(id, METHOD_NOT_FOUND, "method not found");
                    }
                }
            }
            Incoming::Skipped(skipped) => on_skipped(&self.agent, skipped),
            Incoming::Closed { reason } => self.on_closed(st, &reason),
        }
    }

    fn on_closed(&self, st: &mut State, reason: &str) {
        if st.closed || st.dead {
            return;
        }
        st.dead = true;
        // Nothing can answer what waited any more.
        self.drop_cards(st, PermissionOutcome::ProviderFailed, false);
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
            reason: format!("{} ended unexpectedly ({reason}){suffix}", self.agent),
        });
    }

    fn on_update(&self, st: &mut State, params: Value) {
        let Some((session, update)) = wire::parse_update(&params) else {
            note_unknown(st, "an update", "without a session or a kind");
            return;
        };
        if st.session_id.as_deref() != Some(session.as_str()) {
            note_unknown(st, "an update", "for another session");
            return;
        }
        match update {
            Update::Context { used, size } => st.context = Some(ContextGauge { used, size }),
            Update::Mode(mode) => self.check_mode(st, &mode),
            Update::Options { mode: Some(mode) } => self.check_mode(st, &mode),
            Update::Options { mode: None } | Update::Ignored(_) => {}
            Update::Unknown(kind) => note_unknown(st, "an update", &kind),
            // The live stream carries no user messages: only a replay does.
            Update::Chunk {
                kind: ChunkKind::User, ..
            } => {}
            Update::Chunk { text: None, .. } => note_unknown(st, "a message chunk", "that is not text"),
            update @ (Update::Chunk { .. } | Update::ToolCall(_) | Update::ToolCallUpdate(_)) => {
                let Some(turn) = st.turn.as_mut() else {
                    note_unknown(st, "an update", "outside a turn");
                    return;
                };
                let turn_id = turn.id.clone();
                let events = &mut st.events;
                turn.fold.apply(&turn_id, &update, &mut |event| events.push_back(event));
            }
        }
    }

    fn on_permission(&self, st: &mut State, id: Value, params: Value) {
        let Some(request) = PermissionRequest::parse(&params) else {
            eprintln!(
                "agent: {} sent a permission request Eitri cannot read; cancelled",
                self.agent
            );
            self.respond(id, wire::cancelled_outcome());
            return;
        };
        let asking = st.turn.as_ref().is_some_and(|t| !t.interrupting);
        if st.session_id.as_deref() != Some(request.session_id.as_str()) || st.blocked.is_some() || !asking {
            eprintln!(
                "agent: {} asked for a decision Eitri cannot take now ({}); cancelled",
                self.agent, request.tool_call.id
            );
            self.respond(id, wire::cancelled_outcome());
            return;
        }
        // A card for a call whose row is not open yet gets one, or the card and its answer would sit on
        // nothing. What the card shows is what the row and the earlier updates know of the call.
        let tool = match st.turn.as_mut() {
            Some(turn) => {
                let turn_id = turn.id.clone();
                let events = &mut st.events;
                turn.fold
                    .ensure_started(&turn_id, &request.tool_call, &mut |event| events.push_back(event))
            }
            None => request.tool_call.clone(),
        };
        // An input or a diff the card cannot show whole cannot be approved: the card would not show all of
        // what an approval lets through. It can still be refused, and the interrupt ends it.
        let unshowable = translate::input_is_cut(&tool);
        let first = |kind: OptionKind| request.options.iter().find(|o| o.kind == kind).map(|o| o.id.clone());
        let allow = first(OptionKind::AllowOnce).filter(|_| !unshowable);
        let reject = first(OptionKind::RejectOnce);
        // Only what can be answered is shown, by the agent's own names, without the ids.
        let shown: Vec<Value> = request
            .options
            .iter()
            .filter_map(|o| match o.kind {
                OptionKind::AllowOnce if !unshowable => Some(json!({ "kind": "allow_once", "name": o.name })),
                OptionKind::RejectOnce => Some(json!({ "kind": "reject_once", "name": o.name })),
                _ => None,
            })
            .collect();
        let mut input = translate::tool_input(&tool);
        if let Some(object) = input.as_object_mut() {
            object.insert("options".to_string(), Value::Array(shown));
            if unshowable {
                object.insert("inputCut".to_string(), Value::Bool(true));
            }
        }
        let tool = &tool;
        st.next_card += 1;
        let permission_id = format!("acp-{}", st.next_card);
        st.cards.push((
            permission_id.clone(),
            WaitingCard {
                request_id: id,
                allow,
                reject,
            },
        ));
        st.events.push_back(AgentDomainEvent::PermissionRequested {
            permission_id,
            tool_use_id: Some(tool.id.clone()),
            tool_name: translate::tool_name(tool.kind.as_deref()),
            input,
            provider_prompt: None,
        });
    }

    /// Ends the turn with the agent's answer to its prompt.
    fn finish_turn(&self, st: &mut State) {
        let Some(turn) = st.turn.take() else { return };
        let (outcome, detail, stop_reason, usage) = match turn.response {
            Some(Ok(value)) => {
                let response = wire::PromptResponse::parse(&value);
                let (outcome, detail) = translate::stop_ending(response.stop_reason.as_deref());
                let usage = response.usage.map(|u| translate::usage_of(&u, st.model.as_deref()));
                (outcome, detail, response.stop_reason, usage)
            }
            Some(Err(LinkError::Rpc { message, .. })) => {
                let (outcome, detail) = translate::failure_ending("agent_error", &message);
                (outcome, detail, None, None)
            }
            Some(Err(LinkError::TooLarge)) => {
                let (outcome, detail) =
                    translate::failure_ending("message_too_large", "the agent's answer was larger than 16 MiB");
                (outcome, detail, None, None)
            }
            Some(Err(other)) => {
                let (outcome, detail) = translate::failure_ending("agent_unreachable", &other.to_string());
                (outcome, detail, None, None)
            }
            None => (TurnOutcome::Failed, TurnEndDetail::default(), None, None),
        };
        // What was still asked at the end can no longer happen.
        self.drop_cards(st, PermissionOutcome::CancelledByInterrupt, true);
        st.events.push_back(AgentDomainEvent::TurnCompleted {
            turn_id: turn.id,
            outcome,
            result_text: turn.fold.last_message,
            stop_reason,
            usage,
            detail,
        });
    }
}

/// A failed call on the link as the error the host shows.
fn link_error(agent: &str, link: &JsonRpcLink, what: &str, e: LinkError) -> ProviderError {
    match e {
        LinkError::Timeout => ProviderError::Timeout,
        LinkError::Closed => {
            let tail = notice_tail(&link.stderr_tail());
            let suffix = if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            };
            ProviderError::Transport(format!("{agent} ended before it answered {what}{suffix}"))
        }
        LinkError::Busy => ProviderError::Transport(format!("{agent} is not reading its input")),
        LinkError::TooLarge => provider_error(
            ProviderErrorCode::ProviderProtocolError,
            format!("{agent}'s answer to {what} was larger than 16 MiB"),
        ),
        LinkError::Rpc { message, .. } => provider_error(
            ProviderErrorCode::ProviderProtocolError,
            format!("{agent} refused {what}: {message}"),
        ),
    }
}

fn on_skipped(agent: &str, skipped: Skipped) {
    match skipped {
        Skipped::TurnTooLarge { method, .. } | Skipped::ItemTooLarge { method, .. } => {
            eprintln!("agent: a {method} message from {agent} was larger than 16 MiB; not shown");
        }
        Skipped::RequestAnswered { method } => {
            eprintln!("agent: a {method} request from {agent} was larger than 16 MiB and was answered at once");
        }
        Skipped::ResponseFailed { id } => {
            eprintln!("agent: {agent}'s answer to request {id} was larger than 16 MiB");
        }
    }
}

impl AgentProvider for AcpProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            interrupt: true,
            ..Default::default()
        }
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            build_description: Some(match &self.label {
                Some(label) => format!("{} ({label})", self.agent),
                None => self.agent.clone(),
            }),
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
        self.require_session(&st, &request.session_id)?;
        self.require_open(&st)?;
        if st.turn.is_none() {
            return Err(provider_error(ProviderErrorCode::NoActiveTurn, "no turn is running"));
        }
        self.interrupt_running(&mut st);
        Ok(())
    }

    fn resolve_permission(&self, request: ResolvePermissionRequest) -> Result<(), ProviderError> {
        let mut st = lock(&self.state);
        self.require_session(&st, &request.session_id)?;
        self.require_open(&st)?;
        let Some(at) = st.cards.iter().position(|(id, _)| *id == request.permission_id) else {
            return Err(provider_error(
                ProviderErrorCode::PermissionNotFound,
                "no permission request is waiting",
            ));
        };
        let (approve, wanted) = match request.decision {
            PermissionDecision::Allow => (true, st.cards[at].1.allow.clone()),
            // ACP takes no reason with a refusal; the option alone is sent.
            PermissionDecision::Deny { .. } => (false, st.cards[at].1.reject.clone()),
            // An ACP agent has no handling of its own to hand a request back to.
            PermissionDecision::Defer => return Err(ProviderError::UnsupportedCapability("defer")),
        };
        // A request that offers no option of the kind cannot be answered that way, and nothing else is
        // selected in its place: the card stays, and an interrupt can still end it.
        let Some(option) = wanted else {
            let what = if approve { "approved" } else { "refused" };
            return Err(provider_error(
                ProviderErrorCode::PermissionNotFound,
                format!("{} offers no way for this request to be {what}", self.agent),
            ));
        };
        let (permission_id, card) = st.cards.remove(at);
        self.respond(card.request_id, wire::selected_outcome(&option));
        let outcome = if approve {
            PermissionOutcome::Allowed
        } else {
            PermissionOutcome::Denied
        };
        st.events
            .push_back(AgentDomainEvent::PermissionResolved { permission_id, outcome });
        Ok(())
    }

    fn close_session(&self, request: CloseSessionRequest) -> Result<(), ProviderError> {
        let (session, dead) = {
            let mut st = lock(&self.state);
            if st.closed {
                return Ok(());
            }
            if st.session_id.is_some() {
                self.require_session(&st, &request.session_id)?;
            }
            if !st.dead {
                // A running turn is cancelled first, so the agent has not left a command running.
                self.interrupt_running(&mut st);
            }
            let alive = !st.dead;
            self.drop_cards(&mut st, PermissionOutcome::CancelledBySessionClose, alive);
            st.closed = true;
            st.events.push_back(AgentDomainEvent::SessionClosed {
                reason: "closed_by_host".to_string(),
            });
            (st.session_id.clone(), st.dead)
        };
        if let (Some(session), false, true) = (session, dead, self.close_session) {
            if let Ok(pending) = self.request(method::SESSION_CLOSE, wire::session_only_params(&session)) {
                // Whatever the answer, the pipe closes next; a failure only means the agent was already going.
                let _ = pending.wait(Instant::now() + CLOSE_WAIT);
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
        if !st.closed && !st.loading {
            // Take the prompt's answer before reading the queue: everything the agent said ahead of it is
            // in the queue by then, and the turn ends only once the queue has been read to its end.
            if let Some(turn) = st.turn.as_mut() {
                if turn.response.is_none() {
                    if let Some(answer) = turn.pending.as_mut().and_then(Pending::try_take) {
                        // A closed link is reported by its own message, not as this turn's answer.
                        if !matches!(answer, Err(LinkError::Closed)) {
                            turn.response = Some(answer);
                            turn.pending = None;
                        }
                    }
                }
            }
            let read_to_end = self.drain(&mut st);
            if read_to_end && st.turn.as_ref().is_some_and(|t| t.response.is_some()) {
                self.finish_turn(&mut st);
            }
        }
        st.events.drain(..).collect()
    }
}

impl ExternalProvider for AcpProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    /// A saved session is continued with `session/load`, where the agent advertises it.
    fn resumes(&self) -> bool {
        self.load_session
    }

    fn context_gauge(&self) -> Option<ContextGauge> {
        AcpProvider::context_gauge(self)
    }

    /// An ACP agent loads its own configuration, so the trust plan and the policy of the tab's mode are not
    /// applied here: what stands before the start is the host's gate, and the mode is the registered one.
    fn start(&self, req: ExternalStart) -> Result<String, ProviderError> {
        let saved = req.resume.clone();
        {
            let mut st = lock(&self.state);
            self.require_open(&st)?;
            if st.session_id.is_some() {
                return Err(provider_error(
                    ProviderErrorCode::InvalidConfiguration,
                    format!("this {} connection already has a session", self.agent),
                ));
            }
            // Anything the agent says from here on waits until the session is open (and its replay, if
            // there is one, is in): a pump that read a mode report meanwhile would drop it as one for no
            // session, and the start's own check of the mode would never see it.
            st.loading = true;
        }
        let result = match saved {
            None => self.open_new(req),
            Some(saved) => self.open_saved(req, saved),
        };
        if result.is_err() {
            lock(&self.state).loading = false;
        }
        result
    }

    fn take_history(&self) -> Vec<AgentDomainEvent> {
        std::mem::take(&mut lock(&self.state).history)
    }

    fn send_turn_with(&self, req: SendTurnRequest, _policy: TurnPolicy) -> Result<String, ProviderError> {
        let mut st = lock(&self.state);
        self.require_open(&st)?;
        if let Some(why) = st.blocked {
            return Err(provider_error(ProviderErrorCode::ProviderUnavailable, why));
        }
        self.require_session(&st, &req.session_id)?;
        if st.turn.is_some() {
            return Err(provider_error(
                ProviderErrorCode::TurnAlreadyActive,
                "a turn is already running",
            ));
        }
        let pending = self.request(
            method::SESSION_PROMPT,
            wire::session_prompt_params(&req.session_id, &req.text),
        )?;
        // ACP names no turn: the agent's answer to the prompt ends it, so the id is Eitri's own.
        st.turn_seq += 1;
        let turn_id = format!("acp-turn-{}", st.turn_seq);
        st.turn = Some(ActiveTurn {
            id: turn_id.clone(),
            pending: Some(pending),
            response: None,
            fold: Fold::default(),
            interrupting: false,
        });
        st.events.push_back(AgentDomainEvent::TurnStarted {
            turn_id: turn_id.clone(),
        });
        Ok(turn_id)
    }
}
