// agent/src/providers/claude_sidecar/mod.rs
//! `ClaudeSidecarProvider`: drives Verdandi's `apps/claude-sidecar` gRPC service (design doc §10.1,
//! Phase 3). Spawns and owns exactly one sidecar process for its own lifetime (Task 6), connects to
//! it over a UDS via `tonic` on a dedicated `RuntimeThread` (Task 3), and issues the 6 in-scope
//! unary RPCs synchronously (this file) plus the streaming event pump (Task 8). `ResumeSession` has
//! no RPC to call yet (design doc §19) -- `resume_session` always returns
//! `ProviderError::UnsupportedCapability`.

mod spawn;
mod translate;

use crate::provider::{
    AgentProvider, CloseSessionRequest, CreateSessionRequest, InterruptTurnRequest, ProviderCapabilities,
    ProviderError, ProviderErrorCode, ProviderInfo, ResolvePermissionRequest, ResumeSessionRequest,
    SendTurnRequest,
};
use crate::runtime_thread::RuntimeThread;
use crate::{AgentDomainEvent, PermissionMode};
use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
use claude_runtime_protocol::v1::{
    ClaudeHostPolicy, CloseSessionRequest as ProtoCloseSessionRequest, ConfigurationProfile,
    CreateSessionRequest as ProtoCreateSessionRequest, ErrorCode as ProtoErrorCode, ExecutableSource,
    ErrorDetail,
    HandshakeRequest, HandshakeResponse, InterruptTurnRequest as ProtoInterruptTurnRequest,
    PermissionMode as ProtoPermissionMode, PersistenceMode,
    ResolvePermissionRequest as ProtoResolvePermissionRequest, SendTurnRequest as ProtoSendTurnRequest,
    WatchSessionEventsRequest,
};
use hyper_util::rt::TokioIo;
use spawn::SpawnedSidecar;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

const UNARY_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// The protocol major this client speaks. A sidecar reporting anything else is refused outright --
/// design doc §9.3: "major 不兼容：拒绝连接".
const CLIENT_PROTOCOL_MAJOR: u32 = 1;

/// Capability strings this client understands, as the sidecar advertises them in
/// `HandshakeResponse.capabilities`. Named constants rather than inline literals because a typo
/// here fails silently in the worst possible direction: the capability reads as absent, the feature
/// is hidden, and nothing anywhere reports a problem.
const CAP_INTERRUPT_TURN: &str = "interrupt_turn";
const CAP_RESUME_SESSION: &str = "resume_session";
const CAP_FORK_SESSION: &str = "fork_session";
const PERMISSION_MODE_BYPASS: &str = "bypass";

/// Whether THIS client can actually drive resume/fork end to end. Both are `false` until the phase
/// that implements them lands; flipping either one here is the single switch that turns the feature
/// on, and it must not be flipped before `resume_session` does something real.
///
/// A capability is the INTERSECTION of what the provider advertises and what this side implements.
/// Reporting the provider's advertisement alone would put a Resume control in the UI the moment the
/// sidecar learned the word, while this client still returned `UnsupportedCapability` -- a button
/// that cannot work is worse than no button, and it is exactly the "capability advertisement is a
/// contract" failure this design is trying to avoid, just with the lie on the client side.
const CLIENT_IMPLEMENTS_RESUME: bool = false;
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
    }
}

fn info_from_handshake(response: &HandshakeResponse, startup_diagnostics: Vec<String>) -> ProviderInfo {
    ProviderInfo {
        sidecar_version: response.sidecar_version.clone(),
        claude_agent_sdk_version: response.claude_agent_sdk_version.clone(),
        actual_claude_code_version: response.actual_claude_code_version.clone(),
        protocol_major: response.protocol_major,
        protocol_minor: response.protocol_minor,
        advertised_capabilities: response.capabilities.clone(),
        advertised_permission_modes: response.permission_modes.clone(),
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
        let info = info_from_handshake(&handshake, startup_diagnostics_from_stderr(&sidecar.stderr_tail()));
        for diagnostic in &info.startup_diagnostics {
            eprintln!("agent: ClaudeSidecarProvider: {diagnostic}");
        }

        Ok(Self {
            sidecar,
            runtime,
            client,
            events: Arc::new(Mutex::new(Vec::new())),
            capabilities,
            info,
        })
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

    fn start_watching(&self, session_id: String) {
        let mut client = self.client.clone();
        let events = Arc::clone(&self.events);
        self.runtime.handle().spawn(async move {
            let request = WatchSessionEventsRequest { session_id, after_sequence: 0 };
            let mut stream = match client.watch_session_events(request).await {
                Ok(response) => response.into_inner(),
                Err(status) => {
                    eprintln!("agent: ClaudeSidecarProvider: WatchSessionEvents failed to open: {status}");
                    return;
                }
            };
            loop {
                match stream.message().await {
                    Ok(Some(event)) => {
                        if let Some(domain_event) = translate::translate(event) {
                            events.lock().unwrap().push(domain_event);
                        }
                    }
                    Ok(None) => break, // server closed the stream -- session is gone
                    Err(status) => {
                        eprintln!("agent: ClaudeSidecarProvider: WatchSessionEvents stream error: {status}");
                        break;
                    }
                }
            }
        });
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
        let mut client = self.client.clone();
        let proto_request = ProtoCreateSessionRequest {
            cwd: request.cwd,
            policy: Some(ClaudeHostPolicy {
                configuration: ConfigurationProfile::Native as i32,
                permissions: to_proto_permission_mode(request.permission_mode),
                persistence: PersistenceMode::HostCli as i32,
                executable: ExecutableSource::HostCli as i32,
            }),
        };
        let response = self.run_unary(async move { client.create_session(proto_request).await })?;
        self.start_watching(response.session_id.clone());
        Ok(response.session_id)
    }

    fn resume_session(&self, _request: ResumeSessionRequest) -> Result<String, ProviderError> {
        Err(ProviderError::UnsupportedCapability("resume"))
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
        let proto_request = ProtoResolvePermissionRequest {
            session_id: request.session_id,
            command_id: uuid::Uuid::new_v4().to_string(),
            permission_id: request.permission_id,
            allow: request.allow,
            reason: request.reason.unwrap_or_default(),
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
        let info = info_from_handshake(&real_handshake_today(), vec!["diag".into()]);
        assert_eq!(info.actual_claude_code_version, "2.1.269");
        assert_eq!(info.protocol_major, 1);
        assert_eq!(info.sidecar_version, "0.1.0");
        // Raw, not filtered down to the ones this client recognizes -- an unrecognized future
        // capability must stay visible in diagnostics rather than vanish.
        assert!(info.advertised_permission_modes.contains(&"verdandi_rules".to_string()));
        assert_eq!(info.advertised_capabilities.len(), 7);
        assert_eq!(info.startup_diagnostics, vec!["diag".to_string()]);
    }

    #[test]
    fn the_cli_compatibility_warning_is_picked_out_of_real_sidecar_stderr() {
        // Verbatim from a real run of the fixed sidecar against CLI 2.1.269, as it arrives through
        // SpawnedSidecar's stderr tail.
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
