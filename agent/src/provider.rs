//! The provider-neutral `AgentProvider` trait (design doc §10.1) a concrete backend implements.
//! `agent::session::AgentSession` (the legacy Claude-CLI backend) does NOT implement this trait in
//! this phase -- Phase 3's job is proving `ClaudeSidecarProvider` works standalone, not
//! retrofitting the legacy backend onto a shared interface it was never designed for (that's Phase
//! 5's "double backend A/B" job, once both sides are genuinely comparable side by side).
//!
//! Every method here is a synchronous, bounded-time Rust function -- matching the existing
//! `AgentSession` API shape a caller already knows -- even though a real implementation
//! (`ClaudeSidecarProvider`, Task 7) does real async gRPC I/O underneath via `RuntimeThread`
//! (Task 3). Streaming output (assistant text, tool calls, permission requests, ...) is never
//! returned directly from these methods; it arrives via `pump()`, mirroring `AgentSession::pump()`.

use crate::{AgentDomainEvent, PermissionMode};

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

#[derive(Debug, Clone)]
pub struct CreateSessionRequest {
    pub cwd: String,
    pub permission_mode: PermissionMode,
    pub streaming: StreamingPreference,
}

/// Continue an existing provider (Claude) session rather than starting a fresh one.
///
/// Carries a `permission_mode` because the host policy applies to the resumed session exactly as it
/// does to a new one -- resuming does not inherit the policy the original session ran under, and
/// silently defaulting it would mean a conversation could come back with a different permission
/// posture than the caller asked for.
#[derive(Debug, Clone)]
pub struct ResumeSessionRequest {
    /// Claude's own session UUID -- the one `SessionOpened.provider_session_id` reported.
    pub provider_session_id: String,
    pub cwd: String,
    pub permission_mode: PermissionMode,
    pub streaming: StreamingPreference,
}

#[derive(Debug, Clone)]
pub struct SendTurnRequest {
    pub session_id: String,
    pub text: String,
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
}

impl PermissionDecision {
    pub fn allows(&self) -> bool {
        matches!(self, PermissionDecision::Allow)
    }

    /// The text the model is shown. Never `Some` for an approval: there is nowhere for it to go.
    pub fn reason(&self) -> Option<&str> {
        match self {
            PermissionDecision::Allow => None,
            PermissionDecision::Deny { reason } => reason.as_deref(),
        }
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

/// What a provider can actually do, as reported by the provider itself -- never inferred from a
/// provider's name, an enum member's existence, or an RPC being present in a generated stub.
///
/// Design doc §9.3 makes this a contract: "capability 不存在：UI 隐藏或禁用功能，不发送'服务端
/// 应该会忽略'的命令". That only works if every field here has a real wire source, so this struct
/// stays deliberately small -- one bool per thing a caller genuinely branches on, each derivable
/// from a real `HandshakeResponse`. `Copy` on purpose: callers pass it around freely, and anything
/// that needs an owned list (advertised names, versions, diagnostics) belongs in `ProviderInfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// The provider can continue an existing provider session by id.
    pub resume: bool,
    /// The provider can branch an existing provider session into a new one.
    pub fork: bool,
    /// The provider can cancel an in-flight turn.
    pub interrupt: bool,
    /// The provider advertises a BYPASS permission mode: tools run without asking.
    pub bypass_permission_mode: bool,
    /// The provider advertises an INTERACTIVE permission mode: tool use raises a real
    /// `PermissionRequested` that a human answers.
    ///
    /// A separate capability from `bypass_permission_mode` because the two are separately
    /// advertised and separately absent, and because choosing between them is a decision about what
    /// the agent is allowed to do unsupervised. `verdandi_rules` is deliberately NOT modeled: it is
    /// confirmed to behave identically to `interactive` in the current sidecar, so offering it as a
    /// third choice would be a distinction without a difference.
    pub interactive_permission_mode: bool,
}

impl ProviderCapabilities {
    /// Whether this provider can actually honor a given permission policy.
    ///
    /// The hard rule this exists to enforce: **a requested permission mode must never silently
    /// become a different one.** A provider that cannot do what was asked must fail loudly, exactly
    /// as a resume that cannot continue a session must never quietly start a fresh one -- the cost
    /// of getting it wrong is the same in kind, an agent running under a policy nobody chose.
    pub fn supports_permission_mode(&self, mode: PermissionMode) -> bool {
        match mode {
            PermissionMode::Auto => self.interactive_permission_mode,
            PermissionMode::Bypass => self.bypass_permission_mode,
        }
    }
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
    /// Which build of the provider this is (for the sidecar: the Verdandi checkout and revision).
    /// Descriptive and essentially always present -- deliberately NOT in `startup_diagnostics`,
    /// because a list that is never empty cannot drive a "something is wrong" indicator.
    pub build_description: Option<String>,
    /// Only things genuinely worth warning about: a CLI version inside the supported range but
    /// untested, a Verdandi checkout that has drifted from the verified baseline. **Empty is the
    /// normal case**, which is what lets a UI treat non-empty as a real signal.
    pub startup_diagnostics: Vec<String>,
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
    /// unfiltered stack trace, per design doc §9.7), plus the typed code so a caller can tell a
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
    /// Returns the new turn's own id. Design doc §5.2: at most one active turn per session -- a
    /// provider rejects a second `send_turn` while one is already in flight rather than queuing it.
    fn send_turn(&self, request: SendTurnRequest) -> Result<String, ProviderError>;
    fn interrupt_turn(&self, request: InterruptTurnRequest) -> Result<(), ProviderError>;
    fn resolve_permission(&self, request: ResolvePermissionRequest) -> Result<(), ProviderError>;
    fn close_session(&self, request: CloseSessionRequest) -> Result<(), ProviderError>;
    /// Drains every `AgentDomainEvent` that has arrived since the last call. Never blocks --
    /// mirrors `AgentSession::pump()`'s existing contract exactly.
    fn pump(&self) -> Vec<AgentDomainEvent>;
}
