//! Waiting for the mirror on the way to a provider, or for bytes written by an editor tool.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent::remote_client::{WaitFileHashRequest, WaitFileHashResponse};

use super::actor::SyncHandle;
use super::engine::{FlushOutcome, SideCounts};
use super::ignores::Matcher;
use super::status_schema::LateLine;
use crate::remote_project::RemoteConnection;
use crate::turn_review::write::{read_current_within, Current, ProjectDir};

#[derive(Debug, PartialEq, Eq)]
pub enum LateWhy {
    TimedOut,
    Conflict,
    Problem(String),
    Excluded,
    Unconfirmed,
    Incomplete,
    Unavailable(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendSync {
    Ready,
    Cancelled,
    Late { files: Vec<String>, why: LateWhy },
}

impl SendSync {
    fn late(paths: &[PathBuf], why: LateWhy) -> Self {
        Self::Late {
            files: paths.iter().map(|p| p.display().to_string()).collect(),
            why,
        }
    }

    fn line(self, host: &str) -> Option<String> {
        let Self::Late { files, why } = self else { return None };
        let pending = if files.is_empty() {
            format!("sync with {host} has not caught up")
        } else {
            format!("your last save of {} has not reached {host}", files.join(", "))
        };
        Some(match why {
            LateWhy::Conflict | LateWhy::TimedOut => pending,
            LateWhy::Problem(reason) => format!("{pending}; {reason}"),
            LateWhy::Excluded => format!("your last save of {} is excluded from sync", files.join(", ")),
            LateWhy::Unavailable(reason) => format!("{pending}; sync is unavailable: {reason}"),
            LateWhy::Unconfirmed => format!("the conflict state on {host} could not be confirmed"),
            LateWhy::Incomplete => "the list of conflicts and sync problems Mutagen gave is incomplete".into(),
        })
    }
}

pub fn before_send(sync: &dyn SyncHandle, paths: &[PathBuf], cap: Duration) -> SendSync {
    before_send_unless_withheld(sync, paths, cap, &AtomicU8::new(LIVE))
}

/// The flush, then the tree session's conflicts as the engine reports them once it has returned: a flush that
/// returns has run a cycle, so the named status read next holds that cycle's conflicts. Both status reads give up at
/// the send's deadline. The instance is read before the flush too: a tree re-created under the same name between
/// them, or one with no completed cycle, has no conflict list that speaks for the saves, so the state is unconfirmed.
fn before_send_unless_withheld(sync: &dyn SyncHandle, paths: &[PathBuf], cap: Duration, wait: &AtomicU8) -> SendSync {
    let deadline = Instant::now().checked_add(cap);
    let remaining = || deadline.map_or(cap, |deadline| deadline.saturating_duration_since(Instant::now()));
    let withheld = || wait.load(Ordering::Acquire) != LIVE;
    if withheld() {
        return SendSync::Cancelled;
    }
    let before = sync.tree_status(remaining()).ok().map(|status| status.identifier);
    if withheld() {
        return SendSync::Cancelled;
    }
    match sync.flush_tree(remaining()) {
        FlushOutcome::TimedOut => return SendSync::late(paths, LateWhy::TimedOut),
        FlushOutcome::Failed(reason) => return SendSync::late(paths, LateWhy::Unavailable(reason)),
        FlushOutcome::Flushed => {}
    }
    if withheld() {
        return SendSync::Cancelled;
    }
    let status = match remaining() {
        left if left.is_zero() => None,
        left => sync.tree_status(left).ok(),
    };
    let Some(status) = status.filter(|status| {
        status.successful_cycles > 0
            && !status.identifier.is_empty()
            && before.as_deref() == Some(status.identifier.as_str())
    }) else {
        return SendSync::late(paths, LateWhy::Unconfirmed);
    };
    if paths.is_empty() {
        return SendSync::Ready;
    }
    let matcher = match sync.matcher() {
        Ok(matcher) => matcher,
        Err(reason) => return SendSync::late(paths, LateWhy::Unavailable(reason)),
    };
    let excluded = |p: &Path| tree_excluded(Some(&matcher), p, false);
    let mut pending = Vec::new();
    let mut problem = None;
    let mut conflicted = false;
    for path in paths {
        if let Some(found) = status.problems.iter().find(|p| path.starts_with(&p.path)) {
            problem.get_or_insert(found);
        } else if status.conflicts.iter().any(|c| path.starts_with(&c.path)) {
            conflicted = true;
        } else if !excluded(path) {
            continue;
        }
        pending.push(path.clone());
    }
    if (status.excluded_conflicts > 0
        || omitted_problems(&status.excluded_scan_problems, &status.excluded_transition_problems))
        && pending.len() < paths.len()
    {
        SendSync::late(paths, LateWhy::Incomplete)
    } else if !pending.is_empty() {
        let why = if let Some(problem) = problem {
            LateWhy::Problem(problem_detail(problem))
        } else if conflicted {
            LateWhy::Conflict
        } else {
            LateWhy::Excluded
        };
        SendSync::late(&pending, why)
    } else {
        SendSync::Ready
    }
}

pub(super) fn omitted_problems(scan: &SideCounts, transition: &SideCounts) -> bool {
    [scan, transition]
        .iter()
        .any(|count| count.laptop > 0 || count.server > 0)
}

pub(super) fn problem_detail(problem: &super::engine::Problem) -> String {
    let side = match problem.side {
        super::engine::Side::Laptop => "laptop",
        super::engine::Side::Server => "server",
    };
    let path = if problem.path.is_empty() { "." } else { &problem.path };
    format!("{side} sync problem at {path}: {}", problem.text)
}

pub type SavedPaths = Arc<dyn Fn(u64, Duration) -> Result<(u64, Vec<PathBuf>), String> + Send + Sync>;

#[derive(Clone)]
pub struct SendPort {
    pub key: String,
    pub root: PathBuf,
    pub sync: Result<Arc<dyn SyncHandle>, String>,
    pub saved: SavedPaths,
    pub host: String,
    pub cap: Duration,
}

const MAX_LINES: usize = 50;

/// A waiting message goes on ([`LIVE`]) until the person stops it ([`STOPPED`]) or its session ends ([`ENDED`]);
/// the first change wins, and none applies once the worker has decided to hand the message over.
const LIVE: u8 = 0;
const STOPPED: u8 = 1;
const ENDED: u8 = 2;

struct WaitingMessage {
    id: String,
    prompt: String,
    state: Arc<AtomicU8>,
}

impl WaitingMessage {
    fn take_back(&self, to: u8) -> bool {
        self.state
            .compare_exchange(LIVE, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Why a message taken back during its wait was not sent, as the provider's worker answers it.
fn withheld_reason(state: &AtomicU8) -> Option<String> {
    match state.load(Ordering::Acquire) {
        LIVE => None,
        STOPPED => Some(agent::STOPPED_BEFORE_SEND.into()),
        _ => Some(agent::SESSION_ENDED_BEFORE_SEND.into()),
    }
}

#[derive(Default)]
struct Progress {
    generation: u64,
    waiting: Vec<WaitingMessage>,
    stopped: bool,
    last_reset: u64,
    restored: Vec<String>,
    pending_cursors: std::collections::HashMap<String, u64>,
    changed: bool,
    lines: Vec<LateLine>,
}

/// One tab's cursor in the editor's saves and the notices that belong to its messages.
/// Workers never hold the shared lock while asking the editor, disk, or sync engine.
#[derive(Clone, Default)]
pub struct TabSync {
    progress: Arc<Mutex<Progress>>,
}

pub struct SendDisplay {
    pub waiting: bool,
    pub stopped: bool,
    pub lines: Vec<LateLine>,
    pub restored: Vec<String>,
}

#[derive(Clone)]
pub struct MessageSync {
    pub tab: TabSync,
    pub port: SendPort,
    pub wake: Option<agent::UiWake>,
}

impl MessageSync {
    pub fn wait(self, id: &str, seq: Option<u64>, started_at: u64, prompt: &str) -> agent::BeforeSend {
        self.tab.wait(
            self.port,
            LateLine {
                message_id: id.into(),
                seq,
                started_at: seq.is_none().then_some(started_at),
                prompt: prompt.into(),
                text: String::new(),
            },
            self.wake,
        )
    }
}

impl TabSync {
    /// A refused provider call leaves its saves for a retry and withdraws its notice with its message.
    pub fn settle(&self, id: &str, accepted: bool) {
        let mut state = self.progress.lock().unwrap();
        let cursor = state.pending_cursors.remove(id);
        if accepted {
            if let Some(cursor) = cursor {
                state.generation = state.generation.max(cursor);
            }
        } else {
            let count = state.lines.len();
            state.lines.retain(|line| line.message_id != id);
            state.changed |= state.lines.len() != count;
        }
    }

    pub fn delivered(&self, id: &str, seq: u64) {
        let mut state = self.progress.lock().unwrap();
        if let Some(cursor) = state.pending_cursors.remove(id) {
            state.generation = state.generation.max(cursor);
        }
        if let Some(line) = state
            .lines
            .iter_mut()
            .find(|line| line.message_id == id && line.seq != Some(seq))
        {
            line.seq = Some(seq);
            state.changed = true;
        }
    }
    pub fn lines(&self) -> Vec<LateLine> {
        self.progress.lock().unwrap().lines.clone()
    }

    pub fn restore(&self, lines: &[LateLine]) {
        let mut state = self.progress.lock().unwrap();
        state.lines = lines[lines.len().saturating_sub(MAX_LINES)..].to_vec();
        state.restored = state.lines.iter().map(|line| line.message_id.clone()).collect();
        state.changed = true;
    }

    pub fn recover(&self, projection: &agent::AgentSessionProjection) {
        for (id, outcome) in &projection.steer_outcomes {
            match outcome {
                agent::SteerOutcome::Delivered { seq } => self.delivered(id, *seq),
                agent::SteerOutcome::Dropped { .. } => self.settle(id, false),
            }
        }
        self.reset_before(projection.last_reset_revision);
    }

    pub fn reset_before(&self, revision: u64) {
        let mut state = self.progress.lock().unwrap();
        if state.last_reset < revision {
            state.last_reset = revision;
            state.stopped = false;
            state
                .lines
                .retain(|line| line.seq.or(line.started_at).is_some_and(|seq| seq >= revision));
            state.changed = true;
        }
    }

    /// The person's Stop: each message still waiting is answered [`agent::STOPPED_BEFORE_SEND`], and its id and text
    /// come back here for the caller to put where it was typed. The lock orders this with the worker's decision to
    /// hand its request to the provider.
    pub fn cancel_waits(&self) -> Vec<(String, String)> {
        let mut state = self.progress.lock().unwrap();
        let stopped: Vec<_> = state
            .waiting
            .iter()
            .filter(|wait| wait.take_back(STOPPED))
            .map(|wait| (wait.id.clone(), wait.prompt.clone()))
            .collect();
        if !stopped.is_empty() {
            state.changed = true;
            state.stopped = true;
        }
        stopped
    }

    /// The tab's session failed: each message still waiting is answered [`agent::SESSION_ENDED_BEFORE_SEND`], a
    /// refusal that returns its text the way any refusal does, and none reaches the session that failed.
    pub fn end_waits(&self) {
        let mut state = self.progress.lock().unwrap();
        let ended = state.waiting.iter().filter(|wait| wait.take_back(ENDED)).count();
        state.changed |= ended > 0;
    }

    pub fn display(&self, changes_only: bool) -> Option<SendDisplay> {
        let mut state = self.progress.lock().unwrap();
        if changes_only && !state.changed {
            return None;
        }
        if changes_only {
            state.changed = false;
        }
        Some(SendDisplay {
            waiting: state.waiting.iter().any(|w| w.state.load(Ordering::Acquire) == LIVE),
            stopped: state.stopped,
            lines: state.lines.clone(),
            restored: state.restored.clone(),
        })
    }

    pub fn wait(&self, port: SendPort, mut line: LateLine, wake: Option<agent::UiWake>) -> agent::BeforeSend {
        struct Waiting(TabSync, Option<agent::UiWake>, String);
        impl Drop for Waiting {
            fn drop(&mut self) {
                let mut state = self.0.progress.lock().unwrap();
                state.waiting.retain(|w| w.id != self.2);
                state.changed = true;
                drop(state);
                if let Some(wake) = &self.1 {
                    wake();
                }
            }
        }
        let wait_state = Arc::new(AtomicU8::new(LIVE));
        {
            let mut state = self.progress.lock().unwrap();
            state.stopped = false;
            state.waiting.push(WaitingMessage {
                id: line.message_id.clone(),
                prompt: line.prompt.clone(),
                state: wait_state.clone(),
            });
            state.changed = true;
        }
        let waiting = Waiting(self.clone(), wake, line.message_id.clone());
        Box::new(move || {
            let start = Instant::now();
            let cursor = waiting.0.progress.lock().unwrap().generation;
            if let Some(why) = withheld_reason(&wait_state) {
                return Err(why);
            }
            let saved = (port.saved)(cursor, port.cap);
            if let Some(why) = withheld_reason(&wait_state) {
                return Err(why);
            }
            let unknown_saves = saved.as_ref().err().cloned();
            let (generation, paths) = saved.unwrap_or_else(|_| (cursor, Vec::new()));
            let result = match port.sync {
                Ok(sync) => before_send_unless_withheld(
                    sync.as_ref(),
                    &paths,
                    port.cap.saturating_sub(start.elapsed()),
                    &wait_state,
                ),
                Err(reason) => SendSync::late(&paths, LateWhy::Unavailable(reason)),
            };
            let mut notice = result.line(&port.host);
            if let Some(why) = unknown_saves {
                let editor = format!("Eitri could not ask the editor which files were saved ({why})");
                notice = Some(match notice {
                    Some(line) => format!("{editor}. {line}"),
                    None => editor,
                });
            }
            let mut state = waiting.0.progress.lock().unwrap();
            let at = state.waiting.iter().position(|w| w.id == waiting.2).unwrap();
            state.waiting.remove(at);
            // Decided under the lock: a Stop or a session end after this point finds nothing left to take back.
            let withheld = withheld_reason(&wait_state);
            if withheld.is_none() {
                state.pending_cursors.insert(line.message_id.clone(), generation);
                if let Some(text) = notice {
                    if line.seq.or(line.started_at).unwrap_or(0) >= state.last_reset {
                        line.text = text;
                        if state.lines.len() == MAX_LINES {
                            state.lines.remove(0);
                        }
                        state.lines.push(line);
                    }
                }
            }
            drop(state);
            drop(waiting);
            withheld.map_or(Ok(()), Err)
        })
    }
}

const FILE_WAIT: Duration = Duration::from_secs(5);
const TOTAL_WAIT: Duration = Duration::from_secs(15);

pub(super) fn tree_excluded(matcher: Option<&Matcher>, path: &Path, is_dir: bool) -> bool {
    path.iter().any(|part| part == ".git")
        || path.ancestors().skip(usize::from(!is_dir)).any(|dir| {
            matches!(
                dir.file_name().and_then(|name| name.to_str()),
                Some(".svn" | ".hg" | ".bzr" | "_darcs")
            )
        })
        || matcher.is_some_and(|m| m.ignored(&path.to_string_lossy(), is_dir))
}

fn included(matcher: Option<&Matcher>, rel: &Path, host: &str) -> Result<(), String> {
    let name = rel.to_string_lossy();
    if tree_excluded(matcher, rel, false) {
        return Err(format!("{name} is excluded from sync; it is saved on this laptop only"));
    }
    matcher.map(|_| ()).ok_or_else(|| write_late(rel, host))
}

fn write_late(rel: &Path, host: &str) -> String {
    format!(
        "the write to {} has not reached {host} yet; it is saved on this laptop",
        rel.display()
    )
}

fn confirm_file(
    root: &Path,
    rel: &Path,
    host: &str,
    deadline: Instant,
    wait: impl FnOnce(WaitFileHashRequest) -> bool,
) -> Result<(), String> {
    let late = || write_late(rel, host);
    if Instant::now() >= deadline {
        return Err(late());
    }
    let target = ProjectDir::open(root)
        .ok()
        .and_then(|dir| dir.target(rel, false).ok())
        .ok_or_else(late)?;
    let Ok(Current::Regular { bytes, .. }) = read_current_within(&target, super::conflicts::FILE_LIMIT) else {
        return Err(late());
    };
    if bytes.len() as u64 > super::conflicts::FILE_LIMIT {
        return Err(format!(
            "{} is too large to confirm on {host}; it is saved on this laptop",
            rel.display()
        ));
    }
    let sha256 = super::conflicts::hash(&bytes);
    let timeout_ms = deadline
        .saturating_duration_since(Instant::now())
        .as_nanos()
        .div_ceil(1_000_000) as u64;
    if timeout_ms > 0
        && wait(WaitFileHashRequest {
            root: root.display().to_string(),
            rel: rel.to_string_lossy().into_owned(),
            sha256,
            timeout_ms,
        })
    {
        Ok(())
    } else {
        Err(late())
    }
}

pub fn hold_editor_write(conn: &RemoteConnection, matcher: &Matcher, root: &Path, rel: &Path) -> Result<(), String> {
    included(Some(matcher), rel, &conn.project().host)?;
    hold_until(
        Arc::new(conn.clone()),
        &conn.project().host,
        root,
        rel,
        Instant::now() + FILE_WAIT,
    )
}

pub trait FileHashConnection: Send + Sync {
    fn wait_file_hash(&self, request: WaitFileHashRequest) -> Result<WaitFileHashResponse, String>;
}

impl FileHashConnection for RemoteConnection {
    fn wait_file_hash(&self, mut request: WaitFileHashRequest) -> Result<WaitFileHashResponse, String> {
        request.root = self.project().root.display().to_string();
        RemoteConnection::wait_file_hash(self, request)
    }
}

fn hold_until(
    conn: Arc<dyn FileHashConnection>,
    host: &str,
    root: &Path,
    rel: &Path,
    deadline: Instant,
) -> Result<(), String> {
    let late = || write_late(rel, host);
    if Instant::now() >= deadline {
        return Err(late());
    }
    let (tell, result) = std::sync::mpsc::channel();
    let worker_host = host.to_owned();
    let root = root.to_owned();
    let path = rel.to_owned();
    // A disk read and the transport's own deadline can each outlast the tool's budget.
    std::thread::Builder::new()
        .name("eitri-wait-hash".into())
        .spawn(move || {
            let confirmed = confirm_file(&root, &path, &worker_host, deadline, |request| {
                conn.wait_file_hash(request)
                    .is_ok_and(|reply| reply.matched && reply.refusal.is_empty())
            });
            let _ = tell.send(confirmed);
        })
        .map_err(|_| late())?;
    result
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| late())?
}

#[derive(Clone)]
pub struct EditorWriteHold {
    pub connection: Option<Arc<dyn FileHashConnection>>,
    pub host: String,
    pub sync_dir: Option<PathBuf>,
}

impl EditorWriteHold {
    pub fn confirm(&self, root: &Path, paths: &[PathBuf]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let patterns = self
            .sync_dir
            .as_deref()
            .and_then(super::recreate::read_created_patterns);
        let matcher = patterns.as_deref().map(Matcher::new);
        confirm_files(paths, Instant::now, |rel, deadline| {
            if rel.is_absolute() {
                return Err(format!("{} is outside the project and is not synced", rel.display()));
            }
            let late = || write_late(rel, &self.host);
            included(matcher.as_ref(), rel, &self.host)?;
            hold_until(
                self.connection.clone().ok_or_else(late)?,
                &self.host,
                root,
                rel,
                deadline,
            )
        })
    }
}

fn confirm_files(
    paths: &[PathBuf],
    now: impl Fn() -> Instant,
    mut confirm: impl FnMut(&Path, Instant) -> Result<(), String>,
) -> Result<(), String> {
    let deadline = now() + TOTAL_WAIT;
    let errors: Vec<_> = paths
        .iter()
        .filter_map(|rel| confirm(rel, deadline.min(now() + FILE_WAIT)).err())
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

#[cfg(test)]
impl<F> FileHashConnection for F
where
    F: Fn(WaitFileHashRequest) -> Result<WaitFileHashResponse, String> + Send + Sync,
{
    fn wait_file_hash(&self, request: WaitFileHashRequest) -> Result<WaitFileHashResponse, String> {
        self(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_backend::{AgentBackend, AgentId, BackendKind};
    use crate::agent_bridge::SessionModeChoice;
    use crate::remote_project::TabOrigin;
    use crate::remote_sync::engine::{Conflict, EntryKind, Role, SessionStatus, SyncState};
    use crate::tab_set::{FirstTurn, TabBackend, TabSet, ViewId};
    use crate::test_providers::RecordingProvider;
    use std::sync::mpsc;
    use std::thread;

    const HOST: &str = "example-host";
    const ROOT: &str = "/srv/project";
    const TEXT: &str = "read this\n\n  exactly as typed";

    struct Port {
        outcome: FlushOutcome,
        conflict: bool,
        /// What the next tree status reads answer, in order, instead of the session's own status.
        statuses: Mutex<std::collections::VecDeque<Result<SessionStatus, String>>>,
        status_reads: std::sync::atomic::AtomicUsize,
        status_caps: Mutex<Vec<Duration>>,
        calls: Mutex<Vec<thread::ThreadId>>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl SyncHandle for Port {
        fn flush_tree(&self, cap: Duration) -> FlushOutcome {
            assert!(cap <= Duration::from_secs(3));
            self.calls.lock().unwrap().push(thread::current().id());
            if let Some(wait) = self.release.lock().unwrap().take() {
                wait.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            self.outcome.clone()
        }

        /// The conflict appears with the cycle a flush runs, as Mutagen reports it once the flush has returned.
        fn tree_status(&self, cap: Duration) -> Result<SessionStatus, String> {
            self.status_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.status_caps.lock().unwrap().push(cap);
            if let Some(status) = self.statuses.lock().unwrap().pop_front() {
                return status;
            }
            let cycles = self.calls.lock().unwrap().len() as u64 + 1;
            let mut status = tree_with(cycles);
            if self.conflict && cycles > 1 {
                status.conflicts.push(Conflict {
                    path: "src".into(),
                    server: EntryKind::Directory,
                    laptop: EntryKind::File,
                });
            }
            Ok(status)
        }

        fn matcher(&self) -> Result<Matcher, String> {
            Ok(Matcher::new(&["target/".into()]))
        }
    }

    /// The tree session watching with `cycles` completed, as the first instance Mutagen made of it.
    fn tree_with(cycles: u64) -> SessionStatus {
        let mut status = SessionStatus::disconnected("eitri-key-tree", Role::Tree, "x");
        status.identifier = "sync_first".into();
        status.state = SyncState::Watching;
        status.last_error = None;
        status.successful_cycles = cycles;
        status
    }

    struct Desk {
        tabs: TabSet,
        provider: Arc<RecordingProvider>,
        port: Arc<Port>,
        root: PathBuf,
    }

    impl Desk {
        fn new(outcome: FlushOutcome, conflict: bool, remote: bool) -> Self {
            agent::state_dirs::redirect_state_to_a_test_root();
            let root = agent::state_dirs::test_workspace_dir("send-sync");
            let provider = Arc::new(RecordingProvider::steering());
            let conversation = agent::AgentConversation::create(
                provider.clone(),
                &root,
                agent::setting_sources::ProjectTrust::Untrusted,
            )
            .unwrap();
            let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
            if remote {
                tabs.set_project_origin(TabOrigin::Remote {
                    host: HOST.into(),
                    host_key_sha256: "key".into(),
                    root: ROOT.into(),
                })
                .unwrap();
            }
            let port = Arc::new(Port {
                outcome,
                conflict,
                statuses: Default::default(),
                status_reads: Default::default(),
                status_caps: Default::default(),
                calls: Default::default(),
                release: Default::default(),
            });
            tabs.send_port = Some(SendPort {
                key: "key".into(),
                root: ROOT.into(),
                sync: Ok(port.clone()),
                host: HOST.into(),
                cap: Duration::from_secs(3),
                saved: Arc::new(|cursor, _| {
                    Ok((
                        1,
                        if cursor == 0 {
                            vec!["src/main.rs".into()]
                        } else {
                            vec![]
                        },
                    ))
                }),
            });
            tabs.get_mut(tabs.active())
                .unwrap()
                .set_backend(TabBackend::Live(AgentBackend::Conversation {
                    agent: AgentId::Claude,
                    conversation: Box::new(conversation),
                }));
            Self {
                tabs,
                provider,
                port,
                root,
            }
        }

        fn turn() -> FirstTurn {
            FirstTurn {
                wire: TEXT.into(),
                typed: TEXT.into(),
                typed_by_person: true,
                shell_line: false,
                attachments: vec![],
            }
        }

        fn send(&mut self) {
            self.tabs
                .send_prompt(self.tabs.active(), "r", ViewId::BASE, Self::turn());
        }

        fn wait(&mut self, mut done: impl FnMut(&mut Desk) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(self) {
                assert!(Instant::now() < deadline);
                self.tabs.pump(&self.root, true);
                thread::yield_now();
            }
        }

        fn sent(&mut self) -> Vec<LateLine> {
            self.send();
            if self.tabs.sends_off_thread(self.tabs.active()) {
                self.wait(|d| !d.tabs.poll_turn_sends(Instant::now()).is_empty());
            }
            assert_eq!(self.provider.turns(), [TEXT]);
            self.tabs.get(self.tabs.active()).unwrap().send_sync.lines()
        }
    }

    impl Drop for Desk {
        fn drop(&mut self) {
            for tab in self.tabs.take_all() {
                if let Some(retired) = tab.retired {
                    retired.into_backend().shutdown();
                }
            }
        }
    }

    #[test]
    fn send_without_an_editor_flushes_and_adds_no_line() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        d.tabs.send_port.as_mut().unwrap().saved = Arc::new(|cursor, _| Ok((cursor, vec![])));
        assert!(d.sent().is_empty());
        assert_eq!(d.port.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn send_waits_for_the_flush_then_sends() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        let (release, wait) = mpsc::channel();
        *d.port.release.lock().unwrap() = Some(wait);
        d.send();
        d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
        assert!(d.provider.turns().is_empty());
        assert!(
            d.tabs
                .get(d.tabs.active())
                .unwrap()
                .send_sync
                .display(false)
                .unwrap()
                .waiting
        );
        release.send(()).unwrap();
        d.wait(|d| !d.provider.turns().is_empty());
        assert!(
            !d.tabs
                .get(d.tabs.active())
                .unwrap()
                .send_sync
                .display(false)
                .unwrap()
                .waiting
        );
    }

    #[test]
    fn the_flush_never_runs_on_the_gtk_thread() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        assert!(d.sent().is_empty());
        assert_ne!(d.port.calls.lock().unwrap()[0], thread::current().id());
    }

    #[test]
    fn steer_flushes_too() {
        let mut d = Desk::new(FlushOutcome::TimedOut, false, true);
        let tab = d.tabs.active();
        d.provider
            .queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t".into() });
        d.wait(|d| d.tabs.get(tab).unwrap().turn_running());
        d.tabs
            .queue_message(tab, TEXT, TEXT.into(), 1, true, agent::PromptOrigin::Typed)
            .unwrap();
        let (release, wait) = mpsc::channel();
        *d.port.release.lock().unwrap() = Some(wait);
        d.tabs.steer_held(tab, 1, Some("t"));
        d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
        assert!(d.provider.steers().is_empty());
        release.send(()).unwrap();
        d.wait(|d| !d.provider.steers().is_empty());
        let request = &d.provider.steers()[0];
        assert_eq!(request.text, TEXT);
        let line = &d.tabs.get(tab).unwrap().send_sync.lines()[0];
        assert_eq!(line.message_id, request.client_message_id);
        assert_eq!(line.text, "your last save of src/main.rs has not reached example-host");
    }

    #[test]
    fn flush_timeout_sends_with_the_line() {
        let mut d = Desk::new(FlushOutcome::TimedOut, false, true);
        let line = &d.sent()[0];
        assert_eq!(line.text, "your last save of src/main.rs has not reached example-host");
        assert!(line.seq.is_some());
    }

    #[test]
    fn saved_file_in_conflict_sends_with_the_line() {
        assert_eq!(
            Desk::new(FlushOutcome::Flushed, true, true).sent()[0].text,
            "your last save of src/main.rs has not reached example-host"
        );
    }

    #[test]
    fn send_with_sync_unavailable_goes_with_the_line() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        d.tabs.send_port.as_mut().unwrap().sync = Err("Mutagen missing".into());
        assert!(d.sent()[0].text.ends_with("sync is unavailable: Mutagen missing"));
    }

    #[test]
    fn halted_and_missing_sessions_go_late_with_the_engine_reason() {
        for reason in ["session halted", "no such session"] {
            let mut d = Desk::new(FlushOutcome::Failed(reason.into()), false, true);
            assert_eq!(
                d.sent()[0].text,
                format!("your last save of src/main.rs has not reached {HOST}; sync is unavailable: {reason}")
            );
        }
    }

    #[test]
    fn prompt_text_is_never_changed() {
        let mut d = Desk::new(FlushOutcome::Failed("paused".into()), false, true);
        let line = &d.sent()[0];
        assert_eq!(line.prompt.as_bytes(), d.provider.turns()[0].as_bytes());
        assert_eq!(
            line.text,
            "your last save of src/main.rs has not reached example-host; sync is unavailable: paused"
        );
    }

    #[test]
    fn local_tabs_are_unchanged() {
        let mut d = Desk::new(FlushOutcome::TimedOut, false, false);
        assert!(d.sent().is_empty());
        assert!(d.port.calls.lock().unwrap().is_empty());
        assert!(d.tabs.get(d.tabs.active()).unwrap().send_sync_payload(false).is_none());
    }

    #[test]
    fn saved_paths_reset_after_each_send() {
        let mut d = Desk::new(FlushOutcome::Flushed, true, true);
        assert_eq!(d.sent().len(), 1);
        let sync = &d.tabs.get(d.tabs.active()).unwrap().send_sync;
        let message = MessageSync {
            tab: sync.clone(),
            port: d.tabs.send_port.clone().unwrap(),
            wake: None,
        };
        message.wait("second", Some(2), 2, "again")().unwrap();
        assert_eq!(sync.lines().len(), 1);
    }

    #[test]
    fn conflicts_come_from_the_engine_right_after_the_flush() {
        let mut d = Desk::new(FlushOutcome::Flushed, true, true);
        assert_eq!(
            d.sent()[0].text,
            "your last save of src/main.rs has not reached example-host"
        );
        assert_eq!(d.port.calls.lock().unwrap().len(), 1);
        assert_eq!(d.port.status_reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_tree_status_that_cannot_be_read_says_the_conflict_state_is_unconfirmed() {
        for failing in [0, 1] {
            let mut d = Desk::new(FlushOutcome::Flushed, false, true);
            let mut reads = std::collections::VecDeque::from([Ok(tree_with(1)), Ok(tree_with(2))]);
            reads[failing] = Err("no such session: eitri-key-tree".into());
            *d.port.statuses.lock().unwrap() = reads;
            assert_eq!(
                d.sent()[0].text,
                "the conflict state on example-host could not be confirmed"
            );
        }
    }

    /// A tree re-created under its name between the reads, or one with no completed cycle, has no conflict list that
    /// speaks for the saves.
    #[test]
    fn another_instance_of_the_tree_or_one_with_no_cycle_is_unconfirmed() {
        let recreated = SessionStatus {
            identifier: "sync_second".into(),
            ..tree_with(1)
        };
        let unknown = SessionStatus {
            identifier: String::new(),
            ..tree_with(2)
        };
        for after in [recreated, tree_with(0), unknown] {
            let mut d = Desk::new(FlushOutcome::Flushed, false, true);
            *d.port.statuses.lock().unwrap() = [Ok(tree_with(1)), Ok(after)].into();
            assert_eq!(
                d.sent()[0].text,
                "the conflict state on example-host could not be confirmed"
            );
        }
    }

    #[test]
    fn the_status_reads_share_the_sends_budget() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        assert!(d.sent().is_empty());
        let caps = d.port.status_caps.lock().unwrap().clone();
        assert_eq!(caps.len(), 2);
        assert!(caps.iter().all(|cap| *cap <= Duration::from_secs(3)));
        assert!(caps[1] <= caps[0]);
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        d.tabs.send_port.as_mut().unwrap().cap = Duration::ZERO;
        assert_eq!(
            d.sent()[0].text,
            "the conflict state on example-host could not be confirmed"
        );
        assert_eq!(
            d.port.status_reads.load(Ordering::Relaxed),
            1,
            "no read after the budget is spent"
        );
    }

    #[test]
    fn transition_problem_on_a_saved_parent_reports_the_error() {
        use crate::remote_sync::engine::{Problem, Side};
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        let mut after = tree_with(2);
        after.problems.push(Problem {
            path: "src".into(),
            side: Side::Server,
            text: "disk full".into(),
        });
        *d.port.statuses.lock().unwrap() = [Ok(tree_with(1)), Ok(after)].into();
        let lines = d.sent();
        assert!(lines
            .iter()
            .any(|line| line.text.contains("src/main.rs") && line.text.contains("disk full")));
    }

    #[test]
    fn omitted_scan_or_transition_problems_leave_saves_unconfirmed() {
        for scan in [true, false] {
            let mut d = Desk::new(FlushOutcome::Flushed, false, true);
            let mut after = tree_with(2);
            if scan {
                after.excluded_scan_problems.laptop = 1;
            } else {
                after.excluded_transition_problems.server = 1;
            }
            *d.port.statuses.lock().unwrap() = [Ok(tree_with(1)), Ok(after)].into();
            assert!(d.sent().iter().any(|line| line.text.contains("incomplete")));
        }
    }

    #[test]
    fn conflicts_mutagen_left_out_of_its_list_say_the_list_is_incomplete() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        let status = SessionStatus {
            excluded_conflicts: 1,
            ..tree_with(2)
        };
        *d.port.statuses.lock().unwrap() = [Ok(tree_with(1)), Ok(status)].into();
        assert_eq!(
            d.sent()[0].text,
            "the list of conflicts and sync problems Mutagen gave is incomplete"
        );
    }

    #[test]
    fn unknown_saves_still_flush_and_check_status() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        d.tabs.send_port.as_mut().unwrap().saved = Arc::new(|_, _| Err("editor offline".into()));
        assert_eq!(
            d.sent()[0].text,
            "Eitri could not ask the editor which files were saved (editor offline)"
        );
        assert_eq!(d.port.calls.lock().unwrap().len(), 1);
        assert_eq!(d.port.status_reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn timeout_with_no_saves_names_only_the_sync() {
        let mut d = Desk::new(FlushOutcome::TimedOut, false, true);
        d.tabs.send_port.as_mut().unwrap().saved = Arc::new(|cursor, _| Ok((cursor, vec![])));
        assert_eq!(d.sent()[0].text, "sync with example-host has not caught up");
    }

    #[test]
    fn a_failed_send_keeps_its_saves_for_the_retry() {
        let mut d = Desk::new(FlushOutcome::Flushed, true, true);
        d.provider.refuse_sends(true);
        d.send();
        d.wait(|d| !d.tabs.poll_turn_sends(Instant::now()).is_empty());
        assert!(d.tabs.get(d.tabs.active()).unwrap().send_sync.lines().is_empty());
        d.provider.refuse_sends(false);
        d.send();
        d.wait(|d| d.provider.turns().len() == 2);
        assert_eq!(
            d.tabs.get(d.tabs.active()).unwrap().send_sync.lines()[0].text,
            "your last save of src/main.rs has not reached example-host"
        );
    }

    #[test]
    fn interrupt_withdraws_a_waiting_send_or_steer_and_restores_its_draft() {
        for steer in [false, true] {
            let mut d = Desk::new(FlushOutcome::TimedOut, false, true);
            let tab = d.tabs.active();
            d.provider
                .queue(agent::AgentDomainEvent::UserPromptSubmitted { text: "earlier".into() });
            d.wait(|d| !d.tabs.active_tab().live().unwrap().projection().user_prompts.is_empty());
            let (release, wait) = mpsc::channel();
            *d.port.release.lock().unwrap() = Some(wait);
            if steer {
                d.provider
                    .queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t".into() });
                d.wait(|d| d.tabs.get(tab).unwrap().turn_running());
                d.tabs
                    .queue_message(tab, TEXT, TEXT.into(), 1, true, agent::PromptOrigin::Typed)
                    .unwrap();
                d.tabs.steer_held(tab, 1, Some("t"));
            } else {
                d.send();
            }
            d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
            assert_eq!(d.tabs.cancel_sync_send(tab).unwrap(), [TEXT]);
            assert_eq!(d.tabs.active_tab().live().unwrap().projection().user_prompts.len(), 1);
            assert_eq!(d.tabs.get(tab).unwrap().draft(), TEXT);
            assert!(d.tabs.get(tab).unwrap().session.queue.is_empty());
            release.send(()).unwrap();
            d.wait(|d| {
                d.tabs
                    .get(tab)
                    .unwrap()
                    .send_sync
                    .progress
                    .lock()
                    .unwrap()
                    .waiting
                    .is_empty()
            });
            if !steer {
                d.wait(|d| !d.tabs.poll_turn_sends(Instant::now()).is_empty());
            }
            let projection = d.tabs.active_tab().live().unwrap().projection();
            assert_eq!(projection.user_prompts.len(), 1);
            assert_eq!(projection.user_prompts[0].text, "earlier");
            assert!(d.tabs.get(tab).unwrap().send_sync.lines().is_empty());
            assert!(d.provider.turns().is_empty() && d.provider.steers().is_empty());
        }
    }

    /// Starts a desk send of `kind` (`first`, `prompt`, `queue`) or a steer into a running turn (`steer`).
    fn start_send(d: &mut Desk, kind: &str) {
        let tab = d.tabs.active();
        match kind {
            "first" => assert!(d.tabs.send_first_turn(tab, "r", Desk::turn()).is_none()),
            "prompt" => d.send(),
            "queue" => {
                d.tabs
                    .queue_message(tab, TEXT, TEXT.into(), 1, true, agent::PromptOrigin::Typed)
                    .unwrap();
                assert!(d.tabs.flush_queue(tab).is_none());
            }
            _ => {
                d.provider
                    .queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t".into() });
                d.wait(|d| d.tabs.get(tab).unwrap().turn_running());
                d.tabs
                    .queue_message(tab, TEXT, TEXT.into(), 1, true, agent::PromptOrigin::Typed)
                    .unwrap();
                d.tabs.steer_held(tab, 1, Some("t"));
            }
        }
    }

    /// The tab's session goes while its message waits for the flush: a failure, or the permission tripwire.
    fn end_session(d: &mut Desk, trip: bool) -> crate::tab_set::Retired {
        let tab = d.tabs.active();
        if !trip {
            return d
                .tabs
                .fail_session(tab, "session lost".into(), crate::tab_set::FailedHow::AfterStart, true)
                .unwrap();
        }
        d.provider.queue(agent::AgentDomainEvent::UngatedCliMode {
            reported: "acceptEdits".into(),
            detail: "SessionReady".into(),
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(tripped) = d.tabs.pump(&d.root, true).tripped.pop() {
                return tripped.retired;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }

    fn next_sent(d: &mut Desk) -> crate::tab_set::Sent {
        let mut sent = None;
        d.wait(|d| {
            sent = d.tabs.poll_turn_sends(Instant::now()).pop();
            sent.is_some()
        });
        sent.unwrap()
    }

    fn wait_over(d: &mut Desk, sync: &TabSync) {
        d.wait(|_| sync.progress.lock().unwrap().waiting.is_empty());
    }

    #[test]
    fn a_session_that_ends_during_the_wait_refuses_the_send_and_never_reaches_the_provider() {
        use crate::tab_set::Sent;
        for trip in [false, true] {
            for kind in ["first", "prompt", "queue"] {
                let mut d = Desk::new(FlushOutcome::Flushed, false, true);
                let tab = d.tabs.active();
                let (release, wait) = mpsc::channel();
                *d.port.release.lock().unwrap() = Some(wait);
                start_send(&mut d, kind);
                d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
                let retired = end_session(&mut d, trip);
                release.send(()).unwrap();
                match next_sent(&mut d) {
                    Sent::Flushed { flush, .. } => {
                        assert_eq!(kind, "queue");
                        assert_eq!(flush.outcome.unwrap_err().message, agent::SESSION_ENDED_BEFORE_SEND);
                        let session = &d.tabs.get(tab).unwrap().session;
                        assert_eq!(session.queue.len(), 1);
                        assert_eq!(session.queue[0].text, TEXT);
                        assert_eq!(session.queue_error.as_deref(), Some(agent::SESSION_ENDED_BEFORE_SEND));
                    }
                    Sent::First { typed, outcome, .. } | Sent::Prompt { typed, outcome, .. } => {
                        assert_ne!(kind, "queue");
                        assert_eq!(typed, TEXT);
                        assert_eq!(outcome.unwrap_err().message, agent::SESSION_ENDED_BEFORE_SEND);
                        assert!(d.tabs.get(tab).unwrap().session.queue.is_empty());
                        // The host keeps it in the tab's draft, whichever page is on screen.
                        assert_eq!(d.tabs.get(tab).unwrap().draft(), TEXT);
                        assert_eq!(d.tabs.take_stopped_sends(), [(tab, vec![TEXT.to_string()])]);
                    }
                    _ => panic!("unexpected send outcome"),
                }
                assert!(d.provider.turns().is_empty());
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn a_session_that_ends_during_a_steers_wait_holds_the_message_and_never_delivers_it() {
        for trip in [false, true] {
            let mut d = Desk::new(FlushOutcome::Flushed, false, true);
            let tab = d.tabs.active();
            let (release, wait) = mpsc::channel();
            *d.port.release.lock().unwrap() = Some(wait);
            start_send(&mut d, "steer");
            d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
            let sync = d.tabs.active_tab().send_sync.clone();
            let retired = end_session(&mut d, trip);
            release.send(()).unwrap();
            wait_over(&mut d, &sync);
            let queue = &d.tabs.get(tab).unwrap().session.queue;
            assert_eq!(queue.len(), 1);
            assert!(queue[0].is_held());
            assert_eq!(queue[0].text, TEXT);
            retired.into_backend().shutdown();
            assert!(d.provider.steers().is_empty());
        }
    }

    /// How the session goes before `r`: failed; closed and pumped (the tick ends its waits); closed with no pump yet.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Gone {
        Failed,
        Closed,
        ClosedUnpumped,
    }

    /// Closes the session as its provider reports it. The ingestion folds on its own thread, so the tab can show it
    /// ended before any pump has run.
    fn close_session(d: &mut Desk, pumped: bool) {
        d.provider
            .queue(agent::AgentDomainEvent::SessionClosed { reason: "gone".into() });
        let deadline = Instant::now() + Duration::from_secs(10);
        while d.tabs.active_tab().wire_state() != crate::agent_bridge::TabStateWire::Ended {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        if pumped {
            d.tabs.pump(&d.root, true);
        }
    }

    /// `r` after the tick refused the wait leaves the text to that refusal; before it, `r` stops the wait and puts the
    /// text in the draft itself. Either way the text comes back once and nothing is sent.
    #[test]
    fn r_during_the_wait_returns_the_text_once_and_never_reaches_the_provider() {
        use crate::tab_set::Sent;
        for gone in [Gone::Failed, Gone::Closed, Gone::ClosedUnpumped] {
            for kind in ["prompt", "queue", "steer"] {
                let mut d = Desk::new(FlushOutcome::Flushed, false, true);
                let tab = d.tabs.active();
                let (release, wait) = mpsc::channel();
                *d.port.release.lock().unwrap() = Some(wait);
                start_send(&mut d, kind);
                d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
                let sync = d.tabs.active_tab().send_sync.clone();
                let mut retired = Vec::new();
                match gone {
                    Gone::Failed => retired.push(end_session(&mut d, false)),
                    Gone::Closed => close_session(&mut d, true),
                    Gone::ClosedUnpumped => close_session(&mut d, false),
                }
                retired.extend(d.tabs.reset(tab).unwrap());
                let refused = gone != Gone::ClosedUnpumped;
                let draft_at_reset = if refused && kind != "steer" { "" } else { TEXT };
                assert_eq!(d.tabs.get(tab).unwrap().draft(), draft_at_reset, "{gone:?} {kind}");
                release.send(()).unwrap();
                let why = if refused {
                    agent::SESSION_ENDED_BEFORE_SEND
                } else {
                    agent::STOPPED_BEFORE_SEND
                };
                let mut draft = TEXT;
                match kind {
                    "prompt" => {
                        let Sent::Prompt { outcome, .. } = next_sent(&mut d) else {
                            panic!("unexpected send outcome")
                        };
                        assert_eq!(outcome.unwrap_err().message, why);
                    }
                    "queue" => {
                        let Sent::Flushed { flush, .. } = next_sent(&mut d) else {
                            panic!("unexpected send outcome")
                        };
                        assert_eq!(flush.outcome.unwrap_err().message, why);
                        let queue = &d.tabs.get(tab).unwrap().session.queue;
                        let texts: Vec<_> = queue.iter().map(|q| q.text.as_str()).collect();
                        assert_eq!(texts, if refused { vec![TEXT] } else { vec![] });
                        draft = draft_at_reset;
                    }
                    _ => wait_over(&mut d, &sync),
                }
                assert_eq!(d.tabs.get(tab).unwrap().draft(), draft, "{gone:?} {kind}");
                for retired in retired {
                    retired.into_backend().shutdown();
                }
                assert!(d.provider.turns().is_empty() && d.provider.steers().is_empty());
            }
        }
    }

    /// A watch that reports the session closed or unavailable, in its events or through a resync, ends the waits as a
    /// failure does: the agent behind it may still be running.
    #[test]
    fn a_session_shown_ended_during_the_wait_refuses_the_send() {
        use crate::tab_set::Sent;
        for (closed, overflow) in [(true, false), (false, false), (false, true)] {
            let mut d = Desk::new(FlushOutcome::Flushed, false, true);
            let tab = d.tabs.active();
            let (release, wait) = mpsc::channel();
            *d.port.release.lock().unwrap() = Some(wait);
            d.send();
            d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
            let mut events = Vec::new();
            if overflow {
                events.extend((0..300).map(|_| agent::AgentDomainEvent::UserPromptSubmitted { text: "x".into() }));
            }
            events.push(if closed {
                agent::AgentDomainEvent::SessionClosed { reason: "gone".into() }
            } else {
                agent::AgentDomainEvent::SessionUnavailable {
                    reason: "replay gap".into(),
                }
            });
            d.provider.queue_all(events);
            let sync = d.tabs.active_tab().send_sync.clone();
            d.wait(|_| !sync.display(false).unwrap().waiting);
            release.send(()).unwrap();
            let Sent::Prompt { outcome, .. } = next_sent(&mut d) else {
                panic!("unexpected send outcome")
            };
            assert_eq!(outcome.unwrap_err().message, agent::SESSION_ENDED_BEFORE_SEND);
            assert_eq!(d.tabs.get(tab).unwrap().draft(), TEXT);
            assert!(d.provider.turns().is_empty());
        }
    }

    /// A steer the provider reported dropped while its wait still runs is held again in the queue under its old id:
    /// `r` or a Stop then returns its text once.
    #[test]
    fn a_dropped_steer_still_waiting_comes_back_once() {
        for stop in [false, true] {
            let mut d = Desk::new(FlushOutcome::Flushed, false, true);
            let tab = d.tabs.active();
            let (release, wait) = mpsc::channel();
            *d.port.release.lock().unwrap() = Some(wait);
            start_send(&mut d, "steer");
            d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
            let sync = d.tabs.active_tab().send_sync.clone();
            let crate::tab_set::QueuedState::Steered { client_id, .. } =
                d.tabs.active_tab().session.queue[0].state.clone()
            else {
                panic!("the message is steered")
            };
            d.provider.queue(agent::AgentDomainEvent::UserMessageDropped {
                client_message_id: client_id.clone(),
                reason: "the session ended".into(),
            });
            d.wait(|d| d.tabs.active_tab().session.queue[0].is_held());
            assert!(d.tabs.active_tab().session.queue[0].carries(&client_id));
            let mut retired = None;
            if stop {
                assert_eq!(d.tabs.cancel_sync_send(tab).unwrap(), [TEXT]);
                assert!(d.tabs.active_tab().session.queue.is_empty());
            } else {
                close_session(&mut d, false);
                retired = d.tabs.reset(tab).unwrap();
            }
            assert_eq!(d.tabs.get(tab).unwrap().draft(), TEXT);
            release.send(()).unwrap();
            wait_over(&mut d, &sync);
            if let Some(retired) = retired {
                retired.into_backend().shutdown();
            }
            assert!(d.provider.steers().is_empty());
        }
    }

    #[test]
    fn a_stop_owed_to_a_send_refused_as_busy_stops_the_running_turn_and_returns_the_text() {
        use crate::tab_set::Sent;
        for kind in ["prompt", "queue"] {
            let mut d = Desk::new(FlushOutcome::Flushed, false, true);
            let tab = d.tabs.active();
            let release = d.provider.hold_calls();
            start_send(&mut d, kind);
            d.wait(|d| d.provider.calls_waiting() > 0);
            assert!(d.tabs.cancel_sync_send(tab).is_none());
            let stop = d.tabs.interrupt_spawned(tab).unwrap();
            d.provider.refuse_sends(true);
            drop(release);
            let outcome = match next_sent(&mut d) {
                Sent::Prompt { outcome, .. } => outcome,
                Sent::Flushed { flush, .. } => flush.outcome,
                _ => panic!("unexpected send outcome"),
            };
            assert_eq!(outcome.unwrap_err().message, agent::STOPPED_BEFORE_SEND);
            let session = &d.tabs.get(tab).unwrap().session;
            assert!(
                session.queue.is_empty(),
                "nothing waits to steer into the turn the Stop stops"
            );
            assert_eq!(d.tabs.get(tab).unwrap().draft(), TEXT);
            assert_eq!(d.tabs.take_stopped_sends(), [(tab, vec![TEXT.to_string()])]);
            assert!(d.tabs.active_tab().live().unwrap().projection().user_prompts.is_empty());
            d.wait(|d| {
                d.tabs
                    .poll_interrupts()
                    .into_iter()
                    .any(|done| done.number == stop && done.outcome.is_ok())
            });
            assert_eq!(d.provider.interrupts(), 1);
        }
    }

    fn completed(turn_id: &str) -> agent::AgentDomainEvent {
        agent::AgentDomainEvent::TurnCompleted {
            turn_id: turn_id.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    /// A send refused as busy and its Stop: the text comes back to the draft, the Stop is answered, and the provider
    /// is asked to interrupt `interrupts` times.
    fn busy_send_comes_back(d: &mut Desk, stop: u64, interrupts: usize) {
        use crate::tab_set::Sent;
        let tab = d.tabs.active();
        let Sent::Prompt { outcome, .. } = next_sent(d) else {
            panic!("unexpected send outcome")
        };
        assert_eq!(outcome.unwrap_err().message, agent::STOPPED_BEFORE_SEND);
        assert!(d.tabs.get(tab).unwrap().session.queue.is_empty());
        assert_eq!(d.tabs.get(tab).unwrap().draft(), TEXT);
        d.wait(|d| {
            d.tabs
                .poll_interrupts()
                .into_iter()
                .any(|done| done.number == stop && done.outcome.is_ok())
        });
        assert_eq!(d.provider.interrupts(), interrupts);
    }

    /// The running turn showed while the call was out, so the Stop interrupted it at once: the busy reply then
    /// returns the text and asks for no second interrupt.
    #[test]
    fn a_stop_after_the_running_turn_showed_returns_the_busy_send() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        let tab = d.tabs.active();
        let release = d.provider.hold_calls();
        d.send();
        d.wait(|d| d.provider.calls_waiting() > 0);
        d.provider
            .queue(agent::AgentDomainEvent::TurnStarted { turn_id: "a".into() });
        d.wait(|d| d.tabs.get(tab).unwrap().turn_running());
        let stop = d.tabs.interrupt_spawned(tab).unwrap();
        d.provider.refuse_sends(true);
        drop(release);
        busy_send_comes_back(&mut d, stop, 1);
    }

    /// The turn the Stop meant ended, and another started, before the busy reply was read: the Stop settles without
    /// interrupting the later turn.
    #[test]
    fn a_stop_owed_to_a_busy_send_never_interrupts_a_later_turn() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        let tab = d.tabs.active();
        let release = d.provider.hold_calls();
        d.send();
        d.wait(|d| d.provider.calls_waiting() > 0);
        let stop = d.tabs.interrupt_spawned(tab).unwrap();
        d.provider.queue_all(vec![
            agent::AgentDomainEvent::TurnStarted { turn_id: "a".into() },
            completed("a"),
            agent::AgentDomainEvent::TurnStarted { turn_id: "b".into() },
        ]);
        d.wait(|d| {
            d.tabs
                .get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .as_deref()
                == Some("b")
        });
        d.provider.refuse_sends(true);
        drop(release);
        busy_send_comes_back(&mut d, stop, 0);
    }

    #[test]
    fn stop_after_flush_waits_for_its_turn_start_on_either_side_of_the_send_reply() {
        for replied in [false, true] {
            let mut d = Desk::new(FlushOutcome::Flushed, false, true);
            let tab = d.tabs.active();
            let release = d.provider.hold_calls();
            d.send();
            d.wait(|d| d.provider.calls_waiting() > 0);
            assert!(d.tabs.cancel_sync_send(tab).is_none());
            if replied {
                drop(release);
                d.wait(|d| !d.tabs.poll_turn_sends(Instant::now()).is_empty());
                d.tabs.interrupt_spawned(tab).unwrap();
            } else {
                d.tabs.interrupt_spawned(tab).unwrap();
                drop(release);
                d.wait(|d| !d.tabs.poll_turn_sends(Instant::now()).is_empty());
            }
            d.provider.queue_all(vec![
                agent::AgentDomainEvent::TurnStarted {
                    turn_id: "other".into(),
                },
                agent::AgentDomainEvent::TurnCompleted {
                    turn_id: "other".into(),
                    outcome: agent::TurnOutcome::Completed,
                    result_text: String::new(),
                    stop_reason: None,
                    usage: None,
                    detail: Default::default(),
                },
            ]);
            d.wait(|d| {
                d.tabs
                    .active_tab()
                    .live()
                    .unwrap()
                    .projection()
                    .last_turn_end_seq
                    .is_some()
            });
            d.tabs.pump(&d.root, true);
            d.tabs.poll_turn_sends(Instant::now());
            assert_eq!(d.provider.interrupts(), 0);
            assert!(d.tabs.poll_interrupts().is_empty());
            d.provider.queue(agent::AgentDomainEvent::TurnStarted {
                turn_id: "turn-1".into(),
            });
            d.wait(|d| {
                d.tabs.poll_turn_sends(Instant::now());
                d.tabs.poll_interrupts().into_iter().any(|done| done.outcome.is_ok())
            });
            assert_eq!(d.provider.interrupts(), 1);
        }
    }

    #[test]
    fn a_turn_finished_before_its_send_reply_never_interrupts_its_successor() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        let tab = d.tabs.active();
        let release = d.provider.hold_calls();
        d.send();
        d.wait(|d| d.provider.calls_waiting() > 0);
        d.tabs.interrupt_spawned(tab).unwrap();
        d.provider.queue_all(vec![
            agent::AgentDomainEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            agent::AgentDomainEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                outcome: agent::TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            },
            agent::AgentDomainEvent::TurnStarted { turn_id: "next".into() },
        ]);
        d.wait(|d| {
            d.tabs
                .active_tab()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .as_deref()
                == Some("next")
        });
        drop(release);
        d.wait(|d| {
            d.tabs.poll_turn_sends(Instant::now());
            d.tabs.poll_interrupts().into_iter().any(|done| done.outcome.is_ok())
        });
        assert_eq!(d.provider.interrupts(), 0);
    }

    #[test]
    fn cancellation_skips_the_next_editor_or_engine_call() {
        for stop_before_editor in [false, true] {
            let d = Desk::new(FlushOutcome::Flushed, false, true);
            let sync = d.tabs.active_tab().send_sync.clone();
            let mut port = d.tabs.send_port.clone().unwrap();
            let stopping = sync.clone();
            port.saved = Arc::new(move |cursor, _| {
                assert!(!stop_before_editor);
                assert_eq!(stopping.cancel_waits().len(), 1);
                Ok((cursor, vec![]))
            });
            let wait = MessageSync {
                port,
                tab: sync.clone(),
                wake: None,
            }
            .wait("m", Some(1), 1, TEXT);
            if stop_before_editor {
                sync.cancel_waits();
            }
            assert_eq!(wait(), Err(agent::STOPPED_BEFORE_SEND.into()));
            assert!(d.port.calls.lock().unwrap().is_empty());
            assert_eq!(d.port.status_reads.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn cancellation_during_the_flush_skips_the_status_read_after_it() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        let (release, wait) = mpsc::channel();
        *d.port.release.lock().unwrap() = Some(wait);
        d.send();
        d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
        assert!(d.tabs.cancel_sync_send(d.tabs.active()).is_some());
        release.send(()).unwrap();
        d.wait(|d| !d.tabs.poll_turn_sends(Instant::now()).is_empty());
        assert_eq!(
            d.port.status_reads.load(Ordering::Relaxed),
            1,
            "only the read before the flush"
        );
        assert!(d.provider.turns().is_empty());
    }

    #[test]
    fn closing_a_remote_tab_during_its_wait_never_starts_the_turn() {
        let mut d = Desk::new(FlushOutcome::Flushed, false, true);
        let (release, wait) = mpsc::channel();
        *d.port.release.lock().unwrap() = Some(wait);
        d.send();
        d.wait(|d| !d.port.calls.lock().unwrap().is_empty());
        let removed = d.tabs.remove(d.tabs.active()).unwrap();
        release.send(()).unwrap();
        removed.retired.unwrap().into_backend().shutdown();
        d.wait(|d| Arc::strong_count(&d.provider) == 1);
        assert!(d.provider.turns().is_empty());
    }

    #[test]
    fn resets_prune_older_late_lines_even_when_the_events_overflow() {
        for overflow in [false, true] {
            let mut d = Desk::new(FlushOutcome::TimedOut, false, true);
            assert_eq!(d.sent().len(), 1);
            let mut events = vec![agent::AgentDomainEvent::ConversationReset {
                new_conversation_id: "new".into(),
                trigger: None,
            }];
            if overflow {
                events.extend((0..300).map(|_| agent::AgentDomainEvent::UserPromptSubmitted { text: "new".into() }));
            }
            d.provider.queue_all(events);
            d.wait(|d| d.tabs.get(d.tabs.active()).unwrap().send_sync.lines().is_empty());
        }
    }

    #[test]
    fn both_new_and_restored_late_lines_are_bounded() {
        let d = Desk::new(FlushOutcome::TimedOut, false, true);
        let sync = d.tabs.get(d.tabs.active()).unwrap().send_sync.clone();
        let mut all = vec![];
        for i in 0..60 {
            let id = i.to_string();
            assert!(MessageSync {
                port: d.tabs.send_port.clone().unwrap(),
                tab: sync.clone(),
                wake: None
            }
            .wait(&id, Some(i), i, TEXT)()
            .is_ok());
            sync.settle(&id, true);
            all.push(sync.lines().last().unwrap().clone());
        }
        assert_eq!(sync.lines(), all[10..]);
        sync.restore(&all);
        assert_eq!(sync.lines(), all[10..]);
    }

    fn check_write(path: &str, wait: impl FnOnce(WaitFileHashRequest) -> bool + Send + 'static) -> Result<(), String> {
        let root = agent::state_dirs::test_workspace_dir("hold-write");
        std::fs::write(root.join(path), b"\xef\xbb\xbfline\r\n").unwrap();
        super::super::recreate::write_created_patterns(&root, &["*.log".into()]).unwrap();
        let wait = Mutex::new(Some(wait));
        let connection = move |request| {
            Ok(WaitFileHashResponse {
                matched: wait.lock().unwrap().take().unwrap()(request),
                ..Default::default()
            })
        };
        let hold = EditorWriteHold {
            connection: Some(Arc::new(connection)),
            host: HOST.into(),
            sync_dir: Some(root.clone()),
        };
        hold.confirm(&root, &[path.into()])
    }

    #[test]
    fn editor_write_returns_success_only_after_the_server_hash_matches() {
        let (release, wait) = mpsc::channel();
        let (ask, asked) = mpsc::channel();
        let worker = thread::spawn(move || {
            check_write("a", move |_| {
                ask.send(()).unwrap();
                wait.recv().unwrap()
            })
        });
        asked.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(!worker.is_finished());
        release.send(true).unwrap();
        assert!(worker.join().unwrap().is_ok());
    }

    #[test]
    fn editor_write_above_the_limit_is_too_large_to_confirm() {
        let root = agent::state_dirs::test_workspace_dir("large-write");
        let size = crate::remote_sync::conflicts::FILE_LIMIT + 1;
        std::fs::File::create(root.join("large"))
            .unwrap()
            .set_len(size)
            .unwrap();
        let result = confirm_file(
            &root,
            Path::new("large"),
            HOST,
            Instant::now() + Duration::from_secs(60),
            |_| true,
        );
        assert!(result.is_err_and(|reason| reason.contains("too large to confirm")));
    }

    #[test]
    fn editor_write_hashes_the_bytes_on_disk() {
        check_write("a", |req| {
            assert_eq!(req.rel, "a");
            assert_eq!(req.sha256, crate::remote_sync::conflicts::hash(b"\xef\xbb\xbfline\r\n"));
            assert!((1..=5000).contains(&req.timeout_ms));
            true
        })
        .unwrap();
    }

    #[test]
    fn editor_write_timeout_is_an_error_that_says_not_reached() {
        assert_eq!(
            check_write("a", |_| false).unwrap_err(),
            "the write to a has not reached example-host yet; it is saved on this laptop"
        );
    }

    #[test]
    fn editor_write_to_an_excluded_path_says_excluded() {
        assert_eq!(
            check_write("a.log", |_| panic!("excluded")).unwrap_err(),
            "a.log is excluded from sync; it is saved on this laptop only"
        );
    }

    #[test]
    fn vcs_paths_are_excluded_even_when_the_session_patterns_are_missing() {
        let hold = EditorWriteHold {
            connection: None,
            host: HOST.into(),
            sync_dir: None,
        };
        for path in [
            ".git",
            "vendor/.git/config",
            ".hg/store/a",
            ".svn/entries",
            ".bzr/data",
            "_darcs/index",
        ] {
            assert_eq!(
                hold.confirm(Path::new(ROOT), &[path.into()]).unwrap_err(),
                format!("{path} is excluded from sync; it is saved on this laptop only")
            );
        }
    }

    #[test]
    fn multi_file_tool_waits_per_file_with_a_total_cap() {
        let start = Instant::now();
        let clock = std::cell::Cell::new(start);
        let mut waits = Vec::new();
        let paths = ["a", "b", "c", "d"].map(PathBuf::from);
        let errors = confirm_files(
            &paths,
            || clock.get(),
            |path, deadline| {
                waits.push(deadline.saturating_duration_since(clock.get()).as_secs());
                clock.set(deadline);
                Err(path.display().to_string())
            },
        )
        .unwrap_err();
        assert_eq!(waits, [5, 5, 5, 0]);
        assert_eq!(errors, "a\nb\nc\nd");
    }
}
