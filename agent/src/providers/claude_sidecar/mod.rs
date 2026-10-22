// agent/src/providers/claude_sidecar/mod.rs
//! `ClaudeSidecarProvider` drives the sidecar's gRPC service. It spawns and owns one
//! sidecar process for its lifetime and connects over a UDS through `tonic` on a dedicated
//! `RuntimeThread`. Unary RPCs run synchronously here; the event pump handles streaming.
//! There is no resume RPC available here, so `resume_session` returns
//! `ProviderError::UnsupportedCapability`.

/// Handing a sidecar to another process (a chat moving to the other window): pidfds, Linux only.
#[cfg(target_os = "linux")]
mod moving;
mod spawn;
mod steer;
mod translate;
mod watch;

/// Real, billed A/B runs proving that protocol 3's two narrowing fields do what they say at
/// runtime. Read its header before running anything in it.
#[cfg(test)]
mod runtime_policy_verification;

#[cfg(target_os = "linux")]
pub use moving::{ClaudeProviderHandoff, MoveRefusedWhy};
#[cfg(target_os = "linux")]
pub use spawn::SidecarFds;
pub use spawn::{
    sidecar_availability, sidecar_missing_message, user_sidecar_path, SidecarAvailability, EXPECTED_VERDANDI_REVISION,
    NO_SIDECAR_HINT, SIDECAR_EXIT_GRACE,
};

use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, ProviderCapabilities,
    ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest, ResumeSessionRequest, SendTurnRequest,
    SteerTurnRequest, StreamingPreference,
};
use crate::runtime_thread::RuntimeThread;
use crate::setting_sources::{ProjectTrust, Source};
use crate::AgentDomainEvent;
use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
use claude_runtime_protocol::v1::{
    ClaudeHostPolicy, CliPermissionMode, CloseSessionRequest as ProtoCloseSessionRequest, ConfigurationProfile,
    CreateSessionRequest as ProtoCreateSessionRequest, ErrorCode as ProtoErrorCode, ErrorDetail, ExecutableSource,
    HandshakeRequest, HandshakeResponse, InterruptTurnRequest as ProtoInterruptTurnRequest,
    PermissionMode as ProtoPermissionMode, PersistenceMode, ReplayStart,
    ResolvePermissionRequest as ProtoResolvePermissionRequest, SendTurnRequest as ProtoSendTurnRequest, SettingSource,
    SettingSourceSelection, SteerTurnRequest as ProtoSteerTurnRequest, StreamingMode, ToolPolicy,
    WatchSessionEventsRequest,
};
use hyper_util::rt::TokioIo;
use spawn::SpawnedSidecar;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

pub const UNARY_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// The protocol major this client speaks. Connections are refused if the sidecar reports
/// a different major.
pub const CLIENT_PROTOCOL_MAJOR: u32 = 3;

/// Capability strings this client understands, as the sidecar advertises them in
/// `HandshakeResponse.capabilities`. Named constants rather than inline literals because a typo
/// here fails silently in the worst possible direction: the capability reads as absent, the feature
/// is hidden, and nothing anywhere reports a problem.
const CAP_INTERRUPT_TURN: &str = "interrupt_turn";
const CAP_RESUME_SESSION: &str = "resume_session";
const CAP_FORK_SESSION: &str = "fork_session";
/// Read by `connect()` to set `splits_messages`. `start_watching` uses it to split
/// assistant messages when `TextDelta.message_id` changes, through `translate::MessageSplit`.
const CAP_TEXT_DELTA_MESSAGE_ID: &str = "text_delta_message_id";
/// `ClaudeHostPolicy.provider_permission_prompts` routes the CLI's own permission prompts,
/// including sensitive-file checks under `.git/` or `.claude/`, as `PermissionRequested`
/// with origin PROVIDER_PROMPT. `connect()` stores the capability in `provider_prompts`,
/// which `build_create_request` sends.
const CAP_PROVIDER_PERMISSION_PROMPTS: &str = "provider_permission_prompts";
/// Capabilities for `ClaudeHostPolicy.cli_permission_mode` and `ResolvePermissionRequest.defer`.
/// Use the protocol crate's generated constants so a renamed capability fails to compile
/// instead of silently reading as absent.
const CAP_CLI_AUTO_MODE: &str = claude_runtime_protocol::CAP_CLI_AUTO_MODE;
const CAP_PERMISSION_DEFER: &str = claude_runtime_protocol::CAP_PERMISSION_DEFER;
/// The `SteerTurn` RPC and its `UserMessageDelivered` and `UserMessageDropped` events.
/// Advertised only when the CLI can report what became of a message;
/// exposed as `ProviderCapabilities::steer`.
const CAP_STEER_TURN: &str = claude_runtime_protocol::CAP_STEER_TURN;
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
/// True since O3 (2026-09-27): a provider prompt is translated with its origin
/// (`translate::provider_prompt_of`), answered by `eitri_core::agent_backend` on the rules O3 set
/// (bypass allows it unless the user's own ask rule forced it; Auto allows it only after the human
/// approved the same call), and drawn as a card with the CLI's own reason otherwise. Before that a
/// session that asked for these would have had its prompts judged as if the gate had asked.
const CLIENT_IMPLEMENTS_PROVIDER_PROMPTS: bool = true;
/// True since 2026-10-02: a session asks for the CLI's own auto mode, a gate request on it is
/// answered `PermissionDecision::Defer` by `eitri_core::agent_backend` when the tab is in Auto and no
/// saved rule allows it, the CLI's `SessionReady` report is judged against the request
/// (`translate::translate`), and its refusals are shown on their call (`PermissionDenied`).
const CLIENT_IMPLEMENTS_CLI_AUTO_MODE: bool = true;

/// Whether to request the CLI's own prompts: both the handshake advertisement and client
/// support are required. Without this capability, the headless CLI refuses its own prompts
/// without notifying the caller.
/// This stays internal because no caller outside the provider branches on it, just like
/// `splits_messages`. All sessions created here are gated and request prompts when supported.
fn provider_prompts_from_handshake(response: &HandshakeResponse) -> bool {
    CLIENT_IMPLEMENTS_PROVIDER_PROMPTS
        && response
            .capabilities
            .iter()
            .any(|c| c == CAP_PROVIDER_PERMISSION_PROMPTS)
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
    }
}

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
    /// The watch task of each session opened, newest last; a move cancels the source's. Only a move
    /// reads it, so a build without moves keeps none.
    #[cfg(target_os = "linux")]
    watches: Mutex<Vec<Watch>>,
    /// A sidecar taken over from another process and not committed yet: the session whose watch the
    /// commit opens, and where it goes on from. Nothing is watched, read or answered until then.
    parked: Mutex<Option<ParkedWatch>>,
    /// Set once this process handed the sidecar to another: it acts for the session no more.
    released: AtomicBool,
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
        }
    }

    #[cfg(target_os = "linux")]
    fn resumed(mark: &WatchMark, splits_messages: bool, requested: crate::RequestedCliMode) -> Self {
        WatchState {
            tracker: watch::SequenceTracker::after(mark.sequence),
            split: translate::MessageSplit::resuming(splits_messages, mark.last_text.clone()),
            cli_mode_note: translate::CliModeNote::resuming(requested, mark.cli_mode_noted),
            consecutive_failures: 0,
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
        } = self;
        WatchMark {
            sequence: tracker.after_sequence(),
            last_text: split.last_text().map(str::to_string),
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
}

/// Connects to the sidecar's socket and runs the real `Handshake`, refusing a socket served by another
/// user (and, when `expected_pid` is given, by another process than that one) and an incompatible
/// protocol major.
fn handshake_with(
    runtime: &RuntimeThread,
    sidecar: &SpawnedSidecar,
    expected_pid: Option<u32>,
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
        .map_err(|e| std::io::Error::other(e.to_string()))?
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
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .map_err(|status| std::io::Error::other(format!("handshake failed: {status}")))?
        .into_inner();

    // Reject an incompatible protocol major; tolerate minor differences through protobuf's
    // unknown-field handling. A conforming peer rejects a mismatched client major during the
    // handshake RPC, so this branch is a backstop for a peer that returns an incompatible major.
    // That includes a response omitting the scalar field, which proto3 decodes as zero.
    // Do not issue commands when the returned wire version is incompatible.
    if handshake.protocol_major != CLIENT_PROTOCOL_MAJOR {
        return Err(std::io::Error::other(format!(
            "claude-sidecar speaks protocol major {} but this client speaks {CLIENT_PROTOCOL_MAJOR}; \
             refusing to continue rather than issuing commands it may silently misread",
            handshake.protocol_major
        )));
    }

    let capabilities = capabilities_from_handshake(&handshake);
    let splits_messages = handshake.capabilities.iter().any(|c| c == CAP_TEXT_DELTA_MESSAGE_ID);
    let provider_prompts = provider_prompts_from_handshake(&handshake);
    if !provider_prompts {
        // Said once, on stderr only: an older sidecar is not a fault, but a bypass on it is
        // narrower than the CLI's own bypassPermissions (O3), and this is the line that says why.
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
    diagnostics.extend(startup_diagnostics_from_stderr(&sidecar.stderr_tail()));
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
    })
}

impl ClaudeSidecarProvider {
    /// Spawns a fresh sidecar and connects to it, performing the real `Handshake` before returning
    /// -- a provider that constructs successfully has already proven it can talk to a real,
    /// compatible sidecar, not just that a process happened to start.
    pub fn connect(instance_id: &str) -> std::io::Result<Self> {
        Self::around(spawn::spawn(instance_id)?)
    }

    /// [`Self::connect`] with `program` started as the sidecar, found by no search and built by
    /// nothing: the crate's tests start a stand-in this way.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn connect_to_program(instance_id: &str, program: &std::path::Path) -> std::io::Result<Self> {
        Self::around(spawn::spawn_program(instance_id, program)?)
    }

    /// A provider driving `sidecar`, which this process started and holds.
    fn around(sidecar: SpawnedSidecar) -> std::io::Result<Self> {
        let runtime = RuntimeThread::spawn();
        let Handshaken {
            client,
            capabilities,
            info,
            splits_messages,
            provider_prompts,
        } = handshake_with(&runtime, &sidecar, None)?;
        Ok(Self {
            sidecar: Mutex::new(sidecar),
            runtime,
            client,
            events: Arc::new(EventQueue::default()),
            backpressure: Arc::new(BackpressureCounters::default()),
            capabilities,
            info,
            splits_messages,
            provider_prompts,
            #[cfg(target_os = "linux")]
            watches: Mutex::new(Vec::new()),
            parked: Mutex::new(None),
            released: AtomicBool::new(false),
        })
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
    fn open_session(&self, request: ProtoCreateSessionRequest) -> Result<String, ProviderError> {
        self.require_driving()?;
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
        #[cfg(target_os = "linux")]
        let watched = session_id.clone();
        let task = self.runtime.handle().spawn(async move {
            // The single exit for every abnormal path. Nothing here may return quietly except the
            // one genuinely normal ending: a session that closed on its own terms.
            let report = |reason: String| {
                eprintln!("agent: ClaudeSidecarProvider: event stream lost: {reason}");
                events.push(AgentDomainEvent::SessionUnavailable { reason });
            };

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
                        // Whatever a held turn end waits for comes in the replay that follows, so its
                        // wait starts over from here.
                        events.stream_back();
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
                                        translated.push(domain_event);
                                    }
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
    // Business errors arrive as protobuf-encoded `ErrorDetail` in `grpc-status-details-bin`.
    // Tonic extracts this trailer into `status.details()`; it is absent from `status.metadata()`.
    // Decode the details directly so typed provider errors are not lost as transport errors.
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

/// Refuses session creation unless the sidecar advertises the gated `interactive` policy,
/// which every session requires regardless of the tab's mode. Falling back to BYPASS would run
/// the agent under a policy the user did not choose. Kept as a free function so the check can be
/// tested without a sidecar.
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

/// Builds the `CreateSessionRequest` used by both fresh and resumed sessions.
/// Every session uses `INTERACTIVE` with switching disabled, regardless of the tab's mode.
/// Only values with distinct runtime behavior are sent. Equivalent permission and persistence
/// values add no choice, and `executable` is not read by the sidecar. PARTIAL streaming has a
/// measured effect, so the request sends it.
/// `provider_prompts_from_handshake` selects `provider_prompts` from the peer's capabilities.
/// `requested_cli_mode` selects `Auto` only when both `cli_auto_mode` and `permission_defer` are
/// advertised. `Default` leaves the field unset, preserving the request sent to older sidecars.
fn build_create_request(
    cwd: String,
    streaming: StreamingPreference,
    resume_provider_session_id: Option<String>,
    fork: bool,
    provider_prompts: bool,
    cli_mode: crate::RequestedCliMode,
    project: ProjectTrust,
) -> ProtoCreateSessionRequest {
    build_create_request_loading(
        cwd,
        streaming,
        resume_provider_session_id,
        fork,
        provider_prompts,
        crate::setting_sources::loads_user_settings(),
        project,
        cli_mode,
    )
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
    user_settings: bool,
    project: ProjectTrust,
    cli_mode: crate::RequestedCliMode,
) -> ProtoCreateSessionRequest {
    let denied = crate::process::disallowed_tools();
    ProtoCreateSessionRequest {
        cwd,
        policy: Some(ClaudeHostPolicy {
            configuration: ConfigurationProfile::Native as i32,
            // The sidecar's `PreToolUse` gate asks the host about every call; bypass is the host answering
            // `allow` through that gate. The CLI runs `default` or its own `auto` mode, selected by
            // `cli_permission_mode` below.
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
            // Permission switching stays disabled: `true` adds `--allow-dangerously-skip-permissions` to the CLI.
            permission_mode_switchable: false,
            // O3: the CLI's own permission prompts (its sensitive-file check, which neither the
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
        }),
        resume_provider_session_id,
        fork,
        // Each `None` below leaves the CLI's own default in force -- the proto's own documented
        // meaning for an absent `optional` field -- so a caller that does not choose gets exactly
        // what every session got before these fields existed.
        model: None,         // absent = the CLI's default model
        effort: None,        // absent = the CLI's default reasoning effort
        system_prompt: None, // absent = Claude Code's own system prompt, unreplaced
        output_format: None, // absent = plain text, not schema-validated structured output
        // false = `CreateSession` returns as soon as the session exists, as it always has.
        await_account_identity: false,
    }
}

/// The wire request for a fresh session. Destructured whole, so a field added to the request has to
/// be placed here rather than dropped on the way to the wire.
fn fresh_request(
    request: CreateSessionRequest,
    provider_prompts: bool,
    cli_mode: crate::RequestedCliMode,
) -> ProtoCreateSessionRequest {
    let CreateSessionRequest {
        cwd,
        streaming,
        project,
    } = request;
    build_create_request(cwd, streaming, None, false, provider_prompts, cli_mode, project)
}

/// The wire request continuing a Claude session, under the trust the caller states now. Destructured
/// whole, as [`fresh_request`] is.
fn resume_request(
    request: ResumeSessionRequest,
    provider_prompts: bool,
    cli_mode: crate::RequestedCliMode,
) -> ProtoCreateSessionRequest {
    let ResumeSessionRequest {
        provider_session_id,
        cwd,
        streaming,
        project,
    } = request;
    build_create_request(
        cwd,
        streaming,
        Some(provider_session_id),
        false,
        provider_prompts,
        cli_mode,
        project,
    )
}

impl AgentProvider for ClaudeSidecarProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    /// Creates only gated sessions. Refuses sidecars without the interactive policy
    /// rather than starting under another policy.
    fn create_session(&self, request: CreateSessionRequest) -> Result<String, ProviderError> {
        self.require_interactive()?;
        self.open_session(fresh_request(
            request,
            self.provider_prompts,
            requested_cli_mode(&self.capabilities),
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
            // The provider does not advertise resume, so do not send the command.
            // Return an error rather than assuming the server will ignore an unsupported command.
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
            requested_cli_mode(&self.capabilities),
        ))
    }

    fn send_turn(&self, request: SendTurnRequest) -> Result<String, ProviderError> {
        self.require_driving()?;
        let mut client = self.client.clone();
        let proto_request = ProtoSendTurnRequest {
            session_id: request.session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
            text: request.text,
        };
        let response = self.run_unary(async move { client.send_turn(proto_request).await })?;
        Ok(response.turn_id)
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
            text,
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
}

/// The events received off the wire and not yet taken by `pump`, with the signal that says so.
/// Whatever becomes ready raises it, so nothing can be queued without whoever drains `pump` being
/// woken.
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
    /// `send_steers` tasks running.
    steering: usize,
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
            state.settle();
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
            state.settle();
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
        state.settle();
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

    /// Records an accepted message; the next one to send, if one waits.
    fn steer_accepted(&self, client_id: &str) -> Option<(String, ProtoSteerTurnRequest)> {
        self.lock().gate.accepted(client_id)
    }

    fn steer_refused(&self, client_id: &str, reason: &str) {
        let mut state = self.lock();
        let out = state.gate.refused(client_id, reason);
        self.deliver(&mut state, out);
        state.settle();
    }

    fn steer_unknown(&self, client_id: &str) {
        let mut state = self.lock();
        let out = state.gate.unknown(client_id);
        self.deliver(&mut state, out);
        state.settle();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fresh, resumed and forked request uses `INTERACTIVE`, disables permission switching,
    /// and sends an empty deny list as `unrestricted`, regardless of the tab's mode.
    /// `INTERACTIVE` installs the sidecar's `PreToolUse` gate. `unrestricted` prevents its conservative
    /// default deny list from being added, so every tool can reach the gate.
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
                        cli_mode,
                        ProjectTrust::Untrusted,
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
                        cli_mode,
                        ProjectTrust::Untrusted,
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
                        cli_mode,
                        ProjectTrust::Untrusted,
                    ),
                ),
            ]
        });
        for (shape, request) in shapes.into_iter().flatten() {
            let policy = request.policy.expect("a policy is always sent");
            assert_eq!(
                policy.permissions,
                ProtoPermissionMode::Interactive as i32,
                "{shape}: every session is gated (R07)"
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

    /// Every session created here is gated and requests the CLI's own prompts exactly when
    /// the handshake advertises support. Only the prompt flag changes; the remaining policy
    /// stays the same, including the absent `allow` list. The sidecar rejects an allow list
    /// naming the three prompt tools enabled by this flag.
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
                    crate::RequestedCliMode::Default,
                    ProjectTrust::Untrusted,
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
            user_settings,
            project,
            crate::RequestedCliMode::Default,
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
                    true,
                    ProjectTrust::Untrusted,
                    crate::RequestedCliMode::Default,
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
                ProjectTrust::Untrusted,
                crate::RequestedCliMode::Default,
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
                },
                false,
                crate::RequestedCliMode::Default,
            )
        };
        let fresh = |project| {
            fresh_request(
                CreateSessionRequest {
                    cwd: "/p".into(),
                    streaming: StreamingPreference::Partial,
                    project,
                },
                false,
                crate::RequestedCliMode::Default,
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
                crate::RequestedCliMode::Default,
                project,
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

    /// A manually maintained handshake fixture for testing capability and information mapping.
    /// It does not observe a running sidecar, so protocol or capability changes can leave it
    /// stale while unit tests still pass. Update it when the real handshake changes.
    ///
    /// These tests check `capabilities_from_handshake`'s intersection logic, capability names,
    /// and preservation of `ProviderInfo` fields. The ignored real-process test
    /// `the_live_handshake_still_matches_the_fixture_in_mod_rs` checks the live handshake
    /// without spending model tokens.
    ///
    /// `claude_agent_sdk_version` and the two `*claude_code_version` fields are sample installed
    /// versions. The live test does not pin them; unit tests only check that
    /// `info_from_handshake` carries them through unchanged.
    fn real_handshake_today() -> HandshakeResponse {
        HandshakeResponse {
            protocol_major: 3,
            // The handshake reports the protocol minor from `capabilities.json`.
            protocol_minor: 20,
            sidecar_version: "0.2.0".into(),
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
                // Resume and fork are parameters of `CreateSession`, rather than separate RPCs.
                // They need explicit capability advertisements because the service definition cannot
                // show whether they are supported.
                "resume_session",
                "fork_session",
                "setting_sources",
                "tool_policy",
                // These capabilities accompany `CreateSessionRequest`'s `model`, `effort`,
                // `system_prompt` and `output_format` fields. `build_create_request` sends none of them,
                // and this crate does not yet read these capability entries.
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
                // The pinned sidecar still advertises this command, but this client never calls it.
                // No client capability is derived from it.
                "set_permission_mode",
                // Capability 'text_delta_message_id' -- see CAP_TEXT_DELTA_MESSAGE_ID.
                "text_delta_message_id",
                // `ClaudeHostPolicy.provider_permission_prompts` routes the CLI's own permission prompts
                // as `PermissionRequested` with origin PROVIDER_PROMPT. The handshake fixture places
                // this capability before the executable entries.
                "provider_permission_prompts",
                // Protocol 3.13, all unread by this client (`build_create_request` sends neither
                // `init_check` nor `await_account_identity`).
                "init_check",
                "await_account_identity",
                "egress_probe",
                "structured_output_tools",
                "effective_tool_report",
                // The CLI's own auto mode under the gate, deferred answers, and refusal events.
                // The first two together provide `ProviderCapabilities::cli_auto_mode`.
                // The entries follow `capabilities.json` order.
                "cli_auto_mode",
                "permission_defer",
                "permission_denied_events",
                // These capabilities are listed in `capabilities.json`; this client reads none of them.
                "cli_compatibility_report",
                "gated_cli_mode_default",
                "effective_claude_config_dir",
                "startup_report_file",
                "effective_session_report",
                "extra_mcp_servers",
                "extra_mcp_servers_http",
                // `steer_turn` provides `ProviderCapabilities::steer` and is advertised only when the
                // sidecar's CLI is at least 2.1.252. `list_commands` provides the `CommandsUpdated` event,
                // which is translated whenever it arrives.
                "steer_turn",
                "list_commands",
                // Executable sources are advertised as capabilities rather than a separate wire field.
                // `executable_host_cli` is always present; checkout builds also offer `executable_sdk_bundled`.
                // Packaged builds cannot offer it: the SDK uses `createRequire` to find its CLI, and the
                // single-file artifact has no node_modules. This fixture represents a packaged build.
                // A packaged process rejected SDK_BUNDLED with INVALID_CONFIGURATION naming host_cli.
                // The additional capabilities from `session_model` onward were copied from the protocol
                // and have not been checked against a live process.
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
            // Leave `account_binding`, `account_name` and `account_config_dir` at their defaults;
            // this fixture has no measured account identity. `AccountBinding::Unspecified` is zero,
            // as it would be for a peer that omits these fields.
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
            "bypass is advertised -- a reported fact only, never requested since R07"
        );
        assert!(
            capabilities.interactive_permission_mode,
            "interactive is advertised, and is the only policy any session is created under (R07)"
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

    /// Refuses a sidecar without the gated policy and names the missing and advertised policies.
    /// Never starts the session under BYPASS instead.
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
        // A capability needs both provider advertisement and client implementation.
        // Resume has both. Fork is advertised, but remains unavailable because
        // `CLIENT_IMPLEMENTS_FORK` is false, avoiding a UI control that cannot work.
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
        assert_eq!(info.sidecar_version, "0.2.0");
        // Raw, not filtered down to the ones this client recognizes -- an unrecognized future
        // capability must stay visible in diagnostics rather than vanish.
        assert!(info.advertised_permission_modes.contains(&"verdandi_rules".to_string()));
        // Forty-one capabilities in the packaged fixture returned by `real_handshake_today()`.
        // Checkout builds also advertise `executable_sdk_bundled`; a sidecar with egress
        // restrictions enabled additionally advertises `egress_restricted`.
        assert_eq!(info.advertised_capabilities.len(), 41);
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
                    true,
                    ProjectTrust::Untrusted,
                    cli_mode,
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
