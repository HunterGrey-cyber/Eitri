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
    ProviderError, ResolvePermissionRequest, ResumeSessionRequest, SendTurnRequest,
};
use crate::runtime_thread::RuntimeThread;
use crate::{AgentDomainEvent, PermissionMode};
use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
use claude_runtime_protocol::v1::{
    ClaudeHostPolicy, CloseSessionRequest as ProtoCloseSessionRequest, ConfigurationProfile,
    CreateSessionRequest as ProtoCreateSessionRequest, ExecutableSource, ErrorDetail,
    HandshakeRequest, InterruptTurnRequest as ProtoInterruptTurnRequest, PermissionMode as ProtoPermissionMode,
    PersistenceMode, ResolvePermissionRequest as ProtoResolvePermissionRequest,
    SendTurnRequest as ProtoSendTurnRequest, WatchSessionEventsRequest,
};
use hyper_util::rt::TokioIo;
use spawn::SpawnedSidecar;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

const UNARY_RPC_TIMEOUT: Duration = Duration::from_secs(10);

// `RuntimeServiceClient<Channel>` is already cheaply `Clone` (tonic's generated clients wrap a
// `Channel`, designed for exactly this "clone one per concurrent call" pattern -- cloning shares
// the same underlying HTTP/2 connection). No `Mutex` is needed to clone it from `&self`; a `Mutex`
// here would be protecting nothing (`.clone()` never needs exclusive access).
pub struct ClaudeSidecarProvider {
    _sidecar: SpawnedSidecar,
    runtime: RuntimeThread,
    client: RuntimeServiceClient<Channel>,
    events: Arc<Mutex<Vec<AgentDomainEvent>>>,
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

        let provider = Self { _sidecar: sidecar, runtime, client, events: Arc::new(Mutex::new(Vec::new())) };
        provider
            .runtime
            .block_on(
                {
                    let mut client = provider.client.clone();
                    async move { client.handshake(HandshakeRequest { client_protocol_major: 1 }).await }
                },
                UNARY_RPC_TIMEOUT,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .map_err(|status| std::io::Error::other(format!("handshake failed: {status}")))?;

        Ok(provider)
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
                return ProviderError::Provider(detail.message);
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
        ProviderCapabilities { resume: false }
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
