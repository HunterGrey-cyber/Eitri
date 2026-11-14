//! Runtime forwarding and durable session observation share the helper connection.
#[cfg(test)]
mod fake_sidecar;
mod proxy;
mod replay;
mod watcher;

use std::collections::HashMap;
use std::io;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use eitri_server::journal::{Journal, JournalConfig, Pending};
use eitri_server_proto::{PendingPermission, SessionInfo};
use sha2::{Digest, Sha256};
use tonic::Status;

use super::sidecars::Sidecars;
use crate::run_dir::Paths;

pub use proxy::Proxy;

pub(super) struct Session {
    identity: (String, String),
    journal: Journal,
    pending: Pending,
    closed: bool,
    /// Replays deliver the retained events and then end with this refusal.
    error: Option<Status>,
    /// A failed eviction refresh retries without losing the session's original terminal status.
    refresh_needed: bool,
    refresh_terminal: Option<Status>,
    changed: tokio::sync::watch::Sender<u64>,
}

impl Session {
    fn new(identity: (String, String), journal: Journal, pending: Pending, closed: bool) -> Self {
        let (changed, _) = tokio::sync::watch::channel(journal.head());
        Self {
            identity,
            journal,
            pending,
            closed,
            error: None,
            refresh_needed: false,
            refresh_terminal: None,
            changed,
        }
    }
}

type SharedSession = Arc<SessionLock>;

/// Listing reads the last completed update without waiting for journal I/O or eviction.
pub(super) struct SessionLock {
    state: Mutex<Session>,
    info: Mutex<SessionInfo>,
}

impl SessionLock {
    fn new(state: Session) -> Self {
        let info = SessionInfo {
            session_id: state.identity.1.clone(),
            journal_head: state.journal.head(),
            closed: state.closed,
            pending: state.pending.len() as u64,
        };
        Self {
            state: Mutex::new(state),
            info: Mutex::new(info),
        }
    }

    fn guard<'a>(&'a self, state: std::sync::MutexGuard<'a, Session>) -> SessionGuard<'a> {
        SessionGuard {
            state,
            info: &self.info,
        }
    }

    fn lock(&self) -> std::sync::LockResult<SessionGuard<'_>> {
        self.state
            .lock()
            .map(|state| self.guard(state))
            .map_err(|error| std::sync::PoisonError::new(self.guard(error.into_inner())))
    }

    #[cfg(test)]
    fn try_lock(&self) -> std::sync::TryLockResult<SessionGuard<'_>> {
        self.state
            .try_lock()
            .map(|state| self.guard(state))
            .map_err(|error| match error {
                std::sync::TryLockError::WouldBlock => std::sync::TryLockError::WouldBlock,
                std::sync::TryLockError::Poisoned(error) => {
                    std::sync::TryLockError::Poisoned(std::sync::PoisonError::new(self.guard(error.into_inner())))
                }
            })
    }
}

pub(super) struct SessionGuard<'a> {
    state: std::sync::MutexGuard<'a, Session>,
    info: &'a Mutex<SessionInfo>,
}

impl std::ops::Deref for SessionGuard<'_> {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.state
    }
}

impl std::ops::DerefMut for SessionGuard<'_> {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.state
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let mut info = self.info.lock().expect("session info");
        info.journal_head = self.state.journal.head();
        info.closed = self.state.closed;
        info.pending = self.state.pending.len() as u64;
    }
}

pub struct Serving {
    paths: Paths,
    adopted: AtomicBool,
    sidecars: Arc<Sidecars>,
    /// Only lookups and inserts happen under this lock; opening and recovering a journal never does.
    sessions: tokio::sync::Mutex<HashMap<(String, String), SharedSession>>,
    /// Sessions the helper could not follow, by journal name, with the refusal their clients get instead.
    failed: Arc<Mutex<HashMap<String, Status>>>,
    /// Observation and eviction exclude each other only for the same journal.
    observing: Arc<ObservationLocks>,
    permanent: Mutex<std::collections::HashSet<String>>,
    budget_errors: Arc<Mutex<std::collections::HashSet<String>>>,
    tombstones: Arc<Mutex<HashMap<String, String>>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    retrying: Mutex<HashMap<String, (String, String)>>,
    warning: Mutex<Option<String>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().expect("watch tasks") {
            task.abort();
        }
    }
}

/// The journal and pending file name of a session: the SHA-256 of the JSON pair `[sidecar id, session id]`.
fn journal_name(identity: &(String, String)) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(identity).expect("a string pair serializes"))
    )
}

fn is_journal_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Default)]
struct ObservationLocks(Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>);

impl ObservationLocks {
    fn journal(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.0.lock().expect("observation locks");
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(name).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(name.to_owned(), Arc::downgrade(&lock));
        lock
    }
}

fn expired_identity(paths: &Paths, name: &str) -> io::Result<Option<(String, String)>> {
    match agent::private_fs::read_private_nofollow_capped(&paths.state.join("expired").join(name), 16 * 1024) {
        Ok(text) => {
            let identity = serde_json::from_str(&text)?;
            if journal_name(&identity) != name {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expired identity names another journal",
                ));
            }
            Ok(Some(identity))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl Serving {
    pub fn new(paths: Paths, sidecars: Arc<Sidecars>) -> Arc<Self> {
        Arc::new(Self {
            paths,
            adopted: AtomicBool::new(false),
            sidecars,
            sessions: Default::default(),
            failed: Default::default(),
            observing: Default::default(),
            permanent: Default::default(),
            budget_errors: Default::default(),
            tombstones: Default::default(),
            tasks: Default::default(),
            retrying: Default::default(),
            warning: Default::default(),
        })
    }

    pub(super) fn sidecar(&self, id: &str) -> Result<eitri_server_proto::SidecarInfo, Status> {
        match self.sidecars.list().sidecars.into_iter().find(|sc| sc.sidecar_id == id) {
            Some(sc) if sc.alive && sc.handshake => Ok(sc),
            Some(sc) if sc.alive => Err(Status::unavailable("the helper is adopting this sidecar")),
            _ if !self.adopted.load(Ordering::Acquire) => {
                Err(Status::unavailable("the helper is adopting its sessions"))
            }
            _ => Err(Status::not_found("no such sidecar on this server")),
        }
    }

    /// An observed session, or a retryable refusal while its storage cannot be followed.
    pub(super) async fn session(&self, sidecar: &str, id: &str) -> Result<SharedSession, Status> {
        let identity = (sidecar.to_owned(), id.to_owned());
        if let Some(session) = self.sessions.lock().await.get(&identity).cloned() {
            return Ok(session);
        }
        if let Some(refusal) = self
            .failed
            .lock()
            .expect("failed sessions")
            .get(&journal_name(&identity))
            .cloned()
        {
            return Err(refusal);
        }
        // A watch carries the identity even if its first publication was interrupted.
        self.sidecar(sidecar)?;
        self.observe(sidecar, id).await?;
        self.sessions
            .lock()
            .await
            .get(&identity)
            .cloned()
            .ok_or_else(|| Status::unavailable("the helper has not observed this session yet"))
    }

    /// Records that a journal cannot be followed; true when this failure is new, so the caller audits it once.
    fn fail(&self, name: &str, error: &str) -> bool {
        let refusal = Status::unavailable(format!("the helper cannot follow this session yet: {error}"));
        let mut failed = self.failed.lock().expect("failed sessions");
        let new = failed
            .get(name)
            .is_none_or(|previous| previous.message() != refusal.message());
        failed.insert(name.to_owned(), refusal);
        new
    }

    async fn audit(&self, event: &'static str, fields: String) {
        let state = self.paths.state.clone();
        let _ = blocking(move || crate::audit::record(&state, event, &fields)).await;
    }

    pub(super) async fn observe(&self, sidecar: &str, id: &str) -> Result<(), Status> {
        self.observe_with(sidecar, id, open_session).await
    }

    async fn observe_with(
        &self,
        sidecar: &str,
        id: &str,
        open: impl FnOnce(&Paths, &str, (String, String)) -> io::Result<(SharedSession, bool)> + Send + 'static,
    ) -> Result<(), Status> {
        if id.is_empty() || id.len() > 4096 {
            return Err(Status::internal("invalid sidecar session identity"));
        }
        let info = match self.sidecar(sidecar) {
            Ok(info) => Some(info),
            Err(status)
                if matches!(status.code(), tonic::Code::NotFound | tonic::Code::Unavailable)
                    && !self
                        .sidecars
                        .list()
                        .sidecars
                        .iter()
                        .any(|sc| sc.sidecar_id == sidecar && sc.alive) =>
            {
                None
            }
            Err(status) => return Err(status),
        };
        let identity = (sidecar.to_owned(), id.to_owned());
        let name = journal_name(&identity);
        let _observing = self.observing.journal(&name).lock_owned().await;
        let paths = self.paths.clone();
        let file = name.clone();
        if let Some((owner, _)) = blocking(move || expired_identity(&paths, &file)).await? {
            self.tombstones
                .lock()
                .expect("expired sessions")
                .insert(name.clone(), owner);
            let refusal = replay::session_gone("the retained history has expired");
            self.failed
                .lock()
                .expect("failed sessions")
                .insert(name, refusal.clone());
            return Err(refusal);
        }
        if self.sessions.lock().await.contains_key(&identity) {
            return Ok(());
        }
        if let Some(refusal) = self
            .failed
            .lock()
            .expect("failed sessions")
            .get(&name)
            .filter(|refusal| refusal.code() == tonic::Code::NotFound)
            .cloned()
        {
            return Err(refusal);
        }
        self.retrying
            .lock()
            .expect("retry identities")
            .insert(name.clone(), identity.clone());
        let paths = self.paths.clone();
        let (key, file) = (identity.clone(), name.clone());
        let opened = blocking(move || Ok(open(&paths, &file, key))).await?;
        let (session, closed) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                if error.kind() == io::ErrorKind::InvalidData {
                    self.permanent.lock().expect("permanent failures").insert(name.clone());
                }
                if self.fail(&name, &error.to_string()) {
                    self.audit(
                        "watch-observe",
                        format!("sidecar={sidecar} journal={name} error={error}"),
                    )
                    .await;
                }
                return Err(self.failed.lock().expect("failed sessions")[&name].clone());
            }
        };
        self.failed.lock().expect("failed sessions").remove(&name);
        self.retrying.lock().expect("retry identities").remove(&name);
        self.sessions.lock().await.insert(identity, session.clone());
        drop(_observing);
        if !closed && info.is_none() {
            let shared = session.clone();
            blocking(move || {
                let mut state = shared.lock().expect("session");
                state.error = Some(replay::session_gone("the sidecar has stopped"));
                state.journal.finish_sync();
                Ok(())
            })
            .await?;
        }
        if let Some(info) = info.filter(|_| !closed) {
            let live = self.liveness(&info);
            let task = tokio::spawn(watcher::run(self.paths.clone(), info, session, live));
            let mut tasks = self.tasks.lock().expect("watch tasks");
            tasks.retain(|task| !task.is_finished());
            tasks.push(task);
        }
        Ok(())
    }

    /// Whether the sidecar a watch was started on is still that same running process.
    fn liveness(&self, info: &eitri_server_proto::SidecarInfo) -> watcher::Live {
        let sidecars = self.sidecars.clone();
        let (id, pid) = (info.sidecar_id.clone(), info.pid);
        Arc::new(move || {
            sidecars
                .list()
                .sidecars
                .iter()
                .any(|sidecar| sidecar.sidecar_id == id && sidecar.pid == pid && sidecar.alive)
        })
    }

    /// Reopens retained journals, following ready sidecars and ending journals whose sidecar died. One unreadable entry or failed observation is recorded for
    /// that session alone (audited once per distinct failure) and retried on the next pass; the rest go on.
    pub async fn adopt(&self) -> Result<(), Status> {
        let paths = self.paths.clone();
        let diagnostics = self.budget_errors.clone();
        let expired = blocking(move || {
            let mut expired = Vec::new();
            let entries = match std::fs::read_dir(paths.state.join("expired")) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(expired),
                Err(error) => return Err(error),
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        budget_error(&paths.state, &diagnostics, format!("expired entry: {error}"));
                        continue;
                    }
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(".journal-") && name.ends_with(".tmp") {
                    continue;
                }
                if !is_journal_name(&name) {
                    budget_error(
                        &paths.state,
                        &diagnostics,
                        format!("expired entry={name}: invalid journal name"),
                    );
                    continue;
                }
                match expired_identity(&paths, &name) {
                    Ok(Some((owner, _))) => expired.push((name, owner)),
                    Ok(None) => {}
                    Err(error) => budget_error(&paths.state, &diagnostics, format!("expired journal={name}: {error}")),
                }
            }
            Ok(expired)
        })
        .await?;
        for (name, owner) in expired {
            self.tombstones
                .lock()
                .expect("expired sessions")
                .insert(name.clone(), owner);
            self.failed
                .lock()
                .expect("failed sessions")
                .insert(name, replay::session_gone("the retained history has expired"));
        }
        let state = self.paths.state.clone();
        let mut listed = blocking(move || {
            let mut listed = Vec::<(String, Result<(String, String), String>)>::new();
            for entry in std::fs::read_dir(state.join("journal"))? {
                let entry = entry?;
                let path = entry.path();
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !agent::private_fs::is_private_dir(&path) {
                    continue;
                }
                let identity = match agent::private_fs::read_private_nofollow_capped(&path.join("identity"), 16 * 1024)
                {
                    Ok(text) => serde_json::from_str::<(String, String)>(&text)
                        .map_err(|error| error.to_string())
                        .and_then(|identity| {
                            if journal_name(&identity) == name {
                                Ok(identity)
                            } else {
                                Err("the identity does not name this journal".to_owned())
                            }
                        }),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => Err(error.to_string()),
                };
                listed.push((name, identity));
            }
            Ok(listed)
        })
        .await?;
        listed.extend(
            self.retrying
                .lock()
                .expect("retry identities")
                .iter()
                .map(|(name, pair)| (name.clone(), Ok(pair.clone()))),
        );
        for (name, identity) in listed {
            let failed = self.failed.lock().expect("failed sessions").get(&name).cloned();
            if failed.is_some_and(|status| status.code() == tonic::Code::NotFound)
                || self.permanent.lock().expect("permanent failures").contains(&name)
            {
                continue;
            }
            match identity {
                Ok((sidecar, session)) => {
                    let paths = self.paths.clone();
                    let owner = sidecar.clone();
                    let elsewhere = blocking(move || owned_elsewhere(&paths, &owner)).await?;
                    if !elsewhere {
                        let _ = self.observe(&sidecar, &session).await;
                    }
                }
                Err(error) => {
                    if self.fail(&name, &error) {
                        self.audit("watch-adoption", format!("journal={name} error={error}"))
                            .await;
                    }
                }
            }
        }
        self.adopted.store(true, Ordering::Release);
        Ok(())
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warning
            .lock()
            .expect("budget warning")
            .clone()
            .into_iter()
            .collect()
    }

    pub async fn maintain(&self) -> Result<(), Status> {
        self.maintain_with(
            JournalConfig::default(),
            Arc::new(eitri_server::journal::SystemClock::default()),
            eitri_server::journal::available_bytes,
        )
        .await
    }

    async fn maintain_with(
        &self,
        config: JournalConfig,
        clock: Arc<dyn eitri_server::journal::Clock>,
        free: fn(&std::path::Path) -> io::Result<u64>,
    ) -> Result<(), Status> {
        let paths = self.paths.clone();
        let live: std::collections::HashSet<_> = self
            .sidecars
            .list()
            .sidecars
            .into_iter()
            .filter(|sidecar| sidecar.alive)
            .map(|sidecar| sidecar.sidecar_id)
            .collect();
        let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
        let observing = self.observing.clone();
        let diagnostics = self.budget_errors.clone();
        let failed = self.failed.clone();
        let tombstones = self.tombstones.clone();
        let identities = self.retrying.lock().expect("retry identities").clone();
        let warning = blocking(move || {
            for session in &sessions {
                let mut state = session.lock().expect("session");
                if state.error.is_some() && !live.contains(&state.identity.0) {
                    state.error = Some(replay::session_gone("the sidecar has stopped"));
                }
            }
            budget_with(
                &paths,
                sessions,
                &config,
                clock.as_ref(),
                &free,
                BudgetContext {
                    observing: Some(&observing),
                    diagnostics: &diagnostics,
                    live: &live,
                    failed: Some(&failed),
                    tombstones: Some(&tombstones),
                    identities: &identities,
                    measure: &journal_bytes,
                    reopen: &Journal::open,
                },
            )
        })
        .await;
        let evicted: std::collections::HashSet<_> = self
            .tombstones
            .lock()
            .expect("expired sessions")
            .keys()
            .cloned()
            .collect();
        let mut sessions = self.sessions.lock().await;
        let mut failed = self.failed.lock().expect("failed sessions");
        sessions.retain(|identity, _| {
            let name = journal_name(identity);
            if evicted.contains(&name) {
                failed.insert(name, replay::session_gone("the retained history has expired"));
                false
            } else {
                true
            }
        });
        for name in &evicted {
            failed.insert(name.clone(), replay::session_gone("the retained history has expired"));
        }
        self.retrying
            .lock()
            .expect("retry identities")
            .retain(|name, _| !evicted.contains(name));
        self.permanent
            .lock()
            .expect("permanent failures")
            .retain(|name| !evicted.contains(name));
        let expired: Vec<_> = self
            .tombstones
            .lock()
            .expect("expired sessions")
            .iter()
            .map(|(name, owner)| (name.clone(), owner.clone()))
            .collect();
        drop(failed);
        drop(sessions);
        if self.adopted.load(Ordering::Acquire) {
            let recorded: std::collections::HashSet<_> = self
                .sidecars
                .list()
                .sidecars
                .into_iter()
                .map(|sidecar| sidecar.sidecar_id)
                .collect();
            let paths = self.paths.clone();
            let forgotten = blocking(move || {
                let mut forgotten = Vec::new();
                for (name, owner) in expired {
                    if !recorded.contains(&owner) && !owned_elsewhere(&paths, &owner)? {
                        match std::fs::remove_file(paths.state.join("expired").join(&name)) {
                            Ok(()) => std::fs::File::open(paths.state.join("expired"))?.sync_all()?,
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                        forgotten.push(name);
                    }
                }
                Ok(forgotten)
            })
            .await?;
            for name in forgotten {
                self.failed.lock().expect("failed sessions").remove(&name);
                self.tombstones.lock().expect("expired sessions").remove(&name);
                self.permanent.lock().expect("permanent failures").remove(&name);
            }
        }
        *self.warning.lock().expect("budget warning") = warning?;
        Ok(())
    }

    pub async fn audit_budget_error(&self, error: &str) {
        let state = self.paths.state.clone();
        let diagnostics = self.budget_errors.clone();
        let error = error.to_owned();
        let _ = blocking(move || {
            budget_error(&state, &diagnostics, error);
            Ok(())
        })
        .await;
    }

    pub async fn pending(&self, sidecar: &str, id: &str) -> Result<Vec<PendingPermission>, Status> {
        self.sidecar(sidecar)?;
        let session = self.session(sidecar, id).await?;
        blocking(move || {
            Ok(session
                .lock()
                .expect("session")
                .pending
                .list()
                .into_iter()
                .map(|(sequence, event)| PendingPermission { sequence, event })
                .collect())
        })
        .await
    }

    pub async fn sessions(&self, sidecar: &str) -> Vec<SessionInfo> {
        let owned: Vec<SharedSession> = self
            .sessions
            .lock()
            .await
            .iter()
            .filter(|((owner, _), _)| owner == sidecar)
            .map(|(_, session)| session.clone())
            .collect();
        owned
            .into_iter()
            .map(|session| session.info.lock().expect("session info").clone())
            .collect()
    }
}

fn owned_elsewhere(paths: &Paths, sidecar: &str) -> io::Result<bool> {
    if sidecar.is_empty() || !sidecar.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)) {
        return Ok(false);
    }
    for run in std::fs::read_dir(paths.state.join("run"))? {
        let run = run?.path();
        if run != paths.metadata_run()
            && agent::private_fs::is_private_dir(&run)
            && run.join(format!("sc-{sidecar}.pid")).exists()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn claim_dormant_owners(
    paths: &Paths,
    sidecar: &str,
    held: &mut HashMap<std::path::PathBuf, agent::private_fs::PrivateLock>,
) -> io::Result<bool> {
    if sidecar.is_empty() || !sidecar.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)) {
        return Ok(false);
    }
    for run in std::fs::read_dir(paths.state.join("run"))? {
        let run = run?.path();
        if run == paths.metadata_run()
            || !agent::private_fs::is_private_dir(&run)
            || !run.join(format!("sc-{sidecar}.pid")).exists()
            || held.contains_key(&run)
        {
            continue;
        }
        let lock = agent::private_fs::PrivateLock::open(&run.join("live.lock"))?;
        if !lock.try_lock()? {
            return Ok(false);
        }
        held.insert(run, lock);
    }
    Ok(true)
}

fn journal_bytes(dir: &std::path::Path) -> io::Result<u64> {
    std::fs::read_dir(dir)?.try_fold(0u64, |total, entry| match entry?.path().symlink_metadata() {
        Ok(meta) => Ok(total.saturating_add(meta.len())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(total),
        Err(error) => Err(error),
    })
}

fn budget_error(state: &std::path::Path, seen: &Mutex<std::collections::HashSet<String>>, error: String) {
    if seen.lock().expect("budget diagnostics").insert(error.clone()) {
        let _ = crate::audit::record(state, "journal-budget", &error);
    }
}

#[cfg(test)]
fn budget(
    paths: &Paths,
    sessions: Vec<SharedSession>,
    config: &JournalConfig,
    clock: &dyn eitri_server::journal::Clock,
    free: &dyn eitri_server::journal::FreeSpace,
) -> io::Result<Option<String>> {
    budget_with(
        paths,
        sessions,
        config,
        clock,
        free,
        BudgetContext {
            observing: None,
            diagnostics: &Mutex::default(),
            live: &Default::default(),
            failed: None,
            tombstones: None,
            identities: &HashMap::new(),
            measure: &journal_bytes,
            reopen: &Journal::open,
        },
    )
}

struct BudgetContext<'a> {
    observing: Option<&'a ObservationLocks>,
    diagnostics: &'a Mutex<std::collections::HashSet<String>>,
    live: &'a std::collections::HashSet<String>,
    failed: Option<&'a Mutex<HashMap<String, Status>>>,
    tombstones: Option<&'a Mutex<HashMap<String, String>>>,
    identities: &'a HashMap<String, (String, String)>,
    measure: &'a dyn Fn(&std::path::Path) -> io::Result<u64>,
    reopen: &'a dyn Fn(&std::path::Path, &JournalConfig) -> io::Result<Journal>,
}

fn budget_with(
    paths: &Paths,
    sessions: Vec<SharedSession>,
    config: &JournalConfig,
    clock: &dyn eitri_server::journal::Clock,
    free: &dyn eitri_server::journal::FreeSpace,
    context: BudgetContext<'_>,
) -> io::Result<Option<String>> {
    use eitri_server::journal::{enforce_budget_with, BudgetOutcome, BudgetPressure, Disk, SessionUsage};
    let BudgetContext {
        observing,
        diagnostics,
        live,
        failed,
        tombstones,
        identities,
        measure,
        reopen,
    } = context;
    let report = |error: String| budget_error(&paths.state, diagnostics, error);
    let mut usage = Vec::new();
    let mut known_identities = identities.clone();
    let mut owners: HashMap<_, _> = identities
        .iter()
        .map(|(name, pair)| (name.clone(), pair.0.clone()))
        .collect();
    let mut locked = Vec::new();
    let mut active = std::collections::HashSet::new();
    for session in &sessions {
        let state = session.lock().expect("session");
        // A terminal observation refusal can leave the CLI running; only definite loss closes its budget.
        let ended = state.closed
            || state.refresh_needed
            || state
                .error
                .as_ref()
                .is_some_and(|error| error.code() == tonic::Code::NotFound);
        if ended {
            known_identities.insert(journal_name(&state.identity), state.identity.clone());
            owners.insert(journal_name(&state.identity), state.identity.0.clone());
            locked.push(state);
        } else {
            active.insert(journal_name(&state.identity));
        }
    }
    let mut dormant = HashMap::new();
    let known: HashMap<_, _> = locked
        .iter()
        .enumerate()
        .map(|(index, state)| (journal_name(&state.identity), index))
        .collect();
    for entry in std::fs::read_dir(paths.state.join("journal"))? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report(error.to_string());
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path();
        if !is_journal_name(&name) {
            report(format!("journal entry={name}: invalid journal name"));
            continue;
        }
        if !agent::private_fs::is_private_dir(&dir) {
            continue;
        }
        let bytes = match measure(&dir) {
            Ok(bytes) => bytes,
            Err(error) => {
                report(error.to_string());
                continue;
            }
        };
        let mut at = agent::private_fs::read_private_nofollow_capped(&dir.join("closed-at"), 128)
            .ok()
            .and_then(|text| text.parse::<u64>().ok());
        let mut ended = if let Some(index) = known.get(&name) {
            locked[*index].journal.finish_sync();
            true
        } else if active.contains(&name) {
            false
        } else if at.is_some() || identities.contains_key(&name) {
            let identity = agent::private_fs::read_private_nofollow_capped(&dir.join("identity"), 16 * 1024)
                .ok()
                .and_then(|text| serde_json::from_str::<(String, String)>(&text).ok())
                .filter(|identity| journal_name(identity) == name)
                .or_else(|| identities.get(&name).cloned());
            if let Some(identity) = identity.filter(|identity| !live.contains(&identity.0)) {
                known_identities.insert(name.clone(), identity.clone());
                owners.insert(name.clone(), identity.0.clone());
                match claim_dormant_owners(paths, &identity.0, &mut dormant) {
                    Ok(held) => held,
                    Err(error) => {
                        report(error.to_string());
                        false
                    }
                }
            } else {
                false
            }
        } else {
            false
        };
        if ended && at.is_none() {
            let ms = clock
                .now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            match agent::private_fs::write_private(&dir.join("closed-at"), ms.to_string().as_bytes()) {
                Ok(()) => at = Some(ms),
                Err(error) => report(format!("close timestamp: {error}")),
            }
        }
        // Expiry must publish the validated identity even if an interrupted publication left bad bytes.
        if ended {
            if let Some(identity) = known_identities.get(&name) {
                let published = serde_json::to_vec(identity)?;
                let publish = agent::private_fs::open_dir(&dir).and_then(|dir| {
                    let current = agent::private_fs::read_private_nofollow_bytes_capped(
                        &paths.state.join("journal").join(&name).join("identity"),
                        16 * 1024,
                    );
                    if current.as_ref().ok() != Some(&published) {
                        agent::private_fs::publish_private_file(&dir, std::ffi::OsStr::new("identity"), &published)?;
                    }
                    Ok(())
                });
                if let Err(error) = publish {
                    report(format!("journal identity: {error}"));
                    ended = false;
                }
            }
        }
        usage.push(SessionUsage {
            session: name,
            bytes,
            closed_at: if ended {
                at.map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms))
            } else {
                None
            },
        });
    }
    // Only unlinking excludes observation. Stamping, accounting snapshots and journal recovery do not.
    let exclusion: Vec<_> = observing
        .into_iter()
        .flat_map(|observing| {
            usage
                .iter()
                .filter(|session| session.closed_at.is_some())
                .map(|session| observing.journal(&session.session).blocking_lock_owned())
        })
        .collect();
    let result = enforce_budget_with(&paths.state, &usage, config, free, &Disk, clock);
    if let Some(failed) = failed {
        let mut failed = failed.lock().expect("failed sessions");
        let mut tombstones = tombstones.map(|tombstones| tombstones.lock().expect("expired sessions"));
        for session in &usage {
            if session.closed_at.is_some() && paths.state.join("expired").join(&session.session).exists() {
                if let (Some(tombstones), Some(owner)) = (&mut tombstones, owners.get(&session.session)) {
                    tombstones.insert(session.session.clone(), owner.clone());
                }
                failed.insert(
                    session.session.clone(),
                    replay::session_gone("the retained history has expired"),
                );
            }
        }
    }
    drop(exclusion);
    drop(dormant);
    let (warning, evicted) = match result {
        BudgetOutcome::LiveOverCap { .. } | BudgetOutcome::BelowFloor { .. } => (true, Vec::new()),
        BudgetOutcome::Partial {
            evicted,
            errors,
            pressure,
        } => {
            for error in errors {
                report(error.to_string());
            }
            (
                matches!(
                    pressure,
                    Some(BudgetPressure::LiveOverCap { .. } | BudgetPressure::BelowFloor { .. })
                ),
                evicted,
            )
        }
        BudgetOutcome::Evicted(evicted) => (false, evicted),
        BudgetOutcome::Failed(error) => return Err(error),
        BudgetOutcome::Within => (false, Vec::new()),
    };
    let changed: std::collections::HashSet<_> = evicted.into_iter().map(|range| range.session).collect();
    for state in &mut locked {
        let name = journal_name(&state.identity);
        let dir = paths.state.join("journal").join(&name);
        if !dir.exists() || paths.state.join("expired").join(&name).exists() {
            state.journal.forget_history();
            state.error = Some(replay::session_gone("the retained history has expired"));
            state.refresh_needed = false;
            state.refresh_terminal = None;
        } else if changed.contains(&name) || state.refresh_needed {
            if !state.refresh_needed {
                state.refresh_terminal = state.error.take();
                state.refresh_needed = true;
            }
            match reopen(&dir, config) {
                Ok(mut journal) => {
                    journal.finish_sync();
                    state.journal = journal;
                    state.error = state.refresh_terminal.take();
                    state.refresh_needed = false;
                }
                Err(error) => {
                    report(format!("journal={name} refresh: {error}"));
                    state.error = Some(Status::unavailable("the helper cannot refresh this session yet"));
                }
            }
        } else {
            continue;
        }
        let head = state.journal.head();
        state.changed.send_replace(head);
    }
    Ok(warning.then(|| "Eitri's history on this host is over its budget".into()))
}

/// Opens a session's journal and pending file and folds the journal into Pending.
/// The identity is published first (unless the file already holds it), so a session whose journal or pending
/// file cannot be opened yet is retried by the next adoption pass.
fn open_session(paths: &Paths, name: &str, identity: (String, String)) -> io::Result<(SharedSession, bool)> {
    let dir = paths.state.join("journal").join(name);
    agent::private_fs::create_private_dir_all(&dir, &paths.state)?;
    let metadata = agent::private_fs::open_dir(&dir)?;
    // Adopting a session whose identity file is already right writes nothing, so a restart on a full disk
    // still observes it.
    let published = serde_json::to_vec(&identity)?;
    let current = agent::private_fs::read_private_nofollow_bytes_capped(&dir.join("identity"), 16 * 1024);
    if current.as_ref().ok() != Some(&published) {
        agent::private_fs::publish_private_file(&metadata, std::ffi::OsStr::new("identity"), &published)?;
    }
    let mut journal = Journal::open(&dir, &JournalConfig::default())?;
    let mut pending = Pending::open(&paths.state.join("pending").join(format!("{name}.bin")))?;
    let closed = watcher::recover(&journal, &mut pending)?;
    if closed {
        journal.finish_sync();
    }
    Ok((
        Arc::new(SessionLock::new(Session::new(identity, journal, pending, closed))),
        closed,
    ))
}

pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Status::internal(error.to_string()))?
        .map_err(|error| Status::internal(error.to_string()))
}

#[cfg(test)]
fn scratch(label: &str) -> std::path::PathBuf {
    use std::io::Read;
    let mut random = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut random)
        .unwrap();
    let nonce: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = std::env::temp_dir().join(format!("serve-{label}-{nonce}"));
    agent::private_fs::create_private_dir_all(&path, &path).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn paths(label: &str) -> Paths {
        let root = scratch(label);
        let paths = Paths::new("t", &root.join("d"), &root, &root).unwrap();
        paths.prepare().unwrap();
        paths
    }

    fn regression_paths(label: &str) -> Paths {
        let root = scratch(label);
        let paths = Paths {
            version: "t".into(),
            key: "t".into(),
            data: root.join("data"),
            state: root.join("state"),
            run: root.join("run"),
            install: None,
            installed: false,
        };
        paths.prepare().unwrap();
        paths
    }

    struct Cleanup(std::path::PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_readable_foreign_directory_does_not_stop_retention_and_is_reported_once() {
        let paths = regression_paths("readable-foreign");
        let _cleanup = Cleanup(paths.state.parent().unwrap().to_owned());
        let foreign = paths.state.join("journal/old copy");
        agent::private_fs::create_private_dir_all(&foreign, &paths.state).unwrap();
        let diagnostics = Mutex::default();
        for id in ["first", "second"] {
            let pair = ("sidecar".into(), id.into());
            let name = journal_name(&pair);
            let (session, _) = open_session(&paths, &name, pair).unwrap();
            session.lock().unwrap().closed = true;
            budget_with(
                &paths,
                vec![session],
                &JournalConfig {
                    free_floor_bytes: 0,
                    closed_keep: std::time::Duration::ZERO,
                    ..Default::default()
                },
                &Clock(std::time::UNIX_EPOCH),
                &|_: &std::path::Path| Ok(u64::MAX),
                BudgetContext {
                    observing: None,
                    diagnostics: &diagnostics,
                    live: &Default::default(),
                    failed: None,
                    tombstones: None,
                    identities: &HashMap::new(),
                    measure: &journal_bytes,
                    reopen: &Journal::open,
                },
            )
            .unwrap();
            assert!(!paths.state.join("journal").join(name).exists());
        }
        assert!(foreign.is_dir());
        let audit = std::fs::read_to_string(paths.state.join("audit.log")).unwrap();
        assert_eq!(audit.matches("old copy").count(), 1, "{audit}");
    }

    #[tokio::test]
    async fn invalid_expiry_markers_do_not_stop_adoption_and_are_reported_once() {
        let paths = regression_paths("bad-expiry");
        let _cleanup = Cleanup(paths.state.parent().unwrap().to_owned());
        let expired = paths.state.join("expired");
        agent::private_fs::create_private_dir_all(&expired, &paths.state).unwrap();
        let expired_pair = ("expired-sidecar".into(), "session".into());
        let expired_name = journal_name(&expired_pair);
        agent::private_fs::write_private(
            &expired.join(&expired_name),
            &serde_json::to_vec(&expired_pair).unwrap(),
        )
        .unwrap();
        let garbage = "a".repeat(64);
        agent::private_fs::write_private(&expired.join(&garbage), b"garbage").unwrap();
        let mismatch = "b".repeat(64);
        agent::private_fs::write_private(&expired.join(&mismatch), &serde_json::to_vec(&expired_pair).unwrap())
            .unwrap();
        let unreadable = "c".repeat(64);
        std::os::unix::fs::symlink(expired.join(&expired_name), expired.join(&unreadable)).unwrap();
        agent::private_fs::write_private(&expired.join("old copy"), b"foreign").unwrap();
        let temp = format!(".journal-{}-1.tmp", std::process::id());
        agent::private_fs::write_private(&expired.join(&temp), b"partial").unwrap();
        let pair = ("retained-sidecar".into(), "session".into());
        drop(open_session(&paths, &journal_name(&pair), pair.clone()).unwrap());
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
        for _ in 0..2 {
            serving.adopt().await.unwrap();
            assert!(serving.adopted.load(Ordering::Acquire));
            assert!(serving.sessions.lock().await.contains_key(&pair));
            assert_eq!(
                serving.failed.lock().unwrap()[&expired_name].code(),
                tonic::Code::NotFound
            );
        }
        let audit = std::fs::read_to_string(paths.state.join("audit.log")).unwrap();
        for bad in [&garbage, &mismatch, &unreadable, &"old copy".to_owned()] {
            assert_eq!(audit.matches(bad.as_str()).count(), 1, "{audit}");
        }
        assert!(!audit.contains(&temp), "in-progress replacement files must be ignored");
    }

    #[test]
    fn expiry_publishes_the_known_identity_over_a_malformed_identity_file() {
        let paths = regression_paths("expiry-known-identity");
        let _cleanup = Cleanup(paths.state.parent().unwrap().to_owned());
        let pair = ("sidecar".into(), "session".into());
        let name = journal_name(&pair);
        let (session, _) = open_session(&paths, &name, pair.clone()).unwrap();
        session.lock().unwrap().closed = true;
        agent::private_fs::write_private(&paths.state.join("journal").join(&name).join("identity"), b"garbage")
            .unwrap();
        budget(
            &paths,
            vec![session],
            &JournalConfig {
                free_floor_bytes: 0,
                closed_keep: std::time::Duration::ZERO,
                ..Default::default()
            },
            &Clock(std::time::UNIX_EPOCH),
            &|_: &std::path::Path| Ok(u64::MAX),
        )
        .unwrap();
        assert_eq!(expired_identity(&paths, &name).unwrap(), Some(pair));
        assert!(!paths.state.join("journal").join(name).exists());
    }

    #[tokio::test]
    async fn listing_sessions_does_not_wait_for_held_eviction_io() {
        let paths = regression_paths("listing-eviction");
        let _cleanup = Cleanup(paths.state.parent().unwrap().to_owned());
        let pair = ("sidecar".into(), "session".into());
        let (session, _) = open_session(&paths, &journal_name(&pair), pair.clone()).unwrap();
        {
            let mut state = session.lock().unwrap();
            state.closed = true;
            state.journal.append(1, b"event").unwrap();
        }
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
        serving.sessions.lock().await.insert(pair, session);
        let sessions = serving.sessions.lock().await.values().cloned().collect();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let entered = Mutex::new(Some(entered_tx));
            budget(
                &paths,
                sessions,
                &JournalConfig {
                    cap_bytes: u64::MAX,
                    free_floor_bytes: 0,
                    ..Default::default()
                },
                &Clock(std::time::UNIX_EPOCH),
                &|_: &std::path::Path| {
                    if let Some(entered) = entered.lock().unwrap().take() {
                        let _ = entered.send(());
                        release_rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap();
                    }
                    Ok(u64::MAX)
                },
            )
        });
        entered_rx.await.unwrap();
        let listed = tokio::time::timeout(std::time::Duration::from_secs(1), serving.sessions("sidecar")).await;
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        let listed = listed.expect("listing must finish while eviction is still held");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].journal_head, 1);
        assert!(listed[0].closed);
    }

    #[test]
    fn unchanged_closed_journals_are_not_reopened_and_live_appends_are_not_locked_by_eviction() {
        let paths = regression_paths("unchanged-journal");
        let closed_pair = ("sidecar".into(), "closed".into());
        let closed_name = journal_name(&closed_pair);
        let (closed, _) = open_session(&paths, &closed_name, closed_pair).unwrap();
        closed.lock().unwrap().closed = true;
        closed.lock().unwrap().journal.append(1, b"event").unwrap();
        let live_pair = ("sidecar".into(), "live".into());
        let (live, _) = open_session(&paths, &journal_name(&live_pair), live_pair).unwrap();
        let config = JournalConfig {
            cap_bytes: u64::MAX,
            free_floor_bytes: 0,
            ..Default::default()
        };
        let free = |_: &std::path::Path| {
            live.try_lock()
                .expect("maintenance must release the live journal before walking or syncing")
                .journal
                .append(1, b"live event")
                .unwrap();
            Ok(u64::MAX)
        };
        let reopens = std::cell::Cell::new(0);
        budget_with(
            &paths,
            vec![live.clone(), closed.clone()],
            &config,
            &Clock(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100)),
            &free,
            BudgetContext {
                observing: None,
                diagnostics: &Mutex::default(),
                live: &Default::default(),
                failed: None,
                tombstones: None,
                identities: &HashMap::new(),
                measure: &journal_bytes,
                reopen: &|dir, config| {
                    reopens.set(reopens.get() + 1);
                    Journal::open(dir, config)
                },
            },
        )
        .unwrap();
        assert_eq!(
            reopens.get(),
            0,
            "an unchanged reader must keep its existing journal and index"
        );
        assert_eq!(live.lock().unwrap().journal.head(), 1);
        assert_eq!(closed.lock().unwrap().journal.head(), 1);
        assert!(matches!(
            closed.lock().unwrap().journal.read_after(0, 100).unwrap()[0],
            eitri_server::journal::Record::Event { sequence: 1, .. }
        ));
    }

    #[test]
    fn dormant_unclosed_journals_are_not_stamped_as_failed_sessions() {
        let paths = regression_paths("dormant-unclosed");
        let pair = ("sidecar".into(), "session".into());
        let name = journal_name(&pair);
        drop(open_session(&paths, &name, pair).unwrap());
        let owner = paths.state.join("run/other-installation");
        agent::private_fs::create_private_dir_all(&owner, &paths.state).unwrap();
        agent::private_fs::write_private(&owner.join("sc-sidecar.pid"), b"owner record").unwrap();
        let config = JournalConfig {
            cap_bytes: 1,
            free_floor_bytes: 0,
            closed_keep: std::time::Duration::ZERO,
            ..Default::default()
        };
        budget(
            &paths,
            Vec::new(),
            &config,
            &Clock(std::time::UNIX_EPOCH),
            &|_: &std::path::Path| Ok(u64::MAX),
        )
        .unwrap();
        assert!(paths.state.join("journal").join(&name).is_dir());
        assert!(!paths.state.join("journal").join(name).join("closed-at").exists());
    }

    #[test]
    fn changed_readers_reopen_without_observation_exclusion_and_refresh_failures_do_not_stop_the_pass() {
        let paths = regression_paths("refresh-failure");
        let mut sessions = Vec::new();
        for id in ["first", "second"] {
            let pair = ("sidecar".into(), id.into());
            let (session, _) = open_session(&paths, &journal_name(&pair), pair).unwrap();
            session.lock().unwrap().journal.append(1, b"event").unwrap();
            session.lock().unwrap().closed = true;
            sessions.push(session);
        }
        let observing = ObservationLocks::default();
        let reopens = std::cell::Cell::new(0);
        let config = JournalConfig {
            cap_bytes: 1,
            free_floor_bytes: 0,
            ..Default::default()
        };
        budget_with(
            &paths,
            sessions.clone(),
            &config,
            &Clock(std::time::UNIX_EPOCH),
            &|_: &std::path::Path| Ok(u64::MAX),
            BudgetContext {
                observing: Some(&observing),
                diagnostics: &Mutex::default(),
                live: &Default::default(),
                failed: None,
                tombstones: None,
                identities: &HashMap::new(),
                measure: &journal_bytes,
                reopen: &|_, _| {
                    assert!(
                        observing
                            .journal(&journal_name(&("sidecar".into(), "first".into())))
                            .try_lock()
                            .is_ok(),
                        "journal recovery must not hold the observation lock"
                    );
                    reopens.set(reopens.get() + 1);
                    Err(io::Error::other("refresh refused"))
                },
            },
        )
        .unwrap();
        assert_eq!(reopens.get(), 2, "one failed refresh must not skip the next session");
        assert!(sessions
            .iter()
            .all(|session| session.lock().unwrap().error.as_ref().unwrap().code() == tonic::Code::Unavailable));
        let room = JournalConfig {
            cap_bytes: u64::MAX,
            ..config
        };
        budget(
            &paths,
            sessions.clone(),
            &room,
            &Clock(std::time::UNIX_EPOCH),
            &|_: &std::path::Path| Ok(u64::MAX),
        )
        .unwrap();
        assert!(
            sessions.iter().all(|session| {
                let session = session.lock().unwrap();
                !session.refresh_needed && session.error.is_none()
            }),
            "a failed refresh must retry even if the next pass evicts nothing"
        );
    }

    #[tokio::test]
    async fn a_failed_journal_is_stamped_closed_and_expires_without_readoption() {
        let paths = regression_paths("failed-stamp");
        let pair = ("sidecar".into(), "session".into());
        let name = journal_name(&pair);
        drop(open_session(&paths, &name, pair).unwrap());
        let dir = paths.state.join("journal").join(&name);
        agent::private_fs::write_private(&dir.join("poisoned"), b"invalid").unwrap();
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
        let first = serving.observe("sidecar", "session").await.unwrap_err();
        let later = serving.session("sidecar", "session").await.err().unwrap();
        assert_eq!(first.code(), tonic::Code::Unavailable);
        assert_eq!(first.code(), later.code());
        assert_eq!(first.message(), later.message());
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(100);
        let config = JournalConfig {
            cap_bytes: u64::MAX,
            free_floor_bytes: 0,
            closed_keep: std::time::Duration::from_secs(10),
            ..Default::default()
        };
        serving
            .maintain_with(config.clone(), Arc::new(Clock(now)), |_| Ok(u64::MAX))
            .await
            .unwrap();
        assert!(dir.join("closed-at").is_file());
        std::fs::remove_file(dir.join("poisoned")).unwrap();
        serving.adopt().await.unwrap();
        assert!(
            serving.sessions.lock().await.is_empty(),
            "a stamped failure must not be reopened"
        );
        serving
            .maintain_with(config.clone(), Arc::new(Clock(now + config.closed_keep)), |_| {
                Ok(u64::MAX)
            })
            .await
            .unwrap();
        assert!(!dir.exists());
        assert!(!paths.state.join("pending").join(format!("{name}.bin")).exists());
        assert_eq!(
            serving.session("sidecar", "session").await.err().unwrap().code(),
            tonic::Code::NotFound
        );
    }

    #[tokio::test]
    async fn expired_history_keeps_a_tombstone_while_its_sidecar_is_alive() {
        let paths = regression_paths("expired-live-owner");
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        sidecars.test_adopted_self("sidecar", true);
        let pair = ("sidecar".into(), "session".into());
        let name = journal_name(&pair);
        let (session, _) = open_session(&paths, &name, pair.clone()).unwrap();
        session.lock().unwrap().closed = true;
        let serving = Serving::new(paths.clone(), sidecars.clone());
        serving.sessions.lock().await.insert(pair, session.clone());
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(100);
        let config = JournalConfig {
            cap_bytes: u64::MAX,
            free_floor_bytes: 0,
            closed_keep: std::time::Duration::from_secs(10),
            ..Default::default()
        };
        serving
            .maintain_with(config.clone(), Arc::new(Clock(now)), |_| Ok(u64::MAX))
            .await
            .unwrap();
        serving
            .maintain_with(config.clone(), Arc::new(Clock(now + config.closed_keep)), |_| {
                Ok(u64::MAX)
            })
            .await
            .unwrap();
        for _ in 0..2 {
            assert_eq!(
                serving.observe("sidecar", "session").await.unwrap_err().code(),
                tonic::Code::NotFound
            );
            assert_eq!(
                serving.session("sidecar", "session").await.err().unwrap().code(),
                tonic::Code::NotFound
            );
            serving.adopt().await.unwrap();
            assert!(!paths.state.join("journal").join(&name).exists());
        }
        drop(serving);
        let restarted = Serving::new(paths.clone(), sidecars);
        // Direct watch before adoption and a watch after adoption both retain terminal expiry.
        assert_eq!(
            restarted.observe("sidecar", "session").await.unwrap_err().code(),
            tonic::Code::NotFound
        );
        restarted.adopt().await.unwrap();
        assert_eq!(
            restarted.session("sidecar", "session").await.err().unwrap().code(),
            tonic::Code::NotFound
        );
        assert!(!paths.state.join("journal").join(&name).exists());
        assert!(!paths.state.join("pending").join(format!("{name}.bin")).exists());
        let marker = paths.state.join("expired").join(&name);
        assert_eq!(std::fs::metadata(&marker).unwrap().mode() & 0o777, 0o600);
        drop(restarted);
        let adopted = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
        adopted.adopt().await.unwrap();
        assert_eq!(
            adopted.failed.lock().unwrap()[&name].code(),
            tonic::Code::NotFound,
            "adoption must load terminal state without a preceding watch"
        );
        let refused = replay::open(
            session,
            claude_runtime_protocol::v1::WatchSessionEventsRequest {
                session_id: "session".into(),
                start: claude_runtime_protocol::v1::ReplayStart::AfterSequence as i32,
                after_sequence: Some(100),
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            refused.code(),
            tonic::Code::NotFound,
            "expired cursors must get the terminal refusal"
        );
    }

    #[tokio::test]
    async fn a_stamped_transient_open_failure_is_retried_by_adoption() {
        for errno in [libc::ENOSPC, libc::EMFILE] {
            let paths = regression_paths("transient-adoption");
            let identity = ("sidecar".into(), "session".into());
            let name = journal_name(&identity);
            let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
            assert!(serving
                .observe_with("sidecar", "session", move |paths, name, pair| {
                    drop(open_session(paths, name, pair)?);
                    Err(io::Error::from_raw_os_error(errno))
                })
                .await
                .is_err());
            serving
                .maintain_with(
                    JournalConfig {
                        free_floor_bytes: 0,
                        ..Default::default()
                    },
                    Arc::new(Clock(std::time::UNIX_EPOCH)),
                    |_| Ok(u64::MAX),
                )
                .await
                .unwrap();
            assert!(paths.state.join("journal").join(&name).join("closed-at").exists());
            serving.adopt().await.unwrap();
            assert!(serving.sessions.lock().await.contains_key(&identity));
            assert!(!serving.failed.lock().unwrap().contains_key(&name));
        }
    }

    #[test]
    fn eviction_excludes_only_the_journals_it_can_change() {
        let paths = regression_paths("eviction-observe");
        let pair = ("sidecar".into(), "closed".into());
        let name = journal_name(&pair);
        let (session, _) = open_session(&paths, &name, pair).unwrap();
        session.lock().unwrap().closed = true;
        let observing = ObservationLocks::default();
        let checked = std::cell::Cell::new(false);
        let config = JournalConfig {
            free_floor_bytes: 0,
            cap_bytes: u64::MAX,
            ..Default::default()
        };
        budget_with(
            &paths,
            vec![session],
            &config,
            &Clock(std::time::UNIX_EPOCH),
            &|_: &std::path::Path| {
                assert!(
                    observing.journal(&name).try_lock().is_err(),
                    "same journal must be excluded"
                );
                assert!(
                    observing.journal("new-session").try_lock().is_ok(),
                    "new sessions must remain observable during enforcement"
                );
                checked.set(true);
                Ok(u64::MAX)
            },
            BudgetContext {
                observing: Some(&observing),
                diagnostics: &Mutex::default(),
                live: &Default::default(),
                failed: None,
                tombstones: None,
                identities: &HashMap::new(),
                measure: &journal_bytes,
                reopen: &Journal::open,
            },
        )
        .unwrap();
        assert!(checked.get());
    }

    #[test]
    fn foreign_and_vanished_journal_entries_do_not_abort_maintenance() {
        let paths = regression_paths("foreign-entries");
        let pair = ("sidecar".into(), "closed".into());
        let name = journal_name(&pair);
        let (session, _) = open_session(&paths, &name, pair).unwrap();
        session.lock().unwrap().closed = true;
        let vanished = "1".repeat(64);
        let foreign = "2".repeat(64);
        for name in [&vanished, &foreign] {
            agent::private_fs::create_private_dir_all(&paths.state.join("journal").join(name), &paths.state).unwrap();
        }
        let checked = std::cell::RefCell::new(Vec::new());
        budget_with(
            &paths,
            vec![session],
            &JournalConfig {
                free_floor_bytes: 0,
                closed_keep: std::time::Duration::ZERO,
                ..Default::default()
            },
            &Clock(std::time::UNIX_EPOCH),
            &|_: &std::path::Path| Ok(u64::MAX),
            BudgetContext {
                observing: None,
                diagnostics: &Mutex::default(),
                live: &Default::default(),
                failed: None,
                tombstones: None,
                identities: &HashMap::new(),
                measure: &|dir| {
                    let name = dir.file_name().unwrap().to_str().unwrap();
                    if name == vanished {
                        checked.borrow_mut().push(name.to_owned());
                        std::fs::remove_dir(dir).unwrap();
                        journal_bytes(dir)
                    } else if name == foreign {
                        checked.borrow_mut().push(name.to_owned());
                        Err(io::Error::from_raw_os_error(libc::EACCES))
                    } else {
                        journal_bytes(dir)
                    }
                },
                reopen: &Journal::open,
            },
        )
        .unwrap();
        assert!(!paths.state.join("journal").join(&name).exists());
        checked.borrow_mut().sort();
        assert_eq!(*checked.borrow(), [vanished, foreign]);
    }

    #[tokio::test]
    async fn another_daemons_expired_sessions_are_forgotten_after_their_sidecar_record_expires() {
        let paths = regression_paths("foreign-tombstone");
        let pair = ("sidecar".into(), "session".into());
        let name = journal_name(&pair);
        let (session, _) = open_session(&paths, &name, pair).unwrap();
        session.lock().unwrap().closed = true;
        let other = paths.state.join("run/other");
        agent::private_fs::create_private_dir_all(&other, &paths.state).unwrap();
        let record = other.join("sc-sidecar.pid");
        agent::private_fs::write_private(&record, b"record").unwrap();
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
        serving.adopt().await.unwrap();
        let config = JournalConfig {
            free_floor_bytes: 0,
            closed_keep: std::time::Duration::ZERO,
            ..Default::default()
        };
        let live = agent::private_fs::PrivateLock::open(&other.join("live.lock")).unwrap();
        assert!(live.try_lock().unwrap());
        agent::private_fs::write_private(&paths.state.join("journal").join(&name).join("closed-at"), b"0").unwrap();
        fn foreign_eviction(state: &std::path::Path) -> io::Result<u64> {
            use eitri_server::journal::{enforce_budget_with, BudgetOutcome, Disk, SessionUsage};
            let name = journal_name(&("sidecar".into(), "session".into()));
            let result = enforce_budget_with(
                state,
                &[SessionUsage {
                    session: name,
                    bytes: 0,
                    closed_at: Some(std::time::UNIX_EPOCH),
                }],
                &JournalConfig {
                    free_floor_bytes: 0,
                    closed_keep: std::time::Duration::ZERO,
                    ..Default::default()
                },
                &|_: &std::path::Path| Ok(u64::MAX),
                &Disk,
                &Clock(std::time::UNIX_EPOCH),
            );
            assert!(matches!(result, BudgetOutcome::Within | BudgetOutcome::Evicted(_)));
            Ok(u64::MAX)
        }
        serving
            .maintain_with(config.clone(), Arc::new(Clock(std::time::UNIX_EPOCH)), foreign_eviction)
            .await
            .unwrap();
        assert!(
            !serving.failed.lock().unwrap().contains_key(&name),
            "another daemon's deletion must not create an ownerless failure"
        );
        assert!(paths.state.join("expired").join(&name).exists());
        serving.adopt().await.unwrap();
        assert!(serving.failed.lock().unwrap().contains_key(&name));
        drop(live);
        std::fs::remove_file(record).unwrap();
        serving
            .maintain_with(config, Arc::new(Clock(std::time::UNIX_EPOCH)), |_| Ok(u64::MAX))
            .await
            .unwrap();
        assert!(!serving.failed.lock().unwrap().contains_key(&name));
        assert!(!serving.tombstones.lock().unwrap().contains_key(&name));
        assert!(!paths.state.join("expired").join(name).exists());
    }

    #[tokio::test]
    async fn maintenance_diagnostics_are_audited_once_per_kind_for_the_run() {
        let paths = regression_paths("budget-diagnostics");
        let pair = ("sidecar".into(), "session".into());
        let name = journal_name(&pair);
        let (session, _) = open_session(&paths, &name, pair.clone()).unwrap();
        session.lock().unwrap().closed = true;
        agent::private_fs::create_private_dir_all(
            &paths.state.join("journal").join(name).join("closed-at"),
            &paths.state,
        )
        .unwrap();
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
        serving.sessions.lock().await.insert(pair, session);
        for _ in 0..3 {
            serving
                .maintain_with(
                    JournalConfig {
                        cap_bytes: u64::MAX,
                        free_floor_bytes: 0,
                        ..Default::default()
                    },
                    Arc::new(Clock(std::time::UNIX_EPOCH)),
                    |_| Ok(u64::MAX),
                )
                .await
                .unwrap();
            serving.audit_budget_error("first budget failure").await;
            serving.audit_budget_error("second budget failure").await;
        }
        let audit = std::fs::read_to_string(paths.state.join("audit.log")).unwrap();
        assert_eq!(audit.lines().filter(|line| line.contains("close timestamp")).count(), 1);
        assert_eq!(
            audit
                .lines()
                .filter(|line| line.contains("first budget failure"))
                .count(),
            1
        );
        assert_eq!(
            audit
                .lines()
                .filter(|line| line.contains("second budget failure"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn observation_failure_is_retryable_and_retained_dead_sessions_are_adopted() {
        let paths = regression_paths("retry-dead");
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
        let identity = ("sidecar".to_owned(), "session".to_owned());
        let name = journal_name(&identity);
        serving.fail(&name, "disk temporarily full");
        assert_eq!(
            serving.session("sidecar", "session").await.err().unwrap().code(),
            tonic::Code::Unavailable
        );
        use futures_util::StreamExt;
        use prost::Message;
        let (original, _) = open_session(&paths, &name, identity).unwrap();
        original
            .lock()
            .unwrap()
            .journal
            .append(
                1,
                &claude_runtime_protocol::v1::SessionEvent {
                    session_id: "session".into(),
                    sequence: 1,
                    ..Default::default()
                }
                .encode_to_vec(),
            )
            .unwrap();
        drop(original);
        serving.adopt().await.unwrap();
        let session = serving.session("sidecar", "session").await.unwrap();
        assert_eq!(
            session.lock().unwrap().error.as_ref().unwrap().code(),
            tonic::Code::NotFound
        );
        let response = replay::open(
            session,
            claude_runtime_protocol::v1::WatchSessionEventsRequest {
                session_id: "session".into(),
                start: claude_runtime_protocol::v1::ReplayStart::AfterSequence as i32,
                after_sequence: Some(0),
            },
        )
        .await
        .unwrap();
        let stream = response.into_inner();
        tokio::pin!(stream);
        let event = claude_runtime_protocol::v1::SessionEvent::decode(stream.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(event.sequence, 1);
        let gone = stream.next().await.unwrap().unwrap_err();
        assert_eq!(gone.code(), tonic::Code::NotFound);
        assert_eq!(
            claude_runtime_protocol::v1::ErrorDetail::decode(gone.details())
                .unwrap()
                .code,
            claude_runtime_protocol::v1::ErrorCode::SessionNotFound as i32
        );
    }

    #[tokio::test]
    async fn unknown_sidecar_is_retryable_until_initial_adoption_finishes() {
        let paths = regression_paths("adopting");
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths).unwrap()));
        assert_eq!(serving.sidecar("missing").unwrap_err().code(), tonic::Code::Unavailable);
        serving.adopt().await.unwrap();
        assert_eq!(serving.sidecar("missing").unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn failed_identity_publication_retries_from_memory_and_from_a_later_watch() {
        let paths = regression_paths("identity-retry");
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        sidecars.test_adopted_self("sidecar", true);
        let serving = Serving::new(paths.clone(), sidecars.clone());
        let pair = ("sidecar".to_owned(), "session".to_owned());
        let dir = paths.state.join("journal").join(journal_name(&pair));
        agent::private_fs::create_private_dir_all(&dir.join("identity"), &paths.state).unwrap();
        assert!(serving.observe("sidecar", "session").await.is_err());
        assert_eq!(
            serving.session("sidecar", "session").await.err().unwrap().code(),
            tonic::Code::Unavailable
        );
        std::fs::remove_dir(dir.join("identity")).unwrap();
        serving.adopt().await.unwrap();
        assert!(serving.session("sidecar", "session").await.is_ok());
        drop(serving);
        std::fs::remove_file(dir.join("identity")).unwrap();
        let restarted = Serving::new(paths, sidecars);
        assert!(restarted.session("sidecar", "session").await.is_ok());
        assert!(dir.join("identity").is_file());
    }

    #[tokio::test]
    async fn an_adopted_live_sidecar_is_retryable_until_its_handshake_is_ready() {
        let paths = regression_paths("handshake-retry");
        let sidecars = Arc::new(Sidecars::new(paths.clone()).unwrap());
        sidecars.test_adopted_self("sidecar", false);
        let serving = Serving::new(paths, sidecars);
        serving.adopt().await.unwrap();
        assert_eq!(serving.sidecar("sidecar").unwrap_err().code(), tonic::Code::Unavailable);
    }

    struct Clock(std::time::SystemTime);
    impl eitri_server::journal::Clock for Clock {
        fn elapsed(&self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
        fn now(&self) -> std::time::SystemTime {
            self.0
        }
    }

    #[tokio::test]
    async fn maintenance_tick_warns_and_removes_expired_sessions_from_the_registry() {
        let paths = regression_paths("budget-tick");
        let pair = ("sidecar".to_owned(), "session".to_owned());
        let name = journal_name(&pair);
        let (session, _) = open_session(&paths, &name, pair.clone()).unwrap();
        session.lock().unwrap().journal.append(1, b"event").unwrap();
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
        serving.sessions.lock().await.insert(pair, session.clone());
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(100);
        let config = JournalConfig {
            cap_bytes: 1,
            free_floor_bytes: 0,
            closed_keep: std::time::Duration::from_secs(10),
            ..Default::default()
        };
        serving
            .maintain_with(config.clone(), Arc::new(Clock(now)), |_| Ok(u64::MAX))
            .await
            .unwrap();
        assert_eq!(serving.warnings().len(), 1);
        session.lock().unwrap().closed = true;
        let room = JournalConfig {
            cap_bytes: u64::MAX,
            ..config
        };
        serving
            .maintain_with(room.clone(), Arc::new(Clock(now)), |_| Ok(u64::MAX))
            .await
            .unwrap();
        assert!(serving.warnings().is_empty());
        serving
            .maintain_with(room.clone(), Arc::new(Clock(now + room.closed_keep)), |_| Ok(u64::MAX))
            .await
            .unwrap();
        assert!(!paths.state.join("journal").join(name).exists());
        assert!(serving.sessions.lock().await.is_empty());
    }

    #[test]
    fn maintenance_expires_closed_history_and_reports_live_pressure() {
        use eitri_server::journal::Record;
        let paths = regression_paths("budget");
        let identity = ("sidecar".into(), "session".into());
        let name = journal_name(&identity);
        let (session, _) = open_session(&paths, &name, identity).unwrap();
        session.lock().unwrap().journal.append(1, b"recorded event").unwrap();
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(100);
        let config = JournalConfig {
            cap_bytes: 1,
            free_floor_bytes: 0,
            closed_keep: std::time::Duration::from_secs(10),
            ..Default::default()
        };
        assert!(budget(
            &paths,
            vec![session.clone()],
            &config,
            &Clock(now),
            &|_: &std::path::Path| Ok(u64::MAX)
        )
        .unwrap()
        .is_some());
        assert!(matches!(
            session.lock().unwrap().journal.read_after(0, 100).unwrap()[0],
            Record::Event { sequence: 1, .. }
        ));
        session.lock().unwrap().closed = true;
        let room = JournalConfig {
            cap_bytes: u64::MAX,
            ..config.clone()
        };
        assert!(budget(
            &paths,
            vec![session.clone()],
            &room,
            &Clock(now),
            &|_: &std::path::Path| Ok(u64::MAX)
        )
        .unwrap()
        .is_none());
        assert!(paths.state.join("journal").join(&name).exists());
        budget(
            &paths,
            vec![session.clone()],
            &room,
            &Clock(now + room.closed_keep),
            &|_: &std::path::Path| Ok(u64::MAX),
        )
        .unwrap();
        assert!(!paths.state.join("journal").join(&name).exists());
        assert!(!paths.state.join("pending").join(format!("{name}.bin")).exists());
        assert!(session.lock().unwrap().journal.read_after(0, 100).unwrap().is_empty());
    }

    #[test]
    fn dormant_daemon_history_expires_only_while_its_lifetime_lock_is_free() {
        let paths = regression_paths("dormant-history");
        let pair = ("sidecar".into(), "session".into());
        let name = journal_name(&pair);
        drop(open_session(&paths, &name, pair).unwrap());
        let dir = paths.state.join("journal").join(&name);
        agent::private_fs::write_private(&dir.join("closed-at"), b"0").unwrap();
        let owner = paths.state.join("run/old-installation");
        agent::private_fs::create_private_dir_all(&owner, &paths.state).unwrap();
        agent::private_fs::write_private(&owner.join("sc-sidecar.pid"), b"owner record").unwrap();
        let live = agent::private_fs::PrivateLock::open(&owner.join("live.lock")).unwrap();
        assert!(live.try_lock().unwrap());
        let config = JournalConfig {
            cap_bytes: u64::MAX,
            free_floor_bytes: 0,
            closed_keep: std::time::Duration::from_secs(10),
            ..Default::default()
        };
        let now = Clock(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100));
        budget(&paths, Vec::new(), &config, &now, &|_: &std::path::Path| Ok(u64::MAX)).unwrap();
        assert!(dir.exists());
        drop(live);
        budget(&paths, Vec::new(), &config, &now, &|_: &std::path::Path| Ok(u64::MAX)).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn a_terminal_observation_refusal_keeps_a_running_sessions_history() {
        let paths = regression_paths("refused-live");
        let pair = ("sidecar".into(), "session".into());
        let name = journal_name(&pair);
        let (session, _) = open_session(&paths, &name, pair).unwrap();
        {
            let mut state = session.lock().unwrap();
            state.journal.append(1, b"event").unwrap();
            state.error = Some(Status::failed_precondition("session_observation_ended"));
        }
        let config = JournalConfig {
            cap_bytes: 1,
            free_floor_bytes: 0,
            ..Default::default()
        };
        assert!(budget(
            &paths,
            vec![session.clone()],
            &config,
            &Clock(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100)),
            &|_: &std::path::Path| Ok(u64::MAX)
        )
        .unwrap()
        .is_some());
        assert!(matches!(
            session.lock().unwrap().journal.read_after(0, 100).unwrap()[0],
            eitri_server::journal::Record::Event { sequence: 1, .. }
        ));
        assert!(!paths.state.join("journal").join(name).join("closed-at").exists());
    }

    #[tokio::test]
    async fn retention_reads_closed_stamps_even_when_a_journal_cannot_be_opened() {
        let paths = regression_paths("unopened-closed");
        let pair = ("sidecar".into(), "session".into());
        let name = journal_name(&pair);
        let (session, _) = open_session(&paths, &name, pair.clone()).unwrap();
        session.lock().unwrap().journal.append(1, b"event").unwrap();
        drop(session);
        let dir = paths.state.join("journal").join(&name);
        let segment = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "log"))
            .unwrap();
        agent::private_fs::write_private(&segment, b"broken").unwrap();
        agent::private_fs::write_private(&dir.join("closed-at"), b"0").unwrap();
        let config = JournalConfig {
            cap_bytes: u64::MAX,
            free_floor_bytes: 0,
            closed_keep: std::time::Duration::from_secs(10),
            ..Default::default()
        };
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths.clone()).unwrap()));
        serving.adopt().await.unwrap();
        assert_eq!(
            serving.session("sidecar", "session").await.err().unwrap().code(),
            tonic::Code::Unavailable
        );
        serving
            .maintain_with(
                config,
                Arc::new(Clock(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100))),
                |_| Ok(u64::MAX),
            )
            .await
            .unwrap();
        assert!(!dir.exists());
        assert_eq!(
            serving.session("sidecar", "session").await.err().unwrap().code(),
            tonic::Code::NotFound
        );
    }

    #[test]
    fn a_failed_close_stamp_does_not_block_expiry_of_other_sessions() {
        let paths = regression_paths("close-stamp");
        let bad_pair = ("sidecar".into(), "bad".into());
        let good_pair = ("sidecar".into(), "good".into());
        let bad = journal_name(&bad_pair);
        let good = journal_name(&good_pair);
        let (failed, _) = open_session(&paths, &bad, bad_pair).unwrap();
        let (expired, _) = open_session(&paths, &good, good_pair).unwrap();
        failed.lock().unwrap().closed = true;
        expired.lock().unwrap().closed = true;
        agent::private_fs::create_private_dir_all(
            &paths.state.join("journal").join(&bad).join("closed-at"),
            &paths.state,
        )
        .unwrap();
        agent::private_fs::write_private(&paths.state.join("journal").join(&good).join("closed-at"), b"0").unwrap();
        let config = JournalConfig {
            cap_bytes: u64::MAX,
            free_floor_bytes: 0,
            closed_keep: std::time::Duration::from_secs(10),
            ..Default::default()
        };
        budget(
            &paths,
            vec![failed, expired],
            &config,
            &Clock(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100)),
            &|_: &std::path::Path| Ok(u64::MAX),
        )
        .unwrap();
        assert!(paths.state.join("journal").join(bad).exists());
        assert!(!paths.state.join("journal").join(good).exists());
    }

    #[tokio::test]
    async fn adoption_leaves_another_installations_live_journal_alone() {
        let paths = regression_paths("other-daemon");
        let identity = ("sidecar".to_owned(), "session".to_owned());
        let name = journal_name(&identity);
        drop(open_session(&paths, &name, identity).unwrap());
        let other = paths.state.join("run/other-installation");
        agent::private_fs::create_private_dir_all(&other, &paths.state).unwrap();
        agent::private_fs::write_private(&other.join("sc-sidecar.pid"), b"record").unwrap();
        let serving = Serving::new(paths.clone(), Arc::new(Sidecars::new(paths).unwrap()));
        serving.adopt().await.unwrap();
        assert!(serving.sessions.lock().await.is_empty());
    }

    #[test]
    fn adopting_a_session_with_its_identity_file_in_place_writes_nothing() {
        let paths = paths("adopt");
        let identity = ("sidecar".to_owned(), "session".to_owned());
        let name = journal_name(&identity);
        let file = paths.state.join("journal").join(&name).join("identity");
        drop(open_session(&paths, &name, identity.clone()).unwrap());
        let first = std::fs::metadata(&file).unwrap().ino();
        // A second adoption (a daemon restart) must not replace the file: publishing needs room on the disk.
        drop(open_session(&paths, &name, identity.clone()).unwrap());
        assert_eq!(std::fs::metadata(&file).unwrap().ino(), first);
        // An identity file that holds something else is put right.
        std::fs::write(&file, b"[\"other\",\"pair\"]").unwrap();
        drop(open_session(&paths, &name, identity.clone()).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), serde_json::to_vec(&identity).unwrap());
        std::fs::remove_file(&file).unwrap();
        drop(open_session(&paths, &name, identity.clone()).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), serde_json::to_vec(&identity).unwrap());
    }
}
