#[path = "exec/mod.rs"]
pub mod exec;
#[path = "login_env.rs"]
pub mod login_env;
#[path = "serve/mod.rs"]
pub mod serve;
#[path = "sidecars.rs"]
pub mod sidecars;
#[path = "sync/mod.rs"]
pub mod sync;
#[path = "test_exec.rs"]
#[cfg(test)]
#[allow(dead_code)]
pub mod test_exec;
#[path = "trust.rs"]
pub mod trust;

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use eitri_server_proto::helper_server::{Helper, HelperServer};
use eitri_server_proto::*;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tonic::{Request, Response, Status};

use crate::run_dir::Paths;

pub const IDLE_EXIT: Duration = Duration::from_secs(600);

struct Connections {
    active: usize,
    last_closed: Instant,
}

impl Connections {
    fn closed(&mut self, did_work: bool) {
        self.active -= 1;
        if did_work {
            self.last_closed = Instant::now();
        }
    }
}

/// Children are owned here until they have been stopped and reaped.
#[derive(Default)]
pub struct OwnedSidecars {
    pub children: Vec<tokio::process::Child>,
}

impl OwnedSidecars {
    fn reap_exited(&mut self, state: &std::path::Path) {
        self.children.retain_mut(|child| {
            let pid = child.id().unwrap_or(0);
            match child.try_wait() {
                Ok(Some(_)) => {
                    let _ = crate::audit::record(state, "sidecar-stop", &format!("pid={pid} exited"));
                    false
                }
                _ => true,
            }
        });
    }

    pub async fn stop_all(&mut self) -> io::Result<()> {
        let mut failure = None;
        // Signal all children first, so their grace periods overlap.
        for child in &mut self.children {
            if let Some(pid) = child.id() {
                // SAFETY: a live Child owns this pid until wait reaps it.
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        for child in &mut self.children {
            match tokio::time::timeout_at(deadline, child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => failure = Some(error),
                Err(_) => {
                    let _ = child.start_kill();
                    if tokio::time::timeout(Duration::from_secs(2), child.wait())
                        .await
                        .is_err()
                    {
                        failure = Some(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "owned child did not exit after SIGKILL",
                        ));
                    }
                }
            }
        }
        self.children.clear();
        failure.map_or(Ok(()), Err)
    }
}

struct Service {
    sidecars: Arc<sidecars::Sidecars>,
    serving: Arc<serve::Serving>,
}

#[tonic::async_trait]
impl Helper for Service {
    async fn hello(&self, request: Request<HelloRequest>) -> Result<Response<HelloResponse>, Status> {
        if request.get_ref().protocol_major != PROTOCOL_MAJOR {
            return Err(Status::failed_precondition("unsupported helper protocol major"));
        }
        Ok(Response::new(HelloResponse {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            capabilities: [
                CAP_SIDECARS,
                CAP_JOURNAL,
                CAP_PENDING,
                CAP_TRUST_SCAN,
                CAP_INSTALLATION_ARGS,
                CAP_GIT_CONFIG,
            ]
            .map(str::to_owned)
            .to_vec(),
            helper_version: crate::VERSION.to_owned(),
            helper_build: crate::REVISION.to_owned(),
            sidecar_revision: String::new(),
            warnings: self
                .sidecars
                .warnings()
                .into_iter()
                .chain(self.serving.warnings())
                .collect(),
            login_env: self.sidecars.login_env_state() as i32,
        }))
    }

    async fn start_sidecar(
        &self,
        request: Request<StartSidecarRequest>,
    ) -> Result<Response<StartSidecarResponse>, Status> {
        mark_activity(&request);
        let sidecars = self.sidecars.clone();
        // A caller disconnecting must not cancel a spawn after its process has been created.
        let sidecar_id = tokio::spawn(async move { sidecars.start(request.into_inner()).await })
            .await
            .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(StartSidecarResponse { sidecar_id }))
    }

    async fn stop_sidecar(
        &self,
        request: Request<StopSidecarRequest>,
    ) -> Result<Response<StopSidecarResponse>, Status> {
        mark_activity(&request);
        sidecars::validate_command_id(&request.get_ref().command_id)?;
        let sidecars = self.sidecars.clone();
        tokio::spawn(async move { sidecars.stop(&request.into_inner().sidecar_id).await })
            .await
            .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(StopSidecarResponse {}))
    }

    async fn list_sidecars(
        &self,
        request: Request<ListSidecarsRequest>,
    ) -> Result<Response<ListSidecarsResponse>, Status> {
        mark_activity(&request);
        let mut listed = self.sidecars.list();
        listed.warnings.extend(self.serving.warnings());
        for sidecar in &mut listed.sidecars {
            sidecar.sessions = self.serving.sessions(&sidecar.sidecar_id).await;
        }
        Ok(Response::new(listed))
    }

    async fn pending(&self, request: Request<PendingRequest>) -> Result<Response<PendingResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let requests = self.serving.pending(&request.sidecar_id, &request.session_id).await?;
        Ok(Response::new(PendingResponse { requests }))
    }

    async fn trust_scan(&self, request: Request<TrustScanRequest>) -> Result<Response<TrustScanResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        // Discovery walks and hashes a tree, so it never runs on a runtime thread.
        let scanned = tokio::task::spawn_blocking(move || trust::scan(&request.root, &request.profile_key))
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(match scanned {
            Ok(discovery_json) => TrustScanResponse {
                discovery_json,
                refusal: String::new(),
            },
            Err(refusal) => TrustScanResponse {
                discovery_json: Vec::new(),
                refusal,
            },
        }))
    }
    async fn sync_ignores(
        &self,
        _request: Request<SyncIgnoresRequest>,
    ) -> Result<Response<SyncIgnoresResponse>, Status> {
        Err(Status::unimplemented("SyncIgnores is not built yet"))
    }

    async fn git_config(&self, request: Request<GitConfigRequest>) -> Result<Response<GitConfigResponse>, Status> {
        mark_activity(&request);
        let root = request.into_inner().root;
        Ok(Response::new(sync::git_config::answer(&root).await))
    }

    type ListTreeStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ListTreeEntry, Status>> + Send>>;
    async fn list_tree(&self, _request: Request<ListTreeRequest>) -> Result<Response<Self::ListTreeStream>, Status> {
        Err(Status::unimplemented("ListTree is not built yet"))
    }

    type ReadFileStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ReadFileChunk, Status>> + Send>>;
    async fn read_file(&self, _request: Request<ReadFileRequest>) -> Result<Response<Self::ReadFileStream>, Status> {
        Err(Status::unimplemented("ReadFile is not built yet"))
    }

    async fn replace_file_if(
        &self,
        _request: Request<ReplaceFileIfRequest>,
    ) -> Result<Response<ReplaceFileIfResponse>, Status> {
        Err(Status::unimplemented("ReplaceFileIf is not built yet"))
    }

    async fn remove_file_if(
        &self,
        _request: Request<RemoveFileIfRequest>,
    ) -> Result<Response<RemoveFileIfResponse>, Status> {
        Err(Status::unimplemented("RemoveFileIf is not built yet"))
    }

    async fn wait_file_hash(
        &self,
        _request: Request<WaitFileHashRequest>,
    ) -> Result<Response<WaitFileHashResponse>, Status> {
        Err(Status::unimplemented("WaitFileHash is not built yet"))
    }

    async fn path_sizes(&self, _request: Request<PathSizesRequest>) -> Result<Response<PathSizesResponse>, Status> {
        Err(Status::unimplemented("PathSizes is not built yet"))
    }

    async fn project_activity(
        &self,
        _request: Request<ProjectActivityRequest>,
    ) -> Result<Response<ProjectActivityResponse>, Status> {
        Err(Status::unimplemented("ProjectActivity is not built yet"))
    }

    async fn watch_modes(&self, _request: Request<WatchModesRequest>) -> Result<Response<WatchModesResponse>, Status> {
        Err(Status::unimplemented("WatchModes is not built yet"))
    }

    type ExecStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ExecOutput, Status>> + Send>>;
    async fn exec(&self, _request: Request<tonic::Streaming<ExecInput>>) -> Result<Response<Self::ExecStream>, Status> {
        Err(Status::unimplemented("Exec is not built yet"))
    }
}

pub async fn run(paths: Paths, idle: Duration) -> io::Result<()> {
    run_with_sidecars(paths, idle, OwnedSidecars::default()).await
}

pub async fn run_with_sidecars(paths: Paths, idle: Duration, sidecars: OwnedSidecars) -> io::Result<()> {
    run_with_environment(paths, idle, sidecars, login_env::capture()).await
}

pub async fn run_with_environment(
    paths: Paths,
    idle: Duration,
    sidecars: OwnedSidecars,
    capture: impl std::future::Future<Output = io::Result<login_env::Captured>> + Send + 'static,
) -> io::Result<()> {
    paths.prepare()?;
    let live = agent::private_fs::PrivateLock::open(&paths.metadata_run().join("live.lock"))?;
    if !live.try_lock()? {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "a daemon already owns this run directory",
        ));
    }
    let managed = Arc::new(sidecars::Sidecars::new(paths.clone())?);
    let serving = serve::Serving::new(paths.clone(), managed.clone());
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = UnixListener::bind(paths.socket()?)?;
    let _cleanup = Cleanup(paths.clone());
    std::fs::set_permissions(
        paths.socket()?,
        std::fs::Permissions::from_mode(agent::private_fs::PRIVATE_FILE_MODE),
    )?;
    let info = probe::DaemonInfo {
        version: paths.version.clone(),
        rev: crate::REVISION.to_owned(),
        pid: std::process::id(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        started_unix_ms: crate::run_dir::unix_ms(),
    };
    // The installation it runs from goes beside: a stop checks the daemon's executable against it.
    let published = crate::run_dir::Published {
        info: info.clone(),
        installation: paths.installation_name(),
    };
    let dir = agent::private_fs::open_dir(&paths.metadata_run())?;
    agent::private_fs::publish_private_file(
        &dir,
        std::ffi::OsStr::new("info"),
        &serde_json::to_vec(&published).map_err(io::Error::other)?,
    )?;
    let _ = crate::audit::record(
        &paths.state,
        "daemon-start",
        &format!("version={} pid={}", paths.version, info.pid),
    );
    for pid in sidecars.children.iter().filter_map(tokio::process::Child::id) {
        let _ = crate::audit::record(&paths.state, "sidecar-start", &format!("pid={pid}"));
    }
    let environment = {
        let managed = managed.clone();
        tokio::spawn(async move {
            managed.set_environment(capture.await);
        })
    };
    let connections = Arc::new(Mutex::new(Connections {
        active: 0,
        last_closed: Instant::now(),
    }));
    let owned = Arc::new(tokio::sync::Mutex::new(sidecars));
    let tracked = connections.clone();
    let incoming = futures_util::stream::unfold(listener, move |listener| {
        let tracked = tracked.clone();
        async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        if crate::peer::check(&stream).is_err() {
                            continue;
                        }
                        tracked.lock().expect("connection state").active += 1;
                        return Some((
                            Ok::<_, io::Error>(Connection {
                                stream,
                                state: tracked,
                                activity: Arc::new(AtomicBool::new(false)),
                            }),
                            listener,
                        ));
                    }
                    Err(error) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        return Some((Err(error), listener));
                    }
                }
            }
        }
    });
    let result = {
        let server = tonic::transport::Server::builder()
            .http2_keepalive_interval(Some(Duration::from_secs(10)))
            .http2_keepalive_timeout(Some(Duration::from_secs(20)))
            .add_service(HelperServer::new(Service {
                sidecars: managed.clone(),
                serving: serving.clone(),
            }))
            .add_service(serve::Proxy(serving.clone()))
            .serve_with_incoming(incoming);
        tokio::pin!(server);
        let idle_exit = async {
            loop {
                tokio::time::sleep(idle.min(Duration::from_millis(100))).await;
                owned.lock().await.reap_exited(&paths.state);
                managed.observe_exits();
                let empty = owned.lock().await.children.is_empty() && !managed.any_alive();
                let state = connections.lock().expect("connection state");
                if empty && state.active == 0 && state.last_closed.elapsed() >= idle {
                    break;
                }
            }
        };
        let maintenance = async {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let manager = managed.clone();
                let _ = tokio::task::spawn_blocking(move || manager.trim_logs()).await;
                managed.refresh_handshakes().await;
                if let Err(error) = serving.maintain().await {
                    serving.audit_budget_error(&error.to_string()).await;
                }
                if let Err(error) = serving.adopt().await {
                    let _ = crate::audit::record(&paths.state, "watch-adoption", &error.to_string());
                }
            }
        };
        tokio::pin!(maintenance);
        tokio::select! {
            result = &mut server => (result.map_err(io::Error::other), false),
            _ = term.recv() => (Ok(()), true),
            _ = idle_exit => (Ok(()), false),
            _ = &mut maintenance => unreachable!(),
        }
        // Dropping the server stops acceptance without waiting for open RPC streams.
    };
    environment.abort();
    let (result, terminate) = result;
    let managed_stopped = if terminate { managed.stop_all().await } else { Ok(()) };
    let mut sidecars = owned.lock().await;
    let pids: Vec<_> = sidecars.children.iter().filter_map(tokio::process::Child::id).collect();
    let stopped = if terminate { sidecars.stop_all().await } else { Ok(()) };
    for pid in pids {
        crate::audit::record(&paths.state, "sidecar-stop", &format!("pid={pid}"))?;
    }
    let audited = crate::audit::record(
        &paths.state,
        "daemon-stop",
        &format!("version={} pid={}", paths.version, info.pid),
    );
    result.and(managed_stopped).and(stopped).and(audited)
}

fn mark_activity<T>(request: &Request<T>) {
    if let Some(activity) = request.extensions().get::<Arc<AtomicBool>>() {
        activity.store(true, Ordering::Relaxed);
    }
}

struct Cleanup(Paths);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Ok(socket) = self.0.socket() {
            let _ = std::fs::remove_file(socket);
        }
        let _ = std::fs::remove_file(self.0.metadata_run().join("info"));
    }
}

struct Connection {
    stream: UnixStream,
    state: Arc<Mutex<Connections>>,
    activity: Arc<AtomicBool>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("connection state");
        state.closed(self.activity.load(Ordering::Relaxed));
    }
}

impl tonic::transport::server::Connected for Connection {
    type ConnectInfo = Arc<AtomicBool>;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.activity.clone()
    }
}

impl AsyncRead for Connection {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn contract_service() -> (Service, Scratch) {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        // Keep the socket names short even when the runner's TMPDIR is long.
        let root = std::path::PathBuf::from(format!(
            "/tmp/m2-contracts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let scratch = Scratch(root.clone());
        agent::private_fs::create_private_dir_all(&root, &root).unwrap();
        let paths = Paths::new(crate::VERSION, &root.join("data"), &root.join("state"), &root).unwrap();
        paths.prepare().unwrap();
        let sidecars = Arc::new(sidecars::Sidecars::new(paths.clone()).unwrap());
        let serving = serve::Serving::new(paths, sidecars.clone());
        (Service { sidecars, serving }, scratch)
    }

    #[tokio::test]
    async fn hello_lists_no_m2_capability_until_its_handler_is_built() {
        let (service, _scratch) = contract_service();
        let hello = service
            .hello(Request::new(HelloRequest {
                protocol_major: PROTOCOL_MAJOR,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        for capability in [CAP_SYNC_IGNORES, CAP_SYNC_FILES, CAP_SYNC_MODES, CAP_EXEC] {
            assert!(!hello.capabilities.iter().any(|cap| cap == capability), "{capability}");
        }
        assert!(hello.capabilities.iter().any(|cap| cap == CAP_GIT_CONFIG));
        assert_eq!(hello.protocol_minor, 2);
    }

    #[tokio::test]
    async fn new_rpcs_answer_unimplemented_with_their_name() {
        let (service, _scratch) = contract_service();
        let unary = service
            .sync_ignores(Request::new(SyncIgnoresRequest {
                root: "/srv/project".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(unary.code(), tonic::Code::Unimplemented);
        assert_eq!(unary.message(), "SyncIgnores is not built yet");
        let streaming = service
            .list_tree(Request::new(ListTreeRequest {
                root: "/srv/project".into(),
                ..Default::default()
            }))
            .await
            .err()
            .unwrap();
        assert_eq!(streaming.code(), tonic::Code::Unimplemented);
        assert_eq!(streaming.message(), "ListTree is not built yet");
    }

    #[tokio::test]
    async fn sigterm_stops_owned_sidecars_then_the_daemon() {
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        let mut owned = super::OwnedSidecars { children: vec![child] };
        owned.stop_all().await.unwrap();
        assert!(owned.children.is_empty());
        assert!(!crate::run_dir::pid_alive(pid));
    }
    #[tokio::test]
    async fn exited_owned_children_are_reaped_and_no_longer_block_idle_exit() {
        let root = std::env::temp_dir().join(format!("owned-exit-{}", std::process::id()));
        let child = tokio::process::Command::new("/bin/true").spawn().unwrap();
        let pid = child.id().unwrap();
        let mut owned = super::OwnedSidecars { children: vec![child] };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !owned.children.is_empty() {
            owned.reap_exited(&root);
            assert!(tokio::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        assert!(!crate::run_dir::pid_alive(pid));
        assert!(std::fs::read_to_string(root.join("audit.log"))
            .unwrap()
            .contains(&format!("sidecar-stop pid={pid}")));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn hello_only_disconnect_preserves_the_idle_clock() {
        let before = std::time::Instant::now() - std::time::Duration::from_secs(30);
        let mut connections = super::Connections {
            active: 1,
            last_closed: before,
        };
        connections.closed(false);
        assert_eq!(connections.active, 0);
        assert_eq!(connections.last_closed, before);
        connections.active = 1;
        connections.closed(true);
        assert!(connections.last_closed > before);
    }
}
