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
//! The one long-lived process replaced an earlier model of one `claude -p` process per turn, with
//! continuity through `--resume`, which could not interrupt a turn and had no real permission
//! gate. Everything else that model had stayed: the wire -> event -> session-state layering and the
//! orphan-safe shutdown discipline. The wire shapes this crate relies on were captured from the
//! real CLI and are kept as fixtures under `agent/tests/fixtures/`.

mod conversation;
// The legacy backend -- the session, the CLI spawn, the stream-json wire and the hook relay -- is
// compiled only with the `legacy-backend` feature, off in every release, so what users run carries
// only the sidecar path. `process` stays: `PermissionMode`, `disallowed_tools`,
// `CONSERVATIVE_DISALLOWED_TOOLS` and the
// CLI-mode classification (`classify_cli_mode`, `CliModeReport`) live there and the sidecar path uses
// them, so only its spawn items are gated, in place.
#[cfg(feature = "legacy-backend")]
mod event;
mod process;
pub mod projection;
mod provider;
mod runtime_thread;
#[cfg(feature = "legacy-backend")]
mod session;
#[cfg(not(feature = "legacy-backend"))]
mod session_stub;
mod steer_gate;
/// Runs a test body under a chosen umask in a child copy of the test binary, so the process-wide
/// umask is never changed while other tests create files.
/// A throwaway directory under the temp dir that is removed on drop, panics included.
#[cfg(test)]
mod test_tmp;
#[cfg(test)]
mod test_umask;
#[cfg(feature = "legacy-backend")]
mod wire;
/// A source scan keeping every route back to an ungated `claude` out of this crate. Test-only,
/// and declared so on purpose: its own pattern literals are then test code, which it does not scan.
#[cfg(test)]
mod wire_guard;

pub mod account;
#[cfg(target_os = "linux")]
pub mod chat_mover;
#[cfg(target_os = "linux")]
pub mod child;
pub mod external;
pub mod external_link;
pub mod external_writer;
pub mod handoff;
pub mod history;
pub use history::{configure_history_turns, history_turns};
#[cfg(feature = "legacy-backend")]
pub mod hook_protocol;
pub mod ingestion;
pub mod installed_cli;
pub mod jsonrpc_link;
pub mod lease;
pub mod mention;
mod move_seed;
pub mod permission_policy;
pub mod permission_rules;
pub mod persistence;
pub mod private_fs;
#[doc(hidden)]
pub mod process_probe;
pub mod project_boundary;
pub mod provider_move;
pub mod providers;
pub mod read_deny;
pub mod remote_client;
pub mod save_health;
pub mod setting_sources;
#[cfg(feature = "legacy-backend")]
pub mod settings;
#[doc(hidden)]
pub mod socket_path;
pub mod ssh;
pub mod state_dirs;
#[cfg(feature = "test-support")]
pub mod testkit;
pub mod transcript;

pub use account::{AccountError, ClaudeAccount};
pub use conversation::{
    conversation_id_for_cwd, AgentConversation, ConversationError, DenyAndStopOutcome, PromptOrigin,
};
#[cfg(feature = "legacy-backend")]
pub use event::{AgentEvent, PermissionSource};
pub use external::{
    DirTrust, EditorServerSpec, ExternalProvider, ExternalStart, ListedSession, Reviewer, Sandbox, TrustPlan,
    TurnPolicy,
};
pub use ingestion::{IngestStats, ProjectionGuard, RevisedDelivery, UiDelivery, UiWake, UI_EVENT_QUEUE_CAPACITY};
pub use move_seed::{
    ChatMoveCore, ConversationHandoff, HandoffRefused, MoveSeed, MovedProvider, PendingName, SeedRefused,
};
pub use permission_policy::{
    classify_in_manual, classify_permission_request, classify_with_rules, classify_without_a_boundary, judge_edit_path,
    judge_project_path, rule_that_allows, Classification, GateRoot, OutsideReason, PathJudgement, PermissionVerdict,
};
pub use permission_rules::{PrefixRule, PrefixRules, RuleRefusal};
pub use persistence::{
    conversation_is_full, resumable_sessions, NameUpdate, ResumableSession, MAX_RECORDS_PER_CONVERSATION,
};
#[cfg(feature = "legacy-backend")]
pub use process::AgentProcess;
pub use process::{
    classify_cli_mode, classify_reported_cli_mode, classify_reported_cli_mode_planning, disallowed_tools,
    CliExpectation, CliModeReport, PermissionMode, RequestedCliMode, CONSERVATIVE_DISALLOWED_TOOLS,
};
pub use provider_move::MoveRefusedWhy;
#[cfg(feature = "legacy-backend")]
pub use session::AgentSession;
#[cfg(not(feature = "legacy-backend"))]
pub use session_stub::AgentSession;
#[cfg(feature = "legacy-backend")]
pub use wire::translate_line;

/// Whether this build contains the legacy backend (the `legacy-backend` feature, off in every
/// release). The one place the fact lives: backend
/// selection reads it rather than repeating the `cfg!`.
pub const LEGACY_BACKEND_COMPILED: bool = cfg!(feature = "legacy-backend");

/// What a build without the legacy backend says when legacy is asked for: `AgentSession::start`'s
/// error, and the startup error for `--legacy`/`EITRI_AGENT_BACKEND=legacy`. Names the flag that
/// brings it back, because the only reader who can act on this is a developer.
pub const LEGACY_NOT_IN_BUILD: &str =
    "the legacy backend is not in this build (it is development-only: build with --features shell/legacy-backend)";

pub use projection::{
    describe_failed_resume, thinking_open_after, AccountLimitsRecord, AgentDomainEvent, AgentSessionProjection,
    BackgroundTask, BackgroundTaskRecord, CallPlanMark, ContentKind, Evicted, HistoryNotice, HistorySource, InitServer,
    InputChange, MatchedAskRule, NoticeLevel, NoticeRecord, OpenClientCall, PermissionOutcome, PermissionRequestRecord,
    PlanAllowedPrompt, PlanCause, PlanExecutionMode, PlanOutcome, PlanPhase, PlanRecord, PlanState, PlanningState,
    ProjectionStatus, ProviderPrompt, QuestionOptionSpec, QuestionOutcome, QuestionRecord, QuestionSpec, QuestionState,
    ResumeStatus, SessionNoticeKind, ShownAnswer, SlashCommand, SteerOutcome, TaskEdge, TaskKind, TaskRoster,
    TaskStatus, TaskUsage, ThinkingBlock, TokenUsage, ToolCallDenial, ToolCallRecord, ToolCallResult,
    TranscriptMessage, TurnCause, TurnCauseKind, TurnEndDetail, TurnEndingKind, TurnEndingRecord, TurnOrigin,
    TurnOutcome, TurnStartRecord, UngatedCliModeRecord, UsageInfo, UserPromptRecord, WorkflowAgent, WorkflowPhase,
    WorkflowProgress, CLI_REFUSED_DROP, MAX_HOOK_BLOCK_CHARS, MAX_STEER_OUTCOMES, MAX_TURN_END_MESSAGE_CHARS,
    NO_OPEN_TURN_ID, NO_RESULT_REASON, PLAN_FEEDBACK_MAX_BYTES, PLAN_TRANSITION_GUARD_MS,
};
pub use provider::{
    AgentProvider, AppliedSettings, AttachmentMeta, ClientCallRef, ClientToolAnswer, ClientToolCallState,
    ClientToolDecl, ClientToolSet, CloseSessionRequest, CreateSessionRequest, EffortChoice, EventSignal,
    InteractionReply, InteractionResolution, InteractionSend, InteractionState, InterruptTurnRequest, ModelList,
    ModelOption, PermissionDecision, PlanningReply, PlanningSend, ProviderCapabilities, ProviderError,
    ProviderErrorCode, ProviderInfo, QuestionAnswer, QuestionDeclineKind, RemoteEnding, RemoteLinkState, ReplySend,
    ResolveInteractionRequest, ResolvePermissionRequest, ResumeSessionRequest, SendTurnRequest, SetPlanningRequest,
    SteerTurnRequest, StreamingPreference, TaskOutput, TaskReply, TurnContentLimits, TurnImage, TurnStamp,
    WaitingRequest,
};
/// Images on a Claude turn: the sidecar's reason words for a refusal and the composer's words for them.
pub use providers::claude_sidecar::turn_content;
pub use providers::claude_sidecar::{
    apply_sidecar_env, claude_cli_missing_here, claude_problem_here, failure_names_missing_cli, locate_claude_here,
    locate_claude_in, sidecar_availability, sidecar_env_policy, sidecar_for_upload, sidecar_missing_message,
    sidecar_origin, user_sidecar_path, BackpressureStats, ClaudeCli, ClaudeProblem, ClaudeSidecarProvider,
    HelperHandoff, SidecarAvailability, SidecarEnv, UploadSidecar, UploadSidecarOrigin, BUNDLED_SIDECAR_REVISION,
    BUNDLED_SIDECAR_VERSION, CLAUDE_INSTALL_URL, CLAUDE_LOOKED_IN, CLAUDE_REFUSED_NOTE, CLIENT_IMPLEMENTS_RESUME,
    CLIENT_PROTOCOL_MAJOR, CLI_MISSING_FAILURE_MARKER, NO_SIDECAR_HINT, REFUSED_CLAUDE_NOTE, REQUIRED_CAPABILITIES,
    SIDECAR_EXIT_GRACE, UNARY_RPC_TIMEOUT, UNTESTED_CLAUDE_NOTE,
};
