//! agent: a pure-Rust backend that manages Claude Code CLI sessions. It spawns exactly one
//! long-lived, full-duplex `claude --print --input-format stream-json --output-format stream-json`
//! process per whole conversation, streams user turns and control requests (including a real
//! mid-turn `interrupt`) to its stdin over that process's entire life, translates its real
//! stream-json wire protocol into provider-neutral events, and folds them into a session state a
//! future `agent-ui` module will consume. Tool-permission approval is gated by a dynamically
//! generated `PreToolUse` hook relayed over a per-conversation Unix socket by the `agent-hook`
//! companion binary -- the CLI's own `can_use_tool` control_request is confirmed leaky and is
//! never relied on as the gate. No GTK/WebView dependency.
//!
//! Design docs: docs/superpowers/plans/2026-09-07-agent-v2-streaming-and-permissions.md (v2, the
//! design this crate actually implements) supersedes the process model in
//! docs/superpowers/plans/2026-09-07-agent-claude-cli-backend.md (v1: one `claude -p` process per
//! turn, continuity via `--resume`, no interrupt, no real permission gate), which remains the
//! reference for everything v2 kept unchanged (the wire -> event -> session-state layering, the
//! orphan-safe shutdown discipline). Protocol detail:
//! docs/superpowers/specs/2026-09-07-agent-v2-streaming-protocol-design.md.

mod conversation;
mod event;
mod process;
mod projection;
mod provider;
mod runtime_thread;
mod session;
mod wire;

pub mod external_writer;
pub mod handoff;
pub mod hook_protocol;
pub mod lease;
pub mod persistence;
pub mod providers;
pub mod settings;
pub mod transcript;

pub use conversation::{conversation_id_for_cwd, AgentConversation, ConversationError};
pub use persistence::{resumable_session, ResumableSession};
pub use event::{AgentEvent, PermissionSource};
pub use process::{AgentProcess, PermissionMode, CONSERVATIVE_DISALLOWED_TOOLS};
pub use session::AgentSession;
pub use wire::translate_line;

pub use projection::{
    AgentDomainEvent, AgentSessionProjection, ContentKind, PermissionOutcome, ProjectionStatus,
    ToolCallRecord, ToolCallResult, PermissionRequestRecord, UsageInfo, TurnOutcome,
};
pub use provider::{
    AgentProvider, CreateSessionRequest, ResumeSessionRequest, SendTurnRequest,
    InterruptTurnRequest, ResolvePermissionRequest, CloseSessionRequest, ProviderCapabilities,
    ProviderError, ProviderErrorCode, ProviderInfo,
};
pub use providers::claude_sidecar::{ClaudeSidecarProvider, EXPECTED_VERDANDI_REVISION};
