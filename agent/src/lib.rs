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
// The legacy backend -- the session, the CLI spawn, the stream-json wire and the hook relay -- is
// compiled only with the `legacy-backend` feature (spec 2026-09-27-v1-dist-design.md §10, D16).
// `process` stays: `PermissionMode`, `disallowed_tools`, `CONSERVATIVE_DISALLOWED_TOOLS` and the D12
// CLI-mode classification (`classify_cli_mode`, `CliModeReport`) live there and the sidecar path uses
// them, so only its spawn items are gated, in place.
#[cfg(feature = "legacy-backend")]
mod event;
mod process;
mod projection;
mod provider;
mod runtime_thread;
#[cfg(feature = "legacy-backend")]
mod session;
#[cfg(not(feature = "legacy-backend"))]
mod session_stub;
#[cfg(feature = "legacy-backend")]
mod wire;
/// A source scan keeping every route back to an ungated `claude` out of this crate (R07). Test-only,
/// and declared so on purpose: its own pattern literals are then test code, which it does not scan.
#[cfg(test)]
mod wire_guard;

pub mod account;
pub mod external_writer;
pub mod handoff;
pub mod history;
#[cfg(feature = "legacy-backend")]
pub mod hook_protocol;
pub mod ingestion;
pub mod lease;
pub mod permission_policy;
pub mod permission_rules;
pub mod persistence;
pub mod private_fs;
#[doc(hidden)]
pub mod process_probe;
pub mod providers;
pub mod setting_sources;
#[cfg(feature = "legacy-backend")]
pub mod settings;
#[doc(hidden)]
pub mod socket_path;
pub mod state_dirs;
pub mod transcript;

pub use account::{AccountError, ClaudeAccount};
pub use conversation::{conversation_id_for_cwd, AgentConversation, ConversationError};
#[cfg(feature = "legacy-backend")]
pub use event::{AgentEvent, PermissionSource};
pub use ingestion::{IngestStats, ProjectionGuard, RevisedDelivery, UiDelivery, UI_EVENT_QUEUE_CAPACITY};
pub use permission_policy::{
    classify_permission_request, classify_with_rules, rule_that_allows, Classification, PermissionVerdict,
};
pub use permission_rules::{PrefixRule, PrefixRules};
pub use persistence::{resumable_sessions, NameUpdate, ResumableSession};
#[cfg(feature = "legacy-backend")]
pub use process::AgentProcess;
pub use process::{
    classify_cli_mode, classify_reported_cli_mode, disallowed_tools, CliModeReport, PermissionMode, RequestedCliMode,
    CONSERVATIVE_DISALLOWED_TOOLS,
};
#[cfg(feature = "legacy-backend")]
pub use session::AgentSession;
#[cfg(not(feature = "legacy-backend"))]
pub use session_stub::AgentSession;
#[cfg(feature = "legacy-backend")]
pub use wire::translate_line;

/// Whether this build contains the legacy backend (the `legacy-backend` feature, off in every
/// release: spec 2026-09-27-v1-dist-design.md §10, D16). The one place the fact lives: backend
/// selection reads it rather than repeating the `cfg!`.
pub const LEGACY_BACKEND_COMPILED: bool = cfg!(feature = "legacy-backend");

/// What a build without the legacy backend says when legacy is asked for: `AgentSession::start`'s
/// error, and the startup error for `--legacy`/`EITRI_AGENT_BACKEND=legacy`. Names the flag that
/// brings it back, because the only reader who can act on this is a developer.
pub const LEGACY_NOT_IN_BUILD: &str =
    "the legacy backend is not in this build (it is development-only: build with --features shell/legacy-backend)";

pub use projection::{
    describe_failed_resume, AgentDomainEvent, AgentSessionProjection, ContentKind, HistoryNotice, HistorySource,
    MatchedAskRule, PermissionOutcome, PermissionRequestRecord, ProjectionStatus, ProviderPrompt, ResumeStatus,
    TokenUsage, ToolCallDenial, ToolCallRecord, ToolCallResult, TranscriptMessage, TurnEndDetail, TurnEndingKind,
    TurnEndingRecord, TurnOutcome, UngatedCliModeRecord, UsageInfo, UserPromptRecord, MAX_TURN_END_MESSAGE_CHARS,
};
pub use provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, PermissionDecision,
    ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest,
    ResumeSessionRequest, SendTurnRequest, StreamingPreference,
};
pub use providers::claude_sidecar::{
    sidecar_availability, sidecar_missing_message, user_sidecar_path, BackpressureStats, ClaudeSidecarProvider,
    SidecarAvailability, CLIENT_IMPLEMENTS_RESUME, CLIENT_PROTOCOL_MAJOR, EXPECTED_VERDANDI_REVISION, NO_SIDECAR_HINT,
    SIDECAR_EXIT_GRACE, UNARY_RPC_TIMEOUT,
};
