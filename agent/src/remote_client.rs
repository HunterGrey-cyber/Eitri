//! The laptop's end of one connection to the server helper: `eitri-server connect` on one channel
//! (an ssh channel through the host's master, or a local fixture), its stdin and stdout carrying one
//! HTTP/2 connection. The helper's own `Helper` service and Verdandi's `RuntimeService` (routed to a
//! sidecar by `eitri-sidecar` metadata) share it.
//!
//! Every blocking method here runs its call on the connection's own runtime thread and waits on the
//! caller's thread, so it belongs on a worker, never on the GTK thread or on that runtime. When the
//! connection breaks, the next call starts a new channel through the launcher.

use crate::runtime_thread::RuntimeThread;
use crate::ssh::{ChannelLauncher, LinkCheck};
use eitri_server_proto::helper_client::HelperClient as HelperService;
use eitri_server_proto::{
    HelloRequest, HelloResponse, ListSidecarsRequest, PendingPermission, PendingRequest, SidecarInfo,
    StartSidecarRequest, StopSidecarRequest, TrustScanRequest, TrustScanResponse, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::pin::Pin;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::Status;

/// Wire and async types used by a terminal link on this connection's runtime.
/// Keeping these here lets the toolkit-free core use the transport without a second dependency stack.
pub mod terminal_transport {
    pub use eitri_server_proto as wire;
    pub use tokio;
    pub use tonic;
    pub use tonic::codegen::tokio_stream;
}

/// Reconnect delays shared by remote agents and terminals, repeating the last delay indefinitely.
pub const REMOTE_RECONNECT_BACKOFF: [Duration; 8] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

/// HTTP/2 keepalive on the helper connection: a ping every ten seconds, and a connection whose ping
/// goes twenty seconds unanswered is dead. A closed lid shows up within half a minute, not when the
/// kernel gives up on the TCP connection under ssh.
pub const HELPER_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
pub const HELPER_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// The bound on opening the connection, Hello and the other quick calls.
const HELPER_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// The bound on `StartSidecar`: the helper may capture the login environment (five seconds) and waits
/// up to fifteen seconds for the sidecar to bind.
const START_SIDECAR_TIMEOUT: Duration = Duration::from_secs(60);

/// The bound on a trust scan, which walks and hashes a tree on the server.
const TRUST_SCAN_TIMEOUT: Duration = Duration::from_secs(120);

/// The clock a remote tab's reconnects wait on: real time in the product, a clock that lets hours pass
/// in moments in tests.
pub trait ReconnectClock: Send + Sync {
    fn sleep(&self, wait: Duration) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>;
    fn now_unix_ms(&self) -> u64;
}

/// Real time.
pub struct SystemClock;

impl ReconnectClock for SystemClock {
    fn sleep(&self, wait: Duration) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        Box::pin(tokio::time::sleep(wait))
    }

    fn now_unix_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64)
    }
}

/// What a remote tab asks the helper to start: the project root as recorded for the remote project,
/// the account by name, and the sidecar the laptop installed there. Nothing from this machine's
/// environment travels: the server applies the shared environment policy to its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarStart {
    pub root: std::path::PathBuf,
    pub account: Option<String>,
    pub command_id: String,
    /// The installed sidecar's directory under the helper's sidecar store (its revision and the start of its
    /// hash), not the bare revision: the helper joins this as the directory's name and checks the hash
    /// below against what it finds there. The wire field keeps the name `sidecar_revision`.
    pub sidecar_revision: String,
    pub sidecar_sha256: String,
}

pub use eitri_server_proto::{
    exec_input, exec_output, ExecExit, ExecInput, ExecOutput, ExecPurpose, ExecStart, GitConfigEntry, GitConfigRequest,
    GitConfigResponse, ListTreeEntry, ListTreeRequest, PathSize, PathSizesRequest, PathSizesResponse,
    ProjectActivityRequest, ProjectActivityResponse, ReadFileChunk, ReadFileRequest, RemoveFileIfRequest,
    RemoveFileIfResponse, ReplaceFileIfRequest, ReplaceFileIfResponse, SyncIgnoresRequest, SyncIgnoresResponse,
    WaitFileHashRequest, WaitFileHashResponse, WatchModesRequest, WatchModesResponse, CAP_EXEC, CAP_GIT_CONFIG,
    CAP_INSTALLATION_ARGS, CAP_SYNC_FILES, CAP_SYNC_IGNORES, CAP_SYNC_MODES,
};

/// A checked START proved that an older installation was collected with no surviving agents.
pub const MISSING_INSTALLATION: &str = "the owning helper installation is gone";

#[derive(Default)]
struct RouteState {
    started: AtomicBool,
    gone: AtomicBool,
}

type Routes = Arc<Mutex<std::collections::HashMap<String, std::sync::Weak<Inner>>>>;

/// One connection to the helper on `host`. Cheap to clone; clones share the connection.
#[derive(Clone)]
pub struct HelperClient {
    inner: Arc<Inner>,
}

struct Inner {
    hello: Mutex<Option<HelloResponse>>,
    host: String,
    daemon: String,
    routes: Routes,
    installations: Arc<Mutex<Option<Vec<String>>>>,
    state: Arc<RouteState>,
    runtime: Arc<RuntimeThread>,
    channel: Channel,
    launcher: Arc<dyn ChannelLauncher>,
    /// The last line the latest channel wrote to stderr: ssh says there why a connection failed.
    stderr: Arc<ChannelStderr>,
    /// Why the latest channel was refused (the connection showed a host key other than the pinned one,
    /// or none could be read), until a channel or a link check passes the comparison again.
    refused: Arc<Mutex<Option<String>>>,
    /// The SHA-256 fingerprint of the project this connection was opened for. Every channel, the first
    /// included, is refused unless the connection under it shows this key.
    pinned: Option<String>,
}

impl HelperClient {
    /// Opens the connection: starts `<helper> connect` through `launcher` and speaks HTTP/2 over its
    /// stdio. `host` is the host as the person typed it, for the messages a person reads.
    ///
    /// `pinned_host_key` is the SHA-256 host-key fingerprint the project this connection serves was
    /// recorded with. With one, the connection under every channel is compared with it before the
    /// channel starts, the very first one included: a connection showing another key, or one whose key
    /// cannot be read, starts no channel, and the connect fails naming the host and both keys. A
    /// connection that serves no recorded project (a probe before there is a record) passes `None`.
    pub fn connect(
        launcher: Arc<dyn ChannelLauncher>,
        host: &str,
        pinned_host_key: Option<&str>,
    ) -> io::Result<HelperClient> {
        Self::connect_daemon(launcher, host, pinned_host_key, "")
    }

    pub fn connect_daemon(
        launcher: Arc<dyn ChannelLauncher>,
        host: &str,
        pinned_host_key: Option<&str>,
        daemon: &str,
    ) -> io::Result<HelperClient> {
        let runtime = Arc::new(RuntimeThread::spawn());
        let state = Arc::new(RouteState::default());
        let stderr = Arc::new(ChannelStderr::default());
        let refused: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let pinned = pinned_host_key.map(str::to_string);
        let channel = {
            let launcher = Arc::clone(&launcher);
            let stderr = Arc::clone(&stderr);
            let refused = Arc::clone(&refused);
            let pinned = pinned.clone();
            let pinned_host = host.to_string();
            let daemon = daemon.to_string();
            let state = state.clone();
            runtime
                .block_on(
                    async move {
                        Endpoint::try_from("http://[::]:50051")
                            .map_err(io::Error::other)?
                            .http2_keep_alive_interval(HELPER_KEEPALIVE_INTERVAL)
                            .keep_alive_timeout(HELPER_KEEPALIVE_TIMEOUT)
                            .keep_alive_while_idle(true)
                            .connect_with_connector(tower::service_fn(move |_: Uri| {
                                let opening = Opening {
                                    launcher: Arc::clone(&launcher),
                                    daemon: daemon.clone(),
                                    state: state.clone(),
                                    stderr: Arc::clone(&stderr),
                                    pin: KeyPin {
                                        pinned: pinned.clone(),
                                        refused: Arc::clone(&refused),
                                        host: pinned_host.clone(),
                                    },
                                };
                                async move { open_channel(opening).await }
                            }))
                            .await
                            .map_err(|error| io::Error::new(io::ErrorKind::ConnectionRefused, error))
                    },
                    HELPER_RPC_TIMEOUT,
                )
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("the helper on {host} did not answer in time"),
                    )
                })?
        };
        let channel = channel.map_err(|error| {
            // A channel refused on its host key never started, so no stderr line of its own says why.
            let why = stderr_line(&refused)
                .or_else(|| settled_stderr(&stderr))
                .map(|line| format!(": {line}"))
                .unwrap_or_default();
            io::Error::new(
                error.kind(),
                format!("could not reach the helper on {host} ({error}){why}"),
            )
        })?;
        let client = HelperClient {
            inner: Arc::new(Inner {
                hello: Mutex::new(None),
                host: host.to_string(),
                daemon: daemon.to_string(),
                routes: Default::default(),
                installations: Default::default(),
                state,
                runtime,
                channel,
                launcher,
                stderr,
                refused,
                pinned,
            }),
        };
        client
            .inner
            .routes
            .lock()
            .expect("helper routes")
            .insert(daemon.into(), Arc::downgrade(&client.inner));
        Ok(client)
    }

    pub fn daemon(&self) -> &str {
        &self.inner.daemon
    }

    /// Shares a lazy route while an installation still has users. Empty selects this connection.
    /// No network or disk work here.
    pub fn for_daemon(&self, daemon: &str) -> io::Result<Self> {
        if daemon.is_empty() || daemon == self.daemon() {
            return Ok(self.clone());
        }
        if !daemon
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "saved agent has no valid helper installation",
            ));
        }
        let mut routes = self.inner.routes.lock().expect("helper routes");
        routes.retain(|_, route| route.strong_count() > 0);
        if let Some(inner) = routes.get(daemon).and_then(std::sync::Weak::upgrade) {
            return Ok(Self { inner });
        }
        let state = Arc::new(RouteState::default());
        let opening_state = state.clone();
        let launcher = self.inner.launcher.clone();
        let stderr = Arc::new(ChannelStderr::default());
        let pin = KeyPin {
            pinned: self.inner.pinned.clone(),
            refused: self.inner.refused.clone(),
            host: self.inner.host.clone(),
        };
        let route = daemon.to_string();
        let errors = stderr.clone();
        let _entered = self.inner.runtime.handle().enter();
        let channel = Endpoint::from_static("http://[::]:50051")
            .http2_keep_alive_interval(HELPER_KEEPALIVE_INTERVAL)
            .keep_alive_timeout(HELPER_KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true)
            .connect_with_connector_lazy(tower::service_fn(move |_: Uri| {
                open_channel(Opening {
                    launcher: launcher.clone(),
                    stderr: errors.clone(),
                    pin: pin.clone(),
                    daemon: route.clone(),
                    state: opening_state.clone(),
                })
            }));
        let client = Self {
            inner: Arc::new(Inner {
                hello: Mutex::new(None),
                host: self.inner.host.clone(),
                daemon: daemon.into(),
                routes: self.inner.routes.clone(),
                installations: self.inner.installations.clone(),
                state,
                runtime: self.inner.runtime.clone(),
                channel,
                launcher: self.inner.launcher.clone(),
                stderr,
                refused: self.inner.refused.clone(),
                pinned: self.inner.pinned.clone(),
            }),
        };
        routes.insert(daemon.into(), Arc::downgrade(&client.inner));
        Ok(client)
    }

    /// Sets the installations this host record can ask about ownerless sidecars. No I/O here.
    pub fn with_installations(self, installations: impl IntoIterator<Item = String>) -> Self {
        *self.inner.installations.lock().expect("helper installations") = Some(installations.into_iter().collect());
        self
    }

    /// A move is adopted on the UI thread. It can reuse a known owner without I/O; unresolved
    /// ownership must be settled by the worker reattachment before this tab can move. A default
    /// connection without an installation inventory retains its default helper route.
    pub(crate) fn for_known_owner(&self, daemon: &str) -> io::Result<Self> {
        let installations = self.inner.installations.lock().expect("helper installations");
        if daemon.is_empty() && self.daemon().is_empty() && installations.is_none() {
            return Ok(self.clone());
        }
        if daemon.is_empty()
            || installations
                .as_ref()
                .is_some_and(|known| !known.iter().any(|name| name == daemon))
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the agent's helper installation must be resolved before moving this tab",
            ));
        }
        self.for_daemon(daemon)
    }

    /// Resolves an ownerless handoff across the recorded installations. A named owner remains pinned
    /// even when the current inventory no longer knows how to reach it.
    pub fn for_sidecar(&self, daemon: &str, sidecar: &str) -> Result<Self, Status> {
        if !daemon.is_empty() {
            return self
                .for_daemon(daemon)
                .map_err(|error| Status::unavailable(error.to_string()));
        }
        self.for_sidecar_with(daemon, sidecar, Self::list)
    }

    fn for_sidecar_with(
        &self,
        daemon: &str,
        sidecar: &str,
        mut list: impl FnMut(&Self) -> Result<Vec<SidecarInfo>, Status>,
    ) -> Result<Self, Status> {
        let installations = self.inner.installations.lock().expect("helper installations").clone();
        if !daemon.is_empty()
            && installations
                .as_ref()
                .is_none_or(|known| known.iter().any(|name| name == daemon))
        {
            return self
                .for_daemon(daemon)
                .map_err(|error| Status::unavailable(error.to_string()));
        }
        let candidates = installations.unwrap_or_else(|| vec![self.daemon().to_owned()]);
        let mut found = None;
        let mut failure = None;
        // Keep candidates alive through the decision, including their channels and definite-loss state.
        let mut checked = Vec::new();
        for name in candidates {
            let route = self
                .for_daemon(&name)
                .map_err(|error| Status::unavailable(error.to_string()))?;
            match list(&route) {
                Ok(list) if list.iter().any(|info| info.sidecar_id == sidecar && info.alive) => return Ok(route),
                Ok(list) => {
                    if list.iter().any(|info| info.sidecar_id == sidecar) {
                        found = Some(route.clone());
                    }
                }
                Err(_) if route.installation_gone() => {}
                Err(status) => failure = Some(status),
            }
            checked.push(route);
        }
        if let Some(status) = failure {
            return Err(Status::unavailable(format!(
                "cannot determine the agent's owning installation: {}",
                status.message()
            )));
        }
        Ok(found.unwrap_or_else(|| self.clone()))
    }

    /// Stops a saved sidecar through its owner, resolving legacy or forgotten owners first.
    pub fn stop_saved(&self, daemon: &str, sidecar: &str) -> Result<(), String> {
        self.for_sidecar_with(daemon, sidecar, Self::list)
            .map_err(|status| status.message().to_owned())?
            .stop_if_known(sidecar)
    }

    pub(crate) fn installation_gone(&self) -> bool {
        self.inner.state.gone.load(Ordering::Acquire)
    }

    /// The host as the person typed it.
    pub fn host(&self) -> &str {
        &self.inner.host
    }

    /// The HTTP/2 connection both services share. A clone starts no new channel.
    pub fn channel(&self) -> Channel {
        self.inner.channel.clone()
    }

    /// The terminal service's channel and the runtime it runs on. The caller keeps this client alive
    /// outside tasks on that runtime; tasks keep only the channel and handle.
    pub fn terminal_channel(&self) -> Result<(Channel, tokio::runtime::Handle), Status> {
        self.require_pin()?;
        Ok((self.channel(), self.inner.runtime.handle().clone()))
    }

    /// Opens an independent connection through this connection's launcher and installation routing.
    /// Blocking: a worker's.
    pub fn terminal_connection(&self, installation: &str) -> io::Result<Self> {
        self.terminal_connection_through(installation, || Ok(Arc::clone(&self.inner.launcher)))
    }

    /// [`Self::terminal_connection`] through the launcher `launcher` gives: a window's terminals go through an ssh
    /// master of their own, so their output never shares a TCP connection with the agents' channels. The new
    /// connection keeps this one's host and pinned key, compared before every channel it opens, the first
    /// included. The pin and the installation's name are checked before `launcher` runs. Blocking: a worker's.
    pub fn terminal_connection_through(
        &self,
        installation: &str,
        launcher: impl FnOnce() -> io::Result<Arc<dyn ChannelLauncher>>,
    ) -> io::Result<Self> {
        self.require_pin().map_err(io::Error::other)?;
        if !installation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid helper installation",
            ));
        }
        let installation = if installation.is_empty() {
            self.daemon()
        } else {
            installation
        };
        Self::connect_daemon(launcher()?, self.host(), self.inner.pinned.as_deref(), installation)
    }

    pub(crate) fn runtime(&self) -> Arc<RuntimeThread> {
        Arc::clone(&self.inner.runtime)
    }

    /// The last line the latest channel wrote to stderr, if any.
    pub fn last_stderr(&self) -> Option<String> {
        settled_stderr(&self.inner.stderr)
    }

    /// What checks this connection's link, without holding the connection: a task on the connection's
    /// own runtime keeps this, never the client, so the runtime ends with its last owner outside it.
    /// A master restarted onto a key other than the one the connection was opened for carries no channel
    /// until it shows that key again.
    ///
    /// `pinned_host_key` is the SHA-256 fingerprint the tab's project was recorded with. One connection
    /// serves one recorded key: a key other than the connection's, or any key for a connection opened
    /// without one, is refused, since that tab's channels would be compared with a key it was never
    /// recorded with, or with none.
    pub fn link_checker(&self, pinned_host_key: Option<&str>) -> io::Result<LinkChecker> {
        if let Some(key) = pinned_host_key {
            match self.inner.pinned.as_deref() {
                Some(pinned) if pinned == key => {}
                Some(pinned) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "the connection to {} was opened for host key {pinned}, not the {key} this project was \
                             recorded with",
                            self.host()
                        ),
                    ))
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "the connection to {} was opened without the host key {key} this project was recorded \
                             with",
                            self.host()
                        ),
                    ))
                }
            }
        }
        Ok(LinkChecker {
            state: self.inner.state.clone(),
            launcher: Arc::clone(&self.inner.launcher),
            pin: KeyPin {
                pinned: self.inner.pinned.clone(),
                refused: Arc::clone(&self.inner.refused),
                host: self.inner.host.clone(),
            },
        })
    }

    /// Refuses a project call on a connection opened without a pinned host key: its channels were never
    /// compared with the key a project was recorded with.
    fn require_pin(&self) -> Result<(), Status> {
        match self.inner.pinned {
            Some(_) => Ok(()),
            None => Err(Status::failed_precondition(format!(
                "the connection to {} was opened without a project's host key; it serves no project",
                self.host()
            ))),
        }
    }

    /// What asks this connection's helper, from a task on its own runtime, which permission requests
    /// one session of `sidecar_id` has waiting. It holds the connection's channel and nothing that owns
    /// the runtime.
    pub(crate) fn pending_asker(&self, sidecar_id: &str) -> PendingAsker {
        PendingAsker {
            channel: self.channel(),
            sidecar_id: sidecar_id.to_string(),
        }
    }

    fn service(&self) -> HelperService<Channel> {
        HelperService::new(self.channel())
    }

    fn call<T, F>(&self, limit: Duration, future: F) -> Result<T, Status>
    where
        F: std::future::Future<Output = Result<tonic::Response<T>, Status>> + Send + 'static,
        T: Send + 'static,
    {
        self.call_raw(limit, future)
            .map_err(|status| self.with_channel_reason(status))
    }

    /// [`Self::call`] with the status as it came: the code and the transport source a retry is decided
    /// on, before the channel's words are added.
    fn call_raw<T, F>(&self, limit: Duration, future: F) -> Result<T, Status>
    where
        F: std::future::Future<Output = Result<tonic::Response<T>, Status>> + Send + 'static,
        T: Send + 'static,
    {
        if self.installation_gone() {
            return Err(Status::not_found(MISSING_INSTALLATION));
        }
        let deadline = format!(
            "the helper on {} did not answer within {} s",
            self.host(),
            limit.as_secs()
        );
        let expired = deadline.clone();
        // The caller's receive deadline alone leaves the spawned task holding its channel forever.
        let bounded = async move {
            tokio::time::timeout(limit, future)
                .await
                .unwrap_or_else(|_| Err(Status::deadline_exceeded(expired)))
        };
        match self.inner.runtime.block_on(bounded, limit) {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(_)) if self.installation_gone() => Err(Status::not_found(MISSING_INSTALLATION)),
            Ok(Err(status)) => Err(status),
            Err(_) => Err(Status::deadline_exceeded(deadline)),
        }
    }

    /// A call that failed because its channel did adds why: the host-key refusal that kept a new channel
    /// from starting, or else the channel's own last word (ssh's "Permission denied", the helper saying
    /// its daemon is not running); a refusal from the helper stays as it is.
    fn with_channel_reason(&self, status: Status) -> Status {
        if !matches!(status.code(), tonic::Code::Unavailable | tonic::Code::Unknown)
            || std::error::Error::source(&status).is_none()
        {
            return status;
        }
        match stderr_line(&self.inner.refused).or_else(|| settled_stderr(&self.inner.stderr)) {
            Some(line) if !status.message().contains(&line) => {
                Status::new(status.code(), format!("{} ({line})", status.message()))
            }
            _ => status,
        }
    }

    /// Hello, refusing a helper that speaks another protocol major; the refusal names both sides'
    /// versions.
    pub fn hello(&self) -> Result<HelloResponse, Status> {
        let request = HelloRequest {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let mut attempt = 0;
        let response = loop {
            let mut service = self.service();
            let request = request.clone();
            let result = self.call_raw(HELPER_RPC_TIMEOUT, async move { service.hello(request).await });
            match &result {
                Err(status) if answer_lost(status) && attempt < START_RETRIES.len() => {
                    std::thread::sleep(START_RETRIES[attempt]);
                    attempt += 1;
                }
                _ => break result.map_err(|status| self.with_channel_reason(status)),
            }
        }
        .map_err(|status| {
            if status.code() == tonic::Code::FailedPrecondition {
                Status::failed_precondition(format!(
                    "the helper on {} refused this window's helper protocol {PROTOCOL_MAJOR}.{PROTOCOL_MINOR} \
                         (Eitri {}): {}",
                    self.host(),
                    env!("CARGO_PKG_VERSION"),
                    status.message()
                ))
            } else {
                status
            }
        })?;
        check_hello(&response, self.host())?;
        self.inner.launcher.helper_hello(self.daemon(), &response.capabilities);
        *self.inner.hello.lock().expect("helper hello") = Some(response.clone());
        Ok(response)
    }

    /// Starts a sidecar for `start`, returning its id. The helper deduplicates starts by command id,
    /// so a start whose answer was lost (the channel broke, or no answer came in time) is asked again
    /// with the same id, a bounded number of times: the helper then answers with the sidecar it started
    /// or clears a half-started one, and none is left that no tab knows of.
    ///
    /// A start that names an account and fails as unavailable is told apart: when the account's
    /// directory is missing on the server, the error says the account was not found there.
    ///
    /// Refused on a connection opened without a pinned host key: what starts carries the project.
    pub fn start_sidecar(&self, start: &SidecarStart) -> Result<String, Status> {
        self.require_pin()?;
        let root = start
            .root
            .to_str()
            .ok_or_else(|| Status::invalid_argument("a remote project root must be UTF-8"))?
            .to_string();
        let request = StartSidecarRequest {
            root,
            account: start.account.clone().unwrap_or_default(),
            env: Default::default(),
            command_id: start.command_id.clone(),
            sidecar_revision: start.sidecar_revision.clone(),
            sidecar_sha256: start.sidecar_sha256.clone(),
        };
        let mut attempt = 0;
        let started = loop {
            let mut service = self.service();
            let request = request.clone();
            let started = self
                .call_raw(
                    START_SIDECAR_TIMEOUT,
                    async move { service.start_sidecar(request).await },
                )
                .map(|response| response.sidecar_id);
            match &started {
                Err(status) if answer_lost(status) && attempt < START_RETRIES.len() => {
                    std::thread::sleep(START_RETRIES[attempt]);
                    attempt += 1;
                }
                _ => break started,
            }
        };
        let refused = started.as_ref().err().is_some_and(|status| {
            status.code() == tonic::Code::Unavailable && std::error::Error::source(&status).is_none()
        });
        if let (true, Some(account)) = (refused, start.account.as_deref()) {
            if account_present(self.inner.launcher.as_ref(), account) == Some(false) {
                return Err(Status::not_found(format!(
                    "account {account} was not found on {}",
                    self.host()
                )));
            }
        }
        started.map_err(|status| self.with_channel_reason(status))
    }

    pub fn stop_sidecar(&self, sidecar_id: &str, command_id: &str) -> Result<(), Status> {
        let mut service = self.service();
        let request = StopSidecarRequest {
            sidecar_id: sidecar_id.to_string(),
            command_id: command_id.to_string(),
        };
        self.call(
            START_SIDECAR_TIMEOUT,
            async move { service.stop_sidecar(request).await },
        )
        .map(|_| ())
    }

    /// Stops `sidecar_id`, a helper that no longer knows it counting as stopped (it ended, or the server
    /// restarted); a refusal or a failure comes back in words.
    pub fn stop_if_known(&self, sidecar_id: &str) -> Result<(), String> {
        if self.installation_gone() {
            return Ok(());
        }
        // A stop sender may be the last owner after every tab has closed.
        if let LinkCheck::Failed(error) = self.inner.launcher.check_link() {
            return Err(error.to_string());
        }
        self.verified_host_key()?;
        match self.stop_sidecar(sidecar_id, &uuid::Uuid::new_v4().to_string()) {
            Ok(()) => Ok(()),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(()),
            Err(status) => Err(status.message().to_string()),
        }
    }

    pub fn list(&self) -> Result<Vec<SidecarInfo>, Status> {
        let mut service = self.service();
        self.call(HELPER_RPC_TIMEOUT, async move {
            service.list_sidecars(ListSidecarsRequest {}).await
        })
        .map(|response| response.sidecars)
    }

    /// The permission requests the helper holds unanswered for one session, oldest first.
    pub fn pending(&self, sidecar_id: &str, session_id: &str) -> Result<Vec<PendingPermission>, Status> {
        let mut service = self.service();
        let request = PendingRequest {
            sidecar_id: sidecar_id.to_string(),
            session_id: session_id.to_string(),
        };
        self.call(HELPER_RPC_TIMEOUT, async move { service.pending(request).await })
            .map(|response| response.requests)
    }

    /// The SHA-256 fingerprint of the host key ssh verified on the connection under this client, read now
    /// (`ChannelLauncher::host_key`, the record every master start rewrites), when it is the key this
    /// connection was opened for: what a caller keys anything about the project by, so a key that rotated
    /// is never taken for the recorded one. A launcher with no master (a helper run directly) shows no
    /// key, and the pinned one is the answer. `Err` names both keys, or why none could be read; a
    /// connection opened without a pin has no key of its own. Blocking: it reads ssh's record.
    pub fn verified_host_key(&self) -> Result<String, String> {
        shown_host_key(
            self.inner.pinned.as_deref(),
            self.inner.launcher.host_key(),
            self.host(),
        )
    }

    /// The key the connection under this client shows now, when it is one other than the pinned key: ssh accepted
    /// another server's key on a master that started again. `Ok(None)` while it shows the pinned key, when none can
    /// be read, without a pin, or with no master; `Err` while a master start may be under way (its record can hold
    /// a key ssh then refuses). Blocking: it waits a moment for the master's lock and reads ssh's record.
    pub fn shown_other_host_key(&self) -> Result<Option<String>, String> {
        other_host_key(self.inner.pinned.as_deref(), self.inner.launcher.settled_host_key())
    }

    /// Runs trust discovery of `root` on the server. Refused on a connection opened without a pinned
    /// host key, like [`Self::start_sidecar`]: it carries the project root.
    pub fn trust_scan(&self, root: &Path, profile_key: &str) -> Result<TrustScanResponse, Status> {
        self.require_pin()?;
        let root = root
            .to_str()
            .ok_or_else(|| Status::invalid_argument("a remote project root must be UTF-8"))?
            .to_string();
        let mut service = self.service();
        let request = TrustScanRequest {
            root,
            profile_key: profile_key.to_string(),
        };
        self.call(TRUST_SCAN_TIMEOUT, async move { service.trust_scan(request).await })
    }
    /// The capabilities from the last checked Hello, obtaining it once if the caller has not asked yet.
    pub fn hello_capabilities(&self) -> Result<Vec<String>, Status> {
        let cached = self.inner.hello.lock().expect("helper hello").clone();
        Ok(match cached {
            Some(hello) => hello,
            None => self.hello()?,
        }
        .capabilities)
    }

    pub fn sync_ignores(&self, request: SyncIgnoresRequest) -> Result<SyncIgnoresResponse, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(HELPER_RPC_TIMEOUT, async move { service.sync_ignores(request).await })
    }

    pub fn git_config(&self, request: GitConfigRequest) -> Result<GitConfigResponse, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(HELPER_RPC_TIMEOUT, async move { service.git_config(request).await })
    }

    pub fn list_tree(&self, request: ListTreeRequest) -> Result<tonic::Streaming<ListTreeEntry>, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(HELPER_RPC_TIMEOUT, async move { service.list_tree(request).await })
    }

    pub fn read_file(&self, request: ReadFileRequest) -> Result<tonic::Streaming<ReadFileChunk>, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(HELPER_RPC_TIMEOUT, async move { service.read_file(request).await })
    }

    pub fn replace_file_if(&self, request: ReplaceFileIfRequest) -> Result<ReplaceFileIfResponse, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(
            HELPER_RPC_TIMEOUT,
            async move { service.replace_file_if(request).await },
        )
    }

    pub fn remove_file_if(&self, request: RemoveFileIfRequest) -> Result<RemoveFileIfResponse, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(HELPER_RPC_TIMEOUT, async move { service.remove_file_if(request).await })
    }

    pub fn wait_file_hash(&self, request: WaitFileHashRequest) -> Result<WaitFileHashResponse, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(HELPER_RPC_TIMEOUT, async move { service.wait_file_hash(request).await })
    }

    pub fn path_sizes(&self, request: PathSizesRequest) -> Result<PathSizesResponse, Status> {
        self.require_pin()?;
        let mut service = self.service();
        // A whole-project walk has five minutes on the helper, plus time to receive its answer.
        let limit = if request.whole_root {
            Duration::from_secs(300) + HELPER_RPC_TIMEOUT
        } else {
            HELPER_RPC_TIMEOUT
        };
        self.call(limit, async move { service.path_sizes(request).await })
    }

    pub fn project_activity(&self, request: ProjectActivityRequest) -> Result<ProjectActivityResponse, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(
            HELPER_RPC_TIMEOUT,
            async move { service.project_activity(request).await },
        )
    }

    pub fn watch_modes(&self, request: WatchModesRequest) -> Result<WatchModesResponse, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(HELPER_RPC_TIMEOUT, async move { service.watch_modes(request).await })
    }

    pub fn exec(
        &self,
        input: impl tonic::codegen::tokio_stream::Stream<Item = ExecInput> + Send + 'static,
    ) -> Result<tonic::Streaming<ExecOutput>, Status> {
        self.require_pin()?;
        let mut service = self.service();
        self.call(HELPER_RPC_TIMEOUT, async move { service.exec(input).await })
    }

    /// Opens one execution stream without retrying the request. The bounded queues apply backpressure
    /// independently in each direction, so a child can write while its stdin is still being supplied.
    pub fn exec_stream(&self, start: ExecStart) -> Result<HelperExec, Status> {
        self.require_pin()?;
        let (input, receive) = tokio::sync::mpsc::channel(8);
        input
            .blocking_send(ExecInput {
                input: Some(exec_input::Input::Start(start)),
            })
            .map_err(|_| Status::unavailable("execution input closed"))?;
        let stream = self.exec(tonic::codegen::tokio_stream::wrappers::ReceiverStream::new(receive))?;
        let (send, output) = tokio::sync::mpsc::channel(8);
        let task = self.inner.runtime.handle().spawn(async move {
            let mut stream = stream;
            loop {
                let item = stream.message().await;
                let done = !matches!(&item, Ok(Some(_)));
                if send.send(item).await.is_err() || done {
                    break;
                }
            }
        });
        Ok(HelperExec {
            input: Some(input),
            output,
            task: Some(task.abort_handle()),
            _client: Some(self.clone()),
        })
    }

    /// Collects a sync response on this connection's runtime with caller-owned memory bounds.
    /// A stream is dropped on overflow or timeout, so an unfinished response is never returned as complete.
    pub fn collect_sync_stream<T: Send + 'static>(
        &self,
        stream: tonic::Streaming<T>,
        entry_limit: usize,
        byte_limit: usize,
        item_bytes: fn(&T) -> usize,
    ) -> Result<Vec<T>, Status> {
        let timeout = eitri_server_proto::SYNC_STREAM_DEADLINE + HELPER_RPC_TIMEOUT;
        self.call(timeout, async move {
            collect_sync_items(stream, entry_limit, byte_limit, item_bytes)
                .await
                .map(tonic::Response::new)
        })
    }
}

/// A single helper execution. Dropping it cancels observation and closes input; it never reconnects or replays.
pub struct HelperExec {
    input: Option<tokio::sync::mpsc::Sender<ExecInput>>,
    output: tokio::sync::mpsc::Receiver<Result<Option<ExecOutput>, Status>>,
    task: Option<tokio::task::AbortHandle>,
    _client: Option<HelperClient>,
}

impl HelperExec {
    /// A stream fixture that acknowledges close by ending without an exit, without any network channel.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_link_loss_after_close() -> (Self, std::sync::mpsc::Receiver<()>) {
        let (input, mut receive) = tokio::sync::mpsc::channel::<ExecInput>(8);
        let (send, output) = tokio::sync::mpsc::channel(8);
        let (observed, closed) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let first = receive.blocking_recv().expect("close message");
            assert!(matches!(first.input, Some(exec_input::Input::Close(true))));
            let _ = observed.send(());
            let _ = send.blocking_send(Ok(None));
        });
        (
            Self {
                input: Some(input),
                output,
                task: None,
                _client: None,
            },
            closed,
        )
    }

    pub fn take_input(&mut self) -> Option<tokio::sync::mpsc::Sender<ExecInput>> {
        self.input.take()
    }
    /// Copies both output streams while a separate reader supplies stdin. Close is sent only on EOF.
    pub fn copy_stdio(
        &mut self,
        mut stdin: Box<dyn io::Read + Send>,
        stdout: &mut dyn io::Write,
        stderr: &mut dyn io::Write,
    ) -> Result<ExecExit, String> {
        let input = self.take_input().ok_or("execution input already taken")?;
        std::thread::spawn(move || {
            let mut bytes = vec![0; 64 * 1024];
            loop {
                let message = match stdin.read(&mut bytes) {
                    Ok(0) => exec_input::Input::Close(true),
                    Ok(n) => exec_input::Input::Stdin(bytes[..n].to_vec()),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                };
                let closed = matches!(message, exec_input::Input::Close(_));
                if input.blocking_send(ExecInput { input: Some(message) }).is_err() || closed {
                    return;
                }
            }
        });
        loop {
            match self
                .next_output()?
                .ok_or("execution ended without an exit status")?
                .output
            {
                Some(exec_output::Output::Stdout(bytes)) => {
                    stdout.write_all(&bytes).map_err(|e| e.to_string())?;
                    stdout.flush().map_err(|e| e.to_string())?;
                }
                Some(exec_output::Output::Stderr(bytes)) => {
                    stderr.write_all(&bytes).map_err(|e| e.to_string())?;
                    stderr.flush().map_err(|e| e.to_string())?;
                }
                Some(exec_output::Output::Exit(exit)) => return Ok(exit),
                None => return Err("execution output is empty".into()),
            }
        }
    }

    pub fn next_output(&mut self) -> Result<Option<ExecOutput>, String> {
        self.output
            .blocking_recv()
            .ok_or_else(|| "execution link closed".to_owned())?
            .map_err(|error| error.to_string())
    }
}

impl Drop for HelperExec {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn collect_sync_items<T, S>(
    mut stream: S,
    entry_limit: usize,
    byte_limit: usize,
    item_bytes: fn(&T) -> usize,
) -> Result<Vec<T>, Status>
where
    S: tonic::codegen::tokio_stream::Stream<Item = Result<T, Status>> + Unpin,
{
    use tonic::codegen::tokio_stream::StreamExt;
    let mut items = Vec::new();
    let mut bytes = 0usize;
    while let Some(item) = stream.next().await {
        let item = item?;
        bytes = bytes
            .checked_add(item_bytes(&item))
            .filter(|bytes| *bytes <= byte_limit)
            .ok_or_else(|| Status::resource_exhausted("sync stream exceeds the caller's byte bound"))?;
        if items.len() >= entry_limit {
            return Err(Status::resource_exhausted(
                "sync stream exceeds the caller's entry bound",
            ));
        }
        items.push(item);
    }
    Ok(items)
}

/// The helper connections a window holds, one per recorded host key. Asked where a moved remote chat is
/// taken up, which runs on the GTK thread: it gives a connection already open and opens none.
pub trait HelperConnections {
    fn open_for(&self, host: &str, host_key_sha256: &str) -> Option<HelperClient>;
}

/// A window with no helper connection: every moved remote chat is refused.
pub struct NoHelperConnections;

impl HelperConnections for NoHelperConnections {
    fn open_for(&self, _host: &str, _host_key_sha256: &str) -> Option<HelperClient> {
        None
    }
}

/// [`HelperClient::verified_host_key`] for a connection pinned to `pinned` (if it is) whose master shows
/// `shown` (`None`: no master).
fn shown_host_key(pinned: Option<&str>, shown: Option<io::Result<String>>, host: &str) -> Result<String, String> {
    let Some(pinned) = pinned else {
        return Err(format!(
            "the connection to {host} was opened without a project's host key; it serves no project"
        ));
    };
    match shown {
        None => Ok(pinned.to_string()),
        Some(Ok(key)) if key == pinned => Ok(key),
        Some(Ok(key)) => Err(format!(
            "the connection to {host} shows host key {key}, not the {pinned} this project was recorded with"
        )),
        Some(Err(error)) => Err(format!("the host key of {host} could not be read: {error}")),
    }
}

/// [`HelperClient::shown_other_host_key`] for a connection pinned to `pinned` whose master shows `shown`.
/// `Err` when it could not be looked at (a master start under way); an unreadable record says nothing about the
/// server and is `Ok(None)`.
fn other_host_key(pinned: Option<&str>, shown: Option<io::Result<String>>) -> Result<Option<String>, String> {
    match (pinned, shown) {
        (Some(pinned), Some(Ok(key))) if key != pinned => Ok(Some(key)),
        (Some(_), Some(Err(error))) if error.kind() == io::ErrorKind::WouldBlock => Err(error.to_string()),
        _ => Ok(None),
    }
}

/// Asks a helper which permission requests wait in a session, from a task on the connection's runtime.
#[derive(Clone)]
pub(crate) struct PendingAsker {
    channel: Channel,
    sidecar_id: String,
}

impl PendingAsker {
    /// The requests the helper holds unanswered in `session_id`, oldest first; bounded like the quick
    /// calls.
    pub(crate) async fn ask(&self, session_id: &str) -> Result<Vec<PendingPermission>, Status> {
        let mut service = HelperService::new(self.channel.clone());
        let request = PendingRequest {
            sidecar_id: self.sidecar_id.clone(),
            session_id: session_id.to_string(),
        };
        match tokio::time::timeout(HELPER_RPC_TIMEOUT, service.pending(request)).await {
            Ok(answer) => answer.map(|response| response.into_inner().requests),
            Err(_) => Err(Status::deadline_exceeded(
                "the helper did not say what is waiting in time",
            )),
        }
    }
}

/// Why a remote tab did not start, in words for the person, with the facts of the sidecar when one
/// answered (for the notes a failed start carries).
#[derive(Debug)]
pub struct RemoteStartFailure {
    pub message: String,
    pub info: Option<crate::ProviderInfo>,
    /// The sidecar started for the tab when its stop did not go through after the session failed (the link
    /// broke during the create): the caller keeps its stop for when the server can be reached again.
    pub unstopped: Option<crate::HelperHandoff>,
}

/// Starts a remote Claude tab: checks the root on this machine, has the helper start a sidecar
/// (`ClaudeSidecarProvider::connect_through_helper`) and creates the session in the root as recorded.
/// A sidecar the helper started is stopped again when the session cannot be created; when that stop
/// does not go through either, the failure names the sidecar (`unstopped`) for its stop to be kept.
/// Blocking: a worker's call.
pub fn open_remote_session(
    client: HelperClient,
    host_key_sha256: &str,
    start: &SidecarStart,
    project: crate::setting_sources::ProjectTrust,
    client_tools: Option<crate::ClientToolSet>,
) -> Result<crate::AgentConversation, RemoteStartFailure> {
    crate::AgentConversation::check_remote_root(&start.root).map_err(|error| RemoteStartFailure {
        message: error.to_string(),
        info: None,
        unstopped: None,
    })?;
    let provider =
        crate::ClaudeSidecarProvider::connect_through_helper(client, host_key_sha256, start).map_err(|error| {
            RemoteStartFailure {
                message: error.to_string(),
                info: None,
                unstopped: None,
            }
        })?;
    let provider = Arc::new(provider);
    let info = crate::AgentProvider::info(provider.as_ref());
    crate::AgentConversation::create_remote_with_tools(provider.clone(), &start.root, project, client_tools).map_err(
        |error| {
            let unstopped = provider.stop_remote_sidecar().err().and_then(|stop| {
                eprintln!(
                    "agent: the sidecar on {} of a session that could not be created was not stopped: {stop}",
                    info.host.as_deref().unwrap_or("its server")
                );
                provider.remote_sidecar()
            });
            RemoteStartFailure {
                message: format!("failed to create a Claude session: {error}"),
                info: Some(info),
                unstopped,
            }
        },
    )
}

/// The waits before each repeated `StartSidecar` whose answer was lost.
const START_RETRIES: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(2)];

/// Whether a failed call may have reached the helper with its answer lost on the way back: no answer
/// in time, or a channel that broke (a status tonic made of a transport error, not one the helper sent).
fn answer_lost(status: &Status) -> bool {
    match status.code() {
        tonic::Code::DeadlineExceeded => true,
        tonic::Code::Unavailable | tonic::Code::Unknown => std::error::Error::source(&status).is_some(),
        _ => false,
    }
}

/// The host key a connection's project is pinned to, and the reason the last channel was refused.
#[derive(Clone)]
struct KeyPin {
    pinned: Option<String>,
    refused: Arc<Mutex<Option<String>>>,
    host: String,
}

impl KeyPin {
    /// Compares the key the connection shows now with the pinned one: `Err` with the reason when it is
    /// another key or cannot be read. Nothing to compare without a pin or for a launcher with no
    /// master (a local fixture). Blocking: it reads ssh's record of the master's key.
    fn compare(&self, launcher: &dyn ChannelLauncher) -> Result<(), String> {
        let Some(pinned) = self.pinned.as_deref() else {
            return Ok(());
        };
        let Some(shown) = launcher.host_key() else {
            return Ok(());
        };
        let verdict = match shown {
            Ok(key) if key == pinned => Ok(()),
            Ok(key) => Err(format!(
                "the connection to {} shows host key {key}, not the {pinned} this project was recorded with",
                self.host
            )),
            Err(error) => Err(format!("the host key of {} could not be read: {error}", self.host)),
        };
        *self.refused.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = verdict.clone().err();
        verdict
    }
}

/// Checks the link under a helper connection's channels.
#[derive(Clone)]
pub struct LinkChecker {
    state: Arc<RouteState>,
    launcher: Arc<dyn ChannelLauncher>,
    pin: KeyPin,
}

impl LinkChecker {
    pub(crate) fn installation_gone(&self) -> bool {
        self.state.gone.load(Ordering::Acquire)
    }

    pub fn host(&self) -> &str {
        &self.pin.host
    }

    /// Checks the connection channels go through and restarts it when it is gone. A connection that
    /// answers (restarted here or by anyone else sharing the master) is compared with the pinned host
    /// key; another key fails the check. The connector compares again before every channel, so no
    /// channel starts on a connection showing another key, whoever restarted it.
    ///
    /// The check blocks (ssh, a lock another restart may hold), so it runs on a thread of its own,
    /// never on the runtime: a caller that goes away while it runs (a tab closed) does not wait for
    /// it, and the thread finishes on its own.
    pub async fn check(&self) -> LinkCheck {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let checker = self.clone();
        let spawned = std::thread::Builder::new()
            .name("eitri-link-check".into())
            .spawn(move || {
                let _ = tx.send(checker.check_blocking());
            });
        if spawned.is_err() {
            return LinkCheck::NoMaster;
        }
        rx.await.unwrap_or(LinkCheck::NoMaster)
    }

    fn check_blocking(&self) -> LinkCheck {
        let checked = self.launcher.check_link();
        if !matches!(checked, LinkCheck::Alive | LinkCheck::Restarted) {
            return checked;
        }
        match self.pin.compare(self.launcher.as_ref()) {
            Ok(()) => checked,
            Err(why) => LinkCheck::Failed(crate::ssh::ConnectError::from(why)),
        }
    }
}

/// Refuses a Hello answered in another protocol major, naming both sides' versions.
pub fn check_hello(response: &HelloResponse, host: &str) -> Result<(), Status> {
    if response.protocol_major == PROTOCOL_MAJOR {
        return Ok(());
    }
    Err(Status::failed_precondition(format!(
        "the helper on {host} is eitri-server {} speaking helper protocol {}.{}; this window is Eitri {} \
         speaking {PROTOCOL_MAJOR}.{PROTOCOL_MINOR}",
        if response.helper_version.is_empty() {
            "of an unknown version"
        } else {
            response.helper_version.as_str()
        },
        response.protocol_major,
        response.protocol_minor,
        env!("CARGO_PKG_VERSION"),
    )))
}

/// The program that tells whether an account's directory exists on the server, under the convention
/// the sidecar resolves an account by (`$HOME/.claude-<name>`). Exit 0: it does, 1: it does not.
const ACCOUNT_DIRECTORY: &str = r#"test -d "$HOME/.claude-$1""#;

/// Whether `account`'s directory exists on the server; `None` when that cannot be told (an account
/// name the step refuses, a channel that would not start or answered neither way).
fn account_present(launcher: &dyn ChannelLauncher, account: &str) -> Option<bool> {
    let mut child = launcher.spawn_step(ACCOUNT_DIRECTORY, &[account]).ok()?;
    drop(child.stdin.take());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

fn stderr_line(stderr: &Mutex<Option<String>>) -> Option<String> {
    stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

/// Each channel keeps its own reader: a late drain from a replaced channel cannot overwrite the
/// current channel's diagnostic.
#[derive(Default)]
struct ChannelStderr {
    state: Mutex<ChannelStderrState>,
}

#[derive(Default)]
struct ChannelStderrState {
    current: Option<Arc<StderrCapture>>,
    // A reconnect may start before the failed call reports its error. Keep that channel's words
    // until its replacement answers, rather than erasing them as soon as a new child starts.
    previous: Option<String>,
}

impl ChannelStderr {
    fn replace(&self, capture: Arc<StderrCapture>) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(line) = state.current.as_ref().and_then(|reader| reader.last_line()) {
            state.previous = Some(line);
        }
        state.current = Some(capture);
    }

    fn attached(&self, capture: &Arc<StderrCapture>) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, capture))
        {
            state.previous = None;
        }
    }
}

/// Reads bytes already in the pipe on the reporting thread too. A failed channel can be reported
/// before its background reader is scheduled, even though its diagnostic is already buffered.
fn settled_stderr(stderr: &ChannelStderr) -> Option<String> {
    let (capture, previous) = {
        let state = stderr.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.current.clone(), state.previous.clone())
    };
    capture.and_then(|capture| capture.last_line()).or(previous)
}

struct StderrCapture {
    reader: Mutex<StderrReader<std::fs::File>>,
}

impl StderrCapture {
    fn last_line(&self) -> Option<String> {
        let mut reader = self.reader.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = reader.read_available();
        reader.last.clone()
    }

    fn new(errors: std::process::ChildStderr) -> io::Result<Self> {
        let errors = OwnedFd::from(errors);
        nonblocking(&errors)?;
        Ok(Self {
            reader: Mutex::new(StderrReader::new(std::fs::File::from(errors))),
        })
    }

    fn drain(&self) {
        loop {
            let fd = {
                let mut reader = self.reader.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                match reader.read_available() {
                    Ok(true) | Err(_) => return,
                    Ok(false) => reader.input.as_raw_fd(),
                }
            };
            let mut waiting = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: the capture owns this descriptor for the whole wait. No reader lock is held,
            // so reporting a failure can consume buffered stderr without waiting for this thread.
            if unsafe { libc::poll(&mut waiting, 1, -1) } < 0
                && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
            {
                return;
            }
        }
    }
}

struct StderrReader<R> {
    input: R,
    line: Vec<u8>,
    last: Option<String>,
}

impl<R: io::Read> StderrReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            line: Vec::new(),
            last: None,
        }
    }

    /// Returns true at EOF, false when the nonblocking pipe has no more bytes yet. Partial lines
    /// remain here across reads, including when a different thread finishes the line.
    fn read_available(&mut self) -> io::Result<bool> {
        let mut bytes = [0; 8192];
        // A continuously writing child must release the reader lock for an error reporter.
        for _ in 0..8 {
            match self.input.read(&mut bytes) {
                Ok(0) => {
                    self.finish_line();
                    return Ok(true);
                }
                Ok(n) => {
                    for part in bytes[..n].split_inclusive(|byte| *byte == b'\n') {
                        self.line.extend_from_slice(part);
                        if part.ends_with(b"\n") {
                            self.finish_line();
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    fn finish_line(&mut self) {
        let text = String::from_utf8_lossy(&self.line).trim().to_string();
        if !text.is_empty() {
            self.last = Some(text);
        }
        self.line.clear();
    }
}

#[cfg(test)]
fn drain_stderr(errors: impl io::Read, last: &Mutex<Option<String>>) {
    let mut reader = StderrReader::new(errors);
    let _ = reader.read_available();
    *last.lock().unwrap() = reader.last;
}

/// The argument tail of `<helper> connect`, naming a saved sidecar's owning installation when known.
fn connect_tail(daemon: &str) -> [OsString; 3] {
    let args = if daemon.is_empty() {
        Vec::new()
    } else {
        vec![std::ffi::OsStr::new("--daemon"), std::ffi::OsStr::new(daemon)]
    };
    [
        "connect".into(),
        "--args-hex".into(),
        crate::ssh::encode_args(&args).into(),
    ]
}

/// What the connector needs to start one channel.
struct Opening {
    daemon: String,
    state: Arc<RouteState>,
    launcher: Arc<dyn ChannelLauncher>,
    stderr: Arc<ChannelStderr>,
    pin: KeyPin,
}

async fn open_channel(opening: Opening) -> io::Result<hyper_util::rt::TokioIo<ChildPipe>> {
    let Opening {
        launcher,
        stderr,
        pin,
        daemon,
        state,
    } = opening;
    let pipe_state = state.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    // A runtime joins its blocking pool at shutdown. Master checks and START must be free to
    // finish after a tab closes, without making that tab wait for the connection worker.
    std::thread::Builder::new()
        .name("eitri-channel-open".into())
        .spawn(move || {
            let opened = (|| {
                if let LinkCheck::Failed(error) = launcher.check_link() {
                    return Err(io::Error::other(error.to_string()));
                }
                if tx.is_closed() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                pin.compare(launcher.as_ref())
                    .map_err(|why| io::Error::new(io::ErrorKind::PermissionDenied, why))?;
                if !state.started.load(Ordering::Acquire) {
                    if let Err(error) = launcher.ensure_helper(&daemon) {
                        if error.kind() == io::ErrorKind::NotFound && error.to_string() == MISSING_INSTALLATION {
                            state.gone.store(true, Ordering::Release);
                        }
                        return Err(error);
                    }
                    state.started.store(true, Ordering::Release);
                }
                // The guard excludes master replacement from the final comparison through attachment.
                pin.compare(launcher.as_ref())
                    .map_err(|why| io::Error::new(io::ErrorKind::PermissionDenied, why))?;
                let guard = pin
                    .pinned
                    .as_deref()
                    .map(|key| launcher.hold_host_key(key))
                    .transpose()?;
                if tx.is_closed() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                launcher.spawn(&connect_tail(&daemon)).map(|child| OpeningChild {
                    child: Some(child),
                    key_guard: guard,
                })
            })();
            let _ = tx.send(opened);
            drop(launcher);
        })?;
    let mut opened = rx.await.map_err(io::Error::other)??;
    let mut pipe = ChildPipe::new(opened.child.take().unwrap(), stderr)?;
    pipe.key_guard = opened.key_guard.take();
    pipe.route_state = Some(pipe_state);
    Ok(hyper_util::rt::TokioIo::new(pipe))
}

/// Owns the child across the worker handoff, including cancellation before the receiver takes it.
struct OpeningChild {
    child: Option<Child>,
    key_guard: Option<Box<dyn Send>>,
}

impl Drop for OpeningChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A channel's child process seen as one byte stream: its stdout to read, its stdin to write. The
/// child is killed and reaped when the stream goes.
struct ChildPipe {
    route_state: Option<Arc<RouteState>>,
    key_guard: Option<Box<dyn Send>>,
    child: Option<Child>,
    stderr: Arc<ChannelStderr>,
    capture: Option<Arc<StderrCapture>>,
    output: AsyncFd<OwnedFd>,
    input: Option<AsyncFd<OwnedFd>>,
}

impl ChildPipe {
    fn new(mut child: Child, stderr: Arc<ChannelStderr>) -> io::Result<Self> {
        let taken = (child.stdin.take(), child.stdout.take());
        let (Some(input), Some(output)) = taken else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("the channel's stdio is not piped"));
        };
        let capture = if let Some(errors) = child.stderr.take() {
            let capture = match StderrCapture::new(errors) {
                Ok(capture) => Arc::new(capture),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            stderr.replace(capture.clone());
            let drain = capture.clone();
            // Drain continuously so a chatty channel never blocks on a full pipe.
            if let Err(error) = std::thread::Builder::new()
                .name("eitri-helper-stderr".into())
                .spawn(move || drain.drain())
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            Some(capture)
        } else {
            None
        };
        let opened = (|| {
            let input = OwnedFd::from(input);
            let output = OwnedFd::from(output);
            nonblocking(&input)?;
            nonblocking(&output)?;
            Ok::<_, io::Error>((AsyncFd::new(input)?, AsyncFd::new(output)?))
        })();
        match opened {
            Ok((input, output)) => Ok(Self {
                route_state: None,
                key_guard: None,
                child: Some(child),
                stderr,
                capture,
                output,
                input: Some(input),
            }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }
}

fn nonblocking(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: fd is an open pipe end owned by the caller; fcntl reads no memory.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl tokio::io::AsyncRead for ChildPipe {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let mut guard = match this.output.poll_read_ready(cx) {
                Poll::Ready(guard) => guard?,
                Poll::Pending => return Poll::Pending,
            };
            let unfilled = buf.initialize_unfilled();
            let read = guard.try_io(|fd| {
                // SAFETY: the buffer is initialized and `unfilled.len()` bytes long.
                let n = unsafe { libc::read(fd.as_raw_fd(), unfilled.as_mut_ptr().cast(), unfilled.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match read {
                Ok(Ok(n)) => {
                    this.key_guard.take();
                    if n == 0 {
                        if let Some(state) = &this.route_state {
                            state.started.store(false, Ordering::Release);
                        }
                    }
                    if n > 0 {
                        if let Some(capture) = this.capture.take() {
                            this.stderr.attached(&capture);
                        }
                    }
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl tokio::io::AsyncWrite for ChildPipe {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let Some(input) = this.input.as_ref() else {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
        };
        loop {
            let mut guard = match input.poll_write_ready(cx) {
                Poll::Ready(guard) => guard?,
                Poll::Pending => return Poll::Pending,
            };
            let written = guard.try_io(|fd| {
                // SAFETY: `bytes` is a live slice of `bytes.len()` bytes.
                let n = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match written {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Closing the write end is the end of input the channel's child sees.
        self.get_mut().input = None;
        Poll::Ready(Ok(()))
    }
}

impl Drop for ChildPipe {
    fn drop(&mut self) {
        if let Some(state) = &self.route_state {
            state.started.store(false, Ordering::Release);
        }
        let Some(mut child) = self.child.take() else { return };
        let _ = child.kill();
        if !matches!(child.try_wait(), Ok(Some(_))) {
            // Reaped off the runtime: a killed child exits at once, but the wait is still a wait.
            let _ = std::thread::Builder::new()
                .name("eitri-helper-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_client_sync_streams_enforce_bounds_and_preserve_errors() {
        use tonic::codegen::tokio_stream::iter;
        let values = || iter([Ok(vec![1u8, 2]), Ok(vec![3u8])]);
        assert_eq!(
            collect_sync_items(values(), 2, 3, Vec::len).await.unwrap(),
            vec![vec![1, 2], vec![3]]
        );
        let error = collect_sync_items(values(), 1, 3, Vec::len).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        assert!(error.message().contains("entry bound"));
        let error = collect_sync_items(values(), 2, 2, Vec::len).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        assert!(error.message().contains("byte bound"));
        let items = iter([Ok(vec![1u8]), Err(Status::data_loss("incomplete file"))]);
        let error = collect_sync_items(items, 2, 3, Vec::len).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert_eq!(error.message(), "incomplete file");
        assert!(
            collect_sync_items(iter(Vec::<Result<Vec<u8>, Status>>::new()), 0, 0, Vec::len)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn opening_a_replacement_keeps_the_failure_until_that_replacement_answers() {
        let capture = |script| {
            let mut child = std::process::Command::new("sh")
                .args(["-c", script])
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let capture = Arc::new(StderrCapture::new(child.stderr.take().unwrap()).unwrap());
            child.wait().unwrap();
            capture
        };
        let failed = capture("echo 'example-host: Permission denied (publickey).' >&2");
        let replacement = capture(":");
        let stderr = ChannelStderr::default();
        stderr.replace(failed.clone());
        stderr.replace(replacement.clone());
        let reason = "example-host: Permission denied (publickey).";
        assert_eq!(settled_stderr(&stderr).as_deref(), Some(reason));
        // A late response or stderr read from the old channel cannot change the replacement.
        stderr.attached(&failed);
        assert_eq!(settled_stderr(&stderr).as_deref(), Some(reason));
        stderr.attached(&replacement);
        failed.drain();
        assert_eq!(settled_stderr(&stderr), None);
    }

    #[test]
    fn stderr_is_read_before_the_background_drain_runs() {
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                "echo 'example-host: Permission denied (publickey).' >&2; exit 255",
            ])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let capture = Arc::new(StderrCapture::new(child.stderr.take().unwrap()).unwrap());
        assert_eq!(child.wait().unwrap().code(), Some(255));
        assert!(capture.reader.lock().unwrap().last.is_none());
        let stderr = ChannelStderr::default();
        stderr.replace(capture);
        assert_eq!(
            settled_stderr(&stderr).as_deref(),
            Some("example-host: Permission denied (publickey).")
        );
    }

    #[test]
    fn nonblocking_stderr_keeps_partial_lines_until_newline_or_eof() {
        struct Chunks(std::collections::VecDeque<Option<&'static [u8]>>);
        impl io::Read for Chunks {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                match self.0.pop_front() {
                    Some(Some(chunk)) => {
                        bytes[..chunk.len()].copy_from_slice(chunk);
                        Ok(chunk.len())
                    }
                    Some(None) => Err(io::ErrorKind::WouldBlock.into()),
                    None => Ok(0),
                }
            }
        }
        let mut reader = StderrReader::new(Chunks(std::collections::VecDeque::from([
            Some(&b"banner\nPerm"[..]),
            None,
            Some(&b"ission denied (publickey).\n\nlast without newline"[..]),
            None,
        ])));
        assert!(!reader.read_available().unwrap());
        assert_eq!(reader.last.as_deref(), Some("banner"));
        assert!(!reader.read_available().unwrap());
        assert_eq!(reader.last.as_deref(), Some("Permission denied (publickey)."));
        assert!(reader.read_available().unwrap());
        assert_eq!(reader.last.as_deref(), Some("last without newline"));
    }

    #[test]
    fn an_exited_channels_stderr_reaches_the_call_without_a_helper_daemon() {
        struct Refusing;
        impl ChannelLauncher for Refusing {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                std::process::Command::new("sh")
                    .args([
                        "-c",
                        "echo 'example-host: Permission denied (publickey).' >&2; exit 255",
                    ])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
            }
        }
        let failed = match HelperClient::connect(Arc::new(Refusing), "example-host", None) {
            Err(error) => error.to_string(),
            Ok(client) => client.hello().unwrap_err().message().to_string(),
        };
        assert!(failed.contains("Permission denied (publickey)."), "{failed}");
    }

    fn lazy_test_client(launcher: Arc<dyn ChannelLauncher>) -> HelperClient {
        let runtime = Arc::new(RuntimeThread::spawn());
        let channel = {
            let _entered = runtime.handle().enter();
            Endpoint::from_static("http://localhost:1").connect_lazy()
        };
        HelperClient {
            inner: Arc::new(Inner {
                hello: Mutex::new(None),
                host: "example-host".into(),
                daemon: "current-installation".into(),
                routes: Default::default(),
                installations: Default::default(),
                state: Default::default(),
                runtime,
                channel,
                launcher,
                stderr: Default::default(),
                refused: Default::default(),
                pinned: Some("SHA256:example".into()),
            }),
        }
    }

    fn fake_exec(
        body: impl FnOnce(
            tokio::sync::mpsc::Receiver<ExecInput>,
            tokio::sync::mpsc::Sender<Result<Option<ExecOutput>, Status>>,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ) -> HelperExec {
        struct Never;
        impl ChannelLauncher for Never {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                panic!("the execution fake must not open a channel")
            }
        }
        let client = lazy_test_client(Arc::new(Never));
        let (input, receive) = tokio::sync::mpsc::channel(8);
        let (send, output) = tokio::sync::mpsc::channel(8);
        let task = client.inner.runtime.handle().spawn(body(receive, send));
        HelperExec {
            input: Some(input),
            output,
            task: Some(task.abort_handle()),
            _client: Some(client),
        }
    }

    #[test]
    fn helper_exec_stream_closes_only_after_eof_and_streams_while_stdin_is_held() {
        struct HeldInput {
            release: std::sync::mpsc::Receiver<()>,
            sent: bool,
        }
        impl io::Read for HeldInput {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.sent {
                    return Ok(0);
                }
                self.release
                    .recv_timeout(Duration::from_secs(60))
                    .expect("output must arrive while input is held");
                out[..5].copy_from_slice(b"patch");
                self.sent = true;
                Ok(5)
            }
        }
        struct ReleasingOutput {
            release: Option<std::sync::mpsc::Sender<()>>,
            bytes: Vec<u8>,
        }
        impl io::Write for ReleasingOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                if let Some(release) = self.release.take() {
                    release.send(()).unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut stream = fake_exec(|mut input, output| {
            Box::pin(async move {
                output
                    .send(Ok(Some(ExecOutput {
                        output: Some(exec_output::Output::Stdout(b"ready".to_vec())),
                    })))
                    .await
                    .unwrap();
                assert_eq!(
                    input.recv().await.unwrap().input,
                    Some(exec_input::Input::Stdin(b"patch".to_vec()))
                );
                assert_eq!(input.recv().await.unwrap().input, Some(exec_input::Input::Close(true)));
                assert!(input.recv().await.is_none(), "EOF must close the sending half");
                output
                    .send(Ok(Some(ExecOutput {
                        output: Some(exec_output::Output::Stderr(b"diagnostic".to_vec())),
                    })))
                    .await
                    .unwrap();
                output
                    .send(Ok(Some(ExecOutput {
                        output: Some(exec_output::Output::Exit(ExecExit {
                            code: 1,
                            signal: 0,
                            refusal: String::new(),
                        })),
                    })))
                    .await
                    .unwrap();
            })
        });
        let (release, receive) = std::sync::mpsc::channel();
        let mut stdout = ReleasingOutput {
            release: Some(release),
            bytes: vec![],
        };
        let mut stderr = vec![];
        let exit = stream
            .copy_stdio(
                Box::new(HeldInput {
                    release: receive,
                    sent: false,
                }),
                &mut stdout,
                &mut stderr,
            )
            .unwrap();
        assert_eq!(exit.code, 1);
        assert_eq!(stdout.bytes, b"ready");
        assert_eq!(stderr, b"diagnostic");
    }

    #[test]
    fn helper_exec_stream_input_failure_never_sends_close() {
        struct Broken;
        impl io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken input"))
            }
        }
        let mut stream = fake_exec(|mut input, output| {
            Box::pin(async move {
                assert!(input.recv().await.is_none(), "a failed read is not close");
                output
                    .send(Err(Status::cancelled("input dropped before close")))
                    .await
                    .unwrap();
            })
        });
        assert!(stream
            .copy_stdio(Box::new(Broken), &mut vec![], &mut vec![])
            .unwrap_err()
            .contains("input dropped before close"));
    }

    #[test]
    fn helper_exec_stream_link_loss_after_close_has_no_exit_and_no_retry() {
        let mut stream = fake_exec(|mut input, output| {
            Box::pin(async move {
                assert_eq!(input.recv().await.unwrap().input, Some(exec_input::Input::Close(true)));
                output
                    .send(Err(Status::unavailable("link lost after close")))
                    .await
                    .unwrap();
            })
        });
        assert!(stream
            .copy_stdio(Box::new(io::empty()), &mut vec![], &mut vec![])
            .unwrap_err()
            .contains("link lost after close"));
    }

    #[test]
    fn terminal_channels_require_the_connections_pinned_key_before_any_io() {
        struct Never;
        impl ChannelLauncher for Never {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                panic!("a terminal capability check must not open a channel")
            }
        }
        let mut client = lazy_test_client(Arc::new(Never));
        let (_, handle) = client.terminal_channel().unwrap();
        assert_eq!(handle.runtime_flavor(), tokio::runtime::RuntimeFlavor::CurrentThread);
        assert!(client.terminal_connection("../other").is_err());
        Arc::get_mut(&mut client.inner).unwrap().pinned = None;
        assert_eq!(
            client.terminal_channel().unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        assert!(client.terminal_connection("").is_err());
    }

    /// A terminal connection through the launcher it is given opens its channels there and never through the
    /// launcher of the connection it came from; it keeps that connection's pin, compared before its first channel.
    /// The pin and the installation's name are checked before that launcher is even asked for.
    #[test]
    fn a_terminal_connection_goes_through_the_launcher_it_is_given() {
        use std::sync::atomic::AtomicUsize;

        #[derive(Default)]
        struct Counting {
            spawns: AtomicUsize,
            key: Mutex<String>,
        }
        impl ChannelLauncher for Counting {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                self.spawns.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::other("no helper here"))
            }
            fn host_key(&self) -> Option<io::Result<String>> {
                Some(Ok(self.key.lock().unwrap().clone()))
            }
        }
        let agents = Arc::new(Counting::default());
        *agents.key.lock().unwrap() = "SHA256:example".into();
        let mut client = lazy_test_client(agents.clone());
        let terminals = Arc::new(Counting::default());
        let asked = AtomicUsize::new(0);
        let given = || {
            asked.fetch_add(1, Ordering::SeqCst);
            Ok(terminals.clone() as Arc<dyn ChannelLauncher>)
        };

        *terminals.key.lock().unwrap() = "SHA256:other".into();
        let refused = client.terminal_connection_through("", given).err().unwrap().to_string();
        assert!(
            refused.contains("SHA256:other") && refused.contains("SHA256:example"),
            "{refused}"
        );
        assert_eq!(terminals.spawns.load(Ordering::SeqCst), 0);

        *terminals.key.lock().unwrap() = "SHA256:example".into();
        assert!(client.terminal_connection_through("", given).is_err());
        assert_eq!(terminals.spawns.load(Ordering::SeqCst), 1);
        assert_eq!(agents.spawns.load(Ordering::SeqCst), 0);
        assert_eq!(asked.load(Ordering::SeqCst), 2);

        assert!(client.terminal_connection_through("../other", given).is_err());
        Arc::get_mut(&mut client.inner).unwrap().pinned = None;
        assert!(client.terminal_connection_through("", given).is_err());
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        assert_eq!(terminals.spawns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn timed_out_calls_release_their_channels_on_the_shared_route() {
        use std::sync::atomic::AtomicUsize;
        use tonic::codegen::http;

        struct Never;
        impl ChannelLauncher for Never {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                panic!("the in-memory helper must not spawn a process")
            }
        }
        struct InFlight(Arc<AtomicUsize>);
        impl Drop for InFlight {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        #[derive(Clone)]
        struct Stalled {
            active: Arc<AtomicUsize>,
            calls: Arc<AtomicUsize>,
        }
        impl tonic::server::NamedService for Stalled {
            const NAME: &'static str = "eitri.server.v1.Helper";
        }
        impl tower::Service<http::Request<tonic::body::Body>> for Stalled {
            type Response = http::Response<tonic::body::Body>;
            type Error = std::convert::Infallible;
            type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;

            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _: http::Request<tonic::body::Body>) -> Self::Future {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.active.fetch_add(1, Ordering::SeqCst);
                let held = InFlight(self.active.clone());
                Box::pin(async move {
                    let _held = held;
                    std::future::pending().await
                })
            }
        }
        let client = lazy_test_client(Arc::new(Never));
        let mut route = client.for_daemon("old-installation").unwrap();
        client.inner.routes.lock().unwrap().remove("old-installation");
        let handlers = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let channels = Arc::new(AtomicUsize::new(0));
        let connected = channels.clone();
        let handler = Stalled {
            active: handlers.clone(),
            calls: calls.clone(),
        };
        let channel = client
            .inner
            .runtime
            .block_on(
                async move {
                    let (wire, peer) = tokio::io::duplex(64 * 1024);
                    tokio::spawn(async move {
                        tonic::transport::Server::builder()
                            .add_service(handler)
                            .serve_with_incoming(tonic::codegen::tokio_stream::iter([Ok::<_, io::Error>(peer)]))
                            .await
                            .unwrap();
                    });
                    let wire = Arc::new(Mutex::new(Some(wire)));
                    Endpoint::from_static("http://localhost")
                        .connect_with_connector(tower::service_fn(move |_| {
                            connected.fetch_add(1, Ordering::SeqCst);
                            let wire = wire.lock().unwrap().take();
                            async move {
                                wire.map(hyper_util::rt::TokioIo::new)
                                    .ok_or_else(|| io::Error::other("only one channel is allowed"))
                            }
                        }))
                        .await
                        .unwrap()
                },
                Duration::from_secs(60),
            )
            .unwrap();
        Arc::get_mut(&mut route.inner).unwrap().channel = channel;
        client
            .inner
            .routes
            .lock()
            .unwrap()
            .insert("old-installation".into(), Arc::downgrade(&route.inner));
        let weak = Arc::downgrade(&route.inner);
        let active = Arc::new(AtomicUsize::new(0));
        let limit = Duration::from_millis(100);
        for _ in 0..4 {
            let retry = client.for_daemon("old-installation").unwrap();
            assert!(Arc::ptr_eq(&route.inner, &retry.inner));
            let mut service = retry.service();
            let active = active.clone();
            let result = retry.call_raw(limit, async move {
                active.fetch_add(1, Ordering::SeqCst);
                let _held = InFlight(active);
                service.list_sidecars(ListSidecarsRequest {}).await
            });
            assert_eq!(result.unwrap_err().code(), tonic::Code::DeadlineExceeded);
        }
        // Let the runtime's deadline expire even if the caller's receive clock won the race.
        client
            .inner
            .runtime
            .block_on(
                async move { tokio::time::sleep(limit * 2).await },
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "no timed-out channel user may survive"
        );
        assert_eq!(channels.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            handlers.load(Ordering::SeqCst),
            0,
            "the peer must observe cancellation too"
        );
        drop(route);
        assert!(weak.upgrade().is_none(), "the now-idle route must be released");
    }

    #[test]
    fn installation_routes_share_while_used_and_release_the_channel_with_the_last_user() {
        struct Never;
        impl ChannelLauncher for Never {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                panic!("lazy route must not spawn here")
            }
        }
        let client = lazy_test_client(Arc::new(Never));
        let first = client.for_daemon("old-installation").unwrap();
        let state = first.inner.state.clone();
        let old = Arc::downgrade(&first.inner);
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let client = client.clone();
                std::thread::spawn(move || client.for_daemon("old-installation").unwrap())
            })
            .collect();
        for worker in workers {
            assert!(Arc::ptr_eq(&state, &worker.join().unwrap().inner.state));
        }
        assert!(Arc::ptr_eq(&client.inner, &client.for_daemon("").unwrap().inner));
        drop(first);
        assert!(
            old.upgrade().is_none(),
            "the route cache must not retain an idle channel"
        );
        let next = client.for_daemon("old-installation").unwrap();
        assert!(!Arc::ptr_eq(&state, &next.inner.state));
    }

    #[test]
    fn a_collected_installation_ends_reattach_as_agent_gone_and_discharges_its_stop() {
        use crate::AgentProvider;
        struct Gone;
        impl ChannelLauncher for Gone {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                panic!("missing installation must not spawn")
            }
            fn ensure_helper(&self, _: &str) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::NotFound, MISSING_INSTALLATION))
            }
        }
        let client = lazy_test_client(Arc::new(Gone));
        let handoff = crate::HelperHandoff {
            daemon: "old-installation".into(),
            declared_client_tools: ["editor.get_context".to_owned()].into(),
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            sidecar_id: "sidecar".into(),
            session_id: "session".into(),
            last_folded_sequence: 42,
        };
        let provider = crate::ClaudeSidecarProvider::reattach_through_helper(client.clone(), &handoff).unwrap();
        assert_eq!(provider.remote_ending(), Some(crate::RemoteEnding::AgentGone));
        let kept = provider
            .helper_handoff()
            .expect("a gone agent still keeps its watched session identity");
        assert_eq!(kept.daemon, handoff.daemon);
        assert_eq!(kept.session_id, handoff.session_id);
        assert_eq!(kept.declared_client_tools, handoff.declared_client_tools);
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !provider
            .pump()
            .iter()
            .any(|event| matches!(event, crate::AgentDomainEvent::SessionUnavailable { .. }))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "gone session never ended its watch"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            client
                .for_daemon(&handoff.daemon)
                .unwrap()
                .stop_if_known(&handoff.sidecar_id),
            Ok(())
        );
    }

    #[test]
    fn a_default_connection_without_an_installation_inventory_can_take_a_default_move() {
        struct Never;
        impl ChannelLauncher for Never {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                panic!("UI adoption must not spawn")
            }
        }
        let mut client = lazy_test_client(Arc::new(Never));
        assert!(
            client.for_known_owner("").is_err(),
            "a named route cannot assume ownership"
        );
        Arc::get_mut(&mut client.inner).unwrap().daemon.clear();
        let taken = client
            .for_known_owner("")
            .expect("the default helper's live tab can move");
        assert!(Arc::ptr_eq(&client.inner, &taken.inner));

        let client = client.with_installations(["older-installation".to_owned()]);
        assert!(
            client.for_known_owner("").is_err(),
            "a recorded inventory requires worker resolution even on a default connection"
        );
    }

    #[test]
    fn adopting_a_move_requires_known_ownership_without_crossing_the_network() {
        struct Never;
        impl ChannelLauncher for Never {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                panic!("UI adoption must not spawn")
            }
        }
        let client = lazy_test_client(Arc::new(Never)).with_installations(["older-installation".to_owned()]);
        assert!(client.for_known_owner("").is_err());
        assert!(client.for_known_owner("forgotten-installation").is_err());
        assert_eq!(
            client.for_known_owner("older-installation").unwrap().daemon(),
            "older-installation"
        );
    }

    #[test]
    fn ownerless_and_forgotten_records_search_every_installation_before_confirming_loss() {
        struct Never;
        impl ChannelLauncher for Never {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                panic!("lookup fixture must not spawn")
            }
        }
        let client = lazy_test_client(Arc::new(Never))
            .with_installations(["current-installation".into(), "older-installation".into()]);
        for owner in ["", "forgotten-installation"] {
            let mut asked = Vec::new();
            let route = client
                .for_sidecar_with(owner, "sidecar", |route| {
                    asked.push(route.daemon().to_owned());
                    Ok(if route.daemon() == "older-installation" {
                        vec![SidecarInfo {
                            sidecar_id: "sidecar".into(),
                            alive: true,
                            ..Default::default()
                        }]
                    } else {
                        Vec::new()
                    })
                })
                .unwrap();
            assert_eq!(
                route.daemon(),
                "older-installation",
                "live owner must receive reattach and stop"
            );
            assert_eq!(asked, ["current-installation", "older-installation"]);
            let mut asked = Vec::new();
            let _gone = client
                .for_sidecar_with(owner, "sidecar", |route| {
                    asked.push(route.daemon().to_owned());
                    Ok(Vec::new())
                })
                .unwrap();
            assert_eq!(
                asked,
                ["current-installation", "older-installation"],
                "loss needs every installation's answer"
            );
            assert!(
                client
                    .for_sidecar_with(owner, "sidecar", |route| {
                        if route.daemon() == "older-installation" {
                            Err(Status::unavailable("offline"))
                        } else {
                            Ok(Vec::new())
                        }
                    })
                    .is_err(),
                "absence under one installation cannot discharge the stop or offer resume"
            );
        }
    }

    #[test]
    fn unknown_ownership_cannot_be_declared_gone_while_an_installation_is_unreachable() {
        struct Unreachable;
        impl ChannelLauncher for Unreachable {
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                panic!("unreachable route must not spawn")
            }
            fn ensure_helper(&self, _: &str) -> io::Result<()> {
                Err(io::Error::other("cannot reach installation"))
            }
        }
        let client = lazy_test_client(Arc::new(Unreachable))
            .with_installations(["older-installation".to_owned(), "newer-installation".to_owned()]);
        let named = client.for_sidecar("forgotten-installation", "sidecar").unwrap();
        assert_eq!(
            named.daemon(),
            "forgotten-installation",
            "named handoffs must never use another installation's absence"
        );
        assert_eq!(named.hello().unwrap_err().code(), tonic::Code::Unavailable);
        for owner in ["", "forgotten-installation"] {
            let reached = if owner.is_empty() {
                client.for_sidecar(owner, "sidecar")
            } else {
                client.for_sidecar_with(owner, "sidecar", HelperClient::list)
            };
            let error = match reached {
                Err(error) => error,
                Ok(_) => panic!("unknown owner must stay unreachable"),
            };
            assert_eq!(error.code(), tonic::Code::Unavailable);
            assert!(
                client.stop_saved(owner, "sidecar").is_err(),
                "the stop must remain queued"
            );
        }
    }

    #[test]
    fn closing_a_connector_does_not_wait_for_its_worker_and_reaps_a_late_child() {
        use std::sync::mpsc;

        struct HeldLauncher {
            hold_check: bool,
            entered: mpsc::Sender<Option<u32>>,
            release: Mutex<mpsc::Receiver<()>>,
            finished: mpsc::Sender<()>,
        }
        impl Drop for HeldLauncher {
            fn drop(&mut self) {
                let _ = self.finished.send(());
            }
        }
        impl ChannelLauncher for HeldLauncher {
            fn check_link(&self) -> LinkCheck {
                if self.hold_check {
                    self.entered.send(None).unwrap();
                    let _ = self.release.lock().unwrap().recv_timeout(Duration::from_secs(30));
                    return LinkCheck::Failed(crate::ssh::ConnectError::from("link unavailable".to_owned()));
                }
                LinkCheck::NoMaster
            }
            fn spawn(&self, _: &[OsString]) -> io::Result<Child> {
                let child = std::process::Command::new("cat")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()?;
                self.entered.send(Some(child.id())).unwrap();
                let _ = self.release.lock().unwrap().recv_timeout(Duration::from_secs(30));
                Ok(child)
            }
        }

        for hold_check in [true, false] {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            let runtime = RuntimeThread::spawn();
            runtime.handle().spawn(open_channel(Opening {
                daemon: String::new(),
                state: Arc::new(RouteState::default()),
                launcher: Arc::new(HeldLauncher {
                    hold_check,
                    entered: entered_tx,
                    release: Mutex::new(release_rx),
                    finished: finished_tx,
                }),
                stderr: Arc::new(ChannelStderr::default()),
                pin: KeyPin {
                    pinned: None,
                    refused: Default::default(),
                    host: "example-host".into(),
                },
            }));
            let child = entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let (dropped_tx, dropped_rx) = mpsc::channel();
            let dropper = std::thread::spawn(move || {
                drop(runtime);
                let _ = dropped_tx.send(());
            });
            let dropped = dropped_rx.recv_timeout(Duration::from_secs(5));
            // Release the fixture even when shutdown incorrectly waits for it.
            release_tx.send(()).unwrap();
            dropper.join().unwrap();
            finished_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(pid) = child {
                // SAFETY: this is our own child, whose pid cannot be reused before it is reaped.
                let waited = unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
                let error = io::Error::last_os_error();
                if waited == 0 {
                    // SAFETY: waitpid established that this unreaped child still belongs to us.
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                        libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0);
                    }
                }
                assert_eq!(waited, -1, "cancelled connector must reap its child");
                assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
            }
            dropped.expect("closing the connector must finish while its worker remains held");
        }
    }

    #[tokio::test]
    async fn reconnect_checks_and_starts_before_spawning_and_keeps_the_key_guard_until_attachment() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Held(Arc<AtomicBool>);
        impl Drop for Held {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        struct Launcher {
            held: Arc<AtomicBool>,
            steps: Arc<Mutex<Vec<String>>>,
        }
        impl ChannelLauncher for Launcher {
            fn check_link(&self) -> LinkCheck {
                self.steps.lock().unwrap().push("master".into());
                LinkCheck::Restarted
            }
            fn ensure_helper(&self, daemon: &str) -> io::Result<()> {
                self.steps.lock().unwrap().push(format!("ensure {daemon}"));
                Ok(())
            }
            fn hold_host_key(&self, _: &str) -> io::Result<Box<dyn Send>> {
                self.held.store(true, Ordering::SeqCst);
                Ok(Box::new(Held(self.held.clone())))
            }
            fn spawn(&self, tail: &[OsString]) -> io::Result<Child> {
                assert!(self.held.load(Ordering::SeqCst));
                assert_eq!(
                    crate::ssh::decode_args(tail[2].to_str().unwrap()).unwrap(),
                    [OsString::from("--daemon"), OsString::from("old-installation")]
                );
                self.steps.lock().unwrap().push("connect".into());
                std::process::Command::new("sh")
                    .args(["-c", "printf x; cat"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
            }
        }
        let held = Arc::new(AtomicBool::new(false));
        let steps = Arc::new(Mutex::new(Vec::new()));
        let launcher = Arc::new(Launcher {
            held: held.clone(),
            steps: steps.clone(),
        });
        let state = Arc::new(RouteState::default());
        let mut io = open_channel(Opening {
            launcher: launcher.clone(),
            daemon: "old-installation".into(),
            state: state.clone(),
            stderr: Default::default(),
            pin: KeyPin {
                pinned: Some("SHA256:example".into()),
                refused: Default::default(),
                host: "example-host".into(),
            },
        })
        .await
        .unwrap();
        assert_eq!(*steps.lock().unwrap(), ["master", "ensure old-installation", "connect"]);
        assert!(held.load(Ordering::SeqCst));
        let mut byte = [0];
        tokio::io::AsyncReadExt::read_exact(io.inner_mut(), &mut byte)
            .await
            .unwrap();
        assert_eq!(byte, *b"x");
        assert!(!held.load(Ordering::SeqCst));
        let second = open_channel(Opening {
            launcher: launcher.clone(),
            daemon: "old-installation".into(),
            state: state.clone(),
            stderr: Default::default(),
            pin: KeyPin {
                pinned: Some("SHA256:example".into()),
                refused: Default::default(),
                host: "example-host".into(),
            },
        })
        .await
        .unwrap();
        assert_eq!(
            steps
                .lock()
                .unwrap()
                .iter()
                .filter(|step| step.starts_with("ensure"))
                .count(),
            1
        );
        drop(second);
        tokio::io::AsyncWriteExt::shutdown(io.inner_mut()).await.unwrap();
        tokio::io::AsyncReadExt::read_to_end(io.inner_mut(), &mut Vec::new())
            .await
            .unwrap();
        assert!(
            !state.started.load(Ordering::Acquire),
            "a dead daemon must enable checked START again"
        );
        let third = open_channel(Opening {
            launcher,
            daemon: "old-installation".into(),
            state,
            stderr: Default::default(),
            pin: KeyPin {
                pinned: Some("SHA256:example".into()),
                refused: Default::default(),
                host: "example-host".into(),
            },
        })
        .await
        .unwrap();
        assert_eq!(
            steps
                .lock()
                .unwrap()
                .iter()
                .filter(|step| step.starts_with("ensure"))
                .count(),
            2
        );
        drop(third);
    }

    /// A line in another encoding is kept as lossy text and the reading goes on past it.
    #[test]
    fn the_stderr_drain_reads_past_a_line_that_is_not_utf8() {
        let last = Mutex::new(None);
        drain_stderr(
            &b"first\n\xff\xfe banner\nPermission denied (publickey).\n\n"[..],
            &last,
        );
        assert_eq!(stderr_line(&last).as_deref(), Some("Permission denied (publickey)."));
        drain_stderr(&b"only \xff\n"[..], &last);
        assert_eq!(stderr_line(&last).as_deref(), Some("only \u{fffd}"));
    }

    /// Only a status tonic made of a broken channel, or no answer in time, may have lost an answer.
    #[test]
    fn only_a_lost_answer_repeats_a_start() {
        assert!(answer_lost(&Status::deadline_exceeded("no answer")));
        assert!(!answer_lost(&Status::unavailable("sidecar exited before binding")));
        let broken = Status::from_error(Box::new(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe")));
        assert!(std::error::Error::source(&broken).is_some());
        assert!(!answer_lost(&Status::invalid_argument("root must be absolute")));
    }

    /// The key a project is keyed by is the one the connection shows now, and only when it is the pinned one.
    #[test]
    fn the_verified_host_key_is_the_pinned_key_the_master_shows() {
        let host = "example-host";
        assert_eq!(
            shown_host_key(Some("SHA256:a"), Some(Ok("SHA256:a".into())), host),
            Ok("SHA256:a".to_string())
        );
        assert_eq!(shown_host_key(Some("SHA256:a"), None, host), Ok("SHA256:a".to_string()));
        let rotated = shown_host_key(Some("SHA256:a"), Some(Ok("SHA256:b".into())), host).unwrap_err();
        assert!(
            rotated.contains("SHA256:b") && rotated.contains("SHA256:a"),
            "{rotated}"
        );
        assert!(shown_host_key(Some("SHA256:a"), Some(Err(io::Error::other("gone"))), host).is_err());
        assert!(shown_host_key(None, Some(Ok("SHA256:a".into())), host).is_err());
    }

    /// Only a key ssh accepted that is not the pinned one counts as changed: an unreadable key, no master or no pin
    /// says nothing about the server.
    #[test]
    fn another_host_key_is_only_a_readable_key_other_than_the_pin() {
        assert_eq!(
            other_host_key(Some("SHA256:a"), Some(Ok("SHA256:b".into()))),
            Ok(Some("SHA256:b".to_string()))
        );
        assert_eq!(other_host_key(Some("SHA256:a"), Some(Ok("SHA256:a".into()))), Ok(None));
        assert_eq!(other_host_key(Some("SHA256:a"), None), Ok(None));
        assert_eq!(
            other_host_key(Some("SHA256:a"), Some(Err(io::Error::other("gone")))),
            Ok(None)
        );
        assert_eq!(other_host_key(None, Some(Ok("SHA256:b".into()))), Ok(None));
        // A master start under way: the key could not be looked at, which is not "unchanged".
        let busy = io::Error::new(io::ErrorKind::WouldBlock, "a master start is under way");
        assert!(other_host_key(Some("SHA256:a"), Some(Err(busy))).is_err());
    }
}
