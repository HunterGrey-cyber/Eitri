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
use super::{git_replica, SyncKey};
use crate::remote_project::{PauseRecord, RemoteProject, RemoteProjects, SyncRecord};

/// Pause reasons kept in `SyncRecord.paused`.
pub const PAUSE_HOST_KEY: &str = "host-key";
pub const PAUSE_TRIPWIRE: &str = "tripwire";
pub const PAUSE_RECREATE: &str = "recreate";
pub const PAUSE_OTHER_SYNC: &str = "other-sync";
/// `eitri host stop <host>` paused the sessions.
pub const PAUSE_STOPPED: &str = "host-stopped";

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

/// Flushing and reading the status, for whoever needs it: holder or observer, the send path, the shim.
pub trait SyncHandle: Send + Sync {
    fn flush_tree(&self, cap: Duration) -> FlushOutcome;
    fn status(&self) -> Result<StatusFile, String>;
}

/// The [`SyncHandle`] of one project: the engine's flush (safe for anyone) and the holder's `status.json`.
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

    fn status(&self) -> Result<StatusFile, String> {
        status_file::read_status_file(&self.dir)
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
    let bytes = agent::private_fs::read_private_nofollow_bytes_capped(&dir.join("holder"), 4096).ok()?;
    serde_json::from_slice::<HolderFile>(&bytes)
        .ok()
        .map(|holder| holder.kind)
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
    epoch: u64,
    status: Option<SessionStatus>,
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
    monitor_stop: Arc<AtomicBool>,
    /// The monitors' generation: a message from an older one (a session since terminated) is dropped.
    monitor_epoch: u64,
    polling: bool,
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
                            eprintln!("[sync] {}: {name} was not paused: {error}", self.project.root.display());
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
            hold.monitor_stop.store(true, Ordering::Release);
            hold.monitor_epoch += 1;
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
        let fallback = hold.record.clone();
        let host = self.project.host.clone();
        let mut written = None;
        let mut unreadable = None;
        // Whether the file held only what this holder knew: then nothing of the person's is folded in unseen.
        let mut only_mine = false;
        let result = self.store.update_for_host(&self.project.root, &host, |project| {
            let mut record = match project.sync_record() {
                Ok(record) => record.unwrap_or_else(|| fallback.clone()),
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
        let existing = self.reread();
        let set_up = existing.is_some();
        let record = existing.unwrap_or_else(|| SyncRecord {
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
            monitor_stop: Arc::new(AtomicBool::new(false)),
            monitor_epoch: 0,
            polling: false,
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
        });
        // A project first connect never set up gets no record from here (ensure_sessions says so).
        if set_up {
            self.change(|_| {});
        }
        self.settle_host_key();
        if self.hold().record.paused.is_some() {
            self.hold_paused();
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
            // The pause is recorded anyway; every update from a running session pauses it again.
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
        if self.reread().is_none() {
            let text = format!(
                "sync is not set up for {}: run eitri host open {}:{} to set up this machine's copy",
                root.display(),
                self.project.host,
                root.display()
            );
            self.hold().notice = Some(text.clone());
            self.line_once(text);
            return;
        }
        if let Some(why) = state_inside(&self.config.state_home, &root) {
            self.hold().notice = Some(why.clone());
            self.line_once(why);
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
                        if self.hold().record.ignore_digest != digest {
                            self.set_pending(Why::IgnoresChanged);
                        }
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
                None => {
                    if !self.create_tree(&tree_name, &patterns, &digest) {
                        return;
                    }
                }
            }
        }
        self.send_watch_modes();
        self.ensure_git();
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

    fn create_tree(&mut self, name: &str, patterns: &[String], digest: &str) -> bool {
        let key = self.hold().key.clone();
        let spec = sessions::tree_spec(&key, &self.project.root, patterns);
        // Recorded first: the transport connects only for a session the record names. A holder that stops before
        // the create finishes leaves a name Mutagen may not know, which the next holder re-creates as lost.
        self.change(|record| record.tree_session = Some(name.to_owned()));
        if let Err(error) = self.deps.engine.create(&spec) {
            self.change(|record| record.tree_session = None);
            let text = format!("sync not started: {error}");
            self.hold().notice = Some(text.clone());
            self.line(text);
            return false;
        }
        let _ = recreate::write_created_patterns(&self.dir, patterns);
        let hold = self.hold();
        hold.patterns = patterns.to_vec();
        hold.notice = None;
        self.change(|record| record.ignore_digest = digest.to_owned());
        true
    }

    fn send_watch_modes(&mut self) {
        let key = self.hold().key.clone();
        let patterns = self.hold().patterns.clone();
        let root = self.project.root.clone();
        match self
            .deps
            .helper
            .watch_modes(&root, &patterns, true, &sessions::staging_name(&key))
        {
            Ok(response) if !response.refusal.is_empty() => self.line_once(response.refusal),
            Ok(_) => {}
            Err(error) => self.line_once(format!("file modes on {} are not kept: {error}", self.project.host)),
        }
    }

    /// The `.git` replica: the laptop's config written first, then the session. A root whose git commands must run on
    /// the server gets none (and loses one it had).
    fn ensure_git(&mut self) {
        let root = self.project.root.clone();
        let key = self.hold().key.clone();
        let local_git = self.config.local_git.clone();
        let refusal = match (local_git, self.deps.helper.git_config(&root)) {
            (None, _) => Some("git was not found on this machine".to_owned()),
            (Some(local_git), Ok(response)) if response.refusal.is_empty() => {
                let entries: Vec<(String, String)> = response
                    .entries
                    .into_iter()
                    .map(|entry| (entry.key, entry.value))
                    .collect();
                let written = git_replica::install_refuse_hooks(&self.config.state_home).and_then(|hooks| {
                    git_replica::write_laptop_config(&local_git, &root.join(".git"), &entries, &hooks)
                });
                match written {
                    Ok(()) => {
                        self.hold().git_digest = Some(response.digest);
                        None
                    }
                    Err(error) => Some(format!("the laptop's git config could not be written: {error}")),
                }
            }
            (Some(_), Ok(response)) => Some(response.refusal),
            (Some(_), Err(error)) => Some(error),
        };
        if let Some(why) = refusal {
            if let Some(name) = self.hold().record.git_session.clone() {
                let _ = self.deps.engine.terminate(&name);
                self.hold().git = None;
                self.change(|record| {
                    record.git_session = None;
                    record.git_ready = false;
                });
            }
            self.line_once(format!(
                "git history is not mirrored ({why}); git commands run on {}",
                self.project.host
            ));
            return;
        }
        let name = session_name(&key, Role::Git);
        let known = match self.hold().record.git_session.clone() {
            Some(recorded) => sessions::find(self.deps.engine.as_ref(), &root, &recorded)
                .ok()
                .flatten(),
            None => None,
        };
        match known {
            Some(status) => {
                if sessions::is_paused(&status) && self.hold().record.paused.is_none() {
                    let _ = self.deps.engine.resume(&status.name);
                }
                self.hold().git = Some(status);
            }
            None => {
                // Recorded first: the transport connects only for a session the record names.
                self.change(|record| record.git_session = Some(name.clone()));
                if let Err(error) = self.deps.engine.create(&sessions::git_spec(&key, &root)) {
                    self.change(|record| record.git_session = None);
                    self.line(format!("git history is not mirrored: {error}"));
                }
            }
        }
    }

    /// One thread per session follows Mutagen's monitor stream; a session whose stream cannot start is listed every
    /// `poll_every` instead.
    fn start_monitors(&mut self) {
        let names: Vec<String> = {
            let record = &self.hold().record;
            [&record.tree_session, &record.git_session]
                .into_iter()
                .flatten()
                .cloned()
                .collect()
        };
        self.stop_monitors();
        let stop = Arc::new(AtomicBool::new(false));
        self.hold().monitor_stop = stop.clone();
        let epoch = self.hold().monitor_epoch;
        let mut polling = false;
        for name in names {
            match self.deps.engine.monitor(&name) {
                Ok(stream) => {
                    let sender = self.hold().sender.clone();
                    let stop = stop.clone();
                    let _ = std::thread::Builder::new()
                        .name("eitri-sync-monitor".into())
                        .spawn(move || {
                            for status in stream {
                                if stop.load(Ordering::Acquire)
                                    || sender
                                        .send(MonitorMsg {
                                            epoch,
                                            status: Some(status),
                                        })
                                        .is_err()
                                {
                                    return;
                                }
                            }
                            // The stream ended (the daemon stopped or restarted): the actor lists instead.
                            let _ = sender.send(MonitorMsg { epoch, status: None });
                        });
                }
                Err(_) => polling = true,
            }
        }
        self.hold().polling = polling;
    }

    /// The holder's step.
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
        let epoch = self.hold().monitor_epoch;
        let mut updates = Vec::new();
        for message in messages.into_iter().filter(|message| message.epoch == epoch) {
            match message.status {
                Some(status) => updates.push(status),
                None => {
                    self.hold().polling = true;
                    self.hold().recheck_due = true;
                }
            }
        }
        if self.hold().polling && self.hold().last_poll.elapsed() >= self.config.poll_every {
            self.hold().last_poll = Instant::now();
            match self.deps.engine.status(&sessions::root_labels(&self.project.root)) {
                Ok(listed) => updates.extend(listed),
                Err(error) => self.hold().notice = Some(format!("sync status is unavailable: {error}")),
            }
        }
        for status in updates {
            self.take_update(status);
        }
        let transition = super::conflicts::try_action_lock(&self.locks)?;
        // Monitor updates can trip the size limit even while a CLI decision owns the transition lock.
        self.follow_record(transition.is_some());
        if transition.is_some() {
            let recheck = {
                let hold = self.hold();
                hold.recheck_due || hold.last_recheck.elapsed() >= self.config.recheck_every
            };
            if recheck {
                self.recheck();
            }
            if self.hold().pending.is_some() {
                self.try_recreate();
            }
            self.finish_first_cycle();
        }
        drop(transition);
        self.publish();
        Ok(())
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
        if self.hold().polling {
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
                    .path_sizes(&root, std::slice::from_ref(path), &patterns, 100_000)
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
        let text = tripwire::paused_text(&self.project.host, path.as_deref(), bytes);
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

    /// The effective ignores against the ones the tree session was created with.
    fn check_digest(&mut self) {
        let Some(translated) = self.hold().translated.clone() else {
            return;
        };
        let patterns = ignores::effective(&translated, &self.hold().record);
        if digest(&patterns) != self.hold().record.ignore_digest
            && self.hold().record.tree_session.is_some()
            && self.hold().pending.is_none()
        {
            self.set_pending(Why::IgnoresChanged);
        }
    }

    /// The server's ignores, its git config and the mode keeper's lines, asked again; a start that did not complete
    /// is tried again, and a monitor stream that ended is started again.
    fn recheck(&mut self) {
        {
            let hold = self.hold();
            hold.recheck_due = false;
            hold.last_recheck = Instant::now();
        }
        if self.hold().record.paused.is_some() {
            return;
        }
        if !self.hold().started {
            self.hold().lines_shown.clear();
            self.ensure_sessions();
            return;
        }
        if self.hold().polling {
            self.start_monitors();
        }
        if let Some(translated) = self.fetch_ignores() {
            self.hold().notice = None;
            self.hold().translated = Some(translated);
            self.check_digest();
        }
        if let Ok(response) = self.deps.helper.git_config(&self.project.root) {
            let changed = Some(&response.digest) != self.hold().git_digest.as_ref();
            let refused_now = !response.refusal.is_empty() && self.hold().record.git_session.is_some();
            if changed || refused_now {
                // Writes the new config, or ends a replica the server no longer allows.
                self.ensure_git();
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
        // Flush what the laptop has (not when the person paused it, or nothing runs).
        let tree = self.hold().record.tree_session.clone();
        let paused = self.hold().record.paused.is_some();
        if let (Some(tree), false) = (&tree, paused) {
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
            self.stop_monitors();
            if let Err(error) = self.deps.engine.terminate(&tree) {
                if !self.deps.engine.is_missing(&error) {
                    self.start_monitors();
                    self.hold().notice = Some(format!("{waiting} (the old session did not stop: {error})"));
                    return;
                }
            }
            self.hold().tree = None;
            self.change(|record| record.tree_session = None);
            // The git session goes on being followed meanwhile.
            self.start_monitors();
        }
        let Some(translated) = self.fetch_ignores() else { return };
        let patterns = ignores::effective(&translated, &self.hold().record);
        self.hold().translated = Some(translated);
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
        if !self.create_tree(&name, &patterns, &digest) {
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
        let one_sided = recreate::one_sided(&laptop.files, &server);
        let progress = recreate::Progress {
            why: why.as_str().into(),
            one_sided: Some(one_sided.clone()),
        };
        if let Err(error) = recreate::write_progress(&self.dir, &progress) {
            self.line(format!("sync could not note what the re-creation must check: {error}"));
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
        self.start_monitors();
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

    /// Writes `status.json` and tells the window when something changed.
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
        if hold
            .written
            .as_ref()
            .is_some_and(|written| same_status(written, &status))
        {
            return;
        }
        if let Err(error) = status_file::write_status(&self.dir, &status) {
            eprintln!("[sync] {}: status not written: {error}", self.project.root.display());
        }
        self.hold().written = Some(status.clone());
        let _ = self.deps.events.send(SyncEvent::Status(status));
    }
}

/// Refuses a state home inside the project: Eitri's private state and the laptop-only hooks directory would be
/// synced.
fn state_inside(state_home: &Path, root: &Path) -> Option<String> {
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

/// Shared by the window's status hand-off: the last status an actor sent, for a late reader.
pub type LastStatus = Arc<Mutex<Option<StatusFile>>>;

#[cfg(test)]
#[path = "actor_tests.rs"]
pub(crate) mod tests;
