// agent/src/providers/claude_sidecar/mod.rs
//! `ClaudeSidecarProvider`: drives Verdandi's `apps/claude-sidecar` gRPC service.
//! Spawns and owns exactly one sidecar process for its own lifetime, connects to
//! it over a UDS via `tonic` on a dedicated `RuntimeThread`, and issues unary RPCs (this file)
//! plus the streaming event pump. Fresh sessions and resumes share `CreateSession`, selecting
//! Claude Code's preset with static panel context only when the handshake offers it.

mod cli_locate;
mod contract;
mod effort_reply;
/// Handing a sidecar to another process (a chat moving to the other window): pidfds, Linux only.
#[cfg(target_os = "linux")]
mod moving;
mod spawn;
mod steer;
mod system_prompt;
mod translate;
pub(crate) mod translate_322;
mod watch;

/// Real, billed A/B runs proving that protocol 3's two narrowing fields do what they say at
/// runtime. Read its header before running anything in it.
#[cfg(test)]
mod runtime_policy_verification;

pub use cli_locate::{
    claude_cli_missing_here, claude_problem_here, failure_names_missing_cli, locate_claude_here, ClaudeCli,
    ClaudeProblem, CLAUDE_INSTALL_URL, CLAUDE_LOOKED_IN, CLAUDE_REFUSED_NOTE, CLI_MISSING_FAILURE_MARKER,
};
pub use contract::{
    missing_required, SidecarVersion, BUNDLED_SIDECAR_VERSION, REFUSED_CLAUDE_NOTE, REQUIRED_CAPABILITIES,
    UNTESTED_CLAUDE_NOTE,
};
#[cfg(target_os = "linux")]
pub use moving::{ClaudeProviderHandoff, MoveRefusedWhy};
#[cfg(target_os = "linux")]
pub use spawn::SidecarFds;
pub use spawn::{
    sidecar_availability, sidecar_missing_message, sidecar_origin, user_sidecar_path, SidecarAvailability,
    BUNDLED_SIDECAR_REVISION, NO_SIDECAR_HINT, SIDECAR_EXIT_GRACE,
};

use crate::provider::{
    AgentProvider, ClientCallRef, ClientToolAnswer, ClientToolCallState, ClientToolDecl, ClientToolSet,
    CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, ProviderCapabilities, ProviderError,
    ProviderErrorCode, ProviderInfo, ResolvePermissionRequest, ResumeSessionRequest, SendTurnRequest, SteerTurnRequest,
    StreamingPreference, TurnStamp,
};
use crate::runtime_thread::RuntimeThread;
use crate::setting_sources::{ProjectTrust, Source};
use crate::AgentDomainEvent;
use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
use claude_runtime_protocol::v1::{
    client_tool_content, AnswerClientToolCallRequest, ClaudeHostPolicy, CliPermissionMode, ClientToolContent,
    ClientToolDeclaration, ClientTools, CloseSessionRequest as ProtoCloseSessionRequest, ConfigurationProfile,
    CreateSessionRequest as ProtoCreateSessionRequest, ErrorCode as ProtoErrorCode, ErrorDetail, ExecutableSource,
    GetClientToolCallStateRequest, GetClientToolCallStateResponse, HandshakeRequest, HandshakeResponse,
    InterruptTurnRequest as ProtoInterruptTurnRequest, PermissionMode as ProtoPermissionMode, PersistenceMode,
    ReplayStart, ResolvePermissionRequest as ProtoResolvePermissionRequest, SendTurnRequest as ProtoSendTurnRequest,
    SettingSource, SettingSourceSelection, SteerTurnRequest as ProtoSteerTurnRequest, StreamingMode,
    SystemPromptPreset, ToolPolicy, WatchSessionEventsRequest,
};
use hyper_util::rt::TokioIo;
use spawn::{SpawnedSidecar, UpdateChoice, UpdateTrouble};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use system_prompt::EITRI_APPEND;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

pub const UNARY_RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// `SetModel` on an id the CLI does not list waits for the API request the CLI checks it with.
const SET_MODEL_TIMEOUT: Duration = Duration::from_secs(30);
/// Long enough for the sidecar's pump, which ticks every 20 ms, to have emptied what the CLI wrote before a change.
const APPLIED_AGAIN_AFTER: Duration = Duration::from_millis(150);

/// The protocol major this client speaks. A sidecar reporting anything else is refused outright: its
/// wire contract is not one this client can name.
pub const CLIENT_PROTOCOL_MAJOR: u32 = 3;

/// Capability strings this client understands, as the sidecar advertises them in
/// `HandshakeResponse.capabilities`. Named constants rather than inline literals because a typo
/// here fails silently in the worst possible direction: the capability reads as absent, the feature
/// is hidden, and nothing anywhere reports a problem.
const CAP_INTERRUPT_TURN: &str = "interrupt_turn";
const CAP_RESUME_SESSION: &str = "resume_session";
const CAP_FORK_SESSION: &str = "fork_session";
/// The sidecar numbers each assistant message in its text deltas. Read by `connect()` to decide `splits_messages`, which `start_watching` then
/// uses to split two sidecar assistant messages at a `TextDelta.message_id` change -- see
/// `translate::MessageSplit`.
const CAP_TEXT_DELTA_MESSAGE_ID: &str = "text_delta_message_id";
/// `ClaudeHostPolicy.provider_permission_prompts` -- the CLI's OWN permission
/// prompts (its sensitive-file safety check on `.git/`/`.claude/` is the measured one) routed to this
/// host as `PermissionRequested` with origin PROVIDER_PROMPT. Read by `connect()` into
/// `provider_prompts`, which `build_create_request` sends.
const CAP_PROVIDER_PERMISSION_PROMPTS: &str = "provider_permission_prompts";
/// Protocol 3.14: `ClaudeHostPolicy.cli_permission_mode` and
/// `ResolvePermissionRequest.defer`. Taken from the protocol crate's generated constants rather than
/// typed here, so a renamed capability fails to compile instead of reading as absent.
const CAP_CLI_AUTO_MODE: &str = claude_runtime_protocol::CAP_CLI_AUTO_MODE;
const CAP_PERMISSION_DEFER: &str = claude_runtime_protocol::CAP_PERMISSION_DEFER;
/// Protocol 3.20: the `SteerTurn` RPC and its `UserMessageDelivered` /
/// `UserMessageDropped` events. Advertised only when the sidecar's CLI is new enough to report what
/// became of a message; `ProviderCapabilities::steer`.
const CAP_STEER_TURN: &str = claude_runtime_protocol::CAP_STEER_TURN;
/// Protocol 3.22: one capability per `ProviderCapabilities` flag of the same name, taken from the
/// protocol crate so a renamed capability fails to compile instead of reading as absent.
const CAP_CLI_TURNS: &str = claude_runtime_protocol::CAP_CLI_TURNS;
const CAP_HUMAN_ORIGIN: &str = claude_runtime_protocol::CAP_HUMAN_ORIGIN;
const CAP_BACKGROUND_TASKS: &str = claude_runtime_protocol::CAP_BACKGROUND_TASKS;
const CAP_WORKFLOW_PROGRESS: &str = claude_runtime_protocol::CAP_WORKFLOW_PROGRESS;
const CAP_TASK_CONTROL: &str = claude_runtime_protocol::CAP_TASK_CONTROL;
const CAP_TASK_OUTPUT: &str = claude_runtime_protocol::CAP_TASK_OUTPUT;
const CAP_SUBAGENT_FRAMES: &str = claude_runtime_protocol::CAP_SUBAGENT_FRAMES;
const CAP_SESSION_NOTICES: &str = claude_runtime_protocol::CAP_SESSION_NOTICES;
const CAP_CONVERSATION_RESET: &str = claude_runtime_protocol::CAP_CONVERSATION_RESET;
const CAP_STRUCTURED_QUESTIONS_V1: &str = claude_runtime_protocol::CAP_STRUCTURED_QUESTIONS_V1;
const CAP_CLIENT_TOOLS: &str = claude_runtime_protocol::CAP_CLIENT_TOOLS;
const CAP_SET_MODEL: &str = claude_runtime_protocol::CAP_SET_MODEL;
const CAP_LIST_MODELS: &str = claude_runtime_protocol::CAP_LIST_MODELS;
const CAP_APPLY_EFFORT: &str = claude_runtime_protocol::CAP_APPLY_EFFORT;
const CAP_TASK_OUTPUT_RANGE: &str = claude_runtime_protocol::CAP_TASK_OUTPUT_RANGE;
const PERMISSION_MODE_BYPASS: &str = "bypass";
const PERMISSION_MODE_INTERACTIVE: &str = "interactive";

/// Whether THIS client can actually drive resume/fork end to end. Each stays `false` until the
/// change that implements it lands; flipping one here is the single switch that turns the feature
/// on, and it must not be flipped before the call does something real. Resume has since been
/// through that gate (hence `true` below); fork has not.
///
/// A capability is the INTERSECTION of what the provider advertises and what this side implements.
/// Reporting the provider's advertisement alone would put a Resume control in the UI the moment the
/// sidecar learned the word, while this client still returned `UnsupportedCapability` -- a button
/// that cannot work is worse than no button, and it is exactly the "capability advertisement is a
/// contract" failure this design is trying to avoid, just with the lie on the client side.
pub const CLIENT_IMPLEMENTS_RESUME: bool = true;
/// Still false: the wire carries `fork`, and `build_create_request` can set it, but nothing in this
/// client asks for a fork and nothing has verified one end to end. Flip it in the change that does
/// both, not before.
const CLIENT_IMPLEMENTS_FORK: bool = false;
/// True since 2026-09-27: a provider prompt is translated with its origin
/// (`translate::provider_prompt_of`), answered by `eitri_core::agent_backend` on these rules
/// (bypass allows it unless the user's own ask rule forced it; Auto allows it only after the human
/// approved the same call), and drawn as a card with the CLI's own reason otherwise. Before that a
/// session that asked for these would have had its prompts judged as if the gate had asked.
const CLIENT_IMPLEMENTS_PROVIDER_PROMPTS: bool = true;
/// True since 2026-10-02: a session asks for the CLI's own auto mode, a gate request on it is
/// answered `PermissionDecision::Defer` by `eitri_core::agent_backend` when the tab is in Auto and no
/// saved rule allows it, the CLI's `SessionReady` report is judged against the request
/// (`translate::translate`), and its refusals are shown on their call (`PermissionDenied`).
const CLIENT_IMPLEMENTS_CLI_AUTO_MODE: bool = true;
/// Whether sessions ask the sidecar to leave background tasks running when a turn is interrupted
/// (`ClaudeHostPolicy.per_task_stop`, offered with `task_control`). The panel's task list can
/// stop one task explicitly, so interrupting a turn can safely spare its background work.
pub const CLIENT_IMPLEMENTS_PER_TASK_STOP: bool = true;

fn per_task_stop_policy(capable: bool, readiness: Option<bool>) -> bool {
    capable && readiness.unwrap_or(CLIENT_IMPLEMENTS_PER_TASK_STOP)
}
/// Whether sessions ask the sidecar to hand Claude's questions to this host
/// (`CreateSessionRequest.client_interactions.questions`, offered with `structured_questions_v1`).
/// The panel has an explicit question dock; permission policy never supplies its answers.
pub const CLIENT_IMPLEMENTS_QUESTIONS: bool = true;

/// Whether sessions on this sidecar ask for the CLI's own prompts: the intersection of
/// what the handshake advertised and what this client implements, like every capability here. A
/// sidecar without it is sent nothing new and behaves exactly as it did before the capability existed -- the CLI's own
/// asks are refused headlessly, with nobody told.
///
/// Not a `ProviderCapabilities` field, for the reason `splits_messages` is not: no caller outside
/// this provider branches on it. Every session this client creates is gated, so it is asked
/// for on every one of them.
fn provider_prompts_from_handshake(response: &HandshakeResponse) -> bool {
    CLIENT_IMPLEMENTS_PROVIDER_PROMPTS
        && response
            .capabilities
            .iter()
            .any(|c| c == CAP_PROVIDER_PERMISSION_PROMPTS)
}

/// Only an explicit capability opts this sidecar into Claude Code's preset; a protocol minor
/// alone cannot promise that the runtime implements the prompt.
fn claude_code_prompt_from_handshake(response: &HandshakeResponse) -> bool {
    response
        .capabilities
        .iter()
        .any(|c| c == claude_runtime_protocol::CAP_SYSTEM_PROMPT_PRESET)
}

/// Derives what the provider can actually do from what it actually said. Pure, so it is unit-tested
/// against real handshake shapes without a sidecar process.
///
/// Note what this deliberately does NOT do: it never returns `resume: true` because a
/// `ResumeSessionRequest` type exists in this crate, or because `resume_session` is a method on the
/// trait. Both of those have been true since Phase 3 while the wire protocol had no resume at all.
fn capabilities_from_handshake(response: &HandshakeResponse) -> ProviderCapabilities {
    let has = |name: &str| response.capabilities.iter().any(|c| c == name);
    ProviderCapabilities {
        resume: CLIENT_IMPLEMENTS_RESUME && has(CAP_RESUME_SESSION),
        fork: CLIENT_IMPLEMENTS_FORK && has(CAP_FORK_SESSION),
        interrupt: has(CAP_INTERRUPT_TURN),
        bypass_permission_mode: response.permission_modes.iter().any(|m| m == PERMISSION_MODE_BYPASS),
        interactive_permission_mode: response
            .permission_modes
            .iter()
            .any(|m| m == PERMISSION_MODE_INTERACTIVE),
        // Both or neither: see `ProviderCapabilities::cli_auto_mode`.
        cli_auto_mode: CLIENT_IMPLEMENTS_CLI_AUTO_MODE && has(CAP_CLI_AUTO_MODE) && has(CAP_PERMISSION_DEFER),
        steer: has(CAP_STEER_TURN),
        // Reported as offered; whether a session opts into `per_task_stop` or questions is
        // `CLIENT_IMPLEMENTS_PER_TASK_STOP` and `CLIENT_IMPLEMENTS_QUESTIONS`, not these flags.
        cli_turns: has(CAP_CLI_TURNS),
        human_origin: has(CAP_HUMAN_ORIGIN),
        background_tasks: has(CAP_BACKGROUND_TASKS),
        workflow_progress: has(CAP_WORKFLOW_PROGRESS),
        task_control: has(CAP_TASK_CONTROL),
        task_output: has(CAP_TASK_OUTPUT),
        subagent_frames: has(CAP_SUBAGENT_FRAMES),
        session_notices: has(CAP_SESSION_NOTICES),
        conversation_reset: has(CAP_CONVERSATION_RESET),
        structured_questions_v1: has(CAP_STRUCTURED_QUESTIONS_V1),
        client_tools: has(CAP_CLIENT_TOOLS),
        direct_settings: has(CAP_SET_MODEL) && has(CAP_LIST_MODELS) && has(CAP_APPLY_EFFORT),
        task_output_range: has(CAP_TASK_OUTPUT_RANGE),
    }
}

/// What every session this provider creates asks the CLI to run in under the gate.
fn requested_cli_mode(capabilities: &ProviderCapabilities) -> crate::RequestedCliMode {
    if capabilities.cli_auto_mode {
        crate::RequestedCliMode::Auto
    } else {
        crate::RequestedCliMode::Default
    }
}

fn info_from_handshake(
    response: &HandshakeResponse,
    build_description: Option<String>,
    startup_diagnostics: Vec<String>,
) -> ProviderInfo {
    ProviderInfo {
        sidecar_version: response.sidecar_version.clone(),
        claude_agent_sdk_version: response.claude_agent_sdk_version.clone(),
        actual_claude_code_version: response.actual_claude_code_version.clone(),
        protocol_major: response.protocol_major,
        protocol_minor: response.protocol_minor,
        advertised_capabilities: response.capabilities.clone(),
        advertised_permission_modes: response.permission_modes.clone(),
        event_buffer_policy: response.event_buffer_policy.clone(),
        build_description,
        startup_diagnostics,
        configuration_home: None,
        sidecar_update: None,
    }
}

/// Whether a sidecar advertising `capabilities` may run as a selected update. `Err` says which
/// capability it lacks. A rule for choosing an update only: a sidecar that is not an update, and one a
/// move brings along, run with whatever they offer.
pub fn check_update_floor(capabilities: &[String]) -> Result<(), String> {
    match missing_required(capabilities.iter().map(String::as_str)) {
        Some(capability) => Err(format!("lacks {capability}")),
        None => Ok(()),
    }
}

/// A sidecar that will not serve this client's protocol major. Shows as `text`, as any handshake error
/// does; `words` are what a line about a sidecar update quotes.
#[derive(Debug)]
struct RefusedThisClient {
    text: String,
    words: String,
}

impl std::fmt::Display for RefusedThisClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::error::Error for RefusedThisClient {}

/// Maps the sidecar proto's own `ErrorCode` onto this crate's provider-neutral mirror. Exhaustive
/// by construction: adding a variant to the proto makes this stop compiling rather than silently
/// collapsing a new, possibly-serious code into `Unspecified`.
fn provider_error_code(code: ProtoErrorCode) -> ProviderErrorCode {
    match code {
        ProtoErrorCode::Unspecified => ProviderErrorCode::Unspecified,
        ProtoErrorCode::IncompatibleProtocol => ProviderErrorCode::IncompatibleProtocol,
        ProtoErrorCode::UnsupportedCliVersion => ProviderErrorCode::UnsupportedCliVersion,
        ProtoErrorCode::SessionNotFound => ProviderErrorCode::SessionNotFound,
        ProtoErrorCode::TurnAlreadyActive => ProviderErrorCode::TurnAlreadyActive,
        ProtoErrorCode::NoActiveTurn => ProviderErrorCode::NoActiveTurn,
        ProtoErrorCode::PermissionNotFound => ProviderErrorCode::PermissionNotFound,
        ProtoErrorCode::PermissionAlreadyResolved => ProviderErrorCode::PermissionAlreadyResolved,
        ProtoErrorCode::IdempotencyConflict => ProviderErrorCode::IdempotencyConflict,
        ProtoErrorCode::EventGap => ProviderErrorCode::EventGap,
        ProtoErrorCode::InvalidConfiguration => ProviderErrorCode::InvalidConfiguration,
        ProtoErrorCode::ProviderUnavailable => ProviderErrorCode::ProviderUnavailable,
        ProtoErrorCode::ProviderProtocolError => ProviderErrorCode::ProviderProtocolError,
        ProtoErrorCode::DeadlineExceeded => ProviderErrorCode::DeadlineExceeded,
        ProtoErrorCode::TurnMismatch => ProviderErrorCode::TurnMismatch,
        ProtoErrorCode::SteerUnavailable => ProviderErrorCode::SteerUnavailable,
        ProtoErrorCode::TaskNotFound => ProviderErrorCode::TaskNotFound,
        ProtoErrorCode::TaskOutputUnavailable => ProviderErrorCode::TaskOutputUnavailable,
        ProtoErrorCode::InteractionNotFound => ProviderErrorCode::InteractionNotFound,
        ProtoErrorCode::InteractionAnswerRefused => ProviderErrorCode::InteractionAnswerRefused,
        ProtoErrorCode::ClientToolCallNotFound => ProviderErrorCode::ClientToolCallNotFound,
        ProtoErrorCode::ClientToolCallAlreadyAnswered => ProviderErrorCode::ClientToolCallAlreadyAnswered,
    }
}

/// Picks out the lines of a sidecar's startup stderr worth showing a human.
///
/// The sidecar logs plenty that is pure noise to a user. What matters here is its own compatibility
/// diagnostic -- the "this CLI version is inside the supported range but has not been tested"
/// warning, which is the single most likely explanation for otherwise-inexplicable behavior and
/// which no other channel carries (it is deliberately not a protocol field).
fn startup_diagnostics_from_stderr(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| line.contains("CLI version diagnostic"))
        .map(|line| line.trim().to_string())
        .collect()
}

/// The command is available only on installs with the setup helper beside the running binary.
pub fn untested_note_applies(stderr_lines: &[String], exe_dir: Option<&std::path::Path>) -> bool {
    sidecar_command_available(exe_dir) && stderr_lines.iter().any(|line| line.contains("CLI version diagnostic"))
}

fn sidecar_command_available(exe_dir: Option<&std::path::Path>) -> bool {
    exe_dir.is_some_and(|dir| dir.join("eitri-setup").is_file())
}

fn with_failure_notes(error: std::io::Error, exe_dir: Option<&std::path::Path>) -> std::io::Error {
    let mut text = error.to_string();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let untested = untested_note_applies(&lines, exe_dir);
    let refused = sidecar_command_available(exe_dir)
        && text.contains("refusing to start")
        && contract::newer_sidecar_may_help(&text);
    for (applies, note) in [(untested, UNTESTED_CLAUDE_NOTE), (refused, REFUSED_CLAUDE_NOTE)] {
        if applies && !text.contains(note) {
            text.push('\n');
            text.push_str(note);
        }
    }
    if text == error.to_string() {
        error
    } else {
        std::io::Error::new(error.kind(), text)
    }
}

/// The handshake facts reported by a sidecar that passes the update capability floor.
pub struct ProbeReport {
    pub sidecar_version: String,
    pub protocol_major: u32,
    pub protocol_minor: u32,
}

/// Starts the named artifact through the normal spawn and handshake, then ends it without a session.
pub fn probe(path: &std::path::Path) -> Result<ProbeReport, String> {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let sidecar = spawn::spawn_program(&instance_id, path).map_err(|error| spawn::start_failure_reason(&error))?;
    let provider = ClaudeSidecarProvider::around(sidecar).map_err(|error| error.to_string())?;
    check_update_floor(&provider.info.advertised_capabilities)?;
    Ok(ProbeReport {
        sidecar_version: provider.info.sidecar_version.clone(),
        protocol_major: provider.info.protocol_major,
        protocol_minor: provider.info.protocol_minor,
    })
}

// `RuntimeServiceClient<Channel>` is already cheaply `Clone` (tonic's generated clients wrap a
// `Channel`, designed for exactly this "clone one per concurrent call" pattern -- cloning shares
// the same underlying HTTP/2 connection). No `Mutex` is needed to clone it from `&self`; a `Mutex`
// here would be protecting nothing (`.clone()` never needs exclusive access).
pub struct ClaudeSidecarProvider {
    /// Behind a lock because a move hands it on (or takes it up) while the provider is shared with
    /// its conversation's ingest thread.
    sidecar: Mutex<SpawnedSidecar>,
    runtime: RuntimeThread,
    client: RuntimeServiceClient<Channel>,
    events: Arc<EventQueue>,
    backpressure: Arc<BackpressureCounters>,
    /// Captured once at `connect()` from the real `HandshakeResponse`, which this provider used to
    /// discard entirely -- it checked only that the RPC succeeded. Without it there was no truthful
    /// source for any capability at all, so `capabilities()` returned a hardcoded literal.
    capabilities: ProviderCapabilities,
    info: ProviderInfo,
    /// Whether this sidecar advertised `text_delta_message_id` at handshake. Not a
    /// `ProviderCapabilities` field: no caller outside this provider branches on it, it only feeds
    /// `start_watching`'s own `MessageSplit`.
    splits_messages: bool,
    /// `provider_prompts_from_handshake`, captured at `connect()`: whether `create_session` and
    /// `resume_session` ask this sidecar for the CLI's own permission prompts.
    provider_prompts: bool,
    /// Whether this peer offers Claude Code's preset for fresh and resumed sessions.
    claude_code_prompt: bool,
    /// The handshake's limit on one request; 0 when the peer stated none. Read by the answer path.
    max_message_bytes: u64,
    #[cfg(feature = "test-support")]
    questions_override: Option<bool>,
    /// The watch task of each session opened, newest last; a move cancels the source's. Only a move
    /// reads it, so a build without moves keeps none.
    #[cfg(target_os = "linux")]
    watches: Mutex<Vec<Watch>>,
    /// A sidecar taken over from another process and not committed yet: the session whose watch the
    /// commit opens, and where it goes on from. Nothing is watched, read or answered until then.
    parked: Mutex<Option<ParkedWatch>>,
    /// Set once this process handed the sidecar to another: it acts for the session no more.
    released: AtomicBool,
    #[cfg(feature = "test-support")]
    per_task_stop_override: Option<bool>,
}

/// One session's watch task.
#[cfg(target_os = "linux")]
struct Watch {
    session_id: String,
    task: tokio::task::JoinHandle<()>,
}

/// The watch a taken-over sidecar opens on its commit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct ParkedWatch {
    session_id: String,
    mark: WatchMark,
}

/// What one session's watch keeps between events, reconnects included.
struct WatchState {
    tracker: watch::SequenceTracker,
    /// One per session, outside the reconnect loop: a reconnect must not forget which message id it
    /// last saw, and `tracker` already drops `Duplicate` events before they reach it, so a replay after
    /// a reconnect cannot split a message twice.
    split: translate::MessageSplit,
    /// Per session, like `split`: a stricter or unreported CLI permission mode is noted once, not on
    /// every turn's `SessionReady`.
    cli_mode_note: translate::CliModeNote,
    /// Counts only CONSECUTIVE failed attempts: a reconnect that actually delivered a new event
    /// resets it, so the budget bounds "getting nowhere", not "reconnecting at all".
    consecutive_failures: usize,
    /// The questions this watch delivered an ask or a settlement for, so the reconcile after a re-opened
    /// stream adds only questions it never saw (`watch::reconcile_questions`).
    questions: watch::QuestionLedger,
}

/// Where a session's watch stood just after one event: the event's sequence, and the state its
/// translation left behind. Another process's watch of the same session goes on from it, so the
/// first event it reads after that sequence is translated as this one would have translated it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WatchMark {
    sequence: u64,
    /// The message id of the last text delta translated, which decides whether the next delta opens
    /// a new assistant message.
    last_text: Option<String>,
    /// The same for each subagent's open reply, by the call that started it, the longest silent first.
    child_replies: Vec<(String, String)>,
    /// Whether the session's CLI-mode line was printed.
    cli_mode_noted: bool,
}

impl WatchState {
    fn fresh(splits_messages: bool, requested: crate::RequestedCliMode) -> Self {
        WatchState {
            tracker: watch::SequenceTracker::new(),
            split: translate::MessageSplit::new(splits_messages),
            cli_mode_note: translate::CliModeNote::new(requested),
            consecutive_failures: 0,
            questions: watch::QuestionLedger::default(),
        }
    }

    #[cfg(target_os = "linux")]
    fn resumed(mark: &WatchMark, splits_messages: bool, requested: crate::RequestedCliMode) -> Self {
        WatchState {
            tracker: watch::SequenceTracker::after(mark.sequence),
            split: translate::MessageSplit::resuming(
                splits_messages,
                mark.last_text.clone(),
                mark.child_replies.clone(),
            ),
            cli_mode_note: translate::CliModeNote::resuming(requested, mark.cli_mode_noted),
            consecutive_failures: 0,
            questions: watch::QuestionLedger::default(),
        }
    }

    /// The watch's position after the event it delivered last. Destructures the state without a rest
    /// pattern, so a field added to it has to say whether another process's watch needs it.
    fn mark(&self) -> WatchMark {
        let WatchState {
            tracker,
            split,
            cli_mode_note,
            // Not carried: a watch taken up elsewhere starts with a whole reconnect budget.
            consecutive_failures: _,
            // Not carried: the other process's projection holds the questions already asked, and a
            // reconcile there that adds one of them again replaces its record with the same set.
            questions: _,
        } = self;
        WatchMark {
            sequence: tracker.after_sequence(),
            last_text: split.last_text().map(str::to_string),
            child_replies: split.child_replies().to_vec(),
            cli_mode_noted: cli_mode_note.noted(),
        }
    }
}

/// What a handshake tells a provider about its sidecar.
struct Handshaken {
    client: RuntimeServiceClient<Channel>,
    capabilities: ProviderCapabilities,
    info: ProviderInfo,
    splits_messages: bool,
    provider_prompts: bool,
    claude_code_prompt: bool,
    max_message_bytes: u64,
}

/// Connects to the sidecar's socket and runs the real `Handshake`, refusing a socket served by another
/// user (and, when `expected_pid` is given, by another process than that one) and an incompatible
/// protocol major. `exe_dir` is the directory of the running binary, which decides whether the
/// sidecar commands named in the startup notes exist next to it.
fn handshake_with(
    runtime: &RuntimeThread,
    sidecar: &SpawnedSidecar,
    expected_pid: Option<u32>,
    exe_dir: Option<&std::path::Path>,
) -> std::io::Result<Handshaken> {
    let socket_path = sidecar.socket_path.clone();
    let client = runtime
        .block_on(
            async move {
                let channel = Endpoint::try_from("http://[::]:50051")?
                    .connect_with_connector(service_fn(move |_: Uri| {
                        let socket_path = socket_path.clone();
                        async move {
                            let stream = UnixStream::connect(socket_path).await?;
                            let peer = stream.peer_cred()?;
                            crate::socket_path::require_own_peer(peer.uid())?;
                            if let Some(expected) = expected_pid {
                                if peer.pid().map(|pid| pid as u32) != Some(expected) {
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::PermissionDenied,
                                        "the sidecar socket is served by another process than the one handed over",
                                    ));
                                }
                            }
                            Ok::<_, std::io::Error>(TokioIo::new(stream))
                        }
                    }))
                    .await?;
                Ok::<_, tonic::transport::Error>(RuntimeServiceClient::new(channel))
            },
            UNARY_RPC_TIMEOUT,
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::TimedOut, e.to_string()))?
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e.to_string()))?;
    let handshake = runtime
        .block_on(
            {
                let mut client = client.clone();
                async move {
                    client
                        .handshake(HandshakeRequest {
                            client_protocol_major: CLIENT_PROTOCOL_MAJOR,
                        })
                        .await
                }
            },
            UNARY_RPC_TIMEOUT,
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::TimedOut, e.to_string()))?
        .map_err(|status| {
            let text = format!("handshake failed: {status}");
            let incompatible =
                error_detail(&status).is_some_and(|d| d.code == ProtoErrorCode::IncompatibleProtocol as i32);
            if incompatible {
                std::io::Error::other(RefusedThisClient {
                    text,
                    words: status.message().to_string(),
                })
            } else {
                std::io::Error::other(text)
            }
        })?
        .into_inner();

    // An incompatible major is refused, minor differences are tolerated
    // (protobuf unknown-field rules cover the new fields a higher minor may add).
    //
    // Against a CONFORMING peer this branch is unreachable in both directions, and the comment
    // here used to claim otherwise ("this catches a sidecar that is too new"). It does not:
    // `runtimeServiceImpl.ts`'s handshake handler throws `INCOMPATIBLE_PROTOCOL` whenever
    // `clientProtocolMajor !== PROTOCOL_MAJOR` and otherwise answers with its own
    // `PROTOCOL_MAJOR`, so version skew either way is refused by the peer and arrives at the
    // `?` above as a failed RPC -- never here with a mismatched number in hand. What survives
    // is the check's real job: a backstop against a NON-conforming peer, one that answers a
    // major it did not just require (a broken build, a different implementation of the service,
    // or -- since proto3 has no presence on a scalar -- a response that simply omits the field
    // and so arrives as 0). Cheap, and the alternative is issuing commands to a peer whose
    // wire contract this client cannot name.
    if handshake.protocol_major != CLIENT_PROTOCOL_MAJOR {
        let text = format!(
            "claude-sidecar speaks protocol major {} but this client speaks {CLIENT_PROTOCOL_MAJOR}; \
             refusing to continue rather than issuing commands it may silently misread",
            handshake.protocol_major
        );
        return Err(std::io::Error::other(RefusedThisClient {
            words: text.clone(),
            text,
        }));
    }

    let capabilities = capabilities_from_handshake(&handshake);
    let splits_messages = handshake.capabilities.iter().any(|c| c == CAP_TEXT_DELTA_MESSAGE_ID);
    let provider_prompts = provider_prompts_from_handshake(&handshake);
    let claude_code_prompt = claude_code_prompt_from_handshake(&handshake);
    if !provider_prompts {
        // Said once, on stderr only: an older sidecar is not a fault, but a bypass on it is
        // narrower than the CLI's own bypassPermissions, and this is the line that says why.
        eprintln!(
            "agent: ClaudeSidecarProvider: this sidecar does not offer {CAP_PROVIDER_PERMISSION_PROMPTS}; \
             a call the CLI itself asks about after the gate allowed it (a write under .git/ or \
             .claude/) is refused with nobody asked"
        );
    }
    // Warnings only. The checkout DESCRIPTION is always present and travels separately as
    // `build_description`; mixing it in here made `startup_diagnostics` never empty, which lit
    // a permanent warning indicator in the UI for every healthy session.
    let mut diagnostics = sidecar.build_warnings.clone();
    if !claude_code_prompt {
        diagnostics.push(format!(
            "Claude Code's system prompt unavailable: this sidecar does not offer system_prompt_preset, so Claude runs with the Agent SDK's minimal prompt. Build the sidecar for Verdandi {BUNDLED_SIDECAR_REVISION}."
        ));
    }
    let stderr = sidecar.stderr_tail();
    diagnostics.extend(startup_diagnostics_from_stderr(&stderr));
    if untested_note_applies(&stderr, exe_dir) {
        diagnostics.push(UNTESTED_CLAUDE_NOTE.to_string());
    }
    let info = info_from_handshake(&handshake, Some(sidecar.build_description.clone()), diagnostics);
    for diagnostic in &info.startup_diagnostics {
        eprintln!("agent: ClaudeSidecarProvider: {diagnostic}");
    }

    Ok(Handshaken {
        client,
        capabilities,
        info,
        splits_messages,
        provider_prompts,
        claude_code_prompt,
        max_message_bytes: handshake.max_message_bytes,
    })
}

impl ClaudeSidecarProvider {
    /// Spawns a fresh sidecar and connects to it, performing the real `Handshake` before returning
    /// -- a provider that constructs successfully has already proven it can talk to a real,
    /// compatible sidecar, not just that a process happened to start.
    ///
    /// A selected update comes first. If it cannot serve this tab the tab runs what the search finds
    /// and says why, or fails with that sidecar's own error followed by the update's.
    pub fn connect(instance_id: &str) -> std::io::Result<Self> {
        Self::connect_after(instance_id, spawn::selected_update(), || spawn::spawn(instance_id)).map_err(|error| {
            let exe = std::env::current_exe().ok();
            with_failure_notes(error, exe.as_deref().and_then(std::path::Path::parent))
        })
    }

    fn connect_after(
        instance_id: &str,
        choice: UpdateChoice,
        bundled: impl FnOnce() -> std::io::Result<SpawnedSidecar>,
    ) -> std::io::Result<Self> {
        let trouble = match choice {
            UpdateChoice::NoUpdate => return Self::around(bundled()?),
            UpdateChoice::Use { version, path } => match Self::try_update(instance_id, &version, &path) {
                Ok(provider) => return Ok(provider),
                Err(trouble) => {
                    spawn::remember_failed_update(&version);
                    trouble
                }
            },
            UpdateChoice::Unusable(what) => UpdateTrouble::Unusable(what),
            UpdateChoice::FailedHere(version) => UpdateTrouble::FailedHere(version),
        };
        match bundled().and_then(Self::around) {
            Ok(mut provider) => {
                let line = trouble.fallback_line();
                eprintln!("agent: ClaudeSidecarProvider: {line}");
                provider.info.startup_diagnostics.insert(0, line);
                Ok(provider)
            }
            Err(error) => Err(std::io::Error::new(
                error.kind(),
                format!("{error}\n{}", trouble.also_failed_line()),
            )),
        }
    }

    /// A provider on the update at `path`, or why not. A sidecar that fails here is ended here, through
    /// its holder's clean end, before anything else starts on the same socket path.
    fn try_update(instance_id: &str, version: &str, path: &std::path::Path) -> Result<Self, UpdateTrouble> {
        let did_not_start = |reason: String| UpdateTrouble::DidNotStart {
            version: version.to_string(),
            reason: spawn::quotable(reason),
        };
        let sidecar = spawn::spawn_program(instance_id, path)
            .map_err(|error| did_not_start(spawn::start_failure_reason(&error)))?;
        let runtime = RuntimeThread::spawn();
        let mut handshaken =
            handshake_with(&runtime, &sidecar, None, spawn::running_exe_dir().as_deref()).map_err(|error| {
                if let Some(refused) = error
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<RefusedThisClient>())
                {
                    return UpdateTrouble::Refused {
                        version: version.to_string(),
                        words: spawn::quotable(refused.words.clone()),
                    };
                }
                let silent = if error.kind() == std::io::ErrorKind::TimedOut {
                    format!("no answer in {} s", UNARY_RPC_TIMEOUT.as_secs())
                } else {
                    error.to_string()
                };
                did_not_start(spawn::failure_reason(&sidecar.stderr_tail(), silent))
            })?;
        check_update_floor(&handshaken.info.advertised_capabilities).map_err(|lacks| UpdateTrouble::Lacks {
            version: version.to_string(),
            lacks,
        })?;
        handshaken.info.sidecar_update = Some(version.to_string());
        Ok(Self::holding(sidecar, runtime, handshaken))
    }

    /// [`Self::connect`] with what the files on disk would say given by the caller: `update` is the
    /// selected update at `version`, unless that version already failed in this process, and `bundled`
    /// is what the search would find.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn connect_trying_update(
        instance_id: &str,
        update: &std::path::Path,
        version: &str,
        bundled: &std::path::Path,
    ) -> std::io::Result<Self> {
        let choice = if spawn::failed_updates().contains(version) {
            UpdateChoice::FailedHere(version.to_string())
        } else {
            UpdateChoice::Use {
                version: version.to_string(),
                path: update.to_path_buf(),
            }
        };
        Self::connect_after(instance_id, choice, || spawn::spawn_program(instance_id, bundled))
    }

    /// [`Self::connect`] with `program` started as the sidecar, found by no search and built by
    /// nothing: the crate's tests start a stand-in this way.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn connect_to_program(instance_id: &str, program: &std::path::Path) -> std::io::Result<Self> {
        // A stand-in's socket and records go in the test's own root, never the shared temp dir.
        crate::state_dirs::redirect_state_to_a_test_root();
        Self::around(spawn::spawn_program(instance_id, program)?)
    }

    /// [`Self::connect_to_program`] for a tab of a binary installed in `exe_dir`, including its remedies.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn connect_to_program_in(
        instance_id: &str,
        program: &std::path::Path,
        exe_dir: &std::path::Path,
    ) -> std::io::Result<Self> {
        crate::state_dirs::redirect_state_to_a_test_root();
        spawn::spawn_program(instance_id, program)
            .and_then(|sidecar| Self::around_in(sidecar, Some(exe_dir)))
            .map_err(|error| with_failure_notes(error, Some(exe_dir)))
    }

    /// A provider driving `sidecar`, which this process started and holds.
    fn around(sidecar: SpawnedSidecar) -> std::io::Result<Self> {
        let exe = std::env::current_exe().ok();
        Self::around_in(sidecar, exe.as_deref().and_then(std::path::Path::parent))
    }

    fn around_in(sidecar: SpawnedSidecar, exe_dir: Option<&std::path::Path>) -> std::io::Result<Self> {
        let runtime = RuntimeThread::spawn();
        let handshaken = handshake_with(&runtime, &sidecar, None, exe_dir).map_err(|error| {
            let stderr = sidecar.stderr_tail();
            if stderr.is_empty() {
                error
            } else {
                std::io::Error::new(
                    error.kind(),
                    format!("{error}; its stderr said:\n{}", stderr.join("\n")),
                )
            }
        })?;
        Ok(Self::holding(sidecar, runtime, handshaken))
    }

    fn holding(sidecar: SpawnedSidecar, runtime: RuntimeThread, handshaken: Handshaken) -> Self {
        let Handshaken {
            client,
            capabilities,
            info,
            splits_messages,
            provider_prompts,
            claude_code_prompt,
            max_message_bytes,
        } = handshaken;
        Self {
            sidecar: Mutex::new(sidecar),
            runtime,
            client,
            events: Arc::new(EventQueue::default()),
            backpressure: Arc::new(BackpressureCounters::default()),
            capabilities,
            info,
            splits_messages,
            provider_prompts,
            claude_code_prompt,
            max_message_bytes,
            #[cfg(feature = "test-support")]
            questions_override: None,
            #[cfg(target_os = "linux")]
            watches: Mutex::new(Vec::new()),
            parked: Mutex::new(None),
            released: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            per_task_stop_override: None,
        }
    }

    /// Overrides host readiness for scripted interaction tests; the peer must still offer questions.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_questions_override_for_tests(&mut self, enabled: bool) {
        self.questions_override = Some(enabled);
    }

    fn question_policy(&self) -> QuestionPolicy {
        #[cfg(feature = "test-support")]
        let readiness = self.questions_override;
        #[cfg(not(feature = "test-support"))]
        let readiness = None;
        QuestionPolicy::resolve(self.capabilities.structured_questions_v1, readiness)
    }

    /// A private harness choice, made before creating a session; it never overrides a missing capability.
    #[cfg(feature = "test-support")]
    pub fn set_per_task_stop(&mut self, enabled: bool) {
        self.per_task_stop_override = Some(enabled);
    }

    fn per_task_stop(&self) -> bool {
        #[cfg(feature = "test-support")]
        let readiness = self.per_task_stop_override;
        #[cfg(not(feature = "test-support"))]
        let readiness = None;
        per_task_stop_policy(self.capabilities.task_control, readiness)
    }

    /// Schedules a bounded unary RPC and returns its receiver immediately. The runtime's task
    /// awaits network I/O and the timeout without blocking either its thread or the UI thread.
    fn task_rpc<F, T>(&self, future: F) -> crate::TaskReply<T>
    where
        F: std::future::Future<Output = Result<T, ProviderError>> + Send + 'static,
        T: Send + 'static,
    {
        self.rpc_within(UNARY_RPC_TIMEOUT, future)
    }

    fn rpc_within<F, T>(&self, limit: Duration, future: F) -> crate::TaskReply<T>
    where
        F: std::future::Future<Output = Result<T, ProviderError>> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        self.runtime.handle().spawn(async move {
            let result = tokio::time::timeout(limit, future)
                .await
                .unwrap_or(Err(ProviderError::Timeout));
            let _ = tx.send(result);
        });
        rx
    }

    /// What the event stream is costing right now: queue depth, and how far behind production
    /// delivery has fallen. See `BackpressureStats` for why losslessness alone is not the question.
    pub fn backpressure_stats(&self) -> BackpressureStats {
        BackpressureStats {
            pending_events: self.events.len(),
            events_received: self.backpressure.events_received.load(Ordering::Relaxed),
            max_delivery_lag_ms: self.backpressure.max_delivery_lag_ms.load(Ordering::Relaxed),
            last_delivery_lag_ms: self.backpressure.last_delivery_lag_ms.load(Ordering::Relaxed),
            watch_reconnects: self.backpressure.watch_reconnects.load(Ordering::Relaxed),
        }
    }

    /// The sidecar process's recent stderr, for diagnosing a provider that connected but is
    /// misbehaving. `info().startup_diagnostics` is the curated subset worth showing a user.
    pub fn sidecar_stderr_tail(&self) -> Vec<String> {
        lock(&self.sidecar).stderr_tail()
    }

    /// The unix socket the sidecar serves on, for a test that opens a watch of its own beside the provider's.
    /// Owned rather than borrowed: a move can hand the sidecar on, so the path lives behind the lock.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn sidecar_socket_path(&self) -> std::path::PathBuf {
        lock(&self.sidecar).socket_path.clone()
    }

    /// What this provider's watch was told about its sessions, which decides how their wire events translate:
    /// recorded beside a session's wire events so [`replay_frames`] can translate them the same way.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn replay_settings(&self) -> ReplaySettings {
        ReplaySettings {
            splits_messages: self.splits_messages,
            requested_cli_mode: requested_cli_mode(&self.capabilities),
        }
    }

    /// The OS pid of the sidecar process this provider spawned and owns.
    ///
    /// Exists so an orphan check can assert on the ONE process this provider is responsible for.
    /// Never match sidecar or `claude` processes by name for that purpose: Claude Code sessions on a
    /// developer machine are themselves processes named `claude`, so a name-matched search sweeps up
    /// the session running the test. Capture this pid, check this pid.
    pub fn sidecar_pid(&self) -> u32 {
        lock(&self.sidecar).pid()
    }

    fn map_status(&self, status: tonic::Status) -> ProviderError {
        map_status(status)
    }

    fn run_unary<F, T>(&self, future: F) -> Result<T, ProviderError>
    where
        F: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>> + Send + 'static,
        T: Send + 'static,
    {
        let result = self
            .runtime
            .block_on(future, UNARY_RPC_TIMEOUT)
            .map_err(|_| ProviderError::Timeout)?;
        match result {
            Ok(response) => Ok(response.into_inner()),
            Err(status) => Err(self.map_status(status)),
        }
    }

    /// Issues one `CreateSession` (fresh or resuming) and starts watching the resulting session.
    /// Both paths must open the watch stream, and forgetting it on one of them would produce a
    /// session that accepts commands and reports nothing.
    fn open_session(&self, mut request: ProtoCreateSessionRequest) -> Result<String, ProviderError> {
        self.require_driving()?;
        if let Some(policy) = request.policy.as_mut() {
            policy.per_task_stop = self.per_task_stop();
        }
        let mut client = self.client.clone();
        let response = self.run_unary(async move { client.create_session(request).await })?;
        self.start_watching(response.session_id.clone());
        Ok(response.session_id)
    }

    fn require_interactive(&self) -> Result<(), ProviderError> {
        require_interactive(&self.capabilities, &self.info)
    }

    /// Refuses a command while another process drives the session: after this one handed the sidecar
    /// on, and while it has taken one over and not committed. Nothing is sent to the sidecar then.
    fn require_driving(&self) -> Result<(), ProviderError> {
        let why = if self.released.load(Ordering::SeqCst) {
            "this chat moved to the other window"
        } else if lock(&self.parked).is_some() {
            "this chat is still moving to this window"
        } else if self.events.frozen() {
            // A message steered now would be open on this side only, and the taking side's gate would
            // drop its delivery.
            "this chat is moving to another window"
        } else {
            return Ok(());
        };
        Err(ProviderError::Provider {
            code: ProviderErrorCode::ProviderUnavailable,
            message: why.to_string(),
        })
    }

    /// Watches one session's event stream for as long as the session lives, reconnecting across
    /// transient transport failures and reporting -- as a typed event the consumer actually sees --
    /// any loss it cannot repair. See `watch.rs` for the policy, the wire guarantees it rests on,
    /// and the measured failure that motivated it.
    ///
    /// The previous version of this function logged stream errors to stderr and returned. Against a
    /// real sidecar killed mid-reply, that produced zero further events, a projection stuck on an
    /// active turn, and a truncated assistant message no UI had any way to flag.
    fn start_watching(&self, session_id: String) {
        let state = WatchState::fresh(self.splits_messages, requested_cli_mode(&self.capabilities));
        self.watch_from(session_id, state);
    }

    /// Opens `session_id`'s watch with `state`: a fresh one, or one going on from where another
    /// process's watch of the session stood.
    fn watch_from(&self, session_id: String, mut state: WatchState) {
        let client = self.client.clone();
        let events = Arc::clone(&self.events);
        let backpressure = Arc::clone(&self.backpressure);
        // The mode this session's `CreateSession` asked for, the one its CLI's reports are judged
        // against: every session this provider creates asks for the same one.
        let requested = requested_cli_mode(&self.capabilities);
        let asks_questions = self.capabilities.structured_questions_v1;
        #[cfg(target_os = "linux")]
        let watched = session_id.clone();
        let task = self.runtime.handle().spawn(async move {
            // The single exit for every abnormal path. Nothing here may return quietly except the
            // one genuinely normal ending: a session that closed on its own terms.
            let report = |reason: String| {
                eprintln!("agent: ClaudeSidecarProvider: event stream lost: {reason}");
                events.push(AgentDomainEvent::SessionUnavailable { reason });
            };

            // The first open reads from the session's start; every later one is a re-watch.
            let mut reopening = false;
            loop {
                let request = WatchSessionEventsRequest {
                    session_id: session_id.clone(),
                    // Always AFTER_SEQUENCE, on the first open as well as every reconnect. See
                    // `SequenceTracker::after_sequence` for why neither other mode is usable here:
                    // both are defined as unable to report a gap, which is the one thing this client
                    // must never accept silently.
                    start: ReplayStart::AfterSequence as i32,
                    after_sequence: Some(state.tracker.after_sequence()),
                };
                let opened = {
                    let mut client = client.clone();
                    client.watch_session_events(request).await
                };
                let mut stream = match opened {
                    Ok(response) => {
                        let snapshot = (std::mem::replace(&mut reopening, true) && asks_questions)
                            .then(|| read_interaction_state(&client, &session_id));
                        stream_reopened(&events, &mut state, snapshot).await;
                        response.into_inner()
                    }
                    Err(status) => {
                        let detail = match watch::classify_open_failure(&map_status(status)) {
                            watch::OpenFailure::Fatal(detail) => {
                                report(watch::stream_ended_early_reason(&detail));
                                return;
                            }
                            watch::OpenFailure::Retry(detail) => detail,
                        };
                        match watch::WATCH_RECONNECT_BACKOFF.get(state.consecutive_failures) {
                            Some(delay) => {
                                state.consecutive_failures += 1;
                                backpressure.watch_reconnects.fetch_add(1, Ordering::Relaxed);
                                tokio::time::sleep(*delay).await;
                                continue;
                            }
                            None => {
                                report(watch::stream_ended_early_reason(&detail));
                                return;
                            }
                        }
                    }
                };

                let mut delivered_since_open = false;
                let mut closed_by_session = false;
                let end_detail = loop {
                    match stream.message().await {
                        Ok(Some(event)) => {
                            let sequence = event.sequence;
                            record_delivery_lag(&backpressure, event.occurred_at);
                            match state.tracker.observe(sequence) {
                                watch::SequenceVerdict::Deliver => {
                                    delivered_since_open = true;
                                    let mut translated = Vec::new();
                                    if let Some(boundary) = state.split.before(&event) {
                                        translated.push(boundary);
                                    }
                                    if let Some(line) = state.cli_mode_note.observe(&event) {
                                        eprintln!("{line}");
                                    }
                                    for domain_event in translate::translate(event, requested) {
                                        // A session that closes on its own terms is the one ending
                                        // that is not a loss -- recorded from the provider's own
                                        // terminal event, never inferred from the stream stopping.
                                        closed_by_session |=
                                            matches!(domain_event, AgentDomainEvent::SessionClosed { .. });
                                        state.questions.observe(&domain_event);
                                        // A level read before a command that may change it says nothing
                                        // about what the command left.
                                        if events.effort_ask_open()
                                            && matches!(domain_event, AgentDomainEvent::EffortReported { .. })
                                        {
                                            continue;
                                        }
                                        let effort = events.effort_answer(&domain_event);
                                        translated.push(domain_event);
                                        translated.extend(effort);
                                    }
                                    // The asks a reconcile held back until the replay got this far.
                                    translated.extend(state.questions.due(sequence));
                                    // Queued with the watch's position after the event, so whoever
                                    // takes them knows where the watch stood as of the last one.
                                    events.push_wire(translated, state.mark());
                                }
                                // Replay after a reconnect legitimately re-sends what was already
                                // delivered. Dropping those is the one silent path that is correct.
                                watch::SequenceVerdict::Duplicate => {}
                                watch::SequenceVerdict::Lost { first, last } => {
                                    report(watch::events_lost_reason(first, last));
                                    return;
                                }
                                watch::SequenceVerdict::Invalid => {
                                    report(watch::stream_ended_early_reason(
                                        "the provider sent an event with the reserved sequence 0",
                                    ));
                                    return;
                                }
                            }
                        }
                        // A clean end AFTER the session's own terminal event is normal. A clean end
                        // without one is not: the sidecar's pump loop reaches exactly that state
                        // when it tears a session down with no terminal event left to broadcast.
                        Ok(None) => break "the provider closed the event stream".to_string(),
                        Err(status) => break status.to_string(),
                    }
                };

                if closed_by_session {
                    return;
                }
                // Until the stream is open again, a held turn end must not give up on its follow-up:
                // the replay may carry it.
                events.stream_lost();
                if delivered_since_open {
                    state.consecutive_failures = 0;
                }
                match watch::WATCH_RECONNECT_BACKOFF.get(state.consecutive_failures) {
                    Some(delay) => {
                        state.consecutive_failures += 1;
                        backpressure.watch_reconnects.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(*delay).await;
                    }
                    None => {
                        report(watch::stream_ended_early_reason(&end_detail));
                        return;
                    }
                }
            }
        });
        #[cfg(target_os = "linux")]
        lock(&self.watches).push(Watch {
            session_id: watched,
            task,
        });
        // Nothing moves a session here, so nothing will cancel its watch: dropping the handle leaves the
        // task running, as keeping it did.
        #[cfg(not(target_os = "linux"))]
        drop(task);
    }
}

/// The watch stream is open again. First, when `snapshot` reads the sidecar's list of questions still
/// waiting, the ask of each one this watch never saw is queued (`watch::reconcile_questions`), or held
/// back until the replay reaches the sequence the list was read at. Only then does a held turn end
/// start its new wait for its follow-up, which comes in the replay: the replay is not read while the
/// list is, so a slow read must not use that wait up and have the steered message reported dropped
/// while its delivery is still to be read.
async fn stream_reopened<F>(events: &Arc<EventQueue>, state: &mut WatchState, snapshot: Option<F>)
where
    F: std::future::Future<Output = Option<crate::InteractionState>>,
{
    if let Some(read) = snapshot {
        if let Some(snapshot) = read.await {
            let added = watch::reconcile_questions(&mut state.questions, snapshot, state.tracker.after_sequence());
            if !added.is_empty() {
                events.push_wire(added, state.mark());
            }
        }
    }
    events.stream_back();
}

/// The sidecar's list of questions still waiting, after a re-opened watch. The replay itself carries
/// every ask and settlement the break hid, so a failed read loses nothing; it is logged and the watch
/// goes on.
async fn read_interaction_state(
    client: &RuntimeServiceClient<Channel>,
    session_id: &str,
) -> Option<crate::InteractionState> {
    let request = claude_runtime_protocol::v1::GetInteractionStateRequest {
        session_id: session_id.to_string(),
    };
    let mut client = client.clone();
    let read = tokio::time::timeout(UNARY_RPC_TIMEOUT, client.get_interaction_state(request)).await;
    match read {
        Ok(Ok(response)) => Some(translate_322::interaction_state(response.into_inner())),
        Ok(Err(status)) => {
            eprintln!(
                "agent: ClaudeSidecarProvider: the questions still waiting could not be read after a re-watch: {}",
                map_status(status)
            );
            None
        }
        Err(_) => {
            eprintln!("agent: ClaudeSidecarProvider: reading the questions still waiting after a re-watch timed out");
            None
        }
    }
}

/// Locks `mutex`, going on past a panic elsewhere: every value kept behind these locks stays whole
/// between statements.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a stalled consumer is actually costing, as opposed to whether it is losing anything.
///
/// The existing backpressure tests establish that a stalled consumer loses NOTHING. That is a real
/// property and a necessary one, but it is not the same as boundedness, and gRPC supplies the first
/// while concealing the absence of the second: a congested reader is never told it is congested, the
/// queue simply grows. These counters exist to answer the question the loss tests cannot -- is the
/// system lossless because recovery is bounded, or because some layer is buffering without limit?
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackpressureStats {
    /// Events received from the wire and not yet taken by `pump()`. This client's own queue, and the
    /// only one of the three (grpc-js server-side, tonic client-side, this `Vec`) that is
    /// observable from here at all.
    pub pending_events: usize,
    /// Every event this provider has received since it connected.
    pub events_received: u64,
    /// The worst delay seen between the provider stamping an event and this client receiving it,
    /// in milliseconds, from the wire's own `occurred_at`.
    ///
    /// The honest measure of a backlog: a queue that is growing shows up here as delivery falling
    /// further and further behind production, whereas `pending_events` only shows what has not yet
    /// been collected by the consumer.
    pub max_delivery_lag_ms: i64,
    /// The most recent such delay, so a caller can see catch-up happening rather than only the peak.
    pub last_delivery_lag_ms: i64,
    /// How many times this session's watch stream has been re-opened after breaking.
    ///
    /// Zero on a healthy session, and that is what makes it useful: a successful recovery is
    /// otherwise completely silent, so a test asserting "it recovered" has no way to tell a repaired
    /// stream from one that was never broken. Without this, such a test passes vacuously.
    pub watch_reconnects: u64,
}

/// Shared counters the watch task updates and `backpressure_stats()` reads. Atomics rather than a
/// mutex: the watch loop touches these on every single event, and a partial-streamed turn is
/// hundreds of them.
#[derive(Default)]
struct BackpressureCounters {
    events_received: AtomicU64,
    watch_reconnects: AtomicU64,
    max_delivery_lag_ms: AtomicI64,
    last_delivery_lag_ms: AtomicI64,
}

/// Classifies one `tonic::Status` into this crate's own typed error.
///
/// A free function, not just an inherent method, because the watch task owns no `&self` and must
/// classify a failed reconnect exactly the way a failed unary RPC is classified -- two divergent
/// copies of this logic is how a fatal `EVENT_GAP` ends up being retried forever on one path.
fn map_status(status: tonic::Status) -> ProviderError {
    // Business errors travel as
    // protobuf-encoded `ErrorDetail` bytes in the standard `grpc-status-details-bin` trailing
    // metadata key -- but tonic's `Status` already parses that specific well-known trailer out
    // for you: `status.details()` returns the raw bytes directly, `status.metadata()` does NOT
    // contain it (confirmed against a real sidecar-returned `SESSION_NOT_FOUND` error during
    // development -- `status.metadata().get_bin("grpc-status-details-bin")`
    // silently returned `None` every time; `status.details()` decoded correctly on the first
    // try). Do not "fix" this back to a `.metadata()` lookup -- that was the actual bug.
    match error_detail(&status) {
        Some(detail) => ProviderError::Provider {
            code: provider_error_code(detail.code()),
            message: detail.message,
        },
        None => ProviderError::Transport(status.message().to_string()),
    }
}

/// The sidecar's own error, when the status carries one (see `map_status` for where it travels).
fn error_detail(status: &tonic::Status) -> Option<ErrorDetail> {
    let bytes = status.details();
    if bytes.is_empty() {
        return None;
    }
    <ErrorDetail as prost::Message>::decode(bytes).ok()
}

/// The longest part of the sidecar's own words a refused message's reason carries, in characters.
const MAX_STEER_REASON_CHARS: usize = 200;

/// What a failed `SteerTurn` means for the message.
#[derive(Debug)]
enum SteerFailure {
    /// The sidecar answered with its own error, or the call failed in a way that means its handler
    /// never took the message: nothing reached the CLI.
    Refused(ProviderError),
    /// No answer that says so (the connection failed or broke, the call was cut off): the message may
    /// have reached the CLI.
    Unknown(String),
}

/// What goes to the CLI for a message: as it is when a person typed it, whose `@path` the CLI reads; with
/// every mention made plain otherwise, so that text no person typed never chooses a file to read.
fn turn_text(text: String, typed_by_person: bool) -> String {
    if typed_by_person {
        return text;
    }
    match crate::mention::inert(&text) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(plain) => plain,
    }
}

fn steer_failure(status: tonic::Status) -> SteerFailure {
    if let Some(detail) = error_detail(&status) {
        return SteerFailure::Refused(steer_refusal(detail));
    }
    // Without the sidecar's own error, the code says whether the handler can have run. These are the
    // codes gRPC (here or on the sidecar's side) gives a call it rejects before any handler acts on it:
    // a method the server does not have, a message too large or malformed, a refused request. The rest
    // (`Unavailable`, `Cancelled`, `DeadlineExceeded`, `Unknown`, `Internal`, `DataLoss`, ...) can come
    // after the request was taken.
    use tonic::Code;
    let words = match status.code() {
        Code::Unimplemented => Some("this sidecar cannot take messages mid-turn".to_string()),
        Code::ResourceExhausted => Some("the message is too large to send".to_string()),
        Code::InvalidArgument
        | Code::NotFound
        | Code::AlreadyExists
        | Code::PermissionDenied
        | Code::Unauthenticated
        | Code::FailedPrecondition
        | Code::OutOfRange => {
            let message = status.message();
            let words: String = message.chars().take(MAX_STEER_REASON_CHARS).collect();
            let cut = if message.chars().count() > MAX_STEER_REASON_CHARS {
                "…"
            } else {
                ""
            };
            Some(format!("Claude did not take it: {words}{cut}"))
        }
        _ => None,
    };
    match words {
        Some(message) => SteerFailure::Refused(ProviderError::Provider {
            code: ProviderErrorCode::Unspecified,
            message,
        }),
        None => SteerFailure::Unknown(status.message().to_string()),
    }
}

/// A refused `SteerTurn` as the host shows it, after "held until the turn ends:". The code is kept, so
/// a caller can still tell a stale turn from a session that cannot steer at all.
fn steer_refusal(detail: ErrorDetail) -> ProviderError {
    let code = provider_error_code(detail.code());
    let message = match (code, detail.reason.as_str()) {
        (ProviderErrorCode::NoActiveTurn, _) => "the turn had ended when it reached Claude".to_string(),
        (ProviderErrorCode::TurnMismatch, _) => "the turn it was typed in had ended when it reached Claude".to_string(),
        (ProviderErrorCode::SteerUnavailable, "first_turn_held") => {
            "the session's first turn has not reached Claude yet".to_string()
        }
        (ProviderErrorCode::SteerUnavailable, "cli_lifecycle_not_reported") => {
            "Claude has not yet said this session can take messages mid-turn".to_string()
        }
        (ProviderErrorCode::SteerUnavailable, _) => "Claude cannot take messages mid-turn now".to_string(),
        (ProviderErrorCode::SessionNotFound, _) => "the session has ended".to_string(),
        _ => {
            let words: String = detail.message.chars().take(MAX_STEER_REASON_CHARS).collect();
            let cut = if detail.message.chars().count() > MAX_STEER_REASON_CHARS {
                "…"
            } else {
                ""
            };
            format!("Claude did not take it: {words}{cut}")
        }
    };
    ProviderError::Provider { code, message }
}

/// Refuses to start a session on a sidecar that does not advertise the gated (`interactive`)
/// policy -- the only one any session is created under, whatever the tab's mode.
///
/// Failing loudly is the whole point: the alternative, starting the session under whatever the
/// provider does offer, would run an agent under a permission policy nobody chose and nobody was
/// told about. There is no fallback to BYPASS, ever. A free function so it is tested without a
/// sidecar.
fn require_interactive(capabilities: &ProviderCapabilities, info: &ProviderInfo) -> Result<(), ProviderError> {
    if capabilities.interactive_permission_mode {
        return Ok(());
    }
    Err(ProviderError::Provider {
        code: ProviderErrorCode::InvalidConfiguration,
        message: format!(
            "this provider does not offer a gated (interactive) mode, the only one Eitri runs a \
             session in; it advertises: {}",
            if info.advertised_permission_modes.is_empty() {
                "none".to_string()
            } else {
                info.advertised_permission_modes.join(", ")
            }
        ),
    })
}

/// Builds the one `CreateSessionRequest` both a fresh session and a resume go through.
///
/// The policy is fixed rather than parameterised on purpose, and that includes the
/// permission policy: every session is `INTERACTIVE` and never switchable, whatever the tab's
/// mode. This client only ever sends values whose runtime behavior has been confirmed distinct.
/// `verdandi_rules` is identical to `interactive` in the current sidecar, `external_store` is
/// identical to `ephemeral`, and
/// `executable` is not read at all -- sending any of them would be choosing a value that means
/// nothing. `streaming` is the exception that proves the rule: PARTIAL has a measured, distinct
/// effect, so it is sent. `provider_prompts` is another: `provider_prompts_from_handshake`, a
/// policy field that depends on the peer. `claude_code_prompt` opts into the preset only when
/// offered. `cli_mode` (`requested_cli_mode`): `Auto` only when the handshake offered both `cli_auto_mode` and
/// `permission_defer`, and `Default` leaves the field unset, so a request to a sidecar without them
/// is byte-for-byte what it was before the field existed.
#[allow(clippy::too_many_arguments)]
fn build_create_request(
    cwd: String,
    streaming: StreamingPreference,
    resume_provider_session_id: Option<String>,
    fork: bool,
    provider_prompts: bool,
    claude_code_prompt: bool,
    cli_mode: crate::RequestedCliMode,
    project: ProjectTrust,
    questions: QuestionPolicy,
    client_tools: Option<ClientTools>,
) -> ProtoCreateSessionRequest {
    let mut request = build_create_request_loading(
        cwd,
        streaming,
        resume_provider_session_id,
        fork,
        provider_prompts,
        claude_code_prompt,
        crate::setting_sources::loads_user_settings(),
        project,
        cli_mode,
        questions,
    );
    request.client_tools = client_tools;
    request
}

/// The wire's form of what a session declares, for a request to a peer that does (`offered`) or does
/// not offer `client_tools`. Absent for a peer without it, which would run the session with no tools
/// and no word about it, and for a set with no tool in it, which the wire refuses.
fn client_tools_for(set: Option<ClientToolSet>, offered: bool) -> Option<ClientTools> {
    let ClientToolSet {
        server_name,
        timeout_ms,
        tools,
    } = set.filter(|set| offered && !set.tools.is_empty())?;
    let tools = tools
        .into_iter()
        .map(|tool| {
            let ClientToolDecl {
                name,
                description,
                input_schema_json,
                always_load,
            } = tool;
            ClientToolDeclaration {
                name,
                description,
                input_schema_json,
                always_load,
            }
        })
        .collect();
    Some(ClientTools {
        server_name,
        timeout_ms,
        tools,
    })
}

/// The settings tiers on the wire, in the order given. The one place the proto's tier names are
/// written: every selection comes from `setting_sources::for_session`.
fn proto_sources(sources: &[Source]) -> Vec<i32> {
    sources
        .iter()
        .map(|source| match source {
            Source::User => SettingSource::User as i32,
            Source::Project => SettingSource::Project as i32,
            Source::Local => SettingSource::Local as i32,
        })
        .collect()
}

/// [`build_create_request`] with the user tier named instead of read from the process-wide choice,
/// so every selection can be built and compared in one process.
#[allow(clippy::too_many_arguments)]
fn build_create_request_loading(
    cwd: String,
    streaming: StreamingPreference,
    resume_provider_session_id: Option<String>,
    fork: bool,
    provider_prompts: bool,
    claude_code_prompt: bool,
    user_settings: bool,
    project: ProjectTrust,
    cli_mode: crate::RequestedCliMode,
    questions: QuestionPolicy,
) -> ProtoCreateSessionRequest {
    let denied = crate::process::disallowed_tools();
    ProtoCreateSessionRequest {
        cwd,
        policy: Some(ClaudeHostPolicy {
            configuration: ConfigurationProfile::Native as i32,
            // Always gated: Verdandi's `PreToolUse` broker asks this host about every call,
            // and bypass is Eitri answering `allow`, never a policy sent here. The CLI under it
            // runs `default`, or its own `auto` (`cli_permission_mode` below).
            permissions: ProtoPermissionMode::Interactive as i32,
            persistence: PersistenceMode::HostCli as i32,
            executable: ExecutableSource::HostCli as i32,
            // Without PARTIAL a long reply is a blank panel for ten-plus seconds and then a wall of
            // text: the provider only emits the completed message. A UI wants the incremental form;
            // the cost the sidecar's own default guards against (many more events per turn, filling
            // the replay buffer faster) is why it is opt-in rather than the sidecar's default.
            streaming: match streaming {
                StreamingPreference::Complete => StreamingMode::Complete as i32,
                StreamingPreference::Partial => StreamingMode::Partial as i32,
            },
            // Explicit settings and tool denials match the legacy backend's
            // `--setting-sources` and `--disallowedTools` arguments instead of CLI defaults.
            // `Native` uses the selected tiers to choose what loads. `Isolated` has no
            // UI, config or Lua selector.
            // Always send the selection, even when empty. An absent selection loads all three tiers,
            // so an untrusted session with the user tier off must explicitly select none.
            setting_sources: Some(SettingSourceSelection {
                // The user tier by default, so a session behaves as `claude` does in a terminal:
                // the user's own hooks, plugins, skills, `CLAUDE.md` and permission rules apply.
                // `agent.user_settings = false` leaves it out. The project and local tiers load
                // only for a session started as trusted. The permission gate does not depend on
                // which is chosen: it comes from this request's `INTERACTIVE` policy.
                sources: proto_sources(crate::setting_sources::for_session(user_settings, project)),
            }),
            // Bound once: `deny` and `unrestricted` are two statements about the same list, and the
            // sidecar refuses the pair if they disagree.
            //
            // Sent explicitly even under `Bypass`, and that is parity rather than a new restriction:
            // the legacy path passes `--disallowedTools` unconditionally and only skips the hook.
            // Stating it also means it is honoured verbatim; omitting it would earn the sidecar's
            // own conservative default plus a notice, which is the same list by a less direct route.
            //
            // `allow` is left absent, which is not the same as empty. It maps to the SDK's
            // `Options.tools` -- the real restriction list, not `allowedTools`, which is a
            // PRE-APPROVAL list a field named `allow` would have quietly widened access through.
            // Absent means "do not touch the base tool set"; present-and-empty would mean "no
            // built-in tools at all", stated. Eitri has no allowlist to express, so it says
            // nothing rather than saying the empty set.
            tool_policy: Some(ToolPolicy {
                deny: denied.iter().map(|t| (*t).to_string()).collect(),
                // `unrestricted` states "this caller restricts no tool, and means it" -- the exit
                // Verdandi built for exactly this case (`usesDefaultBypassDeny`, `session.ts`).
                // **Without it, emptying the deny list would have changed nothing on this path**:
                // an empty list under `bypass` reads as silence, and the sidecar then injects its
                // own `CONSERVATIVE_BYPASS_DENY` -- the same four tool names this side just stopped
                // sending. Measured against the sidecar's source before the change was made, not
                // discovered afterwards.
                //
                // Derived from the list rather than set by hand, because the sidecar REJECTS
                // `unrestricted` alongside any stated restriction (`runtimeServiceImpl.ts`) rather
                // than guessing which of the two the caller meant. The two can therefore never
                // disagree here.
                unrestricted: denied.is_empty(),
                allow: None,
            }),
            // Never switchable: `true` sets --allow-dangerously-skip-permissions on the CLI.
            permission_mode_switchable: false,
            // The CLI's own permission prompts (its sensitive-file check, which neither the
            // gate's `allow` nor a rule silences) come to this host instead of being refused with
            // nobody asked. Meaningful because the session is INTERACTIVE; an older sidecar is never
            // sent it (`provider_prompts_from_handshake`). Adds no tool: the sidecar disallows the
            // three the SDK's prompt channel brings with it, and `allow` above is absent.
            provider_permission_prompts: provider_prompts,
            // `Auto`: the CLI runs its own auto mode under the gate, so a call this host defers
            // (`PermissionDecision::Defer`) meets the CLI's classifier rather than its ordinary
            // ask. Verdandi accepts it only with INTERACTIVE and not switchable, both fixed above.
            // UNSPECIFIED otherwise, never DEFAULT: DEFAULT would opt a session that cannot defer
            // into `PermissionDenied` events, so its request would no longer be the one sent
            // before the field existed.
            cli_permission_mode: match cli_mode {
                crate::RequestedCliMode::Auto => CliPermissionMode::Auto as i32,
                crate::RequestedCliMode::Default => CliPermissionMode::Unspecified as i32,
            },
            // Never set: a sidecar that refuses its CLI refuses the session, and Eitri has no way
            // for a user to decide otherwise per session.
            accept_unsupported_cli: false,
            // The session opener fills this from the handshake and host readiness, for fresh
            // and resumed sessions alike. A peer without task controls receives no new field.
            per_task_stop: false,
        }),
        resume_provider_session_id,
        fork,
        // Absence preserves the CLI's own question handling until both ends support the host path.
        client_interactions: (questions.capable && questions.client_implements)
            .then_some(claude_runtime_protocol::v1::ClientInteractions { questions: true }),
        // Keep model, effort and output format at their defaults. A replacement would discard
        // Claude Code's instructions; the preset keeps them and adds only static panel context.
        // Without the capability both fields stay absent, retaining the SDK's minimal prompt.
        model: None,  // absent = the CLI's default model
        effort: None, // absent = the CLI's default reasoning effort
        system_prompt: None,
        // Stated by `build_create_request`, which alone knows what the session declares.
        client_tools: None,
        system_prompt_preset: claude_code_prompt.then(|| SystemPromptPreset {
            append: Some(EITRI_APPEND.to_string()),
        }),
        output_format: None, // absent = plain text, not schema-validated structured output
        // false = `CreateSession` returns as soon as the session exists, as it always has.
        await_account_identity: false,
    }
}

/// The peer's offer and the host's readiness are separate requirements for question delivery.
#[derive(Clone, Copy)]
struct QuestionPolicy {
    capable: bool,
    client_implements: bool,
}

impl QuestionPolicy {
    /// The peer's offer, and the host's readiness: the shipped constant unless a scripted run
    /// replaces it. The peer's offer is never replaced.
    fn resolve(capable: bool, readiness_override: Option<bool>) -> Self {
        Self {
            capable,
            client_implements: readiness_override.unwrap_or(CLIENT_IMPLEMENTS_QUESTIONS),
        }
    }
}

impl Default for QuestionPolicy {
    fn default() -> Self {
        Self {
            capable: false,
            client_implements: CLIENT_IMPLEMENTS_QUESTIONS,
        }
    }
}

/// The wire request for a fresh session. Destructured whole, so a field added to the request has to
/// be placed here rather than dropped on the way to the wire.
fn fresh_request(
    request: CreateSessionRequest,
    provider_prompts: bool,
    claude_code_prompt: bool,
    cli_mode: crate::RequestedCliMode,
    questions: QuestionPolicy,
    offers_client_tools: bool,
) -> ProtoCreateSessionRequest {
    let CreateSessionRequest {
        cwd,
        streaming,
        project,
        client_tools,
    } = request;
    build_create_request(
        cwd,
        streaming,
        None,
        false,
        provider_prompts,
        claude_code_prompt,
        cli_mode,
        project,
        questions,
        client_tools_for(client_tools, offers_client_tools),
    )
}

/// The wire request continuing a Claude session, under the trust the caller states now. Destructured
/// whole, as [`fresh_request`] is.
fn resume_request(
    request: ResumeSessionRequest,
    provider_prompts: bool,
    claude_code_prompt: bool,
    cli_mode: crate::RequestedCliMode,
    questions: QuestionPolicy,
    offers_client_tools: bool,
) -> ProtoCreateSessionRequest {
    let ResumeSessionRequest {
        provider_session_id,
        cwd,
        streaming,
        project,
        client_tools,
    } = request;
    build_create_request(
        cwd,
        streaming,
        Some(provider_session_id),
        false,
        provider_prompts,
        claude_code_prompt,
        cli_mode,
        project,
        questions,
        client_tools_for(client_tools, offers_client_tools),
    )
}

fn map_task_status(status: tonic::Status) -> ProviderError {
    match map_status(status) {
        ProviderError::Provider { code, message } => ProviderError::Provider {
            code,
            message: translate_322::bound(&message, 2000),
        },
        ProviderError::Transport(message) => ProviderError::Transport(translate_322::bound(&message, 2000)),
        other => other,
    }
}

/// A task refusal is plain bounded text, with no session-ending side effect.
pub fn task_refusal(error: ProviderError) -> String {
    let text = match error {
        ProviderError::Provider {
            code: ProviderErrorCode::TaskNotFound,
            ..
        } => "task no longer exists".into(),
        ProviderError::Provider {
            code: ProviderErrorCode::TaskOutputUnavailable,
            ..
        } => "task output is not available".into(),
        ProviderError::Provider {
            code: ProviderErrorCode::SettingRefused,
            message,
        } => message,
        other => other.to_string(),
    };
    translate_322::bound(&text, 2000)
}

impl AgentProvider for ClaudeSidecarProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    /// Always gated: refused on a sidecar without the interactive policy, never started under
    /// another one.
    fn create_session(&self, request: CreateSessionRequest) -> Result<String, ProviderError> {
        self.require_interactive()?;
        self.open_session(fresh_request(
            request,
            self.provider_prompts,
            self.claude_code_prompt,
            requested_cli_mode(&self.capabilities),
            self.question_policy(),
            self.capabilities.client_tools,
        ))
    }

    /// Continues an existing Claude session. Goes through the same `CreateSession` RPC as a fresh
    /// session, carrying `resume_provider_session_id` -- resume is a parameter of session creation
    /// on this wire, not a lifecycle of its own, because that is the shape the runtime kernel itself
    /// has (its `ClaudeSessionConfig` carries `resume`/`fork` as config fields).
    ///
    /// Returns the sidecar's session id for the resumed session, which is NOT the provider session
    /// id that was passed in: the former is Verdandi's, minted fresh per `CreateSession`; the latter
    /// is Claude's, and is what continues.
    fn resume_session(&self, request: ResumeSessionRequest) -> Result<String, ProviderError> {
        if !self.capabilities.resume {
            // The provider does not advertise it, so this client must not send it: never send a
            // command on the theory the server will ignore it.
            return Err(ProviderError::UnsupportedCapability("resume"));
        }
        if request.provider_session_id.trim().is_empty() {
            return Err(ProviderError::Provider {
                code: ProviderErrorCode::InvalidConfiguration,
                message: "cannot resume: no provider session id was supplied".to_string(),
            });
        }
        // Checked on this path too, not only on create: resuming does not inherit the policy the
        // original session ran under, and the resumed session is created gated like any other.
        self.require_interactive()?;
        self.open_session(resume_request(
            request,
            self.provider_prompts,
            self.claude_code_prompt,
            requested_cli_mode(&self.capabilities),
            self.question_policy(),
            self.capabilities.client_tools,
        ))
    }

    fn send_turn(&self, request: SendTurnRequest) -> Result<String, ProviderError> {
        self.send_turn_stamped(request, TurnStamp::default())
    }

    /// `SendTurn` with the message's client id when the sidecar can name it back (capability
    /// `cli_turns`): Claude Code may run work of its own before it reads the message, and the sidecar
    /// then reports when it does (`UserMessageDelivered`, shown with the stamp's typed text) or that
    /// it dropped it. Marked as typed by the person only when the stamp says so and the sidecar
    /// accepts the mark (capability `human_origin`; one without it refuses the mark).
    fn send_turn_stamped(&self, request: SendTurnRequest, stamp: TurnStamp) -> Result<String, ProviderError> {
        self.require_driving()?;
        let mut client = self.client.clone();
        let client_message_id = stamp
            .client_message_id
            .filter(|id| self.capabilities.cli_turns && !id.is_empty());
        if let Some(id) = &client_message_id {
            let typed = stamp.as_typed.unwrap_or_else(|| request.text.clone());
            self.events.note_sent(id, typed);
        }
        self.events.note_effort_ask(effort_reply::ask_of(&request.text));
        let text = turn_text(request.text, stamp.typed_by_person);
        let proto_request = ProtoSendTurnRequest {
            session_id: request.session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
            text,
            client_message_id: client_message_id.clone(),
            typed_by_person: stamp.typed_by_person && self.capabilities.human_origin,
        };
        let result = self.run_unary(async move { client.send_turn(proto_request).await });
        if result.is_err() {
            self.events.forget_effort_ask();
            if let Some(id) = &client_message_id {
                self.events.forget_sent(id);
            }
        }
        let turn_id = result?.turn_id;
        self.events.note_effort_turn(&turn_id);
        Ok(turn_id)
    }

    /// Takes one message for the running turn and returns at once: the provider's runtime sends it.
    /// Never waits on the sidecar, since the host calls this on its UI thread.
    ///
    /// **Order.** Verdandi's kernel hands a steered message to the CLI's input queue before it answers
    /// `SteerTurn`, and the CLI takes that queue in order, so messages answered in order reach the CLI
    /// in order. One message is in flight at a time (`send_steers`): a later one waits in the gate,
    /// unsent, until the one before it is answered, and is dropped unsent when that one is refused. A
    /// message the CLI drops after taking it cannot take later ones back with it: nothing on this wire
    /// withdraws a queued message, so each is reported as what the CLI did with it.
    ///
    /// **Refusals** arrive as `UserMessageDropped`, worded for the host's held note (`steer_refusal`).
    /// `Err` only when the gate refuses it here (the id is open already, or too many are open), when
    /// nothing was sent either.
    fn steer_turn(&self, request: SteerTurnRequest) -> Result<(), ProviderError> {
        self.steer_turn_stamped(request, TurnStamp::default())
    }

    /// [`steer_turn`](Self::steer_turn), marked as typed by the person when the stamp says so and the
    /// sidecar accepts the mark (capability `human_origin`).
    fn steer_turn_stamped(&self, request: SteerTurnRequest, stamp: TurnStamp) -> Result<(), ProviderError> {
        if !self.capabilities.steer {
            return Err(ProviderError::UnsupportedCapability("steer"));
        }
        self.require_driving()?;
        let SteerTurnRequest {
            session_id,
            expected_turn_id,
            client_message_id,
            text,
            as_typed,
        } = request;
        let proto_request = ProtoSteerTurnRequest {
            session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
            expected_turn_id,
            client_message_id: client_message_id.clone(),
            text: turn_text(text, stamp.typed_by_person),
            typed_by_person: stamp.typed_by_person && self.capabilities.human_origin,
        };
        if let Some(now) = self.events.open_steer(&client_message_id, as_typed, proto_request)? {
            // Counted before this returns, so a move asked for right after it sees the send in flight.
            let steering = Steering::begin(&self.events);
            self.runtime.handle().spawn(send_steers(
                self.client.clone(),
                Arc::clone(&self.events),
                (client_message_id, now),
                steering,
            ));
        }
        Ok(())
    }

    fn stop_task(&self, session_id: &str, task_id: &str) -> Result<crate::TaskReply<()>, ProviderError> {
        if !self.capabilities.task_control {
            return Err(ProviderError::UnsupportedCapability("stop_task"));
        }
        self.require_driving()?;
        let request = claude_runtime_protocol::v1::StopTaskRequest {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            command_id: uuid::Uuid::new_v4().to_string(),
        };
        let mut client = self.client.clone();
        Ok(self.task_rpc(async move {
            let response = client.stop_task(request).await.map_err(map_task_status)?.into_inner();
            translate_322::task_stopped(response);
            Ok(())
        }))
    }

    fn task_output(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<crate::TaskReply<crate::TaskOutput>, ProviderError> {
        if !self.capabilities.task_output {
            return Err(ProviderError::UnsupportedCapability("task_output"));
        }
        self.require_driving()?;
        let request = claude_runtime_protocol::v1::GetTaskOutputRequest {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            offset: None,
            max_bytes: 0,
        };
        let mut client = self.client.clone();
        Ok(self.task_rpc(async move {
            let response = client
                .get_task_output(request)
                .await
                .map_err(map_task_status)?
                .into_inner();
            let tail = translate_322::task_output(response);
            Ok(crate::TaskOutput {
                output: tail.output,
                total_bytes: tail.total_bytes,
                truncated: tail.truncated,
                offset: tail.offset,
            })
        }))
    }

    fn task_output_range(
        &self,
        session_id: &str,
        task_id: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<crate::TaskReply<crate::TaskOutput>, ProviderError> {
        if !self.capabilities.task_output_range {
            return Err(ProviderError::UnsupportedCapability("task_output_range"));
        }
        self.require_driving()?;
        let request = claude_runtime_protocol::v1::GetTaskOutputRequest {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            offset: Some(offset),
            max_bytes,
        };
        let mut client = self.client.clone();
        Ok(self.task_rpc(async move {
            let response = client
                .get_task_output(request)
                .await
                .map_err(map_task_status)?
                .into_inner();
            let range = translate_322::task_output_range(response);
            Ok(crate::TaskOutput {
                output: range.output,
                total_bytes: range.total_bytes,
                truncated: range.truncated,
                offset: range.offset,
            })
        }))
    }

    fn list_models(&self, session_id: &str) -> Result<crate::TaskReply<crate::ModelList>, ProviderError> {
        if !self.capabilities.direct_settings {
            return Err(ProviderError::UnsupportedCapability("list_models"));
        }
        self.require_driving()?;
        let request = claude_runtime_protocol::v1::ListModelsRequest {
            session_id: session_id.to_owned(),
        };
        let mut client = self.client.clone();
        Ok(self.task_rpc(async move {
            let response = client.list_models(request).await.map_err(map_task_status)?.into_inner();
            Ok(translate_322::model_list(response))
        }))
    }

    fn set_model(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<crate::TaskReply<crate::AppliedSettings>, ProviderError> {
        if !self.capabilities.direct_settings {
            return Err(ProviderError::UnsupportedCapability("set_model"));
        }
        self.require_driving()?;
        let request = claude_runtime_protocol::v1::SetModelRequest {
            session_id: session_id.to_owned(),
            command_id: uuid::Uuid::new_v4().to_string(),
            model: model.to_owned(),
        };
        let mut client = self.client.clone();
        let events = self.events.clone();
        Ok(self.rpc_within(SET_MODEL_TIMEOUT, async move {
            let response = client.set_model(request).await.map_err(map_task_status)?.into_inner();
            let applied = translate_322::applied_settings(response.applied);
            EventQueue::settle_applied(&events, applied.clone());
            Ok(applied)
        }))
    }

    fn apply_effort(
        &self,
        session_id: &str,
        effort: &str,
    ) -> Result<crate::TaskReply<crate::AppliedSettings>, ProviderError> {
        if !self.capabilities.direct_settings {
            return Err(ProviderError::UnsupportedCapability("apply_effort"));
        }
        self.require_driving()?;
        let request = claude_runtime_protocol::v1::ApplyEffortRequest {
            session_id: session_id.to_owned(),
            command_id: uuid::Uuid::new_v4().to_string(),
            effort: effort.to_owned(),
        };
        let mut client = self.client.clone();
        let events = self.events.clone();
        Ok(self.task_rpc(async move {
            let response = client
                .apply_effort(request)
                .await
                .map_err(map_task_status)?
                .into_inner();
            let applied = translate_322::applied_settings(response.applied);
            EventQueue::settle_applied(&events, applied.clone());
            Ok(applied)
        }))
    }

    fn stop_background(&self, session_id: &str, shown: u32, tasks: &[String]) -> Result<(), ProviderError> {
        if !self.capabilities.task_control {
            return Err(ProviderError::UnsupportedCapability("stop_background"));
        }
        self.require_driving()?;
        if shown == 0 || tasks.len() != shown as usize {
            return Err(ProviderError::Provider {
                code: ProviderErrorCode::NoActiveTurn,
                message: "background work changed: nothing was stopped; ask again".into(),
            });
        }
        for task_id in tasks {
            let request = claude_runtime_protocol::v1::StopTaskRequest {
                session_id: session_id.to_owned(),
                task_id: task_id.clone(),
                command_id: uuid::Uuid::new_v4().to_string(),
            };
            let mut client = self.client.clone();
            let events = Arc::clone(&self.events);
            let task_id = task_id.clone();
            self.runtime.handle().spawn(async move {
                let result = tokio::time::timeout(UNARY_RPC_TIMEOUT, client.stop_task(request)).await;
                let result = match result {
                    Ok(Ok(response)) => {
                        translate_322::task_stopped(response.into_inner());
                        Ok(())
                    }
                    Ok(Err(status)) => Err(map_task_status(status)),
                    Err(_) => Err(ProviderError::Timeout),
                };
                if let Err(error) = result {
                    events.push(AgentDomainEvent::SessionNotice {
                        kind: crate::SessionNoticeKind::Task,
                        level: crate::NoticeLevel::Warning,
                        content: Some(task_refusal(error)),
                        scalars: Default::default(),
                        task_id: Some(task_id),
                        tool_use_id: None,
                        task_status: None,
                    });
                }
            });
        }
        Ok(())
    }

    fn interrupt_turn(&self, request: InterruptTurnRequest) -> Result<(), ProviderError> {
        self.require_driving()?;
        let mut client = self.client.clone();
        let proto_request = ProtoInterruptTurnRequest {
            session_id: request.session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
        };
        self.run_unary(async move { client.interrupt_turn(proto_request).await })?;
        Ok(())
    }

    fn resolve_permission(&self, request: ResolvePermissionRequest) -> Result<(), ProviderError> {
        self.require_driving()?;
        let mut client = self.client.clone();
        let proto_request = resolve_request_for(&self.capabilities, request)?;
        self.run_unary(async move { client.resolve_permission(proto_request).await })?;
        Ok(())
    }

    /// One `ResolveInteraction`, spawned on the provider's runtime and bounded by `UNARY_RPC_TIMEOUT`;
    /// the caller is never kept waiting for the sidecar (the person's answer comes from the GTK
    /// thread). Refused here, with nothing sent, on a sidecar that does not offer questions. Until
    /// the returned handle is dropped, which its holder does after reading the reply, the session is
    /// not handed to another window: a refused answer has to put its question back where it was asked.
    fn resolve_interaction(
        &self,
        request: crate::ResolveInteractionRequest,
    ) -> Result<crate::InteractionSend, ProviderError> {
        if !self.capabilities.structured_questions_v1 {
            return Err(ProviderError::UnsupportedCapability("structured_questions_v1"));
        }
        self.require_driving()?;
        let mut client = self.client.clone();
        let proto_request = resolve_interaction_request_for(request, uuid::Uuid::new_v4().to_string());
        // Counted before this returns, so a move asked for right after it sees the answer out.
        let answering = Answering::begin(&self.events);
        let (tx, send) = crate::InteractionSend::channel();
        self.runtime.handle().spawn(async move {
            let reply = match tokio::time::timeout(UNARY_RPC_TIMEOUT, client.resolve_interaction(proto_request)).await {
                Ok(Ok(response)) => Ok(translate_322::resolve_outcome(response.into_inner())),
                Ok(Err(status)) => Err(map_status(status)),
                Err(_) => Err(ProviderError::Timeout),
            };
            // The holder may be gone (its window closed); the reply then has nowhere to go.
            let _ = tx.send(reply);
        });
        Ok(send.with_guard(answering))
    }

    fn get_interaction_state(&self, session_id: &str) -> Result<crate::InteractionState, ProviderError> {
        if !self.capabilities.structured_questions_v1 {
            return Err(ProviderError::UnsupportedCapability("structured_questions_v1"));
        }
        let mut client = self.client.clone();
        let proto_request = claude_runtime_protocol::v1::GetInteractionStateRequest {
            session_id: session_id.to_string(),
        };
        let response = self.run_unary(async move { client.get_interaction_state(proto_request).await })?;
        Ok(translate_322::interaction_state(response))
    }

    /// One `AnswerClientToolCall`, spawned on the provider's runtime and bounded by `UNARY_RPC_TIMEOUT`.
    /// Nothing waits for it, since the person's side of a call runs on the GTK thread, and nothing
    /// holds a move back for it either: unlike a question's answer, a refused one has nothing to put
    /// back. Each answer has its own `command_id`, because the service replays an id it has seen.
    fn answer_client_tool_call(&self, answer: ClientToolAnswer) -> Result<(), ProviderError> {
        if !self.capabilities.client_tools {
            return Err(ProviderError::UnsupportedCapability("client_tools"));
        }
        self.require_driving()?;
        let request = answer_request_for(answer, uuid::Uuid::new_v4().to_string(), self.max_message_bytes);
        let call_id = request.call_id.clone();
        let mut client = self.client.clone();
        self.runtime.handle().spawn(async move {
            let failure = match tokio::time::timeout(UNARY_RPC_TIMEOUT, client.answer_client_tool_call(request)).await {
                Ok(Ok(_)) => return,
                Ok(Err(status)) => map_task_status(status),
                Err(_) => ProviderError::Timeout,
            };
            eprintln!("{}", answer_failure_line(&call_id, &failure));
        });
        Ok(())
    }

    fn client_tool_call_state(&self, session_id: &str) -> Result<crate::TaskReply<ClientToolCallState>, ProviderError> {
        if !self.capabilities.client_tools {
            return Err(ProviderError::UnsupportedCapability("client_tools"));
        }
        self.require_driving()?;
        let request = GetClientToolCallStateRequest {
            session_id: session_id.to_owned(),
        };
        let mut client = self.client.clone();
        Ok(self.task_rpc(async move {
            let response = client
                .get_client_tool_call_state(request)
                .await
                .map_err(map_task_status)?
                .into_inner();
            Ok(client_call_state(response))
        }))
    }

    fn close_session(&self, request: CloseSessionRequest) -> Result<(), ProviderError> {
        self.require_driving()?;
        let mut client = self.client.clone();
        let proto_request = ProtoCloseSessionRequest {
            session_id: request.session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
        };
        self.run_unary(async move { client.close_session(proto_request).await })?;
        Ok(())
    }

    fn pump(&self) -> Vec<AgentDomainEvent> {
        self.events.take()
    }

    /// The sequence of the last wire event whose translation the latest `pump` handed over in full.
    /// A turn's end held back for a steer's follow-up, and what follows it, are not counted until
    /// they are handed over.
    fn pumped_through(&self) -> Option<u64> {
        self.events.taken_through()
    }

    fn event_signal(&self) -> Option<Arc<crate::provider::EventSignal>> {
        Some(Arc::clone(&self.events.signal))
    }
    #[cfg(target_os = "linux")]
    fn mover(&self) -> Option<&dyn crate::chat_mover::ChatMover> {
        Some(self)
    }
}

/// The events received off the wire and not yet taken by `pump`, with the signal that says so.
/// Whatever becomes ready raises it, and so does a move of the ready position with nothing to hand
/// over, so neither an event nor a position can be queued without whoever drains `pump` being woken.
///
/// Every event passes the steer gate (`steer::SteerGate`) on its way in, under the same lock as the
/// ready list, so a turn's end held for its follow-up and the follow-up's start become ready in one
/// step and one `pump()` returns both.
/// How far the watch may have read past the last event folded for a move to go ahead. The sidecar's
/// replay ring keeps the newest 1000 events of a session (see `watch.rs`), and the taking side
/// replays from the fold; half of it leaves room for what arrives between the freeze and the commit.
#[cfg(target_os = "linux")]
const MOVE_UNFOLDED_LIMIT: u64 = 500;

struct EventQueue {
    state: Mutex<QueueState>,
    signal: Arc<crate::provider::EventSignal>,
    /// How long a held turn end waits for its follow-up turn: `steer::FOLLOW_UP_WAIT`, shorter only
    /// in tests.
    follow_up_wait: Duration,
}

#[derive(Default)]
struct QueueState {
    ready: Vec<AgentDomainEvent>,
    gate: steer::SteerGate<ProtoSteerTurnRequest>,
    /// The watch's position after the last wire event queued.
    latest: Option<WatchMark>,
    /// The position after the last wire event whose translation is all in `ready` (or was taken):
    /// `latest`, except while the gate holds a turn's end and what follows it.
    ready_through: Option<WatchMark>,
    /// `ready_through` as of the latest `take`: where the watch stood after the last event the
    /// conversation folded. A move hands this on, not `latest`, since the watch reads ahead of the fold.
    taken: Option<WatchMark>,
    /// The steer gate as of the latest `take`, which is what a taking side's gate must start from: the
    /// events after `taken` are replayed through it there. `Some` when the gate could not be handed
    /// over then (a message open, a turn's end held).
    busy_at_take: Option<steer::SteerBusy>,
    /// The gate's given-up messages as of the latest `take`; meaningful only while `busy_at_take` is
    /// `None`. A delivery read after the take for one of them removes it from the live gate, and the
    /// replay over there needs it still there.
    given_up_at_take: Vec<(String, String)>,
    /// A message was handed to the gate after the latest `take`. Its delivery or refusal may already
    /// have passed the gate here, unfolded, and a taking side's gate would have no record of it.
    opened_since_take: bool,
    /// Frozen for a move: `take` hands over nothing, so the position taken stays where it is.
    frozen: bool,
    /// What the message sent last asks the CLI about its effort, until the turn it started ends.
    effort_ask: Option<EffortAskInFlight>,
    /// How many changes of the model or the effort have been answered.
    applied_changes: u64,
    /// `send_steers` tasks running.
    steering: usize,
    /// Answers to questions whose reply has not been read yet.
    answering: usize,
}

/// An effort command on its way, and the turn `SendTurn` said it runs in once it has said.
struct EffortAskInFlight {
    ask: effort_reply::EffortAsk,
    turn: Option<String>,
    /// Turns that ended before `SendTurn` answered, with what each stated: a local command can finish first, and
    /// only the answer says which of them was this one.
    ended_before_named: Vec<(String, Option<String>)>,
}

impl QueueState {
    /// Brings `ready_through` up to the latest event once nothing is held back.
    fn settle(&mut self) {
        if !self.gate.holding() {
            self.ready_through = self.latest.clone();
        }
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::with_follow_up_wait(steer::FOLLOW_UP_WAIT)
    }
}

impl EventQueue {
    fn with_follow_up_wait(follow_up_wait: Duration) -> Self {
        Self {
            state: Mutex::default(),
            signal: Arc::default(),
            follow_up_wait,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Brings the ready position up to the latest event once nothing is held back, and wakes the reader
    /// when it moved. A wire event can translate to no domain event and still move the position `take`
    /// reports; without the wake that position would wait for the next event to be handed over, and a
    /// session whose last event is such a one would report a fold short of it until something else came.
    fn settle(&self, state: &mut QueueState) {
        let before = state.ready_through.as_ref().map(|mark| mark.sequence);
        state.settle();
        if state.ready_through.as_ref().map(|mark| mark.sequence) != before {
            self.signal.raise();
        }
    }

    /// Makes `events` ready, in order, and wakes the reader when there are any.
    fn deliver(&self, state: &mut QueueState, events: Vec<AgentDomainEvent>) {
        if events.is_empty() {
            return;
        }
        state.ready.extend(events);
        self.signal.raise();
    }

    /// One event this provider makes itself (a lost stream), with no position on the wire. Runs on
    /// the provider's runtime, like `push_wire`.
    fn push(self: &Arc<Self>, event: AgentDomainEvent) {
        let hold_started = {
            let mut state = self.lock();
            let passed = state.gate.pass(event);
            self.deliver(&mut state, passed.ready);
            self.settle(&mut state);
            passed.hold_started
        };
        if let Some(generation) = hold_started {
            self.arm_hold(generation);
        }
    }

    /// The translation of one wire event, in order, and the watch's position after it, together
    /// under one lock. Runs on the provider's runtime, the only place the stream is read: a hold it
    /// begins arms a timer task there, which sleeps rather than blocks.
    fn push_wire(self: &Arc<Self>, events: Vec<AgentDomainEvent>, mark: WatchMark) {
        let mut holds = Vec::new();
        {
            let mut state = self.lock();
            for event in events {
                let passed = state.gate.pass(event);
                self.deliver(&mut state, passed.ready);
                holds.extend(passed.hold_started);
            }
            state.latest = Some(mark);
            self.settle(&mut state);
        }
        for generation in holds {
            self.arm_hold(generation);
        }
    }

    /// The queue of a session taken over from another process: its watch stood at `mark`, everything
    /// up to it was folded there, and `given_up` are the messages that process had reported dropped.
    #[cfg(target_os = "linux")]
    fn taking_over(mark: WatchMark, given_up: Vec<(String, String)>) -> Self {
        let queue = Self::default();
        {
            let mut state = queue.lock();
            state.gate = steer::SteerGate::taken_over(given_up);
            state.given_up_at_take = state.gate.hand_over().unwrap_or_default();
            state.latest = Some(mark.clone());
            state.ready_through = Some(mark.clone());
            state.taken = Some(mark);
        }
        queue
    }

    /// Stops handing events over, for a move, and returns where the watch stood after the last event
    /// taken, with the steer gate as it stood at that take: the taking side replays the events after
    /// that position through it. Refused, nothing changed, while a message is being steered into the
    /// turn, when one was open at the last take or was handed in since (its delivery or refusal may
    /// have passed the gate here unfolded), and when the watch has read so far past the fold that
    /// the sidecar's replay ring may no longer reach back to it.
    #[cfg(target_os = "linux")]
    fn freeze(&self) -> Result<(WatchMark, Vec<(String, String)>), MoveRefusedWhy> {
        let busy = |busy| match busy {
            steer::SteerBusy::Open => MoveRefusedWhy::SteerOpen,
            steer::SteerBusy::HoldingTurnEnd => MoveRefusedWhy::HoldingTurnEnd,
        };
        let mut state = self.lock();
        state.gate.hand_over().map_err(busy)?;
        if state.steering > 0 {
            return Err(MoveRefusedWhy::SteerInFlight);
        }
        if state.answering > 0 {
            return Err(MoveRefusedWhy::AnswerInFlight);
        }
        if let Some(at_take) = state.busy_at_take {
            return Err(busy(at_take));
        }
        if state.opened_since_take {
            return Err(MoveRefusedWhy::SteerOpen);
        }
        let taken = state.taken.clone().unwrap_or_default();
        let read = state.latest.as_ref().map_or(0, |mark| mark.sequence);
        if read.saturating_sub(taken.sequence) > MOVE_UNFOLDED_LIMIT {
            return Err(MoveRefusedWhy::FoldBehind);
        }
        state.frozen = true;
        Ok((taken, state.given_up_at_take.clone()))
    }

    /// Hands events over again after a freeze whose move did not happen; what came meanwhile is there.
    #[cfg(target_os = "linux")]
    fn thaw(&self) {
        self.lock().frozen = false;
        self.signal.raise();
    }

    /// Hands nothing over any more: the session is driven from another process now.
    #[cfg(target_os = "linux")]
    fn close_for_good(&self) {
        self.lock().frozen = true;
    }

    fn taken_through(&self) -> Option<u64> {
        self.lock().taken.as_ref().map(|mark| mark.sequence)
    }

    /// The watch stream broke. A hold in force stops counting its wait: what it waits for may be in the
    /// replay, which comes only once the stream is open again.
    fn stream_lost(&self) {
        self.lock().gate.stream_lost();
    }

    /// The watch stream is open again. A hold still in force gets a whole new wait, which the replay
    /// that follows has to beat. Runs on the provider's runtime, like `push`.
    fn stream_back(self: &Arc<Self>) {
        let generation = self.lock().gate.stream_back();
        if let Some(generation) = generation {
            self.arm_hold(generation);
        }
    }

    /// A timer task on the provider's runtime, which sleeps rather than blocks.
    fn arm_hold(self: &Arc<Self>, generation: u64) {
        let queue = Arc::clone(self);
        let wait = self.follow_up_wait;
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            queue.expire_hold(generation);
        });
    }

    fn expire_hold(&self, generation: u64) {
        let mut state = self.lock();
        let out = state.gate.expire(generation);
        self.deliver(&mut state, out);
        self.settle(&mut state);
    }

    fn open_steer(
        &self,
        client_id: &str,
        as_typed: String,
        request: ProtoSteerTurnRequest,
    ) -> Result<Option<ProtoSteerTurnRequest>, ProviderError> {
        let mut state = self.lock();
        let opened = state.gate.open(client_id, as_typed, request)?;
        state.opened_since_take = true;
        Ok(opened)
    }

    /// A prompt about to be sent with `SendTurn` under `client_id` (`SteerGate::note_sent`). Counts as
    /// handed over after the latest take, as a steered message does: its delivery may pass the gate
    /// here before the next take.
    /// A message is on its way. Noted before the send, since a local command's reply can reach the watch before
    /// `SendTurn` is answered. An effort command ends what was known at once: from here until its reply the level is
    /// not known, and a tab that moves in between must not carry the old one on.
    fn note_effort_ask(&self, ask: Option<effort_reply::EffortAsk>) {
        let mut state = self.lock();
        if ask.is_some() {
            // Past this a command speaks for the session, so a change's second word has nothing to add.
            state.applied_changes += 1;
        }
        state.effort_ask = ask.map(|ask| EffortAskInFlight {
            ask,
            turn: None,
            ended_before_named: Vec::new(),
        });
        if ask.is_some() {
            self.deliver(&mut state, vec![AgentDomainEvent::EffortReported { effort: None }]);
        }
    }

    /// `SendTurn` named the turn the message runs in, so only that turn's end answers it: Claude Code can finish
    /// work of its own first, and what that work says is the model's.
    fn note_effort_turn(&self, turn_id: &str) {
        let mut state = self.lock();
        let Some(pending) = &mut state.effort_ask else {
            return;
        };
        pending.turn = Some(turn_id.to_string());
        let Some(at) = pending.ended_before_named.iter().position(|(turn, _)| turn == turn_id) else {
            return;
        };
        let effort = pending.ended_before_named.swap_remove(at).1;
        state.effort_ask = None;
        if effort.is_some() {
            self.deliver(&mut state, vec![AgentDomainEvent::EffortReported { effort }]);
        }
    }

    /// A command that may change the effort has been sent and its reply has not come.
    fn effort_ask_open(&self) -> bool {
        self.lock().effort_ask.is_some()
    }

    /// What a change left, said now and once more after the watch has caught up: the answer and the stream
    /// are two routes, and a `SessionReady` written ahead of the change can come out of the watch after its
    /// answer. Only the latest change says it again, and not past a command that may have changed it since.
    fn settle_applied(queue: &Arc<Self>, applied: crate::AppliedSettings) {
        let change = queue.note_applied(&applied);
        let queue = Arc::clone(queue);
        tokio::spawn(async move {
            tokio::time::sleep(APPLIED_AGAIN_AFTER).await;
            if queue.lock().applied_changes == change && !queue.effort_ask_open() {
                queue.note_applied_again(&applied);
            }
        });
    }

    fn note_applied(&self, applied: &crate::AppliedSettings) -> u64 {
        let mut state = self.lock();
        state.applied_changes += 1;
        let change = state.applied_changes;
        self.deliver_applied(&mut state, applied);
        change
    }

    fn note_applied_again(&self, applied: &crate::AppliedSettings) {
        let mut state = self.lock();
        self.deliver_applied(&mut state, applied);
    }

    /// A read that answered always names the model: an empty one did not answer, and says nothing of either, so
    /// the next `SessionReady` does. An empty effort beside a model is one: the model takes no level.
    fn deliver_applied(&self, state: &mut QueueState, applied: &crate::AppliedSettings) {
        if applied.model.is_empty() {
            return;
        }
        let said = vec![
            AgentDomainEvent::ModelReported {
                model: applied.model.clone(),
            },
            AgentDomainEvent::EffortReported {
                effort: (!applied.effort.is_empty()).then(|| applied.effort.clone()),
            },
        ];
        self.deliver(state, said);
    }

    fn forget_effort_ask(&self) {
        self.lock().effort_ask = None;
    }

    /// The effort a turn's end states in answer to the message that started it. A refused or failed command has
    /// no answer to read, and the level stays not known.
    fn effort_answer(&self, event: &AgentDomainEvent) -> Option<AgentDomainEvent> {
        let AgentDomainEvent::TurnCompleted {
            turn_id,
            outcome,
            result_text,
            ..
        } = event
        else {
            return None;
        };
        let mut state = self.lock();
        let pending = state.effort_ask.as_mut()?;
        let effort = (*outcome == crate::TurnOutcome::Completed)
            .then(|| effort_reply::reported(pending.ask, result_text))
            .flatten();
        match pending.turn.as_deref() {
            None => pending.ended_before_named.push((turn_id.clone(), effort)),
            Some(turn) if turn == turn_id => {
                state.effort_ask = None;
                return effort.is_some().then_some(AgentDomainEvent::EffortReported { effort });
            }
            Some(_) => {}
        }
        None
    }

    fn note_sent(&self, client_id: &str, as_typed: String) {
        let mut state = self.lock();
        state.gate.note_sent(client_id, as_typed);
        state.opened_since_take = true;
    }

    fn forget_sent(&self, client_id: &str) {
        self.lock().gate.forget_sent(client_id);
    }

    /// Records an accepted message; the next one to send, if one waits.
    fn steer_accepted(&self, client_id: &str) -> Option<(String, ProtoSteerTurnRequest)> {
        self.lock().gate.accepted(client_id)
    }

    fn steer_refused(&self, client_id: &str, reason: &str) {
        let mut state = self.lock();
        let out = state.gate.refused(client_id, reason);
        self.deliver(&mut state, out);
        self.settle(&mut state);
    }

    fn steer_unknown(&self, client_id: &str) {
        let mut state = self.lock();
        let out = state.gate.unknown(client_id);
        self.deliver(&mut state, out);
        self.settle(&mut state);
    }

    fn take(&self) -> Vec<AgentDomainEvent> {
        let mut state = self.lock();
        if state.frozen {
            return Vec::new();
        }
        state.taken = state.ready_through.clone();
        // Every gate transition so far is reflected in what this take hands over (a hold keeps
        // `ready_through` back, and then the gate is busy), so the gate now is the gate as of `taken`.
        match state.gate.hand_over() {
            Ok(given_up) => {
                state.busy_at_take = None;
                state.given_up_at_take = given_up;
            }
            Err(busy) => {
                state.busy_at_take = Some(busy);
                state.given_up_at_take.clear();
            }
        }
        state.opened_since_take = false;
        std::mem::take(&mut state.ready)
    }

    /// Frozen for a move, or for good once handed on.
    fn frozen(&self) -> bool {
        self.lock().frozen
    }

    /// Everything received and not yet taken, held events included.
    fn len(&self) -> usize {
        let state = self.lock();
        state.ready.len() + state.gate.held_len()
    }
}

/// Sends steered messages one at a time, each once the one before it was answered, recording every
/// answer in the gate where it arrives. Runs on the provider's runtime and only awaits.
///
/// The message is recorded before it is sent, so a delivery that races the answer still finds its typed
/// text, and a turn that ends meanwhile is held for its follow-up. An answer that does not come in time,
/// or a transport failure after the request may have gone, leaves the message open for its outcome:
/// the CLI may have it, and reporting it dropped would have the host send it again.
/// One `send_steers` task counted as running, from before it is spawned until it ends or is dropped.
struct Steering(Arc<EventQueue>);

impl Steering {
    fn begin(events: &Arc<EventQueue>) -> Self {
        events.lock().steering += 1;
        Steering(Arc::clone(events))
    }
}

impl Drop for Steering {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.steering = state.steering.saturating_sub(1);
    }
}

/// One answer to a question counted as out, from before it is sent until its reply was read (the
/// handle holding this is dropped).
struct Answering(Arc<EventQueue>);

impl Answering {
    fn begin(events: &Arc<EventQueue>) -> Self {
        events.lock().answering += 1;
        Answering(Arc::clone(events))
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.answering = state.answering.saturating_sub(1);
    }
}

async fn send_steers(
    client: RuntimeServiceClient<Channel>,
    events: Arc<EventQueue>,
    first: (String, ProtoSteerTurnRequest),
    _steering: Steering,
) {
    let mut next = Some(first);
    while let Some((id, request)) = next.take() {
        let mut client = client.clone();
        let answer = tokio::time::timeout(UNARY_RPC_TIMEOUT, client.steer_turn(request)).await;
        next = match answer {
            Ok(Ok(_)) => events.steer_accepted(&id),
            Ok(Err(status)) => match steer_failure(status) {
                SteerFailure::Refused(error) => {
                    let reason = match error {
                        ProviderError::Provider { message, .. } => message,
                        other => other.to_string(),
                    };
                    events.steer_refused(&id, &reason);
                    None
                }
                SteerFailure::Unknown(why) => {
                    eprintln!(
                        "agent: ClaudeSidecarProvider: SteerTurn failed in transport ({why}); the message may have reached Claude, so its outcome is awaited"
                    );
                    events.steer_unknown(&id);
                    None
                }
            },
            Err(_) => {
                eprintln!(
                    "agent: ClaudeSidecarProvider: SteerTurn was not answered in {UNARY_RPC_TIMEOUT:?}; the message may have reached Claude, so its outcome is awaited"
                );
                events.steer_unknown(&id);
                None
            }
        };
    }
}

/// The wire form of one answer, or the refusal to send it.
///
/// `PermissionDecision` is destructured here, at the wire, and nowhere else: the wire's
/// `allow`/`reason`/`defer` fields are the reason the enum has exactly its three variants. An
/// approval or a deferral sends no reason because there is no field on the far side that would ever
/// show it.
///
/// A deferral goes only to a provider whose sessions asked for the CLI's auto mode: on any other it
/// would hand the call to the CLI's ordinary ask, which is not what deferring means here, so it is
/// refused rather than sent. Verdandi refuses one on a provider prompt, together with `allow`, or on
/// a switchable session too; this is the host-side half.
fn resolve_request_for(
    capabilities: &ProviderCapabilities,
    request: ResolvePermissionRequest,
) -> Result<ProtoResolvePermissionRequest, ProviderError> {
    if request.decision.defers() && !capabilities.cli_auto_mode {
        return Err(ProviderError::UnsupportedCapability("permission_defer"));
    }
    Ok(ProtoResolvePermissionRequest {
        session_id: request.session_id,
        command_id: uuid::Uuid::new_v4().to_string(),
        permission_id: request.permission_id,
        allow: request.decision.allows(),
        reason: request.decision.reason().unwrap_or_default().to_string(),
        defer: request.decision.defers(),
    })
}

/// How far under the handshake's limit an answer stays: the room gRPC's own framing needs, and a margin
/// for a sidecar that counts a little differently.
const ANSWER_MARGIN_BYTES: u64 = 4 * 1024;

/// What the model reads for an answer too long to send. An answer over the limit would fail with the
/// call left open until the CLI's own limit runs out, so a short error goes in its place.
const ANSWER_TOO_LONG: &str = "The tool's answer was too long to send.";

fn answer_block(text: String) -> ClientToolContent {
    ClientToolContent {
        kind: Some(client_tool_content::Kind::Text(text)),
    }
}

/// The wire's form of one answer. `max_message_bytes` is the handshake's; 0 says the peer stated no
/// limit, since proto3 cannot tell that from a limit of nothing.
fn answer_request_for(
    answer: ClientToolAnswer,
    command_id: String,
    max_message_bytes: u64,
) -> AnswerClientToolCallRequest {
    let ClientToolAnswer {
        session_id,
        call_id,
        text,
        is_error,
    } = answer;
    let mut request = AnswerClientToolCallRequest {
        session_id,
        command_id,
        call_id,
        content: text.into_iter().map(answer_block).collect(),
        is_error,
    };
    let limit = max_message_bytes.saturating_sub(ANSWER_MARGIN_BYTES);
    if max_message_bytes != 0 && prost::Message::encoded_len(&request) as u64 >= limit {
        request.content = vec![answer_block(ANSWER_TOO_LONG.to_string())];
        request.is_error = true;
    }
    request
}

/// Whether a failed answer is one the host has nothing to learn from: a first answer won, the call
/// settled some other way, or the session closed, each of which ended the call the answer was for.
fn answer_refusal_is_ordinary(failure: &ProviderError) -> bool {
    matches!(
        failure,
        ProviderError::Provider {
            code: ProviderErrorCode::ClientToolCallNotFound
                | ProviderErrorCode::ClientToolCallAlreadyAnswered
                | ProviderErrorCode::SessionNotFound,
            ..
        }
    )
}

/// The one line a failed answer leaves. It reaches nobody else: the send was never waited on.
fn answer_failure_line(call_id: &str, failure: &ProviderError) -> String {
    if answer_refusal_is_ordinary(failure) {
        format!("agent: ClaudeSidecarProvider: answer to client tool call {call_id} not needed: {failure}")
    } else {
        format!("agent: ClaudeSidecarProvider: answer to client tool call {call_id} failed: {failure}")
    }
}

fn client_call_state(response: GetClientToolCallStateResponse) -> ClientToolCallState {
    let GetClientToolCallStateResponse { through_sequence, open } = response;
    ClientToolCallState {
        through_sequence,
        open: open
            .into_iter()
            .map(|call| ClientCallRef {
                call_id: call.call_id,
                tool_use_id: Some(call.tool_use_id).filter(|id| !id.is_empty()),
                tool: call.tool,
            })
            .collect(),
    }
}

/// The `ResolveInteraction` for the person's answer: option indexes, typed text and notes, each
/// question by its index in the set, and nothing else. The sidecar builds the tool's input from the
/// question it kept, so no text the page shows or sends becomes tool input, and no request this
/// client makes carries an input of its own.
fn resolve_interaction_request_for(
    request: crate::ResolveInteractionRequest,
    command_id: String,
) -> claude_runtime_protocol::v1::ResolveInteractionRequest {
    use claude_runtime_protocol::v1 as wire;
    let answers = |answers: Vec<crate::QuestionAnswer>| wire::QuestionAnswers {
        answers: answers
            .into_iter()
            .map(|answer| {
                let crate::QuestionAnswer {
                    question_index,
                    option_indexes,
                    other_text,
                    notes,
                } = answer;
                wire::QuestionAnswer {
                    question_index,
                    option_indexes,
                    other_text,
                    notes,
                }
            })
            .collect(),
    };
    let crate::ResolveInteractionRequest {
        session_id,
        interaction_id,
        resolution,
    } = request;
    let result = match resolution {
        crate::InteractionResolution::Answers(given) => {
            wire::resolve_interaction_request::Result::Answers(answers(given))
        }
        crate::InteractionResolution::Decline { kind, so_far } => {
            // What was answered so far goes only with a request to chat; a cancel carries none.
            let (kind, so_far) = match kind {
                crate::QuestionDeclineKind::Cancel => (wire::QuestionDeclineKind::Cancel, None),
                crate::QuestionDeclineKind::Chat => (wire::QuestionDeclineKind::Chat, Some(answers(so_far))),
            };
            wire::resolve_interaction_request::Result::Decline(wire::QuestionDecline {
                kind: kind as i32,
                so_far,
            })
        }
    };
    wire::ResolveInteractionRequest {
        session_id,
        command_id,
        interaction_id,
        result: Some(result),
    }
}

/// Records how far behind production one delivered event was.
///
/// `occurred_at` is stamped by the sidecar the moment the event is drained from the kernel's pump,
/// so the difference from local wall-clock time is the whole transport-plus-queue delay. Both ends
/// are on this machine, so clock skew is not a factor; a negative value would mean the two clocks
/// disagree anyway, and is discarded rather than recorded as a negative lag.
fn record_delivery_lag(counters: &BackpressureCounters, occurred_at_millis: i64) {
    counters.events_received.fetch_add(1, Ordering::Relaxed);
    if occurred_at_millis <= 0 {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let lag = now - occurred_at_millis;
    if lag < 0 {
        return;
    }
    counters.last_delivery_lag_ms.store(lag, Ordering::Relaxed);
    counters.max_delivery_lag_ms.fetch_max(lag, Ordering::Relaxed);
}

/// What decides how a session's wire events translate, as the provider that watched it held it.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplaySettings {
    /// The sidecar advertised `text_delta_message_id`, so a change of message id opens a new assistant message.
    pub splits_messages: bool,
    /// The permission mode the session asked the CLI to run in.
    pub requested_cli_mode: crate::RequestedCliMode,
}

/// A steered message the host handed the provider, at the point of the recorded wire it was handed over.
#[cfg(feature = "test-support")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplaySteer {
    /// How many wire events were recorded when the host made the call.
    pub after_events: usize,
    pub client_message_id: String,
    /// What the user typed: the text the delivery event is given.
    pub as_typed: String,
    pub outcome: ReplaySteerOutcome,
}

/// How the sidecar answered a steered message.
#[cfg(feature = "test-support")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplaySteerOutcome {
    Accepted,
    Refused(String),
    /// No answer reached the provider: the CLI may have the message.
    Unknown,
    /// Not a steer: a prompt sent with `SendTurn` under this id, which Claude Code names back only when
    /// it reads it after work of its own, or drops it.
    Sent,
}

/// Feeds recorded wire events through the translation the live watch applies (sequence check, message
/// splitting, `translate::translate`, the steer gate with its typed-text fill-in) and returns the domain
/// events the host's `pump` would have handed over. The gate's wait for a follow-up turn runs on the events'
/// own clock (`occurred_at`) and ends at the last event.
#[cfg(feature = "test-support")]
pub fn replay_frames(
    frames: &[claude_runtime_protocol::v1::SessionEvent],
    steers: &[ReplaySteer],
    settings: ReplaySettings,
) -> Result<Vec<AgentDomainEvent>, String> {
    let wait_ms = i64::try_from(steer::FOLLOW_UP_WAIT.as_millis()).unwrap_or(i64::MAX);
    let mut gate: steer::SteerGate<()> = steer::SteerGate::default();
    let mut tracker = watch::SequenceTracker::new();
    let mut split = translate::MessageSplit::new(settings.splits_messages);
    let mut out: Vec<AgentDomainEvent> = Vec::new();
    // The generation of the hold in force and the time its turn end arrived.
    let mut hold: Option<(u64, i64)> = None;
    let mut pending: Vec<&ReplaySteer> = steers.iter().collect();
    pending.sort_by_key(|s| s.after_events);
    let mut pending = pending.into_iter().peekable();

    let hand_in = |gate: &mut steer::SteerGate<()>, out: &mut Vec<AgentDomainEvent>, steer: &ReplaySteer| {
        if steer.outcome == ReplaySteerOutcome::Sent {
            gate.note_sent(&steer.client_message_id, steer.as_typed.clone());
            return Ok(());
        }
        gate.open(&steer.client_message_id, steer.as_typed.clone(), ())
            .map_err(|e| format!("steered message {}: {e}", steer.client_message_id))?;
        match &steer.outcome {
            ReplaySteerOutcome::Accepted => {
                gate.accepted(&steer.client_message_id);
            }
            ReplaySteerOutcome::Refused(reason) => out.extend(gate.refused(&steer.client_message_id, reason)),
            ReplaySteerOutcome::Unknown => out.extend(gate.unknown(&steer.client_message_id)),
            ReplaySteerOutcome::Sent => unreachable!("noted above"),
        }
        Ok::<(), String>(())
    };

    for (at, event) in frames.iter().enumerate() {
        while let Some(steer) = pending.next_if(|s| s.after_events <= at) {
            hand_in(&mut gate, &mut out, steer)?;
        }
        if let Some((generation, since)) = hold {
            if event.occurred_at.saturating_sub(since) >= wait_ms {
                out.extend(gate.expire(generation));
                hold = None;
            }
        }
        match tracker.observe(event.sequence) {
            watch::SequenceVerdict::Deliver => {}
            watch::SequenceVerdict::Duplicate => continue,
            watch::SequenceVerdict::Lost { first, last } => return Err(watch::events_lost_reason(first, last)),
            watch::SequenceVerdict::Invalid => return Err("an event with the reserved sequence 0".into()),
        }
        let mut translated = Vec::new();
        if let Some(boundary) = split.before(event) {
            translated.push(boundary);
        }
        translated.extend(translate::translate(event.clone(), settings.requested_cli_mode));
        for domain_event in translated {
            let passed = gate.pass(domain_event);
            out.extend(passed.ready);
            if let Some(generation) = passed.hold_started {
                hold = Some((generation, event.occurred_at));
            }
        }
        if !gate.holding() {
            hold = None;
        }
    }
    for steer in pending {
        hand_in(&mut gate, &mut out, steer)?;
    }
    if let Some((generation, _)) = hold {
        out.extend(gate.expire(generation));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_readiness_override_replaces_only_the_host_side() {
        let shipped = QuestionPolicy::resolve(true, None);
        assert_eq!(shipped.client_implements, CLIENT_IMPLEMENTS_QUESTIONS);
        assert!(shipped.capable);
        let opted_in = QuestionPolicy::resolve(true, Some(true));
        assert!(opted_in.capable && opted_in.client_implements);
        let opted_out = QuestionPolicy::resolve(true, Some(false));
        assert!(opted_out.capable && !opted_out.client_implements);
        let incapable = QuestionPolicy::resolve(false, Some(true));
        assert!(!incapable.capable && incapable.client_implements);
    }

    #[test]
    fn append_is_exact_static_panel_context() {
        let expected = concat!(
            "This session was started by Eitri, a desktop IDE built around Neovim. While the conversation is shown in Eitri's ",
            "agent panel, the user edits code in Neovim beside it, and the panel renders your replies as GitHub-flavored Markdown, ",
            "not in a terminal (images appear as links and are not loaded), shows each tool call with edits drawn as diffs, and asks the user with a ",
            "card when a tool call needs approval. A file path relative to the project root that you write in a reply, optionally ",
            "with `:line` (for example `src/main.rs:42`), can be opened in the editor from the panel. A tool call can also be denied ",
            "by Eitri itself, for example when a permission request expires, a session closes or a turn is interrupted. Those denial ",
            "messages say what happened; a denial without a reason does not identify who denied it.",
        );
        assert_eq!(EITRI_APPEND, expected);
        assert!(!EITRI_APPEND.contains(['\r', '\n']));
    }

    #[test]
    fn only_the_named_capability_selects_the_prompt() {
        let mut peer = real_handshake_today();
        assert!(claude_code_prompt_from_handshake(&peer));
        peer.capabilities.clear();
        assert!(!claude_code_prompt_from_handshake(&peer));
        peer.capabilities = vec!["system_prompt".into(), "list_commands".into()];
        assert!(!claude_code_prompt_from_handshake(&peer));
        peer.capabilities.push("system_prompt_preset".into());
        assert!(claude_code_prompt_from_handshake(&peer));
    }

    #[test]
    fn preset_requests_follow_the_capability() {
        use prost::Message as _;
        for offered in [true, false] {
            for project in [ProjectTrust::Trusted, ProjectTrust::Untrusted] {
                for user_settings in [true, false] {
                    for cli_mode in [crate::RequestedCliMode::Default, crate::RequestedCliMode::Auto] {
                        for resume in [None, Some("claude-id".to_string())] {
                            let request = build_create_request_loading(
                                "/p".into(),
                                StreamingPreference::Partial,
                                resume.clone(),
                                false,
                                true,
                                offered,
                                user_settings,
                                project,
                                cli_mode,
                                QuestionPolicy::default(),
                            );
                            let decoded =
                                ProtoCreateSessionRequest::decode(request.encode_to_vec().as_slice()).unwrap();
                            assert_eq!(decoded.system_prompt, None);
                            assert_eq!(
                                decoded.system_prompt_preset,
                                offered.then(|| SystemPromptPreset {
                                    append: Some(EITRI_APPEND.into()),
                                })
                            );
                            assert_eq!(decoded.resume_provider_session_id, resume);
                            if !offered {
                                let expected = ProtoCreateSessionRequest {
                                    cwd: "/p".into(),
                                    policy: Some(ClaudeHostPolicy {
                                        configuration: ConfigurationProfile::Native as i32,
                                        permissions: ProtoPermissionMode::Interactive as i32,
                                        persistence: PersistenceMode::HostCli as i32,
                                        executable: ExecutableSource::HostCli as i32,
                                        streaming: StreamingMode::Partial as i32,
                                        setting_sources: Some(SettingSourceSelection {
                                            sources: proto_sources(crate::setting_sources::for_session(
                                                user_settings,
                                                project,
                                            )),
                                        }),
                                        tool_policy: Some(ToolPolicy {
                                            deny: vec![],
                                            unrestricted: true,
                                            allow: None,
                                        }),
                                        permission_mode_switchable: false,
                                        provider_permission_prompts: true,
                                        cli_permission_mode: match cli_mode {
                                            crate::RequestedCliMode::Auto => CliPermissionMode::Auto as i32,
                                            crate::RequestedCliMode::Default => CliPermissionMode::Unspecified as i32,
                                        },
                                        accept_unsupported_cli: false,
                                        per_task_stop: false,
                                    }),
                                    resume_provider_session_id: resume,
                                    fork: false,
                                    client_interactions: None,
                                    client_tools: None,
                                    model: None,
                                    effort: None,
                                    system_prompt: None,
                                    system_prompt_preset: None,
                                    output_format: None,
                                    await_account_identity: false,
                                };
                                assert_eq!(request.encode_to_vec(), expected.encode_to_vec());
                            }
                        }
                    }
                }
            }
            let fresh = fresh_request(
                CreateSessionRequest {
                    cwd: "/p".into(),
                    streaming: StreamingPreference::Partial,
                    project: ProjectTrust::Untrusted,
                    client_tools: None,
                },
                true,
                offered,
                crate::RequestedCliMode::Default,
                QuestionPolicy::default(),
                false,
            );
            let resumed = resume_request(
                ResumeSessionRequest {
                    provider_session_id: "claude-id".into(),
                    cwd: "/p".into(),
                    streaming: StreamingPreference::Partial,
                    project: ProjectTrust::Untrusted,
                    client_tools: None,
                },
                true,
                offered,
                crate::RequestedCliMode::Default,
                QuestionPolicy::default(),
                false,
            );
            assert_eq!(fresh.system_prompt_preset.is_some(), offered);
            assert_eq!(resumed.system_prompt_preset, fresh.system_prompt_preset);
            assert_eq!(resumed.resume_provider_session_id.as_deref(), Some("claude-id"));
        }
    }

    /// Fresh, resumed and forked sidecar requests all use `INTERACTIVE`, are never switchable,
    /// and state an empty deny list as `unrestricted`, regardless of the tab's mode. The request
    /// carries no tab mode. Verdandi installs its `PreToolUse` broker for `INTERACTIVE`
    /// (`permissions !== 'bypass'` in `packages/claude-runtime/src/session.ts`), and `unrestricted`
    /// keeps its `CONSERVATIVE_BYPASS_DENY` floor off, leaving the gate present with no tool denied.
    #[test]
    fn every_request_is_gated_never_switchable_and_restricts_no_tool() {
        let shapes = [crate::RequestedCliMode::Default, crate::RequestedCliMode::Auto].map(|cli_mode| {
            [
                (
                    "fresh",
                    build_create_request(
                        "/tmp/p".into(),
                        StreamingPreference::Partial,
                        None,
                        false,
                        true,
                        false,
                        cli_mode,
                        ProjectTrust::Untrusted,
                        QuestionPolicy::default(),
                        None,
                    ),
                ),
                (
                    "resume",
                    build_create_request(
                        "/tmp/p".into(),
                        StreamingPreference::Partial,
                        Some("claude-id".into()),
                        false,
                        true,
                        false,
                        cli_mode,
                        ProjectTrust::Untrusted,
                        QuestionPolicy::default(),
                        None,
                    ),
                ),
                (
                    "fork",
                    build_create_request(
                        "/tmp/p".into(),
                        StreamingPreference::Complete,
                        Some("claude-id".into()),
                        true,
                        true,
                        false,
                        cli_mode,
                        ProjectTrust::Untrusted,
                        QuestionPolicy::default(),
                        None,
                    ),
                ),
            ]
        });
        for (shape, request) in shapes.into_iter().flatten() {
            let policy = request.policy.expect("a policy is always sent");
            assert_eq!(
                policy.permissions,
                ProtoPermissionMode::Interactive as i32,
                "{shape}: every session is gated"
            );
            assert!(
                !policy.permission_mode_switchable,
                "{shape}: never switchable -- `true` sets --allow-dangerously-skip-permissions"
            );
            let tools = policy.tool_policy.expect("a tool policy is always stated");
            assert!(
                tools.unrestricted && tools.deny.is_empty(),
                "{shape}: no tool is denied, and an empty list is stated rather than left as silence: {tools:?}"
            );
            assert!(
                tools.allow.is_none(),
                "{shape}: absent, not empty: the base tool set is untouched"
            );
        }
    }

    /// The CLI's own permission prompts are asked for on every session this client
    /// creates -- all of them gated -- exactly when the handshake advertised the capability, and the
    /// flag is the ONLY thing that moves: the policy is otherwise the one `every_request_is_gated_...`
    /// pins, `allow` still absent (Verdandi refuses an allow list naming the three prompt tools the
    /// flag would add, and Eitri has none).
    #[test]
    fn the_clis_own_prompts_are_asked_for_exactly_when_the_sidecar_offers_them() {
        assert!(provider_prompts_from_handshake(&real_handshake_today()));
        let mut older = real_handshake_today();
        older.capabilities.retain(|c| c != CAP_PROVIDER_PERMISSION_PROMPTS);
        assert!(
            !provider_prompts_from_handshake(&older),
            "an older sidecar is never asked"
        );

        for offered in [true, false] {
            for (shape, resume, fork) in [
                ("fresh", None, false),
                ("resume", Some("claude-id".to_string()), false),
                ("fork", Some("claude-id".to_string()), true),
            ] {
                let policy = build_create_request(
                    "/tmp/p".into(),
                    StreamingPreference::Partial,
                    resume,
                    fork,
                    offered,
                    false,
                    crate::RequestedCliMode::Default,
                    ProjectTrust::Untrusted,
                    QuestionPolicy::default(),
                    None,
                )
                .policy
                .expect("a policy is always sent");
                assert_eq!(policy.provider_permission_prompts, offered, "{shape}");
                assert_eq!(policy.permissions, ProtoPermissionMode::Interactive as i32, "{shape}");
                assert!(!policy.permission_mode_switchable, "{shape}");
                assert!(
                    policy.tool_policy.expect("stated").allow.is_none(),
                    "{shape}: no allow list, so none of the three prompt tools can be named in one"
                );
            }
        }
    }

    const SHAPES: [(&str, Option<&str>, bool); 3] = [
        ("fresh", None, false),
        ("resume", Some("claude-id"), false),
        ("fork", Some("claude-id"), true),
    ];

    fn sources_sent(user_settings: bool, project: ProjectTrust, resume: Option<&str>, fork: bool) -> Option<Vec<i32>> {
        build_create_request_loading(
            "/p".into(),
            StreamingPreference::Partial,
            resume.map(str::to_string),
            fork,
            false,
            false,
            user_settings,
            project,
            crate::RequestedCliMode::Default,
            QuestionPolicy::default(),
        )
        .policy
        .expect("a policy is always sent")
        .setting_sources
        .map(|selection| selection.sources)
    }

    const ALL_THREE: [i32; 3] = [
        SettingSource::User as i32,
        SettingSource::Project as i32,
        SettingSource::Local as i32,
    ];

    #[test]
    fn an_untrusted_sidecar_request_names_only_user() {
        for (shape, resume, fork) in SHAPES {
            assert_eq!(
                build_create_request_loading(
                    "/p".into(),
                    StreamingPreference::Partial,
                    resume.map(str::to_string),
                    fork,
                    false,
                    false,
                    true,
                    ProjectTrust::Untrusted,
                    crate::RequestedCliMode::Default,
                    QuestionPolicy::default(),
                )
                .policy
                .expect("a policy is always sent")
                .setting_sources,
                Some(SettingSourceSelection {
                    sources: vec![SettingSource::User as i32]
                }),
                "{shape}"
            );
        }
    }

    /// An absent selection defers to the configuration, which loads every tier, so "no tier" has to
    /// be a present, empty list -- in the struct and after a trip over the wire.
    #[test]
    fn an_untrusted_request_without_user_settings_sends_a_present_empty_list() {
        use prost::Message as _;
        for (shape, resume, fork) in SHAPES {
            let request = build_create_request_loading(
                "/p".into(),
                StreamingPreference::Partial,
                resume.map(str::to_string),
                fork,
                false,
                false,
                false,
                ProjectTrust::Untrusted,
                crate::RequestedCliMode::Default,
                QuestionPolicy::default(),
            );
            let selection = request.policy.clone().expect("a policy is always sent").setting_sources;
            assert_ne!(selection, None, "{shape}: never absent");
            assert_eq!(selection, Some(SettingSourceSelection { sources: vec![] }), "{shape}");

            let decoded = ProtoCreateSessionRequest::decode(request.encode_to_vec().as_slice()).expect("decodes");
            assert_eq!(
                decoded.policy.expect("a policy is always sent").setting_sources,
                Some(SettingSourceSelection { sources: vec![] }),
                "{shape}: the empty selection is still present after encoding"
            );
        }
    }

    #[test]
    fn a_trusted_request_names_all_three() {
        for (shape, resume, fork) in SHAPES {
            assert_eq!(
                sources_sent(true, ProjectTrust::Trusted, resume, fork),
                Some(ALL_THREE.to_vec()),
                "{shape}"
            );
        }
    }

    #[test]
    fn a_trusted_request_without_user_names_project_and_local() {
        for (shape, resume, fork) in SHAPES {
            assert_eq!(
                sources_sent(false, ProjectTrust::Trusted, resume, fork),
                Some(vec![SettingSource::Project as i32, SettingSource::Local as i32]),
                "{shape}"
            );
        }
    }

    /// A resume states the trust it is given now; nothing carries the original session's tiers
    /// over. The fresh path is held to the same.
    #[test]
    fn a_resume_request_carries_the_sessions_trust() {
        let resume = |project| {
            resume_request(
                ResumeSessionRequest {
                    provider_session_id: "claude-id".into(),
                    cwd: "/p".into(),
                    streaming: StreamingPreference::Partial,
                    project,
                    client_tools: None,
                },
                false,
                false,
                crate::RequestedCliMode::Default,
                QuestionPolicy::default(),
                false,
            )
        };
        let fresh = |project| {
            fresh_request(
                CreateSessionRequest {
                    cwd: "/p".into(),
                    streaming: StreamingPreference::Partial,
                    project,
                    client_tools: None,
                },
                false,
                false,
                crate::RequestedCliMode::Default,
                QuestionPolicy::default(),
                false,
            )
        };
        let sources = |request: ProtoCreateSessionRequest| request.policy.unwrap().setting_sources.unwrap().sources;

        let untrusted = resume(ProjectTrust::Untrusted);
        assert_eq!(untrusted.resume_provider_session_id.as_deref(), Some("claude-id"));
        assert!(!untrusted.fork);
        assert_eq!(sources(untrusted), vec![SettingSource::User as i32]);
        assert_eq!(sources(resume(ProjectTrust::Trusted)), ALL_THREE.to_vec());

        let fresh_untrusted = fresh(ProjectTrust::Untrusted);
        assert_eq!(fresh_untrusted.resume_provider_session_id, None);
        assert_eq!(sources(fresh_untrusted), vec![SettingSource::User as i32]);
        assert_eq!(sources(fresh(ProjectTrust::Trusted)), ALL_THREE.to_vec());
    }

    /// `prefix i`'s `settings` row (`setting_sources::note_for_session`) says what a session loads,
    /// so the request has to ask for exactly that: for each of the four selections, on a fresh, a
    /// resumed and a forked session alike, the tiers sent are the ones the sentence names, in the
    /// proto enum's order, and the selection is stated on every request, never left to the CLI's
    /// own default.
    #[test]
    fn every_request_loads_the_tiers_the_note_says() {
        for (user_settings, project, expected, note_says) in [
            (
                true,
                ProjectTrust::Trusted,
                ALL_THREE.to_vec(),
                "user + project + local",
            ),
            (
                false,
                ProjectTrust::Trusted,
                vec![SettingSource::Project as i32, SettingSource::Local as i32],
                "project + local only",
            ),
            (
                true,
                ProjectTrust::Untrusted,
                vec![SettingSource::User as i32],
                "user only",
            ),
            (false, ProjectTrust::Untrusted, vec![], "nothing"),
        ] {
            for (shape, resume, fork) in SHAPES {
                assert_eq!(
                    sources_sent(user_settings, project, resume, fork),
                    Some(expected.clone()),
                    "{shape}, user_settings={user_settings}, {project:?}"
                );
            }
            let note = crate::setting_sources::note_for_session(user_settings, project);
            assert!(
                note.starts_with(note_says),
                "user_settings={user_settings}, {project:?}: the note says what loads: {note}"
            );
        }
    }

    /// With nothing configured -- the state every test runs in, and the shipped default -- the
    /// request every session goes through loads the user tier, and the project's tiers only when
    /// the session is trusted.
    #[test]
    fn the_default_request_loads_the_user_tier_and_the_project_only_when_trusted() {
        let sent = |project| {
            build_create_request(
                "/p".into(),
                StreamingPreference::Partial,
                None,
                false,
                false,
                false,
                crate::RequestedCliMode::Default,
                project,
                QuestionPolicy::default(),
                None,
            )
            .policy
            .expect("a policy is always sent")
            .setting_sources
            .expect("stated")
            .sources
        };
        assert_eq!(sent(ProjectTrust::Untrusted), vec![SettingSource::User as i32]);
        assert_eq!(sent(ProjectTrust::Trusted), ALL_THREE.to_vec());
    }

    /// The shape the real sidecar returns at the revision this crate pins
    /// (`BUNDLED_SIDECAR_REVISION`) -- transcribed from
    /// `apps/claude-sidecar/src/runtimeServiceImpl.ts`'s handshake handler, not invented.
    ///
    /// **This is a hand-maintained record, NOT a drift detector.** Nothing in Rust observes the real
    /// sidecar: when Verdandi adds a capability, changes a permission mode, or bumps its protocol
    /// major, every test below keeps passing against this frozen literal, and it stays wrong until a
    /// human opens this file and edits it. That is exactly what happened once already -- this
    /// fixture sat at `protocol_major: 1` with seven capabilities while `CLIENT_PROTOCOL_MAJOR` was
    /// 2 and the pinned sidecar advertised nine, so it described a peer `connect()` would have
    /// REFUSED, and `cargo test -p agent` was green throughout.
    ///
    /// What these tests do earn is the reverse direction: they pin `capabilities_from_handshake`'s
    /// own logic against a realistic input, so a change to the intersection rule, a mistyped
    /// capability constant, or a silently-dropped `ProviderInfo` field fails here. For a check that
    /// a real sidecar still says this, see
    /// `agent/tests/claude_sidecar_unary_conformance.rs::the_live_handshake_still_matches_the_fixture_in_mod_rs`
    /// -- `#[ignore]`d, real-process, and costs no model tokens.
    ///
    /// Two of these fields are not protocol facts and that live test deliberately does not pin
    /// them: `claude_agent_sdk_version` and the two `*claude_code_version`s are whatever happens to
    /// be installed (the run on 2026-09-15 reported CLI 2.1.272 against the 2.1.269 written here).
    /// They stay as plausible sample values, because the tests below only check that
    /// `info_from_handshake` carries them through unchanged.
    fn real_handshake_today() -> HandshakeResponse {
        HandshakeResponse {
            protocol_major: 3,
            // The handshake reports `capabilities.json`'s minor (it was the literal 0 until
            // 2026-09-30): 23 at the pinned revision.
            protocol_minor: 23,
            sidecar_version: "0.3.0".into(),
            claude_agent_sdk_version: "0.3.0".into(),
            sdk_declared_claude_code_version: "2.1.269".into(),
            actual_claude_code_version: "2.1.269".into(),
            capabilities: [
                "handshake",
                "create_session",
                "send_turn",
                "watch_session_events",
                "interrupt_turn",
                "resolve_permission",
                "close_session",
                // Not RPC names: resume and fork are parameters of `CreateSession`, so the sidecar
                // advertises them explicitly because a client cannot discover them from the service
                // definition.
                "resume_session",
                "fork_session",
                "setting_sources",
                "tool_policy",
                // Added alongside `CreateSessionRequest`'s own `model`/`effort`/
                // `system_prompt`/`output_format` fields (`build_create_request` sends none of
                // them, so this fixture change is capability-list-only). Read by nothing in this
                // crate yet.
                "session_model",
                "session_effort",
                "system_prompt",
                "output_format",
                "structured_output",
                "turn_usage",
                // account_binding/account_name/account_config_dir on HandshakeResponse itself.
                "account_identity",
                "init_fingerprint",
                "tool_allow_list",
                // Still advertised by the pinned sidecar, but this client never switches the CLI's
                // permission mode, so no capability is derived from it.
                "set_permission_mode",
                // Capability 'text_delta_message_id' -- see CAP_TEXT_DELTA_MESSAGE_ID.
                "text_delta_message_id",
                // `ClaudeHostPolicy.provider_permission_prompts`,
                // the CLI's own permission prompts routed to the host as `PermissionRequested`
                // with origin PROVIDER_PROMPT. Transcribed from Verdandi's handshake literal
                // (`runtimeServiceImpl.ts`), which puts it last before the executable entries.
                "provider_permission_prompts",
                // Protocol 3.13, all unread by this client (`build_create_request` sends neither
                // `init_check` nor `await_account_identity`).
                "init_check",
                "await_account_identity",
                "egress_probe",
                "structured_output_tools",
                "effective_tool_report",
                // Protocol 3.14: the CLI's own auto mode under the gate, the deferring
                // answer, and the CLI's refusals as events. The first two together are
                // `ProviderCapabilities::cli_auto_mode`. Since 2026-09-30 the list is built from
                // `capabilities.json` in its order, which is the order here.
                "cli_auto_mode",
                "permission_defer",
                "permission_denied_events",
                // Protocol 3.15-3.19, transcribed from `capabilities.json` (minors 15-19); none is read by
                // this client.
                "cli_compatibility_report",
                "gated_cli_mode_default",
                "effective_claude_config_dir",
                "startup_report_file",
                "effective_session_report",
                "extra_mcp_servers",
                "extra_mcp_servers_http",
                // Protocol 3.20: `steer_turn` is `ProviderCapabilities::steer`, and is
                // advertised only when the sidecar's CLI is at least 2.1.252, which every CLI this
                // client meets today is. `list_commands` is the `CommandsUpdated` event, translated
                // whenever it arrives.
                "steer_turn",
                "list_commands",
                "system_prompt_preset",
                // Protocol 3.22, in `capabilities.json`'s order: one `ProviderCapabilities`
                // flag each. `task_output` is conditional (the sidecar must be able to open a task's
                // output file safely), and present on the Linux hosts this fixture describes.
                "cli_turns",
                "human_origin",
                "subagent_frames",
                "background_tasks",
                "workflow_progress",
                "task_control",
                "task_output",
                "session_notices",
                "conversation_reset",
                "structured_questions_v1",
                // Protocol 3.23: `ProviderCapabilities::client_tools`.
                "client_tools",
                // Protocol 3.24: the first three are `ProviderCapabilities::direct_settings`, the last
                // `task_output_range`. The other two are facts of the stream, read as they arrive.
                "set_model",
                "list_models",
                "apply_effort",
                "session_effort_report",
                "cli_mode_reports",
                "task_output_range",
                // Which executable sources this build can actually serve, advertised as capability
                // strings rather than a new wire field. `executable_host_cli` is always present;
                // a checkout build adds one more, `executable_sdk_bundled`, which a PACKAGED build
                // cannot serve at all (the SDK resolves its own CLI through `createRequire`, and a
                // single-file artifact has no node_modules for it to find). This fixture carries
                // the PACKAGED shape because that is what a shipped install meets.
                //
                // Measured 2026-09-18 against the real artifact
                // (verdandi-claude-sidecar-0.1.0-linux-x64), not transcribed from a commit
                // message: a live Handshake returned exactly the first twelve of this list, and a
                // CreateSession carrying SDK_BUNDLED was refused with INVALID_CONFIGURATION naming
                // host_cli. The rest (session_model onward) is transcribed from the proto
                // definition, not independently measured against a live sidecar -- see the live
                // conformance test's own note on that gap.
                "executable_host_cli",
                // `egress_restricted` is the one other conditional entry (present only when the
                // sidecar's own config enables it) and is deliberately NOT in this fixture, so the
                // live conformance test's tolerance for it is exercised against whatever this host
                // actually has, not hidden by a fixture that always includes it.
            ]
            .iter()
            .map(ToString::to_string)
            .collect(),
            configuration_profiles: vec!["native".into(), "isolated".into()],
            permission_modes: vec!["interactive".into(), "verdandi_rules".into(), "bypass".into()],
            max_message_bytes: 4 * 1024 * 1024,
            event_buffer_policy: "bounded-1000".into(),
            // account_binding/account_name/account_config_dir: the account-identity fields.
            // AccountBinding::Unspecified (the i32 default, 0) means "a sidecar older than these
            // fields", which is an honest reading for a fixture that predates them being measured.
            ..Default::default()
        }
    }

    #[test]
    fn todays_real_sidecar_advertises_interrupt_bypass_and_resume_but_this_client_withholds_fork() {
        let capabilities = capabilities_from_handshake(&real_handshake_today());
        assert!(
            capabilities.interrupt,
            "interrupt_turn is advertised and works (proven by the conformance suite)"
        );
        assert!(
            capabilities.bypass_permission_mode,
            "bypass is advertised -- a reported fact only, never requested"
        );
        assert!(
            capabilities.interactive_permission_mode,
            "interactive is advertised, and is the only policy any session is created under"
        );
        assert!(
            capabilities.resume,
            "resume_session is advertised AND CLIENT_IMPLEMENTS_RESUME is true"
        );
        assert!(
            !capabilities.fork,
            "fork_session IS advertised on the wire -- this reports false because CLIENT_IMPLEMENTS_FORK is still false"
        );
    }

    /// A sidecar without the gated policy is refused at session creation, naming what is missing
    /// and what it does offer -- never started under BYPASS instead.
    #[test]
    fn a_sidecar_without_the_gated_policy_is_refused_loudly() {
        let today = real_handshake_today();
        let info = info_from_handshake(&today, None, Vec::new());
        assert!(require_interactive(&capabilities_from_handshake(&today), &info).is_ok());

        let mut bypass_only = real_handshake_today();
        bypass_only.permission_modes.retain(|m| m == "bypass");
        let info = info_from_handshake(&bypass_only, None, Vec::new());
        match require_interactive(&capabilities_from_handshake(&bypass_only), &info) {
            Err(ProviderError::Provider {
                code: ProviderErrorCode::InvalidConfiguration,
                message,
            }) => {
                assert!(message.contains("a gated (interactive) mode"), "{message}");
                assert!(message.contains("advertises: bypass"), "{message}");
            }
            other => panic!("expected a loud refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_capability_the_provider_does_not_advertise_is_never_reported() {
        let mut response = real_handshake_today();
        response.capabilities.retain(|c| c != "interrupt_turn");
        assert!(!capabilities_from_handshake(&response).interrupt);
    }

    #[test]
    fn a_capability_needs_both_the_provider_advertising_it_and_this_client_implementing_it() {
        // Both halves have to bite, and today each is demonstrated by a different capability.
        //
        // The client half was written for a day that has since arrived. Verdandi added
        // `resume_session` to the wire while `resume_session()` here still returned
        // `UnsupportedCapability`, and reporting the advertisement alone would have put a Resume
        // control in the UI that could not work. Resume is genuinely implemented now, so `fork`
        // carries that half: `fork_session` IS advertised by the pinned sidecar, and this still
        // reports false, because `CLIENT_IMPLEMENTS_FORK` is false.
        let advertised = capabilities_from_handshake(&real_handshake_today());
        assert!(advertised.resume, "advertised by the sidecar and implemented here");
        assert!(!advertised.fork, "advertised by the sidecar, not implemented here");

        // The provider half: implementing a call is not licence to claim it against a peer that
        // never offered it. A sidecar too old to know the word must read as no-resume even though
        // `CLIENT_IMPLEMENTS_RESUME` is true -- which the first assertion above just established.
        let mut older = real_handshake_today();
        older.capabilities.retain(|c| c != CAP_RESUME_SESSION);
        assert!(!capabilities_from_handshake(&older).resume);
    }

    /// Steering is offered exactly when the sidecar advertises `steer_turn`, which it does only for a
    /// CLI new enough to report what became of a message: without it a Claude tab's Enter holds.
    #[test]
    fn steering_is_offered_exactly_when_the_sidecar_advertises_it() {
        assert!(capabilities_from_handshake(&real_handshake_today()).steer);
        let mut older = real_handshake_today();
        older.capabilities.retain(|c| c != CAP_STEER_TURN);
        assert!(!capabilities_from_handshake(&older).steer);
        assert_eq!(CAP_STEER_TURN, "steer_turn", "the word the sidecar advertises");
    }

    /// Reads one flag of a `ProviderCapabilities`.
    type Flag = fn(&ProviderCapabilities) -> bool;

    /// The protocol 3.22 capabilities, each with the flag it alone sets.
    fn capabilities_of_3_22() -> [(&'static str, Flag); 10] {
        [
            ("cli_turns", |c| c.cli_turns),
            ("human_origin", |c| c.human_origin),
            ("subagent_frames", |c| c.subagent_frames),
            ("background_tasks", |c| c.background_tasks),
            ("workflow_progress", |c| c.workflow_progress),
            ("task_control", |c| c.task_control),
            ("task_output", |c| c.task_output),
            ("session_notices", |c| c.session_notices),
            ("conversation_reset", |c| c.conversation_reset),
            ("structured_questions_v1", |c| c.structured_questions_v1),
        ]
    }

    /// A handshake listing one 3.22 capability sets exactly its flag and leaves every other flag
    /// where a handshake without any of them puts it; listing none sets none.
    /// A picker needs its list and both changes, so the three are one flag: a sidecar short of any of them
    /// keeps the slash commands.
    #[test]
    fn direct_settings_needs_all_three_capabilities() {
        let all = real_handshake_today();
        assert!(capabilities_from_handshake(&all).direct_settings);
        for missing in [CAP_SET_MODEL, CAP_LIST_MODELS, CAP_APPLY_EFFORT] {
            let mut short = all.clone();
            short.capabilities.retain(|c| c != missing);
            assert!(!capabilities_from_handshake(&short).direct_settings, "{missing}");
        }
        let mut no_range = all.clone();
        no_range.capabilities.retain(|c| c != CAP_TASK_OUTPUT_RANGE);
        let without = capabilities_from_handshake(&no_range);
        assert!(without.direct_settings && !without.task_output_range);
    }

    #[test]
    fn each_protocol_3_22_capability_sets_exactly_its_own_flag() {
        let names: Vec<&str> = capabilities_of_3_22().iter().map(|(name, _)| *name).collect();
        assert_eq!(
            [
                CAP_CLI_TURNS,
                CAP_HUMAN_ORIGIN,
                CAP_SUBAGENT_FRAMES,
                CAP_BACKGROUND_TASKS,
                CAP_WORKFLOW_PROGRESS,
                CAP_TASK_CONTROL,
                CAP_TASK_OUTPUT,
                CAP_SESSION_NOTICES,
                CAP_CONVERSATION_RESET,
                CAP_STRUCTURED_QUESTIONS_V1,
            ]
            .to_vec(),
            names,
            "the words the sidecar advertises"
        );
        let all = capabilities_from_handshake(&real_handshake_today());
        for (name, flag) in capabilities_of_3_22() {
            assert!(flag(&all), "{name} is advertised by the pinned sidecar");
        }

        let mut without = real_handshake_today();
        without.capabilities.retain(|c| !names.contains(&c.as_str()));
        let base = capabilities_from_handshake(&without);
        assert_eq!(base, all_off(all), "no 3.22 flag without its word");

        for (name, _) in capabilities_of_3_22() {
            let mut one = without.clone();
            one.capabilities.push(name.to_string());
            let got = capabilities_from_handshake(&one);
            for (other, flag) in capabilities_of_3_22() {
                assert_eq!(flag(&got), other == name, "advertising {name} alone, {other}");
            }
            assert_eq!(all_off(got), base, "advertising {name} moves no flag from before 3.22");
        }
    }

    #[test]
    fn client_tools_is_read_from_the_handshake_alone() {
        assert!(capabilities_from_handshake(&real_handshake_today()).client_tools);
        let mut without = real_handshake_today();
        without.capabilities.retain(|c| c != "client_tools");
        assert!(!capabilities_from_handshake(&without).client_tools);
    }

    /// `capabilities` with every 3.22 flag cleared.
    fn all_off(capabilities: ProviderCapabilities) -> ProviderCapabilities {
        ProviderCapabilities {
            cli_turns: false,
            human_origin: false,
            subagent_frames: false,
            background_tasks: false,
            workflow_progress: false,
            task_control: false,
            task_output: false,
            session_notices: false,
            conversation_reset: false,
            structured_questions_v1: false,
            ..capabilities
        }
    }

    /// The field numbers of one encoded protobuf message's own fields, in wire order.
    fn wire_field_numbers(mut bytes: &[u8]) -> Vec<u64> {
        fn varint(bytes: &mut &[u8]) -> u64 {
            let mut value = 0u64;
            for shift in (0..64).step_by(7) {
                let (byte, rest) = bytes.split_first().expect("a complete varint");
                *bytes = rest;
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            value
        }
        let mut numbers = Vec::new();
        while !bytes.is_empty() {
            let key = varint(&mut bytes);
            numbers.push(key >> 3);
            match key & 7 {
                0 => {
                    varint(&mut bytes);
                }
                1 => bytes = &bytes[8..],
                2 => {
                    let len = usize::try_from(varint(&mut bytes)).expect("a length");
                    bytes = &bytes[len..];
                }
                5 => bytes = &bytes[4..],
                other => panic!("wire type {other} is not one prost writes"),
            }
        }
        numbers
    }

    /// An absent peer capability leaves questions absent; task control adds only its policy field.
    #[test]
    // The two opt-ins are constants on purpose; asserting them is the point, and flipping one must
    // fail here until the request carries what it asks for.
    #[allow(clippy::assertions_on_constants)]
    fn create_requests_declare_per_task_stop_exactly_with_task_control() {
        use prost::Message as _;
        assert!(CLIENT_IMPLEMENTS_PER_TASK_STOP);
        assert!(CLIENT_IMPLEMENTS_QUESTIONS);
        let offered = capabilities_from_handshake(&real_handshake_today());
        assert!(offered.task_control && offered.structured_questions_v1);
        for cli_mode in [crate::RequestedCliMode::Default, crate::RequestedCliMode::Auto] {
            for (resume, fork) in [(None, false), (Some("claude-id".to_string()), true)] {
                for claude_code_prompt in [false, true] {
                    let request = build_create_request_loading(
                        "/p".into(),
                        StreamingPreference::Partial,
                        resume.clone(),
                        fork,
                        true,
                        claude_code_prompt,
                        true,
                        ProjectTrust::Trusted,
                        cli_mode,
                        QuestionPolicy::default(),
                    );
                    assert!(request.client_interactions.is_none());
                    let mut policy = request.policy.clone().expect("a policy is always sent");
                    assert!(!policy.per_task_stop);
                    let fields = wire_field_numbers(&request.encode_to_vec());
                    assert!(!fields.contains(&11), "request fields {fields:?}");
                    assert!(fields.iter().all(|n| *n <= 10), "request fields {fields:?}");
                    let fields = wire_field_numbers(&policy.encode_to_vec());
                    assert!(!fields.contains(&12), "policy fields {fields:?}");
                    assert!(fields.iter().all(|n| *n <= 11), "policy fields {fields:?}");
                    for capable in [false, true] {
                        policy.per_task_stop = per_task_stop_policy(capable, None);
                        assert_eq!(policy.per_task_stop, capable);
                        let fields = wire_field_numbers(&policy.encode_to_vec());
                        assert_eq!(fields.contains(&12), capable);
                        for readiness in [false, true] {
                            assert_eq!(per_task_stop_policy(capable, Some(readiness)), capable && readiness);
                        }
                    }
                }
            }
        }
    }

    /// Each ordinary refusal leaves its call settled or gone, so the line says the answer was not
    /// needed; any other failure is named as one.
    #[test]
    fn a_refused_answer_is_ordinary_only_for_a_call_or_session_that_is_over() {
        let provider = |code| ProviderError::Provider {
            code,
            message: "refused".into(),
        };
        for code in [
            ProviderErrorCode::ClientToolCallNotFound,
            ProviderErrorCode::ClientToolCallAlreadyAnswered,
            ProviderErrorCode::SessionNotFound,
        ] {
            assert!(answer_refusal_is_ordinary(&provider(code)));
            assert!(answer_failure_line("c1", &provider(code)).contains("c1 not needed"));
        }
        for failure in [
            provider(ProviderErrorCode::InvalidConfiguration),
            ProviderError::Timeout,
            ProviderError::Transport("the pipe closed".into()),
        ] {
            assert!(!answer_refusal_is_ordinary(&failure));
            assert!(answer_failure_line("c1", &failure).contains("c1 failed"));
        }
    }

    #[test]
    fn a_peer_that_states_no_limit_gets_the_answer_as_it_is() {
        let answer = ClientToolAnswer {
            session_id: "s".into(),
            call_id: "c".into(),
            text: vec!["x".repeat(16 * 1024)],
            is_error: false,
        };
        let request = answer_request_for(answer.clone(), "id".into(), 0);
        assert!(!request.is_error);
        let request = answer_request_for(answer, "id".into(), 8 * 1024);
        assert!(request.is_error, "a stated limit holds");
    }

    fn detail(code: ProtoErrorCode, reason: &str, message: &str) -> ErrorDetail {
        ErrorDetail {
            code: code as i32,
            message: message.to_string(),
            reason: reason.to_string(),
        }
    }

    fn refusal_words(error: ProviderError) -> (ProviderErrorCode, String) {
        match error {
            ProviderError::Provider { code, message } => (code, message),
            other => panic!("expected a provider error, got {other}"),
        }
    }

    /// Each refusal reads as a reason the panel can show after "held until the turn ends:", and keeps
    /// its code.
    #[test]
    fn a_refused_steer_is_worded_for_the_held_note_and_keeps_its_code() {
        let cases = [
            (
                detail(ProtoErrorCode::NoActiveTurn, "", "no turn is running"),
                ProviderErrorCode::NoActiveTurn,
                "the turn had ended when it reached Claude",
            ),
            (
                detail(ProtoErrorCode::TurnMismatch, "", "turn t1 is not the running turn"),
                ProviderErrorCode::TurnMismatch,
                "the turn it was typed in had ended when it reached Claude",
            ),
            (
                detail(ProtoErrorCode::SteerUnavailable, "cli_lifecycle_not_reported", "x"),
                ProviderErrorCode::SteerUnavailable,
                "Claude has not yet said this session can take messages mid-turn",
            ),
            (
                detail(ProtoErrorCode::SteerUnavailable, "first_turn_held", "x"),
                ProviderErrorCode::SteerUnavailable,
                "the session's first turn has not reached Claude yet",
            ),
            (
                detail(ProtoErrorCode::SteerUnavailable, "a_reason_from_a_newer_sidecar", "x"),
                ProviderErrorCode::SteerUnavailable,
                "Claude cannot take messages mid-turn now",
            ),
            (
                detail(ProtoErrorCode::SessionNotFound, "", "no such session"),
                ProviderErrorCode::SessionNotFound,
                "the session has ended",
            ),
            (
                detail(
                    ProtoErrorCode::InvalidConfiguration,
                    "steer_text_empty",
                    "a steered message needs text that is not only whitespace",
                ),
                ProviderErrorCode::InvalidConfiguration,
                "Claude did not take it: a steered message needs text that is not only whitespace",
            ),
        ];
        for (wire, code, words) in cases {
            assert_eq!(refusal_words(steer_refusal(wire)), (code, words.to_string()));
        }
        let long = "y".repeat(MAX_STEER_REASON_CHARS + 10);
        let (_, words) = refusal_words(steer_refusal(detail(ProtoErrorCode::InvalidConfiguration, "", &long)));
        assert!(words.ends_with('…'), "{words}");
        assert_eq!(
            words.chars().count(),
            "Claude did not take it: ".len() + MAX_STEER_REASON_CHARS + 1
        );
    }

    /// The sidecar's own error means nothing was sent; a status without one (the transport failed)
    /// means the message may have reached the CLI.
    #[test]
    fn only_the_sidecars_own_error_counts_as_not_sent() {
        let bytes = prost::Message::encode_to_vec(&detail(ProtoErrorCode::TurnMismatch, "", "stale"));
        let refused = tonic::Status::with_details(tonic::Code::FailedPrecondition, "stale", bytes.into());
        assert!(matches!(
            steer_failure(refused),
            SteerFailure::Refused(ProviderError::Provider {
                code: ProviderErrorCode::TurnMismatch,
                ..
            })
        ));
        let broken = tonic::Status::unavailable("connection reset");
        assert!(matches!(steer_failure(broken), SteerFailure::Unknown(why) if why == "connection reset"));
        assert!(matches!(
            steer_failure(tonic::Status::deadline_exceeded("slow")),
            SteerFailure::Unknown(_)
        ));
        // Rejected before any handler ran: not sent, so dropped at once rather than awaited.
        for (status, words) in [
            (
                tonic::Status::unimplemented("no such method"),
                "this sidecar cannot take messages mid-turn",
            ),
            (
                tonic::Status::resource_exhausted("message too large"),
                "the message is too large to send",
            ),
            (
                tonic::Status::invalid_argument("bad request"),
                "Claude did not take it: bad request",
            ),
        ] {
            assert!(
                matches!(steer_failure(status), SteerFailure::Refused(ProviderError::Provider { message, .. }) if message == words)
            );
        }
    }

    /// The two new refusals are complaints about one message, never a reason to end the session.
    #[test]
    fn the_steer_refusals_are_benign() {
        assert!(ProviderErrorCode::TurnMismatch.is_benign());
        assert!(ProviderErrorCode::SteerUnavailable.is_benign());
        assert_eq!(
            provider_error_code(ProtoErrorCode::SteerUnavailable),
            ProviderErrorCode::SteerUnavailable
        );
    }

    /// These codes each answer one request about one task, question or tool call, which may simply have
    /// ended first; the session stays healthy, so none of them is a reason to end it.
    #[test]
    fn the_task_question_and_call_refusals_map_one_to_one_and_are_benign() {
        for (wire, mirrored) in [
            (ProtoErrorCode::TaskNotFound, ProviderErrorCode::TaskNotFound),
            (
                ProtoErrorCode::TaskOutputUnavailable,
                ProviderErrorCode::TaskOutputUnavailable,
            ),
            (
                ProtoErrorCode::InteractionNotFound,
                ProviderErrorCode::InteractionNotFound,
            ),
            (
                ProtoErrorCode::InteractionAnswerRefused,
                ProviderErrorCode::InteractionAnswerRefused,
            ),
            (
                ProtoErrorCode::ClientToolCallNotFound,
                ProviderErrorCode::ClientToolCallNotFound,
            ),
            (
                ProtoErrorCode::ClientToolCallAlreadyAnswered,
                ProviderErrorCode::ClientToolCallAlreadyAnswered,
            ),
        ] {
            assert_eq!(provider_error_code(wire), mirrored);
            assert!(mirrored.is_benign(), "{mirrored:?}");
        }
    }

    #[test]
    fn what_a_change_left_is_delivered_and_an_unanswered_read_says_nothing() {
        let queue = EventQueue::default();
        queue.note_applied(&crate::AppliedSettings {
            model: "claude-sonnet-5-5".into(),
            effort: "low".into(),
        });
        assert_eq!(
            queue.take(),
            [
                AgentDomainEvent::ModelReported {
                    model: "claude-sonnet-5-5".into()
                },
                AgentDomainEvent::EffortReported {
                    effort: Some("low".into())
                },
            ]
        );
        queue.note_applied(&crate::AppliedSettings {
            model: "old".into(),
            effort: String::new(),
        });
        assert_eq!(
            queue.take(),
            [
                AgentDomainEvent::ModelReported { model: "old".into() },
                AgentDomainEvent::EffortReported { effort: None },
            ],
            "a model that takes no level"
        );
        queue.note_applied(&crate::AppliedSettings::default());
        assert_eq!(queue.take(), [], "a read that did not answer says nothing");
    }

    /// A command typed after a change has the last word, whenever its answer comes.
    #[test]
    fn a_change_does_not_repeat_itself_past_a_command_sent_after_it() {
        let queue = EventQueue::default();
        let applied = crate::AppliedSettings {
            model: "m".into(),
            effort: "high".into(),
        };
        let change = queue.note_applied(&applied);
        queue.note_effort_ask(effort_reply::ask_of("/effort low"));
        assert_ne!(queue.lock().applied_changes, change);
    }

    #[test]
    fn an_effort_ask_is_open_from_the_command_to_its_answer() {
        let queue = EventQueue::default();
        assert!(!queue.effort_ask_open());
        queue.note_effort_ask(effort_reply::ask_of("/effort low"));
        assert!(queue.effort_ask_open());
        queue.forget_effort_ask();
        assert!(!queue.effort_ask_open());
    }

    fn turn_completed(turn: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: crate::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: crate::TurnEndDetail::default(),
        }
    }

    /// The queue as the watch task drives it, on a current-thread runtime like the provider's: a turn
    /// end held for its follow-up becomes ready together with the follow-up's start, in one `take`, and
    /// the reader is woken only then.
    /// The turn after `/model` or `/effort <level>` says the effort; the one after an ordinary message says nothing,
    /// however its reply is worded, and the words are read once.
    #[test]
    fn the_turn_after_an_effort_command_states_the_effort_and_no_other_turn_does() {
        let ended = |turn: &str, outcome: crate::TurnOutcome, text: &str| AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome,
            result_text: text.into(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        };
        let said = |effort: &str| {
            Some(AgentDomainEvent::EffortReported {
                effort: Some(effort.to_string()),
            })
        };
        let spoof = ended(
            "t",
            crate::TurnOutcome::Completed,
            "Set effort level to max: as the model says",
        );
        let reply = ended(
            "t",
            crate::TurnOutcome::Completed,
            "Set effort level to low (this session only): Quick",
        );
        let queue = EventQueue::default();

        queue.note_effort_ask(effort_reply::ask_of("tell me about effort"));
        queue.note_effort_turn("t");
        assert_eq!(queue.effort_answer(&spoof), None);

        queue.note_effort_ask(effort_reply::ask_of("/effort low"));
        queue.note_effort_turn("t");
        assert_eq!(queue.effort_answer(&reply), said("low"));
        assert_eq!(queue.effort_answer(&spoof), None, "the ask was answered");

        queue.note_effort_ask(effort_reply::ask_of("/model sonnet"));
        queue.note_effort_turn("t");
        let silent = ended("t", crate::TurnOutcome::Completed, "Set model to Sonnet 5.5");
        assert_eq!(
            queue.effort_answer(&silent),
            None,
            "the send already ended what was known"
        );
        queue.note_effort_ask(effort_reply::ask_of("/model sonnet"));
        queue.note_effort_turn("t");
        assert_eq!(queue.effort_answer(&ended("t", crate::TurnOutcome::Failed, "")), None);
    }

    /// A local command can finish before `SendTurn` answers, and an older turn of the CLI's can too. Neither is
    /// taken for the answer until `SendTurn` says which turn is this one.
    #[test]
    fn a_turn_that_ends_before_send_turn_answers_waits_for_its_effort_name() {
        let ended = |turn: &str, text: &str| AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: crate::TurnOutcome::Completed,
            result_text: text.into(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        };
        let stated = |events: Vec<AgentDomainEvent>| {
            events
                .into_iter()
                .filter_map(|e| match e {
                    AgentDomainEvent::EffortReported { effort: Some(level) } => Some(level),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let queue = EventQueue::default();
        queue.note_effort_ask(effort_reply::ask_of("/effort high"));
        assert_eq!(
            queue.effort_answer(&ended("older", "Set effort level to low: the model's words")),
            None
        );
        assert_eq!(
            queue.effort_answer(&ended("sent", "Set effort level to high: Comprehensive")),
            None
        );
        assert_eq!(stated(queue.take()), Vec::<String>::new());
        queue.note_effort_turn("sent");
        assert_eq!(stated(queue.take()), ["high"]);
        assert_eq!(
            queue.effort_answer(&ended("sent", "Set effort level to max: again")),
            None,
            "answered once"
        );

        // The older turn is the only one that ended: it answers nothing, and the real reply still can.
        queue.note_effort_ask(effort_reply::ask_of("/effort max"));
        assert_eq!(
            queue.effort_answer(&ended("older", "Set effort level to low: the model's words")),
            None
        );
        queue.note_effort_turn("sent");
        assert_eq!(stated(queue.take()), Vec::<String>::new());
        assert!(queue
            .effort_answer(&ended("sent", "Set effort level to max: Maximum"))
            .is_some());
    }

    /// An effort command ends what was known when it is sent, so nothing older outlives it: not a level it is about to
    /// change, and not one a tab would carry to another window while the reply is on its way.
    #[test]
    fn sending_an_effort_command_ends_what_was_known_at_once() {
        let cleared = |events: Vec<AgentDomainEvent>| {
            events
                .iter()
                .filter(|e| matches!(e, AgentDomainEvent::EffortReported { effort: None }))
                .count()
        };
        let queue = EventQueue::default();
        queue.note_effort_ask(effort_reply::ask_of("/effort low"));
        assert_eq!(cleared(queue.take()), 1);
        queue.note_effort_ask(effort_reply::ask_of("hello"));
        assert_eq!(cleared(queue.take()), 0);
    }

    /// Claude Code can finish work of its own before it reads the message. That turn's words are the model's, however
    /// they are worded, so only the turn `SendTurn` named answers the command, and the ask waits for it.
    #[test]
    fn a_turn_of_the_clis_own_does_not_answer_an_effort_command() {
        let ended = |turn: &str, text: &str| AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: crate::TurnOutcome::Completed,
            result_text: text.into(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        };
        let queue = EventQueue::default();
        queue.note_effort_ask(effort_reply::ask_of("/effort high"));
        queue.note_effort_turn("sent");
        assert_eq!(
            queue.effort_answer(&ended("cli", "Set effort level to low: the model's words")),
            None
        );
        assert_eq!(
            queue.effort_answer(&ended("sent", "Set effort level to high: Comprehensive")),
            Some(AgentDomainEvent::EffortReported {
                effort: Some("high".into())
            })
        );
    }

    #[test]
    fn the_queue_releases_a_held_turn_end_with_the_follow_up_in_one_take() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let queue = Arc::new(EventQueue::default());
        queue
            .open_steer("m1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        queue.steer_accepted("m1");
        let seen = queue.signal.seen();
        runtime.block_on(async {
            queue.push(turn_completed("t1"));
        });
        assert!(queue.take().is_empty(), "held");
        assert_eq!(queue.len(), 1, "a held event still counts as pending");
        assert_eq!(queue.signal.seen(), seen, "nothing ready, nobody woken");
        runtime.block_on(async {
            queue.push(AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
        });
        assert_ne!(queue.signal.seen(), seen, "woken once the pair is ready");
        assert_eq!(
            queue.take(),
            [
                turn_completed("t1"),
                AgentDomainEvent::TurnStarted { turn_id: "t2".into() }
            ]
        );
    }

    /// The timer a hold arms runs on the runtime without blocking it, and when no follow-up comes it
    /// drops the open message ahead of the held end and wakes the reader.
    #[test]
    fn the_queue_timer_drops_the_open_message_when_no_follow_up_comes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let queue = Arc::new(EventQueue::with_follow_up_wait(Duration::from_millis(20)));
        queue
            .open_steer("m1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        queue.steer_accepted("m1");
        let seen = queue.signal.seen();
        runtime.block_on(async {
            queue.push(turn_completed("t1"));
            // Other work on the same runtime goes on while the hold waits.
            tokio::time::sleep(Duration::from_millis(5)).await;
            assert!(queue.take().is_empty());
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert_ne!(queue.signal.seen(), seen);
        assert_eq!(
            queue.take(),
            [
                AgentDomainEvent::UserMessageDropped {
                    client_message_id: "m1".into(),
                    reason: steer::UNREPORTED.into(),
                },
                turn_completed("t1"),
            ]
        );
    }

    /// The watch stream broke after a turn's end was held, and re-opening it took longer than the wait.
    /// Had the wait run out meanwhile, the host would have seen the message dropped and no turn
    /// running, and sent the message as a turn of its own; with the follow-up already over on the
    /// sidecar that turn is taken, and the replayed delivery finds nothing left to take back, so the
    /// words run twice. Nothing is released while the stream is down, and the replay after the
    /// re-opening releases the end with the follow-up and the delivery.
    #[test]
    fn the_queue_does_not_give_up_on_a_follow_up_while_the_stream_reconnects() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let queue = Arc::new(EventQueue::with_follow_up_wait(Duration::from_millis(20)));
        queue
            .open_steer("m1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        queue.steer_accepted("m1");
        let started = AgentDomainEvent::TurnStarted { turn_id: "t2".into() };
        runtime.block_on(async {
            queue.push(turn_completed("t1"));
            queue.stream_lost();
            // A reconnect slower than the wait.
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert!(
            queue.take().is_empty(),
            "nothing given up while the stream is down, so the host has nothing to send again"
        );
        runtime.block_on(async {
            queue.stream_back();
            queue.push(started.clone());
            queue.push(AgentDomainEvent::UserMessageDelivered {
                client_message_id: "m1".into(),
                turn_id: "t2".into(),
                text: String::new(),
            });
            // The wait armed on re-opening finds nothing held.
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert_eq!(
            queue.take(),
            [
                turn_completed("t1"),
                started,
                AgentDomainEvent::UserMessageDelivered {
                    client_message_id: "m1".into(),
                    turn_id: "t2".into(),
                    text: "typed".into(),
                },
            ]
        );
    }

    /// Re-opened, a stream first has its waiting questions read, and the replay is not read meanwhile:
    /// a read slower than a held turn end's wait must not use that wait up, or the steered message
    /// would be reported dropped while its delivery is still in the replay, and sent again.
    #[test]
    fn a_slow_question_list_read_after_a_re_open_does_not_use_up_the_follow_up_wait() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let queue = Arc::new(EventQueue::with_follow_up_wait(Duration::from_millis(20)));
        queue
            .open_steer("m1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        queue.steer_accepted("m1");
        let mut state = WatchState::fresh(true, crate::RequestedCliMode::Default);
        let started = AgentDomainEvent::TurnStarted { turn_id: "t2".into() };
        runtime.block_on(async {
            queue.push(turn_completed("t1"));
            queue.stream_lost();
            let slow_read = async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Some(crate::InteractionState {
                    through_sequence: 0,
                    pending: Vec::new(),
                })
            };
            stream_reopened(&queue, &mut state, Some(slow_read)).await;
            // The replay, read once the list was.
            queue.push(started.clone());
            queue.push(AgentDomainEvent::UserMessageDelivered {
                client_message_id: "m1".into(),
                turn_id: "t2".into(),
                text: String::new(),
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert_eq!(
            queue.take(),
            [
                turn_completed("t1"),
                started,
                AgentDomainEvent::UserMessageDelivered {
                    client_message_id: "m1".into(),
                    turn_id: "t2".into(),
                    text: "typed".into(),
                },
            ]
        );
    }

    /// Re-opened, a stream whose replay brings no follow-up is given up on after a whole new wait.
    #[test]
    fn the_queue_gives_up_after_a_new_wait_once_the_stream_is_back() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let queue = Arc::new(EventQueue::with_follow_up_wait(Duration::from_millis(20)));
        queue
            .open_steer("m1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        queue.steer_accepted("m1");
        runtime.block_on(async {
            queue.push(turn_completed("t1"));
            queue.stream_lost();
            tokio::time::sleep(Duration::from_millis(60)).await;
            queue.stream_back();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert_eq!(
            queue.take(),
            [
                AgentDomainEvent::UserMessageDropped {
                    client_message_id: "m1".into(),
                    reason: steer::UNREPORTED.into(),
                },
                turn_completed("t1"),
            ]
        );
    }

    #[test]
    fn bypass_is_not_reported_when_the_provider_does_not_offer_it() {
        let mut response = real_handshake_today();
        response.permission_modes.retain(|m| m != "bypass");
        assert!(!capabilities_from_handshake(&response).bypass_permission_mode);
    }

    #[test]
    fn info_carries_the_advertised_lists_verbatim_for_diagnostics() {
        let info = info_from_handshake(
            &real_handshake_today(),
            Some("checkout @ abc1234".into()),
            vec!["diag".into()],
        );
        assert_eq!(info.actual_claude_code_version, "2.1.269");
        assert_eq!(info.protocol_major, 3);
        assert_eq!(info.sidecar_version, "0.3.0");
        // Raw, not filtered down to the ones this client recognizes -- an unrecognized future
        // capability must stay visible in diagnostics rather than vanish.
        assert!(info.advertised_permission_modes.contains(&"verdandi_rules".to_string()));
        // Fifty-nine: the PACKAGED shape at the pinned revision (twenty-four up to
        // `provider_permission_prompts`, five from protocol 3.13, three from 3.14, seven from 3.15-3.19, two from 3.20, one from 3.21, ten from
        // 3.22, one from 3.23 and six from 3.24), which a shipped install meets. A checkout build advertises one more,
        // executable_sdk_bundled, and a sidecar with egress restrictions enabled advertises
        // egress_restricted besides. See real_handshake_today().
        assert_eq!(info.advertised_capabilities.len(), 59);
        assert_eq!(info.startup_diagnostics, vec!["diag".to_string()]);
        assert_eq!(info.build_description.as_deref(), Some("checkout @ abc1234"));
    }

    #[test]
    fn the_cli_compatibility_warning_is_picked_out_of_real_sidecar_stderr() {
        // The real diagnostic line, as it arrives through SpawnedSidecar's stderr tail, truncated
        // after "Starting anyway." -- the real message continues with guidance text. Truncated on
        // purpose: this asserts that the FILTER matches, and a filter that only worked on the full
        // sentence would be matching prose rather than the stable marker.
        let lines = vec![
            "Debug: something unrelated".to_string(),
            "claude-sidecar: CLI version diagnostic: claude CLI version 2.1.269 is inside this sidecar's supported range (>=2.1.267 <3.0.0) but has not been tested against it (tested: 2.1.267). Starting anyway.".to_string(),
            "another unrelated line".to_string(),
        ];
        let diagnostics = startup_diagnostics_from_stderr(&lines);
        assert_eq!(diagnostics.len(), 1, "got: {diagnostics:?}");
        assert!(diagnostics[0].contains("2.1.269"));
    }

    #[test]
    fn a_clean_startup_produces_no_diagnostics() {
        assert!(startup_diagnostics_from_stderr(&["nothing notable".to_string()]).is_empty());
        assert!(startup_diagnostics_from_stderr(&[]).is_empty());
    }

    /// The CLI's own auto mode is asked for exactly when the handshake offers both halves of it --
    /// `cli_auto_mode` and `permission_defer` -- and never on the strength of one alone.
    #[test]
    fn the_clis_auto_mode_is_asked_for_only_when_the_sidecar_offers_both_halves() {
        let today = capabilities_from_handshake(&real_handshake_today());
        assert!(today.cli_auto_mode);
        assert_eq!(requested_cli_mode(&today), crate::RequestedCliMode::Auto);
        for missing in [CAP_CLI_AUTO_MODE, CAP_PERMISSION_DEFER] {
            let mut older = real_handshake_today();
            older.capabilities.retain(|c| c != missing);
            let caps = capabilities_from_handshake(&older);
            assert!(!caps.cli_auto_mode, "without {missing}");
            assert_eq!(
                requested_cli_mode(&caps),
                crate::RequestedCliMode::Default,
                "without {missing}"
            );
        }
        assert_eq!(CAP_CLI_AUTO_MODE, "cli_auto_mode");
        assert_eq!(CAP_PERMISSION_DEFER, "permission_defer");
    }

    /// A session that asks for auto differs from one that does not in exactly one field, and the one
    /// that does not leaves it off the wire: an older sidecar is sent what it was always sent. The
    /// gate is the same either way -- INTERACTIVE, never switchable, which is all Verdandi accepts
    /// AUTO with.
    #[test]
    fn asking_for_auto_adds_one_field_and_the_default_request_is_byte_for_byte_unchanged() {
        use prost::Message as _;
        for (resume, fork) in [
            (None, false),
            (Some("claude-id".to_string()), false),
            (Some("claude-id".to_string()), true),
        ] {
            let build = |cli_mode| {
                build_create_request_loading(
                    "/p".into(),
                    StreamingPreference::Partial,
                    resume.clone(),
                    fork,
                    true,
                    false,
                    true,
                    ProjectTrust::Untrusted,
                    cli_mode,
                    QuestionPolicy::default(),
                )
            };
            let default = build(crate::RequestedCliMode::Default);
            let auto = build(crate::RequestedCliMode::Auto);
            let default_policy = default.policy.clone().expect("a policy is always sent");
            let auto_policy = auto.policy.clone().expect("a policy is always sent");
            assert_eq!(
                default_policy.cli_permission_mode,
                CliPermissionMode::Unspecified as i32
            );
            assert_eq!(auto_policy.cli_permission_mode, CliPermissionMode::Auto as i32);
            assert_eq!(auto_policy.permissions, ProtoPermissionMode::Interactive as i32);
            assert!(!auto_policy.permission_mode_switchable);
            assert!(!default.await_account_identity && !auto.await_account_identity);

            // Unset again, the AUTO request is the default one exactly.
            let mut reset = auto.clone();
            reset.policy.as_mut().unwrap().cli_permission_mode = CliPermissionMode::Unspecified as i32;
            assert_eq!(reset.encode_to_vec(), default.encode_to_vec());
            // And the default policy carries no tag 11 at all: proto3 leaves a zero off the wire, so
            // its bytes are what a request built before the field existed encodes to. AUTO adds the
            // field's two bytes (key 0x58, value 2), and nothing else.
            assert_eq!(
                default_policy.encoded_len() + 2,
                auto_policy.encoded_len(),
                "{resume:?} {fork}"
            );
            assert_eq!(
                &auto_policy.encode_to_vec()[default_policy.encoded_len()..],
                &[0x58, 0x02],
                "tag 11 comes last, so the AUTO policy is the default one plus these two bytes"
            );
        }
    }

    /// A deferral is sent only by a provider whose sessions asked for auto, and then as `defer` with
    /// `allow` false and no reason; the other two answers are what they always were, `defer` false.
    #[test]
    fn a_deferral_is_sent_only_where_auto_was_asked_for() {
        let request = |decision| ResolvePermissionRequest {
            session_id: "s".into(),
            permission_id: "p".into(),
            decision,
        };
        let auto = capabilities_from_handshake(&real_handshake_today());
        let wire = resolve_request_for(&auto, request(crate::PermissionDecision::Defer)).expect("sent");
        assert!(wire.defer && !wire.allow && wire.reason.is_empty());

        let mut older = real_handshake_today();
        older.capabilities.retain(|c| c != CAP_PERMISSION_DEFER);
        let without = capabilities_from_handshake(&older);
        assert!(matches!(
            resolve_request_for(&without, request(crate::PermissionDecision::Defer)),
            Err(ProviderError::UnsupportedCapability("permission_defer"))
        ));

        for caps in [auto, without] {
            let allow = resolve_request_for(&caps, request(crate::PermissionDecision::Allow)).unwrap();
            assert!(allow.allow && !allow.defer);
            let deny = resolve_request_for(
                &caps,
                request(crate::PermissionDecision::Deny {
                    reason: Some("no".into()),
                }),
            )
            .unwrap();
            assert!(!deny.allow && !deny.defer && deny.reason == "no");
        }
    }

    #[test]
    fn permission_mode_changed_serializes_with_the_wire_shape_the_panel_snapshot_needs() {
        let event = AgentDomainEvent::PermissionModeChanged {
            mode: crate::PermissionMode::Bypass,
            provider_mode: "bypassPermissions".into(),
            floor_applied: false,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "permission_mode_changed");
        assert_eq!(json["mode"], "bypass");
    }

    fn mark(sequence: u64, last_text: &str) -> WatchMark {
        WatchMark {
            sequence,
            last_text: Some(last_text.to_string()),
            child_replies: Default::default(),
            cli_mode_noted: false,
        }
    }

    fn text(words: &str) -> AgentDomainEvent {
        AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: crate::ContentKind::Text,
            text: words.into(),
        }
    }

    /// The watch reads ahead of the fold, so what a move hands on is where the watch stood after the
    /// last event a pump took, not after the last one it read.
    #[test]
    fn a_pump_records_the_position_of_the_last_event_it_took_not_of_one_read_ahead() {
        let queue = Arc::new(EventQueue::default());
        queue.push_wire(vec![text("one")], mark(3, "m1"));
        assert_eq!(queue.taken_through(), None, "nothing taken yet");
        assert_eq!(queue.take(), [text("one")]);
        assert_eq!(queue.taken_through(), Some(3));
        let boundary = AgentDomainEvent::AssistantMessageBoundary { turn_id: "t1".into() };
        queue.push_wire(vec![boundary.clone(), text("two")], mark(4, "m2"));
        assert_eq!(queue.taken_through(), Some(3), "read, not taken");
        // An event that translates to nothing still moves the position once it is taken.
        queue.push_wire(Vec::new(), mark(5, "m2"));
        assert_eq!(queue.take(), [boundary, text("two")]);
        assert_eq!(queue.taken_through(), Some(5));
    }

    /// A wire event that becomes no domain event still moves the position a take reports, so the reader is
    /// woken to take it: a session whose last event is such a one (the CLI's idle notice after a turn's end)
    /// would otherwise report its fold one event short until the next event arrived.
    #[test]
    fn an_event_that_becomes_nothing_wakes_the_reader_to_take_its_position() {
        let queue = Arc::new(EventQueue::default());
        queue.push_wire(vec![text("one")], mark(3, "m1"));
        assert_eq!(queue.take(), [text("one")]);
        let seen = queue.signal.seen();
        queue.push_wire(Vec::new(), mark(4, "m1"));
        assert_ne!(queue.signal.seen(), seen, "the position moved, so the reader is woken");
        assert!(queue.take().is_empty());
        assert_eq!(queue.taken_through(), Some(4));
        let seen = queue.signal.seen();
        queue.push_wire(Vec::new(), mark(4, "m1"));
        assert_eq!(queue.signal.seen(), seen, "the same position again wakes nobody");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_frozen_queue_hands_nothing_over_and_a_thawed_one_hands_over_what_came_meanwhile() {
        let queue = Arc::new(EventQueue::default());
        queue.push_wire(vec![text("one")], mark(3, "m1"));
        queue.take();
        queue.push_wire(vec![text("two")], mark(4, "m2"));
        let (at, given_up) = queue.freeze().unwrap();
        assert_eq!(at, mark(3, "m1"));
        assert!(given_up.is_empty());
        assert!(queue.take().is_empty(), "frozen");
        assert_eq!(queue.taken_through(), Some(3));
        queue.thaw();
        assert_eq!(queue.take(), [text("two")]);
        assert_eq!(queue.taken_through(), Some(4));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_held_turn_end_is_not_taken_and_refuses_a_freeze_until_it_is_released() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let queue = Arc::new(EventQueue::default());
        queue.push_wire(vec![text("one")], mark(3, "m1"));
        queue.take();
        queue
            .open_steer("s1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        assert!(matches!(queue.freeze(), Err(MoveRefusedWhy::SteerOpen)));
        queue.steer_accepted("s1");
        runtime.block_on(async {
            queue.push_wire(vec![turn_completed("t1")], mark(4, "m1"));
        });
        assert!(queue.take().is_empty(), "held");
        assert_eq!(queue.taken_through(), Some(3), "a held end is not counted as taken");
        assert!(matches!(queue.freeze(), Err(MoveRefusedWhy::HoldingTurnEnd)));
        runtime.block_on(async {
            queue.push_wire(
                vec![
                    AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
                    AgentDomainEvent::UserMessageDelivered {
                        client_message_id: "s1".into(),
                        turn_id: "t2".into(),
                        text: String::new(),
                    },
                ],
                mark(6, "m1"),
            );
        });
        assert_eq!(queue.take().len(), 3);
        assert_eq!(queue.taken_through(), Some(6));
        assert_eq!(queue.freeze().unwrap().0, mark(6, "m1"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_send_in_flight_refuses_a_freeze_until_its_task_ends() {
        let queue = Arc::new(EventQueue::default());
        let steering = Steering::begin(&queue);
        assert!(matches!(queue.freeze(), Err(MoveRefusedWhy::SteerInFlight)));
        drop(steering);
        assert!(queue.freeze().is_ok());
    }

    /// A refused answer puts its question back in the window that sent it, so the chat does not move
    /// while an answer's reply is unread; the reply read (its handle dropped), it may.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_answer_whose_reply_is_unread_refuses_a_freeze() {
        let queue = Arc::new(EventQueue::default());
        let send = crate::InteractionSend::ready(Ok(None)).with_guard(Answering::begin(&queue));
        assert!(matches!(queue.freeze(), Err(MoveRefusedWhy::AnswerInFlight)));
        assert!(matches!(send.try_reply(), Some(Ok(None))));
        assert!(
            matches!(queue.freeze(), Err(MoveRefusedWhy::AnswerInFlight)),
            "still held until the handle is dropped"
        );
        drop(send);
        assert!(queue.freeze().is_ok());
    }

    #[cfg(target_os = "linux")]
    fn current_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn delivered(client_message_id: &str, turn_id: &str) -> AgentDomainEvent {
        AgentDomainEvent::UserMessageDelivered {
            client_message_id: client_message_id.into(),
            turn_id: turn_id.into(),
            text: String::new(),
        }
    }

    /// The taking side replays everything after the last take through a gate built as the gate stood
    /// at that take. One that had the message open there would hand over a gate with no record of it,
    /// and the replayed delivery would be dropped there, even though the live gate here is clean.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_message_open_at_the_last_take_refuses_a_freeze_after_its_delivery_was_read_unfolded() {
        let runtime = current_thread_runtime();
        let queue = Arc::new(EventQueue::default());
        queue
            .open_steer("s1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        queue.steer_accepted("s1");
        queue.push_wire(vec![text("one")], mark(3, "m1"));
        queue.take();
        runtime.block_on(async {
            queue.push_wire(vec![turn_completed("t1")], mark(4, "m1"));
            queue.push_wire(
                vec![
                    AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
                    delivered("s1", "t2"),
                ],
                mark(6, "m1"),
            );
        });
        assert!(
            matches!(queue.freeze(), Err(MoveRefusedWhy::SteerOpen)),
            "the delivery passed the gate here and is not folded"
        );
        assert_eq!(queue.take().len(), 3);
        assert_eq!(queue.freeze().unwrap(), (mark(6, "m1"), Vec::new()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_message_handed_in_after_the_last_take_refuses_a_freeze_until_a_take_follows() {
        let queue = Arc::new(EventQueue::default());
        queue.push_wire(vec![text("one")], mark(3, "m1"));
        queue.take();
        queue
            .open_steer("s1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        queue.steer_accepted("s1");
        queue.push_wire(vec![delivered("s1", "t1")], mark(4, "m1"));
        assert!(matches!(queue.freeze(), Err(MoveRefusedWhy::SteerOpen)));
        let taken = queue.take();
        assert!(
            matches!(&taken[..], [AgentDomainEvent::UserMessageDelivered { text, .. }] if text == "typed"),
            "{taken:?}"
        );
        assert_eq!(queue.freeze().unwrap(), (mark(4, "m1"), Vec::new()));
    }

    /// A refusal is made here and never on the wire: a taking side would not see it at all.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_refusal_after_the_last_take_refuses_a_freeze_until_it_is_taken() {
        let queue = Arc::new(EventQueue::default());
        queue.push_wire(vec![text("one")], mark(3, "m1"));
        queue.take();
        queue
            .open_steer("s1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        queue.steer_refused("s1", "no");
        assert!(matches!(queue.freeze(), Err(MoveRefusedWhy::SteerOpen)));
        let taken = queue.take();
        assert!(
            matches!(&taken[..], [AgentDomainEvent::UserMessageDropped { client_message_id, .. }] if client_message_id == "s1"),
            "{taken:?}"
        );
        assert_eq!(queue.freeze().unwrap(), (mark(3, "m1"), Vec::new()));
    }

    /// A delivery read after the last take for a message given up removes it from the live gate; the
    /// taking side replays that delivery and needs the message still on its list.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_freeze_hands_over_the_messages_given_up_as_of_the_last_take() {
        let runtime = current_thread_runtime();
        let queue = Arc::new(EventQueue::default());
        queue
            .open_steer("s1", "typed".into(), ProtoSteerTurnRequest::default())
            .unwrap();
        queue.steer_accepted("s1");
        queue.push_wire(vec![text("one")], mark(3, "m1"));
        runtime.block_on(async {
            queue.push_wire(vec![turn_completed("t1")], mark(4, "m1"));
        });
        // The first hold: its wait runs out without a follow-up.
        queue.expire_hold(1);
        assert_eq!(queue.take().len(), 3, "text, the drop, the released end");
        queue.push_wire(
            vec![
                AgentDomainEvent::TurnStarted { turn_id: "t2".into() },
                delivered("s1", "t2"),
            ],
            mark(6, "m1"),
        );
        let (at, given_up) = queue.freeze().unwrap();
        assert_eq!(at, mark(4, "m1"));
        assert_eq!(given_up, [("s1".to_string(), "typed".to_string())]);
    }

    /// The taking side replays from the fold, out of a ring that keeps only the newest events.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_freeze_is_refused_while_the_watch_has_read_far_past_the_fold() {
        let queue = Arc::new(EventQueue::default());
        queue.push_wire(vec![text("one")], mark(1, "m1"));
        queue.take();
        queue.push_wire(Vec::new(), mark(1 + MOVE_UNFOLDED_LIMIT, "m1"));
        assert!(queue.freeze().is_ok(), "within the limit");
        queue.thaw();
        queue.push_wire(Vec::new(), mark(2 + MOVE_UNFOLDED_LIMIT, "m1"));
        assert!(matches!(queue.freeze(), Err(MoveRefusedWhy::FoldBehind)));
        queue.take();
        assert_eq!(queue.freeze().unwrap().0, mark(2 + MOVE_UNFOLDED_LIMIT, "m1"));
    }

    /// A watch taken up elsewhere translates the next event as this one would have: the same split
    /// and the same note, from the sequence after the mark.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_watch_resumed_from_a_mark_goes_on_as_the_one_that_made_it() {
        let requested = crate::RequestedCliMode::Default;
        let mut state = WatchState::fresh(true, requested);
        assert_eq!(state.mark(), WatchMark::default());
        state.tracker.observe(1);
        state.tracker.observe(2);
        let delta = |id: &str| claude_runtime_protocol::v1::SessionEvent {
            sequence: 2,
            event: Some(claude_runtime_protocol::v1::session_event::Event::TextDelta(
                claude_runtime_protocol::v1::TextDelta {
                    turn_id: "t1".into(),
                    text: "x".into(),
                    message_id: Some(id.into()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        assert!(state.split.before(&delta("m1")).is_none());
        let at = state.mark();
        assert_eq!(at, mark(2, "m1"));

        let mut resumed = WatchState::resumed(&at, true, requested);
        assert_eq!(resumed.mark(), at);
        assert!(matches!(resumed.tracker.observe(3), watch::SequenceVerdict::Deliver));
        assert!(
            resumed.split.before(&delta("m2")).is_some(),
            "a different message after the mark opens a message of its own"
        );
    }
}

#[cfg(test)]
mod question_request_tests {
    use super::resolve_interaction_request_for;
    use crate::{InteractionResolution, QuestionAnswer, QuestionDeclineKind, ResolveInteractionRequest};
    use claude_runtime_protocol::v1 as wire;

    fn given() -> Vec<QuestionAnswer> {
        vec![
            QuestionAnswer {
                question_index: 0,
                option_indexes: vec![1],
                other_text: None,
                notes: Some("keep the sidebar".into()),
            },
            QuestionAnswer {
                question_index: 2,
                option_indexes: vec![0, 3],
                other_text: Some("typed".into()),
                notes: None,
            },
        ]
    }

    /// The request carries the person's choices as indexes and the text they typed, each question by
    /// its index, and nothing else: every field of it is named here, so a field that could carry the
    /// tool's input would have to be added on purpose.
    #[test]
    fn an_answer_goes_as_indexes_and_typed_text_only() {
        let request = resolve_interaction_request_for(
            ResolveInteractionRequest {
                session_id: "s1".into(),
                interaction_id: "i1".into(),
                resolution: InteractionResolution::Answers(given()),
            },
            "c1".into(),
        );
        let wire::ResolveInteractionRequest {
            session_id,
            command_id,
            interaction_id,
            result,
        } = request;
        assert_eq!(
            (session_id.as_str(), command_id.as_str(), interaction_id.as_str()),
            ("s1", "c1", "i1")
        );
        let Some(wire::resolve_interaction_request::Result::Answers(wire::QuestionAnswers { answers })) = result else {
            panic!("an answer goes as answers: {result:?}");
        };
        // Question index, option indexes, typed text, notes: the whole of one answer.
        type Sent = (u32, Vec<u32>, Option<String>, Option<String>);
        let sent: Vec<Sent> = answers
            .into_iter()
            .map(|answer| {
                let wire::QuestionAnswer {
                    question_index,
                    option_indexes,
                    other_text,
                    notes,
                } = answer;
                (question_index, option_indexes, other_text, notes)
            })
            .collect();
        assert_eq!(
            sent,
            vec![
                (0, vec![1], None, Some("keep the sidebar".to_string())),
                (2, vec![0, 3], Some("typed".to_string()), None),
            ]
        );
    }

    #[test]
    fn a_decline_goes_with_its_kind_and_only_a_chat_carries_what_was_answered() {
        let decline = |kind| {
            resolve_interaction_request_for(
                ResolveInteractionRequest {
                    session_id: "s1".into(),
                    interaction_id: "i1".into(),
                    resolution: InteractionResolution::Decline { kind, so_far: given() },
                },
                "c1".into(),
            )
            .result
        };
        let Some(wire::resolve_interaction_request::Result::Decline(cancel)) = decline(QuestionDeclineKind::Cancel)
        else {
            panic!("a cancel goes as a decline");
        };
        assert_eq!(cancel.kind, wire::QuestionDeclineKind::Cancel as i32);
        assert_eq!(cancel.so_far, None, "a cancel carries no answers");
        let Some(wire::resolve_interaction_request::Result::Decline(chat)) = decline(QuestionDeclineKind::Chat) else {
            panic!("a chat goes as a decline");
        };
        assert_eq!(chat.kind, wire::QuestionDeclineKind::Chat as i32);
        assert_eq!(chat.so_far.map(|so_far| so_far.answers.len()), Some(2));
    }
}
