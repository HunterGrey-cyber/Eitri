//! Spawns and manages a real `claude` CLI child process. Mirrors this project's own
//! already-proven `nvim --embed` lifecycle discipline (graceful shutdown attempt, escalate to
//! SIGTERM, escalate to SIGKILL, verify no orphan remains) rather than inventing a new one --
//! see `neovide-editor`'s own process handling (`NeovideEditorPane::shutdown` /
//! `LiveHarness::shutdown` in the sibling `neovide` checkout) for the pattern this follows:
//! attempt a clean stop, escalate if it doesn't take, block until the child is actually gone
//! before returning, and defend against a caller forgetting to call `shutdown()` at all via
//! `Drop`. The concrete mechanism differs (this manages a real `std::process::Child` directly,
//! so unlike `LiveHarness` -- which has no way to force-kill a stuck `nvim` short of an RPC
//! command it hopes the child answers -- this can always fall back to a real SIGKILL and `wait()`
//! for a real, verified exit), but the shutdown *shape* (graceful attempt -> escalate ->
//! block-until-confirmed-gone -> idempotent -> `Drop` fallback) is the same one.

use crate::event::AgentEvent;
use crate::wire::translate_line;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long to wait after a graceful-stop signal before escalating to SIGKILL. `claude -p` is a
/// one-shot, non-interactive invocation with no piped stdin to ask it to quit "nicely" over (see
/// `spawn`'s own doc) -- this grace period exists only for the abnormal case of a caller wanting
/// to abandon an in-flight turn, giving the child a brief chance to unwind on its own signal
/// handler before being killed outright.
const GRACE_PERIOD: Duration = Duration::from_millis(300);

/// Whether this spawn starts a brand-new session or continues an existing one. Session identity
/// is always caller-assigned (see this plan's Global Constraint) -- never scraped from the
/// child's own output as the primary source of truth.
#[derive(Debug, Clone, Copy)]
pub enum SpawnMode {
    New { session_id: Uuid },
    Resume { session_id: Uuid },
}

impl SpawnMode {
    /// The caller-assigned session id this mode carries, regardless of whether it's a fresh
    /// session or a resume -- both variants know what UUID they're asking the CLI to use.
    pub fn session_id(&self) -> Uuid {
        match self {
            SpawnMode::New { session_id } | SpawnMode::Resume { session_id } => *session_id,
        }
    }
}

/// How long `shutdown()` waits for the child to exit naturally (no signal sent at all) before
/// escalating to SIGTERM. `claude -p` is one-shot and should already be finishing on its own by
/// the time a caller normally calls `shutdown()` (right after observing `TurnFinished`) -- this
/// grace period exists so a process that's mid-flush of its own session state on disk isn't
/// killed out from under itself, a real, plausible source of flakiness for anything relying on
/// session continuity (e.g. `--resume`).
const NATURAL_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(500);
const NATURAL_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A conservative, documented starting point for `disallowed_tools` -- see this plan's Global
/// Constraint on why this is best-effort, not a guarantee. Callers may pass their own list
/// instead; this is a suggested default, not hardcoded into `spawn`.
pub const CONSERVATIVE_DISALLOWED_TOOLS: &[&str] = &["Bash", "Write", "Edit", "NotebookEdit"];

/// A live (or just-exited) `claude` CLI child process, with its stdout being translated into
/// `AgentEvent`s on a background thread and buffered for non-blocking pickup via
/// [`poll_events`](Self::poll_events).
pub struct AgentProcess {
    child: Child,
    events_rx: Receiver<AgentEvent>,
    reader_handle: Option<JoinHandle<()>>,
    /// Background thread draining the child's stderr line-by-line into the same channel as
    /// stdout (see `spawn`). Reading this is what stops the child from ever blocking on a full
    /// stderr pipe buffer -- kept separate from `reader_handle` only because it's a second,
    /// independent thread that `shutdown()` must also join.
    stderr_handle: Option<JoinHandle<()>>,
    /// Set once this process's exit has been observed via `try_wait()` and a corresponding
    /// `AgentEvent::ProcessExited` has been pushed -- makes that push fire exactly once, no
    /// matter how many more times `poll_events()` is called afterward.
    exit_reported: bool,
    /// Set once `shutdown()` has run to completion -- makes `shutdown()` idempotent (a second
    /// call is a harmless no-op) and lets `Drop` tell whether it still has work to do, mirroring
    /// `LiveHarness`'s own `shut_down` guard.
    shut_down: bool,
}

impl AgentProcess {
    /// Spawns a real `claude -p` child process for one turn. `prompt` is sent as the CLI's
    /// positional argument (not over stdin -- this crate never pipes stdin to the child; each
    /// turn is a fresh, one-shot invocation, continuing prior context via `SpawnMode::Resume`
    /// rather than a long-lived streaming-input session). `disallowed_tools` is forwarded as
    /// `--disallowedTools` (see this plan's Global Constraint on why this, not
    /// `--permission-mode`/`--permission-prompts`, is v1's best-effort safety default).
    ///
    /// Always passes `--setting-sources project` (never the default, which also loads the
    /// *calling human's own* global `~/.claude` hooks/plugins -- a real, reproduced interaction
    /// this plan's own research confirmed) and never passes `--bare` (confirmed to break
    /// OAuth-authenticated sessions on this machine).
    pub fn spawn(prompt: &str, mode: SpawnMode, disallowed_tools: &[&str]) -> std::io::Result<Self> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--setting-sources")
            .arg("project")
            .stdout(Stdio::piped())
            .stdin(Stdio::null())
            .stderr(Stdio::piped());

        match mode {
            SpawnMode::New { session_id } => {
                cmd.arg("--session-id").arg(session_id.to_string());
            }
            SpawnMode::Resume { session_id } => {
                cmd.arg("--resume").arg(session_id.to_string());
            }
        }

        if !disallowed_tools.is_empty() {
            cmd.arg("--disallowedTools").arg(disallowed_tools.join(","));
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout must be present");
        let stderr = child.stderr.take().expect("piped stderr must be present");

        let (tx, rx) = std::sync::mpsc::channel();
        let stderr_tx = tx.clone();
        let reader_handle = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                for event in translate_line(&line) {
                    // The receiver may already be gone (process dropped/shut down) -- a send
                    // error here just means "stop reading", not a bug to report.
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }
        });

        // Same pattern as the stdout reader above, on the same channel (`Sender` is `Clone`) --
        // this is what actually drains stderr instead of letting the child block once its pipe
        // buffer fills, and what makes real diagnostics observable instead of silently discarded.
        let stderr_handle = std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if stderr_tx.send(AgentEvent::ProcessStderr { line }).is_err() {
                    return;
                }
            }
        });

        Ok(Self {
            child,
            events_rx: rx,
            reader_handle: Some(reader_handle),
            stderr_handle: Some(stderr_handle),
            exit_reported: false,
            shut_down: false,
        })
    }

    /// Non-blocking drain of whatever events have arrived since the last call -- mirrors
    /// `LiveHarness::pump(Duration::ZERO)`'s established shape in this codebase, so a future GTK
    /// tick callback (in `agent-ui`) can call this every frame with no redesign needed.
    pub fn poll_events(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        loop {
            match self.events_rx.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // Runs on the main thread, which already owns `self.child` -- no cross-thread `Child`
        // access here. Fires at most once per process, the first time an exit is actually
        // observed, independent of whatever did or didn't arrive over the channel this call.
        if !self.exit_reported {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exit_reported = true;
                events.push(AgentEvent::ProcessExited { success: status.success() });
            }
        }

        events
    }

    /// True once the child has exited on its own (e.g. after emitting its terminal `result`
    /// line) -- callers should check this after seeing an `AgentEvent::TurnFinished` to confirm
    /// the process actually went away, rather than assuming the two always coincide instantly.
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// The child process's OS pid. Exists to support orphan-safety verification (confirming no
    /// process remains alive after `shutdown()`) -- not otherwise used by this crate's own logic.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Graceful-then-escalating shutdown, matching this project's `nvim --embed` discipline: (1)
    /// if the child already exited on its own, just reap it; (2) otherwise send SIGTERM and give
    /// it a short grace period; (3) if it's still alive after that, SIGKILL it. Every path ends
    /// in a blocking `wait()`, so this only returns once the child is *confirmed* gone (reaped,
    /// not just signaled) -- callers must not assume this returns instantly. Idempotent: a second
    /// call is a no-op, mirroring `LiveHarness::shutdown`'s own idempotence guard.
    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;

        // Give the child a real chance to finish on its own first -- no signal sent at all here.
        // By the time `shutdown()` is normally called (right after seeing `TurnFinished`), the
        // one-shot `claude -p` process should already be exiting; jumping straight to SIGTERM
        // risks killing it mid-flush of its own on-disk session state.
        let natural_deadline = Instant::now() + NATURAL_EXIT_GRACE_PERIOD;
        while Instant::now() < natural_deadline && !self.has_exited() {
            std::thread::sleep(NATURAL_EXIT_POLL_INTERVAL);
        }

        if !self.has_exited() {
            #[cfg(unix)]
            {
                let _ = self.send_signal(SIGTERM);
                let deadline = Instant::now() + GRACE_PERIOD;
                while Instant::now() < deadline && !self.has_exited() {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if !self.has_exited() {
                    let _ = self.child.kill(); // SIGKILL
                }
            }
            #[cfg(not(unix))]
            {
                let _ = self.child.kill();
            }
        }
        // Blocks until the child is actually reaped -- this is what makes `shutdown()` a real
        // verification, not a fire-and-forget signal send. Safe to call even if the child already
        // exited (it just returns the already-recorded exit status).
        let _ = self.child.wait();

        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_handle.take() {
            let _ = handle.join();
        }
    }

    #[cfg(unix)]
    fn send_signal(&self, sig: i32) -> std::io::Result<()> {
        let pid = self.child.id() as i32;
        let ret = unsafe { libc_kill(pid, sig) };
        if ret == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    }
}

#[cfg(unix)]
const SIGTERM: i32 = 15;

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

impl Drop for AgentProcess {
    /// Defense in depth: if a caller drops an `AgentProcess` without calling `shutdown()`
    /// explicitly, this must not leak an orphaned `claude` subprocess -- same non-negotiable this
    /// project already holds `nvim --embed` to (mirrors `LiveHarness`'s own `Drop` -- "harmless/
    /// redundant on the already-shut-down happy path, best-effort otherwise").
    fn drop(&mut self) {
        if !self.shut_down {
            self.shutdown();
        }
    }
}
