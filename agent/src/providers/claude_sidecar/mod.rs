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

pub use spawn::EXPECTED_VERDANDI_REVISION;

use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, ProviderCapabilities,
    ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest, ResumeSessionRequest,
    SendTurnRequest, StreamingPreference,
};
use crate::runtime_thread::RuntimeThread;
use crate::{AgentDomainEvent, PermissionMode};
use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
use claude_runtime_protocol::v1::{
    ClaudeHostPolicy, CloseSessionRequest as ProtoCloseSessionRequest, ConfigurationProfile,
    CreateSessionRequest as ProtoCreateSessionRequest, ErrorCode as ProtoErrorCode, ExecutableSource,
    ErrorDetail, StreamingMode,
    HandshakeRequest, HandshakeResponse, InterruptTurnRequest as ProtoInterruptTurnRequest,
    PermissionMode as ProtoPermissionMode, PersistenceMode,
    ReplayStart, ResolvePermissionRequest as ProtoResolvePermissionRequest,
    SendTurnRequest as ProtoSendTurnRequest, WatchSessionEventsRequest,
};
use hyper_util::rt::TokioIo;
use spawn::SpawnedSidecar;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

const UNARY_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// The protocol major this client speaks. A sidecar reporting anything else is refused outright --
/// design doc §9.3: "major 不兼容：拒绝连接".
const CLIENT_PROTOCOL_MAJOR: u32 = 2;

/// Capability strings this client understands, as the sidecar advertises them in
/// `HandshakeResponse.capabilities`. Named constants rather than inline literals because a typo
/// here fails silently in the worst possible direction: the capability reads as absent, the feature
/// is hidden, and nothing anywhere reports a problem.
const CAP_INTERRUPT_TURN: &str = "interrupt_turn";
const CAP_RESUME_SESSION: &str = "resume_session";
const CAP_FORK_SESSION: &str = "fork_session";
const PERMISSION_MODE_BYPASS: &str = "bypass";
const PERMISSION_MODE_INTERACTIVE: &str = "interactive";

/// Whether THIS client can actually drive resume/fork end to end. Both are `false` until the phase
/// that implements them lands; flipping either one here is the single switch that turns the feature
/// on, and it must not be flipped before `resume_session` does something real.
///
/// A capability is the INTERSECTION of what the provider advertises and what this side implements.
/// Reporting the provider's advertisement alone would put a Resume control in the UI the moment the
/// sidecar learned the word, while this client still returned `UnsupportedCapability` -- a button
/// that cannot work is worse than no button, and it is exactly the "capability advertisement is a
/// contract" failure this design is trying to avoid, just with the lie on the client side.
const CLIENT_IMPLEMENTS_RESUME: bool = true;
/// Still false: the wire carries `fork`, and `build_create_request` can set it, but nothing in this
/// client asks for a fork and nothing has verified one end to end. Flip it in the change that does
/// both, not before.
const CLIENT_IMPLEMENTS_FORK: bool = false;

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
        interactive_permission_mode: response.permission_modes.iter().any(|m| m == PERMISSION_MODE_INTERACTIVE),
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
                        client.handshake(HandshakeRequest { client_protocol_major: CLIENT_PROTOCOL_MAJOR }).await
                    }
                },
                UNARY_RPC_TIMEOUT,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .map_err(|status| std::io::Error::other(format!("handshake failed: {status}")))?
            .into_inner();

        // Design doc §9.3: an incompatible major is refused, minor differences are tolerated
        // (protobuf unknown-field rules cover the new fields a higher minor may add). The sidecar
        // performs the mirror-image check on the client's major, but that only catches a client
        // that is too old -- this catches a sidecar that is too new, which is the direction a
        // stale pinned `rev` in Cargo.toml actually produces.
        if handshake.protocol_major != CLIENT_PROTOCOL_MAJOR {
            return Err(std::io::Error::other(format!(
                "claude-sidecar speaks protocol major {} but this client speaks {CLIENT_PROTOCOL_MAJOR}; \
                 refusing to continue rather than issuing commands it may silently misread",
                handshake.protocol_major
            )));
        }

        let capabilities = capabilities_from_handshake(&handshake);
        // Warnings only. The checkout DESCRIPTION is always present and travels separately as
        // `build_description`; mixing it in here made `startup_diagnostics` never empty, which lit
        // a permanent warning indicator in the UI for every healthy session.
        let mut diagnostics = sidecar.checkout_warnings.clone();
        diagnostics.extend(startup_diagnostics_from_stderr(&sidecar.stderr_tail()));
        let info = info_from_handshake(&handshake, Some(sidecar.checkout_description.clone()), diagnostics);
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
        let result = self.runtime.block_on(future, UNARY_RPC_TIMEOUT).map_err(|_| ProviderError::Timeout)?;
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

    /// Refuses a permission policy the live handshake does not advertise.
    ///
    /// The mode reaches here from a choice made on the start screen, before any provider existed --
    /// so the offer there is what this CLIENT implements, and this is the server half of the same
    /// two-term check `resume` already uses. Failing loudly is the whole point: the alternative,
    /// mapping an unsupported mode onto whatever the provider does support, would start an agent
    /// under a permission policy the user did not pick and would not be told about.
    fn require_permission_mode(&self, mode: PermissionMode) -> Result<(), ProviderError> {
        if self.capabilities.supports_permission_mode(mode) {
            return Ok(());
        }
        Err(ProviderError::Provider {
            code: ProviderErrorCode::InvalidConfiguration,
            message: format!(
                "this provider does not offer the {} permission policy (it advertises: {})",
                match mode {
                    PermissionMode::Auto => PERMISSION_MODE_INTERACTIVE,
                    PermissionMode::Bypass => PERMISSION_MODE_BYPASS,
                },
                if self.info.advertised_permission_modes.is_empty() {
                    "none".to_string()
                } else {
                    self.info.advertised_permission_modes.join(", ")
                }
            ),
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
        let client = self.client.clone();
        let events = Arc::clone(&self.events);
        let backpressure = Arc::clone(&self.backpressure);
        self.runtime.handle().spawn(async move {
            let mut tracker = watch::SequenceTracker::new();
            // Counts only CONSECUTIVE failed attempts: a reconnect that actually delivered a new
            // event resets it, so the budget bounds "getting nowhere", not "reconnecting at all".
            let mut consecutive_failures = 0usize;

            // The single exit for every abnormal path. Nothing here may return quietly except the
            // one genuinely normal ending: a session that closed on its own terms.
            let report = |reason: String| {
                eprintln!("agent: ClaudeSidecarProvider: event stream lost: {reason}");
                events.lock().unwrap().push(AgentDomainEvent::SessionUnavailable { reason });
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
                                    if let Some(domain_event) = translate::translate(event) {
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

/// Builds the one `CreateSessionRequest` both a fresh session and a resume go through.
///
/// The policy is fixed rather than parameterised on purpose: this client only ever sends values
/// whose runtime behavior has been confirmed distinct. `verdandi_rules` is identical to
/// `interactive` in the current sidecar, `external_store` is identical to `ephemeral`, and
/// `executable` is not read at all -- sending any of them would be choosing a value that means
/// nothing. `streaming` is the exception that proves the rule: PARTIAL has a measured, distinct
/// effect, so it is sent.
fn build_create_request(
    cwd: String,
    permission_mode: PermissionMode,
    streaming: StreamingPreference,
    resume_provider_session_id: Option<String>,
    fork: bool,
) -> ProtoCreateSessionRequest {
    ProtoCreateSessionRequest {
        cwd,
        policy: Some(ClaudeHostPolicy {
            configuration: ConfigurationProfile::Native as i32,
            permissions: to_proto_permission_mode(permission_mode),
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
        }),
        resume_provider_session_id,
        fork,
    }
}

fn to_proto_permission_mode(mode: PermissionMode) -> i32 {
    match mode {
        PermissionMode::Auto => ProtoPermissionMode::Interactive as i32,
        PermissionMode::Bypass => ProtoPermissionMode::Bypass as i32,
    }
}

impl AgentProvider for ClaudeSidecarProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    fn create_session(&self, request: CreateSessionRequest) -> Result<String, ProviderError> {
        self.require_permission_mode(request.permission_mode)?;
        self.open_session(build_create_request(request.cwd, request.permission_mode, request.streaming, None, false))
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
        // original session ran under, so a resume into an unsupported mode would be a conversation
        // coming back with a permission posture nobody chose.
        self.require_permission_mode(request.permission_mode)?;
        self.open_session(build_create_request(
            request.cwd,
            request.permission_mode,
            request.streaming,
            Some(request.provider_session_id),
            false,
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
        let proto_request = ProtoInterruptTurnRequest { session_id: request.session_id, command_id: uuid::Uuid::new_v4().to_string() };
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
        let proto_request = ProtoCloseSessionRequest { session_id: request.session_id, command_id: uuid::Uuid::new_v4().to_string() };
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

    /// The exact shape today's real sidecar returns -- copied from
    /// `apps/claude-sidecar/src/runtimeServiceImpl.ts`'s handshake handler, not invented.
    fn real_handshake_today() -> HandshakeResponse {
        HandshakeResponse {
            protocol_major: 1,
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
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            configuration_profiles: vec!["native".into(), "isolated".into()],
            permission_modes: vec!["interactive".into(), "verdandi_rules".into(), "bypass".into()],
            max_message_bytes: 4 * 1024 * 1024,
            event_buffer_policy: "bounded-1000".into(),
        }
    }

    #[test]
    fn todays_real_sidecar_advertises_interrupt_and_bypass_but_not_resume_or_fork() {
        let capabilities = capabilities_from_handshake(&real_handshake_today());
        assert!(capabilities.interrupt, "interrupt_turn is advertised and works (proven by the conformance suite)");
        assert!(capabilities.bypass_permission_mode, "bypass is advertised, and is this milestone's only permission policy");
        assert!(!capabilities.resume, "no resume_session capability exists on the wire yet");
        assert!(!capabilities.fork, "no fork_session capability exists on the wire yet");
    }

    #[test]
    fn a_capability_the_provider_does_not_advertise_is_never_reported() {
        let mut response = real_handshake_today();
        response.capabilities.retain(|c| c != "interrupt_turn");
        assert!(!capabilities_from_handshake(&response).interrupt);
    }

    #[test]
    fn a_provider_advertising_resume_still_reports_false_until_this_client_implements_it() {
        // The regression this pins: the day Verdandi starts advertising `resume_session`, this
        // client must not start reporting `resume: true` -- `resume_session()` here still returns
        // UnsupportedCapability, so a UI acting on the capability would render a control that
        // cannot work. Flipping CLIENT_IMPLEMENTS_RESUME is what changes this, and that flip must
        // happen in the same change that implements the call.
        let mut response = real_handshake_today();
        response.capabilities.push("resume_session".into());
        response.capabilities.push("fork_session".into());
        let capabilities = capabilities_from_handshake(&response);
        assert_eq!(capabilities.resume, CLIENT_IMPLEMENTS_RESUME);
        assert_eq!(capabilities.fork, CLIENT_IMPLEMENTS_FORK);
    }

    #[test]
    fn bypass_is_not_reported_when_the_provider_does_not_offer_it() {
        let mut response = real_handshake_today();
        response.permission_modes.retain(|m| m != "bypass");
        assert!(!capabilities_from_handshake(&response).bypass_permission_mode);
    }

    #[test]
    fn info_carries_the_advertised_lists_verbatim_for_diagnostics() {
        let info = info_from_handshake(&real_handshake_today(), Some("checkout @ abc1234".into()), vec!["diag".into()]);
        assert_eq!(info.actual_claude_code_version, "2.1.269");
        assert_eq!(info.protocol_major, 1);
        assert_eq!(info.sidecar_version, "0.1.0");
        // Raw, not filtered down to the ones this client recognizes -- an unrecognized future
        // capability must stay visible in diagnostics rather than vanish.
        assert!(info.advertised_permission_modes.contains(&"verdandi_rules".to_string()));
        assert_eq!(info.advertised_capabilities.len(), 7);
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
}
