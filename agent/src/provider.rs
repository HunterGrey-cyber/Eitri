//! The provider-neutral `AgentProvider` trait a concrete backend implements.
//! `agent::session::AgentSession` (the legacy Claude-CLI backend) does NOT implement this trait in
//! this phase -- Phase 3's job is proving `ClaudeSidecarProvider` works standalone, not
//! retrofitting the legacy backend onto a shared interface it was never designed for (that's Phase
//! 5's "double backend A/B" job, once both sides are genuinely comparable side by side).
//!
//! Every method here is a synchronous, bounded-time Rust function -- matching the existing
//! `AgentSession` API shape a caller already knows -- even though a real implementation
//! does real async gRPC I/O underneath via `RuntimeThread`
//!. Streaming output (assistant text, tool calls, permission requests, ...) is never
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
}

/// Continues an existing provider session. Resumed sessions do not inherit their earlier
/// permission policy, so they are created gated just like fresh sessions through
/// `CreateSessionRequest`. No permission mode parameter is needed here.
#[derive(Debug, Clone)]
pub struct ResumeSessionRequest {
    /// Claude's own session UUID -- the one `SessionOpened.provider_session_id` reported.
    pub provider_session_id: String,
    pub cwd: String,
    pub streaming: StreamingPreference,
    /// Whether the project's own settings tiers load. Stated afresh for every resume: the
    /// conversation continues, but what the project may run is decided now, not when it began.
    pub project: ProjectTrust,
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

#[derive(Debug, Clone)]
pub struct CloseSessionRequest {
    pub session_id: String,
}

/// Capabilities reported by the provider, never inferred from its name, enum members,
/// or RPCs in a generated stub. Hide or disable unavailable features and send no commands for them.
/// Each bool has a source in `HandshakeResponse` and represents a choice a caller makes.
/// `Copy` lets callers pass capabilities freely. Owned lists, versions and diagnostics
/// belong in `ProviderInfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// The provider can continue an existing provider session by id.
    pub resume: bool,
    /// The provider can branch an existing provider session into a new one.
    pub fork: bool,
    /// The provider can cancel an in-flight turn.
    pub interrupt: bool,
    /// The provider advertises BYPASS mode, where tools run without asking.
    /// This is a handshake fact only. The host implements bypass by answering `allow` through the
    /// gate, which requires `interactive_permission_mode` rather than provider BYPASS mode.
    pub bypass_permission_mode: bool,
    /// The provider advertises INTERACTIVE mode, where tool use raises `PermissionRequested`.
    ///
    /// Tracked separately from `bypass_permission_mode` because providers advertise them independently.
    /// `verdandi_rules` is not modeled: it behaves identically to INTERACTIVE in the current sidecar,
    /// so a third choice would add no distinction. Every session uses INTERACTIVE;
    /// `ClaudeSidecarProvider::require_interactive` refuses providers without it at session creation
    /// rather than silently choosing another permission policy.
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
}

/// Descriptive, non-branching facts about the connected provider: versions to show a human, the
/// raw advertised lists for diagnostics, and whatever the provider process said on its way up.
///
/// Separate from `ProviderCapabilities` because these are for *display and debugging*, never for
/// deciding whether to send a command. Anything a caller branches on belongs in the capabilities
/// struct, with a real wire source behind it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// A business error returned by the provider, such as `SESSION_NOT_FOUND`.
    /// Carries error text safe to show a human, never raw stderr or an unfiltered stack trace.
    /// The typed code distinguishes a recoverable ordering error from a dead session
    /// without parsing the message.
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
    /// Returns the new turn's id. At most one turn may be active in a session;
    /// a second `send_turn` is rejected while a turn is in flight, rather than queued.
    fn send_turn(&self, request: SendTurnRequest) -> Result<String, ProviderError>;
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
    fn resolve_permission(&self, request: ResolvePermissionRequest) -> Result<(), ProviderError>;
    fn close_session(&self, request: CloseSessionRequest) -> Result<(), ProviderError>;
    /// Drains every `AgentDomainEvent` that has arrived since the last call. Never blocks --
    /// mirrors `AgentSession::pump()`'s existing contract exactly.
    fn pump(&self) -> Vec<AgentDomainEvent>;
    /// The signal this provider raises whenever it queues something for `pump`, if it raises one
    /// for every such event. With it the reader of `pump` sleeps until there is news; without it
    /// (the default) the reader has to keep asking, because a provider whose `pump` also acts on
    /// the clock (a settle deadline, say) can have news nobody announced.
    fn event_signal(&self) -> Option<std::sync::Arc<EventSignal>> {
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
