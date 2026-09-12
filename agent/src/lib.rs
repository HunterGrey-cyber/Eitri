//! A pure Rust backend for Claude Code CLI sessions. Each conversation uses one long-lived,
//! full-duplex `claude --print --input-format stream-json --output-format stream-json`
//! process. User turns and control requests, including mid-turn `interrupt`, go to its stdin.
//! The stream-json output becomes provider-neutral events folded into session state for
//! `agent-ui` to consume.
//!
//! Tool permissions are gated by a generated `PreToolUse` hook relayed over a per-conversation
//! Unix socket by the `agent-hook` companion binary. The CLI's `can_use_tool` control request
//! alone does not reliably gate tool execution. This crate has no GTK or WebView dependency.

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
