//! The provider-neutral `AgentProvider` trait a concrete backend implements.
//! `agent::session::AgentSession` (the legacy Claude-CLI backend) does NOT implement this trait in
//! this phase -- Phase 3's job is proving `ClaudeSidecarProvider` works standalone, not
//! retrofitting the legacy backend onto a shared interface it was never designed for (that's Phase
//! 5's "double backend A/B" job, once both sides are genuinely comparable side by side).
//!
//! Every method here is a synchronous, bounded-time Rust function -- matching the existing
//! `AgentSession` API shape a caller already knows -- even though a real implementation
//! (`ClaudeSidecarProvider`) does real async gRPC I/O underneath via `RuntimeThread`.
//! Streaming output (assistant text, tool calls, permission requests, ...) is never
//! returned directly from these methods; it arrives via `pump()`, mirroring `AgentSession::pump()`.

use crate::setting_sources::ProjectTrust;
use crate::AgentDomainEvent;

/// How much of an assistant reply the caller wants while it is still being produced.
///
/// A real behavioral choice with a measured cost, not a style preference: `Partial` turns one turn's
/// handful of content events into hundreds, which fills the provider's replay buffer far faster. A
/// UI wants it; a batch consumer that only reads a turn's final text does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamingPreference {
    /// Assistant text arrives when a message completes.
    Complete,
    /// Assistant text arrives as it is produced. These are PRESENTATION deltas -- the provider's
    /// own streaming events -- and are explicitly not a documented token boundary.
    #[default]
    Partial,
}

/// A fresh session. There is no permission mode on it: every session a provider
/// creates is gated -- the sidecar's `INTERACTIVE` policy, the CLI in `default` -- and whether the
/// host answers `allow` on its own is decided above this API. The field was removed rather than
/// ignored: "a requested permission mode must never silently become a different one" would be
/// broken by a `Bypass` the wire then disregarded, and a field the wire honoured would be a way back
/// to `bypassPermissions`.
#[derive(Debug, Clone)]
pub struct CreateSessionRequest {
    pub cwd: String,
    pub streaming: StreamingPreference,
    /// Whether the project's own settings tiers load (`crate::setting_sources::for_session`).
    pub project: ProjectTrust,
    /// The tools this host answers itself. A provider sends them only when its peer offers the
    /// capability `client_tools` and the set names at least one tool.
    pub client_tools: Option<ClientToolSet>,
}

/// Continue an existing provider (Claude) session rather than starting a fresh one.
///
/// Used to carry a `permission_mode`, because a resumed session does not inherit the policy it
/// originally ran under. It still does not, and it no longer needs telling: since 2026-09-27 every
/// session, resumed or fresh, is created gated (see `CreateSessionRequest`).
#[derive(Debug, Clone)]
pub struct ResumeSessionRequest {
    /// Claude's own session UUID -- the one `SessionOpened.provider_session_id` reported.
    pub provider_session_id: String,
    pub cwd: String,
    pub streaming: StreamingPreference,
    /// Whether the project's own settings tiers load. Stated afresh for every resume: the
    /// conversation continues, but what the project may run is decided now, not when it began.
    pub project: ProjectTrust,
    /// As on [`CreateSessionRequest`]: stated for every resume, since each start of the CLI gets the
    /// list its request names and a resumed session inherits none.
    pub client_tools: Option<ClientToolSet>,
}

/// Tools the host declares on a session and answers itself, seen by the model as
/// `mcp__<server_name>__<tool>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientToolSet {
    pub server_name: String,
    /// The CLI's own limit on one call. Past it the call fails to the model, whatever the host does.
    pub timeout_ms: u32,
    pub tools: Vec<ClientToolDecl>,
}

/// One tool of a [`ClientToolSet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientToolDecl {
    /// Without the `mcp__<server>__` prefix.
    pub name: String,
    pub description: String,
    /// The tool's JSON Schema as JSON text.
    pub input_schema_json: String,
    /// In the model's tool list from the first request, instead of waiting for a tool search.
    pub always_load: bool,
}

/// The host's answer to one call of a declared tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientToolAnswer {
    pub session_id: String,
    pub call_id: String,
    /// The answer's text blocks, in order.
    pub text: Vec<String>,
    /// The tool failed: the model reads `text` as the error.
    pub is_error: bool,
}

/// One call still waiting for an answer, as a read of the open calls gives it: its ids and no
/// arguments, because that read serves a host answering calls it will not run. The arguments are on the
/// call's event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCallRef {
    pub call_id: String,
    pub tool_use_id: Option<String>,
    pub tool: String,
}

/// The calls open at one point of the session's event stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientToolCallState {
    /// The event sequence this reading is consistent with: a call at or below it and not in `open` has
    /// settled, and later events apply on top of it.
    pub through_sequence: u64,
    /// Oldest first.
    pub open: Vec<ClientCallRef>,
}

#[derive(Debug, Clone)]
pub struct SendTurnRequest {
    pub session_id: String,
    pub text: String,
}

/// A user message for the turn that is running ([`AgentProvider::steer_turn`]).
#[derive(Debug, Clone)]
pub struct SteerTurnRequest {
    pub session_id: String,
    /// The turn the page showed running when the user pressed Enter (the host refuses before this when its
    /// own turn is another). A provider refuses the message when another turn (or none) runs by the time it
    /// arrives, so it never lands in a turn the user did not see.
    pub expected_turn_id: String,
    /// The host's own id for this message, echoed back by `UserMessageDelivered` or
    /// `UserMessageDropped`.
    pub client_message_id: String,
    /// What goes to the agent: the user's words with the editor context composed in.
    pub text: String,
    /// What the user typed, which the transcript row shows once the agent has read the message.
    pub as_typed: String,
}

/// What a provider may say about a message besides its text ([`AgentProvider::send_turn_stamped`],
/// [`AgentProvider::steer_turn_stamped`]). A provider that cannot carry a part leaves it off; the
/// default methods leave all of it off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnStamp {
    /// The host's own id for the message, which the agent names back when it reads the message later
    /// than the send (`UserMessageDelivered`) or drops it (`UserMessageDropped`). Sent only by a
    /// provider whose agent can say so.
    pub client_message_id: Option<String>,
    /// The person typed it into the composer; text the host or a plugin composed is `false`. Sent
    /// only by a provider whose agent accepts the mark.
    pub typed_by_person: bool,
    /// What the person typed, which the conversation shows if the agent reads the message later
    /// than the send. `None`: the request's own text.
    pub as_typed: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InterruptTurnRequest {
    pub session_id: String,
}

/// What a human decided about one pending permission request.
///
/// Enumerates exactly what a provider can actually carry and nothing more. Verdandi's
/// `ResolvePermissionRequest` is `bool allow` + `string reason`, and the legacy backend's hook relay
/// is the same shape: there is no `AllowForSession`, no scoped grant, no allow-with-edits anywhere
/// on either path. Modelling one here would produce a control that silently degrades to a plain
/// allow -- the permission-policy equivalent of a resume that quietly starts a fresh session.
/// Widening this enum is a Verdandi protocol change first, a client change second.
///
/// Typed rather than `allow: bool` + `reason: Option<String>` because that pair could express
/// combinations no backend honors: an approval carrying a reason (dropped on the floor), or a denial
/// whose reason field is silently repurposed. The model only ever sees a reason when it is denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run the tool, this once. The only grant any current backend can express.
    Allow,
    /// Refuse the tool. `reason` is shown to the model, which is the entire point of denying rather
    /// than interrupting -- it is optional because the wire's own field is, not because it is
    /// unimportant.
    Deny { reason: Option<String> },
    /// Make no decision: the CLI's own permission handling decides the call. Only meaningful on a
    /// session whose CLI runs its own auto mode under the gate
    /// (`ProviderCapabilities::cli_auto_mode`), where that handling is the CLI's classifier; Verdandi
    /// carries it as `ResolvePermissionRequest.defer` (capability `permission_defer`). Never a
    /// human's answer: no card offers it. A backend that cannot carry it refuses it rather than
    /// sending something else in its place, and the legacy backend's hook relay cannot.
    Defer,
}

impl PermissionDecision {
    /// True only for `Allow`. A deferral is not an approval: whether the call runs is the CLI's
    /// decision, not this host's.
    pub fn allows(&self) -> bool {
        matches!(self, PermissionDecision::Allow)
    }

    /// The text the model is shown. Never `Some` for an approval or a deferral: there is nowhere
    /// for it to go.
    pub fn reason(&self) -> Option<&str> {
        match self {
            PermissionDecision::Allow | PermissionDecision::Defer => None,
            PermissionDecision::Deny { reason } => reason.as_deref(),
        }
    }

    pub fn defers(&self) -> bool {
        matches!(self, PermissionDecision::Defer)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvePermissionRequest {
    pub session_id: String,
    pub permission_id: String,
    pub decision: PermissionDecision,
}

/// One question's answer as the person gave it: the options chosen, by their index in the question
/// Claude asked, and the text typed in its "Type something" field. Never the answer's words: the
/// provider builds those from the question it kept, so nothing a page sends becomes the tool's input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuestionAnswer {
    /// The question's index in its set.
    pub question_index: u32,
    pub option_indexes: Vec<u32>,
    pub other_text: Option<String>,
    /// Taken only by a single-select question that offers previews.
    pub notes: Option<String>,
}

/// How the person turned a question set down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionDeclineKind {
    /// Escape: Claude is told the person declined, and the turn stops as it does in the terminal.
    Cancel,
    /// "Chat about this": Claude is told to ask in the chat what the person wants to clarify, with
    /// what was answered so far, and the turn goes on.
    Chat,
}

/// What the person did with one question set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionResolution {
    /// Submitted; a question left out was not answered.
    Answers(Vec<QuestionAnswer>),
    /// Declined. `so_far` is what was answered before a `Chat`; a `Cancel` carries none.
    Decline {
        kind: QuestionDeclineKind,
        so_far: Vec<QuestionAnswer>,
    },
}

/// The person's answer to one question set Claude asked ([`AgentProvider::resolve_interaction`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveInteractionRequest {
    pub session_id: String,
    pub interaction_id: String,
    pub resolution: InteractionResolution,
}

/// What became of an answer to a question set: the outcome the provider reported when it reported
/// one, or why it was not taken.
pub type InteractionReply = Result<Option<crate::QuestionOutcome>, ProviderError>;

/// An answer to a question set on its way to the provider ([`AgentProvider::resolve_interaction`]).
/// Sending never waits for the provider: the reply comes through this handle, read without blocking
/// ([`Self::try_reply`]) by whoever polls it, or waited for ([`Self::wait`]) by a caller that may
/// block. A provider may tie a guard to the handle that lasts until the handle is dropped, so the
/// session is not handed elsewhere while the reply is still unread.
pub struct InteractionSend {
    rx: std::sync::mpsc::Receiver<InteractionReply>,
    _guard: Option<Box<dyn Send>>,
}

impl std::fmt::Debug for InteractionSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractionSend").finish_non_exhaustive()
    }
}

impl InteractionSend {
    /// A send still on its way, and the sender its reply goes through. A sender dropped with no reply
    /// reads as a transport failure.
    pub fn channel() -> (std::sync::mpsc::Sender<InteractionReply>, Self) {
        let (tx, rx) = std::sync::mpsc::channel();
        (tx, InteractionSend { rx, _guard: None })
    }

    /// A send whose reply is already known.
    pub fn ready(reply: InteractionReply) -> Self {
        let (tx, send) = Self::channel();
        let _ = tx.send(reply);
        send
    }

    /// Keeps `guard` alive for as long as this handle.
    pub fn with_guard(mut self, guard: impl Send + 'static) -> Self {
        self._guard = Some(Box::new(guard));
        self
    }

    /// The reply once it came, `None` while the answer is still on its way. Read once: a later call
    /// after a reply was taken reads as a transport failure.
    pub fn try_reply(&self) -> Option<InteractionReply> {
        match self.rx.try_recv() {
            Ok(reply) => Some(reply),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(Self::stopped())),
        }
    }

    /// Waits for the reply up to `timeout`, for a caller that may block; past it, `Timeout`.
    pub fn wait(&self, timeout: std::time::Duration) -> InteractionReply {
        match self.rx.recv_timeout(timeout) {
            Ok(reply) => reply,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(ProviderError::Timeout),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Self::stopped()),
        }
    }

    fn stopped() -> ProviderError {
        ProviderError::Transport("the answer's send stopped before the provider replied".to_string())
    }
}

/// The question sets a session still waits on, as its provider holds them
/// ([`AgentProvider::get_interaction_state`]).
#[derive(Debug, Clone, PartialEq)]
pub struct InteractionState {
    /// The provider's event sequence the set is consistent with: events after it apply on top.
    pub through_sequence: u64,
    /// One `QuestionAsked` per question set still waiting, checked like a live one.
    pub pending: Vec<AgentDomainEvent>,
}

#[derive(Debug, Clone)]
pub struct CloseSessionRequest {
    pub session_id: String,
}

/// A task RPC completes on the provider runtime; callers poll this receiver without waiting.
pub type TaskReply<T> = std::sync::mpsc::Receiver<Result<T, ProviderError>>;

/// The last bytes of a task's output, decoded lossily rather than treated as a file path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOutput {
    pub output: String,
    pub total_bytes: u64,
    pub truncated: bool,
}

/// What a provider can actually do, as reported by the provider itself -- never inferred from a
/// provider's name, an enum member's existence, or an RPC being present in a generated stub.
///
/// This is a contract: a capability that is absent hides or disables the feature in the UI, and
/// no command is sent on the theory that the server will ignore it. That only works if every field
/// here has a real wire source, so this struct
/// stays deliberately small -- one bool per thing a caller genuinely branches on, each derivable
/// from a real `HandshakeResponse`. `Copy` on purpose: callers pass it around freely, and anything
/// that needs an owned list (advertised names, versions, diagnostics) belongs in `ProviderInfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderCapabilities {
    /// The provider can continue an existing provider session by id.
    pub resume: bool,
    /// The provider can branch an existing provider session into a new one.
    pub fork: bool,
    /// The provider can cancel an in-flight turn.
    pub interrupt: bool,
    /// The provider advertises a BYPASS permission mode: tools run without asking.
    ///
    /// A reported handshake fact only, never requested: Eitri's bypass is its own
    /// `allow` answer under the gate, which needs `interactive_permission_mode` and nothing else.
    pub bypass_permission_mode: bool,
    /// The provider advertises an INTERACTIVE permission mode: tool use raises a real
    /// `PermissionRequested` that a human answers.
    ///
    /// A separate capability from `bypass_permission_mode` because the two are separately
    /// advertised and separately absent, and because choosing between them is a decision about what
    /// the agent is allowed to do unsupervised. `verdandi_rules` is deliberately NOT modeled: it is
    /// confirmed to behave identically to `interactive` in the current sidecar, so offering it as a
    /// third choice would be a distinction without a difference.
    ///
    /// The only policy any session is created under, so a provider without it is refused
    /// at session creation (`ClaudeSidecarProvider::require_interactive`), loudly -- never mapped
    /// onto whatever the provider does offer.
    pub interactive_permission_mode: bool,
    /// Every session this provider creates asks the CLI to run its own `auto` mode under the gate
    /// (Verdandi `ClaudeHostPolicy.cli_permission_mode = AUTO`), and a gate request on it may be
    /// answered `PermissionDecision::Defer`, which leaves the call to the CLI's classifier.
    ///
    /// True only when the handshake advertised both `cli_auto_mode` and `permission_defer`: asking
    /// for auto without being able to defer would leave the classifier nothing to decide, and
    /// deferring without auto would hand the call to the CLI's ordinary ask. Without both the
    /// request is exactly what it was before either existed. The gate is installed either way; this
    /// changes only what a deferred call meets. False on the legacy backend.
    pub cli_auto_mode: bool,
    /// The provider can put a user message into the turn that is running, without interrupting it
    /// ([`AgentProvider::steer_turn`]); the agent reads it at its next step. Without it a message
    /// typed while a turn runs is held by the host until the turn ends.
    pub steer: bool,
    // Protocol 3.22: one flag per capability the sidecar advertises, each read from the handshake's
    // list alone. They report what the sidecar offers; what this client asks the sidecar for is decided
    // separately (`CLIENT_IMPLEMENTS_*` in the Claude provider), so a flag here never puts anything new
    // on the wire by itself. `serde(default)`: a value serialized before a flag existed reads as
    // "not offered".
    /// The sidecar opens a turn for work Claude Code starts by itself, with its origin and cause.
    #[serde(default)]
    pub cli_turns: bool,
    /// The sidecar can mark a message as typed by a person.
    #[serde(default)]
    pub human_origin: bool,
    /// The sidecar reports the CLI's background tasks (`BackgroundTasksChanged`, `TaskEvent`).
    #[serde(default)]
    pub background_tasks: bool,
    /// A workflow task's events carry its phases and agents.
    #[serde(default)]
    pub workflow_progress: bool,
    /// The sidecar can stop one background task (`StopTask`), and can keep an interrupt from
    /// stopping background work when the session asks for that.
    #[serde(default)]
    pub task_control: bool,
    /// The sidecar can read a background task's output (`GetTaskOutput`).
    #[serde(default)]
    pub task_output: bool,
    /// A subagent's text and tool calls carry the id of the call that started it.
    #[serde(default)]
    pub subagent_frames: bool,
    /// The sidecar reports the CLI's notices: retries, rate limits, model fallbacks, compaction,
    /// notifications.
    #[serde(default)]
    pub session_notices: bool,
    /// The sidecar reports `/clear` starting a new conversation in the same session.
    #[serde(default)]
    pub conversation_reset: bool,
    /// The sidecar can hand Claude's questions to this host to answer, when the session asks for it.
    #[serde(default)]
    pub structured_questions_v1: bool,
    /// The sidecar serves tools this host declares and answers (`ClientToolCall`), when the session
    /// declares some. Protocol 3.23; read from the handshake's list alone, as the flags above.
    #[serde(default)]
    pub client_tools: bool,
}

/// Descriptive, non-branching facts about the connected provider: versions to show a human, the
/// raw advertised lists for diagnostics, and whatever the provider process said on its way up.
///
/// Separate from `ProviderCapabilities` because these are for *display and debugging*, never for
/// deciding whether to send a command. Anything a caller branches on belongs in the capabilities
/// struct, with a real wire source behind it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderInfo {
    pub sidecar_version: String,
    pub claude_agent_sdk_version: String,
    /// The Claude Code CLI version the provider is actually running against.
    pub actual_claude_code_version: String,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    /// Verbatim `capabilities[]` from the handshake -- kept raw so an unrecognized future
    /// capability is visible in diagnostics rather than silently dropped by the mapping above.
    pub advertised_capabilities: Vec<String>,
    pub advertised_permission_modes: Vec<String>,
    /// The provider's own statement of how much event history it retains, verbatim from the
    /// handshake (the sidecar reports `bounded-<n>`).
    ///
    /// Descriptive, never branched on. It exists so a test that configures a small replay buffer can
    /// confirm the configuration actually took effect rather than asserting on downstream symptoms
    /// -- a ring that silently stayed at its default never evicts, and never gapping is
    /// indistinguishable from correct behaviour.
    pub event_buffer_policy: String,
    /// Which build of the provider this is (for the sidecar: the Verdandi checkout and revision).
    /// Descriptive and essentially always present -- deliberately NOT in `startup_diagnostics`,
    /// because a list that is never empty cannot drive a "something is wrong" indicator.
    pub build_description: Option<String>,
    /// Only things genuinely worth warning about: a CLI version inside the supported range but
    /// untested, a Verdandi checkout that has drifted from the verified baseline. **Empty is the
    /// normal case**, which is what lets a UI treat non-empty as a real signal.
    pub startup_diagnostics: Vec<String>,
    /// The directory of the user's own configuration for this agent, as the agent itself reported it
    /// (Codex's `CODEX_HOME`), when it has one that a status row should name. Descriptive only.
    pub configuration_home: Option<String>,
}

/// A provider-neutral mirror of the sidecar proto's own `ErrorCode`. Exists because the code was
/// being thrown away: `ProviderError::Provider` used to carry only `ErrorDetail.message`, so a
/// caller had no way to tell "you sent a second turn while one was running" (a recoverable ordering
/// complaint about one command) from "the session is gone" (the session must be torn down) except
/// by matching on English prose.
///
/// Mirrored rather than re-exported so no proto type crosses this boundary -- the same discipline
/// that keeps `AgentDomainEvent` free of wire types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorCode {
    Unspecified,
    IncompatibleProtocol,
    UnsupportedCliVersion,
    SessionNotFound,
    TurnAlreadyActive,
    NoActiveTurn,
    PermissionNotFound,
    PermissionAlreadyResolved,
    IdempotencyConflict,
    EventGap,
    InvalidConfiguration,
    ProviderUnavailable,
    ProviderProtocolError,
    DeadlineExceeded,
    /// A message for the running turn named a turn that is no longer the one running.
    TurnMismatch,
    /// The session cannot take a message into the running turn now; the host holds it for the turn's
    /// end.
    SteerUnavailable,
    /// A request named a background task the session does not know (it may have ended already).
    TaskNotFound,
    /// A background task's output cannot be read.
    TaskOutputUnavailable,
    /// A request named a question the session does not hold (it may have been resolved already).
    InteractionNotFound,
    /// An answer to a question was refused as it was given.
    InteractionAnswerRefused,
    /// An answer named a call of a tool this host serves that the session does not hold.
    ClientToolCallNotFound,
    /// An answer named a call that is already over: answered first, or settled some other way.
    ClientToolCallAlreadyAnswered,
}

impl ProviderErrorCode {
    /// True when the error is a complaint about ONE command's timing or arguments, leaving the
    /// session itself healthy. A benign error must be surfaced to the caller and then forgotten;
    /// tearing the session down over one would lose a working conversation.
    ///
    /// Everything not listed is treated as NOT benign on purpose -- an unrecognized future code
    /// must fail toward "this might be serious", never toward "probably fine".
    pub fn is_benign(self) -> bool {
        matches!(
            self,
            ProviderErrorCode::TurnAlreadyActive
                | ProviderErrorCode::NoActiveTurn
                | ProviderErrorCode::PermissionNotFound
                | ProviderErrorCode::PermissionAlreadyResolved
                | ProviderErrorCode::IdempotencyConflict
                | ProviderErrorCode::TurnMismatch
                | ProviderErrorCode::SteerUnavailable
                | ProviderErrorCode::TaskNotFound
                | ProviderErrorCode::TaskOutputUnavailable
                | ProviderErrorCode::InteractionNotFound
                | ProviderErrorCode::InteractionAnswerRefused
                | ProviderErrorCode::ClientToolCallNotFound
                | ProviderErrorCode::ClientToolCallAlreadyAnswered
        )
    }
}

#[derive(Debug)]
pub enum ProviderError {
    /// The caller asked for something this provider's `capabilities()` already says it doesn't
    /// support (e.g. `resume_session` on `ClaudeSidecarProvider` today) -- never returned for
    /// something the provider *should* support but failed to do.
    UnsupportedCapability(&'static str),
    /// A request took longer than its allotted bound to complete -- see `RuntimeThread::block_on`.
    Timeout,
    /// The transport itself failed (spawn, connect, or a gRPC transport-level error) -- distinct
    /// from a business-level error the provider's own service returned successfully as a typed
    /// response.
    Transport(String),
    /// The provider's own service returned a real business error (e.g. `SESSION_NOT_FOUND`) --
    /// carries the provider's own error text, already safe to show a human (never raw stderr or an
    /// unfiltered stack trace), plus the typed code so a caller can tell a
    /// recoverable ordering complaint from a dead session without parsing that text.
    Provider { code: ProviderErrorCode, message: String },
}

impl ProviderError {
    /// True only for a provider-reported error the session can continue past. Transport failures and
    /// timeouts are never benign: they say nothing about whether the command took effect.
    pub fn is_benign(&self) -> bool {
        match self {
            ProviderError::Provider { code, .. } => code.is_benign(),
            _ => false,
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::UnsupportedCapability(what) => write!(f, "unsupported capability: {what}"),
            ProviderError::Timeout => write!(f, "provider request timed out"),
            ProviderError::Transport(msg) => write!(f, "provider transport error: {msg}"),
            ProviderError::Provider { code, message } => write!(f, "provider error ({code:?}): {message}"),
        }
    }
}

impl std::error::Error for ProviderError {}

pub trait AgentProvider {
    fn capabilities(&self) -> ProviderCapabilities;
    /// Descriptive facts for display and diagnostics -- never a substitute for `capabilities()`
    /// when deciding whether a command may be sent.
    fn info(&self) -> ProviderInfo;
    /// Returns the new session's own id.
    fn create_session(&self, request: CreateSessionRequest) -> Result<String, ProviderError>;
    /// Returns the resumed session's own id.
    fn resume_session(&self, request: ResumeSessionRequest) -> Result<String, ProviderError>;
    /// Returns the new turn's own id. At most one active turn per session -- a
    /// provider rejects a second `send_turn` while one is already in flight rather than queuing it.
    fn send_turn(&self, request: SendTurnRequest) -> Result<String, ProviderError>;
    /// [`send_turn`](Self::send_turn) with what the host knows about the message beyond its text. The
    /// default drops the stamp: a provider that cannot carry it sends exactly what `send_turn` does.
    fn send_turn_stamped(&self, request: SendTurnRequest, stamp: TurnStamp) -> Result<String, ProviderError> {
        let _ = stamp;
        self.send_turn(request)
    }
    fn interrupt_turn(&self, request: InterruptTurnRequest) -> Result<(), ProviderError>;
    /// Puts one user message into the running turn without interrupting it. Called on the host's UI
    /// thread, so it must not wait on the agent: `Ok` means the provider took the message and will send
    /// it (or has), never that the agent has it. `Err` means the provider refused it here and nothing was
    /// or will be sent, so the host still holds it. A provider without capability `steer` refuses every
    /// one.
    ///
    /// **Outcomes.** What became of every message taken arrives through [`pump`](Self::pump): one of
    /// `AgentDomainEvent::UserMessageDelivered` (the agent read it) or `UserMessageDropped` (the agent or
    /// the provider refused it, or the provider will not wait for it any longer), with one exception
    /// below. **When:** before that turn's `TurnCompleted`; or, when the agent runs the message as a turn
    /// of its own right after that turn (a follow-up turn the provider opens itself, with no
    /// `send_turn`), inside the follow-up, whose `TurnStarted` is returned by the same `pump()` call as
    /// the previous turn's `TurnCompleted`. So the host never sees no turn running while a message it
    /// handed over has no outcome.
    ///
    /// **Giving up is not knowing.** A provider that stops waiting -- a follow-up that does not come in
    /// time, a session or stream that ends -- reports the message dropped although the agent may have
    /// it; one whose event stream is being re-opened with a replay does not give up meanwhile, since the
    /// outcome may be in that replay. If the agent is later seen to read it, `UserMessageDelivered` still comes under the same id:
    /// the one case of two outcomes for one message, and the later one wins. The host takes such a
    /// message back from its queue rather than send it again (`Queued::dropped_as` in the tab set), and
    /// never puts a message dropped once into a turn again, so a late delivery has one copy to settle. A
    /// session that has ended reports nothing more, so a message dropped with it may have reached the
    /// agent unseen.
    ///
    /// **Order.** Messages reach the agent in the order they were taken: a provider sends one only after
    /// the agent has taken every earlier one, and when one is refused it drops, unsent, every later one
    /// it has not sent. One the agent drops after taking it cannot take back later ones the agent
    /// already has; each is reported as what the agent did with it.
    fn steer_turn(&self, request: SteerTurnRequest) -> Result<(), ProviderError> {
        let _ = request;
        Err(ProviderError::UnsupportedCapability("steer"))
    }
    /// [`steer_turn`](Self::steer_turn) with the stamp's mark of a typed message. The default drops the
    /// stamp. The request's own `client_message_id` and `as_typed` stand; the stamp's are not read.
    fn steer_turn_stamped(&self, request: SteerTurnRequest, stamp: TurnStamp) -> Result<(), ProviderError> {
        let _ = stamp;
        self.steer_turn(request)
    }
    /// Stops work left running between turns. `tasks` is the canonical projection's live,
    /// non-ambient roster; providers with their own background count still validate `shown`.
    fn stop_background(&self, session_id: &str, shown: u32, tasks: &[String]) -> Result<(), ProviderError> {
        let _ = (session_id, shown, tasks);
        Err(ProviderError::UnsupportedCapability("stop_background"))
    }
    /// Starts one stop without waiting on the UI thread. The receiver carries the RPC's outcome.
    fn stop_task(&self, session_id: &str, task_id: &str) -> Result<TaskReply<()>, ProviderError> {
        let _ = (session_id, task_id);
        Err(ProviderError::UnsupportedCapability("stop_task"))
    }
    /// Reads a bounded output tail on the provider runtime, never opening a sidecar-supplied path.
    fn task_output(&self, session_id: &str, task_id: &str) -> Result<TaskReply<TaskOutput>, ProviderError> {
        let _ = (session_id, task_id);
        Err(ProviderError::UnsupportedCapability("task_output"))
    }
    fn resolve_permission(&self, request: ResolvePermissionRequest) -> Result<(), ProviderError>;
    /// Hands the person's answer to one question set to the agent without waiting for it: `Err` is a
    /// refusal before anything was sent, `Ok` the send on its way, whose reply says whether the
    /// provider took it, with the outcome it reported when it reported one. The question ends only
    /// with its `QuestionSettled`, which arrives through [`pump`](Self::pump). An id is single use: an
    /// answer to one already settled is refused (`InteractionNotFound`), and an answer that breaks a
    /// rule of the set is refused (`InteractionAnswerRefused`) with the question still waiting. A
    /// provider that cannot ask questions refuses every one.
    fn resolve_interaction(&self, request: ResolveInteractionRequest) -> Result<InteractionSend, ProviderError> {
        let _ = request;
        Err(ProviderError::UnsupportedCapability("structured_questions_v1"))
    }
    /// The question sets the session still waits on, asked after its event stream was opened again.
    fn get_interaction_state(&self, session_id: &str) -> Result<InteractionState, ProviderError> {
        let _ = session_id;
        Err(ProviderError::UnsupportedCapability("structured_questions_v1"))
    }
    /// Hands one answer to the agent without waiting for it: the caller is the UI thread. `Err` is a
    /// refusal before anything was sent, `Ok` the send on its way, and what becomes of it arrives
    /// through [`pump`](Self::pump) (`ClientToolCallSettled`). The first answer to a call wins, and a
    /// call that has settled, or a session that has closed, makes an answer pointless rather than wrong,
    /// so neither comes back as an error.
    fn answer_client_tool_call(&self, answer: ClientToolAnswer) -> Result<(), ProviderError> {
        let _ = answer;
        Err(ProviderError::UnsupportedCapability("client_tools"))
    }
    /// Reads the session's open calls on the provider runtime. The receiver carries the outcome.
    fn client_tool_call_state(&self, session_id: &str) -> Result<TaskReply<ClientToolCallState>, ProviderError> {
        let _ = session_id;
        Err(ProviderError::UnsupportedCapability("client_tools"))
    }
    fn close_session(&self, request: CloseSessionRequest) -> Result<(), ProviderError>;
    /// Drains every `AgentDomainEvent` that has arrived since the last call. Never blocks --
    /// mirrors `AgentSession::pump()`'s existing contract exactly.
    fn pump(&self) -> Vec<AgentDomainEvent>;
    /// The provider's own sequence number (its wire cursor) of the last event it had taken in when
    /// the latest [`pump`](Self::pump) returned, including one that translated to no domain event.
    /// Asked right after each pump, by the same thread. `None` (the default) for a provider that
    /// numbers nothing. A conversation moved to another process hands this over as
    /// `ChatMoveCore::folded_through`, and the destination's provider reads on from the event after
    /// it: a count of domain events cannot stand in for it, since one provider event may become
    /// none, one or several of them.
    fn pumped_through(&self) -> Option<u64> {
        None
    }
    /// The signal this provider raises whenever it queues something for `pump`, if it raises one
    /// for every such event. With it the reader of `pump` sleeps until there is news; without it
    /// (the default) the reader has to keep asking, because a provider whose `pump` also acts on
    /// the clock (a settle deadline, say) can have news nobody announced.
    fn event_signal(&self) -> Option<std::sync::Arc<EventSignal>> {
        None
    }
    /// This provider's way to move with its chat to another process, if it has one (the default: none, and such a
    /// chat does not move).
    #[cfg(target_os = "linux")]
    fn mover(&self) -> Option<&dyn crate::chat_mover::ChatMover> {
        None
    }
}

/// A counter one side raises and another waits on: "something may have changed since you last
/// looked". Raising never blocks and is never lost, because the waiter names the count it last saw
/// rather than waiting for an edge it might have missed.
#[derive(Debug, Default)]
pub struct EventSignal {
    raised: std::sync::Mutex<u64>,
    changed: std::sync::Condvar,
}

impl EventSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wakes every waiter.
    pub fn raise(&self) {
        let mut raised = self.raised.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *raised = raised.wrapping_add(1);
        self.changed.notify_all();
    }

    /// The count to hand to [`Self::wait_past`]: read it BEFORE looking for news, so news that
    /// lands between the look and the wait still ends the wait.
    pub fn seen(&self) -> u64 {
        *self.raised.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sleeps until the signal has been raised since `seen` was read.
    pub fn wait_past(&self, seen: u64) {
        self.wait_past_unless(seen, || false);
    }

    /// [`Self::wait_past`], ending at once, and on every wake, when `done` says so. `done` is asked
    /// under the signal's lock: whatever a raiser set before its `raise` is visible to it, so a flag
    /// set and raised after `seen` was read (and so not counted as news) is still seen here.
    pub fn wait_past_unless(&self, seen: u64, done: impl Fn() -> bool) {
        let mut raised = self.raised.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while *raised == seen && !done() {
            raised = self
                .changed
                .wait(raised)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}
