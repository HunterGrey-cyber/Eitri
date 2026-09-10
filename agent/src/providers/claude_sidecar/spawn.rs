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

use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

pub(crate) struct SpawnedSidecar {
    pub(crate) socket_path: PathBuf,
    stdin_keepalive: Option<ChildStdin>,
    child: Child,
}

/// Locates a real Verdandi checkout: `$NEOVIBE_VERDANDI_CHECKOUT` if set, otherwise
/// `$HOME/src/verdandi` -- the actual sibling checkout every real verification during
/// this plan's own preparation ran against. This is a deliberate, documented dev-machine
/// convenience (this plan's own "Explicitly out of scope" section), not a production distribution
/// story -- spec §13's packaging profiles are separately deferred future work.
fn locate_verdandi_checkout() -> std::io::Result<PathBuf> {
    if let Ok(path) = std::env::var("NEOVIBE_VERDANDI_CHECKOUT") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME").map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is unset and NEOVIBE_VERDANDI_CHECKOUT was not provided")
    })?;
    let default_path = PathBuf::from(home).join("src/verdandi");
    if !default_path.join("apps/claude-sidecar/package.json").exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no Verdandi checkout found at {default_path:?} -- set NEOVIBE_VERDANDI_CHECKOUT to override"),
        ));
    }
    Ok(default_path)
}

/// Existence check only, not mtime-based staleness (unlike `shell/build.rs`'s own agent-ui
/// pattern) -- this runs at every provider construction (spawn time), not once at `cargo build`
/// time, so walking every source file's mtime on each spawn is a real, avoidable cost. A developer
/// who changes Verdandi's sidecar/kernel source without rebuilding must `rm -rf dist/` themselves
/// to force a rebuild -- a documented manual step, not a silent staleness bug this module hides.
fn ensure_sidecar_built(checkout: &Path) -> std::io::Result<PathBuf> {
    let sidecar_dir = checkout.join("apps/claude-sidecar");
    let dist_entry = sidecar_dir.join("dist/src/index.js");
    if dist_entry.exists() {
        return Ok(dist_entry);
    }
    if !checkout.join("node_modules").exists() {
        run_command(checkout, "npm", &["ci"])?;
    }
    run_command(&sidecar_dir, "npm", &["run", "build"])?;
    if !dist_entry.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("npm run build completed but {dist_entry:?} still does not exist"),
        ));
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

    if let Some(stdout) = child.stdout.take() {
        spawn_log_drain_thread(stdout, "stdout");
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_drain_thread(stderr, "stderr");
    }

    const RETRY_ATTEMPTS: u32 = 20;
    const RETRY_DELAY: Duration = Duration::from_millis(50);
    for _ in 0..RETRY_ATTEMPTS {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return Ok(SpawnedSidecar { socket_path, stdin_keepalive, child });
        }
        std::thread::sleep(RETRY_DELAY);
    }
    // The sidecar never bound its socket in time -- clean up rather than leaking a half-started
    // process, then report the failure.
    drop(stdin_keepalive);
    let _ = child.kill();
    let _ = child.wait();
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("claude-sidecar did not bind {socket_path:?} within {}ms", RETRY_ATTEMPTS as u64 * 50),
    ))
}

fn spawn_log_drain_thread<R: std::io::Read + Send + 'static>(reader: R, stream_name: &'static str) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(reader).lines().map_while(Result::ok) {
            eprintln!("claude-sidecar[{stream_name}]: {line}");
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
