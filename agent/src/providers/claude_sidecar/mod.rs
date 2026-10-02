// agent/src/providers/claude_sidecar/mod.rs
//! `ClaudeSidecarProvider`: drives Verdandi's `apps/claude-sidecar` gRPC service (design doc §10.1,
//! Phase 3). Spawns and owns exactly one sidecar process for its own lifetime (Task 6), connects to
//! it over a UDS via `tonic` on a dedicated `RuntimeThread` (Task 3), and issues the 6 in-scope
//! unary RPCs synchronously (this file) plus the streaming event pump (Task 8). `ResumeSession` has
//! no RPC to call yet (design doc §19) -- `resume_session` always returns
//! `ProviderError::UnsupportedCapability`.

mod spawn;
mod translate;
mod watch;

/// Real, billed A/B runs proving that protocol 3's two narrowing fields do what they say at
/// runtime. Read its header before running anything in it.
#[cfg(test)]
mod runtime_policy_verification;

pub use spawn::{
    sidecar_availability, sidecar_missing_message, user_sidecar_path, SidecarAvailability, EXPECTED_VERDANDI_REVISION,
    NO_SIDECAR_HINT, SIDECAR_EXIT_GRACE,
};

use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, ProviderCapabilities,
    ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest, ResumeSessionRequest, SendTurnRequest,
    StreamingPreference,
};
use crate::runtime_thread::RuntimeThread;
use crate::AgentDomainEvent;
use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
use claude_runtime_protocol::v1::{
    ClaudeHostPolicy, CloseSessionRequest as ProtoCloseSessionRequest, ConfigurationProfile,
    CreateSessionRequest as ProtoCreateSessionRequest, ErrorCode as ProtoErrorCode, ErrorDetail, ExecutableSource,
    HandshakeRequest, HandshakeResponse, InterruptTurnRequest as ProtoInterruptTurnRequest,
    PermissionMode as ProtoPermissionMode, PersistenceMode, ReplayStart,
    ResolvePermissionRequest as ProtoResolvePermissionRequest, SendTurnRequest as ProtoSendTurnRequest, SettingSource,
    SettingSourceSelection, StreamingMode, ToolPolicy, WatchSessionEventsRequest,
};
use hyper_util::rt::TokioIo;
use spawn::SpawnedSidecar;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

pub const UNARY_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// The protocol major this client speaks. A sidecar reporting anything else is refused outright --
/// design doc §9.3: "major 不兼容：拒绝连接".
pub const CLIENT_PROTOCOL_MAJOR: u32 = 3;

/// Capability strings this client understands, as the sidecar advertises them in
/// `HandshakeResponse.capabilities`. Named constants rather than inline literals because a typo
/// here fails silently in the worst possible direction: the capability reads as absent, the feature
/// is hidden, and nothing anywhere reports a problem.
const CAP_INTERRUPT_TURN: &str = "interrupt_turn";
const CAP_RESUME_SESSION: &str = "resume_session";
const CAP_FORK_SESSION: &str = "fork_session";
/// Verdandi 133dc03. Read by `connect()` to decide `splits_messages`, which `start_watching` then
/// uses to split two sidecar assistant messages at a `TextDelta.message_id` change -- see
/// `translate::MessageSplit`.
const CAP_TEXT_DELTA_MESSAGE_ID: &str = "text_delta_message_id";
/// Verdandi b3aa188: `ClaudeHostPolicy.provider_permission_prompts` -- the CLI's OWN permission
/// prompts (its sensitive-file safety check on `.git/`/`.claude/` is the measured one) routed to this
/// host as `PermissionRequested` with origin PROVIDER_PROMPT. Read by `connect()` into
/// `provider_prompts`, which `build_create_request` sends.
const CAP_PROVIDER_PERMISSION_PROMPTS: &str = "provider_permission_prompts";
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

/// Whether sessions on this sidecar ask for the CLI's own prompts (O3 ruling 2): the intersection of
/// what the handshake advertised and what this client implements, like every capability here. A
/// sidecar without it is sent nothing new and behaves exactly as before b3aa188 -- the CLI's own
/// asks are refused headlessly, with nobody told.
///
/// Not a `ProviderCapabilities` field, for the reason `splits_messages` is not: no caller outside
/// this provider branches on it. Every session this client creates is gated (R07), so it is asked
/// for on every one of them.
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
    sidecar: SpawnedSidecar,
    runtime: RuntimeThread,
    client: RuntimeServiceClient<Channel>,
    events: Arc<Mutex<Vec<AgentDomainEvent>>>,
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
    /// `resume_session` ask this sidecar for the CLI's own permission prompts (O3 ruling 2).
    provider_prompts: bool,
}

impl ClaudeSidecarProvider {
    /// Spawns a fresh sidecar and connects to it, performing the real `Handshake` before returning
    /// -- a provider that constructs successfully has already proven it can talk to a real,
    /// compatible sidecar, not just that a process happened to start.
    pub fn connect(instance_id: &str) -> std::io::Result<Self> {
        let sidecar = spawn::spawn(instance_id)?;
        let runtime = RuntimeThread::spawn();
        let socket_path = sidecar.socket_path.clone();

        let client = runtime
            .block_on(
                async move {
                    let channel = Endpoint::try_from("http://[::]:50051")?
                        .connect_with_connector(service_fn(move |_: Uri| {
                            let socket_path = socket_path.clone();
                            async move {
                                let stream = UnixStream::connect(socket_path).await?;
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

        // Design doc §9.3: an incompatible major is refused, minor differences are tolerated
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

        Ok(Self {
            sidecar,
            runtime,
            client,
            events: Arc::new(Mutex::new(Vec::new())),
            backpressure: Arc::new(BackpressureCounters::default()),
            capabilities,
            info,
            splits_messages,
            provider_prompts,
        })
    }

    /// What the event stream is costing right now: queue depth, and how far behind production
    /// delivery has fallen. See `BackpressureStats` for why losslessness alone is not the question.
    pub fn backpressure_stats(&self) -> BackpressureStats {
        BackpressureStats {
            pending_events: self.events.lock().unwrap().len(),
            events_received: self.backpressure.events_received.load(Ordering::Relaxed),
            max_delivery_lag_ms: self.backpressure.max_delivery_lag_ms.load(Ordering::Relaxed),
            last_delivery_lag_ms: self.backpressure.last_delivery_lag_ms.load(Ordering::Relaxed),
            watch_reconnects: self.backpressure.watch_reconnects.load(Ordering::Relaxed),
        }
    }

    /// The sidecar process's recent stderr, for diagnosing a provider that connected but is
    /// misbehaving. `info().startup_diagnostics` is the curated subset worth showing a user.
    pub fn sidecar_stderr_tail(&self) -> Vec<String> {
        self.sidecar.stderr_tail()
    }

    /// The OS pid of the sidecar process this provider spawned and owns.
    ///
    /// Exists so an orphan check can assert on the ONE process this provider is responsible for.
    /// Never match sidecar or `claude` processes by name for that purpose: Claude Code sessions on a
    /// developer machine are themselves processes named `claude`, so a name-matched search sweeps up
    /// the session running the test. Capture this pid, check this pid.
    pub fn sidecar_pid(&self) -> u32 {
        self.sidecar.pid()
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
        let mut client = self.client.clone();
        let response = self.run_unary(async move { client.create_session(request).await })?;
        self.start_watching(response.session_id.clone());
        Ok(response.session_id)
    }

    fn require_interactive(&self) -> Result<(), ProviderError> {
        require_interactive(&self.capabilities, &self.info)
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
        let client = self.client.clone();
        let events = Arc::clone(&self.events);
        let backpressure = Arc::clone(&self.backpressure);
        let splits_messages = self.splits_messages;
        self.runtime.handle().spawn(async move {
            let mut tracker = watch::SequenceTracker::new();
            // One per session, outside the reconnect loop below: a reconnect must not forget which
            // message id it last saw, and `tracker` (just above) already drops `Duplicate` events
            // before they reach here, so a replay after a reconnect cannot split a message twice.
            let mut split = translate::MessageSplit::new(splits_messages);
            // Per session, like `split`: a stricter or unreported CLI permission mode is noted once,
            // not on every turn's `SessionReady` (spec §2.3).
            let mut cli_mode_note = translate::CliModeNote::default();
            // Counts only CONSECUTIVE failed attempts: a reconnect that actually delivered a new
            // event resets it, so the budget bounds "getting nowhere", not "reconnecting at all".
            let mut consecutive_failures = 0usize;

            // The single exit for every abnormal path. Nothing here may return quietly except the
            // one genuinely normal ending: a session that closed on its own terms.
            let report = |reason: String| {
                eprintln!("agent: ClaudeSidecarProvider: event stream lost: {reason}");
                events
                    .lock()
                    .unwrap()
                    .push(AgentDomainEvent::SessionUnavailable { reason });
            };

            loop {
                let request = WatchSessionEventsRequest {
                    session_id: session_id.clone(),
                    // Always AFTER_SEQUENCE, on the first open as well as every reconnect. See
                    // `SequenceTracker::after_sequence` for why neither other mode is usable here:
                    // both are defined as unable to report a gap, which is the one thing this client
                    // must never accept silently.
                    start: ReplayStart::AfterSequence as i32,
                    after_sequence: Some(tracker.after_sequence()),
                };
                let opened = {
                    let mut client = client.clone();
                    client.watch_session_events(request).await
                };
                let mut stream = match opened {
                    Ok(response) => response.into_inner(),
                    Err(status) => {
                        let detail = match watch::classify_open_failure(&map_status(status)) {
                            watch::OpenFailure::Fatal(detail) => {
                                report(watch::stream_ended_early_reason(&detail));
                                return;
                            }
                            watch::OpenFailure::Retry(detail) => detail,
                        };
                        match watch::WATCH_RECONNECT_BACKOFF.get(consecutive_failures) {
                            Some(delay) => {
                                consecutive_failures += 1;
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
                            match tracker.observe(sequence) {
                                watch::SequenceVerdict::Deliver => {
                                    delivered_since_open = true;
                                    if let Some(boundary) = split.before(&event) {
                                        events.lock().unwrap().push(boundary);
                                    }
                                    if let Some(line) = cli_mode_note.observe(&event) {
                                        eprintln!("{line}");
                                    }
                                    for domain_event in translate::translate(event) {
                                        // A session that closes on its own terms is the one ending
                                        // that is not a loss -- recorded from the provider's own
                                        // terminal event, never inferred from the stream stopping.
                                        closed_by_session |=
                                            matches!(domain_event, AgentDomainEvent::SessionClosed { .. });
                                        events.lock().unwrap().push(domain_event);
                                    }
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
                if delivered_since_open {
                    consecutive_failures = 0;
                }
                match watch::WATCH_RECONNECT_BACKOFF.get(consecutive_failures) {
                    Some(delay) => {
                        consecutive_failures += 1;
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
    }
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
    // Design doc §9.7 / this plan's "Verified facts" point 9: business errors travel as
    // protobuf-encoded `ErrorDetail` bytes in the standard `grpc-status-details-bin` trailing
    // metadata key -- but tonic's `Status` already parses that specific well-known trailer out
    // for you: `status.details()` returns the raw bytes directly, `status.metadata()` does NOT
    // contain it (confirmed against a real sidecar-returned `SESSION_NOT_FOUND` error during
    // this plan's own preparation -- `status.metadata().get_bin("grpc-status-details-bin")`
    // silently returned `None` every time; `status.details()` decoded correctly on the first
    // try). Do not "fix" this back to a `.metadata()` lookup -- that was the actual bug.
    let bytes = status.details();
    if !bytes.is_empty() {
        if let Ok(detail) = <ErrorDetail as prost::Message>::decode(bytes) {
            return ProviderError::Provider {
                code: provider_error_code(detail.code()),
                message: detail.message,
            };
        }
    }
    ProviderError::Transport(status.message().to_string())
}

/// Refuses to start a session on a sidecar that does not advertise the gated (`interactive`)
/// policy -- since R07 the only one any session is created under, whatever the tab's mode.
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
/// The policy is fixed rather than parameterised on purpose, and since R07 that includes the
/// permission policy: every session is `INTERACTIVE` and never switchable, whatever the tab's
/// mode. This client only ever sends values whose runtime behavior has been confirmed distinct.
/// `verdandi_rules` is identical to `interactive` in the current sidecar, `external_store` is
/// identical to `ephemeral`, and
/// `executable` is not read at all -- sending any of them would be choosing a value that means
/// nothing. `streaming` is the exception that proves the rule: PARTIAL has a measured, distinct
/// effect, so it is sent. `provider_prompts` is the other: `provider_prompts_from_handshake`, the one
/// policy field that depends on the peer (O3 ruling 2).
fn build_create_request(
    cwd: String,
    streaming: StreamingPreference,
    resume_provider_session_id: Option<String>,
    fork: bool,
    provider_prompts: bool,
) -> ProtoCreateSessionRequest {
    build_create_request_loading(
        cwd,
        streaming,
        resume_provider_session_id,
        fork,
        provider_prompts,
        crate::setting_sources::loads_user_settings(),
    )
}

/// The settings tiers a session asks for, in the proto enum's order. The user tier is the user's
/// own `~/.claude` (hooks, plugins, skills, `CLAUDE.md`, permission rules); it is left out only
/// when `agent.user_settings` is false.
fn setting_sources_for(user_settings: bool) -> Vec<i32> {
    let mut sources = Vec::with_capacity(3);
    if user_settings {
        sources.push(SettingSource::User as i32);
    }
    sources.push(SettingSource::Project as i32);
    sources.push(SettingSource::Local as i32);
    sources
}

/// [`build_create_request`] with the user tier named instead of read from the process-wide choice,
/// so both selections can be built and compared in one process.
fn build_create_request_loading(
    cwd: String,
    streaming: StreamingPreference,
    resume_provider_session_id: Option<String>,
    fork: bool,
    provider_prompts: bool,
    user_settings: bool,
) -> ProtoCreateSessionRequest {
    let denied = crate::process::disallowed_tools();
    ProtoCreateSessionRequest {
        cwd,
        policy: Some(ClaudeHostPolicy {
            configuration: ConfigurationProfile::Native as i32,
            // Always gated (R07): the CLI runs `default` under Verdandi's `PreToolUse` broker, and
            // bypass is Eitri answering `allow`, never a policy sent here.
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
            // Parity with the legacy backend, which passes `--setting-sources` and
            // `--disallowedTools` on every spawn. Until protocol 3 neither had a field here, so the
            // sidecar path ran with whatever the CLI loaded by default and with no tool denials at
            // all; stating the tiers keeps the two backends on one selection.
            //
            // `Native` stays: it is the tier set below that decides what loads, and this field is
            // the axis that distinguishes a real project from an isolated one. Eitri has no
            // product answer for `Isolated` yet (no UI, config or Lua seam picks it), and there is a
            // trap waiting there -- see `agent/MANUAL_VERIFICATION.md`.
            setting_sources: Some(SettingSourceSelection {
                // The user tier by default, so a session behaves as `claude` does in a terminal:
                // the user's own hooks, plugins, skills, `CLAUDE.md` and permission rules apply.
                // `agent.user_settings = false` leaves it out. The permission gate does not depend
                // on which is chosen: it comes from this request's `INTERACTIVE` policy.
                sources: setting_sources_for(user_settings),
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
            // Never switchable (R07): `true` sets --allow-dangerously-skip-permissions on the CLI.
            permission_mode_switchable: false,
            // O3: the CLI's own permission prompts (its sensitive-file check, which neither the
            // gate's `allow` nor a rule silences) come to this host instead of being refused with
            // nobody asked. Meaningful because the session is INTERACTIVE; an older sidecar is never
            // sent it (`provider_prompts_from_handshake`). Adds no tool: the sidecar disallows the
            // three the SDK's prompt channel brings with it, and `allow` above is absent.
            provider_permission_prompts: provider_prompts,
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
    }
}

impl AgentProvider for ClaudeSidecarProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    /// Always gated (R07): refused on a sidecar without the interactive policy, never started under
    /// another one.
    fn create_session(&self, request: CreateSessionRequest) -> Result<String, ProviderError> {
        self.require_interactive()?;
        self.open_session(build_create_request(
            request.cwd,
            request.streaming,
            None,
            false,
            self.provider_prompts,
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
            // The provider does not advertise it, so this client must not send it -- design doc
            // §9.3: never send a command on the theory the server will ignore it.
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
        self.open_session(build_create_request(
            request.cwd,
            request.streaming,
            Some(request.provider_session_id),
            false,
            self.provider_prompts,
        ))
    }

    fn send_turn(&self, request: SendTurnRequest) -> Result<String, ProviderError> {
        let mut client = self.client.clone();
        let proto_request = ProtoSendTurnRequest {
            session_id: request.session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
            text: request.text,
        };
        let response = self.run_unary(async move { client.send_turn(proto_request).await })?;
        Ok(response.turn_id)
    }

    fn interrupt_turn(&self, request: InterruptTurnRequest) -> Result<(), ProviderError> {
        let mut client = self.client.clone();
        let proto_request = ProtoInterruptTurnRequest {
            session_id: request.session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
        };
        self.run_unary(async move { client.interrupt_turn(proto_request).await })?;
        Ok(())
    }

    fn resolve_permission(&self, request: ResolvePermissionRequest) -> Result<(), ProviderError> {
        let mut client = self.client.clone();
        // `PermissionDecision` is destructured here, at the wire, and nowhere else: the wire's
        // `allow`/`reason` pair is the reason the enum has exactly these two variants. An approval
        // sends no reason because there is no field on the far side that would ever show it.
        let proto_request = ProtoResolvePermissionRequest {
            session_id: request.session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
            permission_id: request.permission_id,
            allow: request.decision.allows(),
            reason: request.decision.reason().unwrap_or_default().to_string(),
        };
        self.run_unary(async move { client.resolve_permission(proto_request).await })?;
        Ok(())
    }

    fn close_session(&self, request: CloseSessionRequest) -> Result<(), ProviderError> {
        let mut client = self.client.clone();
        let proto_request = ProtoCloseSessionRequest {
            session_id: request.session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
        };
        self.run_unary(async move { client.close_session(proto_request).await })?;
        Ok(())
    }

    fn pump(&self) -> Vec<AgentDomainEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// R07 on the sidecar wire: every request -- fresh, resume and fork alike -- is `INTERACTIVE`,
    /// never switchable, and states an empty deny list as `unrestricted`, whatever mode the tab is
    /// in (the request has no mode to carry any more). `INTERACTIVE` is what Verdandi installs its
    /// `PreToolUse` broker for (`permissions !== 'bypass'`, `packages/claude-runtime/src/session.ts`),
    /// and `unrestricted` is what keeps its `CONSERVATIVE_BYPASS_DENY` floor off (checked in
    /// Verdandi's source, read-only, 2026-09-25) -- so the gate is there and no tool is denied.
    #[test]
    fn every_request_is_gated_never_switchable_and_restricts_no_tool() {
        let shapes = [
            (
                "fresh",
                build_create_request("/tmp/p".into(), StreamingPreference::Partial, None, false, true),
            ),
            (
                "resume",
                build_create_request(
                    "/tmp/p".into(),
                    StreamingPreference::Partial,
                    Some("claude-id".into()),
                    false,
                    true,
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
                ),
            ),
        ];
        for (shape, request) in shapes {
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

    /// O3 ruling 2: the CLI's own permission prompts are asked for on every session this client
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
                let policy = build_create_request("/tmp/p".into(), StreamingPreference::Partial, resume, fork, offered)
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

    /// `prefix i`'s `settings` row (`setting_sources::note`) says what every session loads, so the
    /// request has to ask for exactly that. By default the sidecar is sent `[USER, PROJECT, LOCAL]`,
    /// in the proto enum's order, on a fresh, a resumed and a forked session alike; with
    /// `agent.user_settings = false` it is `[PROJECT, LOCAL]` and the note says the user's
    /// `~/.claude` is not loaded. The selection is stated on every request, never left to the CLI's
    /// own default. If either has to change, the note changes with it: it is what the panel tells
    /// the user.
    #[test]
    fn every_request_loads_the_tiers_the_note_says() {
        for (user_settings, expected, note_says) in [
            (
                true,
                vec![
                    SettingSource::User as i32,
                    SettingSource::Project as i32,
                    SettingSource::Local as i32,
                ],
                "user + project + local",
            ),
            (
                false,
                vec![SettingSource::Project as i32, SettingSource::Local as i32],
                "project + local only",
            ),
        ] {
            for (shape, resume, fork) in [
                ("fresh", None, false),
                ("resume", Some("claude-id".to_string()), false),
                ("fork", Some("claude-id".to_string()), true),
            ] {
                let policy = build_create_request_loading(
                    "/p".into(),
                    StreamingPreference::Partial,
                    resume,
                    fork,
                    false,
                    user_settings,
                )
                .policy
                .expect("a policy is always sent");
                let sources = policy
                    .setting_sources
                    .expect("stated on every request, never left to the CLI's own default");
                assert_eq!(sources.sources, expected, "{shape}, user_settings={user_settings}");
            }
            let note = crate::setting_sources::note_for(user_settings);
            assert!(
                note.starts_with(note_says),
                "user_settings={user_settings}: the note says what loads: {note}"
            );
        }
    }

    /// With nothing configured -- the state every test runs in, and the shipped default -- the
    /// request every session goes through loads the user tier, and the note says so.
    #[test]
    fn the_default_request_loads_the_user_tier_and_the_note_says_so() {
        let policy = build_create_request("/p".into(), StreamingPreference::Partial, None, false, false)
            .policy
            .expect("a policy is always sent");
        assert_eq!(
            policy.setting_sources.expect("stated").sources,
            vec![
                SettingSource::User as i32,
                SettingSource::Project as i32,
                SettingSource::Local as i32
            ]
        );
        assert_eq!(crate::setting_sources::note(), crate::setting_sources::NOTE_WITH_USER);
    }

    /// The shape the real sidecar returns at the revision this crate pins
    /// (`EXPECTED_VERDANDI_REVISION`, 133dc03) -- transcribed from
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
            protocol_minor: 0,
            sidecar_version: "0.1.0".into(),
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
                // definition. Added on Verdandi's side by `2fd30fb`.
                "resume_session",
                "fork_session",
                "setting_sources",
                "tool_policy",
                // Added at 133dc03, alongside `CreateSessionRequest`'s own `model`/`effort`/
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
                // Still advertised by the pinned sidecar; since R07 this client never calls it (spec
                // §6: removed, not disabled), so no capability is derived from it.
                "set_permission_mode",
                // Capability 'text_delta_message_id' -- see CAP_TEXT_DELTA_MESSAGE_ID.
                "text_delta_message_id",
                // Added at b3aa188 (2026-09-27): `ClaudeHostPolicy.provider_permission_prompts`,
                // the CLI's own permission prompts routed to the host as `PermissionRequested`
                // with origin PROVIDER_PROMPT. Transcribed from Verdandi's handshake literal
                // (`runtimeServiceImpl.ts`), which puts it last before the executable entries.
                "provider_permission_prompts",
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
                // host_cli. The rest (session_model onward) is transcribed from the 133dc03 proto
                // bump, not independently measured against a live sidecar -- see the live
                // conformance test's own note on that gap.
                "executable_host_cli",
                // `egress_restricted` is the one other conditional entry (present only when the
                // sidecar's own config enables it) and is deliberately NOT in this fixture, so the
                // live conformance test's tolerance for it is exercised against whatever this host
                // actually has, not hidden by a fixture that always includes it.
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            configuration_profiles: vec!["native".into(), "isolated".into()],
            permission_modes: vec!["interactive".into(), "verdandi_rules".into(), "bypass".into()],
            max_message_bytes: 4 * 1024 * 1024,
            event_buffer_policy: "bounded-1000".into(),
            // account_binding/account_name/account_config_dir: added at 133dc03.
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

    /// A sidecar without the gated policy is refused at session creation, naming what is missing
    /// and what it does offer -- never started under BYPASS instead (R07).
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
        // `resume_session` to the wire (`2fd30fb`) while `resume_session()` here still returned
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
        assert_eq!(info.sidecar_version, "0.1.0");
        // Raw, not filtered down to the ones this client recognizes -- an unrecognized future
        // capability must stay visible in diagnostics rather than vanish.
        assert!(info.advertised_permission_modes.contains(&"verdandi_rules".to_string()));
        // Twenty-four: the PACKAGED shape at b3aa188 (133dc03's twenty-three plus
        // provider_permission_prompts), which is what a shipped install meets. A checkout build
        // advertises one more, executable_sdk_bundled, and a sidecar with egress restrictions
        // enabled advertises egress_restricted besides. See real_handshake_today().
        assert_eq!(info.advertised_capabilities.len(), 24);
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
}
