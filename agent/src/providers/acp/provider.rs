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
use super::wire::{
    self, method, ChunkKind, InitializeResponse, ModeReport, OptionKind, PermissionRequest, ToolCallFields, Update,
};
use crate::external::{ContextGauge, ExternalProvider, ExternalStart, TurnPolicy, ANSWERED_BY_EITRI};
use crate::external_link::{self, link_error, lock, provider_error, ClosingNotice};
use crate::jsonrpc_link::{
    resolve_program, Framing, Incoming, JsonRpcLink, LinkError, LinkHandoff, LinkSpec, OverCapReply, Pending, Skipped,
    REQUEST_TOO_LARGE,
};
use crate::projection::{AgentDomainEvent, PermissionOutcome, ResumeStatus, SlashCommand, TurnEndDetail, TurnOutcome};
use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, PermissionDecision,
    ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest,
    ResumeSessionRequest, SendTurnRequest,
};
use crate::provider_move::{forget_unclaimed, handed_ids, MovedAnswer, START_WAIT};
use crate::MoveRefusedWhy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Mutex;
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

/// JSON-RPC's "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
/// Why an adopted chat refuses to move on before its own move is committed.
const NOT_COMMITTED: &str = "this chat's move here is not committed yet";
/// The prefix of the id of a row Eitri adds for a request it answered whose call it could not name.
const ANSWERED_ROW_PREFIX: &str = "eitri-answered-";

/// Why a session takes no more turns: the agent would not load the saved session.
const BLOCKED_LOAD: &str = "the agent would not load this conversation";
/// Why a session takes no more turns: the agent reported a mode other than the one asked for.
const BLOCKED_MODE: &str = "the agent is not running in the mode Eitri asked for";
/// Every reason a session can be blocked for, so a moved session's reason is read back as the same one.
const BLOCKED_REASONS: [&str; 2] = [BLOCKED_LOAD, BLOCKED_MODE];

/// How the link answers a request from the agent too large to read: a permission request `cancelled`, the
/// one ACP answer that needs none of the request's options, and anything else "invalid request".
pub fn over_cap_reply(name: &str) -> OverCapReply {
    if name == method::REQUEST_PERMISSION {
        OverCapReply::Result(wire::cancelled_outcome())
    } else {
        OverCapReply::Error {
            code: REQUEST_TOO_LARGE,
            message: "too large",
        }
    }
}

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
    /// The mode id that stands for `auto_mode` where the project's root is no boundary, as the registration
    /// says: ACP has no sandbox to ask for, and Eitri cannot tell what any mode of an agent means.
    pub read_only_mode: Option<String>,
}

impl AcpAgentSpec {
    /// The spec a session at this root starts from. Only the mode changes, before the provider sees it, so what
    /// sets the mode and holds the agent to it is the same either way.
    pub fn at_root(mut self, root_is_a_boundary: bool) -> AcpAgentSpec {
        if !root_is_a_boundary {
            if let Some(mode) = self.read_only_mode.take() {
                self.auto_mode = Some(mode);
            }
        }
        self
    }
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
#[derive(Debug, Serialize, Deserialize)]
struct WaitingCard {
    /// The agent's id of the request, which the answer goes back under.
    request_id: Value,
    /// The agent's first `allow_once` option, if it offered one. Never shown.
    allow: Option<String>,
    /// The agent's first `reject_once` option, if it offered one. Never shown.
    reject: Option<String>,
    /// The agent offered an `allow_once` option, but the card could not show the whole call, so it is not
    /// used: a refused approval then says so instead of blaming the agent.
    allow_held_back: bool,
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
    /// Rows Eitri added for requests it answered, for their ids.
    next_answered_row: u64,
    seen_unknown: HashSet<String>,
    context: Option<ContextGauge>,
    /// The commands the agent last listed (`available_commands_update`); empty before any list.
    commands: Vec<SlashCommand>,
    /// Why the session may not take turns any more: the agent reported a mode other than the one asked
    /// for, or would not load the saved session.
    blocked: Option<&'static str>,
    /// A saved session is being loaded: what arrives meanwhile is left in the link, so the replay is read
    /// in one place.
    loading: bool,
    /// The session being opened is a saved one, whose replay can take long: set and cleared with
    /// `loading`.
    replaying: bool,
    /// The loaded session's history, until the host takes it. Never pumped.
    history: Vec<AgentDomainEvent>,
    /// The child ended on its own and its notice waits for the last of its stderr: the reason the link
    /// gave, and when to give up waiting. Finished by `pump`, so nothing sleeps while holding this state.
    closing_notice: Option<ClosingNotice>,
    closed: bool,
    dead: bool,
    /// A chat taken up from another process waits for its move's commit, and pumps nothing until then:
    /// the move can still be given back, and the source then reports what was carried itself.
    awaiting_commit: bool,
}

/// What a wait during the start collects: the mode the agent last reported, and the replay.
#[derive(Default)]
struct Boot {
    mode: Option<ModeReport>,
    /// The latest command list the agent sent before the session was open.
    commands: Option<Vec<SlashCommand>>,
    history: Option<History>,
    /// The link's end, read while the start was still being served. The link says it only once, so a
    /// start that went on to succeed hands it to the open session, which then ends at once.
    closed: Option<String>,
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

/// Keeps the agent's command list and tells the host, but only when it differs from the one held: an
/// agent that repeats its list, or says an empty one before it has any, changes nothing.
fn note_commands(st: &mut State, commands: Vec<SlashCommand>) {
    if st.commands == commands {
        return;
    }
    st.commands = commands.clone();
    st.events.push_back(AgentDomainEvent::CommandsAvailable { commands });
}

/// A mode as a sentence for the tab.
fn mode_words(mode: &str) -> String {
    format!("mode {mode}")
}

fn mode_report_words(report: &ModeReport) -> String {
    match report {
        ModeReport::Known(mode) => mode_words(mode),
        ModeReport::Unreadable => "a mode report Eitri could not read".to_string(),
    }
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
            over_cap: over_cap_reply,
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
                external_link::stop_child(link);
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

    /// A card's answer, sent or refused as a whole: `Err` when it was not queued, so the card can stay.
    fn respond_to_card(&self, id: Value, result: Value) -> Result<(), ProviderError> {
        let sent = match lock(&self.link).as_ref() {
            Some(link) => link.try_respond(id, result),
            None => Err(LinkError::Closed),
        };
        sent.map_err(|e| {
            provider_error(
                ProviderErrorCode::ProviderUnavailable,
                format!(
                    "the answer did not reach {}: {e}; the card is still waiting",
                    self.agent
                ),
            )
        })
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
                let next = external_link::try_recv(&self.link);
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
                    Update::Mode(mode) | Update::Options { mode: Some(mode) } => {
                        boot.mode = Some(ModeReport::Known(mode.clone()));
                    }
                    Update::UnreadableMode => boot.mode = Some(ModeReport::Unreadable),
                    Update::Commands(commands) => boot.commands = Some(commands.clone()),
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
            Incoming::Closed { reason } => boot.closed = Some(reason),
            _ => {}
        }
    }

    fn open_new(&self, req: ExternalStart) -> Result<String, ProviderError> {
        let mut boot = Boot::default();
        let pending = self.request(
            method::SESSION_NEW,
            wire::session_new_params(&req.cwd, req.editor_server.as_ref()),
        )?;
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
            commands: None,
            history: Some(History::default()),
            closed: None,
        };
        let pending = self.request(
            method::SESSION_LOAD,
            wire::session_load_params(&saved, &req.cwd, req.editor_server.as_ref()),
        )?;
        let answer = match self.serve(pending, LOAD_TIMEOUT, &mut boot) {
            Ok(answer) => answer,
            // The agent's own words are what the user needs, shown as the verdict on the resume.
            Err(LinkError::Rpc { message, .. }) => {
                let opened = self.resume_refused(&req, &saved, message);
                if let Some(reason) = boot.closed.take() {
                    self.on_closed(&mut lock(&self.state), &reason);
                }
                return Ok(opened);
            }
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
            if matches!(&reported, Some(ModeReport::Known(mode)) if mode == want) {
                // Already in the mode asked for.
            } else {
                let pending = self.request(method::SESSION_SET_MODE, wire::set_mode_params(&session_id, want))?;
                match self.serve(pending, START_TIMEOUT, &mut boot) {
                    // codex-acp sends no `current_mode_update` after `session/set_mode`, so an agent saying
                    // nothing has taken the change; a report that arrived says otherwise or agrees.
                    Ok(_) => {
                        let now = boot.mode.take().unwrap_or_else(|| ModeReport::Known(want.clone()));
                        if !matches!(&now, ModeReport::Known(mode) if mode == want) {
                            mismatch = Some((mode_words(want), mode_report_words(&now)));
                        }
                    }
                    Err(LinkError::Rpc { message, .. }) => {
                        let was = reported
                            .as_ref()
                            .map_or("no mode reported".to_string(), mode_report_words);
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
        st.replaying = false;
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
        if let Some(commands) = boot.commands.take() {
            note_commands(&mut st, commands);
        }
        if let Some((requested, reported)) = mismatch {
            self.trip(&mut st, requested, reported);
        }
        if let Some(reason) = boot.closed.take() {
            self.on_closed(&mut st, &reason);
        }
        Ok(session_id)
    }

    /// The saved session the agent would not load, as events: the session opens so there is something to
    /// say the verdict on.
    fn resume_refused(&self, req: &ExternalStart, requested: &str, message: String) -> String {
        // Its words go on the page, not in the log: they can quote the session.
        eprintln!(
            "agent: {} would not load a saved session ({} bytes of reason)",
            self.agent,
            message.len()
        );
        let mut st = lock(&self.state);
        st.loading = false;
        st.replaying = false;
        st.blocked = Some(BLOCKED_LOAD);
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
        st.blocked = Some(BLOCKED_MODE);
        self.drop_cards(st, PermissionOutcome::ProviderFailed, true);
        st.events
            .push_back(AgentDomainEvent::PolicyMismatch { requested, reported });
    }

    /// The agent reported its mode: it must be the one asked for. With no mode registered, nothing is asked
    /// and nothing is checked.
    fn check_mode(&self, st: &mut State, reported: &ModeReport) {
        let Some(want) = &self.auto_mode else { return };
        if !matches!(reported, ModeReport::Known(mode) if mode == want) {
            self.trip(st, mode_words(want), mode_report_words(reported));
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
            let next = external_link::try_recv(&self.link);
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
        self.drop_cards(st, PermissionOutcome::ProviderFailed, false);
        // The end of the output stream can be read a moment before the last of stderr; the notice waits a
        // short while for it so it says why the child went. `pump` finishes it: waiting here would hold this
        // state, which the UI thread's calls (a card's answer, Esc, a send, the gauge) take.
        st.closing_notice = Some(ClosingNotice::after(reason, Instant::now(), &self.link));
        self.finish_closing_notice(st);
    }

    /// Sends the child's end once its stderr says why, or once the wait is over without it.
    fn finish_closing_notice(&self, st: &mut State) {
        if let Some(reason) = external_link::finish_closing_notice(&mut st.closing_notice, &self.agent, &self.link) {
            st.events.push_back(AgentDomainEvent::SessionUnavailable { reason });
        }
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
            Update::Commands(commands) => note_commands(st, commands),
            Update::Mode(mode) | Update::Options { mode: Some(mode) } => {
                self.check_mode(st, &ModeReport::Known(mode));
            }
            Update::UnreadableMode => self.check_mode(st, &ModeReport::Unreadable),
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
        let offered_allow = first(OptionKind::AllowOnce);
        let allow_held_back = unshowable && offered_allow.is_some();
        let allow = offered_allow.filter(|_| !unshowable);
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
                allow_held_back,
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

    fn on_skipped(&self, st: &mut State, skipped: Skipped) {
        let agent = &self.agent;
        match skipped {
            Skipped::TurnTooLarge { method: name, .. } | Skipped::ItemTooLarge { method: name, .. } => {
                eprintln!("agent: a {name} message from {agent} was larger than 16 MiB; not shown");
            }
            Skipped::RequestAnswered {
                method: name,
                session_id,
                item_id,
                ..
            } => {
                eprintln!("agent: a {name} request from {agent} was larger than 16 MiB and was answered at once");
                if name == method::REQUEST_PERMISSION {
                    self.note_over_cap(st, session_id.as_deref(), item_id);
                }
            }
            Skipped::ResponseFailed { id } => {
                eprintln!("agent: {agent}'s answer to request {id} was larger than 16 MiB");
            }
        }
    }

    /// Says on the call's row that Eitri cancelled a permission request too large to read, which no card
    /// could show. The call is the one the request's first bytes named, with this session; a request that
    /// named no call, or no session, within them gets a row Eitri adds, since a note on no row would never be
    /// seen. A request about another
    /// session, or one made with no turn running, is only logged.
    fn note_over_cap(&self, st: &mut State, session_id: Option<&str>, call_id: Option<String>) {
        if session_id.is_some_and(|session| st.session_id.as_deref() != Some(session)) {
            return;
        }
        // A call is named only for a request that named this session too: one whose session id came after
        // the bytes read could be another session's call of the same id.
        let call_id = call_id.filter(|_| session_id.is_some());
        // Field by field, so the turn, the queue and the counter are borrowed apart.
        let events = &mut st.events;
        let next_answered_row = &mut st.next_answered_row;
        let Some(turn) = st.turn.as_mut() else { return };
        let turn_id = turn.id.clone();
        let note = format!(
            "{} asked for a decision in a request larger than 16 MiB, which Eitri cannot show; Eitri cancelled it",
            self.agent
        );
        let denied = |row: String, tool_name: String| AgentDomainEvent::PermissionDenied {
            tool_use_id: Some(row),
            tool_name,
            reason_type: Some(ANSWERED_BY_EITRI.to_string()),
            reason: Some(note.clone()),
        };
        match call_id.filter(|id| !id.is_empty()) {
            Some(id) => {
                let named = ToolCallFields {
                    id,
                    title: None,
                    kind: None,
                    status: None,
                    raw_input: None,
                    raw_output: None,
                    locations: None,
                    content: Vec::new(),
                    content_given: false,
                };
                let call = turn
                    .fold
                    .ensure_started(&turn_id, &named, &mut |event| events.push_back(event));
                events.push_back(denied(call.id.clone(), translate::tool_name(call.kind.as_deref())));
            }
            None => {
                *next_answered_row += 1;
                // The turn's id keeps the row's id unique in a projection that outlives this connection.
                let row = format!("{ANSWERED_ROW_PREFIX}{turn_id}-{next_answered_row}");
                let tool_name = translate::tool_name(None);
                events.push_back(AgentDomainEvent::ToolCallStarted {
                    turn_id: turn_id.clone(),
                    tool_use_id: row.clone(),
                    name: tool_name.clone(),
                    input: json!({}),
                });
                events.push_back(denied(row.clone(), tool_name));
                events.push_back(AgentDomainEvent::ToolCallCompleted {
                    turn_id,
                    tool_use_id: row,
                    content: Value::String(String::new()),
                    is_error: false,
                });
            }
        }
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

// ------------------------------------------------------------ moving the chat to another process

/// An ACP tab's provider state as data, taken when its chat moves to another process: every field of the
/// provider's state is here, rebuilt there, or named where it is taken as the reason it does not move.
/// The connection's own state crosses beside it as the link's [`LinkHandoff`].
#[derive(Debug, Serialize, Deserialize)]
pub struct AcpProviderHandoff {
    agent: String,
    auto_mode: Option<String>,
    load_session: bool,
    close_session: bool,
    label: Option<String>,
    session_id: Option<String>,
    model: Option<String>,
    /// Folded already and not pumped yet when the chat froze.
    events: VecDeque<AgentDomainEvent>,
    turn: Option<MovedTurn>,
    turn_seq: u64,
    cards: Vec<(String, WaitingCard)>,
    next_card: u64,
    next_answered_row: u64,
    seen_unknown: HashSet<String>,
    /// The gauge's used and size.
    context: Option<(u64, u64)>,
    commands: Vec<SlashCommand>,
    blocked: Option<String>,
    history: Vec<AgentDomainEvent>,
}

/// [`ActiveTurn`] as data: the prompt's answer by its request id, or the answer itself if it had come.
#[derive(Debug, Serialize, Deserialize)]
struct MovedTurn {
    id: String,
    pending: Option<MovedAnswer>,
    response: Option<Result<Value, LinkError>>,
    fold: Fold,
    interrupting: bool,
}

fn invalid(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

impl ActiveTurn {
    fn into_moved(self) -> MovedTurn {
        let ActiveTurn {
            id,
            mut pending,
            response,
            fold,
            interrupting,
        } = self;
        MovedTurn {
            id,
            pending: pending.as_mut().map(MovedAnswer::of),
            response,
            fold,
            interrupting,
        }
    }

    fn from_moved(moved: MovedTurn, link: &JsonRpcLink) -> io::Result<ActiveTurn> {
        let MovedTurn {
            id,
            pending,
            response,
            fold,
            interrupting,
        } = moved;
        Ok(ActiveTurn {
            id,
            pending: pending.map(|answer| answer.rebind(link)).transpose()?,
            response,
            fold,
            interrupting,
        })
    }
}

impl AcpProvider {
    /// Whether a running turn moves with the chat. An ACP agent keeps its turn when its stdio changes
    /// hands, so a chat moves mid-turn as well as between turns.
    pub fn live_move(&self) -> bool {
        true
    }

    /// Why the provider cannot freeze now, if it cannot.
    fn move_blocker(&self, st: &State) -> Option<MoveRefusedWhy> {
        if st.awaiting_commit {
            Some(MoveRefusedWhy::Link(NOT_COMMITTED.to_string()))
        } else if st.closed || st.dead {
            Some(MoveRefusedWhy::Ended)
        } else if st.loading && st.replaying {
            // A replay can run far past the start's short wait, so it is refused at once.
            Some(MoveRefusedWhy::HistoryLoading)
        } else if st.loading || st.session_id.is_none() {
            Some(MoveRefusedWhy::Starting)
        } else if st.turn.is_some() && !self.live_move() {
            Some(MoveRefusedWhy::TurnRunning)
        } else {
            None
        }
    }

    /// Stops the provider where it stands so that another process can take the chat up with
    /// [`adopt`](Self::adopt): the link stops (its reader at once, its writer once what was queued is
    /// written), and the provider's state comes out as data, the prompt's answer by its request id or, if it
    /// had come, as the answer itself. Nothing is written to the agent and nothing is ended: the child, its
    /// stdin, stdout and stderr go with the returned descriptors. From here on this provider is closed and
    /// pumps nothing.
    ///
    /// Refused, with the provider left as it was, while a new session is still opening 2 s on, at once
    /// while a saved session's replay is read, once the session has ended, before an adopted chat's own
    /// move is committed, or when the link cannot stop (a caller waiting on an answer that does not come
    /// within 2 s, a write the agent does not take).
    ///
    /// The provider stays locked while the link stops, so that nothing is pumped from a half-stopped
    /// link: its other calls wait for up to those 2 s, and a caller on a UI thread freezes from another.
    pub fn freeze_for_move(&self) -> Result<(AcpProviderHandoff, LinkHandoff, [OwnedFd; 3]), MoveRefusedWhy> {
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
        Ok((self.handoff_of(state), link_handoff, fds))
    }

    fn handoff_of(&self, state: State) -> AcpProviderHandoff {
        let AcpProvider {
            // The interned form of `agent`, interned again where the chat is taken up.
            name: _,
            agent,
            auto_mode,
            // Stopped and handed over as the link's own record.
            link: _,
            // Taken apart below.
            state: _,
            // The link's record carries the child's pid.
            pid: _,
            load_session,
            close_session,
            label,
        } = self;
        let State {
            session_id,
            model,
            events,
            turn,
            turn_seq,
            cards,
            next_card,
            next_answered_row,
            seen_unknown,
            context,
            commands,
            blocked,
            // A freeze is refused while the session opens, so both are false.
            loading: _,
            replaying: _,
            history,
            // A freeze is refused once the session is closed or its child gone, so both are false.
            closed: _,
            dead: _,
            // Set only once the child is gone, which refuses a freeze, so it is `None`.
            closing_notice: _,
            // A freeze is refused until an adopted chat's move is committed, so it is false.
            awaiting_commit: _,
        } = state;
        AcpProviderHandoff {
            agent: agent.clone(),
            auto_mode: auto_mode.clone(),
            load_session: *load_session,
            close_session: *close_session,
            label: label.clone(),
            session_id,
            model,
            events,
            turn: turn.map(ActiveTurn::into_moved),
            turn_seq,
            cards,
            next_card,
            next_answered_row,
            seen_unknown,
            context: context.map(|gauge| (gauge.used, gauge.size)),
            commands,
            blocked: blocked.map(str::to_string),
            history,
        }
    }

    /// Takes up a chat another process froze with [`freeze_for_move`](Self::freeze_for_move). The link is
    /// adopted parked and the prompt's answer is bound again by its request id, so nothing is read from or
    /// written to the agent until [`commit`](Self::commit); dropping the provider before then only closes
    /// this process's copies of the descriptors.
    pub fn adopt(handoff: AcpProviderHandoff, link: LinkHandoff, fds: [OwnedFd; 3]) -> io::Result<AcpProvider> {
        let pid = link.pid;
        let handed = handed_ids(&link);
        let link = JsonRpcLink::adopt(link, fds, over_cap_reply)?;
        Self::rebuilt(handoff, link, pid, &handed)
    }

    /// Starts the adopted link once the move is committed: what was carried is read first, then the
    /// agent's stdout, and the answers given meanwhile are written. Nothing to do for a running link.
    /// Until then [`pump`](AgentProvider::pump) reports nothing.
    pub fn commit(&self) -> io::Result<()> {
        let mut st = lock(&self.state);
        let started = match lock(&self.link).as_mut() {
            Some(link) => link.start_after_commit(),
            None => Ok(()),
        };
        // Pumping resumes even when the link's threads could not start: the tab then learns that the
        // session ended, instead of waiting for good.
        st.awaiting_commit = false;
        started
    }

    /// The source's way back when a move is given up before its commit: this provider takes its own
    /// frozen state up again and reads on at once, as it would have without the move.
    pub fn resume_after_move(
        &self,
        handoff: AcpProviderHandoff,
        link: LinkHandoff,
        fds: [OwnedFd; 3],
    ) -> io::Result<()> {
        let pid = link.pid;
        let handed = handed_ids(&link);
        let link = JsonRpcLink::resume_after_move(link, fds, over_cap_reply)?;
        let AcpProvider {
            link,
            state,
            // The source's own, unchanged by the move.
            name: _,
            agent: _,
            auto_mode: _,
            pid: _,
            load_session: _,
            close_session: _,
            label: _,
        } = Self::rebuilt(handoff, link, pid, &handed)?;
        let mut st = lock(&self.state);
        *st = state.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
        // The source's link reads on at once, so there is no commit to wait for.
        st.awaiting_commit = false;
        *lock(&self.link) = link.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(())
    }

    fn rebuilt(handoff: AcpProviderHandoff, link: JsonRpcLink, pid: u32, handed: &[u64]) -> io::Result<AcpProvider> {
        let AcpProviderHandoff {
            agent,
            auto_mode,
            load_session,
            close_session,
            label,
            session_id,
            model,
            events,
            turn,
            turn_seq,
            cards,
            next_card,
            next_answered_row,
            seen_unknown,
            context,
            commands,
            blocked,
            history,
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
        let turn = turn.map(|moved| ActiveTurn::from_moved(moved, &link)).transpose()?;
        forget_unclaimed(&link, handed);
        let state = State {
            session_id,
            model,
            events,
            turn,
            turn_seq,
            cards,
            next_card,
            next_answered_row,
            seen_unknown,
            context: context.map(|(used, size)| ContextGauge { used, size }),
            commands,
            blocked,
            loading: false,
            replaying: false,
            history,
            closing_notice: None,
            closed: false,
            dead: false,
            awaiting_commit: true,
        };
        Ok(AcpProvider {
            name: interned_name(&agent),
            agent,
            auto_mode,
            link: Mutex::new(Some(link)),
            state: Mutex::new(state),
            pid,
            load_session,
            close_session,
            label,
        })
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
            if approve && st.cards[at].1.allow_held_back {
                return Err(provider_error(
                    ProviderErrorCode::PermissionNotFound,
                    format!(
                        "Eitri held back {}'s allow-once option: this card cannot show the whole call, so it cannot be approved",
                        self.agent
                    ),
                ));
            }
            let what = if approve { "approved" } else { "refused" };
            return Err(provider_error(
                ProviderErrorCode::PermissionNotFound,
                format!("{} offers no way for this request to be {what}", self.agent),
            ));
        };
        // Sent before the card goes: an answer the link did not take leaves the card waiting, to be
        // answered again, rather than an agent waiting for an answer no card can give.
        self.respond_to_card(st.cards[at].1.request_id.clone(), wire::selected_outcome(&option))?;
        let (permission_id, _card) = st.cards.remove(at);
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
        external_link::stop_linked_child(&self.link);
        Ok(())
    }

    fn pump(&self) -> Vec<AgentDomainEvent> {
        let mut st = lock(&self.state);
        if st.awaiting_commit {
            return Vec::new();
        }
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
        // Also once the host has closed the session: a child that ended first still says why.
        self.finish_closing_notice(&mut st);
        st.events.drain(..).collect()
    }
    #[cfg(target_os = "linux")]
    fn mover(&self) -> Option<&dyn crate::chat_mover::ChatMover> {
        Some(self)
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
            st.replaying = saved.is_some();
        }
        let result = match saved {
            None => self.open_new(req),
            Some(saved) => self.open_saved(req, saved),
        };
        if result.is_err() {
            let mut st = lock(&self.state);
            st.loading = false;
            st.replaying = false;
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
