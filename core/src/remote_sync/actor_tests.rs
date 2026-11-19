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
    git_config_seen_at_git_create: Option<bool>,
    root: PathBuf,
}

pub(crate) struct FakeEngine {
    clear_conflicts: Mutex<Option<(PathBuf, Vec<u8>)>>,
    state: Mutex<EngineState>,
    log: Log,
}

impl FakeEngine {
    fn new(log: Log, root: &Path) -> Arc<Self> {
        Arc::new(Self {
            clear_conflicts: Default::default(),
            state: Mutex::new(EngineState {
                sessions: BTreeMap::new(),
                monitors: vec![],
                flush: FlushOutcome::Flushed,
                monitor_fails: false,
                git_config_seen_at_git_create: None,
                root: root.to_owned(),
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

    /// Sets a session's status and sends it to its monitors.
    pub(crate) fn push(&self, status: SessionStatus) {
        let mut state = self.state.lock().unwrap();
        if let Some((current, _)) = state.sessions.get_mut(&status.name) {
            *current = status.clone();
        }
        state
            .monitors
            .retain(|(name, sender)| name != &status.name || sender.send(status.clone()).is_ok());
    }

    fn insert(&self, name: &str, role: Role, root: &Path, state: SyncState) {
        let key = name.trim_start_matches("eitri-").split('-').next().unwrap().to_owned();
        let mut labels = engine::project_labels(&SyncKey::new("x", root), role, root);
        labels.insert("eitri-key".into(), key);
        let mut status = SessionStatus::disconnected(name, role, "x");
        status.state = state;
        status.last_error = None;
        self.state
            .lock()
            .unwrap()
            .sessions
            .insert(name.into(), (status, labels));
    }

    pub(crate) fn clear_when_equal(&self, path: PathBuf, bytes: Vec<u8>) {
        *self.clear_conflicts.lock().unwrap() = Some((path, bytes));
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
    fn create(&self, spec: &SessionSpec) -> Result<(), crate::remote_sync::SyncError> {
        self.log(format!("create {} {}", spec.name, spec.ignores.join(",")));
        let mut state = self.state.lock().unwrap();
        if spec.role == Role::Git {
            let seen = state.root.join(".git/config").exists();
            state.git_config_seen_at_git_create = Some(seen);
        }
        let mut status = SessionStatus::disconnected(&spec.name, spec.role, "x");
        status.state = SyncState::Scanning;
        status.last_error = None;
        state.sessions.insert(spec.name.clone(), (status, spec.labels.clone()));
        Ok(())
    }
    fn terminate(&self, name: &str) -> Result<(), crate::remote_sync::SyncError> {
        self.log(format!("terminate {name}"));
        let mut state = self.state.lock().unwrap();
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
        self.set_state(name, SyncState::Paused)
    }
    fn resume(&self, name: &str) -> Result<(), crate::remote_sync::SyncError> {
        self.log(format!("resume {name}"));
        self.set_state(name, SyncState::Scanning)
    }
    fn status(&self, labels: &Labels) -> Result<Vec<SessionStatus>, crate::remote_sync::SyncError> {
        let state = self.state.lock().unwrap();
        Ok(state
            .sessions
            .values()
            .filter(|(_, have)| labels.iter().all(|(k, v)| have.get(k) == Some(v)))
            .map(|(status, _)| {
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
            })
            .collect())
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
    on_list: Option<Box<dyn Fn() + Send>>,
}

pub(crate) struct FakeHelper {
    pub(crate) files: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    pub(crate) refuse_compare: std::sync::atomic::AtomicBool,
    state: Mutex<HelperState>,
    log: Log,
}

impl FakeHelper {
    fn new(log: Log) -> Arc<Self> {
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
                on_list: None,
            }),
            log,
        })
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
        _patterns: &[String],
        _budget: u64,
    ) -> Result<wire::PathSizesResponse, String> {
        let bytes = self.state.lock().unwrap().sizes;
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
        Ok(wire::WatchModesResponse::default())
    }
    fn mode_lines(&self) -> Result<Vec<ModeLine>, String> {
        Ok(self.state.lock().unwrap().mode_lines.clone())
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
        let log: Log = Arc::default();
        let store = RemoteProjects::new(Some(state.as_os_str()), None);
        let mut project = RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: HOST_KEY.into(),
            root: root.clone(),
            helper_version: "0.4.0-example".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        };
        // As first connect leaves it: a sync record with no sessions yet.
        project.set_sync_record(&SyncRecord {
            engine: "mutagen".into(),
            host_key_sha256: HOST_KEY.into(),
            ..Default::default()
        });
        store.save(&project).unwrap();
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
    assert!(lab
        .calls()
        .iter()
        .any(|c| c == &format!("watch_modes true {}", sessions::staging_name(&lab.key()))));
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

#[test]
fn observer_flush_runs_the_engine_flush() {
    let lab = Lab::new();
    let (_first, _events) = lab.attach();
    let other_log: Log = Arc::default();
    let other = FakeEngine::new(other_log.clone(), &lab.root);
    let (second, _events2) = lab.attach_as(HolderKind::Window, other);
    assert!(!second.is_holder());
    assert_eq!(second.port().flush_tree(Duration::from_secs(1)), FlushOutcome::Flushed);
    assert_eq!(other_log.lock().unwrap().clone(), vec![format!("flush {}", lab.tree())]);
}

#[test]
fn watch_hands_the_lock_to_a_window() {
    let lab = Lab::new();
    let (watch, _events) = lab.attach_as(HolderKind::Watch, lab.engine.clone());
    assert!(watch.is_holder());
    wait(|| lab.engine.status_of(&lab.tree()).is_some());
    let (window, _events2) = lab.attach();
    assert!(window.is_holder());
    wait(|| !watch.is_holder());
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
    assert!(sessions::pause_host(lab.engine.as_ref(), &lab.store, "example-host").is_empty());
    assert_eq!(lab.engine.calls("pause").len(), 2);
    assert!(sessions::pause_host(lab.engine.as_ref(), &lab.store, "other-host").is_empty());
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
    std::fs::write(lab.root.join("big.bin"), "old").unwrap();
    let (_actor, events) = lab.synced();
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

#[test]
fn newly_ignored_paths_are_listed_as_kept_here() {
    let lab = Lab::new();
    std::fs::create_dir_all(lab.root.join("dist")).unwrap();
    std::fs::write(lab.root.join("dist/app.js"), "x").unwrap();
    let (_actor, _events) = lab.synced();
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
    sessions::pause_host(lab.engine.as_ref(), &lab.store, "example-host");
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
