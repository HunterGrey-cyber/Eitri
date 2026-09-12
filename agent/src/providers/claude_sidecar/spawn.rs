//! Locates, builds if necessary, and spawns Verdandi's `apps/claude-sidecar` Node.js process --
//! deliberately NOT reusing `shell/src/supervisor_client.rs`'s spawn pattern wholesale. Two real,
//! verified differences from that pattern (see this plan's "Verified facts", points 5 and 6):
//!
//! 1. `apps/claude-sidecar/dist/` is gitignored in Verdandi, not committed -- a git-dependency
//!    checkout of the Rust protocol crate (Task 1) does NOT give this process a runnable sidecar.
//!    This module locates a real Verdandi checkout on disk and builds the sidecar there if needed.
//! 2. The sidecar treats stdin EOF as its own parent-death shutdown signal (its own `lifecycle.ts`
//!    says so explicitly). `Stdio::null()` -- the pattern `supervisor_client.rs` uses -- makes it
//!    exit within the same tick it starts, since reading from `/dev/null` is an instant EOF. This
//!    module spawns with `Stdio::piped()` for stdin and holds the write handle open for the whole
//!    `SpawnedSidecar`'s lifetime; dropping (or explicitly closing) that handle is the intended
//!    shutdown signal, not a process_group/signal trick.
//!
//! Unlike `supervisor` (a shared, machine-wide, detached daemon `shell` never waits on), each
//! `ClaudeSidecarProvider` owns exactly one sidecar process for its own lifetime, closer to how
//! `agent::process::AgentProcess` owns its one `claude` child -- so this module does NOT detach via
//! `process_group(0)`; the sidecar is meant to die with its owning provider, not outlive it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many of the sidecar's most recent stderr lines are retained for diagnostics. Bounded on
/// purpose: a long-lived sidecar can log indefinitely, and this buffer exists to explain a startup
/// failure or carry a compatibility warning, not to be a second log file.
const STDERR_TAIL_CAPACITY: usize = 40;

/// The sidecar's recent stderr, shared between its drain thread and whoever needs to explain what
/// went wrong. Before this existed, the sidecar's own startup errors reached `eprintln!` and
/// nowhere else: a real CLI-version refusal (which happens *before* the socket is bound) surfaced
/// to the caller as nothing but `TimedOut: did not bind ... within 1000ms`, with the actual cause
/// invisible to any UI.
#[derive(Default)]
struct StderrTail {
    lines: VecDeque<String>,
}

impl StderrTail {
    fn push(&mut self, line: String) {
        if self.lines.len() == STDERR_TAIL_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn snapshot(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

pub(crate) struct SpawnedSidecar {
    pub(crate) socket_path: PathBuf,
    /// Which Verdandi checkout and revision this sidecar was built and launched from. Descriptive,
    /// always present -- reaches the UI as `ProviderInfo::verdandi_checkout`, NOT as a diagnostic.
    pub(crate) checkout_description: String,
    /// Only things genuinely worth warning about (today: baseline drift). Usually empty.
    pub(crate) checkout_warnings: Vec<String>,
    stdin_keepalive: Option<ChildStdin>,
    child: Child,
    stderr_tail: Arc<Mutex<StderrTail>>,
}

impl SpawnedSidecar {
    /// The sidecar's most recent stderr lines, oldest first. Not only a failure path: the sidecar
    /// emits a real CLI-compatibility diagnostic here on a successful-but-untested start, which a
    /// UI should be able to show without scraping this process's own stderr.
    pub(crate) fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().map(|tail| tail.snapshot()).unwrap_or_default()
    }

    /// The spawned sidecar's OS pid -- the only safe handle for an orphan check (see
    /// `ClaudeSidecarProvider::sidecar_pid`).
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }
}

/// The Verdandi revision this client is developed and verified against.
///
/// Not a hard pin -- `NEOVIBE_VERDANDI_CHECKOUT` exists precisely so a Verdandi feature branch or a
/// protocol migration can be tested against an unreleased sidecar, and hard-failing on a different
/// revision would defeat that. It is a BASELINE: when the checkout is at a different revision, that
/// fact is surfaced as a startup diagnostic instead of being silent, so "which sidecar build was
/// this session actually running?" is answerable after the fact rather than guessed at.
///
/// Currently `c331615` on Verdandi's `sdk-mainline-unblock`: the revision whose real-sidecar
/// conformance this crate's own `claude_sidecar_lifecycle_conformance` suite passed against --
/// multi-turn with a content oracle, BYPASS tool execution, post-interrupt reuse, orphan-free
/// teardown, and (as of this revision) a real resume proven by both a content oracle and a
/// provider-session-id match, and partial assistant streaming measured before/after. Moved
/// here (eb70aa3 -> 2fd30fb -> c331615) only after that suite went green against each; move it
/// again on the same terms, not before.
pub const EXPECTED_VERDANDI_REVISION: &str = "a2f194a";

/// Where `NEOVIBE_VERDANDI_CHECKOUT` came from, and what it points at. Carried onto `ProviderInfo`
/// so the UI can name the backend build it is talking to.
pub(crate) struct VerdandiCheckout {
    pub(crate) path: PathBuf,
    /// `git rev-parse --short HEAD`, or `None` when the checkout is not a git repo or `git` is
    /// unavailable. Best-effort diagnostics only -- never a reason to refuse to start.
    pub(crate) revision: Option<String>,
    /// True when `NEOVIBE_VERDANDI_CHECKOUT` chose this path rather than the default.
    pub(crate) from_override: bool,
}

/// Locates a real Verdandi checkout.
///
/// `$NEOVIBE_VERDANDI_CHECKOUT` is a **supported development/integration override**, not a
/// temporary hack: it is how this client is pointed at a Verdandi feature branch, a protocol
/// migration, or an isolated checkout while the default one is mid-work. It takes precedence over
/// the default `$HOME/src/verdandi` and is validated the same way, so a typo'd path
/// fails with a clear message rather than silently falling back to a different sidecar than the
/// operator intended.
///
/// Either way this is a dev-machine story, not a production distribution one -- spec §13's
/// packaging profiles remain separately deferred.
fn locate_verdandi_checkout() -> std::io::Result<VerdandiCheckout> {
    let (path, from_override) = match std::env::var("NEOVIBE_VERDANDI_CHECKOUT") {
        Ok(path) if !path.trim().is_empty() => (PathBuf::from(path.trim()), true),
        _ => {
            let home = std::env::var("HOME").map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "HOME is unset and NEOVIBE_VERDANDI_CHECKOUT was not provided",
                )
            })?;
            (PathBuf::from(home).join("src/verdandi"), false)
        }
    };

    // Validated for BOTH paths now. Previously only the default was checked, so a mistyped override
    // reached `ensure_sidecar_built` and failed somewhere much less informative.
    if !path.join("apps/claude-sidecar/package.json").exists() {
        let origin = if from_override {
            "NEOVIBE_VERDANDI_CHECKOUT points at"
        } else {
            "no Verdandi checkout found at the default"
        };
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "{origin} {path:?}, which has no apps/claude-sidecar/package.json -- \
                 set NEOVIBE_VERDANDI_CHECKOUT to a real Verdandi checkout"
            ),
        ));
    }

    let revision = git_short_revision(&path);
    Ok(VerdandiCheckout { path, revision, from_override })
}

/// Best-effort `git rev-parse --short HEAD`. Any failure (not a repo, no `git`, detached weirdness)
/// yields `None` -- this is diagnostics, and a diagnostic that can refuse to start is worse than no
/// diagnostic.
fn git_short_revision(checkout: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(checkout)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if revision.is_empty() {
        None
    } else {
        Some(revision)
    }
}

/// One line saying which backend build this session is running, plus a separate list of things
/// genuinely worth warning about.
///
/// The split is load-bearing. The description is unconditional -- there is always a checkout, and a
/// missing git revision degrades to "unknown revision" rather than suppressing the line -- so
/// folding it into the warning list made that list never empty, and any UI that treats "has
/// diagnostics" as "something is wrong" lit up permanently for every healthy session. A warning
/// that is always on is not a warning.
fn describe_checkout(checkout: &VerdandiCheckout) -> (String, Vec<String>) {
    let source = if checkout.from_override { "NEOVIBE_VERDANDI_CHECKOUT" } else { "default path" };
    let revision = checkout.revision.as_deref().unwrap_or("unknown revision");
    let description = format!("Verdandi checkout: {} @ {revision} (via {source})", checkout.path.display());

    let mut warnings = Vec::new();
    // `starts_with` rather than equality: `git rev-parse --short` picks its own abbreviation length,
    // which grows as a repository does, so a strict comparison would start reporting false drift.
    if let Some(actual) = &checkout.revision {
        if !actual.starts_with(EXPECTED_VERDANDI_REVISION) && !EXPECTED_VERDANDI_REVISION.starts_with(actual.as_str()) {
            warnings.push(format!(
                "Verdandi baseline drift: running {actual}, this client was verified against \
                 {EXPECTED_VERDANDI_REVISION}. Not an error -- testing a Verdandi branch is exactly \
                 what NEOVIBE_VERDANDI_CHECKOUT is for -- but if behavior looks wrong, this is the \
                 first thing to check."
            ));
        }
    }
    (description, warnings)
}

/// True if the checkout's root `package.json` declares a `build` script.
///
/// Load-bearing, not a nicety: `apps/claude-sidecar`'s own `build` script does NOT build the
/// `@verdandi/claude-runtime` kernel it imports, and the sidecar resolves that package through a
/// workspace symlink to `packages/claude-runtime/dist`. Building only the app therefore either
/// fails outright (clean checkout: 8x TS2307) or -- worse -- silently succeeds against whatever
/// stale kernel `dist` happens to be present. Verdandi gained a kernel-first root fan-out on
/// 2026-09-11; a checkout predating that has no root `build` script, so this probe is what decides
/// between the correct path and the legacy fallback below.
fn checkout_has_root_build_script(checkout: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(checkout.join("package.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    manifest.get("scripts").and_then(|s| s.get("build")).and_then(|b| b.as_str()).is_some()
}

/// Existence check only, not mtime-based staleness (unlike `shell/build.rs`'s own agent-ui
/// pattern) -- this runs at every provider construction (spawn time), not once at `cargo build`
/// time, so walking every source file's mtime on each spawn is a real, avoidable cost. A developer
/// who changes Verdandi's sidecar/kernel source without rebuilding must `rm -rf dist/` themselves
/// to force a rebuild -- a documented manual step, not a silent staleness bug this module hides.
///
/// Both dist entries are checked, not just the sidecar's: a present sidecar `dist` with a missing
/// kernel `dist` is a real, reachable state (they are built by separate `tsc` invocations) and it
/// fails at `node` startup with a module-resolution error rather than anywhere useful.
fn ensure_sidecar_built(checkout: &Path) -> std::io::Result<PathBuf> {
    let sidecar_dir = checkout.join("apps/claude-sidecar");
    let dist_entry = sidecar_dir.join("dist/src/index.js");
    let kernel_dist_entry = checkout.join("packages/claude-runtime/dist/src/index.js");
    if dist_entry.exists() && kernel_dist_entry.exists() {
        return Ok(dist_entry);
    }
    if !checkout.join("node_modules").exists() {
        run_command(checkout, "npm", &["ci"])?;
    }
    if checkout_has_root_build_script(checkout) {
        run_command(checkout, "npm", &["run", "build"])?;
    } else {
        // A Verdandi checkout predating the root fan-out. Do the ordering by hand rather than
        // building only the app, which is what silently produced stale-kernel runs.
        eprintln!(
            "agent: {checkout:?} has no root `build` script (it predates Verdandi's kernel-first \
             fan-out); building the kernel and the sidecar separately, in that order"
        );
        run_command(&checkout.join("packages/claude-runtime"), "npm", &["run", "build"])?;
        run_command(&sidecar_dir, "npm", &["run", "build"])?;
    }
    for entry in [&kernel_dist_entry, &dist_entry] {
        if !entry.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("the Verdandi build completed but {entry:?} still does not exist"),
            ));
        }
    }
    Ok(dist_entry)
}

fn run_command(dir: &Path, program: &str, args: &[&str]) -> std::io::Result<()> {
    let status = Command::new(program).args(args).current_dir(dir).status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "{program} {args:?} failed in {dir:?} with status {status}"
        )));
    }
    Ok(())
}

/// Spawns a fresh sidecar for one `ClaudeSidecarProvider` instance. `instance_id` becomes part of
/// the socket path (mirrors `agent::process`'s own per-conversation UUID socket convention,
/// `std::env::temp_dir().join(format!("neovibe-agent-hook-{conversation_id}.sock"))`) so multiple
/// concurrent providers never collide on one path.
pub(crate) fn spawn(instance_id: &str) -> std::io::Result<SpawnedSidecar> {
    let checkout = locate_verdandi_checkout()?;
    let (checkout_description, checkout_warnings) = describe_checkout(&checkout);
    eprintln!("agent: {checkout_description}");
    for line in &checkout_warnings {
        eprintln!("agent: {line}");
    }
    let checkout = checkout.path;
    let dist_entry = ensure_sidecar_built(&checkout)?;
    let socket_path = std::env::temp_dir().join(format!("neovibe-claude-sidecar-{instance_id}.sock"));
    let _ = std::fs::remove_file(&socket_path); // stale leftover from a prior crash, if any

    let mut command = Command::new("node");
    command
        .arg(&dist_entry)
        .env("VERDANDI_CLAUDE_SIDECAR_SOCKET", &socket_path)
        .stdin(Stdio::piped())
        // stdout/stderr are captured, not nulled -- `supervisor`'s own follow-up review found that
        // nulling a spawned child's stderr silently discards real crash diagnostics (2026-09-09,
        // supervisor-robustness-followup plan). A Node.js sidecar crash during real CLI parity
        // testing (Task 9) is exactly the kind of thing worth seeing.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let stdin_keepalive = child.stdin.take();

    let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
    if let Some(stdout) = child.stdout.take() {
        spawn_log_drain_thread(stdout, "stdout", None);
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_drain_thread(stderr, "stderr", Some(Arc::clone(&stderr_tail)));
    }

    // 3s, not the former 1s. The old budget had no headroom over a cold Node start plus the Claude
    // Agent SDK's own module graph, and its expiry was indistinguishable from a real refusal.
    const RETRY_ATTEMPTS: u32 = 60;
    const RETRY_DELAY: Duration = Duration::from_millis(50);
    for _ in 0..RETRY_ATTEMPTS {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return Ok(SpawnedSidecar { socket_path, checkout_description, checkout_warnings, stdin_keepalive, child, stderr_tail });
        }
        // Check for a dead child BEFORE sleeping again. The sidecar fails closed on a policy
        // violation (an incompatible Claude CLI, a socket already in use) by throwing before it
        // ever binds, so the process is simply gone -- waiting out the remaining retries would turn
        // an immediate, fully-explained failure into a multi-second silent timeout. This is exactly
        // how a real CLI-version refusal used to reach the UI as `did not bind ... within 1000ms`.
        if let Ok(Some(status)) = child.try_wait() {
            drop(stdin_keepalive);
            // The drain threads may still be flushing the final lines the child wrote on its way
            // out; those lines are the entire diagnosis, so give them a moment to land rather than
            // reporting an empty cause.
            std::thread::sleep(Duration::from_millis(100));
            return Err(std::io::Error::other(describe_startup_failure(
                &format!("claude-sidecar exited with {status} before binding {socket_path:?}"),
                &stderr_tail,
            )));
        }
        std::thread::sleep(RETRY_DELAY);
    }
    // Still running but never bound -- clean up rather than leaking a half-started process, then
    // report the failure with whatever it managed to say for itself.
    drop(stdin_keepalive);
    let _ = child.kill();
    let _ = child.wait();
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        describe_startup_failure(
            &format!(
                "claude-sidecar did not bind {socket_path:?} within {}ms",
                RETRY_ATTEMPTS as u64 * RETRY_DELAY.as_millis() as u64
            ),
            &stderr_tail,
        ),
    ))
}

/// Builds a startup-failure message that actually names the cause, by appending whatever the
/// sidecar printed to stderr before dying. Without this the caller gets a transport-shaped symptom
/// ("did not bind") for what is usually a policy-shaped cause ("refusing to start: claude CLI
/// version 2.1.269 ...") -- and the real message exists only in this process's own stderr, where no
/// UI can reach it.
fn describe_startup_failure(summary: &str, stderr_tail: &Arc<Mutex<StderrTail>>) -> String {
    let lines = stderr_tail.lock().map(|tail| tail.snapshot()).unwrap_or_default();
    if lines.is_empty() {
        return format!("{summary} (the sidecar printed nothing to stderr)");
    }
    format!("{summary}; its stderr said:\n{}", lines.join("\n"))
}

fn spawn_log_drain_thread<R: std::io::Read + Send + 'static>(
    reader: R,
    stream_name: &'static str,
    stderr_tail: Option<Arc<Mutex<StderrTail>>>,
) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(reader).lines().map_while(Result::ok) {
            eprintln!("claude-sidecar[{stream_name}]: {line}");
            if let Some(tail) = &stderr_tail {
                if let Ok(mut tail) = tail.lock() {
                    tail.push(line);
                }
            }
        }
    });
}

impl Drop for SpawnedSidecar {
    /// Graceful-first shutdown: close the held-open stdin (the sidecar's own documented
    /// parent-death signal, verified for real during this plan's preparation), wait briefly (a
    /// graceful exit lets the sidecar fail-close any pending permissions server-side before this
    /// process moves on), then escalate to SIGKILL if it hasn't exited -- matching design doc
    /// §12.1's shutdown escalation requirement. Deliberately simpler than
    /// `shell/src/supervisor_client.rs::connect_or_spawn`'s own background-thread reaper: that
    /// pattern exists there because a *detached* supervisor process may run for an unbounded time
    /// after being spawned, so its eventual `wait()` has no natural bound to block on. Here, by the
    /// time `drop` runs, the sidecar is either already exiting gracefully (bounded by the 3s
    /// deadline below) or about to be SIGKILLed (which returns near-instantly) -- both paths are
    /// genuinely bounded, so blocking `Drop` itself for at most ~3s is a deliberate, bounded wait,
    /// not the unbounded kind Global Constraints forbids.
    fn drop(&mut self) {
        drop(self.stdin_keepalive.take()); // closes the write end -> sidecar sees stdin EOF

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match self.child.try_wait() {
                Ok(Some(_status)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real, not mocked -- spawns the actual compiled sidecar (building it first if needed) and
    /// confirms both that it binds its socket and that closing the held-open stdin handle (this
    /// module's own documented shutdown mechanism) actually makes it exit. No real API cost: no
    /// Claude CLI turn is ever sent.
    #[test]
    #[ignore]
    fn spawn_binds_the_socket_and_stdin_close_shuts_it_down_cleanly() {
        let instance_id = uuid::Uuid::new_v4().to_string();
        let mut sidecar = spawn(&instance_id).expect("spawn should succeed against a real Verdandi checkout");
        assert!(std::os::unix::net::UnixStream::connect(&sidecar.socket_path).is_ok());

        drop(sidecar.stdin_keepalive.take());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while std::time::Instant::now() < deadline {
            if matches!(sidecar.child.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(exited, "sidecar did not exit within 5s of its stdin being closed");
        let _ = std::fs::remove_file(&sidecar.socket_path);
    }
}
