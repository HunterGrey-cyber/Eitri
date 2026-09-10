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

/// No RPC exists yet to fulfill this (design doc §19, confirmed with Verdandi) -- kept as a real,
/// typed request so the trait's shape doesn't need to change again once Phase 4 adds it. Every
/// current implementation of `resume_session` returns `ProviderError::UnsupportedCapability`.
#[derive(Debug, Clone)]
pub struct ResumeSessionRequest {
    pub provider_session_id: String,
    pub cwd: String,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub resume: bool,
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
    /// unfiltered stack trace, per design doc §9.7).
    Provider(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::UnsupportedCapability(what) => write!(f, "unsupported capability: {what}"),
            ProviderError::Timeout => write!(f, "provider request timed out"),
            ProviderError::Transport(msg) => write!(f, "provider transport error: {msg}"),
            ProviderError::Provider(msg) => write!(f, "provider error: {msg}"),
        }
    }
}

impl std::error::Error for ProviderError {}

pub trait AgentProvider {
    fn capabilities(&self) -> ProviderCapabilities;
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
