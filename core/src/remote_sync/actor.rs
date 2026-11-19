//! The sync actor: the one process per project that acts on its sync sessions.
//!
//! A window (or `eitri host sync watch --json` when no window holds the project) takes the project's sync lock,
//! `<sync dir>/lock`, an exclusive `flock`. Only the holder creates, pauses, resumes, re-creates or terminates the
//! sessions; everyone else observes `status.json`, which the holder writes on every change. A watch holding the lock
//! gives it up when a window asks (`<sync dir>/handover`), within two seconds, and goes on as an observer; an
//! observer takes the lock when it comes free. When the holder lets go (window closed, watch ended) it pauses both
//! sessions, so sync runs only while someone is looking; the next holder resumes them unless the record holds a pause
//! that waits for the person (a changed host key, the size tripwire, a re-creation that could not list the trees).
//!
//! Everything here blocks (Mutagen, the helper, the disk) and runs on the actor's own thread.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent::private_fs::PrivateLock;
use serde::{Deserialize, Serialize};

use super::engine::{session_name, FlushOutcome, Role, SessionStatus, SyncEngine, SyncState};
use super::ignores::{self, Matcher, Translated};
use super::local_git::LocalGit;
use super::quiet::{self, ActivitySource, EditorReports, EditorState};
use super::recreate::{self, ListError};
use super::sessions::{self, SyncHelper};
use super::status_file::{self, now_unix_ms};
use super::status_schema::StatusFile;
use super::tripwire::{self, Allowances, MIB};
use super::{git_replica, mirror_identity, SyncKey};
use crate::remote_project::{PauseRecord, RemoteProject, RemoteProjects, SyncRecord};

/// Pause reasons kept in `SyncRecord.paused`.
pub const PAUSE_HOST_KEY: &str = "host-key";
pub const PAUSE_TRIPWIRE: &str = "tripwire";
pub const PAUSE_RECREATE: &str = "recreate";
pub const PAUSE_OTHER_SYNC: &str = "other-sync";
/// `eitri host stop <host>` paused the sessions.
pub const PAUSE_STOPPED: &str = "host-stopped";
/// The folder at the root is not the one the sessions synced (or an earlier build noted none): only first connect,
/// which asks before moving anything, sets it up again.
pub const PAUSE_FOLDER: &str = "folder";

/// What the Mutagen transport says when the master's verified key is not the sync record's (Mutagen passes it on as
/// the session's error).
pub const KEY_REFUSAL: &str = "is not the one this mirror syncs with";

/// Who holds the lock: a window gets it from a watch on request; a watch never takes it from a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HolderKind {
    Window,
    Watch,
}

/// What the actor tells its window (or the watch's stdout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    Status(StatusFile),
    /// A line the person must see (a pause, a re-creation's outcome, a mode keeper's line, a failure).
    Line(String),
}

/// Timings and limits; [`ActorConfig::new`] gives the product's.
#[derive(Clone)]
pub struct ActorConfig {
    pub state_home: PathBuf,
    pub kind: HolderKind,
    pub tripwire_mib: u32,
    pub flush_cap: Duration,
    /// No change seen for this long counts towards quiet.
    pub quiet_for: Duration,
    /// How often the holder asks for the server's ignores and mode lines again.
    pub recheck_every: Duration,
    /// The holder's and observer's loop step.
    pub tick: Duration,
    /// Without Mutagen's monitor stream, how often the sessions are listed.
    pub poll_every: Duration,
    /// How long a window waits for a watch to hand the lock over.
    pub hand_over_wait: Duration,
    pub list_budget: u64,
    /// The laptop's git, for writing the replica's config; `None` makes no git session.
    pub local_git: Option<LocalGit>,
}

impl ActorConfig {
    pub fn new(state_home: PathBuf, kind: HolderKind, tripwire_mib: u32, flush_cap: Duration) -> Self {
        let local_git = super::shim_dir::real_git_from_process().ok().map(LocalGit::new);
        Self {
            state_home,
            kind,
            tripwire_mib,
            flush_cap,
            quiet_for: Duration::from_secs(2),
            recheck_every: Duration::from_secs(60),
            tick: Duration::from_millis(500),
            poll_every: Duration::from_secs(2),
            hand_over_wait: Duration::from_secs(3),
            list_budget: recreate::LIST_BUDGET,
            local_git,
        }
    }
}

/// Flushing the tree and reading what the engine says of it, for whoever sends: holder or observer.
pub trait SyncHandle: Send + Sync {
    fn flush_tree(&self, cap: Duration) -> FlushOutcome;
    /// The tree session as the engine reports it now, by name, given up after `cap`. `status.json` is not read: the
    /// holder writes it on its own tick, so it can lag behind a flush that has just returned.
    fn tree_status(&self, cap: Duration) -> Result<SessionStatus, String>;
    fn matcher(&self) -> Result<Matcher, String>;
}

/// The [`SyncHandle`] of one project: the engine's flush and named status, both safe for anyone.
#[derive(Clone)]
pub struct SyncPort {
    engine: Arc<dyn SyncEngine>,
    tree: String,
    git: String,
    dir: PathBuf,
}

impl SyncHandle for SyncPort {
    fn flush_tree(&self, cap: Duration) -> FlushOutcome {
        self.engine.flush(&self.tree, cap)
    }

    fn tree_status(&self, cap: Duration) -> Result<SessionStatus, String> {
        self.engine
            .session_status_within(&self.tree, cap)
            .map_err(|error| error.to_string())
    }

    fn matcher(&self) -> Result<Matcher, String> {
        recreate::read_created_patterns(&self.dir)
            .map(|p| Matcher::new(&p))
            .ok_or_else(|| "the session’s ignore patterns are unavailable".into())
    }
}

/// What the actor works with.
pub struct Deps {
    pub engine: Arc<dyn SyncEngine>,
    pub helper: Arc<dyn SyncHelper>,
    /// The editor this process owns (the window's nvim, the watch's reporting nvim); other processes' editors are
    /// read from their report files.
    pub editor: Arc<dyn EditorState>,
    pub events: Sender<SyncEvent>,
}

const OBSERVER: u8 = 0;
const HOLDER: u8 = 1;

struct Shared {
    role: AtomicU8,
    stop: AtomicBool,
}

/// A process's attachment to a project's sync: holder or observer, changing as the lock moves. Dropping it lets the
/// lock go (pausing the sessions if it held them) and stops its thread.
pub struct SyncActor {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    port: SyncPort,
}

impl SyncActor {
    pub fn is_holder(&self) -> bool {
        self.shared.role.load(Ordering::Acquire) == HOLDER
    }

    pub fn port(&self) -> SyncPort {
        self.port.clone()
    }

    /// Pauses both sessions at once when this attachment holds them: what a shutting-down window does first, so
    /// a slow step on the actor's thread cannot keep them running past the process.
    pub fn pause_now(&self) {
        if self.is_holder() {
            let _ = self.port.engine.pause(&self.port.tree);
            let _ = self.port.engine.pause(&self.port.git);
        }
    }
}

impl Drop for SyncActor {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Serialize, Deserialize)]
struct HolderFile {
    kind: HolderKind,
    pid: u32,
}

/// A token naming one attachment: a hand-over request names the attachment asking, so a holder never takes its own
/// request for another's (two attachments may share a process).
fn attachment_token() -> String {
    format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple())
}

/// The project's sync key: the record's host key (the one its sessions run under) and its root.
pub fn sync_key(project: &RemoteProject) -> SyncKey {
    let host_key = project
        .sync_record()
        .ok()
        .flatten()
        .map(|record| record.host_key_sha256)
        .unwrap_or_else(|| project.host_key_sha256.clone());
    SyncKey::new(&host_key, &project.root)
}

/// The sync directory the project's status lives in (`status.json`, `ignores.json`, `recreate.json`). It follows the
/// project's current host key; a key change waiting for the person keeps its pause in the record, which every holder
/// reads.
pub fn project_dir(state_home: &Path, project: &RemoteProject) -> PathBuf {
    status_file::sync_dir(state_home, &SyncKey::new(&project.host_key_sha256, &project.root))
}

/// The directory of the project's lock, its holder, a hand-over request and the editors' reports:
/// `<state home>/eitri/remote/sync/roots/<16 hex of SHA-256 of the root>`. Keyed by the root alone, so a host-key
/// change can never give one root two holders.
pub fn lock_dir(state_home: &Path, root: &Path) -> PathBuf {
    let labels = super::engine::project_labels(&SyncKey::new("", root), Role::Tree, root);
    state_home.join("eitri/remote/sync/roots").join(&labels["eitri-root"])
}

/// Attaches this process to `project`'s sync: takes the lock when it is free (a window asks a holding watch to hand
/// it over and waits up to `hand_over_wait`), else observes. Blocking: a worker's.
pub fn attach(project: RemoteProject, deps: Deps, config: ActorConfig) -> std::io::Result<SyncActor> {
    let dir = project_dir(&config.state_home, &project);
    status_file::prepare_dir(&config.state_home, &dir)?;
    let locks = lock_dir(&config.state_home, &project.root);
    status_file::prepare_dir(&config.state_home, &locks)?;
    let lock = PrivateLock::open(&locks.join("lock"))?;
    let token = attachment_token();
    let mut holding = lock.try_lock()?;
    if !holding && config.kind == HolderKind::Window && holder_kind(&locks) == Some(HolderKind::Watch) {
        agent::private_fs::write_private(&locks.join("handover"), token.as_bytes())?;
        let deadline = Instant::now() + config.hand_over_wait;
        while !holding && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            holding = lock.try_lock()?;
        }
    }
    let shared = Arc::new(Shared {
        role: AtomicU8::new(if holding { HOLDER } else { OBSERVER }),
        stop: AtomicBool::new(false),
    });
    let key = SyncKey::new(&project.host_key_sha256, &project.root);
    let port = SyncPort {
        engine: deps.engine.clone(),
        tree: session_name(&key, Role::Tree),
        git: session_name(&key, Role::Git),
        dir: dir.clone(),
    };
    let runner = Runner::new(project, deps, config, dir, locks, lock, holding, shared.clone(), token);
    let thread = std::thread::Builder::new()
        .name("eitri-sync-actor".into())
        .spawn(move || runner.run())?;
    Ok(SyncActor {
        shared,
        thread: Some(thread),
        port,
    })
}

/// Lets the project's lock go while keeping its file open, so this process can take it again later.
fn unlock(lock: &PrivateLock) {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor belongs to the open lock file for as long as `lock` lives.
    unsafe {
        libc::flock(lock.as_file().as_raw_fd(), libc::LOCK_UN);
    }
}

fn holder_kind(dir: &Path) -> Option<HolderKind> {
    read_holder(dir).map(|holder| holder.kind)
}

/// The holder record is published by the actor after it takes the root's lock.
pub(crate) fn window_holds_lock(dir: &Path, pid: u32) -> bool {
    read_holder(dir).is_some_and(|holder| holder.kind == HolderKind::Window && holder.pid == pid)
}

fn read_holder(dir: &Path) -> Option<HolderFile> {
    let bytes = agent::private_fs::read_private_nofollow_bytes_capped(&dir.join("holder"), 4096).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Why the tree session is re-created.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Why {
    IgnoresChanged,
    Lost,
    HostKey,
}

impl Why {
    fn as_str(&self) -> &'static str {
        match self {
            Self::IgnoresChanged => "ignores",
            Self::Lost => "lost",
            Self::HostKey => "host-key",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "lost" => Self::Lost,
            "host-key" => Self::HostKey,
            _ => Self::IgnoresChanged,
        }
    }
}

/// A re-creation waiting for quiet, and the old session's patterns it compares the new ones with.
struct Pending {
    why: Why,
}

/// The one-sided files listed before a new session, to compare after its first cycle.
struct AwaitingFirstCycle {
    one_sided: (Vec<String>, Vec<String>),
}

/// A session update from a monitor of generation `epoch`; `status: None` says the stream ended.
struct MonitorMsg {
    name: String,
    epoch: u64,
    status: Option<SessionStatus>,
}

/// An ended stream retains its retry delay until the replacement reports a connected state.
struct MonitorState {
    epoch: u64,
    stop: Arc<AtomicBool>,
    retry_at: Option<Instant>,
    delay: Duration,
}

struct Runner {
    project: RemoteProject,
    deps: Deps,
    config: ActorConfig,
    dir: PathBuf,
    /// [`lock_dir`]: the lock, `holder`, `handover`, `editors/`.
    locks: PathBuf,
    lock: PrivateLock,
    holding: bool,
    shared: Arc<Shared>,
    store: RemoteProjects,
    /// A session update received while waiting, applied first by the next step (never queued again behind later ones).
    received: Option<MonitorMsg>,
    hold: Option<Hold>,
    last_status_seen: Option<StatusFile>,
    last_report: Option<Instant>,
    last_try: Instant,
    handed_over: bool,
    token: String,
}

/// What the holder keeps while it holds.
struct Hold {
    key: SyncKey,
    record: SyncRecord,
    tree: Option<SessionStatus>,
    git: Option<SessionStatus>,
    updates: Receiver<MonitorMsg>,
    sender: Sender<MonitorMsg>,
    monitors: BTreeMap<String, MonitorState>,
    monitor_epoch: u64,
    /// Whether the sessions were set up (or found) and are being followed; until then every recheck tries again.
    started: bool,
    /// The record as the holder last reacted to it: the person's writes are told from this, not from `record`
    /// (which a holder write merges them into).
    seen: SyncRecord,
    last_poll: Instant,
    last_change: Instant,
    last_recheck: Instant,
    recheck_due: bool,
    translated: Option<Translated>,
    git_digest: Option<String>,
    patterns: Vec<String>,
    pending: Option<Pending>,
    awaiting: Option<AwaitingFirstCycle>,
    notice: Option<String>,
    written: Option<StatusFile>,
    modes_shown: BTreeMap<String, (u64, u64)>,
    lines_shown: BTreeSet<String>,
    poll_bytes: Option<(u64, u64)>,
    batch_cycles: Option<u64>,
    /// When the first-cycle accounting may try its listings again after one failed.
    accounting_after: Option<Instant>,
    /// A recorded pause Mutagen did not carry out yet: the sessions are listed and paused again every `poll_every`
    /// until none runs.
    pause_retry: bool,
    last_pause_try: Instant,
    /// When the replica may be tried again after a create or a terminate failed, and how long the next wait is.
    git_retry_at: Option<Instant>,
    git_backoff: Duration,
    /// The git session was paused because the laptop `.git` it writes into moved; it stays paused until that `.git`
    /// is judged Eitri's again (and resumed) or the session is stopped.
    git_held: bool,
    /// The staging name the helper last took for this root (WatchModes answered without a refusal); `None` until
    /// then or after a failed send. A recheck sends again when the tree session's name differs.
    modes_sent: Option<String>,
}

impl Runner {
    fn new(
        project: RemoteProject,
        deps: Deps,
        config: ActorConfig,
        dir: PathBuf,
        locks: PathBuf,
        lock: PrivateLock,
        holding: bool,
        shared: Arc<Shared>,
        token: String,
    ) -> Self {
        let store = RemoteProjects::new(Some(config.state_home.as_os_str()), None);
        Self {
            project,
            deps,
            config,
            dir,
            locks,
            lock,
            holding,
            shared,
            store,
            received: None,
            hold: None,
            last_status_seen: None,
            last_report: None,
            last_try: Instant::now(),
            handed_over: false,
            token,
        }
    }

    fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    fn line(&self, text: impl Into<String>) {
        let _ = self.deps.events.send(SyncEvent::Line(text.into()));
    }

    fn run(mut self) {
        while !self.stopping() {
            if self.holding {
                if self.hold.is_none() {
                    // Starting to hold is a transition: only with the action lock, so no CLI decision is half done.
                    match super::conflicts::try_action_lock(&self.locks) {
                        Ok(Some(_transition)) => self.start_holding(),
                        Ok(None) => {}
                        Err(why) => {
                            self.line(format!("could not open action.lock: {why}"));
                            break;
                        }
                    }
                }
                if let Err(why) = self.act() {
                    self.line(format!("could not open action.lock: {why}"));
                    break;
                }
                if self.config.kind == HolderKind::Watch && self.handover_requested() {
                    self.hand_over();
                }
            } else {
                self.observe();
            }
            if let Some(hold) = &self.hold {
                // Wake for a session update at once; otherwise once per tick.
                let received = hold.updates.recv_timeout(self.config.tick).ok();
                self.received = received;
            } else {
                std::thread::sleep(self.config.tick);
            }
        }
        self.release();
    }

    /// The observer's step: report this process's editor, pass the holder's status on, take the lock when free.
    fn observe(&mut self) {
        // A window that found the lock taken before the watch named itself asks again once it has.
        if self.config.kind == HolderKind::Window
            && holder_kind(&self.locks) == Some(HolderKind::Watch)
            && !self.locks.join("handover").exists()
        {
            let _ = agent::private_fs::write_private(&self.locks.join("handover"), self.token.as_bytes());
        }
        if self.last_report.is_none_or(|at| at.elapsed() >= Duration::from_secs(2)) {
            // Each loaded file is reported at the editor's newest time: the holder only asks whether that grew.
            let answer = self.deps.editor.unsaved_under(&self.project.root).map(|unsaved| {
                let (generation, files) = self.deps.editor.saves_since(0);
                quiet::EditorAnswer {
                    unsaved,
                    files: files.into_iter().map(|path| (path, generation)).collect(),
                }
            });
            let report = quiet::EditorReport {
                pid: std::process::id(),
                written_unix_ms: now_unix_ms(),
                answer,
            };
            let _ = quiet::write_report(&self.config.state_home, &self.locks, &report);
            self.last_report = Some(Instant::now());
        }
        if let Ok(status) = status_file::read_status_file(&self.dir) {
            if self
                .last_status_seen
                .as_ref()
                .is_none_or(|seen| !same_status(seen, &status))
            {
                let _ = self.deps.events.send(SyncEvent::Status(status.clone()));
                self.last_status_seen = Some(status);
            }
        }
        let wait = if self.handed_over {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(2)
        };
        if self.last_try.elapsed() >= wait {
            self.last_try = Instant::now();
            if self.lock.try_lock().unwrap_or(false) {
                self.holding = true;
                self.handed_over = false;
                self.shared.role.store(HOLDER, Ordering::Release);
                quiet::remove_report(&self.locks, std::process::id());
            }
        }
    }

    fn handover_requested(&self) -> bool {
        agent::private_fs::read_private_nofollow_capped(&self.locks.join("handover"), 256)
            .ok()
            .is_some_and(|text| !text.trim().is_empty() && text.trim() != self.token)
    }

    /// A watch hands the lock to a window: the sessions keep running (the window resumes them anyway).
    fn hand_over(&mut self) {
        self.stop_monitors();
        self.hold = None;
        self.received = None;
        let _ = std::fs::remove_file(self.locks.join("holder"));
        let _ = std::fs::remove_file(self.locks.join("handover"));
        // An observer before the lock goes, so this and the window never both say they hold it.
        self.shared.role.store(OBSERVER, Ordering::Release);
        unlock(&self.lock);
        self.holding = false;
        self.handed_over = true;
        self.last_try = Instant::now();
    }

    fn release(&mut self) {
        if self.holding {
            self.stop_monitors();
            // A window can receive hand-over and close while a CLI action still owns the transition lock.
            let record = self
                .hold
                .as_ref()
                .map(|hold| hold.record.clone())
                .or_else(|| self.reread());
            if let Some(record) = record {
                for name in [&record.tree_session, &record.git_session].into_iter().flatten() {
                    if let Err(error) = self.deps.engine.pause(name) {
                        if !self.deps.engine.is_missing(&error) {
                            self.line(format!(
                                "{}: {name} was not paused: {error}",
                                self.project.root.display()
                            ));
                        }
                    }
                }
                let mut status = status_file::compose(
                    &self.project.host,
                    &record,
                    None,
                    None,
                    Some(format!(
                        "no Eitri window or eitri host sync watch follows {}; its sync is paused until one does",
                        self.project.root.display()
                    )),
                    now_unix_ms(),
                );
                status.phase = "paused".into();
                let _ = status_file::write_status(&self.dir, &status);
            }
            let _ = std::fs::remove_file(self.locks.join("holder"));
            unlock(&self.lock);
        } else {
            quiet::remove_report(&self.locks, std::process::id());
        }
    }

    /// Stops following the sessions: monitor threads end at their next update, and what they already queued is dropped.
    fn stop_monitors(&mut self) {
        if let Some(hold) = &mut self.hold {
            for (_, monitor) in std::mem::take(&mut hold.monitors) {
                monitor.stop.store(true, Ordering::Release);
            }
        }
    }

    /// Applies one of the holder's changes to the record as it is on disk now, under the store's lock, so a decision
    /// the person wrote meanwhile (`exclude`, `allow`, `resume`) is kept; then holds the result. A record on disk that
    /// cannot be read is left alone and said.
    fn change(&mut self, change: impl Fn(&mut SyncRecord)) {
        let Some(hold) = &mut self.hold else { return };
        let before = hold.record.clone();
        let caught_up = before == hold.seen;
        change(&mut hold.record);

        let host = self.project.host.clone();
        let mut written = None;
        let mut unreadable = None;
        // Whether the file held only what this holder knew: then nothing of the person's is folded in unseen.
        let mut only_mine = false;
        let result = self.store.update_for_host(&self.project.root, &host, |project| {
            let mut record = match project.sync_record() {
                Ok(Some(record)) => record,
                Ok(None) => {
                    unreadable = Some(project.sync_not_set_up());
                    return;
                }
                Err(why) => {
                    unreadable = Some(why);
                    return;
                }
            };
            only_mine = caught_up && record == before;
            change(&mut record);
            project.set_sync_record(&record);
            written = Some(record);
        });
        match (result, unreadable) {
            (_, Some(why)) => self.line(why),
            (Ok(project), None) => {
                self.project = project;
                if let Some(record) = written {
                    let hold = self.hold();
                    if only_mine {
                        hold.seen = record.clone();
                    }
                    hold.record = record;
                }
            }
            (Err(error), None) => self.line(format!(
                "the sync record of {} was not saved: {error}",
                self.project.root.display()
            )),
        }
    }

    /// Reads the record again (the CLI changes it: `allow`, `exclude`, `include`, `resume`).
    fn reread(&mut self) -> Option<SyncRecord> {
        let project = self.store.find(&self.project.root).ok().flatten()?;
        if project.host != self.project.host {
            return None;
        }
        let record = project.sync_record().ok().flatten();
        self.project = project;
        record
    }

    fn start_holding(&mut self) {
        super::conflicts::sweep(&self.config.state_home);
        let _ = agent::private_fs::write_private(
            &self.locks.join("holder"),
            &serde_json::to_vec(&HolderFile {
                kind: self.config.kind,
                pid: std::process::id(),
            })
            .unwrap_or_default(),
        );
        if self.config.kind == HolderKind::Window {
            let _ = std::fs::remove_file(self.locks.join("handover"));
        }
        let record = self.reread().unwrap_or_else(|| SyncRecord {
            engine: "mutagen".into(),
            host_key_sha256: self.project.host_key_sha256.clone(),
            ..Default::default()
        });
        let (sender, updates) = mpsc::channel();
        self.hold = Some(Hold {
            key: SyncKey::new(&record.host_key_sha256, &self.project.root),
            seen: record.clone(),
            record,
            tree: None,
            git: None,
            updates,
            sender,
            monitors: BTreeMap::new(),
            monitor_epoch: 0,
            started: false,
            last_poll: Instant::now(),
            last_change: Instant::now(),
            last_recheck: Instant::now(),
            recheck_due: false,
            translated: None,
            git_digest: None,
            patterns: vec![],
            pending: None,
            awaiting: None,
            notice: None,
            written: None,
            modes_shown: BTreeMap::new(),
            lines_shown: BTreeSet::new(),
            poll_bytes: None,
            batch_cycles: None,
            accounting_after: None,
            pause_retry: false,
            last_pause_try: Instant::now(),
            git_retry_at: None,
            git_backoff: Duration::ZERO,
            git_held: false,
            modes_sent: None,
        });
        if self.hold().record.authorized() {
            self.settle_host_key();
        }
        if self.hold().record.authorized() && self.hold().record.paused.is_some() {
            self.hold_paused();
            // An exclude written while no holder ran is a changed digest only this look can see.
            self.check_digest();
        } else {
            self.ensure_sessions();
        }
        self.publish();
    }

    fn hold(&mut self) -> &mut Hold {
        self.hold.as_mut().expect("held")
    }

    /// A record whose sessions run under another host key than the project's: the person resumes it (and the sessions
    /// are re-created under the new key), or it waits paused.
    fn settle_host_key(&mut self) {
        let current = self.project.host_key_sha256.clone();
        let recorded = self.hold().record.host_key_sha256.clone();
        if recorded == current {
            return;
        }
        if self.hold().record.accepted_host_key.as_deref() == Some(current.as_str()) {
            // Ending the sessions would erase what says the folder was never confirmed: the folder comes first.
            if self.folder_refused() {
                return;
            }
            let old = self.hold().record.clone();
            for name in [&old.tree_session, &old.git_session].into_iter().flatten() {
                let _ = self.deps.engine.terminate(name);
            }
            self.change(|record| {
                record.host_key_sha256 = current.clone();
                record.accepted_host_key = None;
                record.tree_session = None;
                record.git_session = None;
                record.git_ready = false;
                record.paused = None;
            });
            let root = self.project.root.clone();
            let hold = self.hold();
            hold.key = SyncKey::new(&current, &root);
            hold.tree = None;
            hold.git = None;
            // The tree comes back through a re-creation, so what the gap changed is listed.
            if old.tree_session.is_some() {
                self.set_pending(Why::HostKey);
            }
            return;
        }
        if self
            .hold()
            .record
            .paused
            .as_ref()
            .is_some_and(|pause| pause.reason == PAUSE_HOST_KEY)
        {
            return;
        }
        let text = host_key_text(&self.project.host, &recorded, &current, &self.project.root);
        self.pause_with(PAUSE_HOST_KEY, text, None);
    }

    /// The connection under the helper now shows a host key ssh accepted and the project was not recorded with (a
    /// window's link that came back to another server): sync pauses with the key line, as in a window opened after
    /// the change. This window's connection stays pinned to the old key, so nothing it holds is moved to the new one;
    /// a window opened again takes the new key and its resume settles it. `Paused` when it paused now; `Unknown`
    /// when the key could not be looked at, which only a re-creation waits on (the connection's channels are
    /// refused on another key whatever is looked at here).
    fn key_moved(&mut self) -> KeyLook {
        // A tripwire pause still lets a re-creation run; the key pause takes its place. Any other pause holds.
        let other_pause = self
            .hold()
            .record
            .paused
            .as_ref()
            .is_some_and(|pause| pause.reason != PAUSE_TRIPWIRE);
        if other_pause || !self.hold().record.authorized() {
            return KeyLook::Same;
        }
        let shown = match self.deps.helper.changed_host_key() {
            Ok(Some(shown)) => shown,
            Ok(None) => return KeyLook::Same,
            Err(_) => return KeyLook::Unknown,
        };
        let text = key_moved_text(
            &self.project.host,
            &self.project.host_key_sha256,
            &shown,
            &self.project.root,
        );
        self.pause_with(PAUSE_HOST_KEY, text, None);
        KeyLook::Paused
    }

    /// Records a pause that waits for the person, pauses both sessions and says so.
    fn pause_with(&mut self, reason: &str, text: String, path: Option<String>) {
        let names: Vec<String> = {
            let record = &self.hold().record;
            [&record.tree_session, &record.git_session]
                .into_iter()
                .flatten()
                .cloned()
                .collect()
        };
        let mut failed = Vec::new();
        for name in names {
            if let Err(error) = self.deps.engine.pause(&name) {
                if !self.deps.engine.is_missing(&error) {
                    failed.push(format!("{name}: {error}"));
                }
            }
        }
        if !failed.is_empty() {
            // The pause is recorded anyway; the sessions are listed and paused again until none runs.
            self.hold().pause_retry = true;
            self.line(format!(
                "sync could not pause yet ({}); it keeps trying",
                failed.join("; ")
            ));
        }
        let pause = PauseRecord {
            reason: reason.into(),
            detail: text.clone(),
            since_unix_ms: now_unix_ms(),
            path,
        };
        self.change(|record| record.paused = Some(pause.clone()));
        self.line(text);
    }

    /// While a pause waits for the person: the sessions stay paused.
    fn hold_paused(&mut self) {
        let names: Vec<String> = {
            let record = &self.hold().record;
            [&record.tree_session, &record.git_session]
                .into_iter()
                .flatten()
                .cloned()
                .collect()
        };
        for name in names {
            if let Err(error) = self.deps.engine.pause(&name) {
                if !self.deps.engine.is_missing(&error) {
                    self.hold().pause_retry = true;
                    self.line(format!(
                        "sync for {} could not be paused: {error}",
                        self.project.root.display()
                    ));
                }
            }
        }
        if let Some(pause) = self.hold().record.paused.clone() {
            self.line_once(pause.detail);
        }
    }

    fn line_once(&mut self, text: String) {
        if self.hold().lines_shown.insert(text.clone()) {
            self.line(text);
        }
    }

    /// Fetches the server's ignores; `None` (with the notice set) when sync cannot start.
    fn fetch_ignores(&mut self) -> Option<Translated> {
        let root = self.project.root.clone();
        match Translated::from_response(self.deps.helper.sync_ignores(&root)) {
            Ok(translated) => {
                for problem in &translated.problems {
                    let text = format!("sync on {}: {problem}", self.project.host);
                    self.line_once(text);
                }
                Some(translated)
            }
            Err(error) => {
                let text = error.to_string();
                self.hold().notice = Some(text.clone());
                self.line_once(text);
                None
            }
        }
    }

    /// Creates or resumes the project's sessions: other keys' sessions of this root first, then tree, then git.
    /// Marks a re-creation as due, here and in `recreate.json`, so the next holder finishes it if this one lets go.
    fn set_pending(&mut self, why: Why) {
        self.hold().awaiting = None;
        let progress = recreate::Progress {
            why: why.as_str().into(),
            one_sided: None,
        };
        if let Err(error) = recreate::write_progress(&self.dir, &progress) {
            self.line(format!("sync could not note its re-creation: {error}"));
        }
        self.hold().pending = Some(Pending { why });
    }

    fn ensure_sessions(&mut self) {
        let root = self.project.root.clone();
        let key = self.hold().key.clone();
        // Sessions exist only for a project first connect set up (it checks what the laptop path holds first).
        let latest = self.reread();
        let Some(latest) = latest.filter(SyncRecord::authorized) else {
            let text = self.project.sync_not_set_up();
            self.hold().notice = Some(text.clone());
            self.line_once(text);
            return;
        };
        self.hold().record = latest;
        if self.key_moved() == KeyLook::Paused {
            return;
        }
        if let Some(why) = state_inside(&self.config.state_home, &root) {
            self.hold().notice = Some(why.clone());
            self.line_once(why);
            return;
        }
        match mirror_identity::overlapping(&self.store, &root) {
            Ok(None) => {}
            Ok(Some(why)) | Err(why) => {
                self.hold().notice = Some(why.clone());
                self.line_once(why);
                return;
            }
        }
        if self.folder_refused() {
            return;
        }
        if let Err(error) = self.deps.engine.start() {
            let text = format!("sync stopped: {error}");
            self.hold().notice = Some(text.clone());
            self.line_once(text);
            return;
        }
        // A re-creation an earlier holder left unfinished is finished here.
        if self.hold().pending.is_none() && self.hold().awaiting.is_none() {
            match recreate::read_progress(&self.dir) {
                Some(recreate::Progress {
                    one_sided: Some(one_sided),
                    ..
                }) if self.hold().record.tree_session.is_some() => {
                    self.hold().awaiting = Some(AwaitingFirstCycle { one_sided });
                }
                Some(progress) => {
                    self.hold().pending = Some(Pending {
                        why: Why::parse(&progress.why),
                    })
                }
                None => {}
            }
        }
        match sessions::terminate_other_keys(self.deps.engine.as_ref(), &key, &root) {
            Ok(terminated) => {
                for name in terminated {
                    self.line(format!(
                        "stopped sync session {name}: it synced {} with another server",
                        root.display()
                    ));
                }
            }
            Err(error) => {
                let text = format!("sync stopped: {error}");
                self.hold().notice = Some(text.clone());
                self.line_once(text);
                return;
            }
        }
        let Some(translated) = self.fetch_ignores() else { return };
        let patterns = ignores::effective(&translated, &self.hold().record);
        let digest = digest(&patterns);
        self.hold().translated = Some(translated);
        let tree_name = session_name(&key, Role::Tree);
        let recorded = self.hold().record.tree_session.clone();
        if self.hold().pending.is_none() {
            match recorded {
                Some(name) => match sessions::find(self.deps.engine.as_ref(), &root, &name) {
                    Ok(Some(status)) => {
                        // An equal digest means the session runs with these very patterns.
                        self.hold().patterns = if self.hold().record.ignore_digest == digest {
                            patterns.clone()
                        } else {
                            recreate::read_created_patterns(&self.dir).unwrap_or_else(|| patterns.clone())
                        };
                        self.check_digest();
                        // Resumed (it syncs with its own ignores until any re-creation) once its monitor runs.
                        self.hold().tree = Some(status);
                    }
                    Ok(None) => self.set_pending(Why::Lost),
                    Err(error) => {
                        let text = format!("sync stopped: {error}");
                        self.hold().notice = Some(text.clone());
                        self.line_once(text);
                        return;
                    }
                },
                None if !self.hold().record.set_up => {
                    self.line_once(self.project.sync_not_set_up());
                }
                None => {
                    if !self.create_tree(&tree_name, &patterns, &digest) {
                        return;
                    }
                }
            }
        }
        self.send_watch_modes();
        self.ensure_git();
        // Looked at again right before the resume: the folder can be replaced while the replica is set up.
        if self.folder_paused() || self.folder_refused() {
            return;
        }
        self.start_monitors();
        // Resumed only now, with its monitor running, so a batch that was waiting is seen by the size tripwire.
        if let Some(name) = self.hold().record.tree_session.clone() {
            if let Ok(Some(status)) = sessions::find(self.deps.engine.as_ref(), &root, &name) {
                if sessions::is_paused(&status) {
                    if let Err(error) = self.deps.engine.resume(&name) {
                        self.line(format!("sync could not resume: {error}"));
                    }
                }
            }
        }
        self.show_mode_lines();
        self.hold().notice = None;
        self.hold().started = true;
    }

    fn reserve_session(&mut self, role: Role, name: &str) -> bool {
        let before = self.hold().record.clone();
        // A re-creation (pending while it creates) replaces a session that already ran over this mirror.
        let recreating = self.hold().pending.is_some();
        let mut only_mine = false;
        let result = self.store.update_checked(&self.project.root, |project| {
            if !project.same_identity(&self.project) {
                return Err(std::io::Error::other("the project now points at another server"));
            }
            let refused = || std::io::Error::other(project.sync_not_set_up());
            let mut record = project
                .sync_record()
                .map_err(std::io::Error::other)?
                .ok_or_else(refused)?;
            only_mine = record == before;
            // A sessionless mirror was empty when first connect prepared it. A later holder
            // cannot treat files that appeared since then as an approved first baseline.
            if role == Role::Tree
                && !recreating
                && !record.tree_synced
                && !record.has_sessions()
                && std::fs::read_dir(&project.root)?.next().transpose()?.is_some()
            {
                return Err(std::io::Error::other(format!(
                    "{} is no longer empty; run eitri host open again",
                    project.root.display()
                )));
            }
            // The folder the first session syncs is the one every later session must find there.
            if role == Role::Tree && record.root_identity.is_none() && !record.has_sessions() {
                record.root_identity = mirror_identity::identity(&project.root);
            }
            if record.root_identity.is_some_and(|recorded| {
                mirror_identity::identity(&project.root)
                    .is_none_or(|found| mirror_identity::compare(&recorded, &found) != mirror_identity::Verdict::Same)
            }) {
                return Err(std::io::Error::other(mirror_identity::replaced_text(
                    &project.host,
                    &project.root,
                )));
            }
            let session = match role {
                Role::Tree => &mut record.tree_session,
                Role::Git => &mut record.git_session,
            };
            if record.host_key_sha256 != project.host_key_sha256 || (!record.set_up && session.as_deref() != Some(name))
            {
                return Err(refused());
            }
            *session = Some(name.into());
            project.set_sync_record(&record);
            Ok(())
        });
        match result {
            Ok(project) => {
                let record = project.sync_record().expect("just serialized").expect("just saved");
                if only_mine {
                    self.hold().seen = record.clone();
                }
                self.hold().record = record;
                true
            }
            Err(error) => {
                self.hold().notice = Some(error.to_string());
                self.line_once(error.to_string());
                false
            }
        }
    }

    fn create_tree(&mut self, name: &str, patterns: &[String], digest: &str) -> bool {
        if self.folder_refused() {
            return false;
        }
        let key = self.hold().key.clone();
        let spec = sessions::tree_spec(&key, &self.project.root, patterns);
        // Recorded first: the transport connects only for a session the record names. A holder that stops before
        // the create finishes leaves a name Mutagen may not know, which the next holder re-creates as lost.
        if !self.reserve_session(Role::Tree, name) {
            return false;
        }
        // The send path and the git shim read the session's exclusions from `ignores.json`: it is written before the
        // session exists, and a session never runs beside an older list. An unwritable list is removed (they then
        // refuse as unknown) and no session is made.
        if let Err(error) = recreate::write_created_patterns(&self.dir, patterns) {
            let _ = std::fs::remove_file(self.dir.join("ignores.json"));
            self.change(|record| record.tree_session = None);
            let text = format!("sync not started: the session's ignore list could not be saved ({error})");
            self.hold().notice = Some(text.clone());
            self.line_once(text);
            return false;
        }
        if let Err(error) = self.deps.engine.create(&spec) {
            self.change(|record| record.tree_session = None);
            let text = format!("sync not started: {error}");
            self.hold().notice = Some(text.clone());
            self.line(text);
            return false;
        }
        let hold = self.hold();
        hold.patterns = patterns.to_vec();
        hold.notice = None;
        self.change(|record| record.ignore_digest = digest.to_owned());
        true
    }

    fn send_watch_modes(&mut self) {
        let patterns = self.hold().patterns.clone();
        let root = self.project.root.clone();
        // The tree session's identifier names its staging directory; a session made just now is looked up for it.
        let running = self.hold().tree.as_ref().map(|status| status.identifier.clone());
        let identifier = match (running, self.hold().record.tree_session.clone()) {
            (Some(identifier), _) => identifier,
            (None, Some(name)) => sessions::find(self.deps.engine.as_ref(), &root, &name)
                .ok()
                .flatten()
                .map(|status| status.identifier)
                .unwrap_or_default(),
            (None, None) => String::new(),
        };
        // With the identifier unknown for a moment, the last name stays registered rather than none.
        let staging = match sessions::staging_name(&identifier) {
            name if name.is_empty() => self.hold().modes_sent.clone().unwrap_or_default(),
            name => name,
        };
        self.register_modes(&patterns, staging);
    }

    /// One WatchModes; `modes_sent` keeps the staging name the helper took, `None` after a refusal or a failure.
    fn register_modes(&mut self, patterns: &[String], staging: String) -> Registered {
        let root = self.project.root.clone();
        let (sent, registered) = match self.deps.helper.watch_modes(&root, patterns, true, &staging) {
            Ok(response) if !response.refusal.is_empty() => {
                self.line_once(response.refusal);
                (None, Registered::Not)
            }
            Ok(response) if response.settling => (Some(staging), Registered::Settling),
            Ok(_) => (Some(staging), Registered::Settled),
            Err(error) => {
                self.line_once(format!("file modes on {} are not kept: {error}", self.project.host));
                (None, Registered::Not)
            }
        };
        self.hold().modes_sent = sent;
        registered
    }

    /// Whether the folder at the root is not the one the record's sessions sync: then the sessions are paused, the
    /// pause recorded and said, and nothing is resumed, created or written into `.git`.
    fn folder_refused(&mut self) -> bool {
        let record = self.hold().record.clone();
        if !record.authorized() {
            return false;
        }
        let Some(text) = mirror_identity::root_refusal(&record, &self.project.host, &self.project.root) else {
            return false;
        };
        // The monitors stay: a session whose pause failed is seen running and paused again.
        let paused = self.folder_paused();
        if paused {
            self.hold_paused();
        } else {
            self.pause_with(PAUSE_FOLDER, text.clone(), None);
        }
        let hold = self.hold();
        hold.notice = Some(text);
        hold.started = false;
        true
    }

    /// Whether the sessions wait paused because the folder at the root is not the one they synced.
    fn folder_paused(&mut self) -> bool {
        self.hold()
            .record
            .paused
            .as_ref()
            .is_some_and(|pause| pause.reason == PAUSE_FOLDER)
    }

    /// The `.git` replica: the laptop's config written first, then the session. A root whose git commands must run on
    /// the server gets none (and loses one it had). Only a `.git` Eitri made is written to or replicated onto.
    fn ensure_git(&mut self) {
        if !self.hold().record.set_up && self.hold().record.git_session.is_none() {
            self.line_once(self.project.sync_not_set_up());
            return;
        }
        if self.folder_refused() {
            return;
        }
        let root = self.project.root.clone();
        let key = self.hold().key.clone();
        let Some(local_git) = self.config.local_git.clone() else {
            self.end_replica(GitRefusal::Here("git was not found on this machine".into()));
            return;
        };
        let response = match self.deps.helper.git_config(&root) {
            Ok(response) if response.refusal.is_empty() => response,
            Ok(response) => return self.end_replica(GitRefusal::Here(response.refusal)),
            Err(error) => return self.end_replica(GitRefusal::Unreachable(error)),
        };
        let unwritable =
            |error: std::io::Error| GitRefusal::Here(format!("the laptop's git config could not be written: {error}"));
        let hooks = match git_replica::install_refuse_hooks(&self.config.state_home) {
            Ok(hooks) => hooks,
            Err(error) => return self.end_replica(unwritable(error)),
        };
        // The server's answer took a round trip: the folder is looked at again before its `.git` is judged.
        if self.folder_refused() {
            return;
        }
        let record = self.hold().record.clone();
        let git_dir = root.join(".git");
        let identity = match mirror_identity::git_dir(&local_git, &root, &record, &hooks) {
            Err(error) => return self.end_replica(unwritable(error)),
            Ok(mirror_identity::GitDir::Foreign) => {
                // The person's own repository: no config is written into it and no replica deletes what it holds. A
                // replica that would not stop stays recorded and is stopped at a later step.
                if !self.stop_git_session() {
                    return;
                }
                self.line_once(format!(
                    "git history is not mirrored (the laptop copy has its own .git; move it out of {} to mirror the \
                     server's history); git commands run on {}",
                    root.display(),
                    self.project.host
                ));
                return;
            }
            Ok(mirror_identity::GitDir::Ours(identity)) => {
                match mirror_identity::retire_operations(&self.config.state_home, &key, &root) {
                    Ok(Some(kept)) => self.line(format!(
                        "moved an interrupted rebase or cherry-pick out of {} to {}: the copy can neither continue \
                         nor abort it",
                        git_dir.display(),
                        kept.display()
                    )),
                    Ok(None) => {}
                    Err(error) => self.line(format!(
                        "an interrupted rebase or cherry-pick in {} could not be moved out: {error}",
                        git_dir.display()
                    )),
                }
                identity
            }
            Ok(mirror_identity::GitDir::Missing) => {
                use std::os::unix::fs::DirBuilderExt;
                if let Err(error) = std::fs::DirBuilder::new().mode(0o700).create(&git_dir) {
                    // Made by someone else meanwhile: judged again at the next start.
                    return self.end_replica(unwritable(error));
                }
                match mirror_identity::identity(&git_dir) {
                    Some(identity) => identity,
                    None => {
                        return self.end_replica(GitRefusal::Here(format!("{} is not a directory", git_dir.display())))
                    }
                }
            }
        };
        if self.hold().record.git_identity != Some(identity) {
            self.change(|record| record.git_identity = Some(identity));
        }
        if let Err(error) =
            git_replica::write_laptop_config(&local_git, &git_dir, &config_pairs(response.entries), &hooks)
        {
            return self.end_replica(unwritable(error));
        }
        self.hold().git_digest = Some(response.digest);
        let name = session_name(&key, Role::Git);
        let mut known = match self.hold().record.git_session.clone() {
            Some(recorded) => sessions::find(self.deps.engine.as_ref(), &root, &recorded)
                .ok()
                .flatten(),
            None => None,
        };
        // Mutagen keeps a session's ignores for its whole life: one made from an older description is made again.
        if known.is_some() && self.hold().record.git_spec_version != git_replica::GIT_SESSION_VERSION {
            if !self.stop_git_session() {
                return;
            }
            known = None;
        }
        match known {
            Some(status) => {
                if sessions::is_paused(&status) && self.hold().record.paused.is_none() {
                    let _ = self.deps.engine.resume(&status.name);
                }
                let hold = self.hold();
                hold.git_held = false;
                hold.git = Some(status);
            }
            None => {
                // Recorded first: the transport connects only for a session the record names.
                if !self.reserve_session(Role::Git, &name) {
                    return;
                }
                match self.deps.engine.create(&sessions::git_spec(&key, &root)) {
                    Ok(()) => {
                        let hold = self.hold();
                        hold.git_retry_at = None;
                        hold.git_backoff = Duration::ZERO;
                        self.change(|record| record.git_spec_version = git_replica::GIT_SESSION_VERSION);
                    }
                    Err(error) => {
                        self.change(|record| record.git_session = None);
                        self.git_failed();
                        self.line_once(format!("git history is not mirrored: {error}"));
                    }
                }
            }
        }
    }

    /// Terminates the recorded git session and takes it out of the record; the laptop `.git` stays as it is. A
    /// terminate Mutagen did not carry out (a timeout) keeps the name, so release still pauses it and a later step
    /// stops it, and pauses what it can meanwhile: `false` then.
    fn stop_git_session(&mut self) -> bool {
        let Some(name) = self.hold().record.git_session.clone() else {
            return true;
        };
        if let Err(error) = self.deps.engine.terminate(&name) {
            if !self.deps.engine.is_missing(&error) {
                if self.deps.engine.pause(&name).is_err() {
                    self.hold().pause_retry = true;
                }
                self.git_failed();
                self.line_once(format!(
                    "the git replica {name} could not be stopped yet ({error}); it keeps trying"
                ));
                return false;
            }
        }
        self.stop_monitor(&name);
        let hold = self.hold();
        hold.git = None;
        hold.git_held = false;
        self.change(|record| {
            record.git_session = None;
            record.git_ready = false;
        });
        true
    }

    /// Pauses the git session because its `.git` moved, once; a pause Mutagen did not carry out is tried at the next
    /// step.
    fn hold_git(&mut self) {
        if self.hold().git_held {
            return;
        }
        let Some(name) = self.hold().record.git_session.clone() else {
            return;
        };
        match self.deps.engine.pause(&name) {
            Ok(()) => self.hold().git_held = true,
            Err(error) if self.deps.engine.is_missing(&error) => self.hold().git_held = true,
            Err(error) => self.line_once(format!(
                "the git replica {name} could not be paused yet ({error}); it keeps trying"
            )),
        }
    }

    /// A replica create or stop failed: the next try waits, twice as long each time up to 32 rechecks, so a server or
    /// daemon that keeps refusing costs one attempt (and one line) now and then rather than at every recheck.
    fn git_failed(&mut self) {
        let cap = self.config.recheck_every.saturating_mul(32);
        let first = self.config.recheck_every;
        let hold = self.hold();
        hold.git_retry_at = Some(Instant::now() + hold.git_backoff);
        hold.git_backoff = hold.git_backoff.saturating_mul(2).max(first).min(cap);
    }

    fn git_retry_due(&mut self) -> bool {
        self.hold().git_retry_at.is_none_or(|at| Instant::now() >= at)
    }

    /// No replica runs: the git session ends, and when the server or this laptop refused one, Eitri's `.git` leaves
    /// the mirror (kept under the state home), so plain git on the laptop cannot write into a copy that a later
    /// replica would bring back to the server's state. A server that could not be reached keeps the copy as it is.
    fn end_replica(&mut self, refusal: GitRefusal) {
        let root = self.project.root.clone();
        let record = self.hold().record.clone();
        let ours = match &self.config.local_git {
            Some(local_git) => matches!(
                mirror_identity::git_dir(
                    local_git,
                    &root,
                    &record,
                    &git_replica::refuse_hooks_dir(&self.config.state_home)
                ),
                Ok(mirror_identity::GitDir::Ours(_))
            ),
            None => record.git_identity.is_some_and(|recorded| {
                mirror_identity::identity(&root.join(".git"))
                    .is_some_and(|found| mirror_identity::compare(&recorded, &found) == mirror_identity::Verdict::Same)
            }),
        };
        // Eitri's `.git` leaves the mirror only once its replica has stopped: one still running would make it again.
        let stopped = self.stop_git_session();
        let why = match refusal {
            GitRefusal::Unreachable(why) => why,
            GitRefusal::Here(why) => {
                // A refusal here is tried again only after the git backoff, so a laptop config that keeps failing
                // does not make and end a `.git` at every recheck.
                self.git_failed();
                if ours && stopped && mirror_identity::holds_only_eitri_files(&root.join(".git")) {
                    // Nothing of the server's or the person's is in it: removed, not kept among the retired copies.
                    let _ = std::fs::remove_dir_all(root.join(".git"));
                    self.change(|record| record.git_identity = None);
                } else if ours && stopped {
                    let key = self.hold().key.clone();
                    match mirror_identity::retire_git_dir(&self.config.state_home, &key, &root) {
                        Ok(kept) => {
                            self.change(|record| record.git_identity = None);
                            self.line(format!(
                                "moved the laptop's copy of the git history out of {} to {}",
                                root.display(),
                                kept.display()
                            ));
                        }
                        Err(error) => self.line(format!(
                            "the laptop's copy of the git history in {} could not be moved out: {error}",
                            root.display()
                        )),
                    }
                }
                why
            }
        };
        self.line_once(format!(
            "git history is not mirrored ({why}); git commands run on {}",
            self.project.host
        ));
    }

    /// One thread per session follows Mutagen's monitor stream; a session whose stream cannot start is listed every
    /// `poll_every` instead.
    fn start_monitors(&mut self) {
        let hold = self.hold();
        let tree = hold
            .record
            .tree_session
            .as_ref()
            .filter(|_| !hold.pending.as_ref().is_some_and(|p| p.why == Why::Lost));
        let names: Vec<_> = tree.into_iter().chain(&hold.record.git_session).cloned().collect();
        hold.monitors.retain(|name, monitor| {
            if names.contains(name) {
                return true;
            }
            monitor.stop.store(true, Ordering::Release);
            false
        });
        for name in names {
            if !self.hold().monitors.contains_key(&name) {
                self.start_monitor(name, self.config.poll_every.min(self.config.recheck_every));
            }
        }
    }

    fn stop_monitor(&mut self, name: &str) {
        if let Some(monitor) = self.hold().monitors.remove(name) {
            monitor.stop.store(true, Ordering::Release);
        }
    }

    fn start_monitor(&mut self, name: String, delay: Duration) {
        let hold = self.hold();
        hold.monitor_epoch += 1;
        let epoch = hold.monitor_epoch;
        let sender = hold.sender.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let started = self.deps.engine.monitor(&name).ok().and_then(|stream| {
            let name = name.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("eitri-sync-monitor".into())
                .spawn(move || {
                    for status in stream {
                        if stop.load(Ordering::Acquire)
                            || sender
                                .send(MonitorMsg {
                                    epoch,
                                    name: name.clone(),
                                    status: Some(status),
                                })
                                .is_err()
                        {
                            return;
                        }
                    }
                    let _ = sender.send(MonitorMsg {
                        epoch,
                        name,
                        status: None,
                    });
                })
                .ok()
        });
        self.hold().monitors.insert(
            name,
            MonitorState {
                epoch,
                stop,
                delay,
                retry_at: started.is_none().then(|| Instant::now() + delay),
            },
        );
    }

    fn retry_monitors(&mut self) {
        let cap = self.config.recheck_every;
        let due: Vec<_> = self
            .hold()
            .monitors
            .iter()
            .filter_map(|(name, monitor)| {
                monitor
                    .retry_at
                    .filter(|at| *at <= Instant::now())
                    .map(|_| (name.clone(), monitor.delay.saturating_mul(2).min(cap)))
            })
            .collect();
        for (name, delay) in due {
            self.start_monitor(name, delay);
        }
    }

    /// The holder's step. Update intake (and the tripwire, which must never wait) and the status write run without the
    /// action lock; the record's decisions, rechecks, re-creation and the first-cycle accounting run only while this
    /// step holds it, which it takes with a single try and lets go before writing the status, so a CLI action waiting
    /// for it finds it free between steps.
    fn act(&mut self) -> std::io::Result<()> {
        if self.hold.is_none() {
            return Ok(());
        }
        // In arrival order: the one the wait took first, then the rest. A message from a stopped monitor is dropped.
        let mut messages: Vec<MonitorMsg> = self.received.take().into_iter().collect();
        while let Ok(message) = self.hold().updates.try_recv() {
            messages.push(message);
        }
        let mut updates = Vec::new();
        let initial_delay = self.config.poll_every.min(self.config.recheck_every);
        for message in messages {
            let Some(monitor) = self.hold().monitors.get_mut(&message.name) else {
                continue;
            };
            if monitor.epoch != message.epoch {
                continue;
            }
            match message.status {
                Some(status) => {
                    if !matches!(status.state, SyncState::Disconnected(_)) {
                        monitor.delay = initial_delay;
                    }
                    updates.push(status);
                }
                None => monitor.retry_at = Some(Instant::now() + monitor.delay),
            }
        }
        let polling = self.hold().monitors.values().any(|m| m.retry_at.is_some());
        if polling && self.hold().last_poll.elapsed() >= self.config.poll_every {
            self.hold().last_poll = Instant::now();
            match self.deps.engine.status(&sessions::root_labels(&self.project.root)) {
                Ok(listed) => updates.extend(
                    listed
                        .into_iter()
                        .filter(|s| self.hold().monitors.get(&s.name).is_some_and(|m| m.retry_at.is_some())),
                ),
                Err(error) => self.hold().notice = Some(format!("sync status is unavailable: {error}")),
            }
        }
        for status in updates {
            self.take_update(status);
        }
        self.retry_pause();
        let transition = super::conflicts::try_action_lock(&self.locks)?;
        // Monitor updates can trip the size limit even while a CLI decision owns the transition lock.
        self.follow_record(transition.is_some());
        if transition.is_some() {
            if self.hold().started && self.hold().record.paused.is_none() {
                // A folder replaced while the sessions run is paused within a step; a `.git` replaced under a running
                // replica is judged again (one look, no git run, unless it moved).
                if !self.folder_refused()
                    && mirror_identity::git_dir_moved(&self.hold.as_ref().expect("held").record, &self.project.root)
                {
                    // Paused at once, before any server round trip and whatever the backoff says: a one-way replica
                    // writing into a `.git` that is not the recorded one resets what is there, or makes a deleted
                    // one again before it can be judged.
                    self.hold_git();
                    if self.git_retry_due() {
                        self.ensure_git();
                        if !self.folder_paused() {
                            self.start_monitors();
                        }
                    }
                }
            }
            let recheck = {
                let hold = self.hold();
                hold.recheck_due || hold.last_recheck.elapsed() >= self.config.recheck_every
            };
            if recheck {
                self.recheck();
            }
            if self.hold().pending.is_some() && self.hold().record.authorized() {
                self.try_recreate();
            }
            self.retry_monitors();
            self.finish_first_cycle();
        }
        drop(transition);
        self.publish();
        Ok(())
    }

    /// While a recorded pause has not been carried out by Mutagen (a pause that failed or timed out), lists this
    /// project's sessions every `poll_every` and pauses again any still running.
    fn retry_pause(&mut self) {
        let every = self.config.poll_every;
        let hold = self.hold();
        if !hold.pause_retry || hold.record.paused.is_none() || hold.last_pause_try.elapsed() < every {
            return;
        }
        hold.last_pause_try = Instant::now();
        let names: Vec<String> = [&hold.record.tree_session, &hold.record.git_session]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        let Ok(listed) = self.deps.engine.status(&sessions::root_labels(&self.project.root)) else {
            return;
        };
        let mut running = false;
        for status in listed.iter().filter(|status| names.contains(&status.name)) {
            if !sessions::is_paused(status) && self.deps.engine.pause(&status.name).is_err() {
                running = true;
            }
        }
        self.hold().pause_retry = running;
    }

    fn take_update(&mut self, status: SessionStatus) {
        let key = self.hold().key.clone();
        let ours = status.name == session_name(&key, Role::Git) || status.name == session_name(&key, Role::Tree);
        if !ours {
            return;
        }
        let failure = match &status.state {
            SyncState::Disconnected(why) => Some(why.as_str()),
            _ => None,
        }
        .or(status.last_error.as_deref());
        if failure.is_some_and(|why| why.contains(KEY_REFUSAL)) && self.hold().record.paused.is_none() {
            let text = format!(
                "the server behind {} is not the one this mirror syncs with (its host key changed); sync is paused. \
                 If you expected this, run eitri host sync resume in {} and answer y",
                self.project.host,
                self.project.root.display()
            );
            self.pause_with(PAUSE_HOST_KEY, text, None);
        }
        // A recorded pause holds: a session found running (a pause that failed, one resumed from elsewhere) is paused
        // again.
        let running = matches!(
            status.state,
            SyncState::Connecting
                | SyncState::Scanning
                | SyncState::Staging
                | SyncState::Applying
                | SyncState::Watching
        );
        if running && self.hold().record.paused.is_some() {
            if let Err(error) = self.deps.engine.pause(&status.name) {
                self.hold().pause_retry = true;
                self.line_once(format!("sync could not pause {}: {error}", status.name));
            }
        }
        if status.name == session_name(&key, Role::Git) {
            if status.state == SyncState::Watching && !self.hold().record.git_ready {
                self.change(|record| record.git_ready = true);
            }
            self.hold().git = Some(status);
            return;
        }
        let previous_cycles = self.hold().tree.as_ref().map(|s| s.successful_cycles);
        if status.state != SyncState::Watching || previous_cycles != Some(status.successful_cycles) {
            self.hold().last_change = Instant::now();
        }
        if let Some(staging) = &status.staging {
            if staging.path.as_deref().is_some_and(touches_ignores) {
                self.hold().recheck_due = true;
            }
        }
        if status.successful_cycles > 0 && !self.hold().record.tree_synced {
            self.change(|record| record.tree_synced = true);
        }
        if self.hold().record.tripwire_allow_batch {
            let started = *self.hold().batch_cycles.get_or_insert(status.successful_cycles);
            if status.successful_cycles > started {
                self.hold().batch_cycles = None;
                self.change(|record| record.tripwire_allow_batch = false);
            }
        }
        self.check_tripwire(&status);
        if self
            .hold()
            .monitors
            .get(&status.name)
            .is_some_and(|m| m.retry_at.is_some())
        {
            self.check_arrived(&status);
        }
        self.hold().tree = Some(status);
    }

    fn check_tripwire(&mut self, status: &SessionStatus) {
        let Some(staging) = &status.staging else { return };
        if self.hold().record.paused.is_some() {
            return;
        }
        let record = &self.hold.as_ref().expect("held").record;
        let tripped = tripwire::trips(
            staging,
            &Allowances {
                limit_bytes: u64::from(self.config.tripwire_mib) * MIB,
                allowed: &record.tripwire_allowed,
                allow_batch: record.tripwire_allow_batch,
                first_sync: !record.tree_synced,
            },
        );
        if !tripped {
            return;
        }
        let Some(tree) = record.tree_session.clone() else {
            return;
        };
        // Pause first: everything else can wait, the bytes cannot.
        if let Err(error) = self.deps.engine.pause(&tree) {
            self.line(format!("sync could not pause for a large change: {error}"));
        }
        let root = self.project.root.clone();
        let path = staging.path.as_deref().map(|rel| tripwire::top_missing(&root, rel));
        let fallback = staging.total_received_bytes.max(staging.expected_bytes);
        let bytes = match &path {
            Some(path) => {
                let patterns = self.hold().patterns.clone();
                match self
                    .deps
                    .helper
                    .path_sizes(&root, std::slice::from_ref(path), false, &patterns, 100_000)
                {
                    Ok(response) if response.refusal.is_empty() => response
                        .sizes
                        .first()
                        .map(|size| size.bytes.max(fallback))
                        .unwrap_or(fallback),
                    _ => fallback,
                }
            }
            None => fallback,
        };
        let text = tripwire::paused_text(&self.project.root, &self.project.host, path.as_deref(), bytes);
        self.pause_with(PAUSE_TRIPWIRE, text, path);
    }

    /// Without the monitor stream, a batch can finish between two listings: the laptop's side grew past the limit
    /// within one cycle and no pause came. Said afterwards.
    fn check_arrived(&mut self, status: &SessionStatus) {
        let limit = u64::from(self.config.tripwire_mib) * MIB;
        let now = (status.laptop_bytes, status.successful_cycles);
        let before = self.hold().poll_bytes.replace(now);
        let Some((bytes, cycles)) = before else { return };
        let record = &self.hold.as_ref().expect("held").record;
        if record.paused.is_some() || !record.tree_synced || record.tripwire_allow_batch {
            return;
        }
        if now.1 > cycles && now.0.saturating_sub(bytes) > limit {
            self.line(tripwire::arrived_text(&self.project.host, now.0 - bytes));
        }
    }

    /// Reacts to what the CLI wrote into the record.
    fn follow_record(&mut self, can_transition: bool) {
        let Some(latest) = self.reread() else { return };
        let seen = self.hold().seen.clone();
        if !can_transition {
            if latest.paused.is_some() && latest.paused != seen.paused {
                self.hold().record.paused = latest.paused.clone();
                self.hold().seen.paused = latest.paused;
                self.hold_paused();
            }
            return;
        }
        self.hold().record = latest.clone();
        if latest == seen {
            return;
        }
        self.hold().seen = latest.clone();
        if latest.set_up && !seen.set_up {
            self.ensure_sessions();
            return;
        }
        if latest.accepted_host_key.is_some()
            && latest.accepted_host_key.as_deref() == Some(&self.project.host_key_sha256)
        {
            self.settle_host_key();
            self.ensure_sessions();
            return;
        }
        if seen.paused.is_some() && latest.paused.is_none() {
            // Resumed: the whole start again (resume, ignores, the replica, monitors), as when a holder starts.
            self.hold().lines_shown.clear();
            self.ensure_sessions();
            return;
        }
        if latest.paused.is_some() && seen.paused.is_none() {
            // Paused from outside (`eitri host stop`): the sessions stay paused until the person resumes.
            self.hold_paused();
            return;
        }
        self.check_digest();
    }

    /// The checks every session start makes before it creates anything: the state home not inside the root, no other
    /// mirror around or inside it, the folder the record's, the daemon up, and no session of another host key on the
    /// root. A failure is the notice and a line, as at a start.
    fn may_start_sessions(&mut self) -> bool {
        let root = self.project.root.clone();
        let refusal = state_inside(&self.config.state_home, &root)
            .or_else(|| match mirror_identity::overlapping(&self.store, &root) {
                Ok(found) => found,
                Err(why) => Some(why),
            })
            .or_else(|| {
                self.deps
                    .engine
                    .start()
                    .err()
                    .map(|error| format!("sync stopped: {error}"))
            });
        if let Some(why) = refusal {
            self.hold().notice = Some(why.clone());
            self.line_once(why);
            return false;
        }
        if self.folder_refused() {
            return false;
        }
        let key = self.hold().key.clone();
        match sessions::terminate_other_keys(self.deps.engine.as_ref(), &key, &root) {
            Ok(terminated) => {
                for name in terminated {
                    self.line(format!(
                        "stopped sync session {name}: it synced {} with another server",
                        root.display()
                    ));
                }
                true
            }
            Err(error) => {
                let text = format!("sync stopped: {error}");
                self.hold().notice = Some(text.clone());
                self.line_once(text);
                false
            }
        }
    }

    /// The effective ignores against the ones the tree session was created with. A holder that started under a
    /// tripwire pause has not fetched the server's ignores yet; it fetches them here, since an exclude that leaves the
    /// tripped path on the server ends that pause through the re-creation this starts.
    fn check_digest(&mut self) {
        if self.key_moved() == KeyLook::Paused {
            return;
        }
        let tripped = |record: &SyncRecord| record.paused.as_ref().is_some_and(|p| p.reason == PAUSE_TRIPWIRE);
        if self.hold().translated.is_none() && tripped(&self.hold().record) && self.hold().record.authorized() {
            // This holder never ran the start's checks; a re-creation must not skip them.
            if !self.may_start_sessions() {
                return;
            }
            let Some(translated) = self.fetch_ignores() else { return };
            self.hold().translated = Some(translated);
        }
        let Some(translated) = self.hold().translated.clone() else {
            return;
        };
        let patterns = ignores::effective(&translated, &self.hold().record);
        let hold = self.hold();
        // A restart may see the replacement session before its digest was recorded. Keep its accounting
        // until the first cycle, unless a paused transfer needs an exclusion to make progress.
        if digest(&patterns) != hold.record.ignore_digest
            && hold.record.tree_session.is_some()
            && hold.pending.is_none()
            && (hold.awaiting.is_none() || hold.record.paused.is_some())
        {
            self.set_pending(Why::IgnoresChanged);
        }
    }

    /// The server's ignores, its git config and the mode keeper's lines, asked again; a start that did not complete
    /// is tried again. Once started, a listing also checks for sessions lost while this actor was holding.
    fn recheck(&mut self) {
        {
            let hold = self.hold();
            hold.recheck_due = false;
            hold.last_recheck = Instant::now();
        }
        if self.key_moved() == KeyLook::Paused || self.hold().record.paused.is_some() {
            return;
        }
        if !self.hold().started {
            self.hold().lines_shown.clear();
            self.ensure_sessions();
            return;
        }
        let labels = sessions::root_labels(&self.project.root);
        let listed = self.deps.engine.status(&labels).or_else(|_| {
            // A plain listing cannot start an absent daemon. The engine checks liveness before restarting it.
            self.deps.engine.start()?;
            self.deps.engine.status(&labels)
        });
        let listed = match listed {
            Ok(sessions) => sessions,
            Err(error) => {
                self.hold().notice = Some(format!("sync status is unavailable: {error}"));
                return;
            }
        };
        // A WatchModes that failed, or one sent before the session's identifier was known, is sent again: the keeper
        // keeps only the registered staging directory ready.
        let staging = self.hold().record.tree_session.clone().and_then(|name| {
            listed
                .iter()
                .find(|s| s.name == name && !s.identifier.is_empty())
                .map(|s| sessions::staging_name(&s.identifier))
        });
        if staging.is_some() && staging != self.hold().modes_sent {
            self.send_watch_modes();
        }
        if let Some(name) = self.hold().record.tree_session.clone() {
            if !listed.iter().any(|s| s.name == name)
                && !self.hold().pending.as_ref().is_some_and(|p| p.why == Why::Lost)
            {
                self.stop_monitor(&name);
                self.hold().tree = None;
                self.set_pending(Why::Lost);
            }
        }
        let git_name = self.hold().record.git_session.clone();
        let git_missing = !listed.iter().any(|s| Some(&s.name) == git_name.as_ref());
        if let Some(name) = git_name.filter(|_| git_missing) {
            self.stop_monitor(&name);
            self.hold().git = None;
            self.change(|record| record.git_ready = false);
        }
        if let Some(translated) = self.fetch_ignores() {
            self.hold().notice = None;
            self.hold().translated = Some(translated);
            self.check_digest();
        }
        if let Ok(response) = self.deps.helper.git_config(&self.project.root) {
            let changed = Some(&response.digest) != self.hold().git_digest.as_ref();
            let refused_now = !response.refusal.is_empty() && self.hold().record.git_session.is_some();
            let retry = response.refusal.is_empty() && git_missing && self.git_retry_due();
            if changed || refused_now || retry {
                // Writes the new config, makes a replica whose creation failed, or ends one the server no longer
                // allows; a folder replaced meanwhile leaves the sessions paused and unfollowed.
                self.ensure_git();
                if !self.folder_paused() {
                    self.start_monitors();
                }
            }
        }
        self.show_mode_lines();
    }

    /// Mode keeper lines for this root: a counted line when its (epoch, sequence) is new, any other line once.
    fn show_mode_lines(&mut self) {
        let Ok(lines) = self.deps.helper.mode_lines() else {
            return;
        };
        let root = self.project.root.to_string_lossy().into_owned();
        for line in lines.into_iter().filter(|line| line.root == root) {
            let text = name_host(&line.text, &self.project.host);
            if line.sequence == 0 {
                self.line_once(text);
                continue;
            }
            let shown = self.hold().modes_shown.get(&root).copied();
            let new = match shown {
                None => true,
                Some((epoch, sequence)) => line.epoch != epoch || line.sequence > sequence,
            };
            if new {
                self.hold()
                    .modes_shown
                    .insert(root.clone(), (line.epoch, line.sequence));
                self.line(text);
            }
        }
    }

    /// Re-creates the tree session once the project is quiet.
    fn try_recreate(&mut self) {
        let Some(why) = self.hold().pending.as_ref().map(|p| p.why.clone()) else {
            return;
        };
        if self
            .hold()
            .record
            .paused
            .as_ref()
            .is_some_and(|p| p.reason != PAUSE_TRIPWIRE)
        {
            return;
        }
        // Not past a key the connection may have moved to: a key that could not be looked at (a master start under
        // way) waits for the next tick too.
        if self.key_moved() != KeyLook::Same {
            return;
        }
        let waiting = match why {
            Why::IgnoresChanged => "ignore rules changed; applying when idle",
            Why::Lost => "the sync session was lost; re-creating it when idle",
            Why::HostKey => "re-creating sync under the new host key when idle",
        };
        let own = self.deps.editor.clone();
        let helper = self.deps.helper.clone();
        let root = self.project.root.clone();
        let reports = EditorReports::new(self.locks.clone());
        let editors = EditorsRef(vec![own.as_ref(), &reports]);
        let activity = Activity {
            helper: helper.as_ref(),
            root: &root,
        };
        let last_change = self.hold().last_change;
        if let Err(not_quiet) = quiet::check(
            &activity,
            &editors,
            &self.project.root,
            last_change,
            self.config.quiet_for,
        ) {
            self.hold().notice = Some(format!("{waiting} ({})", not_quiet.text()));
            return;
        }
        let generation = editors.saves_since(0).0;
        let Some(translated) = self.fetch_ignores() else { return };
        let patterns = ignores::effective(&translated, &self.hold().record);
        self.hold().translated = Some(translated);
        if let Err(error) = super::mutagen::validate_ignores(&patterns) {
            self.hold().notice = Some(error.to_string());
            return;
        }
        // Flush what the laptop has (not when the person paused it, or nothing runs).
        let tree = self.hold().record.tree_session.clone();
        let paused = self.hold().record.paused.is_some();
        if let (Some(tree), false) = (&tree, paused || why == Why::Lost) {
            if self.hold().tree.as_ref().is_some_and(|s| s.state != SyncState::Paused) {
                match self.deps.engine.flush(tree, self.config.flush_cap) {
                    FlushOutcome::Flushed => {}
                    FlushOutcome::TimedOut => {
                        self.hold().notice = Some(format!("{waiting} (the last flush did not finish)"));
                        return;
                    }
                    FlushOutcome::Failed(why) => {
                        self.hold().notice = Some(format!("{waiting} (the last flush failed: {why})"));
                        return;
                    }
                }
            }
        }
        if let Some(tree) = tree {
            // What the old session's monitor still has queued (its last cycle, its end) is not the new session's.
            self.stop_monitor(&tree);
            if let Err(error) = self.deps.engine.terminate(&tree) {
                if !self.deps.engine.is_missing(&error) {
                    self.start_monitors();
                    self.hold().notice = Some(format!("{waiting} (the old session did not stop: {error})"));
                    return;
                }
            }
            self.hold().tree = None;
            if self.hold().record.set_up {
                self.change(|record| record.tree_session = None);
            }
        }
        // Changed patterns restart the helper's mode keeper, whose first walk takes the modes it finds: registered
        // now, with the old session ended and the new one not yet made, that walk sees no sync write. Asked again
        // while the keeper has not settled (each answer waits a few seconds on the server), a bounded number of
        // times; the trees are listed and the save check made after it, so the session is made from what holds then.
        // The new session's staging name follows after the create, a change the running keeper takes in place.
        if patterns != self.hold().patterns {
            let staging = self.hold().modes_sent.clone().unwrap_or_default();
            let mut asked = 1;
            while self.register_modes(&patterns, staging.clone()) == Registered::Settling {
                if self.stopping() {
                    return;
                }
                if asked == MODES_SETTLE_ASKS {
                    self.line_once(format!(
                        "file modes on {}: the mode keeper was not ready for {} when sync started again; files sync \
                         writes first may keep the mode Mutagen gives them",
                        self.project.host,
                        self.project.root.display()
                    ));
                    break;
                }
                asked += 1;
            }
        }
        let old_patterns = recreate::read_created_patterns(&self.dir).unwrap_or_else(|| patterns.clone());
        let laptop = recreate::list_laptop(
            &root,
            &Matcher::new(&old_patterns),
            &Matcher::new(&patterns),
            self.config.list_budget,
        );
        let server = recreate::list_server(self.deps.helper.as_ref(), &root, &patterns, self.config.list_budget);
        let (laptop, server) = match (laptop, server) {
            (Ok(laptop), Ok(server)) => (laptop, server),
            (Err(ListError::Truncated), _) | (_, Err(ListError::Truncated)) => {
                self.hold().pending = None;
                self.pause_with(PAUSE_RECREATE, recreate::TOO_MANY_FILES.into(), None);
                return;
            }
            (Err(ListError::Failed(why)), _) | (_, Err(ListError::Failed(why))) => {
                self.hold().notice = Some(format!("{waiting} (the trees could not be listed: {why})"));
                return;
            }
        };
        // A save since the quiet check: start over.
        let (_, saved) = editors.saves_since(generation);
        if !saved.is_empty() || quiet::check(&activity, &editors, &root, last_change, Duration::ZERO).is_err() {
            self.hold().notice = Some(format!("{waiting} (a file was saved meanwhile)"));
            return;
        }
        let key = self.hold().key.clone();
        let name = session_name(&key, Role::Tree);
        let digest = digest(&patterns);
        let one_sided = recreate::one_sided(&laptop.files, &server);
        let progress = recreate::Progress {
            why: why.as_str().into(),
            one_sided: Some(one_sided.clone()),
        };
        if let Err(error) = recreate::write_progress(&self.dir, &progress) {
            self.line(format!("sync could not note what the re-creation must check: {error}"));
            return;
        }
        if !self.create_tree(&name, &patterns, &digest) {
            self.set_pending(why);
            return;
        }
        // Earlier entries stay while the laptop still holds them and the new rules still leave them out.
        let matcher = Matcher::new(&patterns);
        let mut kept_here = laptop.kept_here.clone();
        kept_here.extend(
            self.hold()
                .record
                .kept_here
                .iter()
                .filter(|path| {
                    std::fs::symlink_metadata(root.join(path)).is_ok_and(|meta| matcher.ignored(path, meta.is_dir()))
                })
                .cloned(),
        );
        kept_here.sort();
        kept_here.dedup();
        let keep = recreate::bounded_count(&kept_here);
        let kept_more = laptop.kept_here_more + (kept_here.len() - keep) as u64;
        kept_here.truncate(keep);
        if kept_more > 0 {
            self.line(format!(
                "{kept_more} more paths than :EitriSync lists no longer sync; this laptop keeps its copies"
            ));
        }
        // A tripwire pause ends only when the new rules leave its path on the server; otherwise the new session waits
        // paused for the person's choice too.
        let tripped_still = self.hold().record.paused.as_ref().is_some_and(|p| {
            p.reason == PAUSE_TRIPWIRE
                && !p
                    .path
                    .as_deref()
                    .is_some_and(|path| matcher.ignored(path, root.join(path).is_dir()) || matcher.ignored(path, true))
        });
        if tripped_still {
            let _ = self.deps.engine.pause(&name);
        }
        let hold = self.hold();
        hold.pending = None;
        hold.awaiting = Some(AwaitingFirstCycle { one_sided });
        self.change(|record| {
            record.kept_here = kept_here.clone();
            if !tripped_still && record.paused.as_ref().is_some_and(|p| p.reason == PAUSE_TRIPWIRE) {
                record.paused = None;
            }
        });
        self.send_watch_modes();
        // Resumes a replica a tripwire pause held, or makes one when none runs.
        self.ensure_git();
        if !self.folder_paused() {
            self.start_monitors();
        }
        self.line(format!("sync with {} was re-created", self.project.host));
    }

    /// After a re-created session's first cycle: what was copied to the other side is recorded and said.
    fn finish_first_cycle(&mut self) {
        let due = self.hold().accounting_after.is_none_or(|at| Instant::now() >= at);
        let done = due
            && self.hold().awaiting.is_some()
            && self
                .hold()
                .tree
                .as_ref()
                .is_some_and(|tree| tree.state == SyncState::Watching && tree.successful_cycles > 0);
        if !done {
            return;
        }
        let awaiting = self.hold().awaiting.take().expect("checked");
        let root = self.project.root.clone();
        let patterns = self.hold().patterns.clone();
        let laptop = recreate::laptop_files(&root, &Matcher::new(&patterns), self.config.list_budget);
        let server = recreate::list_server(self.deps.helper.as_ref(), &root, &patterns, self.config.list_budget);
        let (laptop, server) = match (laptop, server) {
            (Ok(laptop), Ok(server)) => (laptop, server),
            (Err(why), _) | (_, Err(why)) => {
                // Not named until confirmed: the listing is tried again at the next recheck.
                let why = match why {
                    ListError::Truncated => "too many files".to_owned(),
                    ListError::Failed(why) => why,
                };
                let retry_at = Instant::now() + self.config.recheck_every;
                let hold = self.hold();
                hold.notice = Some(format!(
                    "which files the re-created sync copied back is not confirmed yet ({why}); trying again"
                ));
                hold.accounting_after = Some(retry_at);
                hold.awaiting = Some(awaiting);
                return;
            }
        };
        let copied = recreate::copied_back(&awaiting.one_sided, &laptop, &server);
        self.record_copied_back(copied);
        recreate::clear_progress(&self.dir);
        self.check_digest();
    }

    fn record_copied_back(&mut self, copied: Vec<String>) {
        if copied.is_empty() {
            return;
        }
        let count = copied.len();
        // Joined with what the record holds now (the person may have dismissed some meanwhile).
        self.change(|record| {
            let mut all: BTreeSet<String> = record.copied_back.iter().cloned().collect();
            all.extend(copied.iter().cloned());
            let mut all: Vec<String> = all.into_iter().collect();
            let keep = recreate::bounded_count(&all);
            let more = (all.len() - keep) as u64;
            all.truncate(keep);
            record.copied_back = all;
            record.copied_back_more += more;
        });
        self.line(format!(
            "{count} files were copied to the other side when sync was re-created; if you had deleted one, delete it \
             again (see :EitriSync)"
        ));
    }

    /// Disk readers and the window each receive changed status, even if one delivery fails.
    fn publish(&mut self) {
        let Some(hold) = &self.hold else { return };
        let mut status = status_file::compose(
            &self.project.host,
            &hold.record,
            hold.tree.as_ref(),
            hold.git.as_ref(),
            hold.notice.clone(),
            now_unix_ms(),
        );
        if hold.pending.is_some() || hold.awaiting.is_some() {
            status.phase = "recreating".into();
        }
        let unchanged = hold
            .written
            .as_ref()
            .is_some_and(|written| same_status(written, &status));
        if !unchanged {
            match status_file::write_status(&self.dir, &status) {
                Ok(()) => self.hold().written = Some(status.clone()),
                Err(error) => eprintln!("[sync] {}: status not written: {error}", self.project.root.display()),
            }
        }
        if self
            .last_status_seen
            .as_ref()
            .is_none_or(|seen| !same_status(seen, &status))
        {
            let _ = self.deps.events.send(SyncEvent::Status(status.clone()));
            self.last_status_seen = Some(status);
        }
    }
}

/// Refuses a state home inside the project: Eitri's private state and the laptop-only hooks directory would be
/// synced.
pub(crate) fn state_inside(state_home: &Path, root: &Path) -> Option<String> {
    let state = state_home.canonicalize().unwrap_or_else(|_| state_home.to_owned());
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    state.starts_with(&canonical).then(|| {
        format!(
            "sync not started: Eitri's state directory {} is inside {}, which would sync it",
            state_home.display(),
            root.display()
        )
    })
}

/// The server's own host name at the start of a keeper line (`file modes on <host>: ...`), which the laptop names
/// as the person typed it.
fn name_host(text: &str, host: &str) -> String {
    const PREFIX: &str = "file modes on ";
    let Some(rest) = text.strip_prefix(PREFIX) else {
        return text.to_owned();
    };
    let end = rest.find([':', ' ']).unwrap_or(rest.len());
    format!("{PREFIX}{host}{}", &rest[end..])
}

fn same_status(a: &StatusFile, b: &StatusFile) -> bool {
    StatusFile {
        written_unix_ms: 0,
        ..a.clone()
    } == StatusFile {
        written_unix_ms: 0,
        ..b.clone()
    }
}

fn digest(patterns: &[String]) -> String {
    Translated::new(patterns.to_vec(), vec![]).digest
}

/// A path whose change may change the server's ignores.
fn touches_ignores(rel: &str) -> bool {
    rel == ".gitignore" || rel.ends_with("/.gitignore")
}

/// What [`Runner::key_moved`] found.
#[derive(PartialEq)]
enum KeyLook {
    /// The connection shows the project's key, or none to compare.
    Same,
    /// It shows another key: the key pause was recorded now.
    Paused,
    /// The key could not be looked at (a master start under way).
    Unknown,
}

/// What the helper's mode keeper answered a registration.
#[derive(PartialEq)]
enum Registered {
    /// Refused or failed: the line says why.
    Not,
    /// Taken, but the keeper had not looked at every change made before it (its first walk still running).
    Settling,
    Settled,
}

/// How many times a registration made before a session is created is asked while the keeper is still settling.
const MODES_SETTLE_ASKS: u32 = 12;

/// The key pause of a window whose own connection saw the new key: its connection stays pinned to the old one, so
/// the new key is settled from a window opened again, which reconciles it.
pub fn key_moved_text(host: &str, old: &str, new: &str, root: &Path) -> String {
    format!(
        "the server behind {host} changed its host key from {old} to {new}; sync is paused. This window's connection \
         stays on the old key: if you expected this, close the windows of {root}, open it again and run eitri host \
         sync resume in {root}",
        root = root.display()
    )
}

pub fn host_key_text(host: &str, old: &str, new: &str, root: &Path) -> String {
    format!(
        "the server behind {host} changed its host key from {old} to {new}; sync is paused. If you expected this, \
         run eitri host sync resume in {} and answer y",
        root.display()
    )
}

struct Activity<'a> {
    helper: &'a dyn SyncHelper,
    root: &'a Path,
}

impl ActivitySource for Activity<'_> {
    fn busy(&self) -> Result<bool, String> {
        let response = self.helper.project_activity(self.root)?;
        if !response.refusal.is_empty() {
            return Err(response.refusal);
        }
        Ok(response.unknown || response.turns_running > 0 || response.tasks_running > 0)
    }
}

/// Borrowed editors as one.
struct EditorsRef<'a>(Vec<&'a dyn EditorState>);

impl EditorState for EditorsRef<'_> {
    fn unsaved_under(&self, root: &Path) -> Option<Vec<PathBuf>> {
        let mut all = Vec::new();
        for editor in &self.0 {
            all.extend(editor.unsaved_under(root)?);
        }
        Some(all)
    }

    fn saves_since(&self, generation: u64) -> (u64, Vec<PathBuf>) {
        let mut newest = generation;
        let mut saved = Vec::new();
        for editor in &self.0 {
            let (their, paths) = editor.saves_since(generation);
            newest = newest.max(their);
            saved.extend(paths);
        }
        (newest, saved)
    }
}

/// Why no replica runs: the server or this laptop refused one, or the server could not be asked.
enum GitRefusal {
    Here(String),
    Unreachable(String),
}

fn config_pairs(entries: Vec<agent::remote_client::GitConfigEntry>) -> Vec<(String, String)> {
    entries.into_iter().map(|entry| (entry.key, entry.value)).collect()
}

/// Shared by the window's status hand-off: the last status an actor sent, for a late reader.
pub type LastStatus = Arc<Mutex<Option<StatusFile>>>;

#[cfg(test)]
#[path = "actor_tests.rs"]
pub(crate) mod tests;
