//! The actor against a fake engine, a fake helper and a fake editor.

use super::*;
use crate::remote_sync::engine::{Labels, SessionSpec, Side, Staging, SyncState};
use crate::remote_sync::quiet::{EditorAnswer, EditorState};
use crate::remote_sync::sessions::ModeLine;
use agent::remote_client as wire;
use std::sync::Mutex;

/// Every call the fakes see, in order, shared by engine and helper.
pub(crate) type Log = Arc<Mutex<Vec<String>>>;

struct EngineState {
    sessions: BTreeMap<String, (SessionStatus, Labels)>,
    monitors: Vec<(String, Sender<SessionStatus>)>,
    flush: FlushOutcome,
    monitor_fails: bool,
    ending_tree: bool,
    daemon_down: bool,
    fail_create: Option<Role>,
    /// Every create of this role fails, not only the next.
    fail_create_always: Option<Role>,
    pause_fails: bool,
    terminate_fails: bool,
    progress_dir: Option<PathBuf>,
    progress_at_create: Option<recreate::Progress>,
    git_config_seen_at_git_create: Option<bool>,
    root: PathBuf,
    /// Sessions created so far: each instance gets its own identifier, as Mutagen gives it.
    created: u64,
}

pub(crate) struct FakeEngine {
    clear_conflicts: Mutex<Option<(PathBuf, Vec<u8>)>>,
    status_reads: std::sync::atomic::AtomicUsize,
    state: Mutex<EngineState>,
    log: Log,
}

impl FakeEngine {
    pub(crate) fn new(log: Log, root: &Path) -> Arc<Self> {
        Arc::new(Self {
            clear_conflicts: Default::default(),
            status_reads: Default::default(),
            state: Mutex::new(EngineState {
                sessions: BTreeMap::new(),
                monitors: vec![],
                flush: FlushOutcome::Flushed,
                monitor_fails: false,
                ending_tree: false,
                daemon_down: false,
                fail_create: None,
                fail_create_always: None,
                pause_fails: false,
                terminate_fails: false,
                progress_dir: None,
                progress_at_create: None,
                git_config_seen_at_git_create: None,
                root: root.to_owned(),
                created: 0,
            }),
            log,
        })
    }

    fn log(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }

    fn set_state(&self, name: &str, new: SyncState) -> Result<(), crate::remote_sync::SyncError> {
        let mut state = self.state.lock().unwrap();
        let changed = state.sessions.get_mut(name).map(|(status, _)| {
            status.state = new;
            status.clone()
        });
        match changed {
            Some(status) => {
                emit(&mut state, status);
                Ok(())
            }
            None => Err(crate::remote_sync::SyncError::Engine("no such session".into())),
        }
    }

    /// Ends every monitor stream, as a daemon that stops does.
    pub(crate) fn end_monitors(&self) {
        self.state.lock().unwrap().monitors.clear();
    }

    /// Sets a session's status and sends it to its monitors. A status that names no instance keeps the current one.
    pub(crate) fn push(&self, mut status: SessionStatus) {
        let mut state = self.state.lock().unwrap();
        if let Some((current, _)) = state.sessions.get_mut(&status.name) {
            if status.identifier.is_empty() {
                status.identifier = current.identifier.clone();
            }
            *current = status.clone();
        }
        state
            .monitors
            .retain(|(name, sender)| name != &status.name || sender.send(status.clone()).is_ok());
    }

    pub(crate) fn insert(&self, name: &str, role: Role, root: &Path, state: SyncState) {
        let key = name.trim_start_matches("eitri-").split('-').next().unwrap().to_owned();
        let mut labels = engine::project_labels(&SyncKey::new("x", root), role, root);
        labels.insert("eitri-key".into(), key);
        let mut status = SessionStatus::disconnected(name, role, "x");
        status.state = state;
        status.last_error = None;
        let mut engine = self.state.lock().unwrap();
        engine.created += 1;
        status.identifier = format!("sync_{}", engine.created);
        engine.sessions.insert(name.into(), (status, labels));
    }

    pub(crate) fn clear_when_equal(&self, path: PathBuf, bytes: Vec<u8>) {
        *self.clear_conflicts.lock().unwrap() = Some((path, bytes));
    }

    /// A session's status as a listing reports it.
    fn reported(&self, status: &SessionStatus) -> SessionStatus {
        let mut status = status.clone();
        if self
            .clear_conflicts
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(path, bytes)| std::fs::read(path).ok().as_ref() == Some(bytes))
        {
            status.conflicts.clear();
        }
        status
    }

    pub(crate) fn status_of(&self, name: &str) -> Option<SessionStatus> {
        self.state.lock().unwrap().sessions.get(name).map(|(s, _)| s.clone())
    }

    pub(crate) fn calls(&self, prefix: &str) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with(prefix))
            .cloned()
            .collect()
    }
}

use crate::remote_sync::engine;

fn emit(state: &mut EngineState, status: SessionStatus) {
    state
        .monitors
        .retain(|(name, sender)| name != &status.name || sender.send(status.clone()).is_ok());
}

impl SyncEngine for FakeEngine {
    fn start(&self) -> Result<(), crate::remote_sync::SyncError> {
        self.state.lock().unwrap().daemon_down = false;
        Ok(())
    }

    fn create(&self, spec: &SessionSpec) -> Result<(), crate::remote_sync::SyncError> {
        self.log(format!("create {} {}", spec.name, spec.ignores.join(",")));
        let mut state = self.state.lock().unwrap();
        if spec.role == Role::Tree {
            state.progress_at_create = state.progress_dir.as_ref().and_then(|dir| recreate::read_progress(dir));
        }
        if state.fail_create == Some(spec.role) || state.fail_create_always == Some(spec.role) {
            state.fail_create = None;
            return Err(crate::remote_sync::SyncError::Engine("create failed".into()));
        }
        if spec.role == Role::Git {
            let seen = state.root.join(".git/config").exists();
            state.git_config_seen_at_git_create = Some(seen);
        }
        let mut status = SessionStatus::disconnected(&spec.name, spec.role, "x");
        status.state = SyncState::Scanning;
        status.last_error = None;
        state.created += 1;
        status.identifier = format!("sync_{}", state.created);
        state.sessions.insert(spec.name.clone(), (status, spec.labels.clone()));
        Ok(())
    }
    fn terminate(&self, name: &str) -> Result<(), crate::remote_sync::SyncError> {
        self.log(format!("terminate {name}"));
        let mut state = self.state.lock().unwrap();
        if state.terminate_fails {
            return Err(crate::remote_sync::SyncError::Engine("terminate timed out".into()));
        }
        match state.sessions.remove(name) {
            Some((status, _)) => {
                // As Mutagen's monitor does, the stream says the session is gone.
                let gone = SessionStatus::disconnected(name, status.role, "no such session");
                emit(&mut state, gone);
                Ok(())
            }
            None => Err(crate::remote_sync::SyncError::Engine("no such session".into())),
        }
    }
    fn pause(&self, name: &str) -> Result<(), crate::remote_sync::SyncError> {
        self.log(format!("pause {name}"));
        if self.state.lock().unwrap().pause_fails {
            return Err(crate::remote_sync::SyncError::Engine("pause timed out".into()));
        }
        self.set_state(name, SyncState::Paused)
    }
    fn resume(&self, name: &str) -> Result<(), crate::remote_sync::SyncError> {
        self.log(format!("resume {name}"));
        self.set_state(name, SyncState::Scanning)
    }
    fn status(&self, labels: &Labels) -> Result<Vec<SessionStatus>, crate::remote_sync::SyncError> {
        self.status_reads.fetch_add(1, Ordering::Relaxed);
        let state = self.state.lock().unwrap();
        if state.daemon_down {
            return Err(crate::remote_sync::SyncError::Engine(
                "unable to connect to daemon: offline".into(),
            ));
        }
        Ok(state
            .sessions
            .values()
            .filter(|(_, have)| labels.iter().all(|(k, v)| have.get(k) == Some(v)))
            .map(|(status, _)| self.reported(status))
            .collect())
    }
    fn session_status_within(
        &self,
        name: &str,
        _cap: Duration,
    ) -> Result<SessionStatus, crate::remote_sync::SyncError> {
        let state = self.state.lock().unwrap();
        match state.sessions.get(name) {
            Some((status, _)) => Ok(self.reported(status)),
            None => Err(crate::remote_sync::SyncError::Engine(format!(
                "no such session: {name}"
            ))),
        }
    }
    fn flush(&self, name: &str, _cap: Duration) -> FlushOutcome {
        self.log(format!("flush {name}"));
        let mut state = self.state.lock().unwrap();
        let outcome = state.flush.clone();
        // A flush runs one more cycle, which the monitor reports.
        let next = state.sessions.get_mut(name).map(|(status, _)| {
            status.state = SyncState::Watching;
            status.successful_cycles += 1;
            status.clone()
        });
        if let Some(next) = next {
            emit(&mut state, next);
        }
        outcome
    }
    fn monitor(
        &self,
        name: &str,
    ) -> Result<Box<dyn Iterator<Item = SessionStatus> + Send>, crate::remote_sync::SyncError> {
        let mut state = self.state.lock().unwrap();
        self.log(format!("monitor {name}"));
        if state.ending_tree && name.ends_with("-tree") {
            return Ok(Box::new(std::iter::once(SessionStatus::disconnected(
                name,
                Role::Tree,
                "monitor ended",
            ))));
        }
        if state.monitor_fails {
            return Err(crate::remote_sync::SyncError::Engine("no monitor".into()));
        }
        let (sender, receiver) = mpsc::channel();
        // Like Mutagen's, the stream starts with the session's state now.
        if let Some((status, _)) = state.sessions.get(name) {
            let _ = sender.send(status.clone());
        }
        state.monitors.push((name.into(), sender));
        Ok(Box::new(receiver.into_iter()))
    }
}

struct HelperState {
    file_size: u64,
    patterns: Vec<String>,
    git_refusal: String,
    server_files: Vec<String>,
    truncated: bool,
    sizes: u64,
    activity: wire::ProjectActivityResponse,
    mode_lines: Vec<ModeLine>,
    mode_lines_error: Option<String>,
    /// WatchModes fails with this while set.
    watch_modes_error: Option<String>,
    /// The host key the connection under the helper shows when it is not the project's.
    shown_key: Option<String>,
    /// The connection's key cannot be looked at (a master start under way) while set.
    key_unknown: bool,
    /// How many more WatchModes answers say the keeper has not settled.
    watch_modes_settling: u32,
    on_list: Option<Box<dyn Fn() + Send>>,
    /// Runs while the helper is asked for the git config, as a slow server reply would let things happen meanwhile.
    on_git_config: Option<Box<dyn Fn() + Send>>,
}

pub(crate) struct FakeHelper {
    pub(crate) files: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    pub(crate) refuse_compare: std::sync::atomic::AtomicBool,
    state: Mutex<HelperState>,
    log: Log,
}

impl FakeHelper {
    pub(crate) fn new(log: Log) -> Arc<Self> {
        Arc::new(Self {
            files: Default::default(),
            refuse_compare: Default::default(),
            state: Mutex::new(HelperState {
                file_size: 1,
                patterns: vec!["/target/".into()],
                git_refusal: String::new(),
                server_files: vec![],
                truncated: false,
                sizes: 80 * MIB,
                activity: wire::ProjectActivityResponse::default(),
                mode_lines: vec![],
                mode_lines_error: None,
                watch_modes_error: None,
                shown_key: None,
                watch_modes_settling: 0,
                key_unknown: false,
                on_list: None,
                on_git_config: None,
            }),
            log,
        })
    }

    pub(crate) fn truncate_sizes(&self) {
        self.set(|state| state.truncated = true);
    }

    pub(crate) fn activity(&self, activity: wire::ProjectActivityResponse) {
        self.set(|s| s.activity = activity);
    }

    pub(crate) fn list_file(&self, name: &str, size: u64) {
        self.set(|s| {
            s.server_files = vec![name.into()];
            s.file_size = size;
        });
    }

    fn set(&self, change: impl FnOnce(&mut HelperState)) {
        change(&mut self.state.lock().unwrap());
    }
}

impl SyncHelper for FakeHelper {
    fn read_file(&self, request: wire::ReadFileRequest) -> Result<Vec<wire::ReadFileChunk>, String> {
        let files = self.files.lock().unwrap();
        Ok(vec![match files.get(&request.rel) {
            Some(data) if data.len() as u64 > request.limit => return Err("over the read limit".into()),
            Some(data) => wire::ReadFileChunk {
                data: data.clone(),
                sha256: super::super::conflicts::hash(data),
                kind: "file".into(),
                ..Default::default()
            },
            None => wire::ReadFileChunk {
                kind: "missing".into(),
                ..Default::default()
            },
        }])
    }
    fn remove_file_if(&self, request: wire::RemoveFileIfRequest) -> Result<wire::RemoveFileIfResponse, String> {
        let mut files = self.files.lock().unwrap();
        let removed = !self.refuse_compare.load(Ordering::Relaxed)
            && files
                .get(&request.rel)
                .is_some_and(|data| super::super::conflicts::hash(data) == request.expected_sha256);
        if removed {
            files.remove(&request.rel);
        }
        Ok(wire::RemoveFileIfResponse {
            removed,
            refusal: if removed {
                String::new()
            } else {
                "changed on the server".into()
            },
        })
    }
    fn replace_file_if(&self, request: wire::ReplaceFileIfRequest) -> Result<wire::ReplaceFileIfResponse, String> {
        let mut files = self.files.lock().unwrap();
        let replaced = !self.refuse_compare.load(Ordering::Relaxed)
            && files
                .get(&request.rel)
                .map(|data| super::super::conflicts::hash(data))
                .unwrap_or_default()
                == request.expected_sha256;
        if replaced {
            files.insert(request.rel, request.data);
        }
        Ok(wire::ReplaceFileIfResponse {
            replaced,
            refusal: if replaced {
                String::new()
            } else {
                "changed on the server".into()
            },
        })
    }
    fn sync_ignores(&self, _root: &Path) -> Result<wire::SyncIgnoresResponse, String> {
        self.log.lock().unwrap().push("sync_ignores".into());
        let patterns = self.state.lock().unwrap().patterns.clone();
        let digest = Translated::new(patterns.clone(), vec![]).digest;
        Ok(wire::SyncIgnoresResponse {
            patterns,
            tracked_ignored: vec![],
            digest,
            problems: vec![],
            refusal: String::new(),
        })
    }
    fn git_config(&self, _root: &Path) -> Result<wire::GitConfigResponse, String> {
        let hook = self.state.lock().unwrap().on_git_config.take();
        if let Some(hook) = hook {
            hook();
        }
        let refusal = self.state.lock().unwrap().git_refusal.clone();
        Ok(wire::GitConfigResponse {
            entries: vec![wire::GitConfigEntry {
                key: "user.name".into(),
                value: "Someone".into(),
            }],
            digest: "d".into(),
            refusal,
        })
    }
    fn list_tree(&self, _root: &Path, patterns: &[String], _budget: u64) -> Result<Vec<wire::ListTreeEntry>, String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("list_tree {}", patterns.join(",")));
        let state = self.state.lock().unwrap();
        if let Some(hook) = &state.on_list {
            hook();
        }
        let mut entries: Vec<wire::ListTreeEntry> = state
            .server_files
            .iter()
            .map(|rel| wire::ListTreeEntry {
                rel: rel.clone(),
                kind: "file".into(),
                size: state.file_size,
                truncated: false,
                refusal: String::new(),
            })
            .collect();
        if state.truncated {
            entries.push(wire::ListTreeEntry {
                truncated: true,
                ..Default::default()
            });
        }
        Ok(entries)
    }
    fn path_sizes(
        &self,
        _root: &Path,
        rels: &[String],
        whole_root: bool,
        _patterns: &[String],
        _budget: u64,
    ) -> Result<wire::PathSizesResponse, String> {
        let state = self.state.lock().unwrap();
        let bytes = state.sizes;
        if whole_root {
            assert!(rels.is_empty());
            return Ok(wire::PathSizesResponse {
                sizes: vec![wire::PathSize {
                    rel: String::new(),
                    bytes,
                }],
                truncated: state.truncated,
                refusal: String::new(),
            });
        }
        Ok(wire::PathSizesResponse {
            sizes: rels
                .iter()
                .map(|rel| wire::PathSize {
                    rel: rel.clone(),
                    bytes,
                })
                .collect(),
            truncated: false,
            refusal: String::new(),
        })
    }
    fn project_activity(&self, _root: &Path) -> Result<wire::ProjectActivityResponse, String> {
        Ok(self.state.lock().unwrap().activity.clone())
    }
    fn watch_modes(
        &self,
        _root: &Path,
        _patterns: &[String],
        on: bool,
        staging_name: &str,
    ) -> Result<wire::WatchModesResponse, String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("watch_modes {on} {staging_name}"));
        let mut state = self.state.lock().unwrap();
        if let Some(error) = state.watch_modes_error.clone() {
            return Err(error);
        }
        let settling = state.watch_modes_settling > 0;
        state.watch_modes_settling = state.watch_modes_settling.saturating_sub(1);
        Ok(wire::WatchModesResponse {
            settling,
            ..Default::default()
        })
    }
    fn mode_lines(&self) -> Result<Vec<ModeLine>, String> {
        let state = self.state.lock().unwrap();
        match &state.mode_lines_error {
            Some(error) => Err(error.clone()),
            None => Ok(state.mode_lines.clone()),
        }
    }
    fn changed_host_key(&self) -> Result<Option<String>, String> {
        let state = self.state.lock().unwrap();
        if state.key_unknown {
            return Err("a master start is under way".into());
        }
        Ok(state.shown_key.clone())
    }
}

pub(crate) struct FakeEditor {
    unsaved: Mutex<Option<Vec<PathBuf>>>,
    files: Mutex<Vec<(PathBuf, u64)>>,
}

impl EditorState for Arc<FakeEditor> {
    fn unsaved_under(&self, root: &Path) -> Option<Vec<PathBuf>> {
        self.unsaved
            .lock()
            .unwrap()
            .clone()
            .map(|paths| paths.into_iter().filter(|p| p.starts_with(root)).collect())
    }
    fn saves_since(&self, generation: u64) -> (u64, Vec<PathBuf>) {
        let answer = EditorAnswer {
            unsaved: vec![],
            files: self.files.lock().unwrap().clone(),
        };
        (answer.generation().max(generation), answer.saved_after(generation))
    }
}

pub(crate) struct Lab {
    pub(crate) state: PathBuf,
    pub(crate) root: PathBuf,
    pub(crate) log: Log,
    pub(crate) engine: Arc<FakeEngine>,
    pub(crate) helper: Arc<FakeHelper>,
    pub(crate) editor: Arc<FakeEditor>,
    pub(crate) store: RemoteProjects,
}

const HOST_KEY: &str = "SHA256:example";

impl Lab {
    pub(crate) fn new() -> Self {
        let state = agent::state_dirs::test_workspace_dir("sync-actor-state");
        let root = agent::state_dirs::test_workspace_dir("sync-actor-root");
        let lab = Self::in_dirs(state, root);
        let mut project = RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: HOST_KEY.into(),
            root: lab.root.clone(),
            helper_version: "0.4.0-example".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        };
        // As first connect leaves it: a sync record with no sessions yet, naming the folder it prepared.
        project.set_sync_record(&SyncRecord {
            set_up: true,
            engine: "mutagen".into(),
            host_key_sha256: HOST_KEY.into(),
            root_identity: super::mirror_identity::identity(&lab.root),
            ..Default::default()
        });
        lab.store.save(&project).unwrap();
        lab
    }

    fn in_dirs(state: PathBuf, root: PathBuf) -> Self {
        let log: Log = Arc::default();
        let store = RemoteProjects::new(Some(state.as_os_str()), None);
        Self {
            engine: FakeEngine::new(log.clone(), &root),
            helper: FakeHelper::new(log.clone()),
            editor: Arc::new(FakeEditor {
                unsaved: Mutex::new(Some(vec![])),
                files: Mutex::new(vec![]),
            }),
            state,
            root,
            log,
            store,
        }
    }

    pub(crate) fn project(&self) -> RemoteProject {
        self.store.find(&self.root).unwrap().unwrap()
    }

    pub(crate) fn record(&self) -> SyncRecord {
        self.project().sync_record().unwrap().unwrap_or_default()
    }

    pub(crate) fn update_record(&self, change: impl FnOnce(&mut SyncRecord)) {
        self.store
            .update(&self.root, |project| {
                let mut record = project.sync_record().unwrap().unwrap_or_else(|| SyncRecord {
                    engine: "mutagen".into(),
                    host_key_sha256: HOST_KEY.into(),
                    ..Default::default()
                });
                change(&mut record);
                project.set_sync_record(&record);
            })
            .unwrap();
    }

    fn key(&self) -> SyncKey {
        SyncKey::new(HOST_KEY, &self.root)
    }

    pub(crate) fn tree(&self) -> String {
        session_name(&self.key(), Role::Tree)
    }

    pub(crate) fn git(&self) -> String {
        session_name(&self.key(), Role::Git)
    }

    pub(crate) fn config(&self, kind: HolderKind) -> ActorConfig {
        ActorConfig {
            state_home: self.state.clone(),
            kind,
            tripwire_mib: 64,
            flush_cap: Duration::from_secs(1),
            quiet_for: Duration::from_millis(30),
            recheck_every: Duration::from_secs(3600),
            tick: Duration::from_millis(10),
            poll_every: Duration::from_millis(20),
            hand_over_wait: Duration::from_secs(5),
            list_budget: 10_000,
            local_git: Some(LocalGit::with_environment(
                crate::remote_sync::git_replica::red_team_git_binary(),
                vec![
                    ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
                    ("HOME".into(), self.state.clone().into()),
                ],
            )),
        }
    }

    fn attach_as(&self, kind: HolderKind, engine: Arc<dyn SyncEngine>) -> (SyncActor, Receiver<SyncEvent>) {
        self.attach_with(self.config(kind), engine)
    }

    fn attach_with(&self, config: ActorConfig, engine: Arc<dyn SyncEngine>) -> (SyncActor, Receiver<SyncEvent>) {
        let (events, receiver) = mpsc::channel();
        let actor = attach(
            self.project(),
            Deps {
                engine,
                helper: self.helper.clone(),
                editor: Arc::new(self.editor.clone()),
                events,
            },
            config,
        )
        .unwrap();
        (actor, receiver)
    }

    fn attach(&self) -> (SyncActor, Receiver<SyncEvent>) {
        self.attach_as(HolderKind::Window, self.engine.clone())
    }

    /// A tree session that has synced once, watching.
    pub(crate) fn synced(&self) -> (SyncActor, Receiver<SyncEvent>) {
        let attached = self.attach();
        wait(|| self.engine.status_of(&self.tree()).is_some());
        self.engine.push(self.watching(&self.tree(), 1));
        wait(|| self.record().tree_synced);
        attached
    }

    pub(crate) fn watching(&self, name: &str, cycles: u64) -> SessionStatus {
        let role = if name.ends_with("-git") { Role::Git } else { Role::Tree };
        let mut status = SessionStatus::disconnected(name, role, "x");
        status.state = SyncState::Watching;
        status.last_error = None;
        status.successful_cycles = cycles;
        status
    }

    fn staging(&self, path: Option<&str>, total: u64, expected: u64, cycles: u64) -> SessionStatus {
        let mut status = self.watching(&self.tree(), cycles);
        status.state = SyncState::Staging;
        status.staging = Some(Staging {
            side: Side::Laptop,
            expected_bytes: expected,
            received_bytes: 0,
            total_received_bytes: total,
            received_files: 0,
            expected_files: 1,
            path: path.map(str::to_owned),
        });
        status
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

pub(crate) fn wait(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn lines(events: &Receiver<SyncEvent>) -> Vec<String> {
    events
        .try_iter()
        .filter_map(|event| match event {
            SyncEvent::Line(line) => Some(line),
            SyncEvent::Status(_) => None,
        })
        .collect()
}

fn wait_line(events: &Receiver<SyncEvent>, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(SyncEvent::Line(line)) if line.contains(needle) => return line,
            _ => assert!(Instant::now() < deadline, "no line containing {needle:?}"),
        }
    }
}

#[test]
fn sessions_are_created_tree_first_then_git() {
    let lab = Lab::new();
    let (actor, _events) = lab.attach();
    assert!(actor.is_holder());
    wait(|| lab.engine.status_of(&lab.git()).is_some() && lab.record().git_session.is_some());
    let creates = lab.engine.calls("create");
    assert!(
        creates[0].starts_with(&format!("create {} .git,/target/,.git", lab.tree())),
        "{creates:?}"
    );
    assert!(
        creates[1].starts_with(&format!("create {} config,", lab.git())),
        "{creates:?}"
    );
    // The laptop's own config was there before the replica session existed, and refuses commits.
    assert_eq!(
        lab.engine.state.lock().unwrap().git_config_seen_at_git_create,
        Some(true)
    );
    let config = std::fs::read_to_string(lab.root.join(".git/config")).unwrap();
    assert!(
        config.contains("hookspath") && config.contains("allow = never"),
        "{config}"
    );
    let record = lab.record();
    assert_eq!(record.tree_session, Some(lab.tree()));
    assert_eq!(record.git_session, Some(lab.git()));
    let identifier = lab.engine.status_of(&lab.tree()).unwrap().identifier;
    assert!(!identifier.is_empty());
    let staging = sessions::staging_name(&identifier);
    assert!(
        lab.calls().iter().any(|c| c == &format!("watch_modes true {staging}")),
        "{:?}",
        lab.calls()
    );
}

#[test]
fn a_failed_watch_modes_is_sent_again_at_a_recheck() {
    let lab = Lab::new();
    lab.helper
        .set(|h| h.watch_modes_error = Some("the link dropped".into()));
    let mut config = lab.config(HolderKind::Window);
    config.recheck_every = Duration::from_millis(20);
    let (_actor, _events) = lab.attach_with(config, lab.engine.clone());
    wait(|| lab.record().tree_session.is_some());
    let staging = || {
        let identifier = lab.engine.status_of(&lab.tree()).unwrap().identifier;
        format!("watch_modes true {}", sessions::staging_name(&identifier))
    };
    wait(|| lab.calls().iter().filter(|c| **c == staging()).count() >= 2);
    lab.helper.set(|h| h.watch_modes_error = None);
    wait(|| lab.calls().iter().filter(|c| **c == staging()).count() >= 3);
    let sent = lab.calls().iter().filter(|c| **c == staging()).count();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        lab.calls().iter().filter(|c| **c == staging()).count(),
        sent,
        "a name the helper took is not sent again"
    );
}

#[test]
fn second_window_observes_and_never_acts() {
    let lab = Lab::new();
    let (first, _events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    let other_log: Log = Arc::default();
    let other = FakeEngine::new(other_log.clone(), &lab.root);
    let (second, events) = lab.attach_as(HolderKind::Window, other.clone());
    assert!(first.is_holder());
    assert!(!second.is_holder());
    // The observer passes the holder's status on.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(SyncEvent::Status(status)) = events.recv_timeout(Duration::from_millis(50)) {
            assert_eq!(status.host, "example-host");
            break;
        }
        assert!(Instant::now() < deadline);
    }
    std::thread::sleep(Duration::from_millis(100));
    drop(second);
    assert!(other_log.lock().unwrap().is_empty(), "{:?}", other_log.lock().unwrap());
}

fn status_file_of(port: &SyncPort) -> Result<StatusFile, String> {
    status_file::read_status_file(&port.dir)
}

#[test]
fn observer_window_flushes_too() {
    let lab = Lab::new();
    let (_holder, _events) = lab.synced();
    let (observer, _events2) = lab.attach_as(HolderKind::Window, lab.engine.clone());
    assert!(!observer.is_holder());
    assert_eq!(
        crate::remote_sync::flush::before_send(&observer.port(), &[], Duration::from_secs(1)),
        crate::remote_sync::flush::SendSync::Ready
    );
    assert!(lab.engine.calls("flush").contains(&format!("flush {}", lab.tree())));
}

/// The holder writes `status.json` on its own tick, so after a flush it can lag behind the engine or say what is no
/// longer true: a sender reads the tree session's conflicts from the engine, by name.
#[test]
fn the_send_check_reads_the_engine_and_never_the_status_file() {
    use crate::remote_sync::engine::{Conflict, EntryKind};
    use crate::remote_sync::flush::{before_send, LateWhy, SendSync};
    use crate::remote_sync::status_schema::ConflictEntry;
    let lab = Lab::new();
    let (actor, _events) = lab.synced();
    wait(|| lab.engine.status_of(&lab.git()).is_some());
    lab.engine.push(lab.watching(&lab.git(), 1));
    let port = actor.port();
    wait(|| status_file_of(&port).is_ok_and(|s| s.git_ready));
    let saved = [PathBuf::from("src/main.rs")];

    // A file that says there is a conflict when the engine says there is none.
    let mut stale = status_file_of(&port).unwrap();
    stale.conflicts.push(ConflictEntry {
        path: "src".into(),
        laptop_kind: "file".into(),
        server_kind: "directory".into(),
    });
    status_file::write_status(&port.dir, &stale).unwrap();
    assert_eq!(before_send(&port, &saved, Duration::from_secs(1)), SendSync::Ready);
    assert_eq!(status_file_of(&port).unwrap().conflicts, stale.conflicts);

    // A file that can no longer be written while the engine reports a conflict on the saved path.
    let path = status_file::status_path(&port.dir);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let mut tree = lab.watching(&lab.tree(), 2);
    tree.conflicts.push(Conflict {
        path: "src".into(),
        laptop: EntryKind::File,
        server: EntryKind::Directory,
    });
    lab.engine.push(tree);
    assert_eq!(
        before_send(&port, &saved, Duration::from_secs(1)),
        SendSync::Late {
            files: vec!["src/main.rs".into()],
            why: LateWhy::Conflict
        }
    );
    std::fs::remove_dir(path).unwrap();
}

#[test]
fn repeated_tree_listings_in_the_same_cycle_keep_the_status_file() {
    use std::os::unix::fs::MetadataExt;
    let lab = Lab::new();
    lab.engine.state.lock().unwrap().monitor_fails = true;
    let (actor, _events) = lab.synced();
    wait(|| lab.engine.status_of(&lab.git()).is_some());
    lab.engine.push(lab.watching(&lab.git(), 1));
    wait(|| status_file_of(&actor.port()).is_ok_and(|s| s.git_ready));
    let path = status_file::status_path(&actor.port().dir);
    let held = std::fs::File::open(&path).unwrap();
    let polls = lab.engine.status_reads.load(Ordering::Relaxed);
    wait(|| lab.engine.status_reads.load(Ordering::Relaxed) >= polls + 3);
    assert_eq!(held.metadata().unwrap().ino(), std::fs::metadata(path).unwrap().ino());
}

#[test]
fn holder_reports_changed_status_even_when_status_cannot_be_written() {
    let lab = Lab::new();
    let (actor, events) = lab.synced();
    wait(|| status_file_of(&actor.port()).is_ok());
    let path = status_file::status_path(&actor.port().dir);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let mut tree = lab.watching(&lab.tree(), 2);
    tree.conflicts.push(crate::remote_sync::engine::Conflict {
        path: "src".into(),
        laptop: crate::remote_sync::engine::EntryKind::File,
        server: crate::remote_sync::engine::EntryKind::Directory,
    });
    lab.engine.push(tree);
    wait(|| {
        events
            .try_iter()
            .any(|event| matches!(event, SyncEvent::Status(s) if s.conflicts.len() == 1))
    });
    assert!(status_file_of(&actor.port()).is_err());
    std::fs::remove_dir(path).unwrap();
    wait(|| status_file_of(&actor.port()).is_ok_and(|s| s.conflicts.len() == 1));
}

#[test]
fn watch_hands_the_lock_to_a_window() {
    let lab = Lab::new();
    let locks = lock_dir(&lab.state, &lab.root);
    let (watch, _events) = lab.attach_as(HolderKind::Watch, lab.engine.clone());
    assert!(watch.is_holder());
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    assert!(!window_holds_lock(&locks, std::process::id()));
    let (window, _events2) = lab.attach();
    assert!(window.is_holder());
    wait(|| !watch.is_holder());
    wait(|| window_holds_lock(&locks, std::process::id()));
    assert!(!window_holds_lock(&locks, 0));
    // Handing over pauses nothing.
    assert!(lab.engine.calls("pause").is_empty(), "{:?}", lab.calls());
    // A watch never takes the lock from a window.
    let (second_watch, _events3) = lab.attach_as(HolderKind::Watch, lab.engine.clone());
    assert!(!second_watch.is_holder());
}

#[test]
fn closing_the_last_actor_pauses_the_sessions() {
    let lab = Lab::new();
    let (actor, _events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.git()).is_some());
    drop(actor);
    let pauses = lab.engine.calls("pause");
    assert!(pauses.contains(&format!("pause {}", lab.tree())), "{pauses:?}");
    assert!(pauses.contains(&format!("pause {}", lab.git())), "{pauses:?}");
}

#[test]
fn next_actor_resumes_unless_a_pause_is_recorded() {
    let lab = Lab::new();
    let (actor, _events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.git()).is_some());
    drop(actor);
    let (actor, _events) = lab.attach();
    wait(|| lab.engine.calls("resume").len() == 2);
    assert_eq!(
        lab.engine.calls("create").len(),
        2,
        "existing sessions are resumed, not created"
    );
    drop(actor);
    lab.update_record(|record| {
        record.paused = Some(PauseRecord {
            reason: PAUSE_TRIPWIRE.into(),
            detail: "sync paused: x".into(),
            since_unix_ms: 1,
            path: Some("x".into()),
        })
    });
    let resumes = lab.engine.calls("resume").len();
    let (_actor, events) = lab.attach();
    wait_line(&events, "sync paused: x");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(lab.engine.calls("resume").len(), resumes);
    assert_eq!(lab.engine.status_of(&lab.tree()).unwrap().state, SyncState::Paused);
    let status = status_file::read_status_file(&project_dir(&lab.state, &lab.project())).unwrap();
    assert_eq!(status.phase, "paused");
}

#[test]
fn stop_pauses_and_uninstall_terminates_sessions() {
    let lab = Lab::new();
    let (actor, _events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.git()).is_some());
    drop(actor);
    lab.log.lock().unwrap().clear();
    assert!(sessions::pause_host(lab.engine.as_ref(), &lab.store, HOST_KEY, "stop", "example-host").is_empty());
    assert_eq!(lab.engine.calls("pause").len(), 2);
    assert!(sessions::pause_host(lab.engine.as_ref(), &lab.store, "other-key", "stop", "example-host").is_empty());
    assert_eq!(lab.engine.calls("pause").len(), 2);
    assert!(sessions::terminate_host(lab.engine.as_ref(), &lab.store, HOST_KEY).is_empty());
    assert_eq!(
        lab.engine.calls("terminate"),
        vec![format!("terminate {}", lab.tree()), format!("terminate {}", lab.git())]
    );
    let record = lab.record();
    assert_eq!((record.tree_session, record.git_session), (None, None));
}

#[test]
fn sessions_of_this_root_under_another_key_are_terminated_first() {
    let lab = Lab::new();
    let other = session_name(&SyncKey::new("SHA256:other", &lab.root), Role::Tree);
    lab.engine.insert(&other, Role::Tree, &lab.root, SyncState::Watching);
    let (_actor, events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    let calls = lab.calls();
    let terminated = calls.iter().position(|c| c == &format!("terminate {other}")).unwrap();
    let created = calls.iter().position(|c| c.starts_with("create")).unwrap();
    assert!(terminated < created, "{calls:?}");
    wait_line(&events, "synced");
}

#[test]
fn an_unknown_recorded_session_is_recreated_and_listed() {
    let lab = Lab::new();
    lab.update_record(|record| {
        record.engine = "mutagen".into();
        record.host_key_sha256 = HOST_KEY.into();
        record.tree_session = Some(session_name(&SyncKey::new(HOST_KEY, &lab.root), Role::Tree));
        record.tree_synced = true;
    });
    std::fs::write(lab.root.join("laptop-only.txt"), "x").unwrap();
    lab.helper.set(|h| h.server_files = vec!["server-only.txt".into()]);
    let (_actor, events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    assert!(lab.calls().iter().any(|c| c.starts_with("list_tree")));
    // Mutagen copies each one-sided file across in the new session's first cycle.
    std::fs::write(lab.root.join("server-only.txt"), "x").unwrap();
    lab.helper
        .set(|h| h.server_files = vec!["server-only.txt".into(), "laptop-only.txt".into()]);
    lab.engine.push(lab.watching(&lab.tree(), 1));
    wait(|| !lab.record().copied_back.is_empty());
    assert_eq!(
        lab.record().copied_back,
        vec!["laptop-only.txt".to_owned(), "server-only.txt".to_owned()]
    );
    wait_line(&events, "2 files were copied to the other side");
}

#[test]
fn transport_key_refusal_pauses_and_persists() {
    let lab = Lab::new();
    let (actor, events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    let mut refused = lab.watching(&lab.tree(), 0);
    refused.state = SyncState::Disconnected(format!(
        "eitri host mutagen-transport: the server behind example-host {KEY_REFUSAL}"
    ));
    lab.engine.push(refused);
    let line = wait_line(&events, "is not the one this mirror syncs with");
    assert!(line.contains("sync is paused"), "{line}");
    assert_eq!(lab.record().paused.unwrap().reason, PAUSE_HOST_KEY);
    drop(actor);
    // A record whose sessions run under an older key than the project's waits for the person.
    lab.update_record(|record| {
        record.paused = None;
        record.host_key_sha256 = "SHA256:old".into();
    });
    let (_actor, events) = lab.attach();
    let line = wait_line(&events, "changed its host key from SHA256:old to SHA256:example");
    assert!(line.contains("eitri host sync resume"), "{line}");
    assert_eq!(lab.record().paused.unwrap().reason, PAUSE_HOST_KEY);
    assert_eq!(lab.engine.calls("resume").len(), 0);
}

#[test]
fn a_key_change_under_an_open_window_pauses_sync_with_the_key_line() {
    let lab = Lab::new();
    let mut config = lab.config(HolderKind::Window);
    config.recheck_every = Duration::from_millis(20);
    let (_actor, events) = lab.attach_with(config, lab.engine.clone());
    wait(|| lab.record().tree_session.is_some());
    let tree = lab.tree();
    // The window's link came back to a server whose key ssh accepted and the project was not recorded with.
    lab.helper.set(|h| h.shown_key = Some("SHA256:new".into()));
    let line = wait_line(&events, "changed its host key from SHA256:example to SHA256:new");
    assert!(
        line.contains("sync is paused") && line.contains("eitri host sync resume"),
        "{line}"
    );
    // This window's connection stays on the old key, so a resume here cannot settle the new one.
    assert!(line.contains("open it again"), "{line}");
    let pause = lab.record().paused.unwrap();
    assert_eq!(pause.reason, PAUSE_HOST_KEY);
    assert!(pause.detail.contains("SHA256:new"), "{}", pause.detail);
    wait(|| lab.engine.status_of(&tree).is_some_and(|s| sessions::is_paused(&s)));
    // The record stays under the key it synced with: nothing is settled, re-created or resumed for the new one.
    assert_eq!(lab.record().host_key_sha256, HOST_KEY);
    let resumed = lab.engine.calls("resume").len();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(lab.engine.calls("resume").len(), resumed);
    assert_eq!(lab.record().tree_session.as_deref(), Some(tree.as_str()));
}

#[test]
fn host_key_resume_recreates_under_the_new_key() {
    let lab = Lab::new();
    let old_key = SyncKey::new("SHA256:old", &lab.root);
    let old_tree = session_name(&old_key, Role::Tree);
    lab.engine.insert(&old_tree, Role::Tree, &lab.root, SyncState::Paused);
    lab.update_record(|record| {
        record.engine = "mutagen".into();
        record.host_key_sha256 = "SHA256:old".into();
        record.tree_session = Some(old_tree.clone());
        record.tree_synced = true;
        record.accepted_host_key = Some(HOST_KEY.into());
    });
    let (_actor, _events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    assert!(lab.calls().contains(&format!("terminate {old_tree}")));
    assert!(
        lab.calls().iter().any(|c| c.starts_with("list_tree")),
        "re-created with listings"
    );
    let record = lab.record();
    assert_eq!(record.host_key_sha256, HOST_KEY);
    assert_eq!(record.accepted_host_key, None);
    assert_eq!(record.tree_session, Some(lab.tree()));
}

#[test]
fn tripwire_pauses_and_names_the_new_path() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    lab.engine
        .push(lab.staging(Some("assets/video/a.mov"), 70 * MIB, MIB, 1));
    let line = wait_line(&events, "sync paused:");
    assert!(
        line.starts_with("sync paused: assets on example-host would send 80 MiB to this laptop"),
        "{line}"
    );
    assert!(lab.engine.calls("pause").contains(&format!("pause {}", lab.tree())));
    let pause = lab.record().paused.unwrap();
    assert_eq!(
        (pause.reason.as_str(), pause.path.as_deref()),
        (PAUSE_TRIPWIRE, Some("assets"))
    );
}

#[test]
fn tripwire_fires_on_one_large_modified_file() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    std::fs::write(lab.root.join("big.bin"), "old").unwrap();
    lab.helper.set(|h| h.sizes = 300 * MIB);
    lab.engine.push(lab.staging(Some("big.bin"), MIB, 300 * MIB, 1));
    let line = wait_line(&events, "sync paused:");
    assert!(
        line.starts_with("sync paused: big.bin on example-host would send 300 MiB"),
        "{line}"
    );
}

#[test]
fn tripwire_pause_survives_an_actor_restart() {
    let lab = Lab::new();
    let (actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    drop(actor);
    let resumes = lab.engine.calls("resume").len();
    let (_actor, events) = lab.attach();
    wait_line(&events, "sync paused: assets");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(lab.engine.calls("resume").len(), resumes);
}

#[test]
fn an_exclude_lifts_a_tripwire_pause_recorded_before_the_holder_started() {
    let lab = Lab::new();
    let (actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    drop(actor);
    let (_actor, events) = lab.attach();
    wait_line(&events, "sync paused: assets");
    lab.update_record(|record| record.excludes_added.push("assets".into()));
    wait(|| lab.engine.calls("create").len() == 3);
    assert!(lab.engine.calls("create")[2].contains("/assets"));
    wait(|| lab.record().paused.is_none());
}

#[test]
fn an_exclude_written_while_no_holder_ran_lifts_the_tripwire_pause_at_start() {
    let lab = Lab::new();
    let (actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    drop(actor);
    lab.update_record(|record| record.excludes_added.push("assets".into()));
    let (_actor, _events) = lab.attach();
    wait(|| lab.engine.calls("create").len() == 3);
    assert!(lab.engine.calls("create")[2].contains("/assets"));
    wait(|| lab.record().paused.is_none());
}

#[test]
fn tripwire_allowed_path_does_not_trip_again() {
    let lab = Lab::new();
    lab.update_record(|record| record.tripwire_allowed = vec!["assets".into()]);
    let (_actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    lab.engine.push(lab.watching(&lab.tree(), 2));
    std::thread::sleep(Duration::from_millis(200));
    assert!(lab.record().paused.is_none());
    assert!(lines(&events).iter().all(|l| !l.contains("sync paused")));
}

#[test]
fn allow_lets_the_tripped_path_through_and_resumes() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    let resumes = lab.engine.calls("resume").len();
    // What `eitri host sync allow` writes.
    lab.update_record(|record| {
        record.tripwire_allowed.push("assets".into());
        record.paused = None;
    });
    wait(|| lab.engine.calls("resume").len() > resumes);
}

#[test]
fn first_sync_is_not_tripped() {
    let lab = Lab::new();
    let (_actor, _events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    lab.engine.push(lab.staging(Some("assets/a"), 900 * MIB, 900 * MIB, 0));
    std::thread::sleep(Duration::from_millis(200));
    assert!(lab.record().paused.is_none());
    assert!(lab.engine.calls("pause").is_empty());
}

#[test]
fn a_batch_finished_between_polls_is_reported_afterwards() {
    let lab = Lab::new();
    lab.engine.state.lock().unwrap().monitor_fails = true;
    lab.update_record(|record| record.tree_synced = true);
    let (_actor, events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    let mut before = lab.watching(&lab.tree(), 1);
    before.laptop_bytes = 10 * MIB;
    lab.engine.push(before);
    std::thread::sleep(Duration::from_millis(100));
    let mut after = lab.watching(&lab.tree(), 2);
    after.laptop_bytes = 110 * MIB;
    lab.engine.push(after);
    let line = wait_line(&events, "grew by");
    assert_eq!(line, tripwire::arrived_text("example-host", 100 * MIB));
}

#[test]
fn leave_on_server_records_the_exclude_and_recreates_when_quiet() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    // What `eitri host sync exclude assets` writes: the pause stays until the new session exists.
    lab.update_record(|record| record.excludes_added.push("assets".into()));
    wait(|| lab.engine.calls("create").len() == 3);
    let creates = lab.engine.calls("create");
    assert!(creates[2].contains("/assets"), "{creates:?}");
    assert!(lab.calls().contains(&format!("terminate {}", lab.tree())));
    wait(|| lab.record().paused.is_none());
    // Paused sessions are not flushed.
    assert!(lab.engine.calls("flush").is_empty());
}

#[test]
fn recreate_waits_for_a_running_turn_and_for_unknown_activity() {
    let lab = Lab::new();
    let (_actor, _events) = lab.synced();
    lab.helper.set(|h| h.activity.turns_running = 1);
    lab.update_record(|record| record.excludes_added.push("dist".into()));
    let status_dir = project_dir(&lab.state, &lab.project());
    wait(|| {
        status_file::read_status_file(&status_dir)
            .ok()
            .and_then(|s| s.notice)
            .is_some_and(|n| n.contains("ignore rules changed; applying when idle (waiting for the agent's turn"))
    });
    lab.helper.set(|h| {
        h.activity.turns_running = 0;
        h.activity.unknown = true;
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(lab.engine.calls("terminate").is_empty());
    lab.helper.set(|h| h.activity.unknown = false);
    wait(|| lab.engine.calls("create").len() == 3);
}

#[test]
fn recreate_waits_for_an_unsaved_buffer() {
    let lab = Lab::new();
    let (_actor, _events) = lab.synced();
    *lab.editor.unsaved.lock().unwrap() = Some(vec![lab.root.join("src/lib.rs")]);
    lab.update_record(|record| record.excludes_added.push("dist".into()));
    let status_dir = project_dir(&lab.state, &lab.project());
    wait(|| {
        status_file::read_status_file(&status_dir)
            .ok()
            .and_then(|s| s.notice)
            .is_some_and(|n| n.contains("unsaved buffers"))
    });
    assert!(lab.engine.calls("terminate").is_empty());
    *lab.editor.unsaved.lock().unwrap() = None;
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        lab.engine.calls("terminate").is_empty(),
        "an editor that cannot tell is not quiet"
    );
    *lab.editor.unsaved.lock().unwrap() = Some(vec![]);
    wait(|| lab.engine.calls("create").len() == 3);
}

#[test]
fn recreate_aborts_when_a_save_lands_after_the_quiet_check() {
    let lab = Lab::new();
    let (_actor, _events) = lab.synced();
    let editor = lab.editor.clone();
    let root = lab.root.clone();
    let saves = Arc::new(Mutex::new(0u64));
    let counter = saves.clone();
    // The first listing sees a save land.
    lab.helper.set(move |h| {
        h.on_list = Some(Box::new(move || {
            let mut count = counter.lock().unwrap();
            *count += 1;
            if *count == 1 {
                editor.files.lock().unwrap().push((root.join("a.rs"), u64::MAX / 2));
            }
        }))
    });
    lab.update_record(|record| record.excludes_added.push("dist".into()));
    wait(|| lab.engine.calls("create").len() == 3);
    assert_eq!(
        *saves.lock().unwrap(),
        2,
        "the first attempt went back to waiting for quiet"
    );
}

#[test]
fn recreate_lists_after_terminate_and_persists_copied_back() {
    let lab = Lab::new();
    let (_actor, _events) = lab.synced();
    std::fs::write(lab.root.join("deleted-on-server.txt"), "x").unwrap();
    lab.update_record(|record| record.excludes_added.push("dist".into()));
    wait(|| lab.engine.calls("create").len() == 3);
    let calls = lab.calls();
    let flush = calls.iter().position(|c| c.starts_with("flush")).unwrap();
    let terminate = calls.iter().rposition(|c| c.starts_with("terminate")).unwrap();
    let list = calls.iter().rposition(|c| c.starts_with("list_tree")).unwrap();
    let create = calls.iter().rposition(|c| c.starts_with("create")).unwrap();
    assert!(flush < terminate && terminate < list && list < create, "{calls:?}");
    lab.helper
        .set(|h| h.server_files = vec!["deleted-on-server.txt".into()]);
    lab.engine.push(lab.watching(&lab.tree(), 1));
    wait(|| lab.record().copied_back == vec!["deleted-on-server.txt".to_owned()]);
    wait(|| {
        status_file::read_status_file(&project_dir(&lab.state, &lab.project()))
            .is_ok_and(|s| s.copied_back == vec!["deleted-on-server.txt".to_owned()])
    });
}

#[test]
fn truncated_listing_aborts_recreation_visibly() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    lab.helper.set(|h| h.truncated = true);
    lab.update_record(|record| record.excludes_added.push("dist".into()));
    wait_line(&events, recreate::TOO_MANY_FILES);
    let record = lab.record();
    assert_eq!(record.paused.unwrap().reason, PAUSE_RECREATE);
    assert_eq!(record.tree_session, None, "the old session stays terminated");
    assert_eq!(lab.engine.calls("create").len(), 2);
}

/// While the connection's key cannot be looked at (a master start may be bringing another server's key), a
/// re-creation waits; it runs once the key can be read again.
#[test]
fn a_re_creation_waits_while_the_connections_key_cannot_be_looked_at() {
    let lab = Lab::new();
    let (_actor, _events) = lab.synced();
    lab.helper.set(|h| h.key_unknown = true);
    lab.helper.set(|h| h.patterns = vec!["/target/".into(), "/out/".into()]);
    let mut touched = lab.staging(Some(".gitignore"), 10, 10, 1);
    touched.staging.as_mut().unwrap().side = Side::Server;
    lab.engine.push(touched);
    lab.engine.push(lab.watching(&lab.tree(), 2));
    wait(|| lab.calls().iter().any(|c| c == "sync_ignores"));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        lab.engine.calls("create").len(),
        2,
        "not re-created while the key is unknown"
    );
    assert!(
        lab.engine.calls("terminate").is_empty(),
        "{:?}",
        lab.engine.calls("terminate")
    );
    lab.helper.set(|h| h.key_unknown = false);
    wait(|| lab.engine.calls("create").len() == 3);
}

#[test]
fn ignore_digest_change_marks_pending_and_recreates() {
    let lab = Lab::new();
    let (_actor, _events) = lab.synced();
    lab.helper.set(|h| h.patterns = vec!["/target/".into(), "/out/".into()]);
    // A cycle that staged a .gitignore asks for the ignores again.
    let mut touched = lab.staging(Some(".gitignore"), 10, 10, 1);
    touched.staging.as_mut().unwrap().side = Side::Server;
    lab.engine.push(touched);
    lab.engine.push(lab.watching(&lab.tree(), 2));
    wait(|| lab.engine.calls("create").len() == 3);
    assert!(lab.engine.calls("create")[2].contains("/out/"));
    let expected = digest(&["/target/".to_owned(), "/out/".to_owned()]);
    wait(|| lab.record().ignore_digest == expected);
}

/// Changed patterns are registered before the new session is created, and asked again until the helper's keeper has
/// settled: its first walk must not see the new session's writes.
#[test]
fn a_pattern_change_creates_the_session_once_the_keeper_settled() {
    let lab = Lab::new();
    let (_actor, _events) = lab.synced();
    lab.helper.set(|h| h.watch_modes_settling = 2);
    lab.helper.set(|h| h.patterns = vec!["/target/".into(), "/out/".into()]);
    let mut touched = lab.staging(Some(".gitignore"), 10, 10, 1);
    touched.staging.as_mut().unwrap().side = Side::Server;
    lab.engine.push(touched);
    lab.engine.push(lab.watching(&lab.tree(), 2));
    wait(|| lab.engine.calls("create").len() == 3);
    let calls = lab.calls();
    let create = calls
        .iter()
        .rposition(|c| c.starts_with("create") && c.contains("/out/"))
        .unwrap();
    let before = calls[..create]
        .iter()
        .rev()
        .take_while(|c| !c.starts_with("create"))
        .filter(|c| c.starts_with("watch_modes true"))
        .count();
    assert!(before >= 3, "{calls:?}");
    assert_eq!(lab.helper.state.lock().unwrap().watch_modes_settling, 0);
    // The registration is settled before both trees are listed: the listing and the save check after it are what
    // the session is created from.
    let listed = calls[..create]
        .iter()
        .rposition(|c| c.starts_with("list_tree"))
        .unwrap();
    let registered = calls[..create]
        .iter()
        .rposition(|c| c.starts_with("watch_modes true"))
        .unwrap();
    assert!(registered < listed, "{calls:?}");
}

/// A keeper that never says it settled holds the re-creation back for a bounded number of asks; then the session is
/// created and the person is told.
#[test]
fn a_keeper_that_never_settles_lets_the_session_be_created_and_says_so() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    lab.helper.set(|h| h.watch_modes_settling = 1000);
    lab.helper.set(|h| h.patterns = vec!["/target/".into(), "/out/".into()]);
    let mut touched = lab.staging(Some(".gitignore"), 10, 10, 1);
    touched.staging.as_mut().unwrap().side = Side::Server;
    lab.engine.push(touched);
    lab.engine.push(lab.watching(&lab.tree(), 2));
    wait(|| lab.engine.calls("create").len() == 3);
    let line = wait_line(&events, "the mode keeper was not ready");
    assert!(line.contains("may keep the mode Mutagen gives them"), "{line}");
}

/// A key change seen while a tripwire pause waits: the key pause takes its place, and nothing is re-created.
#[test]
fn a_key_change_under_a_tripwire_pause_pauses_for_the_key_and_recreates_nothing() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    lab.helper.set(|h| h.shown_key = Some("SHA256:new".into()));
    lab.update_record(|record| record.excludes_added.push("assets".into()));
    wait_line(&events, "changed its host key from SHA256:example to SHA256:new");
    assert_eq!(lab.record().paused.unwrap().reason, PAUSE_HOST_KEY);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(lab.engine.calls("create").len(), 2, "nothing re-created");
    assert!(
        lab.engine.calls("terminate").is_empty(),
        "{:?}",
        lab.engine.calls("terminate")
    );
}

#[test]
fn newly_ignored_paths_are_listed_as_kept_here() {
    let lab = Lab::new();
    let (_actor, _events) = lab.synced();
    std::fs::create_dir_all(lab.root.join("dist")).unwrap();
    std::fs::write(lab.root.join("dist/app.js"), "x").unwrap();
    lab.update_record(|record| record.excludes_added.push("dist".into()));
    wait(|| lab.record().kept_here == vec!["dist".to_owned()]);
}

#[test]
fn mode_lines_show_once_per_epoch_and_sequence() {
    let lab = Lab::new();
    let root = lab.root.to_string_lossy().into_owned();
    let counted = |sequence: u64, epoch: u64| ModeLine {
        root: root.clone(),
        text: format!("file modes on srv1: {sequence} sync writes into {root} were left as Mutagen wrote them"),
        sequence,
        epoch,
    };
    let other_root = ModeLine {
        root: "/srv/other".into(),
        text: "file modes on srv1: 9 sync writes into /srv/other were left as Mutagen wrote them".into(),
        sequence: 9,
        epoch: 1,
    };
    lab.helper
        .set(|h| h.mode_lines = vec![counted(2, 7), other_root.clone()]);
    let mut config = lab.config(HolderKind::Window);
    config.recheck_every = Duration::from_millis(20);
    let (_actor, events) = lab.attach_with(config, lab.engine.clone());
    let first = wait_line(&events, "sync writes");
    assert!(
        first.starts_with("file modes on example-host: 2 sync writes"),
        "{first}"
    );
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        lines(&events).iter().all(|l| !l.contains("sync writes")),
        "a repeated line is shown once"
    );
    lab.helper
        .set(|h| h.mode_lines = vec![counted(3, 7), other_root.clone()]);
    assert!(wait_line(&events, "sync writes").contains(": 3 sync writes"));
    lab.helper.set(|h| h.mode_lines = vec![counted(1, 8), other_root]);
    assert!(wait_line(&events, "sync writes").contains(": 1 sync writes"));
    std::thread::sleep(Duration::from_millis(100));
    assert!(lines(&events).iter().all(|l| !l.contains("/srv/other")));
}

/// A Hello whose mode lines pass their bound shows the refusal once; any other failure shows nothing.
#[test]
fn a_refused_mode_lines_answer_shows_its_refusal_once() {
    let lab = Lab::new();
    let refusal = agent::remote_answers::MODE_WARNINGS.past_entries();
    lab.helper
        .set(|h| h.mode_lines_error = Some("status: Unavailable, the link is down".into()));
    let mut config = lab.config(HolderKind::Window);
    config.recheck_every = Duration::from_millis(20);
    let (_actor, events) = lab.attach_with(config, lab.engine.clone());
    std::thread::sleep(Duration::from_millis(200));
    assert!(lines(&events).iter().all(|l| !l.contains("link is down")));
    lab.helper.set(|h| h.mode_lines_error = Some(refusal.clone()));
    assert_eq!(wait_line(&events, "mode lines"), refusal);
    std::thread::sleep(Duration::from_millis(200));
    assert!(lines(&events).iter().all(|l| !l.contains("mode lines")), "shown once");
}

/// A flood of the server's one-time lines is shown and never turns into a refusal that would hide later
/// lines; what the holder remembers of it is bounded (`shown_once`'s tests).
#[test]
fn a_flood_of_server_lines_is_shown_and_hides_nothing() {
    let lab = Lab::new();
    let root = lab.root.to_string_lossy().into_owned();
    let line = |n: usize| ModeLine {
        root: root.clone(),
        text: format!("file modes on srv1: note {n}"),
        sequence: 0,
        epoch: 0,
    };
    lab.helper.set(|h| h.mode_lines = (0..600).map(line).collect());
    let mut config = lab.config(HolderKind::Window);
    config.recheck_every = Duration::from_millis(20);
    let (_actor, events) = lab.attach_with(config, lab.engine.clone());
    wait_line(&events, "note 599");
    std::thread::sleep(Duration::from_millis(100));
    assert!(lines(&events).iter().all(|l| !l.contains("refused")));
}

#[test]
fn a_holder_write_keeps_a_decision_the_person_wrote_meanwhile() {
    let lab = Lab::new();
    let mut config = lab.config(HolderKind::Window);
    // The holder re-reads the record only once a step; a long step leaves the person's write unread.
    config.tick = Duration::from_secs(30);
    let (_actor, _events) = lab.attach_with(config, lab.engine.clone());
    wait(|| lab.engine.status_of(&lab.git()).is_some() && lab.record().git_session.is_some());
    lab.update_record(|record| record.tripwire_allowed.push("keep-me".into()));
    // A session update wakes the holder, which writes `git_ready` before it re-reads.
    lab.engine.push(lab.watching(&lab.git(), 1));
    wait(|| lab.record().git_ready);
    assert_eq!(lab.record().tripwire_allowed, vec!["keep-me".to_owned()]);
}

#[test]
fn resume_after_a_recorded_pause_follows_the_sessions_again() {
    let lab = Lab::new();
    let (actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    drop(actor);
    let (_actor, events) = lab.attach();
    wait_line(&events, "sync paused: assets");
    lab.update_record(|record| {
        record.tripwire_allowed.push("assets".into());
        record.paused = None;
    });
    wait(|| {
        lab.engine
            .status_of(&lab.tree())
            .is_some_and(|s| s.state != SyncState::Paused)
    });
    // The resumed sessions are followed again: a later large batch trips.
    lab.engine.push(lab.staging(Some("video/b"), 70 * MIB, MIB, 2));
    let line = wait_line(&events, "sync paused:");
    assert!(
        line.contains(&format!("sync of {} with example-host waits", lab.root.display())),
        "{line}"
    );
    assert!(line.starts_with("sync paused: video on example-host"), "{line}");
}

#[test]
fn an_ended_monitor_falls_back_to_listing() {
    let lab = Lab::new();
    let (_actor, _events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.git()).is_some() && lab.record().git_session.is_some());
    lab.engine.end_monitors();
    lab.engine.state.lock().unwrap().monitor_fails = true;
    let mut ready = lab.watching(&lab.git(), 1);
    ready.state = SyncState::Watching;
    lab.engine.push(ready);
    wait(|| lab.record().git_ready);
}

#[test]
fn a_recreation_left_unfinished_is_finished_by_the_next_holder() {
    let lab = Lab::new();
    let (actor, _events) = lab.synced();
    std::fs::write(lab.root.join("gap.txt"), "x").unwrap();
    lab.update_record(|record| record.excludes_added.push("dist".into()));
    let dir = project_dir(&lab.state, &lab.project());
    wait(|| recreate::read_progress(&dir).is_some_and(|p| p.one_sided.is_some()));
    // Let go before the new session's first cycle: nothing is recorded yet.
    drop(actor);
    assert!(lab.record().copied_back.is_empty());
    let (_actor, events) = lab.attach();
    lab.helper.set(|h| h.server_files = vec!["gap.txt".into()]);
    wait(|| {
        lab.engine
            .status_of(&lab.tree())
            .is_some_and(|s| s.state != SyncState::Paused)
    });
    lab.engine.push(lab.watching(&lab.tree(), 1));
    wait(|| lab.record().copied_back == vec!["gap.txt".to_owned()]);
    wait_line(&events, "1 files were copied");
    wait(|| recreate::read_progress(&dir).is_none());
}

#[test]
fn resuming_a_too_many_files_pause_lists_again() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    lab.helper.set(|h| h.truncated = true);
    lab.update_record(|record| record.excludes_added.push("dist".into()));
    wait_line(&events, recreate::TOO_MANY_FILES);
    let lists = lab.calls().iter().filter(|c| c.starts_with("list_tree")).count();
    lab.helper.set(|h| h.truncated = false);
    lab.update_record(|record| record.paused = None);
    wait(|| lab.engine.calls("create").len() == 3);
    assert!(lab.calls().iter().filter(|c| c.starts_with("list_tree")).count() > lists);
}

#[test]
fn git_resumes_after_a_tripwire_recreation() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    assert_eq!(lab.engine.status_of(&lab.git()).unwrap().state, SyncState::Paused);
    lab.update_record(|record| record.excludes_added.push("assets".into()));
    wait(|| lab.record().paused.is_none());
    wait(|| {
        lab.engine
            .status_of(&lab.git())
            .is_some_and(|s| s.state != SyncState::Paused)
    });
}

#[test]
fn one_root_has_one_holder_across_a_host_key_change() {
    let lab = Lab::new();
    let (watch, _events) = lab.attach_as(HolderKind::Watch, lab.engine.clone());
    assert!(watch.is_holder());
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    // The window opens after the server's key changed (the record now names the new key).
    lab.store
        .update(&lab.root, |project| project.host_key_sha256 = "SHA256:new".into())
        .unwrap();
    let (window, _events2) = lab.attach();
    assert!(window.is_holder());
    assert!(!watch.is_holder(), "two holders for one root");
}

#[test]
fn letting_go_leaves_a_paused_status() {
    let lab = Lab::new();
    let (actor, _events) = lab.synced();
    let dir = project_dir(&lab.state, &lab.project());
    wait(|| status_file::read_status_file(&dir).is_ok_and(|s| s.phase == "history" || s.phase == "watching"));
    drop(actor);
    let status = status_file::read_status_file(&dir).unwrap();
    assert_eq!(status.phase, "paused");
    assert!(status.notice.unwrap().contains("its sync is paused until one does"));
}

#[test]
fn stop_under_a_live_holder_keeps_the_sessions_paused_until_resumed() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    sessions::pause_host(lab.engine.as_ref(), &lab.store, HOST_KEY, "stop", "example-host");
    wait_line(&events, "sync paused by eitri host stop example-host");
    assert_eq!(lab.record().paused.unwrap().reason, PAUSE_STOPPED);
    let resumes = lab.engine.calls("resume").len();
    lab.update_record(|record| record.paused = None);
    wait(|| lab.engine.calls("resume").len() > resumes);
}

#[test]
fn mode_lines_name_the_host_only_in_their_prefix() {
    assert_eq!(
        name_host("file modes on srv: 2 sync writes into /srv/p were left", "au"),
        "file modes on au: 2 sync writes into /srv/p were left"
    );
    assert_eq!(name_host("something else about srv", "au"), "something else about srv");
}

#[test]
fn a_project_first_connect_never_set_up_gets_no_sessions() {
    let lab = Lab::new();
    lab.store.update(&lab.root, |project| project.sync = None).unwrap();
    let (_actor, events) = lab.attach();
    let line = wait_line(&events, "sync is not set up");
    assert!(line.contains("eitri host open example-host:"), "{line}");
    std::thread::sleep(Duration::from_millis(100));
    assert!(lab.engine.calls("create").is_empty());
    assert!(lab.project().sync.is_none(), "no record is written for it");
}

#[test]
fn an_unrelated_ignore_change_keeps_a_tripwire_pause() {
    let lab = Lab::new();
    let (_actor, events) = lab.synced();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    lab.update_record(|record| record.excludes_added.push("dist".into()));
    wait(|| lab.engine.calls("create").len() == 3);
    std::thread::sleep(Duration::from_millis(100));
    let record = lab.record();
    assert_eq!(
        record.paused.unwrap().reason,
        PAUSE_TRIPWIRE,
        "assets still syncs, so the choice is still open"
    );
    assert_eq!(lab.engine.status_of(&lab.tree()).unwrap().state, SyncState::Paused);
}

#[test]
fn unused_setup_cannot_sync_a_nonempty_first_tree() {
    let lab = Lab::new();
    std::fs::write(lab.root.join("notes"), b"local").unwrap();
    let (actor, events) = lab.attach();
    assert!(wait_line(&events, "no longer empty").contains("eitri host open"));
    drop(actor);
    assert!(!lab.record().has_sessions());
    assert!(lab.engine.calls("create").is_empty());
}

#[test]
fn sessionless_record_without_setup_gets_no_sessions() {
    let lab = Lab::new();
    lab.update_record(|r| r.set_up = false);
    let (_actor, events) = lab.attach();
    assert!(wait_line(&events, "sync is not set up").contains("eitri host open example-host:"));
    assert!(lab.engine.calls("create").is_empty());
    assert!(lab.record().tree_session.is_none() && lab.record().git_session.is_none());
}

#[test]
fn action_in_progress_allows_tripwire_handover_and_final_release() {
    let lab = Lab::new();
    let (watch, events) = lab.attach_as(HolderKind::Watch, lab.engine.clone());
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    lab.engine.push(lab.watching(&lab.tree(), 1));
    wait(|| lab.record().tree_synced);
    let locks = lock_dir(&lab.state, &lab.root);
    let lock = super::super::conflicts::action_lock(&locks).unwrap();
    lab.engine.push(lab.staging(Some("assets/a"), 70 * MIB, MIB, 1));
    wait_line(&events, "sync paused:");
    let (window, _) = lab.attach();
    assert!(window.is_holder());
    let before = lab.engine.calls("pause").len();
    drop(window);
    assert!(lab.engine.calls("pause").len() > before);
    drop(watch);
    drop(lock);
}

/// A tree session lost before its first cycle finished is re-created over the files it already brought: the
/// emptiness check guards only the first create.
#[test]
fn a_lost_first_sync_is_recreated_over_its_partial_mirror() {
    let lab = Lab::new();
    lab.helper.set(|h| h.git_refusal = "not a git repository".into());
    lab.update_record(|record| {
        record.tree_session = Some(lab.tree());
        record.tree_synced = false;
    });
    std::fs::write(lab.root.join("arrived.txt"), b"from the server").unwrap();
    let (_actor, events) = lab.attach();
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    assert!(lines(&events).iter().all(|l| !l.contains("no longer empty")));
    assert_eq!(lab.record().tree_session, Some(lab.tree()));
}

fn fast_rechecks(lab: &Lab) -> ActorConfig {
    ActorConfig {
        recheck_every: Duration::from_millis(50),
        ..lab.config(HolderKind::Window)
    }
}

/// A mirror moved away and another folder put at its path: the sessions' history describes the old folder, so they
/// are never resumed over the new one and its `.git` is not written.
#[test]
fn a_folder_replaced_at_the_root_is_paused_and_nothing_resumes_or_writes_git() {
    let lab = Lab::new();
    drop(lab.synced());
    assert!(lab.record().git_session.is_some());
    let aside = lab.root.with_extension("aside");
    std::fs::rename(&lab.root, &aside).unwrap();
    std::fs::create_dir_all(lab.root.join(".git")).unwrap();
    std::fs::write(lab.root.join("checkout.txt"), b"a fresh clone").unwrap();
    std::fs::write(lab.root.join(".git/config"), b"[core]\n\tbare = false\n").unwrap();
    let before = lab.calls().len();
    let (actor, events) = lab.attach();
    let line = wait_line(&events, "is not the copy Eitri synced");
    assert!(line.contains("eitri host open example-host:"), "{line}");
    wait(|| lab.record().paused.is_some_and(|p| p.reason == PAUSE_FOLDER));
    std::thread::sleep(Duration::from_millis(100));
    drop(actor);
    let after: Vec<String> = lab.calls()[before..].to_vec();
    assert!(
        after
            .iter()
            .all(|c| !c.starts_with("resume") && !c.starts_with("create")),
        "{after:?}"
    );
    assert_eq!(
        std::fs::read(lab.root.join(".git/config")).unwrap(),
        b"[core]\n\tbare = false\n"
    );
    assert_eq!(std::fs::read(lab.root.join("checkout.txt")).unwrap(), b"a fresh clone");
    // `eitri host sync resume` cannot lift it: resuming would sync the sessions over this folder.
    let record = lab.record();
    assert!(
        record.tree_session.is_some(),
        "the sessions stay for first connect to end"
    );
    std::fs::remove_dir_all(&aside).unwrap();
}

/// A record an earlier build wrote names sessions but no folder: nothing tells this folder from another put there
/// since, so the sessions wait for the person to confirm it.
#[test]
fn a_record_without_a_folder_identity_waits_for_eitri_host_open() {
    let lab = Lab::new();
    lab.engine.insert(&lab.tree(), Role::Tree, &lab.root, SyncState::Paused);
    lab.update_record(|record| {
        record.root_identity = None;
        record.tree_session = Some(lab.tree());
        record.tree_synced = true;
    });
    let (_actor, events) = lab.attach();
    assert!(wait_line(&events, "cannot tell whether the folder there").contains("eitri host open example-host:"));
    wait(|| lab.record().paused.is_some_and(|p| p.reason == PAUSE_FOLDER));
    assert!(lab.engine.calls("resume").is_empty() && lab.engine.calls("create").is_empty());
    assert!(!lab.root.join(".git").exists());
}

/// Two recorded mirrors that nest would carry one server's files into the other: the inner or outer one never
/// starts, whatever host the other is on.
#[test]
fn a_root_inside_or_around_another_mirror_never_starts() {
    for outer in [true, false] {
        let lab = Lab::new();
        let other_root = if outer {
            lab.root.parent().unwrap().to_owned()
        } else {
            lab.root.join("inner")
        };
        let mut other = lab.project();
        other.host = "other-host".into();
        other.root = other_root;
        lab.store.save(&other).unwrap();
        let (_actor, events) = lab.attach();
        let line = wait_line(&events, "sync not started:");
        assert!(line.contains("other-host") && line.contains(" is inside "), "{line}");
        assert!(lab.engine.calls("create").is_empty());
    }
}

/// A `.git` the person made while the server had no repository (git commands ran on the server, the tree session
/// leaves `.git` alone): when the server gains one, Eitri neither writes the person's config nor replicates over it.
#[test]
fn a_git_dir_the_person_made_is_never_written_or_replicated() {
    let lab = Lab::new();
    lab.helper.set(|h| h.git_refusal = "not a git repository".into());
    let (_actor, events) = lab.attach_with(fast_rechecks(&lab), lab.engine.clone());
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    wait_line(&events, "not a git repository");
    let git_dir = lab.root.join(".git");
    std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
    std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/mine\n").unwrap();
    std::fs::write(
        git_dir.join("refs/heads/mine"),
        b"0123456789012345678901234567890123456789\n",
    )
    .unwrap();
    lab.helper.set(|h| h.git_refusal.clear());
    let line = wait_line(&events, "has its own .git");
    assert!(line.contains("move it out of"), "{line}");
    assert!(lab.engine.status_of(&lab.git()).is_none());
    assert!(!git_dir.join("config").exists());
    assert!(git_dir.join("refs/heads/mine").exists());
    assert_eq!(lab.record().git_identity, None);
}

/// A replica the server stops allowing ends, and Eitri's `.git` leaves the mirror whole, kept under the state home.
#[test]
fn a_refused_replica_moves_eitris_git_dir_out_of_the_mirror() {
    let lab = Lab::new();
    let (_actor, events) = lab.attach_with(fast_rechecks(&lab), lab.engine.clone());
    wait(|| lab.record().git_session.is_some() && lab.record().git_identity.is_some());
    std::fs::write(lab.root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    lab.helper
        .set(|h| h.git_refusal = "the server's .git holds commondir".into());
    wait_line(&events, "moved the laptop's copy of the git history");
    wait(|| lab.record().git_session.is_none());
    assert!(!lab.root.join(".git").exists());
    let retired: Vec<PathBuf> = std::fs::read_dir(mirror_identity::retired_dir(&lab.state))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(retired.len(), 1);
    assert_eq!(
        std::fs::read(retired[0].join("HEAD")).unwrap(),
        b"ref: refs/heads/main\n"
    );
    assert_eq!(lab.record().git_identity, None);
    assert!(lab
        .engine
        .calls("terminate")
        .contains(&format!("terminate {}", lab.git())));
}

/// Plain git on the copy can start a rebase it can never finish there: on connect Eitri moves its directory out.
#[test]
fn an_interrupted_rebase_on_the_copy_is_moved_out_on_connect() {
    let lab = Lab::new();
    drop(lab.synced());
    let rebase = lab.root.join(".git/rebase-merge");
    std::fs::create_dir_all(&rebase).unwrap();
    std::fs::write(rebase.join("message"), b"what I was writing").unwrap();
    let (_actor, events) = lab.attach();
    let line = wait_line(&events, "moved an interrupted rebase");
    assert!(!rebase.exists(), "{line}");
    let kept: Vec<PathBuf> = std::fs::read_dir(mirror_identity::retired_dir(&lab.state))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        std::fs::read(kept[0].join("rebase-merge/message")).unwrap(),
        b"what I was writing"
    );
    assert!(lab.root.join(".git/config").exists(), "the copy itself stays");
}

/// Mutagen keeps a session's ignores for its life: a git session made from an older description is made again.
#[test]
fn a_git_session_from_an_older_description_is_made_again() {
    let lab = Lab::new();
    drop(lab.synced());
    assert_eq!(lab.record().git_spec_version, git_replica::GIT_SESSION_VERSION);
    lab.update_record(|record| record.git_spec_version = 0);
    let (_actor, _events) = lab.attach();
    wait(|| lab.record().git_spec_version == git_replica::GIT_SESSION_VERSION && lab.record().git_session.is_some());
    assert!(lab
        .engine
        .calls("terminate")
        .contains(&format!("terminate {}", lab.git())));
    let git_creates = lab
        .engine
        .calls("create")
        .into_iter()
        .filter(|c| c.starts_with(&format!("create {}", lab.git())))
        .count();
    assert_eq!(git_creates, 2);
}

/// A holder driven one step at a time, so periodic checks and retries can be exercised independently.
fn holding_runner(lab: &Lab) -> Runner {
    let project = lab.project();
    let dir = project_dir(&lab.state, &project);
    let locks = lock_dir(&lab.state, &lab.root);
    for path in [&dir, &locks] {
        status_file::prepare_dir(&lab.state, path).unwrap();
    }
    let lock = PrivateLock::open(&locks.join("lock")).unwrap();
    assert!(lock.try_lock().unwrap());
    let mut runner = Runner::new(
        project,
        Deps {
            engine: lab.engine.clone(),
            helper: lab.helper.clone(),
            editor: Arc::new(lab.editor.clone()),
            events: mpsc::channel().0,
        },
        lab.config(HolderKind::Window),
        dir,
        locks,
        lock,
        true,
        Arc::new(Shared {
            role: AtomicU8::new(HOLDER),
            stop: AtomicBool::new(false),
        }),
        attachment_token(),
    );
    runner.start_holding();
    runner
}

#[test]
fn ended_tree_monitor_does_not_restart_git_or_trigger_server_rechecks() {
    let lab = Lab::new();
    lab.engine.state.lock().unwrap().ending_tree = true;
    let mut runner = holding_runner(&lab);
    runner.config.poll_every = Duration::from_secs(60);
    runner.hold().monitors.get_mut(&lab.tree()).unwrap().delay = Duration::from_secs(60);
    // Deliver the ended stream before advancing the actor through idle ticks.
    loop {
        let msg = runner.hold().updates.recv_timeout(Duration::from_secs(10)).unwrap();
        if msg.status.is_none() {
            runner.received = Some(msg);
            break;
        }
    }
    for _ in 0..20 {
        runner.act().unwrap();
    }
    assert_eq!(lab.engine.calls("sync_ignores").len(), 1);
    assert_eq!(lab.engine.calls(&format!("monitor {}", lab.git())).len(), 1);
    assert_eq!(lab.engine.calls(&format!("monitor {}", lab.tree())).len(), 1);
    // Advance just the retry deadline. A disconnected first item must not reset the backoff.
    runner.config.recheck_every = Duration::from_secs(300);
    for expected in [120, 240, 300, 300] {
        let tree = lab.tree();
        runner.hold().monitors.get_mut(&tree).unwrap().retry_at = Some(Instant::now());
        runner.retry_monitors();
        let mut messages = vec![];
        loop {
            let message = runner.hold().updates.recv_timeout(Duration::from_secs(10)).unwrap();
            let ended = message.status.is_none();
            messages.push(message);
            if ended {
                break;
            }
        }
        for message in messages {
            runner.received = Some(message);
            runner.act().unwrap();
        }
        let monitor = &runner.hold().monitors[&tree];
        assert!(monitor.retry_at.is_some());
        assert_eq!(monitor.delay, Duration::from_secs(expected));
    }
    assert_eq!(lab.engine.calls(&format!("monitor {}", lab.git())).len(), 1);
    assert_eq!(lab.engine.calls("sync_ignores").len(), 1);
}

#[test]
fn periodic_recheck_recovers_a_dead_daemon_and_a_lost_tree() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    lab.engine.state.lock().unwrap().daemon_down = true;
    runner.recheck();
    assert!(!lab.engine.state.lock().unwrap().daemon_down);
    lab.engine.terminate(&lab.tree()).unwrap();
    runner.recheck();
    assert_eq!(runner.hold().pending.as_ref().map(|p| &p.why), Some(&Why::Lost));
    runner.config.quiet_for = Duration::ZERO;
    runner.try_recreate();
    assert!(lab.engine.status_of(&lab.tree()).is_some());
}

#[test]
fn git_creation_retries_without_a_config_change() {
    let lab = Lab::new();
    lab.engine.state.lock().unwrap().fail_create = Some(Role::Git);
    let mut runner = holding_runner(&lab);
    assert!(lab.record().git_session.is_none());
    runner.recheck();
    assert!(lab.engine.status_of(&lab.git()).is_some());
}

#[test]
fn recreation_records_one_sided_files_before_create_and_discards_a_failed_attempt() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    let dir = runner.dir.clone();
    std::fs::write(lab.root.join("gap.txt"), "x").unwrap();
    lab.engine.state.lock().unwrap().progress_dir = Some(dir.clone());
    runner.set_pending(Why::IgnoresChanged);
    runner.config.quiet_for = Duration::ZERO;
    lab.engine.state.lock().unwrap().fail_create = Some(Role::Tree);
    runner.try_recreate();
    let saved = lab
        .engine
        .state
        .lock()
        .unwrap()
        .progress_at_create
        .take()
        .and_then(|p| p.one_sided);
    assert!(saved.unwrap().0.contains(&"gap.txt".to_owned()));
    assert!(recreate::read_progress(&dir).unwrap().one_sided.is_none());
}

#[test]
fn invalid_recreation_patterns_leave_the_tree_running() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    lab.update_record(|record| record.excludes_added.push("bad\nname".into()));
    runner.config.quiet_for = Duration::ZERO;
    runner.act().unwrap();
    assert!(lab.engine.calls("terminate").is_empty());
    assert!(runner.hold().notice.as_deref().unwrap().contains("invalid"));
}

#[test]
fn resumed_recreation_accounts_for_the_created_tree_before_an_ignore_recheck() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    let dir = runner.dir.clone();
    std::fs::write(lab.root.join("gap.txt"), "x").unwrap();
    runner.config.quiet_for = Duration::ZERO;
    lab.update_record(|r| r.excludes_added.push("build".into()));
    runner.act().unwrap();
    assert!(recreate::read_progress(&dir).unwrap().one_sided.is_some());
    runner.release();
    drop(runner);
    // The process stopped after create, before recording the digest.
    lab.update_record(|r| r.ignore_digest.clear());
    let mut next = holding_runner(&lab);
    assert!(next.hold().pending.is_none());
    assert!(recreate::read_progress(&dir).unwrap().one_sided.is_some());
    lab.helper.set(|h| h.server_files = vec!["gap.txt".into()]);
    next.take_update(lab.watching(&lab.tree(), 1));
    next.finish_first_cycle();
    assert!(lab.record().copied_back.contains(&"gap.txt".into()));
}

/// A git session whose creation failed is made at a later recheck, and that retry still passes the `.git` gate: a
/// repository the person put at the root meanwhile is neither written nor replicated onto.
#[test]
fn a_retried_git_creation_never_writes_a_foreign_git_dir() {
    let lab = Lab::new();
    lab.engine.state.lock().unwrap().fail_create = Some(Role::Git);
    let mut runner = holding_runner(&lab);
    assert!(lab.record().git_session.is_none());
    assert!(
        lab.record().git_identity.is_some(),
        "Eitri made .git before the create that failed"
    );
    let git_dir = lab.root.join(".git");
    // Deleted and made again by the person, and given the old inode number back as ext4 would.
    let old = mirror_identity::identity(&git_dir).unwrap();
    mirror_identity::test_reuse::after_a_clock_tick();
    std::fs::remove_dir_all(&git_dir).unwrap();
    std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
    std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/mine\n").unwrap();
    mirror_identity::test_reuse::report(&git_dir, old.dev, old.ino);
    let git_creates = || {
        lab.engine
            .calls("create")
            .into_iter()
            .filter(|c| c.starts_with(&format!("create {}", lab.git())))
            .count()
    };
    let before = git_creates();
    runner.recheck();
    runner.recheck();
    assert_eq!(git_creates(), before);
    assert!(lab.engine.status_of(&lab.git()).is_none());
    assert!(lab.record().git_session.is_none());
    assert!(!git_dir.join("config").exists());
    assert_eq!(std::fs::read(git_dir.join("HEAD")).unwrap(), b"ref: refs/heads/mine\n");
}

fn runner_paused_for_folder(runner: &mut Runner) -> bool {
    runner
        .hold()
        .record
        .paused
        .as_ref()
        .is_some_and(|pause| pause.reason == PAUSE_FOLDER)
}

fn resumed_or_created(calls: &[String]) -> Vec<String> {
    calls
        .iter()
        .filter(|c| c.starts_with("resume") || c.starts_with("create"))
        .cloned()
        .collect()
}

/// A mirror deleted and made again at the same path (`rm -rf` and a fresh clone) is not the mirror, even when the
/// file system hands the old inode number to the new directory, as ext4 does.
#[test]
fn a_folder_deleted_and_made_again_with_its_inode_back_is_never_resumed() {
    let lab = Lab::new();
    drop(lab.synced());
    let old = mirror_identity::identity(&lab.root).unwrap();
    mirror_identity::test_reuse::after_a_clock_tick();
    std::fs::remove_dir_all(&lab.root).unwrap();
    std::fs::create_dir(&lab.root).unwrap();
    std::fs::write(lab.root.join("clone.txt"), b"a fresh clone").unwrap();
    mirror_identity::test_reuse::report(&lab.root, old.dev, old.ino);
    let before = lab.calls().len();
    let mut runner = holding_runner(&lab);
    let after = resumed_or_created(&lab.calls()[before..]);
    assert!(after.is_empty(), "{after:?}");
    assert!(runner_paused_for_folder(&mut runner));
    assert!(!lab.root.join(".git").exists());
}

fn plain_git(lab: &Lab, args: &[&str]) {
    let out = std::process::Command::new(crate::remote_sync::git_replica::red_team_git_binary())
        .current_dir(&lab.root)
        .env("HOME", &lab.state)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// `rm -rf .git && git init` in the mirror makes the person's repository, whatever inode it gets: Eitri neither
/// writes its config there nor replicates onto it.
#[test]
fn a_git_dir_deleted_and_made_again_with_its_inode_back_is_not_eitris() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    let git_dir = lab.root.join(".git");
    let old = mirror_identity::identity(&git_dir).unwrap();
    assert!(lab.record().git_identity.is_some() && lab.record().git_session.is_some());
    mirror_identity::test_reuse::after_a_clock_tick();
    std::fs::remove_dir_all(&git_dir).unwrap();
    plain_git(&lab, &["init", "-q"]);
    mirror_identity::test_reuse::report(&git_dir, old.dev, old.ino);
    let config = std::fs::read(git_dir.join("config")).unwrap();
    runner.ensure_git();
    assert_eq!(std::fs::read(git_dir.join("config")).unwrap(), config);
    assert!(lab.record().git_session.is_none());
    assert!(lab.engine.status_of(&lab.git()).is_none());
}

/// The folder can be replaced while the holder waits for the server's git config: the tree is never resumed over
/// the replacement, whatever its `.git` holds.
#[test]
fn a_root_replaced_while_the_git_config_is_asked_for_is_never_resumed() {
    let lab = Lab::new();
    drop(lab.synced());
    let root = lab.root.clone();
    let aside = lab.root.with_extension("aside");
    let moved = aside.clone();
    lab.helper.set(move |h| {
        h.on_git_config = Some(Box::new(move || {
            std::fs::rename(&root, &moved).unwrap();
            std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
            std::fs::write(root.join(".git/HEAD"), b"ref: refs/heads/mine\n").unwrap();
        }))
    });
    let before = lab.calls().len();
    let mut runner = holding_runner(&lab);
    let after: Vec<String> = lab.calls()[before..]
        .iter()
        .filter(|c| c.starts_with(&format!("resume {}", lab.tree())))
        .cloned()
        .collect();
    assert!(after.is_empty(), "{after:?}");
    assert!(runner_paused_for_folder(&mut runner));
    std::fs::remove_dir_all(&aside).unwrap();
}

/// A pause Mutagen refused when the folder was found replaced is tried again until the session stops.
#[test]
fn a_folder_pause_that_failed_is_tried_again() {
    let lab = Lab::new();
    let (actor, _events) = lab.synced();
    lab.engine.state.lock().unwrap().pause_fails = true;
    let aside = lab.root.with_extension("aside");
    std::fs::rename(&lab.root, &aside).unwrap();
    std::fs::create_dir(&lab.root).unwrap();
    wait(|| lab.record().paused.is_some_and(|p| p.reason == PAUSE_FOLDER));
    assert_ne!(lab.engine.status_of(&lab.tree()).unwrap().state, SyncState::Paused);
    lab.engine.state.lock().unwrap().pause_fails = false;
    wait(|| lab.engine.status_of(&lab.tree()).unwrap().state == SyncState::Paused);
    drop(actor);
    std::fs::remove_dir_all(&aside).unwrap();
}

/// A `.git` replaced under a running replica (the folder itself unchanged, the server's config unchanged) is seen
/// within a step: the replica stops, and the new repository is left as it is.
#[test]
fn a_git_dir_replaced_under_a_running_replica_stops_the_replica() {
    let lab = Lab::new();
    let (actor, events) = lab.synced();
    wait(|| lab.record().git_session.is_some());
    let git_dir = lab.root.join(".git");
    let aside = lab.root.with_extension("eitri-git");
    std::fs::rename(&git_dir, &aside).unwrap();
    std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
    std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/mine\n").unwrap();
    wait_line(&events, "has its own .git");
    wait(|| lab.engine.status_of(&lab.git()).is_none());
    assert!(!git_dir.join("config").exists());
    drop(actor);
    std::fs::remove_dir_all(&aside).unwrap();
}

/// A git session Mutagen did not terminate stays recorded (so it is still paused on release and stopped later), and
/// Eitri's `.git` is not moved out from under a replica that may still run.
#[test]
fn a_git_session_that_would_not_terminate_stays_recorded_and_is_tried_again() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    assert_eq!(lab.record().git_session, Some(lab.git()));
    lab.engine.state.lock().unwrap().terminate_fails = true;
    lab.helper.set(|h| h.git_refusal = "not a git repository".into());
    runner.ensure_git();
    assert_eq!(lab.record().git_session, Some(lab.git()));
    assert!(lab.root.join(".git").exists());
    lab.engine.state.lock().unwrap().terminate_fails = false;
    runner.ensure_git();
    assert_eq!(lab.record().git_session, None);
    assert!(!lab.root.join(".git").exists());
}

/// A record written without a folder identity keeps waiting for the person even when a new host key is accepted:
/// its sessions are not ended (which would hide that it never confirmed its folder).
#[test]
fn an_accepted_host_key_on_a_record_without_a_folder_identity_waits_for_the_folder() {
    let lab = Lab::new();
    drop(lab.synced());
    lab.store
        .update(&lab.root, |project| project.host_key_sha256 = "SHA256:new".into())
        .unwrap();
    lab.update_record(|record| {
        record.root_identity = None;
        record.accepted_host_key = Some("SHA256:new".into());
    });
    let before = lab.calls().len();
    let mut runner = holding_runner(&lab);
    let terminated: Vec<String> = lab.calls()[before..]
        .iter()
        .filter(|c| c.starts_with("terminate"))
        .cloned()
        .collect();
    assert!(terminated.is_empty(), "{terminated:?}");
    assert!(runner_paused_for_folder(&mut runner));
    assert_eq!(lab.record().tree_session, Some(lab.tree()));
}

/// A git session whose creation keeps failing is tried again less and less often, not at every recheck.
#[test]
fn a_git_session_that_keeps_failing_is_retried_with_backoff() {
    let lab = Lab::new();
    lab.engine.state.lock().unwrap().fail_create_always = Some(Role::Git);
    let mut runner = holding_runner(&lab);
    for _ in 0..5 {
        runner.recheck();
    }
    let git_creates = lab
        .engine
        .calls("create")
        .into_iter()
        .filter(|c| c.starts_with(&format!("create {}", lab.git())))
        .count();
    assert!(git_creates <= 2, "{git_creates} creates");
}

/// When the new ignore list cannot be saved, no tree session runs beside an older list that sends and the git shim
/// would read as its exclusions.
#[test]
fn a_tree_whose_ignore_list_cannot_be_saved_never_runs_beside_the_old_list() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    let dir = runner.dir.clone();
    assert!(recreate::read_created_patterns(&dir).is_some());
    lab.update_record(|record| record.excludes_added.push("build".into()));
    runner.config.quiet_for = Duration::ZERO;
    recreate::test_failures::UNWRITABLE.lock().unwrap().insert(dir.clone());
    runner.act().unwrap();
    recreate::test_failures::UNWRITABLE.lock().unwrap().remove(&dir);
    assert!(
        lab.engine
            .calls("terminate")
            .contains(&format!("terminate {}", lab.tree())),
        "the re-creation did not run"
    );
    let created = lab.engine.status_of(&lab.tree()).is_some();
    let saved = recreate::read_created_patterns(&dir);
    assert!(
        !created || saved.as_ref() == Some(&runner.hold().patterns),
        "a session runs with {:?} while ignores.json says {saved:?}",
        runner.hold().patterns
    );
}

fn replace_git_dir_with_the_persons(lab: &Lab) -> PathBuf {
    let git_dir = lab.root.join(".git");
    let aside = lab
        .root
        .with_extension(format!("eitri-git-{}", uuid::Uuid::new_v4().simple()));
    std::fs::rename(&git_dir, &aside).unwrap();
    std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
    std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/mine\n").unwrap();
    aside
}

/// A `.git` replaced under a running replica: the replica is paused at once, before the server is asked anything,
/// so it does not keep resetting the person's refs through a round trip.
#[test]
fn a_replaced_git_dir_pauses_the_replica_before_the_server_is_asked() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    assert!(lab
        .engine
        .status_of(&lab.git())
        .is_some_and(|s| s.state != SyncState::Paused));
    let aside = replace_git_dir_with_the_persons(&lab);
    let paused = Arc::new(AtomicBool::new(false));
    let (engine, git, seen) = (lab.engine.clone(), lab.git(), paused.clone());
    lab.helper.set(move |h| {
        h.on_git_config = Some(Box::new(move || {
            let state = engine.status_of(&git).map(|s| s.state);
            seen.store(state == Some(SyncState::Paused), Ordering::SeqCst);
        }))
    });
    runner.act().unwrap();
    assert!(
        paused.load(Ordering::SeqCst),
        "the replica ran on while the server was asked"
    );
    std::fs::remove_dir_all(&aside).unwrap();
}

/// A replica whose stop failed and whose pause failed too is paused again once Mutagen answers, though the next
/// stop waits for the backoff.
#[test]
fn a_replaced_git_dir_is_paused_even_while_a_failed_stop_waits() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    let aside = replace_git_dir_with_the_persons(&lab);
    {
        let mut state = lab.engine.state.lock().unwrap();
        state.terminate_fails = true;
        state.pause_fails = true;
    }
    // Two failed stops: the second puts the next try behind the backoff.
    runner.act().unwrap();
    runner.act().unwrap();
    assert!(!runner.git_retry_due());
    assert_ne!(lab.engine.status_of(&lab.git()).unwrap().state, SyncState::Paused);
    lab.engine.state.lock().unwrap().pause_fails = false;
    runner.config.poll_every = Duration::ZERO;
    for _ in 0..3 {
        runner.act().unwrap();
    }
    assert_eq!(lab.engine.status_of(&lab.git()).unwrap().state, SyncState::Paused);
    assert_eq!(
        lab.record().git_session,
        Some(lab.git()),
        "the replica that would not stop stays recorded"
    );
    std::fs::remove_dir_all(&aside).unwrap();
}

/// A laptop git config that keeps failing to write after Eitri made `.git` never retires that empty `.git` again and
/// again, so it never pushes a real retired replica (with what plain git wrote into it) out of the kept ones.
#[test]
fn a_git_config_that_keeps_failing_never_pushes_out_the_real_retired_replica() {
    let lab = Lab::new();
    let mut runner = holding_runner(&lab);
    std::fs::write(lab.root.join(".git/HEAD"), b"ref: refs/heads/stashed-work\n").unwrap();
    lab.helper
        .set(|h| h.git_refusal = "the server's .git holds commondir".into());
    runner.ensure_git();
    let retired = mirror_identity::retired_dir(&lab.state);
    let real: Vec<PathBuf> = std::fs::read_dir(&retired)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(real.len(), 1);
    lab.helper.set(|h| h.git_refusal.clear());
    runner.config.local_git = Some(LocalGit::new("/nonexistent/eitri-test-git"));
    for _ in 0..5 {
        runner.recheck();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        std::fs::read(real[0].join("HEAD")).unwrap(),
        b"ref: refs/heads/stashed-work\n",
        "the real retired replica was removed"
    );
}
