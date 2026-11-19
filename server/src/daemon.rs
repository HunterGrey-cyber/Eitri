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
    sync_ignores: Arc<sync::ignores::SyncIgnores>,
    paths: Paths,
    /// Runs started by Exec that have not ended: the daemon does not exit for idleness under them, and stopping kills
    /// the ones whose input is unfinished and waits a while for the rest.
    runs: Arc<exec::Runs>,
    /// The roots whose file modes are kept; a running keeper holds the daemon past its idle exit.
    modes: Arc<sync::modes::Keepers>,
}

type Streamed<T> = Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

fn streamed<T: Send + 'static>(receiver: tokio::sync::mpsc::Receiver<Result<T, Status>>) -> Streamed<T> {
    Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver))
}

/// The most a request may carry: a ReplaceFileIf's 8 MiB of contents and room for its other fields and framing.
pub const MAX_REQUEST_BYTES: usize = sync::MAX_REPLACE_BYTES + 64 * 1024;

/// A streamed answer filled from a blocking thread: items go through a bounded channel, each waiting at most until
/// the call's deadline for the laptop to take it, and the last message (a refusal, or what ends the stream) goes into
/// a slot of its own that never waits, so a laptop that stopped reading still finds it after what it had not read.
struct Outbox<T> {
    sender: tokio::sync::mpsc::Sender<Result<T, Status>>,
    last: Option<tokio::sync::oneshot::Sender<Result<T, Status>>>,
    runtime: tokio::runtime::Handle,
    deadline: tokio::time::Instant,
    /// The deadline passed before everything was sent.
    late: bool,
}

impl<T: Send + 'static> Outbox<T> {
    fn new(within: Duration) -> (Self, Streamed<T>) {
        use futures_util::StreamExt;
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let (last, final_receiver) = tokio::sync::oneshot::channel();
        let stream = tokio_stream::wrappers::ReceiverStream::new(receiver)
            .chain(futures_util::stream::once(final_receiver).filter_map(|last| async move { last.ok() }));
        let outbox = Outbox {
            sender,
            last: Some(last),
            runtime: tokio::runtime::Handle::current(),
            deadline: tokio::time::Instant::now() + within,
            late: false,
        };
        (outbox, Box::pin(stream))
    }

    /// Sends one item from a blocking thread; `false` when nobody reads any more or the deadline passed.
    fn send(&mut self, item: T) -> bool {
        let sent = self
            .runtime
            .block_on(tokio::time::timeout_at(self.deadline, self.sender.send(Ok(item))));
        match sent {
            Ok(Ok(())) => true,
            Ok(Err(_)) => false,
            Err(_) => {
                self.late = true;
                false
            }
        }
    }

    /// Ends the stream with `last`, after everything already sent.
    fn finish(mut self, last: T) {
        if let Some(slot) = self.last.take() {
            let _ = slot.send(Ok(last));
        }
    }
}

/// A walk's budget: at least one entry, and never more than the helper's own cap.
fn budget_of(asked: u64) -> Result<u64, String> {
    if asked == 0 {
        return Err("a budget of at least one entry is needed".to_owned());
    }
    Ok(asked.min(sync::MAX_BUDGET))
}

/// Runs blocking file work off the runtime.
async fn off_runtime<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, Status> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Status::internal(error.to_string()))
}

#[tonic::async_trait]
impl Helper for Service {
    async fn hello(&self, request: Request<HelloRequest>) -> Result<Response<HelloResponse>, Status> {
        if request.get_ref().protocol_major != PROTOCOL_MAJOR {
            return Err(Status::failed_precondition("unsupported helper protocol major"));
        }
        let modes = self.modes.clone();
        let mode_warnings = off_runtime(move || modes.hello_warnings()).await?;
        Ok(Response::new(HelloResponse {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            capabilities: [
                CAP_SIDECARS,
                CAP_JOURNAL,
                CAP_PENDING,
                CAP_TRUST_SCAN,
                CAP_INSTALLATION_ARGS,
                CAP_SYNC_IGNORES,
                CAP_GIT_CONFIG,
                CAP_SYNC_FILES,
                CAP_SYNC_MODES,
                CAP_EXEC,
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
            mode_warnings: mode_warnings
                .into_iter()
                .map(|warning| SyncModeWarning {
                    host: warning.host,
                    root: warning.root,
                    text: warning.text,
                    paths: warning.paths,
                    sequence: warning.sequence,
                    epoch: warning.epoch,
                })
                .collect(),
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
        request: Request<SyncIgnoresRequest>,
    ) -> Result<Response<SyncIgnoresResponse>, Status> {
        mark_activity(&request);
        Ok(Response::new(
            self.sync_ignores.response(&request.into_inner().root).await,
        ))
    }

    async fn git_config(&self, request: Request<GitConfigRequest>) -> Result<Response<GitConfigResponse>, Status> {
        mark_activity(&request);
        let root = request.into_inner().root;
        Ok(Response::new(sync::git_config::answer(&root).await))
    }

    type ListTreeStream = Streamed<ListTreeEntry>;
    async fn list_tree(&self, request: Request<ListTreeRequest>) -> Result<Response<Self::ListTreeStream>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let (mut outbox, stream) = Outbox::new(sync::LIST_DEADLINE);
        tokio::task::spawn_blocking(move || {
            let listed = (|| {
                let budget = budget_of(request.budget)?;
                let root = sync::root(&request.root)?;
                let ignored = sync::listing::ignore_predicate(&request.patterns);
                sync::listing::list_tree(&root.dir, &*ignored, budget, &mut |listed| {
                    outbox.send(match listed {
                        sync::listing::Listed::Entry(visit) => ListTreeEntry {
                            rel: visit.rel,
                            kind: visit.kind.wire().to_owned(),
                            size: visit.size,
                            ..Default::default()
                        },
                        sync::listing::Listed::Truncated => ListTreeEntry {
                            truncated: true,
                            ..Default::default()
                        },
                    })
                })
            })();
            let refusal = match listed {
                Err(refusal) => refusal,
                Ok(()) if outbox.late => format!("the listing took longer than {} s", sync::LIST_DEADLINE.as_secs()),
                Ok(()) => return,
            };
            outbox.finish(ListTreeEntry {
                refusal,
                ..Default::default()
            });
        });
        Ok(Response::new(stream))
    }

    type ReadFileStream = Streamed<ReadFileChunk>;
    async fn read_file(&self, request: Request<ReadFileRequest>) -> Result<Response<Self::ReadFileStream>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let (mut outbox, stream) = Outbox::new(sync::READ_DEADLINE);
        tokio::task::spawn_blocking(move || {
            let last = match sync::root(&request.root) {
                Ok(root) => sync::files::read_file(&root, &request.rel, request.limit, &mut |chunk| outbox.send(chunk)),
                Err(refusal) => Some(ReadFileChunk {
                    refusal,
                    ..Default::default()
                }),
            };
            let last = match last {
                Some(last) if !outbox.late => last,
                _ if outbox.late => ReadFileChunk {
                    refusal: format!("the read took longer than {} s", sync::READ_DEADLINE.as_secs()),
                    ..Default::default()
                },
                // Nobody reads any more.
                _ => return,
            };
            outbox.finish(last);
        });
        Ok(Response::new(stream))
    }

    async fn replace_file_if(
        &self,
        request: Request<ReplaceFileIfRequest>,
    ) -> Result<Response<ReplaceFileIfResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let state = self.paths.state.clone();
        let response = off_runtime(move || match sync::root(&request.root) {
            Ok(root) => sync::files::replace_if(&state, &root, &request),
            Err(refusal) => ReplaceFileIfResponse {
                replaced: false,
                refusal,
            },
        })
        .await?;
        Ok(Response::new(response))
    }

    async fn remove_file_if(
        &self,
        request: Request<RemoveFileIfRequest>,
    ) -> Result<Response<RemoveFileIfResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let state = self.paths.state.clone();
        let response = off_runtime(move || match sync::root(&request.root) {
            Ok(root) => sync::files::remove_if(&state, &root, &request),
            Err(refusal) => RemoveFileIfResponse {
                removed: false,
                refusal,
            },
        })
        .await?;
        Ok(Response::new(response))
    }

    async fn wait_file_hash(
        &self,
        request: Request<WaitFileHashRequest>,
    ) -> Result<Response<WaitFileHashResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let response = off_runtime(move || match sync::root(&request.root) {
            Ok(root) => sync::files::wait_hash(&root, &request),
            Err(refusal) => WaitFileHashResponse {
                refusal,
                ..Default::default()
            },
        })
        .await?;
        Ok(Response::new(response))
    }

    async fn path_sizes(&self, request: Request<PathSizesRequest>) -> Result<Response<PathSizesResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let response = off_runtime(move || {
            let measured = (|| {
                let budget = budget_of(request.budget)?;
                let root = sync::root(&request.root)?;
                let ignored = sync::listing::ignore_predicate(&request.patterns);
                let deadline = std::time::Instant::now() + sync::LIST_DEADLINE;
                sync::listing::path_sizes(&root.dir, &request.rels, &*ignored, budget, deadline)
            })();
            match measured {
                Ok((sizes, truncated)) => PathSizesResponse {
                    sizes: sizes.into_iter().map(|(rel, bytes)| PathSize { rel, bytes }).collect(),
                    truncated,
                    refusal: String::new(),
                },
                Err(refusal) => PathSizesResponse {
                    refusal,
                    ..Default::default()
                },
            }
        })
        .await?;
        Ok(Response::new(response))
    }

    async fn project_activity(
        &self,
        request: Request<ProjectActivityRequest>,
    ) -> Result<Response<ProjectActivityResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let canonical = match off_runtime(move || sync::root(&request.root).map(|root| root.canonical)).await? {
            Ok(canonical) => canonical,
            Err(refusal) => {
                return Ok(Response::new(ProjectActivityResponse {
                    refusal,
                    ..Default::default()
                }))
            }
        };
        let facts = self.serving.activity_facts().await?;
        let sidecars = self.sidecars.list().sidecars;
        let (state, own_run, root) = (self.paths.state.clone(), self.paths.metadata_run(), canonical.clone());
        let others = off_runtime(move || sync::activity::other_daemons_at(&state, &own_run, &root)).await?;
        Ok(Response::new(sync::activity::judge(
            &canonical, &facts, &sidecars, others,
        )))
    }

    async fn watch_modes(&self, request: Request<WatchModesRequest>) -> Result<Response<WatchModesResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let modes = self.modes.clone();
        let host = modes.host().to_owned();
        let refusal =
            off_runtime(move || modes.set(&request.root, request.patterns, &request.staging_name, request.on))
                .await?
                .err()
                .unwrap_or_default();
        Ok(Response::new(WatchModesResponse { refusal, host }))
    }

    type ExecStream = Streamed<ExecOutput>;
    async fn exec(&self, request: Request<tonic::Streaming<ExecInput>>) -> Result<Response<Self::ExecStream>, Status> {
        mark_activity(&request);
        let started_ms = crate::run_dir::unix_ms();
        let mut input = request.into_inner();
        let start = match input.message().await? {
            Some(ExecInput {
                input: Some(exec_input::Input::Start(start)),
            }) => start,
            _ => {
                return Ok(Response::new(streamed(exec::refused(
                    "the first message must start the command".to_owned(),
                ))))
            }
        };
        let captured = match self.sidecars.login_environment().await {
            Ok(captured) => captured,
            Err(status) => {
                return Ok(Response::new(streamed(exec::refused(format!(
                    "the login environment is unavailable: {}",
                    status.message()
                )))))
            }
        };
        let prepared = match off_runtime(move || exec::git::prepare(&start, &captured.env)).await? {
            Ok(prepared) => prepared,
            Err(refusal) => return Ok(Response::new(streamed(exec::refused(refusal)))),
        };
        let (output, _ended) = exec::run(prepared, input, self.paths.state.clone(), started_ms, self.runs.clone());
        Ok(Response::new(streamed(output)))
    }
}

/// The helper service with the transport limits its requests need.
fn helper_server(service: Service) -> HelperServer<Service> {
    HelperServer::new(service).max_decoding_message_size(MAX_REQUEST_BYTES)
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
    let sync_ignores = Arc::new(sync::ignores::SyncIgnores::default());
    let environment = {
        let managed = managed.clone();
        let sync_ignores = sync_ignores.clone();
        tokio::spawn(async move {
            let captured = capture.await;
            sync_ignores.set_environment(&captured);
            managed.set_environment(captured);
        })
    };
    let connections = Arc::new(Mutex::new(Connections {
        active: 0,
        last_closed: Instant::now(),
    }));
    let owned = Arc::new(tokio::sync::Mutex::new(sidecars));
    let runs = Arc::new(exec::Runs::default());
    let modes = Arc::new(sync::modes::Keepers::new(&paths.state, mode_options(), &paths.key));
    {
        // Registrations outlive the daemon: every one on disk is taken up again (or left to the daemon keeping it).
        let modes = modes.clone();
        let _ = tokio::task::spawn_blocking(move || modes.resume()).await;
    }
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
            .add_service(helper_server(Service {
                sidecars: managed.clone(),
                serving: serving.clone(),
                sync_ignores: sync_ignores.clone(),
                paths: paths.clone(),
                runs: runs.clone(),
                modes: modes.clone(),
            }))
            .add_service(serve::Proxy(serving.clone()))
            .serve_with_incoming(incoming);
        tokio::pin!(server);
        let idle_exit = async {
            loop {
                tokio::time::sleep(idle.min(Duration::from_millis(100))).await;
                owned.lock().await.reap_exited(&paths.state);
                managed.observe_exits();
                let empty = owned.lock().await.children.is_empty()
                    && !managed.any_alive()
                    && runs.running() == 0
                    && !modes.holds();
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
                // A root another daemon kept, or one that could not be kept, is tried again.
                let keepers = modes.clone();
                let _ = tokio::task::spawn_blocking(move || keepers.resume()).await;
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
    // A run whose laptop has not closed its input is killed before the daemon's end closes that input for it; the
    // others get a little time to finish and are audited if they do not.
    runs.kill_unclosed();
    runs.settle(&paths.state, exec::STOP_GRACE).await;
    {
        let modes = modes.clone();
        let _ = tokio::task::spawn_blocking(move || modes.stop_all()).await;
    }
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

/// The account's umask (as the helper inherited it), the server's name and half the account's inotify watches, for
/// the mode keepers (who share that budget).
fn mode_options() -> sync::modes::Options {
    sync::modes::Options {
        umask: crate::run_dir::inherited_umask() as u32,
        host: sync::modes::host_name(),
        watch_limit: sync::modes::watch_budget(),
        watches: Default::default(),
    }
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
        let serving = serve::Serving::new(paths.clone(), sidecars.clone());
        let modes = Arc::new(sync::modes::Keepers::new(&paths.state, mode_options(), &paths.key));
        (
            Service {
                sidecars,
                serving,
                sync_ignores: Arc::new(sync::ignores::SyncIgnores::default()),
                paths,
                runs: Default::default(),
                modes,
            },
            scratch,
        )
    }

    #[tokio::test]
    async fn hello_lists_each_m2_capability_once_its_handler_is_built() {
        let (service, _scratch) = contract_service();
        let hello = service
            .hello(Request::new(HelloRequest {
                protocol_major: PROTOCOL_MAJOR,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        for capability in [
            CAP_SYNC_IGNORES,
            CAP_GIT_CONFIG,
            CAP_SYNC_FILES,
            CAP_SYNC_MODES,
            CAP_EXEC,
        ] {
            assert!(hello.capabilities.iter().any(|cap| cap == capability), "{capability}");
        }
        assert_eq!(hello.protocol_minor, 2);
    }

    #[tokio::test]
    async fn watch_modes_refuses_a_root_that_is_not_one() {
        let (service, _scratch) = contract_service();
        let response = service
            .watch_modes(Request::new(WatchModesRequest {
                root: "relative/project".into(),
                on: true,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!response.refusal.is_empty());
        assert!(!service.modes.holds());
    }

    #[tokio::test]
    async fn a_replacement_of_eight_mib_passes_the_transport() {
        let (service, scratch) = contract_service();
        let project = scratch.0.join("project");
        std::fs::create_dir(&project).unwrap();
        let socket = scratch.0.join("t.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let incoming = futures_util::stream::unfold(listener, |listener| async move {
            Some((listener.accept().await.map(|(stream, _)| stream), listener))
        });
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(helper_server(service))
                .serve_with_incoming(incoming),
        );
        let channel = tonic::transport::Endpoint::from_static("http://localhost")
            .connect_with_connector(tower::service_fn(move |_| {
                let socket = socket.clone();
                async move { Ok::<_, io::Error>(hyper_util::rt::TokioIo::new(UnixStream::connect(socket).await?)) }
            }))
            .await
            .unwrap();
        let mut client = helper_client::HelperClient::new(channel);
        // Over tonic's default 4 MiB, within the contract's 8 MiB.
        let data = vec![7u8; 5 * 1024 * 1024];
        let response = client
            .replace_file_if(ReplaceFileIfRequest {
                root: project.to_str().unwrap().into(),
                rel: "big.bin".into(),
                expected_sha256: String::new(),
                data: data.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert!(response.replaced, "{response:?}");
        assert_eq!(std::fs::read(project.join("big.bin")).unwrap(), data);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stream_that_runs_out_of_time_ends_with_its_refusal_after_what_was_sent() {
        use tokio_stream::StreamExt;
        let (outbox, mut stream) = Outbox::<ListTreeEntry>::new(Duration::from_millis(200));
        let filled = tokio::task::spawn_blocking(move || {
            let mut outbox = outbox;
            let mut sent = 0;
            // Nobody reads: the channel fills, and the next send waits until the deadline.
            while outbox.send(ListTreeEntry {
                rel: format!("f{sent}"),
                ..Default::default()
            }) {
                sent += 1;
            }
            assert!(outbox.late);
            outbox.finish(ListTreeEntry {
                refusal: "the listing took longer than 0 s".into(),
                ..Default::default()
            });
            sent
        })
        .await
        .unwrap();
        let mut seen = Vec::new();
        while let Some(entry) = stream.next().await {
            seen.push(entry.unwrap());
        }
        assert_eq!(seen.len(), filled + 1);
        assert!(seen[..filled].iter().all(|entry| entry.refusal.is_empty()));
        assert_eq!(seen[filled].refusal, "the listing took longer than 0 s");
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
