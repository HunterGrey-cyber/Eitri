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
// The session, CLI spawning, stream-json wire, and hook relay require `legacy-backend`.
// `process` remains available because the sidecar uses `PermissionMode`, `disallowed_tools`,
// `CONSERVATIVE_DISALLOWED_TOOLS`, and CLI-mode classification (`classify_cli_mode`, `CliModeReport`).
// Only its spawning items are feature-gated.
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
/// Runs a test body under a chosen umask in a child copy of the test binary, so the process-wide
/// umask is never changed while other tests create files.
#[cfg(test)]
mod test_umask;
#[cfg(feature = "legacy-backend")]
mod wire;
/// Scans source for routes to an ungated `claude` process. This module is test-only so its own
/// pattern literals are excluded from the source it scans.
#[cfg(test)]
mod wire_guard;

pub mod account;
pub mod external;
pub mod external_writer;
pub mod handoff;
pub mod history;
#[cfg(feature = "legacy-backend")]
pub mod hook_protocol;
pub mod ingestion;
pub mod jsonrpc_link;
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
pub use external::{DirTrust, ExternalProvider, ExternalStart, Reviewer, TrustPlan, TurnPolicy};
pub use ingestion::{IngestStats, ProjectionGuard, RevisedDelivery, UiDelivery, UiWake, UI_EVENT_QUEUE_CAPACITY};
pub use permission_policy::{
    classify_in_manual, classify_permission_request, classify_with_rules, rule_that_allows, Classification,
    PermissionVerdict,
};
pub use permission_rules::{PrefixRule, PrefixRules, RuleRefusal};
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

/// Whether this build includes the `legacy-backend` feature. Backend selection reads this
/// constant so the feature check has a single source.
pub const LEGACY_BACKEND_COMPILED: bool = cfg!(feature = "legacy-backend");

/// What a build without the legacy backend says when legacy is asked for: `AgentSession::start`'s
/// error, and the startup error for `--legacy`/`EITRI_AGENT_BACKEND=legacy`. Names the flag that
/// brings it back, because the only reader who can act on this is a developer.
pub const LEGACY_NOT_IN_BUILD: &str =
    "the legacy backend is not in this build (it is development-only: build with --features shell/legacy-backend)";

pub use projection::{
    describe_failed_resume, AccountLimitsRecord, AgentDomainEvent, AgentSessionProjection, ContentKind, HistoryNotice,
    HistorySource, InputChange, MatchedAskRule, PermissionOutcome, PermissionRequestRecord, ProjectionStatus,
    ProviderPrompt, ResumeStatus, TokenUsage, ToolCallDenial, ToolCallRecord, ToolCallResult, TranscriptMessage,
    TurnEndDetail, TurnEndingKind, TurnEndingRecord, TurnOutcome, UngatedCliModeRecord, UsageInfo, UserPromptRecord,
    MAX_TURN_END_MESSAGE_CHARS,
};
pub use provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, EventSignal, InterruptTurnRequest, PermissionDecision,
    ProviderCapabilities, ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest,
    ResumeSessionRequest, SendTurnRequest, StreamingPreference,
};
pub use providers::claude_sidecar::{
    sidecar_availability, sidecar_missing_message, user_sidecar_path, BackpressureStats, ClaudeSidecarProvider,
    SidecarAvailability, CLIENT_IMPLEMENTS_RESUME, CLIENT_PROTOCOL_MAJOR, EXPECTED_VERDANDI_REVISION, NO_SIDECAR_HINT,
    SIDECAR_EXIT_GRACE, UNARY_RPC_TIMEOUT,
};
