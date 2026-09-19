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
pub mod ingestion;
pub mod lease;
pub mod persistence;
#[doc(hidden)]
pub mod process_probe;
pub mod providers;
pub mod settings;
#[doc(hidden)]
pub mod socket_path;
pub mod state_dirs;
pub mod transcript;

pub use conversation::{conversation_id_for_cwd, AgentConversation, ConversationError};
pub use ingestion::{IngestStats, ProjectionGuard, UiDelivery, UI_EVENT_QUEUE_CAPACITY};
pub use persistence::{resumable_sessions, ResumableSession};
pub use event::{AgentEvent, PermissionSource};
pub use process::{disallowed_tools_for, AgentProcess, PermissionMode, CONSERVATIVE_DISALLOWED_TOOLS};
pub use session::AgentSession;
pub use wire::translate_line;

pub use projection::{
    AgentDomainEvent, AgentSessionProjection, ContentKind, PermissionOutcome, ProjectionStatus,
    ToolCallRecord, ToolCallResult, PermissionRequestRecord, TranscriptMessage, UsageInfo,
    TurnOutcome, ResumeStatus, describe_failed_resume,
};
pub use provider::{
    AgentProvider, CreateSessionRequest, ResumeSessionRequest, SendTurnRequest,
    InterruptTurnRequest, PermissionDecision, ResolvePermissionRequest, CloseSessionRequest, ProviderCapabilities,
    ProviderError, ProviderErrorCode, ProviderInfo, StreamingPreference,
};
pub use providers::claude_sidecar::{
    BackpressureStats, ClaudeSidecarProvider, CLIENT_IMPLEMENTS_RESUME, CLIENT_PROTOCOL_MAJOR, EXPECTED_VERDANDI_REVISION,
};
