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

#[derive(Debug, Clone)]
pub struct CreateSessionRequest {
    pub cwd: String,
    pub permission_mode: PermissionMode,
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

#[derive(Debug, Clone)]
pub struct ResolvePermissionRequest {
    pub session_id: String,
    pub permission_id: String,
    pub allow: bool,
    pub reason: Option<String>,
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
    /// The provider advertises a BYPASS permission mode -- the only permission policy this
    /// milestone actually uses, and the only one whose runtime behavior is genuinely distinct
    /// (`interactive` and `verdandi_rules` are confirmed equivalent in the current sidecar, so
    /// neither is modeled here as a separate capability).
    pub bypass_permission_mode: bool,
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
