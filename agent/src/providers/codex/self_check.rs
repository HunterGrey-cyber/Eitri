//! The per-binary self-check: before the first start with a `codex` Eitri has not checked, a real
//! `codex app-server` runs a scripted conversation against a **scripted model**, to prove that the
//! levers every Codex tab relies on still work in this binary.
//!
//! What a Codex tab relies on is written into each `thread/start` and `thread/resume`: the project
//! trust entries, the skills switch, the extra-permission features, the narrow sandbox table. An
//! untested Codex could change any of them, so while the check has not passed **every** Codex start is
//! refused, trusted ones included.
//!
//! **The check is hostile on purpose.** Its `CODEX_HOME` is private and its `config.toml` does what a
//! careless user file could do (a wider sandbox, extra permission requests on); the fixture project
//! holds everything an untrusted folder must not be able to use (a tool server, a hook trusted by hash,
//! an instruction file, a skill, an exec-policy rule that allows a command outside the sandbox). The
//! session is started the way an untrusted tab is, and it passes only when none of that took effect --
//! in the parent, in a child the parent spawns, and after a resume in a new process.
//!
//! **The fixture files are data, not wire.** They live in `check_fixture/` and are written into the
//! check's own private directory, read by the Codex the check starts and by nothing else; what Eitri
//! sends over the wire is only what [`super::config`] builds.
//!
//! The verdict is cached per [`BinaryKey`] (path, size, modification time, version) and names the revision
//! of the check that earned it, so a pass from an older check does not answer for a newer one; a failure is
//! not cached, so a transient one is retried at the next start, with the reasons kept in a log.

use super::config::{expected_echo, session_config};
use super::scripted_model::{ScriptedModel, Step};
use super::translate::{parse_user_agent_version, version_text};
use super::tripwire;
use super::wire::{
    method, Decision, DecisionResponse, InitializeParams, InitializeResponse, ThreadResumeParams, ThreadStartParams,
    ThreadStartResponse, TurnStartParams,
};
use crate::external::Sandbox;
use crate::external::{DirTrust, Reviewer, TrustPlan};
use crate::jsonrpc_link::{Framing, Incoming, JsonRpcLink, LinkSpec};
use crate::private_fs::{create_private_dir_all, open_private, read_private_to_string, write_private};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The catalog model the scripted conversation names. Codex knows it, so its tool set is the one a real
/// model gets; a name Codex does not know would be offered a different tool set.
pub const SCRIPTED_MODEL_SLUG: &str = "gpt-6-luna";

/// The notice every refused start carries; `{}` is the log's path.
const NOTICE: &str = "Eitri could not confirm that this Codex keeps its levers; see";

/// Everything the check does must be over in this time.
const CHECK_BUDGET: Duration = Duration::from_secs(150);
/// How long a start waits for another start's check of the same binary: that check's own budget, and some
/// time for what it does around it.
const LOCK_WAIT: Duration = Duration::from_secs(CHECK_BUDGET.as_secs() + 30);
/// How long `codex --version` may take.
const VERSION_BUDGET: Duration = Duration::from_secs(10);
/// One request's answer.
const CALL_BUDGET: Duration = Duration::from_secs(30);
/// A turn, children included.
const TURN_BUDGET: Duration = Duration::from_secs(60);
/// How long a child that is shut down may take before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// The revision of the check itself. **Bump it whenever the check's script, its fixture or the rules a run
/// is judged by change**: a recorded pass names the revision it was earned under, and a pass of any other
/// revision is not a pass, so a binary an older, weaker check let through is checked again. A verdict
/// written before revisions were recorded has none and is not a pass either.
const CHECK_REVISION: u32 = 3;
/// The directory under Eitri's state directory that holds one directory per checked binary.
const CHECK_DIR: &str = "codex-check";
/// The file in a binary's check directory that records how its last run ended, so the starts that waited
/// for that run get its answer.
const LAST_RUN: &str = "last-run.json";
/// The method that lists the hooks of a project; read only, used to learn the fixture hook's key and hash.
const HOOKS_LIST: &str = "hooks/list";

const PARENT_PROMPT: &str = "EITRI-CHECK-PARENT-PROMPT";
const CHILD_TASK: &str = "EITRI-CHECK-CHILD-TASK";
const RESUME_PROMPT: &str = "EITRI-CHECK-RESUME-PROMPT";
const READ_ONLY_PROMPT: &str = "EITRI-CHECK-READ-ONLY-PROMPT";
const AGENTS_MARKER: &str = "EITRI-CHECK-AGENTS-MARKER";
const SKILL_NAME: &str = "eitri-check-skill";
const EXTRA_PERMISSIONS_FIELD: &str = "with_additional_permissions";
/// The tool server's name in the fixture project's configuration.
const PROJECT_TOOL_SERVER: &str = "eitri_check_tool_server";
/// The namespace Codex's multi-agent tools live in.
const AGENT_NAMESPACE: &str = "collaboration";

const FIXTURE_HOME: &str = include_str!("check_fixture/home.toml");
/// The fixture project: where each file goes, and the templates they are written from.
const FIXTURE_MANIFEST: &str = include_str!("check_fixture/project.manifest");
const FIXTURE_TEMPLATES: &[(&str, &str)] = &[
    ("project_config.toml", include_str!("check_fixture/project_config.toml")),
    ("project_hooks.json", include_str!("check_fixture/project_hooks.json")),
    ("project_rules.rules", include_str!("check_fixture/project_rules.rules")),
    ("project_agents.md", include_str!("check_fixture/project_agents.md")),
    ("project_skill.md", include_str!("check_fixture/project_skill.md")),
];
const FIXTURE_HOOK_STATE: &str = include_str!("check_fixture/hook_state.toml");
const FIXTURE_MARKER_SH: &str = include_str!("check_fixture/marker.sh");

// ------------------------------------------------------------------------------------ the error

/// Why a start was refused by the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfCheckError {
    /// The program is not there.
    NotFound,
    /// The program is older than the oldest Codex Eitri works with.
    TooOld { found: (u32, u32, u32) },
    /// The check failed or could not run. `reasons` say why; `log` is the file that keeps them.
    Failed { reasons: Vec<String>, log: PathBuf },
    /// Another start's check of the same binary still held it when this start stopped waiting.
    Unfinished { waited: Duration },
}

impl fmt::Display for SelfCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelfCheckError::NotFound => write!(f, "{}", super::provider::NOT_FOUND),
            SelfCheckError::TooOld { found } => write!(
                f,
                "Codex {} is older than the oldest Eitri works with ({}): update Codex",
                version_text(*found),
                version_text(super::provider::MIN_CODEX)
            ),
            SelfCheckError::Failed { log, .. } => write!(f, "{NOTICE} {}", log.display()),
            SelfCheckError::Unfinished { waited } => write!(
                f,
                "another check of this Codex did not finish in {} s; start the tab again",
                waited.as_secs()
            ),
        }
    }
}

impl std::error::Error for SelfCheckError {}

// ------------------------------------------------------------------------------------ identity

/// What makes one `codex` binary the one that was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryKey {
    path: PathBuf,
    size: u64,
    mtime_ns: i128,
    version: String,
}

impl BinaryKey {
    /// The key of a binary already known: the resolved path, its size, its modification time in
    /// nanoseconds since the epoch, and the version it reports.
    pub fn new(path: PathBuf, size: u64, mtime_ns: i128, version: String) -> BinaryKey {
        BinaryKey {
            path,
            size,
            mtime_ns,
            version,
        }
    }

    /// Reads the key off the file system and `<program> --version`.
    pub fn of(program: &Path) -> Result<BinaryKey, SelfCheckError> {
        let path = match std::fs::canonicalize(program) {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(SelfCheckError::NotFound),
            Err(e) => {
                return Err(failed_without_log(format!(
                    "{} cannot be resolved: {e}",
                    program.display()
                )))
            }
        };
        let meta = std::fs::metadata(&path).map_err(|e| failed_without_log(format!("{}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(SelfCheckError::NotFound);
        }
        let modified = meta
            .modified()
            .map_err(|e| failed_without_log(format!("{}: no modification time: {e}", path.display())))?;
        let mtime_ns = match modified.duration_since(UNIX_EPOCH) {
            Ok(after) => after.as_nanos() as i128,
            Err(before) => -(before.duration().as_nanos() as i128),
        };
        let version = probe_version(&path).map_err(failed_without_log)?;
        Ok(BinaryKey {
            path,
            size: meta.len(),
            mtime_ns,
            version,
        })
    }

    /// The version the binary reported, as its own text.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The version as numbers.
    pub fn version_numbers(&self) -> Option<(u32, u32, u32)> {
        parse_user_agent_version(&self.version)
    }

    /// Whether `reported` is the version this binary reported when it was keyed. A version that could
    /// not be read matches nothing.
    pub fn is_version(&self, reported: (u32, u32, u32)) -> bool {
        self.version_numbers() == Some(reported)
    }

    /// Sixteen hex digits naming this key: the directory and file names are built from it.
    pub fn id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.path.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        hasher.update(self.size.to_le_bytes());
        hasher.update(self.mtime_ns.to_le_bytes());
        hasher.update(self.version.as_bytes());
        hasher.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    fn describe(&self) -> Value {
        json!({
            "path": self.path.to_string_lossy(),
            "size": self.size,
            "mtime_ns": self.mtime_ns.to_string(),
            "version": self.version,
        })
    }
}

fn failed_without_log(reason: String) -> SelfCheckError {
    SelfCheckError::Failed {
        reasons: vec![reason],
        log: PathBuf::from(CHECK_DIR),
    }
}

/// `<program> --version`: the last word of its first line, such as `0.161.0`.
fn probe_version(program: &Path) -> Result<String, String> {
    // A file written a moment ago can still be open in a child another thread is forking; the kernel then
    // refuses to run it for a few milliseconds ("text file busy").
    let mut attempts = 0;
    let mut child = loop {
        let spawned = Command::new(program)
            .arg("--version")
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempts < 50 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            other => break other.map_err(|e| format!("{} --version could not run: {e}", program.display()))?,
        }
    };
    let end = Instant::now() + VERSION_BUDGET;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} --version did not finish", program.display()));
            }
        }
    };
    let mut text = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.by_ref().take(4096).read_to_string(&mut text);
    }
    if !status.success() {
        return Err(format!("{} --version failed", program.display()));
    }
    let word = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().last())
        .unwrap_or_default();
    if parse_user_agent_version(word).is_none() {
        return Err(format!(
            "{} --version did not print a version Eitri can read",
            program.display()
        ));
    }
    Ok(word.to_string())
}

// ------------------------------------------------------------------------------------ the cache

/// Makes sure this binary has passed the check, running it when it has not.
///
/// `state_home` is Eitri's own state directory; the check keeps `codex-check/<key>/` in it (0700, its
/// files 0600): the verdict, the log of the last run, and, while a run is going, its scratch directory.
/// Concurrent callers take turns on a lock, so the second finds the first's verdict. Returns the key of
/// the binary that passed, so the caller can tell whether what it starts afterwards is still that binary.
pub fn ensure_checked(program: &Path, state_home: &Path) -> Result<BinaryKey, SelfCheckError> {
    ensure_checked_with(program, state_home, true, |key, run_dir, log| {
        run_check(program, key, run_dir, log, &CheckTweaks::default())
    })
}

/// Deliberate faults for the check's own tests: a check that cannot fail proves nothing. A run with
/// any of them never records a verdict.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct CheckTweaks {
    /// The scripted model calls `spawn_agent` without its namespace, as a Codex that refuses to spawn answers.
    pub spawn_without_namespace: bool,
    /// The session config loses the switch that keeps skills out of the thread.
    pub drop_skills_lever: bool,
    /// The fixture project is trusted, as a lever that no longer holds would leave it.
    pub grant_trust: bool,
    /// The thread that should be read-only gets the workspace sandbox, as a Codex that ignored the ask would.
    pub widen_read_only: bool,
}

#[cfg(feature = "test-support")]
impl CheckTweaks {
    fn any(&self) -> bool {
        self.spawn_without_namespace || self.drop_skills_lever || self.grant_trust || self.widen_read_only
    }
}

/// [`ensure_checked`] with deliberate faults. Never records a verdict.
#[doc(hidden)]
#[cfg(feature = "test-support")]
pub fn ensure_checked_tweaked(
    program: &Path,
    state_home: &Path,
    tweaks: &CheckTweaks,
) -> Result<BinaryKey, SelfCheckError> {
    ensure_checked_with(program, state_home, !tweaks.any(), |key, run_dir, log| {
        run_check(program, key, run_dir, log, tweaks)
    })
}

/// The cache around a check: `run` is the check itself, given the key, a scratch directory and the log.
fn ensure_checked_with(
    program: &Path,
    state_home: &Path,
    record: bool,
    run: impl FnOnce(&BinaryKey, &Path, &mut Log) -> Result<(), Vec<String>>,
) -> Result<BinaryKey, SelfCheckError> {
    Pending::new(program, state_home)?.finish(LOCK_WAIT, record, run)
}

/// A start that knows which binary it is about to check and has not yet taken the lock.
struct Pending {
    key: BinaryKey,
    state_home: PathBuf,
    dir: PathBuf,
    log_path: PathBuf,
    /// The record of the last finished run as it stood before this start waited for the lock: a run that
    /// finishes while it waits leaves a different one.
    seen: Option<String>,
}

impl Pending {
    fn new(program: &Path, state_home: &Path) -> Result<Pending, SelfCheckError> {
        let key = BinaryKey::of(program)?;
        match key.version_numbers() {
            Some(found) if found < super::provider::MIN_CODEX => return Err(SelfCheckError::TooOld { found }),
            _ => {}
        }
        let dir = state_home.join(CHECK_DIR).join(key.id());
        let log_path = dir.join("check.log");
        create_private_dir_all(&dir, state_home).map_err(|e| SelfCheckError::Failed {
            reasons: vec![format!("{} cannot be made: {e}", dir.display())],
            log: log_path.clone(),
        })?;
        let seen = read_private_to_string(&dir.join(LAST_RUN)).ok();
        Ok(Pending {
            key,
            state_home: state_home.to_path_buf(),
            dir,
            log_path,
            seen,
        })
    }

    fn finish(
        self,
        lock_wait: Duration,
        record: bool,
        run: impl FnOnce(&BinaryKey, &Path, &mut Log) -> Result<(), Vec<String>>,
    ) -> Result<BinaryKey, SelfCheckError> {
        let Pending {
            key,
            state_home,
            dir,
            log_path,
            seen,
        } = self;
        let state_home = state_home.as_path();
        let fail = |reason: String| SelfCheckError::Failed {
            reasons: vec![reason],
            log: log_path.clone(),
        };
        let verdict_path = dir.join("verdict.json");
        if verdict_is_pass(&verdict_path, &key) {
            return Ok(key);
        }
        // One check at a time per binary; whoever waits finds the verdict, or the failure of the run it
        // waited for. Waiting has an end, so a check that hangs past its own budget does not hold every
        // later start of this binary for good.
        let _lock = DirLock::take(&dir.join("lock"), Instant::now() + lock_wait).map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut => SelfCheckError::Unfinished { waited: lock_wait },
            _ => fail(format!("the check's lock could not be taken: {e}")),
        })?;
        if verdict_is_pass(&verdict_path, &key) {
            return Ok(key);
        }
        // A run that failed while this start waited answers it too: running the same check again at once
        // would only make every waiting start pay a few seconds for the same answer. A start that comes
        // after that run finished saw its record before the lock and runs the check itself, so a failure
        // is still not kept for later starts.
        if let Some(reasons) = failed_since(&dir.join(LAST_RUN), seen.as_deref(), &key) {
            return Err(SelfCheckError::Failed { reasons, log: log_path });
        }

        let run_dir = dir.join("run");
        let _ = std::fs::remove_dir_all(&run_dir);
        create_private_dir_all(&run_dir, state_home)
            .map_err(|e| fail(format!("{} cannot be made: {e}", run_dir.display())))?;
        let mut log = Log::new();
        log.line(format!("codex {} at {}", key.version, key.path.display()));
        let started = Instant::now();
        let result = run(&key, &run_dir, &mut log);
        let _ = std::fs::remove_dir_all(&run_dir);
        log.line(match &result {
            Ok(()) => format!("passed in {:.1}s", started.elapsed().as_secs_f32()),
            Err(_) => format!("FAILED after {:.1}s", started.elapsed().as_secs_f32()),
        });
        if let Err(reasons) = &result {
            for reason in reasons {
                log.line(format!("reason: {reason}"));
            }
        }
        if let Err(e) = write_private(&log_path, log.text().as_bytes()) {
            eprintln!("agent: the Codex check's log could not be written: {e}");
        }
        if record {
            let last = json!({
                "run": uuid::Uuid::new_v4().to_string(),
                "result": if result.is_ok() { "passed" } else { "failed" },
                "reasons": result.as_ref().err().cloned().unwrap_or_default(),
                "check_revision": CHECK_REVISION,
                "key": key.describe(),
            });
            if let Err(e) = write_replacing(&dir, LAST_RUN, &last) {
                eprintln!("agent: the Codex check's last run could not be recorded: {e}");
            }
        }
        match result {
            Ok(()) => {
                if record {
                    let verdict = json!({
                        "version": 1,
                        "result": "passed",
                        "check_revision": CHECK_REVISION,
                        "key": key.describe(),
                        "checked_at": SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
                    });
                    if let Err(e) = write_replacing(&dir, "verdict.json", &verdict) {
                        eprintln!("agent: the Codex check's verdict could not be recorded: {e}");
                    }
                }
                Ok(key)
            }
            Err(reasons) => Err(SelfCheckError::Failed { reasons, log: log_path }),
        }
    }
}

/// Writes `name` in `dir` whole or not at all: a reader never sees half of it.
fn write_replacing(dir: &Path, name: &str, value: &Value) -> std::io::Result<()> {
    let temp = dir.join(format!("{name}.{}", std::process::id()));
    let written =
        write_private(&temp, value.to_string().as_bytes()).and_then(|()| std::fs::rename(&temp, dir.join(name)));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// The reasons of a failed run of this check on this key that finished after `seen` was read, if the last
/// run is one. Anything unreadable, unchanged, passed or about another key or revision is not.
fn failed_since(path: &Path, seen: Option<&str>, key: &BinaryKey) -> Option<Vec<String>> {
    let text = read_private_to_string(path).ok()?;
    if seen == Some(text.as_str()) {
        return None;
    }
    let value: Value = serde_json::from_str(&text).ok()?;
    if value.get("result") != Some(&json!("failed"))
        || value.get("check_revision") != Some(&json!(CHECK_REVISION))
        || value.get("key") != Some(&key.describe())
    {
        return None;
    }
    let reasons: Vec<String> = value
        .get("reasons")?
        .as_array()?
        .iter()
        .map(|r| r.as_str().map(str::to_string))
        .collect::<Option<_>>()?;
    (!reasons.is_empty()).then_some(reasons)
}

/// A recorded pass for exactly this key, earned under this revision of the check. Anything unreadable,
/// foreign, malformed, about another key or from another revision of the check is not a pass.
fn verdict_is_pass(path: &Path, key: &BinaryKey) -> bool {
    let Ok(text) = read_private_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    value.get("version") == Some(&json!(1))
        && value.get("result") == Some(&json!("passed"))
        && value.get("check_revision") == Some(&json!(CHECK_REVISION))
        && value.get("key") == Some(&key.describe())
}

/// An exclusive `flock` on a private file, released when dropped.
struct DirLock(File);

impl DirLock {
    /// Takes the lock, waiting for whoever holds it until `deadline`; past it the error is `TimedOut`.
    fn take(path: &Path, deadline: Instant) -> std::io::Result<DirLock> {
        let file = open_private(path)?;
        loop {
            // SAFETY: the descriptor is `file`'s own and stays open for the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(DirLock(file));
            }
            let e = std::io::Error::last_os_error();
            match e.kind() {
                std::io::ErrorKind::Interrupted => {}
                std::io::ErrorKind::WouldBlock if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                std::io::ErrorKind::WouldBlock => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "another check holds the lock",
                    ))
                }
                _ => return Err(e),
            }
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // SAFETY: as above; closing the file would release it too.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

struct Log {
    started: Instant,
    lines: Vec<String>,
}

impl Log {
    fn new() -> Log {
        Log {
            started: Instant::now(),
            lines: Vec::new(),
        }
    }

    fn line(&mut self, text: String) {
        self.lines
            .push(format!("[{:6.1}s] {text}", self.started.elapsed().as_secs_f32()));
    }

    fn text(&self) -> String {
        let mut out = self.lines.join("\n");
        out.push('\n');
        out
    }
}

// ------------------------------------------------------------------------------------ the fixture

/// Where everything of one run lives: under the run's scratch directory, except the directories the sandbox
/// must keep commands out of when that directory is one the sandbox always lets them write to.
struct Layout {
    home: PathBuf,
    probe_home: PathBuf,
    project: PathBuf,
    outside: PathBuf,
    extra: PathBuf,
    markers: PathBuf,
    marker_sh: PathBuf,
    /// Holds `outside` and `extra` when they could not be under the run directory; removed with the layout.
    _elsewhere: Elsewhere,
}

/// A directory made for one run outside the state directory, removed when dropped.
struct Elsewhere(Option<PathBuf>);

impl Drop for Elsewhere {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The directories a command in Codex's `workspace-write` sandbox may write to whatever the session says:
/// `/tmp` and the one `TMPDIR` names, which the session leaves writable as Codex's own default does. A
/// "forbidden" file under one of them proves nothing either way.
fn always_writable() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/tmp")];
    if let Some(dir) = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
    {
        roots.push(dir);
    }
    roots
        .into_iter()
        .map(|root| std::fs::canonicalize(&root).unwrap_or(root))
        .collect()
}

/// The first of `candidates` (canonical directories) that no always-writable root holds.
fn outside_base(candidates: &[PathBuf], writable: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|dir| !writable.iter().any(|root| dir.starts_with(root)))
        .cloned()
}

/// Where the run's forbidden files go: the run directory itself when the sandbox does not always let
/// commands write there; else a fresh private directory in the user's runtime or cache directory. With
/// none of them outside `/tmp` and `TMPDIR` the check cannot tell a held sandbox from a broken one, and it
/// fails.
fn forbidden_base(run: &Path) -> Result<(PathBuf, Elsewhere), String> {
    let writable = always_writable();
    if outside_base(&[run.to_path_buf()], &writable).is_some() {
        return Ok((run.to_path_buf(), Elsewhere(None)));
    }
    let absolute = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|d| d.is_absolute());
    let cache = absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|home| home.join(".cache")));
    let candidates: Vec<PathBuf> = [absolute("XDG_RUNTIME_DIR"), cache]
        .into_iter()
        .flatten()
        .filter_map(|dir| std::fs::canonicalize(dir).ok())
        .filter(|dir| dir.is_dir())
        .collect();
    let Some(base) = outside_base(&candidates, &writable) else {
        return Err(format!(
            "{} is under /tmp or TMPDIR, where Codex's sandbox always lets commands write, and neither \
             XDG_RUNTIME_DIR nor the cache directory is elsewhere, so the check could not tell whether the \
             sandbox holds; set XDG_STATE_HOME outside /tmp",
            run.display()
        ));
    };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let dir = base.join(format!("eitri-codex-check-{}-{stamp}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok((dir.clone(), Elsewhere(Some(dir))))
}

impl Layout {
    /// The files the sandbox must keep the model's commands from creating.
    fn forbidden_writes(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            (
                "the rule-matching touch in the parent",
                self.outside.join("rule-parent"),
            ),
            ("the rule-matching touch in the child", self.outside.join("rule-child")),
            ("a plain write into the extra writable root", self.extra.join("plain")),
            ("the declined escalation", self.outside.join("escalated")),
            (
                "a plain write inside the project of a read-only thread",
                self.project.join("read-only-plain"),
            ),
            (
                "the declined escalation of a read-only thread",
                self.project.join("read-only-escalated"),
            ),
        ]
    }

    fn markers_present(&self) -> Vec<&'static str> {
        ["tool-server", "hook"]
            .into_iter()
            .filter(|name| self.markers.join(name).exists())
            .collect()
    }
}

/// The `(destination, template)` pairs of the manifest.
fn manifest_entries() -> impl Iterator<Item = (&'static str, &'static str)> {
    FIXTURE_MANIFEST
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('\t'))
}

/// A path that can be put inside a TOML string, a JSON string and a single-quoted shell word as it is.
fn plain_path(path: &Path) -> Result<&str, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
    if text.chars().any(|c| c.is_control() || matches!(c, '"' | '\'' | '\\')) {
        return Err(format!(
            "{text} holds a quote, a backslash or a control character, which the check's fixture files cannot hold"
        ));
    }
    Ok(text)
}

fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in values {
        out = out.replace(name, value);
    }
    out
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    write_private(path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

fn build_layout(run_dir: &Path, tweaks: &CheckTweaks) -> Result<Layout, String> {
    let run = std::fs::canonicalize(run_dir).map_err(|e| format!("{}: {e}", run_dir.display()))?;
    let (forbidden, elsewhere) = forbidden_base(&run)?;
    let layout = Layout {
        home: run.join("home"),
        probe_home: run.join("probe-home"),
        project: run.join("project"),
        outside: forbidden.join("outside"),
        extra: forbidden.join("extra"),
        markers: run.join("markers"),
        marker_sh: run.join("marker.sh"),
        _elsewhere: elsewhere,
    };
    for dir in [
        &layout.home,
        &layout.probe_home,
        &layout.outside,
        &layout.extra,
        &layout.markers,
    ] {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    write_file(&layout.marker_sh, FIXTURE_MARKER_SH)?;
    let values = [
        ("@MARKER_SH@", plain_path(&layout.marker_sh)?),
        ("@MARKERS@", plain_path(&layout.markers)?),
        ("@OUTSIDE@", plain_path(&layout.outside)?),
        ("@EXTRA@", plain_path(&layout.extra)?),
    ];
    plain_path(&layout.project)?;
    for (dest, template) in manifest_entries() {
        let text = FIXTURE_TEMPLATES
            .iter()
            .find(|(name, _)| *name == template)
            .map(|(_, text)| *text)
            .ok_or_else(|| format!("the fixture manifest names a template that is not built in: {template}"))?;
        write_file(&layout.project.join(dest), &fill(text, &values))?;
    }
    let _ = tweaks;
    Ok(layout)
}

/// The home's `config.toml`: the scripted model, then the hostile file.
fn home_config(model: &ScriptedModel, layout: &Layout, extra_tables: &str) -> Result<String, String> {
    let values = [("@EXTRA@", plain_path(&layout.extra)?)];
    Ok(format!(
        "{}{}{}",
        model.provider_toml(SCRIPTED_MODEL_SLUG),
        fill(FIXTURE_HOME, &values),
        extra_tables
    ))
}

// ------------------------------------------------------------------------------------ the peer

/// A request Codex made of the check, answered at once.
struct Asked {
    method: String,
    thread_id: Option<String>,
    /// How many notifications had arrived when it did.
    after_notes: usize,
}

/// One `codex app-server` child under the check, with everything it said.
struct Peer {
    link: JsonRpcLink,
    notes: Vec<(String, Value)>,
    asked: Vec<Asked>,
    closed: Option<String>,
    deadline: Instant,
}

impl Peer {
    fn spawn(program: &Path, home: &Path, cwd: &Path, deadline: Instant) -> Result<Peer, String> {
        let no_proxy = OsString::from("127.0.0.1,localhost");
        let link = JsonRpcLink::spawn(LinkSpec {
            program: program.to_path_buf(),
            args: ["app-server", "--listen", "stdio://"]
                .iter()
                .map(OsString::from)
                .collect(),
            cwd: cwd.to_path_buf(),
            framing: Framing::Bare,
            env: vec![
                (OsString::from("CODEX_HOME"), home.as_os_str().to_os_string()),
                (OsString::from("NO_PROXY"), no_proxy.clone()),
                (OsString::from("no_proxy"), no_proxy),
            ],
            env_remove: [
                "TMUX",
                "TMUX_PANE",
                "NVIM",
                "NVIM_LISTEN_ADDRESS",
                "OPENAI_API_KEY",
                "CODEX_API_KEY",
            ]
            .iter()
            .map(OsString::from)
            .collect(),
            over_cap: super::approvals::over_cap_reply,
        })
        .map_err(|e| format!("codex app-server could not be started: {e}"))?;
        Ok(Peer {
            link,
            notes: Vec::new(),
            asked: Vec::new(),
            closed: None,
            deadline,
        })
    }

    /// Reads what has arrived. Approvals are declined, anything else Codex asks is answered "not found".
    fn pump(&mut self) {
        while let Some(message) = self.link.try_recv() {
            match message {
                Incoming::Notification { method, params } => self.notes.push((method, params)),
                Incoming::Request { id, method, params } => {
                    let thread_id = params.get("threadId").and_then(Value::as_str).map(str::to_string);
                    if method.ends_with("requestApproval") {
                        let answer = serde_json::to_value(DecisionResponse {
                            decision: Decision::Decline,
                        })
                        .unwrap_or(Value::Null);
                        self.link.respond(id, answer);
                    } else {
                        self.link.respond_error(id, -32601, "method not found");
                    }
                    self.asked.push(Asked {
                        method,
                        thread_id,
                        after_notes: self.notes.len(),
                    });
                }
                Incoming::Skipped(_) => {}
                Incoming::Closed { reason } => self.closed = Some(reason),
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let mut pending = self.link.request(method, params);
        let end = (Instant::now() + CALL_BUDGET).min(self.deadline);
        loop {
            self.pump();
            if let Some(answer) = pending.try_take() {
                return answer.map_err(|e| format!("{method}: {e}"));
            }
            if let Some(reason) = &self.closed {
                return Err(format!("{method}: codex ended ({reason})"));
            }
            if Instant::now() >= end {
                return Err(format!("{method} was not answered in time"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for(&mut self, within: Duration, mut done: impl FnMut(&Peer) -> bool) -> bool {
        let end = (Instant::now() + within).min(self.deadline);
        loop {
            self.pump();
            if done(self) {
                return true;
            }
            if self.closed.is_some() || Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn turn_completed(&self, thread_id: &str) -> bool {
        self.notes
            .iter()
            .any(|(m, p)| m == method::TURN_COMPLETED && p.get("threadId").and_then(Value::as_str) == Some(thread_id))
    }

    /// `initialize`, then `initialized`. Returns the version Codex reports and refuses to go on when
    /// Codex is not using the home it was given: nothing else may be sent to a Codex that reads the user's own.
    fn handshake(&mut self, home: &Path) -> Result<String, String> {
        let params =
            serde_json::to_value(InitializeParams::eitri(env!("CARGO_PKG_VERSION"))).map_err(|e| e.to_string())?;
        let answer = self.call(method::INITIALIZE, params)?;
        let init: InitializeResponse =
            serde_json::from_value(answer).map_err(|e| format!("the handshake answer could not be read: {e}"))?;
        self.link.notify(method::INITIALIZED, Value::Null);
        let reported = init
            .codex_home
            .as_deref()
            .map(Path::new)
            .and_then(|p| std::fs::canonicalize(p).ok());
        if reported.as_deref() != std::fs::canonicalize(home).ok().as_deref() {
            return Err("codex does not use the private home the check gave it".to_string());
        }
        Ok(init.user_agent)
    }

    fn stderr_tail(&self) -> String {
        self.link.stderr_tail()
    }

    fn finish(self) {
        self.link.shutdown(EXIT_GRACE);
    }
}

// ------------------------------------------------------------------------------------ the run

/// What one thread start or resume answered.
struct ThreadFacts {
    thread_id: String,
    echo_problem: Option<String>,
    instruction_sources: Vec<String>,
    reasoning_effort: Option<String>,
}

fn thread_facts(answer: Value, reviewer: Reviewer, sandbox: Sandbox) -> Result<ThreadFacts, String> {
    let effort = answer
        .get("reasoningEffort")
        .and_then(Value::as_str)
        .map(str::to_string);
    let started: ThreadStartResponse =
        serde_json::from_value(answer).map_err(|e| format!("the thread answer could not be read: {e}"))?;
    let echo_problem = tripwire::check_echo(&expected_echo(reviewer, sandbox), &started.echo())
        .err()
        .map(|mismatch| mismatch.to_string());
    Ok(ThreadFacts {
        thread_id: started.thread.id.clone(),
        echo_problem,
        instruction_sources: started.instruction_sources,
        reasoning_effort: effort,
    })
}

fn script(layout: &Layout, tweaks: &CheckTweaks) -> Vec<Step> {
    let outside = layout.outside.display();
    // Quoted, as `plain_path` keeps a quote out of these: split on a space, a command would miss the file it names
    // and fail, and a file that was never aimed at reads as a write the sandbox held.
    let touch = |name: &str| format!("touch '{outside}/{name}'");
    let namespace = if tweaks.spawn_without_namespace {
        None
    } else {
        Some(AGENT_NAMESPACE)
    };
    let for_child = |step: Step| step.when_body_has(CHILD_TASK).unless_body_has(PARENT_PROMPT);
    let project = layout.project.display();
    vec![
        // A read-only thread: a plain write inside the project, where the workspace sandbox would let it through,
        // then the same write asking to leave the sandbox.
        Step::exec("ro1", &format!("touch '{project}/read-only-plain'"), false)
            .when_body_has(READ_ONLY_PROMPT)
            .unless_body_has("\"ro1\""),
        Step::exec("ro2", &format!("touch '{project}/read-only-escalated'"), true)
            .when_body_has(READ_ONLY_PROMPT)
            .when_body_has("\"ro1\"")
            .unless_body_has("\"ro2\""),
        // The parent: the rule-matching command alone, then a child, then waiting for it.
        Step::exec("p0", &touch("rule-parent"), false)
            .when_body_has(PARENT_PROMPT)
            .unless_body_has("\"p0\""),
        Step::call(
            "sp1",
            namespace,
            "spawn_agent",
            &json!({"task_name": "child", "message": format!("{CHILD_TASK} run the commands"), "fork_turns": "none"}),
        )
        .when_body_has(PARENT_PROMPT)
        .when_body_has("\"p0\"")
        .unless_body_has("\"sp1\""),
        Step::call("wa1", namespace, "wait_agent", &json!({"timeout_ms": 30000}))
            .when_body_has(PARENT_PROMPT)
            .when_body_has("\"sp1\"")
            .unless_body_has("\"wa1\""),
        // The child, told apart by the absence of the parent's prompt: the same command, a plain write
        // into the extra root, and one that asks to leave the sandbox.
        for_child(Step::exec("cc1", &touch("rule-child"), false)).unless_body_has("\"cc1\""),
        for_child(Step::exec(
            "cc0",
            &format!("touch '{}/plain'", layout.extra.display()),
            false,
        ))
        .when_body_has("\"cc1\"")
        .unless_body_has("\"cc0\""),
        for_child(Step::exec("cc2", &touch("escalated"), true))
            .when_body_has("\"cc0\"")
            .unless_body_has("\"cc2\""),
    ]
}

/// What the run saw, reduced to the facts the verdict is made of.
#[derive(Default)]
struct Evidence {
    start: Option<ThreadFacts>,
    resume: Option<ThreadFacts>,
    read_only: Option<ThreadFacts>,
    read_only_turn_done: bool,
    read_only_asked: bool,
    announced_child: Option<String>,
    child_asked_after_announcement: bool,
    child_requests: usize,
    outputs: BTreeSet<String>,
    markers: Vec<&'static str>,
    created: Vec<&'static str>,
    leaked: Vec<&'static str>,
    projects_written: bool,
    steps_left_after_first_turn: usize,
    first_turn_done: bool,
    child_turn_done: bool,
    resumed_turn_done: bool,
    version_ok: bool,
}

/// The reasons the run does not pass; empty when it does.
fn judge(e: &Evidence) -> Vec<String> {
    let mut why = Vec::new();
    if !e.version_ok {
        why.push("the version Codex reported is not the one its binary printed".to_string());
    }
    for (label, facts) in [
        ("start", &e.start),
        ("resume", &e.resume),
        ("read-only start", &e.read_only),
    ] {
        match facts {
            None => why.push(format!("the thread {label} gave no answer")),
            Some(facts) => {
                if let Some(problem) = &facts.echo_problem {
                    why.push(format!("the thread {label} did not echo the narrow policy: {problem}"));
                }
                if !facts.instruction_sources.is_empty() {
                    why.push(format!(
                        "the thread {label} loaded instruction files: {}",
                        facts.instruction_sources.join(", ")
                    ));
                }
                if facts.reasoning_effort.as_deref() == Some("minimal") {
                    why.push(format!("the thread {label} echoed the project's reasoning effort"));
                }
            }
        }
    }
    if !e.markers.is_empty() {
        why.push(format!("the project's own commands ran: {}", e.markers.join(", ")));
    }
    for what in &e.created {
        why.push(format!("the sandbox let a write through: {what}"));
    }
    if !e.read_only_turn_done {
        why.push("the read-only thread's turn did not finish".to_string());
    }
    if !e.read_only_asked {
        why.push("the read-only thread's escalation did not arrive on its own thread".to_string());
    }
    if !e.first_turn_done {
        why.push("the first turn did not finish".to_string());
    }
    if e.steps_left_after_first_turn > 0 {
        why.push(format!(
            "{} scripted steps were never asked for, so the conversation did not run as scripted",
            e.steps_left_after_first_turn
        ));
    }
    if e.announced_child.is_none() {
        why.push(
            "no child was announced: the parent could not spawn one, so the child's levers are unchecked".to_string(),
        );
    }
    if e.child_requests == 0 {
        why.push("no request of a child reached the model".to_string());
    }
    if e.announced_child.is_some() && !e.child_asked_after_announcement {
        why.push("the child's escalation did not arrive on its own thread after the parent announced it".to_string());
    }
    for call in ["ro1", "p0", "cc1", "cc0"] {
        if !e.outputs.contains(call) {
            why.push(format!(
                "the command {call} never reported back, so what the sandbox did with it is unknown"
            ));
        }
    }
    if e.announced_child.is_some() && !e.child_turn_done {
        why.push("the child's turn did not finish".to_string());
    }
    if !e.resumed_turn_done {
        why.push("the turn after the resume did not finish".to_string());
    }
    for what in &e.leaked {
        why.push(format!("a request to the model carried {what}"));
    }
    if e.projects_written {
        why.push("Codex wrote a project entry into the check's own config file".to_string());
    }
    why
}

fn run_check(
    program: &Path,
    key: &BinaryKey,
    run_dir: &Path,
    log: &mut Log,
    tweaks: &CheckTweaks,
) -> Result<(), Vec<String>> {
    let deadline = Instant::now() + CHECK_BUDGET;
    let one = |reason: String| vec![reason];
    let layout = build_layout(run_dir, tweaks).map_err(one)?;
    if let Some(parent) = layout.outside.parent().filter(|p| !p.starts_with(run_dir)) {
        log.line(format!(
            "the files the sandbox must keep commands from writing are in {}: the run directory is where Codex's \
             sandbox always lets commands write",
            parent.display()
        ));
    }
    let model = ScriptedModel::start(script(&layout, tweaks))
        .map_err(|e| one(format!("the scripted model could not start: {e}")))?;
    log.line(format!(
        "scripted model on 127.0.0.1:{} with the catalog model {SCRIPTED_MODEL_SLUG}",
        model.port
    ));

    // The fixture hook is trusted by hash in the home, as a hostile user file would have it. The hash is
    // Codex's own, learnt from a throwaway home that trusts the project.
    let project_text = plain_path(&layout.project).map_err(one)?.to_string();
    let probe_tables = format!("\n[projects.\"{project_text}\"]\ntrust_level = \"trusted\"\n");
    write_file(
        &layout.probe_home.join("config.toml"),
        &home_config(&model, &layout, &probe_tables).map_err(one)?,
    )
    .map_err(one)?;
    let (hook_key, hook_hash) = learn_hook(program, &layout, deadline, log).map_err(one)?;
    let state = fill(
        FIXTURE_HOOK_STATE,
        &[
            ("@KEY@", plain_key(&hook_key).map_err(one)?),
            ("@HASH@", plain_key(&hook_hash).map_err(one)?),
        ],
    );
    let config_path = layout.home.join("config.toml");
    write_file(&config_path, &home_config(&model, &layout, &state).map_err(one)?).map_err(one)?;

    let trusted = tweaks.grant_trust;
    let plan = TrustPlan {
        dirs: vec![(
            layout.project.clone(),
            if trusted {
                DirTrust::Trusted
            } else {
                DirTrust::Untrusted
            },
        )],
        skills_off: !trusted,
        fingerprint: "self-check".to_string(),
        project_only: vec![PROJECT_TOOL_SERVER.to_string()],
    };
    let mut config = session_config(&plan).map_err(|e| one(e.to_string()))?;
    if tweaks.drop_skills_lever {
        config = drop_skills_lever(config);
    }
    let cwd = plain_path(&layout.project).map_err(one)?.to_string();

    let mut evidence = Evidence::default();
    let mut notes_for_log = Vec::new();

    let ctx = Ctx {
        home: &layout.home,
        config: &config,
        cwd: &cwd,
        binary_version: &key.version,
    };

    // ---- a process of its own: a read-only thread, whose steps count as scripted like every other
    let mut peer = Peer::spawn(program, &layout.home, &layout.project, deadline).map_err(one)?;
    #[cfg(feature = "test-support")]
    let asked_for = if tweaks.widen_read_only {
        Sandbox::Project
    } else {
        Sandbox::ReadOnly
    };
    #[cfg(not(feature = "test-support"))]
    let asked_for = Sandbox::ReadOnly;
    let outcome = read_only_process(&mut peer, &mut evidence, &ctx, asked_for, log);
    notes_for_log.push(peer.stderr_tail());
    peer.finish();
    outcome.map_err(one)?;

    // ---- the first process: start, one turn with a child
    let mut peer = Peer::spawn(program, &layout.home, &layout.project, deadline).map_err(one)?;
    let outcome = first_process(&mut peer, &mut evidence, &ctx, log);
    notes_for_log.push(peer.stderr_tail());
    peer.finish();
    outcome.map_err(one)?;
    evidence.steps_left_after_first_turn = model.steps_left();

    // ---- a new process: resume, one more turn
    let thread_id = evidence.start.as_ref().map(|f| f.thread_id.clone()).unwrap_or_default();
    let mut peer = Peer::spawn(program, &layout.home, &layout.project, deadline).map_err(one)?;
    let outcome = second_process(&mut peer, &mut evidence, &ctx, &thread_id, log);
    notes_for_log.push(peer.stderr_tail());
    peer.finish();
    outcome.map_err(one)?;

    // ---- what is on the disk and what the model was sent
    evidence.markers = layout.markers_present();
    evidence.created = layout
        .forbidden_writes()
        .into_iter()
        .filter(|(_, path)| path.exists())
        .map(|(what, _)| what)
        .collect();
    let configured = std::fs::read_to_string(&config_path).unwrap_or_default();
    evidence.projects_written = configured.contains("[projects");
    let requests = model.requests();
    evidence.child_requests = requests
        .iter()
        .filter(|r| {
            let text = r.to_string();
            text.contains(CHILD_TASK) && !text.contains(PARENT_PROMPT)
        })
        .count();
    for body in &requests {
        for item in body.get("input").and_then(Value::as_array).into_iter().flatten() {
            if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    evidence.outputs.insert(id.to_string());
                }
            }
        }
    }
    let all = requests.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
    for (needle, what) in [
        (AGENTS_MARKER, "the project's instruction file"),
        (SKILL_NAME, "the project's skill"),
        (EXTRA_PERMISSIONS_FIELD, "the extra-permission field of the shell tool"),
    ] {
        if all.contains(needle) {
            evidence.leaked.push(what);
        }
    }

    for tail in notes_for_log.iter().filter(|t| !t.trim().is_empty()) {
        log.line(format!(
            "codex stderr (tail): {}",
            without_colours(tail.trim()).replace('\n', " | ")
        ));
    }
    let reasons = judge(&evidence);
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

#[cfg(feature = "test-support")]
fn drop_skills_lever(config: super::config::SessionConfig) -> super::config::SessionConfig {
    config.without_skills_lever()
}

#[cfg(not(feature = "test-support"))]
fn drop_skills_lever(config: super::config::SessionConfig) -> super::config::SessionConfig {
    config
}

/// Codex colours its log lines; the check's log is read in an editor.
fn without_colours(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// A key or hash Codex printed, fit to go inside a TOML string.
fn plain_key(text: &str) -> Result<&str, String> {
    if text.is_empty() || text.chars().any(|c| c.is_control() || matches!(c, '"' | '\\')) {
        return Err("Codex printed a hook key or hash the check cannot write down".to_string());
    }
    Ok(text)
}

/// Asks a throwaway Codex that trusts the fixture project for the key and hash of its hook.
fn learn_hook(program: &Path, layout: &Layout, deadline: Instant, log: &mut Log) -> Result<(String, String), String> {
    let mut peer = Peer::spawn(program, &layout.probe_home, &layout.project, deadline)?;
    let result = (|| {
        peer.handshake(&layout.probe_home)?;
        let cwd = layout.project.to_string_lossy().to_string();
        let answer = peer.call(HOOKS_LIST, json!({ "cwds": [cwd.clone()] }))?;
        find_hook(&answer, &cwd).ok_or_else(|| "Codex did not list the fixture project's hook".to_string())
    })();
    peer.finish();
    if let Ok((key, _)) = &result {
        log.line(format!("learnt the fixture hook's key: {key}"));
    }
    result
}

/// The first `key` / `currentHash` pair in an answer whose key begins with the project's path.
fn find_hook(value: &Value, project: &str) -> Option<(String, String)> {
    match value {
        Value::Object(map) => {
            if let (Some(Value::String(key)), Some(Value::String(hash))) = (map.get("key"), map.get("currentHash")) {
                if key.starts_with(project) {
                    return Some((key.clone(), hash.clone()));
                }
            }
            map.values().find_map(|v| find_hook(v, project))
        }
        Value::Array(items) => items.iter().find_map(|v| find_hook(v, project)),
        _ => None,
    }
}

/// What both processes of the run are given.
struct Ctx<'a> {
    home: &'a Path,
    config: &'a super::config::SessionConfig,
    cwd: &'a str,
    binary_version: &'a str,
}

fn start_turn(peer: &mut Peer, thread_id: &str, sandbox: Sandbox, text: String) -> Result<(), String> {
    let turn = serde_json::to_value(TurnStartParams::new(
        thread_id.to_string(),
        text,
        Reviewer::User,
        sandbox,
    ))
    .map_err(|e| e.to_string())?;
    peer.call(method::TURN_START, turn).map(|_| ())
}

/// What a tab at a root that is no boundary runs under. `asked_for` is the sandbox sent; the echo is held to
/// read-only whatever was sent.
fn read_only_process(
    peer: &mut Peer,
    evidence: &mut Evidence,
    ctx: &Ctx,
    asked_for: Sandbox,
    log: &mut Log,
) -> Result<(), String> {
    peer.handshake(ctx.home)?;
    let params = serde_json::to_value(ThreadStartParams::new(
        ctx.cwd.to_string(),
        Reviewer::User,
        asked_for,
        ctx.config.clone(),
    ))
    .map_err(|e| e.to_string())?;
    let facts = thread_facts(
        peer.call(method::THREAD_START, params)?,
        Reviewer::User,
        Sandbox::ReadOnly,
    )?;
    log.line(format!(
        "read-only thread started; the policy echo was as asked = {}",
        facts.echo_problem.is_none()
    ));
    let thread_id = facts.thread_id.clone();
    evidence.read_only = Some(facts);
    start_turn(
        peer,
        &thread_id,
        asked_for,
        format!("{READ_ONLY_PROMPT} run the commands"),
    )?;
    evidence.read_only_turn_done = peer.wait_for(TURN_BUDGET, |p| p.turn_completed(&thread_id));
    evidence.read_only_asked = peer
        .asked
        .iter()
        .any(|a| a.method.ends_with("requestApproval") && a.thread_id.as_deref() == Some(thread_id.as_str()));
    log.line(format!(
        "the read-only turn finished = {}; its escalation was asked about = {}",
        evidence.read_only_turn_done, evidence.read_only_asked
    ));
    Ok(())
}

fn first_process(peer: &mut Peer, evidence: &mut Evidence, ctx: &Ctx, log: &mut Log) -> Result<(), String> {
    let user_agent = peer.handshake(ctx.home)?;
    evidence.version_ok = parse_user_agent_version(&user_agent) == parse_user_agent_version(ctx.binary_version);
    let params = serde_json::to_value(ThreadStartParams::new(
        ctx.cwd.to_string(),
        Reviewer::User,
        Sandbox::Project,
        ctx.config.clone(),
    ))
    .map_err(|e| e.to_string())?;
    let facts = thread_facts(
        peer.call(method::THREAD_START, params)?,
        Reviewer::User,
        Sandbox::Project,
    )?;
    log.line(format!(
        "thread started; the policy echo was as asked = {}",
        facts.echo_problem.is_none()
    ));
    let thread_id = facts.thread_id.clone();
    evidence.start = Some(facts);

    start_turn(
        peer,
        &thread_id,
        Sandbox::Project,
        format!("{PARENT_PROMPT} run the commands"),
    )?;
    evidence.first_turn_done = peer.wait_for(TURN_BUDGET, |p| p.turn_completed(&thread_id));
    log.line(format!("first turn finished = {}", evidence.first_turn_done));

    // The child, announced by the parent's items first, and its own turn.
    let announced_at = announcement_index(peer, &thread_id);
    evidence.announced_child = announced_child(peer, &thread_id);
    if let Some(child) = evidence.announced_child.clone() {
        evidence.child_turn_done = peer.wait_for(TURN_BUDGET, |p| p.turn_completed(&child));
        evidence.child_asked_after_announcement = peer.asked.iter().any(|a| {
            a.method.ends_with("requestApproval")
                && a.thread_id.as_deref() == Some(child.as_str())
                && announced_at.is_some_and(|at| a.after_notes > at)
        });
        log.line(format!(
            "a child was announced and its turn finished = {}",
            evidence.child_turn_done
        ));
    }
    Ok(())
}

fn announcement_index(peer: &Peer, parent: &str) -> Option<usize> {
    peer.notes.iter().position(|(m, p)| {
        (m == method::ITEM_STARTED || m == method::ITEM_COMPLETED)
            && p.get("threadId").and_then(Value::as_str) == Some(parent)
            && p.pointer("/item/type").and_then(Value::as_str) == Some("subAgentActivity")
            && p.pointer("/item/kind").and_then(Value::as_str) == Some("started")
    })
}

fn announced_child(peer: &Peer, parent: &str) -> Option<String> {
    let at = announcement_index(peer, parent)?;
    peer.notes[at]
        .1
        .pointer("/item/agentThreadId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn second_process(
    peer: &mut Peer,
    evidence: &mut Evidence,
    ctx: &Ctx,
    thread_id: &str,
    log: &mut Log,
) -> Result<(), String> {
    peer.handshake(ctx.home)?;
    let params = serde_json::to_value(ThreadResumeParams::new(
        thread_id.to_string(),
        ctx.cwd.to_string(),
        Reviewer::User,
        Sandbox::Project,
        ctx.config.clone(),
    ))
    .map_err(|e| e.to_string())?;
    let facts = thread_facts(
        peer.call(method::THREAD_RESUME, params)?,
        Reviewer::User,
        Sandbox::Project,
    )?;
    log.line(format!(
        "thread resumed; the policy echo was as asked = {}",
        facts.echo_problem.is_none()
    ));
    evidence.resume = Some(facts);
    start_turn(peer, thread_id, Sandbox::Project, format!("{RESUME_PROMPT} once more"))?;
    evidence.resumed_turn_done = peer.wait_for(TURN_BUDGET, |p| p.turn_completed(thread_id));
    log.line(format!(
        "the turn after the resume finished = {}",
        evidence.resumed_turn_done
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn forbidden_files_never_go_where_the_sandbox_always_lets_commands_write() {
        let writable = [PathBuf::from("/tmp"), PathBuf::from("/var/tmp/me")];
        let pick = |dirs: &[&str]| outside_base(&dirs.iter().map(PathBuf::from).collect::<Vec<_>>(), &writable);
        assert_eq!(
            pick(&["/home/u/.local/state/eitri/run"]),
            Some(PathBuf::from("/home/u/.local/state/eitri/run"))
        );
        assert_eq!(
            pick(&["/tmp/state/run", "/run/user/1000"]),
            Some(PathBuf::from("/run/user/1000"))
        );
        assert_eq!(
            pick(&["/var/tmp/me/x", "/tmp", "/home/u/.cache"]),
            Some(PathBuf::from("/home/u/.cache"))
        );
        assert_eq!(pick(&["/tmp/a", "/var/tmp/me"]), None);
        // A sibling whose name only starts the same is not inside the root.
        assert_eq!(pick(&["/tmpfoo/run"]), Some(PathBuf::from("/tmpfoo/run")));
    }

    #[test]
    fn a_run_directory_under_tmp_puts_the_forbidden_files_elsewhere_and_removes_them() {
        let run = std::env::temp_dir().join(format!("eitri-forbidden-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&run).expect("run dir");
        let run = std::fs::canonicalize(&run).expect("canonical");
        let writable = always_writable();
        assert!(
            writable.iter().any(|root| run.starts_with(root)),
            "the temporary directory should be one the sandbox always allows"
        );
        match forbidden_base(&run) {
            Ok((base, elsewhere)) => {
                assert!(!base.starts_with(&run), "{base:?} is under the run directory");
                assert!(!writable.iter().any(|root| base.starts_with(root)), "{base:?}");
                assert!(base.is_dir());
                assert_eq!(
                    std::fs::metadata(&base).expect("meta").permissions().mode() & 0o777,
                    0o700
                );
                drop(elsewhere);
                assert!(!base.exists(), "the directory made for the run is removed with it");
            }
            // Only when the runtime and cache directories are under /tmp too; the check then fails.
            Err(why) => assert!(why.contains("XDG_STATE_HOME"), "{why}"),
        }
        let _ = std::fs::remove_dir_all(&run);
    }

    /// A scratch directory that goes away with the test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("eitri-self-check-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Scratch(dir)
        }

        /// An executable that prints a version line, as `codex --version` does.
        fn program(&self, name: &str, version_line: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho '{version_line}'\n")).expect("writes");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        }

        fn state(&self) -> PathBuf {
            self.0.join("state")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("exists").permissions().mode() & 0o777
    }

    fn check(
        program: &Path,
        state: &Path,
        runs: &Cell<u32>,
        outcome: Result<(), Vec<String>>,
    ) -> Result<(), SelfCheckError> {
        ensure_checked_with(program, state, true, |_, _, _| {
            runs.set(runs.get() + 1);
            outcome
        })
        .map(|_| ())
    }

    #[test]
    fn verdict_cache_is_private_and_keyed() {
        let scratch = Scratch::new("cache");
        let a = scratch.program("codex-a", "codex-cli 0.161.0");
        let b = scratch.program("codex-b", "codex-cli 0.161.0");
        let state = scratch.state();
        let runs = Cell::new(0);

        // A pass is recorded privately, once per binary.
        check(&a, &state, &runs, Ok(())).expect("passes");
        assert_eq!(runs.get(), 1);
        let key_a = BinaryKey::of(&a).expect("key");
        let dir_a = state.join(CHECK_DIR).join(key_a.id());
        assert_eq!(mode(&dir_a), 0o700);
        assert_eq!(mode(&dir_a.join("verdict.json")), 0o600);
        assert_eq!(mode(&dir_a.join("check.log")), 0o600);
        assert!(!dir_a.join("run").exists(), "the scratch directory is removed");
        check(&a, &state, &runs, Err(vec!["not asked".into()])).expect("the cached pass answers");
        assert_eq!(runs.get(), 1, "a recorded pass is not run again");

        // Another binary has its own key and its own directory.
        check(&b, &state, &runs, Ok(())).expect("passes");
        assert_eq!(runs.get(), 2);
        let key_b = BinaryKey::of(&b).expect("key");
        assert_ne!(key_a.id(), key_b.id());
        assert!(state.join(CHECK_DIR).join(key_b.id()).join("verdict.json").is_file());

        // A verdict that names another key is not a pass for this one.
        let verdict = dir_a.join("verdict.json");
        let text = std::fs::read_to_string(&verdict).expect("reads");
        std::fs::write(&verdict, text.replace("codex-a", "codex-z")).expect("tampers");
        check(&a, &state, &runs, Ok(())).expect("passes again");
        assert_eq!(runs.get(), 3, "a verdict about another file is rerun");
        // So is one that is not JSON.
        std::fs::write(&verdict, "passed").expect("tampers");
        check(&a, &state, &runs, Ok(())).expect("passes again");
        assert_eq!(runs.get(), 4);
    }

    #[test]
    fn a_pass_of_another_check_revision_is_run_again() {
        let scratch = Scratch::new("revision");
        let program = scratch.program("codex", "codex-cli 0.161.0");
        let state = scratch.state();
        let runs = Cell::new(0);
        check(&program, &state, &runs, Ok(())).expect("passes");
        assert_eq!(runs.get(), 1);
        let key = BinaryKey::of(&program).expect("key");
        let verdict = state.join(CHECK_DIR).join(key.id()).join("verdict.json");

        // A pass an older Eitri recorded, before the check's revision was written down: the same binary,
        // but a check that may have been weaker than this one.
        let old = json!({"version": 1, "result": "passed", "key": key.describe(), "checked_at": 1});
        std::fs::write(&verdict, old.to_string()).expect("writes");
        check(&program, &state, &runs, Ok(())).expect("passes again");
        assert_eq!(runs.get(), 2, "a pass of an unrecorded check revision is not a pass");

        // The fresh verdict names this revision and answers the next start.
        let text = std::fs::read_to_string(&verdict).expect("reads");
        let value: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value.get("check_revision"), Some(&json!(CHECK_REVISION)));
        check(&program, &state, &runs, Err(vec!["not asked".into()])).expect("the cached pass");
        assert_eq!(runs.get(), 2);

        // A pass of any other revision, older or newer, is run again.
        for other in [CHECK_REVISION - 1, CHECK_REVISION + 1] {
            let mut value = value.clone();
            value["check_revision"] = json!(other);
            std::fs::write(&verdict, value.to_string()).expect("writes");
            let before = runs.get();
            check(&program, &state, &runs, Ok(())).expect("passes again");
            assert_eq!(runs.get(), before + 1, "revision {other} is not this check's");
        }
    }

    #[test]
    fn a_start_that_waited_for_a_failed_run_gets_its_outcome_and_a_later_one_runs_again() {
        let scratch = Scratch::new("waited");
        let program = scratch.program("codex", "codex-cli 0.161.0");
        let state = scratch.state();
        let runs = Cell::new(0);

        // A start is under way (it has found its binary) when another start's run fails.
        let waiting = Pending::new(&program, &state).expect("pending");
        let first = check(
            &program,
            &state,
            &runs,
            Err(vec!["the sandbox let a write through".into()]),
        )
        .expect_err("fails");
        assert_eq!(runs.get(), 1);

        // The start that was waiting gets that run's failure instead of running the check again.
        let shared = waiting
            .finish(LOCK_WAIT, true, |_, _, _| {
                runs.set(runs.get() + 1);
                Ok(())
            })
            .expect_err("the waiting start shares the failure");
        assert_eq!(runs.get(), 1, "a start that waited for a failed run does not run again");
        assert_eq!(shared, first);

        // A failure is still not cached: a start that comes afterwards runs the check.
        check(&program, &state, &runs, Ok(())).expect("passes");
        assert_eq!(runs.get(), 2);
    }

    #[test]
    fn a_start_that_waited_for_a_passed_run_finds_its_verdict() {
        let scratch = Scratch::new("waited-pass");
        let program = scratch.program("codex", "codex-cli 0.161.0");
        let state = scratch.state();
        let runs = Cell::new(0);
        let waiting = Pending::new(&program, &state).expect("pending");
        check(&program, &state, &runs, Ok(())).expect("passes");
        waiting
            .finish(LOCK_WAIT, true, |_, _, _| Err(vec!["not asked".into()]))
            .expect("the recorded pass answers");
        assert_eq!(runs.get(), 1);
    }

    #[test]
    fn waiting_for_another_check_has_a_deadline() {
        let scratch = Scratch::new("deadline");
        let program = scratch.program("codex", "codex-cli 0.161.0");
        let state = scratch.state();
        let key = BinaryKey::of(&program).expect("key");
        let dir = state.join(CHECK_DIR).join(key.id());
        create_private_dir_all(&dir, &state).expect("dir");
        // Another check holds the lock and never finishes.
        let held = DirLock::take(&dir.join("lock"), Instant::now() + Duration::from_secs(5)).expect("lock");

        let (done, finished) = std::sync::mpsc::channel();
        let (program_in, state_in) = (program.clone(), state.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = Pending::new(&program_in, &state_in).and_then(|pending| {
                pending.finish(Duration::from_millis(300), true, |_, _, _| Err(vec!["ran".into()]))
            });
            let _ = done.send((result, started.elapsed()));
        });
        let (result, waited) = finished
            .recv_timeout(Duration::from_secs(20))
            .expect("the waiting start gives up instead of waiting for ever");
        drop(held);
        let err = result.expect_err("refused");
        assert!(matches!(err, SelfCheckError::Unfinished { .. }), "{err:?}");
        assert!(
            err.to_string().contains("another check of this Codex did not finish"),
            "{err}"
        );
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
    }

    #[test]
    fn a_failure_is_not_recorded_and_names_its_log() {
        let scratch = Scratch::new("fail");
        let program = scratch.program("codex", "codex-cli 0.161.0");
        let state = scratch.state();
        let runs = Cell::new(0);
        let err = check(
            &program,
            &state,
            &runs,
            Err(vec!["the sandbox let a write through".into()]),
        )
        .expect_err("fails");
        let SelfCheckError::Failed { reasons, log } = &err else {
            panic!("{err:?}")
        };
        assert_eq!(reasons, &vec!["the sandbox let a write through".to_string()]);
        let message = err.to_string();
        assert!(message.starts_with("Eitri could not confirm that this Codex keeps its levers; see "));
        assert!(message.ends_with(&log.display().to_string()));
        let text = std::fs::read_to_string(log).expect("the log is kept");
        assert!(
            text.contains("FAILED") && text.contains("the sandbox let a write through"),
            "{text}"
        );
        assert_eq!(mode(log), 0o600);
        assert!(!log.parent().expect("dir").join("verdict.json").exists());
        // The next start tries again.
        check(&program, &state, &runs, Ok(())).expect("passes");
        assert_eq!(runs.get(), 2);
    }

    #[test]
    fn a_changed_binary_reruns_the_check() {
        let scratch = Scratch::new("changed");
        let program = scratch.program("codex", "codex-cli 0.161.0");
        let state = scratch.state();
        let runs = Cell::new(0);
        check(&program, &state, &runs, Ok(())).expect("passes");
        let before = BinaryKey::of(&program).expect("key");

        // The same file with a later modification time is another binary.
        let file = File::options().write(true).open(&program).expect("opens");
        file.set_modified(SystemTime::now() + Duration::from_secs(60))
            .expect("sets mtime");
        drop(file);
        let after = BinaryKey::of(&program).expect("key");
        assert_eq!(before.size, after.size);
        assert_ne!(before.mtime_ns, after.mtime_ns);
        assert_ne!(before.id(), after.id());
        check(&program, &state, &runs, Ok(())).expect("passes");
        assert_eq!(runs.get(), 2);

        // A new version at the same path is another binary too.
        let upgraded = BinaryKey::new(after.path.clone(), after.size, after.mtime_ns, "0.162.0".into());
        assert_ne!(after.id(), upgraded.id());
    }

    #[test]
    fn only_the_checked_version_is_the_checked_binary() {
        let key = BinaryKey::new(PathBuf::from("/c"), 1, 1, "0.161.0".into());
        assert!(key.is_version((0, 161, 0)));
        assert!(!key.is_version((0, 162, 0)));
        assert!(!key.is_version((0, 160, 0)));
        let unreadable = BinaryKey::new(PathBuf::from("/c"), 1, 1, "unknown".into());
        assert!(
            !unreadable.is_version((0, 161, 0)),
            "a version that cannot be read matches nothing"
        );
    }

    #[test]
    fn ensure_checked_names_the_binary_it_checked() {
        let scratch = Scratch::new("returns-key");
        let program = scratch.program("codex", "codex-cli 0.161.0");
        let key = ensure_checked_with(&program, &scratch.state(), true, |_, _, _| Ok(())).expect("passes");
        assert_eq!(key, BinaryKey::of(&program).expect("key"));
        let cached = ensure_checked_with(&program, &scratch.state(), true, |_, _, _| {
            Err(vec!["not asked".into()])
        })
        .expect("the cached pass");
        assert_eq!(cached, key);
    }

    #[test]
    fn a_missing_or_too_old_binary_is_refused_before_any_check() {
        let scratch = Scratch::new("refused");
        let state = scratch.state();
        let runs = Cell::new(0);
        let missing = scratch.0.join("no-such-codex");
        assert_eq!(check(&missing, &state, &runs, Ok(())), Err(SelfCheckError::NotFound));
        let old = scratch.program("old", "codex-cli 0.159.2");
        let err = check(&old, &state, &runs, Ok(())).expect_err("too old");
        assert_eq!(err, SelfCheckError::TooOld { found: (0, 159, 2) });
        assert!(err.to_string().contains("0.159.2") && err.to_string().contains("0.160.0"));
        let nonsense = scratch.program("nonsense", "not a version");
        assert!(matches!(
            check(&nonsense, &state, &runs, Ok(())),
            Err(SelfCheckError::Failed { .. })
        ));
        assert_eq!(runs.get(), 0, "no check ran for any of them");
    }

    fn facts(thread: &str) -> ThreadFacts {
        ThreadFacts {
            thread_id: thread.to_string(),
            echo_problem: None,
            instruction_sources: Vec::new(),
            reasoning_effort: Some("low".to_string()),
        }
    }

    fn passing() -> Evidence {
        Evidence {
            start: Some(facts("parent")),
            resume: Some(facts("parent")),
            read_only: Some(facts("read-only")),
            read_only_turn_done: true,
            read_only_asked: true,
            announced_child: Some("child".into()),
            child_asked_after_announcement: true,
            child_requests: 4,
            outputs: ["ro1", "p0", "cc1", "cc0"].iter().map(|s| s.to_string()).collect(),
            first_turn_done: true,
            child_turn_done: true,
            resumed_turn_done: true,
            version_ok: true,
            ..Evidence::default()
        }
    }

    #[test]
    fn the_verdict_passes_only_when_every_lever_held() {
        assert_eq!(judge(&passing()), Vec::<String>::new());

        let says = |mutate: fn(&mut Evidence), word: &str| {
            let mut e = passing();
            mutate(&mut e);
            let why = judge(&e);
            assert!(why.iter().any(|r| r.contains(word)), "{word:?} not in {why:?}");
        };
        says(|e| e.markers = vec!["hook"], "project's own commands ran");
        says(|e| e.created = vec!["the declined escalation"], "let a write through");
        says(|e| e.leaked = vec!["the project's skill"], "skill");
        says(|e| e.projects_written = true, "project entry");
        says(|e| e.announced_child = None, "no child was announced");
        says(|e| e.child_requests = 0, "no request of a child");
        says(|e| e.child_asked_after_announcement = false, "escalation");
        says(|e| e.outputs.clear(), "never reported back");
        says(|e| e.steps_left_after_first_turn = 2, "never asked for");
        says(|e| e.first_turn_done = false, "first turn");
        says(|e| e.resumed_turn_done = false, "after the resume");
        says(|e| e.version_ok = false, "version");
        says(|e| e.resume = None, "resume gave no answer");
        says(|e| e.read_only = None, "read-only start gave no answer");
        says(|e| e.read_only_turn_done = false, "read-only thread's turn");
        says(|e| e.read_only_asked = false, "read-only thread's escalation");
        says(
            |e| {
                e.outputs.remove("ro1");
            },
            "ro1 never reported back",
        );
        says(
            |e| {
                if let Some(f) = e.read_only.as_mut() {
                    f.echo_problem = Some("sandbox differs".into());
                }
            },
            "read-only start did not echo",
        );
        says(
            |e| {
                if let Some(f) = e.start.as_mut() {
                    f.echo_problem = Some("sandbox differs".into());
                }
            },
            "narrow policy",
        );
        says(
            |e| {
                if let Some(f) = e.resume.as_mut() {
                    f.instruction_sources = vec!["/p/AGENTS.md".into()];
                }
            },
            "instruction files",
        );
        says(
            |e| {
                if let Some(f) = e.start.as_mut() {
                    f.reasoning_effort = Some("minimal".into());
                }
            },
            "reasoning effort",
        );
    }

    #[test]
    fn colour_codes_are_taken_out_of_the_log() {
        assert_eq!(
            without_colours("\u{1b}[2m12:00\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m text"),
            "12:00 ERROR text"
        );
    }

    #[test]
    fn the_fixture_files_fill_in_and_refuse_odd_paths() {
        let values = [
            ("@MARKER_SH@", "/r/marker.sh"),
            ("@MARKERS@", "/r/markers"),
            ("@OUTSIDE@", "/r/outside"),
            ("@EXTRA@", "/r/extra"),
        ];
        for (_, template) in FIXTURE_TEMPLATES.iter().chain([&("home", FIXTURE_HOME)]) {
            let filled = fill(template, &values);
            assert!(!filled.contains('@'), "an unfilled placeholder in {filled}");
        }
        for (dest, template) in manifest_entries() {
            let text = FIXTURE_TEMPLATES.iter().find(|(name, _)| *name == template);
            assert!(
                text.is_some(),
                "{dest} names a template that is not built in: {template}"
            );
            if dest.ends_with(".json") {
                let filled = fill(text.map_or("", |(_, t)| t), &values);
                assert!(
                    serde_json::from_str::<Value>(&filled).is_ok(),
                    "{dest} is not JSON: {filled}"
                );
            }
        }
        assert_eq!(manifest_entries().count(), FIXTURE_TEMPLATES.len());
        assert!(plain_path(Path::new("/a b/c.d")).is_ok());
        for odd in ["/a\"b", "/a'b", "/a\\b", "/a\nb"] {
            assert!(plain_path(Path::new(odd)).is_err(), "{odd}");
        }
        // The hook's key and hash are read out of whatever shape the listing has.
        let listing = json!({"data": [{"cwd": "/p", "hooks": [{"key": "/p/.codex/hooks.json:pre_tool_use:0:0", "currentHash": "sha256:ab"},
            {"key": "/other/hooks.json:pre_tool_use:0:0", "currentHash": "sha256:cd"}]}]});
        assert_eq!(
            find_hook(&listing, "/p"),
            Some((
                "/p/.codex/hooks.json:pre_tool_use:0:0".to_string(),
                "sha256:ab".to_string()
            ))
        );
        assert_eq!(find_hook(&listing, "/q"), None);
    }
}
