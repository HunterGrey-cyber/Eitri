//! Spawns and manages one long-lived, full-duplex `claude` CLI child process for an entire
//! conversation (v2 -- see docs/superpowers/specs/2026-09-07-agent-v2-streaming-protocol-design.md).
//! Unlike v1 (one process per turn, prompt as a CLI arg, no piped stdin, session continuity via
//! `--resume`), this spawns exactly once per conversation with piped stdin/stdout/stderr, writes
//! NDJSON turns/control-requests to stdin as the conversation progresses, and relies on a
//! per-conversation Unix-socket listener thread to receive real `PreToolUse` permission requests
//! relayed by the `agent-hook` companion binary (Task 3) and write decisions back. Session
//! continuity is now just "the process is still alive" -- there is no session-id/`--resume`
//! concept left in this module at all.
//!
//! Shutdown discipline mirrors this project's already-proven `nvim --embed` lifecycle (graceful
//! attempt, escalate to SIGTERM, escalate to SIGKILL, block until confirmed gone, idempotent,
//! `Drop` fallback) -- see `neovide-editor`'s `NeovideEditorPane::shutdown`/`LiveHarness::shutdown`
//! in the sibling `neovide` checkout for the pattern this follows. New here vs v1: `shutdown()`'s
//! first action is to genuinely close stdin (drop the write end, sending real EOF to the child) --
//! both official Claude Code SDKs converge on this exact ordering as the real, intended way to ask
//! a long-lived `claude --print --input-format stream-json` process to wrap up.

use crate::event::{AgentEvent, PermissionSource};
use crate::hook_protocol::parse_pretooluse_input;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long `shutdown()` waits after sending SIGTERM before escalating to SIGKILL.
const GRACE_PERIOD: Duration = Duration::from_millis(300);
/// How long `shutdown()` waits for the child to exit naturally after stdin is closed (real EOF)
/// before escalating to a signal at all.
const NATURAL_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(500);
const NATURAL_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How often the hook-listener thread's non-blocking `accept()` poll loop checks both for an
/// incoming connection and for `hook_listener_stop` -- bounds how long `shutdown()` can be kept
/// waiting on that thread when nothing is connecting to the hook socket (the common case for any
/// turn that never triggers a real tool call).
const HOOK_LISTENER_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How long the hook-listener thread waits for an accepted connection to finish writing its one
/// line before giving up on it. On Linux, `accept()` being non-blocking does NOT make the accepted
/// `UnixStream` non-blocking too (confirmed real: an accepted socket on Linux does not inherit
/// `O_NONBLOCK` from a non-blocking listener). **macOS is the opposite** -- XNU copies the flag --
/// so the listener sets every accepted stream blocking explicitly before relying on this timeout
/// (see `spawn_hook_listener`; M1, 2026-09-17). Either way, without this, a connection that starts but never
/// completes its line (a real `agent-hook` process killed/orphaned mid-write) would leave
/// `read_line` blocked forever, with the same practical effect as the accept()-side deadlock this
/// module already fixed once. A few hundred ms is generous: under normal operation `agent-hook`
/// writes its one already-fully-read-from-its-own-stdin line immediately after connecting.
const HOOK_CONNECTION_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// How many of the child's most recent stderr lines are retained (newest evicting oldest) so a
/// later `ProcessExited { success: false, .. }` can carry the real reason instead of a generic
/// one. A handful is enough for a startup refusal (typically one line) without this becoming an
/// unbounded log of a long conversation's entire stderr.
const STDERR_TAIL_CAPACITY: usize = 20;

/// A conservative, documented starting point for `disallowed_tools` -- see this plan's Global
/// Constraint on why this is best-effort, not a guarantee. Callers may pass their own list
/// instead; this is a suggested default, not hardcoded into `spawn`.
pub const CONSERVATIVE_DISALLOWED_TOOLS: &[&str] = &["Bash", "Write", "Edit", "NotebookEdit"];

/// The deny list for a session in `mode`, and the difference between the two is a security
/// decision, not a convenience.
///
/// **Why they differ.** The real gate is the `PreToolUse` hook, and `Bypass` installs none -- see
/// `spawn_with_binary`, which passes no `--settings` at all in that mode, and Verdandi's own proto
/// comment saying the same of `BYPASS`. So in `Auto` an `Edit` is a permission card the user
/// answers, and in `Bypass` it is a file rewritten under a live buffer with no diff, no decision
/// and nothing in the transcript that had to be read. Those are not the same risk, so they do not
/// get the same list.
///
/// `Auto` therefore drops the three editing tools -- the gate covers them -- and keeps `Bash`,
/// whose blast radius is not a file but a machine. Un-denying `Bash` is a separate decision with
/// its own evidence, not a rounding error in this one.
///
/// **`Bypass` kept all four until 2026-09-20**, which was a narrower answer than removing the mode:
/// the mode stayed available and honest, running tools without asking, and the tools it could run
/// were the ones whose worst case is bounded by what the model can already do through `Read`/`Grep`.
///
/// **Overridden by the owner, 2026-09-20: `Bypass` now denies nothing.** He put it plainly -- "bypass
/// 不是应该是具有所有权限吗" -- and the paragraph above is the answer to why it was not, kept because
/// its reasoning is unchanged and will be the argument against this when the permission design is
/// revisited. What the paragraph got wrong is not the risk, it is the NAME: a mode labelled "bypass
/// all permissions" that is the only mode which cannot edit a file is not a narrow answer, it is a
/// mode whose label is false. `Auto` allows the three editing tools and gates them with a card;
/// `Bypass` allowed none of them. The weaker-sounding mode was strictly the more capable one.
///
/// **This is a debugging-phase decision with a stated expiry** ("我们在调试阶段，权限设计快上线了再做").
/// What it restores is exactly the risk the paragraph above names, and nothing has changed about
/// that risk: with no hook installed, an `Edit` rewrites a file under a live buffer with no diff, no
/// decision, and nothing in the transcript that had to be read. Before this ships to anyone else,
/// the survey's recommendation is on record and is a better answer than either version of this
/// function -- stop using `bypassPermissions` at all, run `Auto` with the host auto-answering every
/// request, and keep the tool calls and their diffs in the transcript. That is the shape every other
/// implementation uses: ACP's `PermissionOptionKind` makes "stop asking me" an ANSWER
/// (`AllowAlways`), never a capability the agent loses. See
/// `docs/canonical/2026-09-20-cursor-permission-survey.md`.
///
/// **Overridden by the owner again, 2026-09-25, for `Auto`: it denies nothing either.** "auto模式给
/// claude，和claude本身的做法一致" -- Auto gets `Bash`, as Claude Code's own `default` mode does.
/// The paragraph above that kept it ("whose blast radius is not a file but a machine") is answered
/// by the gate rather than by the list: every `Bash` call reaches the `PreToolUse` hook (matcher
/// `*`, both backends), `crate::permission_policy` auto-answers only a read-only command inside the
/// project, and everything else -- a write, a redirect, compound syntax, an option naming a path
/// outside the tree -- is a card a human answers. Removing `Bash` also removed the thing phase 3's
/// prefix rules (`crate::permission_rules`) and the card's "always allow" exist to answer, which the
/// phase-3 sandbox pass found. So both lists are empty now and what separates the modes is only
/// the gate: `Auto` installs it, `Bypass` does not. On the sidecar an empty list is sent as
/// `unrestricted`; Verdandi's `CONSERVATIVE_BYPASS_DENY` floor applies only under `bypass` and its
/// hook is installed for every other mode, so `Auto` keeps its gate (checked in Verdandi's source,
/// `usesDefaultBypassDeny` and `createSession`).
///
/// This is best-effort in both modes and was documented as such from the start -- no single
/// permission flag reliably blocks all tool use. It is the second line; the hook is the first.
pub fn disallowed_tools_for(mode: PermissionMode) -> &'static [&'static str] {
    match mode {
        // The hook gates every tool (matcher `*`), so an edit or a shell command reaches the policy,
        // and through it the user as a card when it needs one (owner's ruling, 2026-09-25).
        PermissionMode::Auto => &[],
        // Empty, not `CONSERVATIVE_DISALLOWED_TOOLS`. On the sidecar path this ALONE would have
        // changed nothing -- an empty list under `bypass` reads to Verdandi as silence and earns its
        // own identical floor -- so the request also states `unrestricted` (see
        // `providers::claude_sidecar::build_create_request`). The constant itself is untouched: it
        // is still the floor Verdandi injects for a caller that says nothing, and it is still
        // hand-copied there as `CONSERVATIVE_BYPASS_DENY`.
        PermissionMode::Bypass => &[],
    }
}

/// Which permission gate the CLI itself enforces for the whole conversation. `Auto` is the normal
/// mode: the real, reliable `PreToolUse` hook (relayed by `agent-hook` over the per-conversation
/// Unix socket) is the primary gate; `CanUseTool` control_requests are a secondary, unreliable
/// signal (see `PermissionSource`). `Bypass` skips permission gating entirely (no hook settings
/// are even generated) -- intended only for trusted, non-interactive callers.
///
/// `Serialize` (Task 1 of the wave-5 plan): carried on `AgentDomainEvent::PermissionModeChanged`,
/// which the panel's snapshot serializes as JSON. `rename_all = "snake_case"` matches this crate's
/// existing convention for wire-facing enums (see `AgentDomainEvent`'s own `#[serde(...)]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Auto,
    Bypass,
}

impl PermissionMode {
    fn as_cli_flag(&self) -> &'static str {
        match self {
            PermissionMode::Auto => "auto",
            PermissionMode::Bypass => "bypassPermissions",
        }
    }
}

/// Tracks one pending `HookRelay` permission request so `respond_permission` knows which live
/// `agent-hook` socket connection to write the decision back to. Removed once answered.
struct PendingHookConnection {
    stream: UnixStream,
}

/// A live (or just-exited) `claude` CLI child process for one whole conversation, with its stdout
/// being translated into `AgentEvent`s on a background thread and buffered for non-blocking
/// pickup via [`poll_events`](Self::poll_events).
pub struct AgentProcess {
    child: Child,
    /// `None` after `shutdown()` has taken it (closing the write end, sending EOF to the child)
    /// -- see `shutdown()`'s doc for why this is the chosen mechanism over raw-fd juggling.
    stdin: Option<ChildStdin>,
    events_rx: Receiver<AgentEvent>,
    reader_handle: Option<JoinHandle<()>>,
    /// Background thread draining the child's stderr line-by-line into the same channel as
    /// stdout (see `spawn`). Reading this is what stops the child from ever blocking on a full
    /// stderr pipe buffer -- kept separate from `reader_handle` only because it's a second,
    /// independent thread that `shutdown()` must also join.
    stderr_handle: Option<JoinHandle<()>>,
    /// Background thread accepting `agent-hook` connections on the per-conversation Unix socket.
    /// Runs a non-blocking poll loop (see `spawn`'s doc on why) so `shutdown()` can ask it to
    /// stop via `hook_listener_stop` and have it actually notice within one poll interval --
    /// unlike a blocking `for incoming in listener.incoming()` loop, which would never wake up on
    /// its own (confirmed real: removing the socket's filesystem path does NOT unblock a thread
    /// already parked inside `accept()` on Linux -- the listening fd stays valid and blocked
    /// regardless of whether its path still exists on disk; a real run of this exact pattern
    /// hung for hours before this was caught).
    hook_listener_handle: Option<JoinHandle<()>>,
    /// Set by `shutdown()` to ask the hook-listener thread to stop; polled by that thread at
    /// `HOOK_LISTENER_POLL_INTERVAL` granularity. See `hook_listener_handle`'s doc.
    hook_listener_stop: Arc<AtomicBool>,
    /// Live `agent-hook` socket connections awaiting a decision, keyed by `tool_use_id` --
    /// written to by the hook-listener thread, read/removed by `respond_permission`.
    pending_hook_connections: Arc<Mutex<std::collections::HashMap<String, PendingHookConnection>>>,
    socket_path: std::path::PathBuf,
    /// The child's own last few stderr lines, verbatim, newest last -- shared with the stderr
    /// thread so `poll_events` can stamp them onto `AgentEvent::ProcessExited` when the child dies.
    /// Before this existed, a child that died before opening (the exact shape of the 2026-09-18
    /// launcher-collision bug) surfaced only as the generic "provider process exited
    /// unexpectedly", even though the real reason was sitting in `AgentEvent::ProcessStderr`
    /// events this same process had already emitted and only `eprintln!`'d -- never reaching the
    /// panel, which has no way to correlate a past stderr line with a later exit.
    stderr_tail: Arc<Mutex<std::collections::VecDeque<String>>>,
    /// Set once this process's exit has been observed via `try_wait()` and a corresponding
    /// `AgentEvent::ProcessExited` has been pushed -- makes that push fire exactly once, no
    /// matter how many more times `poll_events()` is called afterward.
    exit_reported: bool,
    /// Set once `shutdown()` has run to completion -- makes `shutdown()` idempotent (a second
    /// call is a harmless no-op) and lets `Drop` tell whether it still has work to do.
    shut_down: bool,
}

/// Spawns the hook-relay listener thread: accepts one connection per `PreToolUse` firing, reads
/// the real stdin JSON `agent-hook` forwarded, parses it, stashes the live connection in
/// `pending` so `respond_permission` can write the answer back later, and emits a
/// `PermissionRequest` event over `tx`. Runs a non-blocking `accept()` poll loop with a stop flag
/// (rather than a blocking `for incoming in listener.incoming()` loop) so the returned stop flag
/// can make this thread return in bounded time even when nothing ever connects -- a blocking loop
/// here is a real, confirmed deadlock hazard, not a hypothetical one: removing a Unix socket's
/// filesystem path does NOT unblock a thread already parked inside `accept()` on Linux (the
/// listening fd stays valid and blocked regardless of whether its path still exists on disk), so
/// a real run of the earlier blocking-loop version hung for hours in `shutdown()`'s `join()` the
/// first time a real test exercised a turn that never triggered a tool call (the common case).
///
/// Also bounds the per-connection read via `HOOK_CONNECTION_READ_TIMEOUT`: on Linux a non-blocking
/// listener's accepted streams do NOT themselves come back non-blocking, and on macOS they do, so
/// each one is set blocking explicitly right after `accept()` (confirmed real on both, same
/// class of deadlock as the accept()-side one above, just triggered by "a connection starts but
/// never completes its line" instead of "nothing ever connects") -- without this, a real
/// `agent-hook` process killed/orphaned mid-write would park this thread in `read_line` forever.
///
/// **Fail-closed security posture (non-negotiable):** every way this loop can fail to relay a
/// request -- a read error or read timeout, an empty/EOF line, or a payload that doesn't parse --
/// writes an explicit `deny` decision back on that connection before dropping it. Silently
/// dropping the connection instead (what this loop originally did) is a real, confirmed *allow*:
/// `agent-hook` then exits 0 with empty stdout, its documented "no data" behavior, which the CLI
/// treats as "the hook expressed no `permissionDecision`" -- under `--permission-mode auto` the
/// tool call then proceeds on the CLI's own judgment, i.e. exactly the leak this whole hook
/// mechanism exists to close. A parent crash, a malformed payload, and a transient socket hiccup
/// must never be indistinguishable from a human's real approval. The deny write is best-effort
/// (errors ignored): in the "peer already gone" case there is nothing to tell, and attempting it
/// is harmless.
///
/// Split out from `spawn` as its own function (rather than inlined there) specifically so this
/// exact logic -- the fast-stop guarantee, the stalled-connection guarantee, the fail-closed deny
/// on every error branch, and the real PreToolUse-parsing happy path -- can be unit-tested
/// directly against a plain `UnixListener`, without spawning a real `claude` child process at all.
fn spawn_hook_listener(
    listener: UnixListener,
    tx: std::sync::mpsc::Sender<AgentEvent>,
    pending: Arc<Mutex<std::collections::HashMap<String, PendingHookConnection>>>,
) -> std::io::Result<(JoinHandle<()>, Arc<AtomicBool>)> {
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let handle = std::thread::spawn(move || loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let stream = match listener.accept() {
            Ok((stream, _addr)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(HOOK_LISTENER_POLL_INTERVAL);
                continue;
            }
            Err(_) => break, // listener itself is gone/broken -- nothing more to do
        };
        // Blocking first, explicitly. Linux clears `O_NONBLOCK` on an accepted fd; macOS copies
        // it from this non-blocking listener (rust-lang/rust#67027), and a non-blocking stream's
        // `read_line` returns `WouldBlock` whenever `agent-hook` has not finished writing yet --
        // which the branch below turns into a deny of a tool call nobody refused. A stream whose
        // mode cannot be set is denied here instead, for the same fail-closed reason.
        if let Err(e) = stream.set_nonblocking(false) {
            write_fail_closed_deny(&stream, &format!("could not make the relay connection blocking ({e})"));
            continue;
        }
        // See `HOOK_CONNECTION_READ_TIMEOUT`'s doc: without this, a connection that starts but
        // never completes its line would block this thread forever, never re-checking `stop`.
        let _ = stream.set_read_timeout(Some(HOOK_CONNECTION_READ_TIMEOUT));
        let mut reader = BufReader::new(stream.try_clone().expect("clone hook stream"));
        let mut line = String::new();
        // Every failure branch below denies explicitly rather than dropping the connection -- see
        // this function's own doc on why a dropped connection is a real allow, not a no-op.
        if let Err(e) = reader.read_line(&mut line) {
            write_fail_closed_deny(&stream, &format!("could not read the relayed request ({e})"));
            continue;
        }
        if line.trim().is_empty() {
            write_fail_closed_deny(&stream, "relayed request was empty (peer closed before sending it)");
            continue;
        }
        let Ok(parsed) = parse_pretooluse_input(line.trim()) else {
            write_fail_closed_deny(&stream, "relayed request could not be parsed as PreToolUse JSON");
            continue;
        };
        let request_id = parsed.tool_use_id.clone();
        pending
            .lock()
            .unwrap()
            .insert(request_id.clone(), PendingHookConnection { stream });
        let event = AgentEvent::PermissionRequest {
            request_id,
            // The same string as `request_id` above, filled in separately and on purpose. This
            // one is the identity of the gated call, taken straight from the `PreToolUse`
            // payload; `request_id` is the key this connection was just filed under, which
            // `respond_permission` answers by. They coincide because filing under the tool-use id
            // is what makes an answer routable.
            //
            // Being separate fields buys exactly one thing: neither can be silently redefined into
            // the other. It does NOT make the two decoupled -- `respond_permission` and
            // `write_hook_decision` both look this connection up by that same string, and nothing
            // in this crate pins that they agree. Changing the keying scheme is a change to all
            // three sites at once; it is not absorbed here.
            tool_use_id: Some(parsed.tool_use_id),
            tool_name: parsed.tool_name,
            input: parsed.tool_input,
            source: PermissionSource::HookRelay,
        };
        if tx.send(event).is_err() {
            break;
        }
    });
    Ok((handle, stop))
}

/// Runs the resolved binary once with the EXACT `--settings` value the real spawn is about to
/// pass, plus `--version` as a terminating flag, and requires exit 0 -- see `spawn_with_binary`'s
/// call site for why this exists and what it caught. Deliberately synchronous (`Command::output`)
/// rather than going through the same piped-stdio/background-thread machinery as the real spawn:
/// this process is expected to exit in well under a second and produce no interleaved-stream
/// hazard worth guarding against.
///
/// Returns the resolved-binary's own stderr (falling back to stdout, then to the bare exit
/// status) in the error, verbatim, un-summarized -- that text is the whole point: a generic
/// "preflight failed" would reproduce exactly the diagnostic gap this function exists to close.
///
/// **What this does NOT prove, and the claim must not be widened.** `--version` terminates before
/// the CLI reads the settings content, so a pass means only that the resolved binary ACCEPTS the
/// flag -- never that a `PreToolUse` hook sourced from it will actually run. That distinction is
/// not hypothetical: on 2026-09-18, `--managed-settings` and `CLAUDE_CODE_MANAGED_SETTINGS_PATH`
/// were both measured to exit 0, run a normal session, and fire no hook at all. If anything ever
/// answers `--settings` with exit 0 while routing it to a tier that does not execute hooks, this
/// preflight goes green and the session runs with no gate -- worse than the crash it replaces.
/// Only a real turn that observes a hook firing can close that gap; this closes the cheaper one.
///
/// A binary that cannot be spawned at all (does not exist, not executable) is passed through as
/// the underlying `io::Error` unchanged -- `spawn_with_binary`'s real `cmd.spawn()` a few lines
/// down would fail on it identically, so this just surfaces the same failure slightly earlier and
/// before anything else (a socket, a listener thread) has been created.
fn preflight_gate_flag_is_accepted(binary: &str, settings_json: &str, project_dir: &Path) -> std::io::Result<()> {
    let output = Command::new(binary)
        // The same working directory the real spawn sets, and this is load-bearing rather than
        // tidiness: a wrapper standing in for `claude` may decide whether to run FROM THE CWD --
        // this host's multi-account launcher derives a per-repository key from `pwd -P` and
        // refuses a directory name it cannot use. Measured 2026-09-18: identical argv exits 0
        // from one directory and 64 from another. Probing this process's own cwd while the real
        // spawn uses the project directory reproduces exactly the failure the preflight exists to
        // prevent -- green here, dead there -- and the inverse refuses a spawn that would work.
        .current_dir(project_dir)
        .arg("--settings")
        .arg(settings_json)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!(
            "exited with {:?} and produced no output on stdout or stderr",
            output.status
        )
    };
    Err(std::io::Error::other(format!(
        "the resolved `{binary}` binary refused the --settings flag this permission gate needs \
         (probed harmlessly with --version, before starting a real session or spending any \
         tokens): {detail}"
    )))
}

/// Resolves `binary` (as `Command::spawn` itself would via `execvp`) to the absolute path it
/// actually names, for the one-line diagnostic `spawn_with_binary` logs on every spawn attempt.
/// Nothing in this crate recorded this before 2026-09-18 -- when this host's `PATH` turned out to
/// point `claude` at a multi-account launcher rather than the real CLI, working that out took
/// four separate investigations because nothing anywhere said which absolute path had actually
/// been exec'd.
///
/// Best-effort and never fatal: `None` (rendered as a placeholder by the caller) means only that
/// this diagnostic could not resolve a path, never that the real spawn is expected to fail --
/// `cmd.spawn()` does its own, authoritative resolution independently of this.
fn resolve_binary_absolute_path(binary: &str) -> Option<std::path::PathBuf> {
    if binary.contains('/') {
        return Path::new(binary).canonicalize().ok();
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let Ok(meta) = candidate.metadata() else { continue };
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Some(candidate.canonicalize().unwrap_or(candidate));
            }
        }
        #[cfg(not(unix))]
        {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Writes an explicit `deny` back over a hook connection whose relay failed, so a broken relay can
/// never be mistaken for an approval -- see `spawn_hook_listener`'s fail-closed doc. Deliberately
/// best-effort and infallible from the caller's point of view: if the peer has already gone away
/// there is no one left to tell, and the connection is being dropped either way.
fn write_fail_closed_deny(stream: &UnixStream, reason: &str) {
    let decision = crate::hook_protocol::format_decision(false, Some(&format!("agent-hook relay failed: {reason}")));
    let mut sink = stream;
    let _ = writeln!(sink, "{decision}");
}

/// Releases every live `agent-hook` connection still awaiting a decision: writes a fail-closed
/// `deny` on each (see `write_fail_closed_deny`), then drops it. Dropping a stored `UnixStream`
/// closes that socket, which the blocked `agent-hook` process on the other end observes as an
/// immediate EOF -- letting it exit instead of sitting there for the full 600s hook timeout
/// waiting for an answer that is never coming.
///
/// Called by `shutdown()`, which is exactly when "never coming" becomes true: the whole public API
/// for answering a request lives on the object being shut down, and the `claude` process that
/// spawned those hooks is about to be (or already has been) killed, so every pending request is
/// permanently unanswerable and its helper process would otherwise be orphaned onto init, still
/// blocked. The explicit deny (rather than only closing the socket) matters for the narrow window
/// where `claude` is still alive during shutdown's own grace period: a bare EOF makes `agent-hook`
/// print nothing, which the CLI reads as "no decision" and may act on -- the same silent-allow
/// hazard `spawn_hook_listener`'s doc describes. Split out as a free function taking `pending`
/// directly so this behavior can be tested against a real, genuinely-blocked peer without a live
/// `AgentProcess`.
fn release_pending_hook_connections(pending: &Arc<Mutex<std::collections::HashMap<String, PendingHookConnection>>>) {
    let mut pending = pending.lock().unwrap();
    for (_request_id, conn) in pending.drain() {
        write_fail_closed_deny(&conn.stream, "the conversation shut down before this was answered");
    }
}

/// Writes a permission decision back over a pending `HookRelay` connection's live socket, keyed
/// by `request_id`, and removes it from `pending` -- the real logic behind
/// `AgentProcess::respond_permission`'s `HookRelay` branch. Split out as a free function (taking
/// `pending` directly rather than `&mut self`) so it can be unit-tested against a hand-built
/// `pending` map and a real `UnixStream::pair()`, without needing a live `AgentProcess`/child
/// process. A request id with no matching pending connection (already answered, or never a real
/// `HookRelay` request) is a harmless no-op, not an error -- mirrors the same reasoning as
/// `AgentSessionProjection::apply`'s `ToolResult`-for-unknown-id case elsewhere in this crate.
fn write_hook_decision(
    pending: &Arc<Mutex<std::collections::HashMap<String, PendingHookConnection>>>,
    request_id: &str,
    allow: bool,
    reason: Option<&str>,
) -> std::io::Result<()> {
    let mut pending = pending.lock().unwrap();
    // Write BEFORE removing: a `writeln!` failure must leave the entry in place so a retried
    // `respond_permission` call can genuinely try again, rather than silently no-op'ing against
    // an already-vanished entry (the "unknown id" branch below returns `Ok(())`, which would
    // otherwise make a failed delivery look identical to a successful one to the caller).
    if let Some(conn) = pending.get_mut(request_id) {
        let decision = crate::hook_protocol::format_decision(allow, reason);
        writeln!(conn.stream, "{decision}")?;
    } else {
        return Ok(());
    }
    pending.remove(request_id);
    Ok(())
}

/// Builds the full `control_response` JSON payload `AgentProcess::respond_permission`'s
/// `CanUseTool` branch writes to the CLI's stdin. Split out as a pure function (no `self`, no
/// I/O) so its exact shape can be unit-tested directly, independent of `respond_permission`'s
/// stdin-writing side (which needs a live `ChildStdin` and is otherwise already covered by
/// `send_turn`/`interrupt`'s identical write-then-flush pattern).
fn build_can_use_tool_response_payload(request_id: &str, allow: bool, reason: Option<&str>) -> serde_json::Value {
    let behavior = if allow { "allow" } else { "deny" };
    let mut response = serde_json::json!({ "behavior": behavior });
    if !allow {
        if let Some(r) = reason {
            response["message"] = serde_json::json!(r);
        }
    }
    serde_json::json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": response },
    })
}

impl AgentProcess {
    /// Spawns exactly one long-lived process for the whole conversation. `project_dir` is both
    /// the CLI's cwd (so `--setting-sources project,local` resolves the project's own real
    /// settings there) and the root this conversation operates on. This crate writes nothing into
    /// it: the hook configuration goes in argv and the per-conversation socket in the temp dir.
    pub fn spawn(project_dir: &Path, mode: PermissionMode, disallowed_tools: &[&str]) -> std::io::Result<Self> {
        Self::spawn_with_binary(project_dir, mode, disallowed_tools, "claude")
    }

    /// The real body of [`spawn`](Self::spawn), parameterized only by which binary to exec.
    /// `spawn` always passes `"claude"`; the parameter exists so this module's own tests can make
    /// the real `cmd.spawn()` call genuinely fail (by naming a binary that does not exist) and
    /// assert on this function's real partial-failure cleanup, rather than only reasoning about it.
    ///
    /// **Ordering here is load-bearing, not incidental** (a real leak a final review caught):
    /// the hook-listener *thread* is spawned only AFTER `cmd.spawn()` has succeeded. It used to be
    /// spawned before, which meant a failed `cmd.spawn()` (e.g. `claude` not on `PATH`) returned
    /// `Err` while leaving behind a thread whose stop flag no one could ever set again -- it spun
    /// its poll-plus-sleep loop for the rest of the host process's life, with the socket file
    /// left on disk too. The listener is still *bound* early (cheap, and the socket must be ready
    /// before the CLI could possibly invoke a hook), but every fallible step after it now cleans
    /// up what came before.
    ///
    /// The hook configuration itself is an argv value (`--settings`), not a file, so there is no
    /// longer anything on disk to generate first or to restore afterwards -- see
    /// `crate::settings` for the real-CLI spike that forced that change.
    fn spawn_with_binary(
        project_dir: &Path,
        mode: PermissionMode,
        disallowed_tools: &[&str],
        binary: &str,
    ) -> std::io::Result<Self> {
        let conversation_id = Uuid::new_v4();
        // A configured account (`init.lua`'s `agent.account`) governs the SIDECAR child and where
        // this crate reads transcripts -- not this spawn. The `claude` on `PATH` here is, on the
        // host this was written for, a launcher that accepts only a complete, exactly-matching
        // four-variable tuple AND requires an approved `tmux` server for every non-test role; a
        // GTK window started from a desktop entry has neither, so writing the tuple on here would
        // turn a working spawn into `claude-wrapper: an approved tmux server is required`. So this
        // child keeps inheriting the environment the window was launched from -- and says so,
        // because the alternative is two halves of one product quietly spending two accounts.
        if let Some(account) = crate::account::configured() {
            eprintln!(
                "[account] the legacy backend spawns '{binary}' with this window's own environment,                  NOT the configured account '{}' -- transcripts are read from {} while this child                  writes wherever its inherited CLAUDE_CONFIG_DIR points",
                account.name(),
                account.config_dir().display()
            );
        }
        let socket_path = crate::socket_path::hook_socket(&std::env::temp_dir(), conversation_id)?;
        let _ = std::fs::remove_file(&socket_path); // stale leftover from a prior crash, if any

        // Bypass mode asks for no gate at all, so no hook is installed. In every other mode the
        // gate travels in this one process's argv, where no other conversation can reach it.
        let hook_settings_arg = if mode == PermissionMode::Bypass {
            None
        } else {
            Some(crate::settings::hook_settings_arg(&socket_path)?)
        };

        // Preflight: in every mode that installs a gate, confirm the resolved binary actually
        // accepts `--settings` before doing anything else -- binding the socket, spawning the
        // real long-lived process, any of it. This is the fix for a real, reproduced failure
        // (2026-09-18, work, production binary): a multi-account launcher on this host's
        // `PATH` refuses `--settings` outright (`claude-wrapper: production launcher owns
        // --settings for autoMemoryDirectory`, exit 64) because ITS OWN wrapper needs that flag
        // for something else and treats a caller's use of it as a collision, not an override.
        // The real spawn then died before ever emitting `system`/`init`, and the panel had
        // nothing more specific to show than "the provider process exited unexpectedly" --
        // which is exactly the generic text this whole check exists to avoid reaching.
        //
        // `--version` was chosen, and verified live against that exact launcher, specifically
        // because it is the cheapest terminating flag known to open no session and spend no
        // tokens: the CLI prints its version and exits before doing anything else, so this is
        // safe to run unconditionally rather than only when something looks wrong.
        if let Some(settings) = &hook_settings_arg {
            preflight_gate_flag_is_accepted(binary, settings, project_dir)?;
        }

        // Undoes everything `spawn` has created so far, for use on the error path of every
        // fallible step below -- no thread has been started at any point where this is called, so
        // there is never anything to stop, and now only one file to remove.
        let cleanup_partial_spawn = || {
            let _ = std::fs::remove_file(&socket_path);
        };

        let pending_hook_connections = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (tx, rx) = std::sync::mpsc::channel();

        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(e) => {
                cleanup_partial_spawn();
                return Err(e);
            }
        };

        let mut cmd = Command::new(binary);
        cmd.current_dir(project_dir)
            .arg("--print")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--setting-sources")
            .arg("project,local")
            .arg("--permission-mode")
            .arg(mode.as_cli_flag())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if !disallowed_tools.is_empty() {
            cmd.arg("--disallowedTools").arg(disallowed_tools.join(","));
        }

        if let Some(settings) = &hook_settings_arg {
            cmd.arg("--settings").arg(settings);
        }

        // Logged once per spawn attempt, unconditionally -- see `resolve_binary_absolute_path`'s
        // doc for why nothing recorded this before 2026-09-18. Deliberately placed after every
        // `cmd.arg(...)` call above and before `cmd.spawn()`, so it shows exactly the argv the
        // real spawn is about to attempt (including `--settings`'s value), not a reconstruction of
        // it -- and BEFORE the outcome is known, so a spawn that then hangs or dies is not the only
        // way this line reaches stderr.
        let resolved_path = resolve_binary_absolute_path(binary)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("<could not resolve {binary:?} on PATH>"));
        let argv: Vec<String> = std::iter::once(binary.to_string())
            .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect();
        eprintln!("[agent] spawning resolved binary {resolved_path} -- argv: {argv:?}");

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                // No listener thread exists yet (see this function's ordering doc), so the bound
                // listener just drops here -- only the two files it created need removing.
                drop(listener);
                cleanup_partial_spawn();
                return Err(e);
            }
        };

        // Only now that a real child exists is the hook-listener thread started: from here on
        // there is genuinely something that can stop it again (`shutdown()`, via the stop flag on
        // the returned `Self`).
        let (hook_listener_handle, hook_listener_stop) =
            match spawn_hook_listener(listener, tx.clone(), pending_hook_connections.clone()) {
                Ok(started) => started,
                Err(e) => {
                    // The child is already running and nothing owns it yet -- kill and reap it
                    // here rather than returning an Err that orphans it.
                    let _ = child.kill();
                    let _ = child.wait();
                    cleanup_partial_spawn();
                    return Err(e);
                }
            };

        let stdin = child.stdin.take().expect("piped stdin must be present");
        let stdout = child.stdout.take().expect("piped stdout must be present");
        let stderr = child.stderr.take().expect("piped stderr must be present");

        let reader_tx = tx.clone();
        let reader_handle = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                for event in crate::wire::translate_line(&line) {
                    if reader_tx.send(event).is_err() {
                        return;
                    }
                }
            }
        });

        let stderr_tx = tx;
        let stderr_tail = Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(
            STDERR_TAIL_CAPACITY,
        )));
        let stderr_tail_writer = stderr_tail.clone();
        let stderr_handle = std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                {
                    let mut tail = stderr_tail_writer.lock().unwrap();
                    if tail.len() == STDERR_TAIL_CAPACITY {
                        tail.pop_front();
                    }
                    tail.push_back(line.clone());
                }
                if stderr_tx.send(AgentEvent::ProcessStderr { line }).is_err() {
                    return;
                }
            }
        });

        Ok(Self {
            child,
            stdin: Some(stdin),
            events_rx: rx,
            reader_handle: Some(reader_handle),
            stderr_handle: Some(stderr_handle),
            hook_listener_handle: Some(hook_listener_handle),
            hook_listener_stop,
            pending_hook_connections,
            socket_path,
            stderr_tail,
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
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if !self.exit_reported {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exit_reported = true;
                // Snapshotted here, at the moment the exit is observed, rather than read later
                // out of a stale reference -- by the time a caller reacts to this event, the
                // stderr thread may have already exited too (having hit EOF right alongside the
                // child), so the tail must travel with the event itself.
                let stderr_tail: Vec<String> = self.stderr_tail.lock().unwrap().iter().cloned().collect();
                events.push(AgentEvent::ProcessExited {
                    success: status.success(),
                    stderr_tail,
                });
            }
        }
        events
    }

    /// Writes one user turn to stdin. Caller's responsibility to only call this once the prior
    /// turn's `result` has arrived (or before the first turn) -- see this plan's spec on why
    /// mid-turn stdin queuing is not relied upon.
    pub fn send_turn(&mut self, text: &str) -> std::io::Result<()> {
        let payload = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text },
            "parent_tool_use_id": serde_json::Value::Null,
        });
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdin already closed (process shut down)",
            )
        })?;
        writeln!(stdin, "{payload}")?;
        stdin.flush()
    }

    /// Sends a real interrupt control_request. Returns the generated request_id so the caller
    /// can recognize the matching `AgentEvent::ControlResponse` when it arrives -- `agent` does
    /// not correlate this internally (see the spec's "No internal request/response correlation").
    ///
    /// Also releases every pending hook connection (see `release_pending_hook_connections`),
    /// regardless of whether the stdin write itself succeeds -- this is the real fix for a real
    /// stall found via `agent-ui`'s own Task 8 sandbox verification (see
    /// `shell/MANUAL_VERIFICATION.md`'s "agent-ui verification" section): if a `PreToolUse`
    /// permission request was still pending at the moment `interrupt()` was called, the CLI is
    /// parked inside the synchronous hook subprocess call and cannot service a buffered interrupt
    /// control_request on its own stdin, so the turn (and any subsequent interrupt) would
    /// otherwise stall for the CLI's full 600s hook timeout. A request from the turn being
    /// interrupted can never be genuinely answered either way once the caller has asked to stop,
    /// so releasing it here mirrors `shutdown()`'s identical reasoning exactly.
    pub fn interrupt(&mut self) -> std::io::Result<Uuid> {
        let request_id = Uuid::new_v4();
        let payload = serde_json::json!({
            "type": "control_request",
            "request_id": request_id.to_string(),
            "request": { "subtype": "interrupt", "cancel_queued": true },
        });
        let write_result = (|| -> std::io::Result<()> {
            let stdin = self.stdin.as_mut().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "stdin already closed (process shut down)",
                )
            })?;
            writeln!(stdin, "{payload}")?;
            stdin.flush()
        })();

        // Release every pending hook connection regardless of whether the interrupt write itself
        // succeeded: a permission request from the turn being interrupted can never be genuinely
        // answered either way once the caller has asked to stop, and leaving `agent-hook` blocked
        // in `read_line` for the CLI's full 600s hook timeout is exactly the stall this closes --
        // found via agent-ui's real Task 8 sandbox verification (see
        // shell/MANUAL_VERIFICATION.md's "agent-ui verification" section). Mirrors shutdown()'s
        // identical reasoning exactly.
        release_pending_hook_connections(&self.pending_hook_connections);

        write_result.map(|()| request_id)
    }

    /// Answers a pending `PermissionRequest`, routing based on `source`. For `HookRelay`, writes
    /// the decision back over that request's live `agent-hook` socket connection (removing it
    /// from `pending_hook_connections`) -- this is the confirmed-real, primary mechanism. For
    /// `CanUseTool`, writes a `control_response` to the CLI's own stdin instead.
    pub fn respond_permission(
        &mut self,
        request_id: &str,
        source: PermissionSource,
        allow: bool,
        reason: Option<&str>,
    ) -> std::io::Result<()> {
        match source {
            PermissionSource::HookRelay => {
                write_hook_decision(&self.pending_hook_connections, request_id, allow, reason)
            }
            PermissionSource::CanUseTool => {
                let payload = build_can_use_tool_response_payload(request_id, allow, reason);
                let stdin = self.stdin.as_mut().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "stdin already closed (process shut down)",
                    )
                })?;
                writeln!(stdin, "{payload}")?;
                stdin.flush()
            }
        }
    }

    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// The child process's OS pid. Exists to support orphan-safety verification (confirming no
    /// process remains alive after `shutdown()`) -- not otherwise used by this crate's own logic.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Graceful-then-escalating shutdown. New first step vs v1: close stdin for real (drop the
    /// `ChildStdin`, which closes the write end and sends EOF to the child) before anything else
    /// -- both official Claude Code SDKs converge on this exact ordering to ask a long-lived
    /// `claude --print --input-format stream-json` process to wrap up on its own. Also signals
    /// the hook-listener thread to stop right away (see `hook_listener_handle`'s doc for why this
    /// must be a signal it can notice on its own, not a blocking-`accept()` hope) -- this happens
    /// up front, in parallel with the grace-period waits below, so that thread has the most
    /// possible time to actually notice and exit before this function ever blocks on joining it.
    /// Also releases every `agent-hook` connection still awaiting a decision (see
    /// `release_pending_hook_connections`): those requests can never be answered once this object
    /// is shutting down, so their helper processes are handed a fail-closed deny and an EOF to
    /// exit on, instead of being left blocked, orphaned onto init, for the CLI's full 600s hook
    /// timeout.
    ///
    /// Everything else is v1's already-proven chain, unchanged: wait for natural exit, escalate
    /// to SIGTERM, escalate to SIGKILL, block until confirmed reaped, join reader threads, join
    /// the (by now already-signaled) hook-listener thread. There is no settings file to restore
    /// any more -- the gate lives in the spawned process's argv and dies with it.
    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;

        // Dropping the ChildStdin here (not just letting it drop implicitly at end-of-struct-life)
        // closes the write end and sends real EOF to the child right now, before the grace-period
        // wait below -- this is the actual mechanism both SDKs rely on for a graceful stop.
        self.stdin.take();

        // Ask the hook-listener thread to stop now (it'll notice within one
        // `HOOK_LISTENER_POLL_INTERVAL`) and remove the socket file so no new `agent-hook`
        // connection can arrive during shutdown. Neither of these depends on the child process,
        // so doing this before the grace-period waits below lets the thread wind down
        // concurrently with them instead of adding its own separate wait afterward.
        self.hook_listener_stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.socket_path);

        // Release every `agent-hook` process still blocked waiting for a decision. Their requests
        // are permanently unanswerable from this point (the whole public API for answering one
        // lives on this object, which is shutting down, and the `claude` process that spawned them
        // is about to die), so leaving them connected would strand each helper process -- blocked
        // in `read_line`, reparented to init, for the full 600s hook timeout. Each gets an
        // explicit fail-closed deny and then an EOF to exit on.
        release_pending_hook_connections(&self.pending_hook_connections);

        let natural_deadline = Instant::now() + NATURAL_EXIT_GRACE_PERIOD;
        while Instant::now() < natural_deadline && !self.has_exited() {
            std::thread::sleep(NATURAL_EXIT_POLL_INTERVAL);
        }

        if !self.has_exited() {
            let _ = self.send_signal(SIGTERM);
            let deadline = Instant::now() + GRACE_PERIOD;
            while Instant::now() < deadline && !self.has_exited() {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !self.has_exited() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();

        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.hook_listener_handle.take() {
            let _ = handle.join();
        }
        // Once more, now that the listener thread is provably finished: it could have accepted and
        // stashed one final connection in the narrow window between the drain above and its own
        // next stop-flag check, and that peer deserves the same EOF rather than a 600s wait.
        release_pending_hook_connections(&self.pending_hook_connections);
    }

    fn send_signal(&self, sig: i32) -> std::io::Result<()> {
        let pid = self.child.id() as i32;
        let ret = unsafe { libc_kill(pid, sig) };
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

const SIGTERM: i32 = 15;

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

#[cfg(test)]
mod tests {

    /// **Superseded by the owner's ruling of 2026-09-20, and kept as this paragraph because its
    /// reasoning is the argument against the ruling when the permission design is revisited.**
    /// It read: the two modes must not get the same list, and the test says why rather than just
    /// that. `Bypass` installs no `PreToolUse` gate at all, so an `Edit` there is a file rewritten
    /// under a live buffer with no card and no decision. Collapsing these two lists into one -- in
    /// either direction -- is the change this asserts against: widening `Bypass` ships that hole,
    /// and narrowing `Auto` back makes the permission card unreachable for the one thing the
    /// product's own MVP sentence is about.
    ///
    /// The half of that which survives is `Auto`'s, and it is still asserted below. What the old
    /// assertion could not see is that the mode labelled "bypass all permissions" was the ONLY mode
    /// unable to edit a file -- strictly less capable than the mode with a gate. The hole it names
    /// is real and is now open; `disallowed_tools_for`'s own doc carries the ruling, its stated
    /// expiry, and the better answer (auto-answer under a gate, the shape ACP uses).
    ///
    /// This test now pins the ruling, so restoring the old list fails here and has to argue with
    /// the paragraph above rather than around it.
    ///
    /// **Superseded again, 2026-09-25, for `Auto`'s half.** It asserted `Auto` still denied `Bash`
    /// ("its worst case is not a file, and un-denying it there is a separate decision that owes its
    /// own evidence"). The owner made that decision -- "auto模式给claude，和claude本身的做法一致" --
    /// and the evidence is the gate itself: every `Bash` call reaches the `PreToolUse` hook (matcher
    /// `*`), `permission_policy` answers only the read-only-inside-the-project ones, and the rest are
    /// cards. That is Claude Code's own `default` mode. With `Bash` removed, phase 3's prefix rules
    /// and the card's "always allow" had nothing on the real CLI to answer.
    #[test]
    fn auto_offers_every_tool_because_the_gate_covers_them_and_bypass_denies_nothing() {
        let auto = disallowed_tools_for(PermissionMode::Auto);
        let bypass = disallowed_tools_for(PermissionMode::Bypass);
        for tool in ["Bash", "Edit", "Write", "NotebookEdit"] {
            assert!(
                !auto.contains(&tool),
                "Auto must offer {tool}: the PreToolUse hook gates every call (owner's ruling, 2026-09-25)"
            );
            assert!(
                !bypass.contains(&tool),
                "Bypass must permit {tool}: a mode named after having every permission cannot be \
                 the one mode that may not edit (owner's ruling, 2026-09-20)"
            );
        }
        // Both empty, and that is the whole of both lists: an empty list is what makes the legacy
        // spawn omit `--disallowedTools` and the sidecar request state `unrestricted`.
        assert!(auto.is_empty(), "Auto denies nothing; the gate is the boundary");
        assert!(
            bypass.is_empty(),
            "Bypass denies nothing; an empty list is also what makes the sidecar request state \
             `unrestricted`, without which Verdandi re-applies its own identical floor"
        );
        // What still separates the two modes is the gate, not the list: Auto installs the hook and
        // Bypass installs none (`spawn_with_binary`). The list no longer carries that difference.
    }

    /// Pins the exact contents of `CONSERVATIVE_DISALLOWED_TOOLS`, because a typo in it is silent
    /// on BOTH sides of a repository boundary.
    ///
    /// Verdandi's sidecar applies a conservative deny floor of its own when a session states no
    /// tool policy — but **any** stated restriction takes that floor off, including one that names
    /// no real tool. The sidecar cannot distinguish `deny: ["bash"]` (a typo) from a deliberate
    /// statement: it owns no tool namespace, and a hardcoded list of real tool names on its side
    /// would rot. So it is named as a known hole in its own proto comment rather than fixed there.
    ///
    /// This list is the statement neovibe sends. Lowercasing one entry here would disarm the floor
    /// over there, install no hook, and emit no notice — nothing anywhere would say so. This repo
    /// owns this list, so this is the one place the hole can be closed: an edit has to be
    /// deliberate enough to update an assertion that spells the consequence out.
    ///
    /// If you are legitimately changing the list, change it here too and say why in the commit.
    #[test]
    fn the_conservative_tool_denylist_is_pinned_exactly_because_a_typo_is_silent_everywhere() {
        assert_eq!(
            CONSERVATIVE_DISALLOWED_TOOLS,
            &["Bash", "Write", "Edit", "NotebookEdit"],
            "changing this list changes what the sidecar's own conservative floor is replaced by"
        );
        for name in CONSERVATIVE_DISALLOWED_TOOLS {
            let mut chars = name.chars();
            let first = chars.next().expect("a tool name is never empty");
            assert!(
                first.is_ascii_uppercase() && chars.all(|c| c.is_ascii_alphanumeric()),
                "{name:?} is not shaped like a built-in tool name -- built-ins are CamelCase ASCII, \
                 and a name the provider does not recognize is accepted verbatim, restricting \
                 nothing while disarming the default floor"
            );
        }
    }
    use super::*;

    /// Real end-to-end: spawns one long-lived process, sends two separate turns over the same
    /// process (no `--resume`, no respawn), and confirms the second turn's answer genuinely
    /// recalls the first -- proving context continuity comes from the process staying alive, not
    /// from any session-id/resume mechanism (which no longer exists on `AgentProcess` at all).
    #[test]
    #[ignore]
    fn real_two_turns_in_one_process_no_resume() {
        let dir = std::env::temp_dir().join(format!("agent-process-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut process = AgentProcess::spawn(&dir, PermissionMode::Auto, CONSERVATIVE_DISALLOWED_TOOLS).unwrap();

        process.send_turn("reply with exactly the word: pong").unwrap();
        let result1 = drain_until_turn_finished(&mut process);
        assert!(result1.to_lowercase().contains("pong"));

        process.send_turn("what word did you just say?").unwrap();
        let result2 = drain_until_turn_finished(&mut process);
        assert!(
            result2.to_lowercase().contains("pong"),
            "same process must recall turn 1 with no --resume: {result2}"
        );

        // Neither turn above ever triggers a real tool call (CONSERVATIVE_DISALLOWED_TOOLS blocks
        // the tool-using tools, and both prompts are plain text anyway), so the hook socket never
        // received a single connection -- exactly the case that used to hang `shutdown()` forever
        // (see `spawn_hook_listener`'s doc). Assert real, bounded shutdown latency, not just that
        // it eventually returns (which a suite-level timeout would also "catch", six hours later).
        let shutdown_started = Instant::now();
        process.shutdown();
        assert!(
            shutdown_started.elapsed() < Duration::from_secs(5),
            "shutdown() must return in bounded time even when nothing ever connected to the hook socket, took {:?}",
            shutdown_started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real end-to-end: sends a turn deliberately designed to run long, interrupts it mid-flight,
    /// then shuts down and confirms via `/proc/<pid>` (not just this crate's own bookkeeping) that
    /// no orphaned `claude` process is left behind. Exercises the interrupt control_request path
    /// and the shutdown-while-still-alive path together, since that combination is exactly the
    /// real-world case (a user cancels a long-running turn, then closes the session).
    #[test]
    #[ignore]
    fn shutdown_after_interrupt_leaves_no_orphan() {
        let dir = std::env::temp_dir().join(format!("agent-process-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut process = AgentProcess::spawn(&dir, PermissionMode::Auto, CONSERVATIVE_DISALLOWED_TOOLS).unwrap();
        let pid = process.pid();

        process
            .send_turn("write a very long story, at least 2000 words, about a journey")
            .unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        process.interrupt().unwrap();
        std::thread::sleep(Duration::from_millis(1500));

        let shutdown_started = Instant::now();
        process.shutdown();
        assert!(
            shutdown_started.elapsed() < Duration::from_secs(5),
            "shutdown() must return in bounded time, took {:?}",
            shutdown_started.elapsed()
        );
        assert!(
            !crate::process_probe::pid_is_alive(pid),
            "pid {pid} outlived shutdown()"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn drain_until_turn_finished(process: &mut AgentProcess) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            for event in process.poll_events() {
                if let AgentEvent::TurnFinished { result_text, .. } = event {
                    return result_text;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("no TurnFinished within 60s");
    }

    fn temp_socket_path() -> std::path::PathBuf {
        crate::socket_path::in_dir(
            &std::env::temp_dir(),
            &format!("nv-listener-{}.sock", uuid::Uuid::new_v4().simple()),
        )
        .expect("the listener tests' socket path must fit the macOS limit")
    }

    /// The regression test for the real deadlock this task found: no real `claude` process
    /// involved at all -- binds a plain `UnixListener` nothing will ever connect to (exactly the
    /// common case: a turn that never triggers a real tool call), spawns the hook-listener thread
    /// on it directly via `spawn_hook_listener`, then confirms setting the stop flag makes the
    /// thread actually finish within a bounded, short time. Before the fix (a blocking `for
    /// incoming in listener.incoming()` loop "unblocked" by removing the socket file, which does
    /// NOT work on Linux), this exact scenario is what hung a real `#[ignore]`d test for hours.
    #[test]
    fn hook_listener_stops_quickly_when_nothing_ever_connects() {
        let socket_path = temp_socket_path();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (handle, stop) = spawn_hook_listener(listener, tx, pending).unwrap();

        // Give the thread a moment to actually enter its poll loop before asking it to stop.
        std::thread::sleep(Duration::from_millis(50));
        let stop_requested = Instant::now();
        stop.store(true, Ordering::Relaxed);

        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle.is_finished(),
            "hook-listener thread did not stop within 2s of the stop flag being set"
        );
        handle.join().unwrap();
        assert!(
            stop_requested.elapsed() < Duration::from_secs(2),
            "hook-listener thread took {:?} to stop -- should be ~HOOK_LISTENER_POLL_INTERVAL",
            stop_requested.elapsed()
        );
        let _ = std::fs::remove_file(&socket_path);
    }

    /// The regression test for the second, narrower deadlock a review found in the same code
    /// path: `listener.set_nonblocking(true)` only affects the *listening* socket, not sockets it
    /// accepts -- an accepted `UnixStream` comes back blocking by default (confirmed real via a
    /// standalone `fcntl(fd, F_GETFL)` check), so without `HOOK_CONNECTION_READ_TIMEOUT`, a
    /// connection that starts but never completes its line (a real `agent-hook` process
    /// killed/orphaned mid-write) would park this thread in `read_line` forever, never
    /// re-checking `stop` -- functionally the same class of hang as the accept()-side one above,
    /// just triggered by "a connection starts but never finishes" instead of "nothing connects."
    /// Connects a real client, writes a partial line with no trailing newline, then never sends
    /// anything else (holding the connection open, simulating exactly that failure mode), and
    /// confirms the listener thread still stops promptly once asked to.
    #[test]
    fn hook_listener_does_not_hang_when_a_connection_stalls_without_completing_its_line() {
        let socket_path = temp_socket_path();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (handle, stop) = spawn_hook_listener(listener, tx, pending).unwrap();

        // Connect, write a partial line (no trailing newline) and never send more -- the
        // connection is kept alive (not dropped) for the rest of the test so the listener thread
        // genuinely has to give up on the read via its timeout, not just see a clean EOF.
        let mut stalled_client = UnixStream::connect(&socket_path).unwrap();
        write!(stalled_client, "{{\"incomplete").unwrap();

        std::thread::sleep(Duration::from_millis(50));
        let stop_requested = Instant::now();
        stop.store(true, Ordering::Relaxed);

        let deadline = Instant::now() + Duration::from_secs(3);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle.is_finished(),
            "hook-listener thread hung on a connection that connected but never completed its line"
        );
        handle.join().unwrap();
        assert!(
            stop_requested.elapsed() < Duration::from_secs(3),
            "hook-listener thread took {:?} to stop after a stalled connection -- should be bounded by HOOK_CONNECTION_READ_TIMEOUT",
            stop_requested.elapsed()
        );

        drop(stalled_client);
        let _ = std::fs::remove_file(&socket_path);
    }

    /// The macOS regression test for the accepted stream's blocking mode (M1, 2026-09-17).
    ///
    /// Linux gives `accept()`'s new fd a clear `O_NONBLOCK` whatever the listener has; XNU copies
    /// the listener's (rust-lang/rust#67027). This listener is non-blocking, so on macOS the
    /// accepted stream was too, and a `read_line` that ran before `agent-hook` had written its
    /// line returned `WouldBlock` -- which the loop treats as a failed read and answers with a
    /// fail-closed deny. The gate refused a real tool call nobody had refused.
    ///
    /// Every other relay test here writes immediately after connecting, which is why none of them
    /// caught it. This one waits several poll intervals first, so the listener has certainly
    /// accepted and reached `read_line` with nothing to read, and then writes a payload larger
    /// than 16 KiB, so the line cannot arrive in one read either (macOS's AF_UNIX stream buffer is
    /// 8 KiB). It must come out as a `PermissionRequest`, not a deny.
    #[test]
    fn a_relayed_request_written_after_a_poll_interval_and_larger_than_16_kib_is_not_denied() {
        let fixture = std::fs::read_to_string(format!(
            "{}/tests/fixtures/v2_hook_pretooluse_stdin.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let mut payload: serde_json::Value = serde_json::from_str(fixture.trim()).unwrap();
        payload["tool_input"]["command"] = serde_json::Value::String(format!("echo {}", "x".repeat(32 * 1024)));
        let line = serde_json::to_string(&payload).unwrap();
        assert!(line.len() > 16 * 1024);

        let socket_path = temp_socket_path();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (handle, stop) = spawn_hook_listener(listener, tx, pending.clone()).unwrap();

        let client = UnixStream::connect(&socket_path).unwrap();
        std::thread::sleep(HOOK_LISTENER_POLL_INTERVAL * 5);
        // From another thread: 32 KiB does not fit the socket buffer, so this write blocks until
        // the listener reads, and a listener that has already given up would leave it blocked.
        let mut writer = client.try_clone().unwrap();
        let write = std::thread::spawn(move || writeln!(writer, "{line}"));

        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(AgentEvent::PermissionRequest { request_id, source, .. }) => {
                assert_eq!(request_id, "toolu_01CtdezhmhUCrBaswxW5HYmC");
                assert_eq!(source, PermissionSource::HookRelay);
            }
            Ok(other) => panic!("expected PermissionRequest, got {other:?}"),
            Err(_) => {
                // Best-effort: macOS refuses setsockopt with EINVAL once the peer has shut down.
                let _ = client.set_read_timeout(Some(Duration::from_millis(200)));
                let mut reply = String::new();
                let _ = BufReader::new(&client).read_line(&mut reply);
                panic!("no PermissionRequest within 2s; the listener answered {reply:?}");
            }
        }
        write
            .join()
            .unwrap()
            .expect("the whole payload should have been written");
        assert!(pending.lock().unwrap().contains_key("toolu_01CtdezhmhUCrBaswxW5HYmC"));

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        let _ = std::fs::remove_file(&socket_path);
    }

    /// The listener's happy path, exercised without a real `claude` process: a real client
    /// connects over a real `UnixStream` and writes the exact real captured `PreToolUse` stdin
    /// JSON (Task 1's fixture) that `agent-hook` would relay verbatim -- confirms the thread
    /// parses it and emits a matching `AgentEvent::PermissionRequest` with `PermissionSource::
    /// HookRelay`, that the connection is stashed in `pending` under the real `tool_use_id`
    /// (which `respond_permission`/`write_hook_decision` depend on to find it again), and that
    /// the event carries that same real id in its own `tool_use_id` field -- the link a
    /// permission card needs to name the exact call it gates, which this backend used to drop.
    ///
    /// `request_id` and `tool_use_id` are asserted separately although they hold the same string
    /// today: the first is the key this connection was filed under, the second is the identity of
    /// the gated call, and a future change to either must not be able to silently redefine the
    /// other.
    ///
    /// The test then keeps going, one layer past this module, through
    /// `session::translate_wire_event` -- the whole point being that this is the ONE test in the
    /// crate where the id is never typed out by the test itself between the fixture and the
    /// domain event. It comes off disk, crosses a real `UnixStream`, is parsed, translated, and
    /// only then compared. Every earlier version of this coverage stopped at `AgentEvent` and the
    /// translation step was hand-fed on the other side, which is how a hardcoded `None` in that
    /// step survived a green workspace.
    #[test]
    fn hook_listener_relays_a_real_pretooluse_connection_into_a_permission_request_event() {
        let fixture = std::fs::read_to_string(format!(
            "{}/tests/fixtures/v2_hook_pretooluse_stdin.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();

        let socket_path = temp_socket_path();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (handle, stop) = spawn_hook_listener(listener, tx, pending.clone()).unwrap();

        let mut client = UnixStream::connect(&socket_path).unwrap();
        writeln!(client, "{}", fixture.trim()).unwrap();

        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("no PermissionRequest event within 2s");
        match &event {
            AgentEvent::PermissionRequest {
                request_id,
                tool_use_id,
                tool_name,
                source,
                ..
            } => {
                assert_eq!(request_id, "toolu_01CtdezhmhUCrBaswxW5HYmC");
                assert_eq!(
                    tool_use_id.as_deref(),
                    Some("toolu_01CtdezhmhUCrBaswxW5HYmC"),
                    "the hook payload's own tool_use_id must reach the event -- it is the only \
                     thing that can link a permission card to the call it gates"
                );
                assert_eq!(tool_name, "Bash");
                assert_eq!(*source, PermissionSource::HookRelay);
            }
            other => panic!("expected PermissionRequest, got {other:?}"),
        }
        assert!(pending.lock().unwrap().contains_key("toolu_01CtdezhmhUCrBaswxW5HYmC"));

        // One layer on, with the event the real socket just produced -- not a reconstruction of
        // it. This is the join the two halves of this feature used to be tested either side of.
        let mut interrupt_requested = false;
        let mut sources = std::collections::HashMap::new();
        let domain =
            crate::session::translate_wire_event(event, None, &mut interrupt_requested, &mut sources, &mut None);
        assert_eq!(domain.len(), 1);
        match &domain[0] {
            crate::projection::AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                ..
            } => {
                assert_eq!(
                    tool_use_id.as_deref(),
                    Some("toolu_01CtdezhmhUCrBaswxW5HYmC"),
                    "the id survived the socket but was dropped in translation -- the domain event \
                     is what the projection and the frontend actually see"
                );
                assert_eq!(permission_id, "toolu_01CtdezhmhUCrBaswxW5HYmC");
            }
            other => panic!("expected PermissionRequested, got {other:?}"),
        }

        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&socket_path);
        handle.join().unwrap();
    }

    /// `respond_permission`'s `HookRelay` branch, exercised via its real underlying helper
    /// (`write_hook_decision`) against a real connected `UnixStream::pair()` -- no listener
    /// thread, no `claude` process needed. Confirms the exact decision JSON
    /// `hook_protocol::format_decision` produces is what actually lands on the wire, and that the
    /// entry is removed from `pending` afterward (so a second answer to the same `request_id`
    /// becomes a no-op rather than double-writing).
    #[test]
    fn write_hook_decision_writes_the_decision_and_removes_the_pending_entry() {
        let (agent_side, hook_side) = UnixStream::pair().unwrap();
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        pending
            .lock()
            .unwrap()
            .insert("toolu_1".to_string(), PendingHookConnection { stream: agent_side });

        write_hook_decision(&pending, "toolu_1", true, None).unwrap();

        let mut reader = BufReader::new(hook_side);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), crate::hook_protocol::format_decision(true, None));
        assert!(
            !pending.lock().unwrap().contains_key("toolu_1"),
            "answered request must be removed from pending"
        );
    }

    /// I2's transport half: two concurrent `agent-hook` connections are live at once (a real
    /// possibility -- one assistant message can carry several `tool_use` blocks and the hook
    /// matcher is `"*"`), and each must be answerable on its own connection without disturbing the
    /// other. Answers the SECOND one first (out of order, the interesting case) and confirms the
    /// first's peer sees nothing yet, then answers the first and confirms its own decision lands.
    #[test]
    fn two_concurrent_pending_connections_are_each_answered_on_their_own_socket() {
        let (agent_side_a, hook_side_a) = UnixStream::pair().unwrap();
        let (agent_side_b, hook_side_b) = UnixStream::pair().unwrap();
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        pending
            .lock()
            .unwrap()
            .insert("toolu_a".to_string(), PendingHookConnection { stream: agent_side_a });
        pending
            .lock()
            .unwrap()
            .insert("toolu_b".to_string(), PendingHookConnection { stream: agent_side_b });

        write_hook_decision(&pending, "toolu_b", false, Some("denied b")).unwrap();
        assert!(
            pending.lock().unwrap().contains_key("toolu_a"),
            "answering b must leave a pending"
        );

        // b's peer got b's decision...
        let mut reader_b = BufReader::new(hook_side_b);
        let mut line_b = String::new();
        reader_b.read_line(&mut line_b).unwrap();
        assert_eq!(
            line_b.trim(),
            crate::hook_protocol::format_decision(false, Some("denied b"))
        );

        // ...and a's peer, still unanswered, gets its own (different) decision when answered.
        write_hook_decision(&pending, "toolu_a", true, None).unwrap();
        let mut reader_a = BufReader::new(hook_side_a);
        let mut line_a = String::new();
        reader_a.read_line(&mut line_a).unwrap();
        assert_eq!(line_a.trim(), crate::hook_protocol::format_decision(true, None));
        assert!(pending.lock().unwrap().is_empty());
    }

    /// The regression test for I3, the real orphan a final review found: `shutdown()` never
    /// touched `pending_hook_connections`, so any `agent-hook` process blocked on a stashed
    /// connection stayed blocked forever once `claude` was killed (reparented to init, waiting out
    /// the full 600s hook timeout for a decision that could never come -- nothing in the public
    /// API can answer a request after shutdown). Uses the REAL listener path (a real
    /// `spawn_hook_listener`, a real client connection relaying the real captured `PreToolUse`
    /// fixture) so the stranded connection in the map is a genuinely accepted one, then confirms
    /// the peer -- genuinely blocked, verified before the drain -- is released once `shutdown()`'s
    /// drain runs: it receives an explicit fail-closed `deny` (so the still-alive-for-now CLI can
    /// never read the release as a silent allow) immediately followed by a real EOF (`read_line`
    /// returning `Ok(0)`, exactly what `agent-hook` itself would then see and exit on).
    #[test]
    fn releasing_pending_hook_connections_denies_and_then_eofs_a_blocked_peer() {
        let fixture = std::fs::read_to_string(format!(
            "{}/tests/fixtures/v2_hook_pretooluse_stdin.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();

        let socket_path = temp_socket_path();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (handle, stop) = spawn_hook_listener(listener, tx, pending.clone()).unwrap();

        // A real client playing `agent-hook`: relay the request, then block waiting for a decision.
        let mut client = UnixStream::connect(&socket_path).unwrap();
        writeln!(client, "{}", fixture.trim()).unwrap();
        rx.recv_timeout(Duration::from_secs(2))
            .expect("no PermissionRequest event within 2s");
        assert!(pending.lock().unwrap().contains_key("toolu_01CtdezhmhUCrBaswxW5HYmC"));

        let peer = std::thread::spawn(move || {
            let mut reader = BufReader::new(client);
            let mut decision = String::new();
            reader.read_line(&mut decision).unwrap();
            // Whatever comes next must be the socket closing, not another wait.
            let mut after = String::new();
            let bytes_after = reader.read_line(&mut after).unwrap();
            (decision, bytes_after)
        });

        // It must genuinely be blocked -- otherwise this test would prove nothing about stranding.
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !peer.is_finished(),
            "the peer should still be blocked waiting for a decision"
        );

        // Exactly what `shutdown()` now does.
        release_pending_hook_connections(&pending);

        let deadline = Instant::now() + Duration::from_secs(2);
        while !peer.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            peer.is_finished(),
            "the stranded peer was not released by draining pending_hook_connections"
        );
        let (decision, bytes_after) = peer.join().unwrap();
        let decision: serde_json::Value =
            serde_json::from_str(decision.trim()).expect("the released peer must receive a real decision");
        assert_eq!(
            decision["hookSpecificOutput"]["permissionDecision"], "deny",
            "a request released by shutdown must fail closed, not be answered with silence"
        );
        assert_eq!(
            bytes_after, 0,
            "the connection must then close, giving agent-hook a real EOF to exit on"
        );
        assert!(pending.lock().unwrap().is_empty());

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        let _ = std::fs::remove_file(&socket_path);
    }

    /// I5's regression test, the security-critical one: a relayed payload that cannot be parsed
    /// must come back as an explicit `deny`, never as a dropped connection. A dropped connection
    /// makes `agent-hook` exit 0 with empty stdout ("no decision"), which under
    /// `--permission-mode auto` lets the CLI run the tool on its own judgment -- a silent ALLOW
    /// produced by a failure, which is exactly what this hook exists to prevent.
    #[test]
    fn a_malformed_relay_payload_is_denied_explicitly_rather_than_silently_allowed() {
        let socket_path = temp_socket_path();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (handle, stop) = spawn_hook_listener(listener, tx, pending.clone()).unwrap();

        let mut client = UnixStream::connect(&socket_path).unwrap();
        writeln!(client, "{{\"this\": \"is not a PreToolUse payload\"}}").unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let decision: serde_json::Value = serde_json::from_str(line.trim()).expect("a decision must come back at all");
        assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
        let reason = decision["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(
            reason.contains("agent-hook relay failed"),
            "the deny must say why: {reason}"
        );
        assert!(
            pending.lock().unwrap().is_empty(),
            "an unparseable request is never left pending"
        );

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        let _ = std::fs::remove_file(&socket_path);
    }

    /// The same fail-closed guarantee for the other error branch: a peer that connects and closes
    /// its write side without ever sending a request (a crashed/killed `agent-hook`, or a socket
    /// hiccup) still gets an explicit deny, not silence, as long as it's still listening.
    #[test]
    fn an_empty_relay_payload_is_denied_explicitly_rather_than_silently_allowed() {
        let socket_path = temp_socket_path();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (handle, stop) = spawn_hook_listener(listener, tx, pending).unwrap();

        let client = UnixStream::connect(&socket_path).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let decision: serde_json::Value =
            serde_json::from_str(line.trim()).expect("a decision must come back even for an empty request");
        assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        let _ = std::fs::remove_file(&socket_path);
    }

    /// I4's regression test, exercising the REAL failure path: `spawn_with_binary` runs the exact
    /// production `spawn` body (real hook-settings construction, real `UnixListener::bind`, real
    /// `Command::spawn`) with only the binary name changed to one that does not exist, so
    /// `cmd.spawn()` genuinely fails. Before the fix, that left behind (a) a hook-listener thread
    /// whose stop flag nobody could ever set again, spinning its poll-plus-sleep loop for the rest
    /// of the process's life, and (b) the socket file. Runs the failing spawn five times so a
    /// per-call thread leak would be unmistakable against the noise of other tests' own
    /// short-lived threads running in parallel.
    ///
    /// The third leak this test used to guard -- a generated settings file -- can no longer exist:
    /// the hook configuration is an argv value now. The test still asserts no `.claude/` appears,
    /// since that absence is the property the change bought and a regression would reintroduce it
    /// silently.
    #[test]
    fn a_failed_spawn_leaks_no_thread_no_socket_and_no_settings_file() {
        let _socket_guard = hook_socket_guard();
        let dir = std::env::temp_dir().join(format!("agent-failed-spawn-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Precondition: building the hook configuration (which happens BEFORE `cmd.spawn()` and
        // can itself fail with the same `NotFound` kind if the `agent-hook` binary isn't built)
        // must genuinely succeed here -- otherwise the assertions below would pass for the wrong
        // reason.
        crate::settings::hook_settings_arg(&dir.join("probe.sock"))
            .expect("agent-hook must be built for this test to exercise the cmd.spawn() failure path");

        let sockets_before = temp_hook_socket_names();
        let threads_before = live_thread_count();
        for _ in 0..5 {
            let outcome = AgentProcess::spawn_with_binary(
                &dir,
                PermissionMode::Auto,
                &[],
                "/nonexistent/definitely-not-a-real-claude-binary",
            );
            match outcome {
                Ok(_) => panic!("spawning a nonexistent binary must fail"),
                Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            }
        }

        // Nothing may have been written into the project directory at all. This is no longer a
        // cleanup assertion but an absence-by-construction one: a regression that reintroduced a
        // file-based hook config would land here first.
        assert!(
            !dir.join(".claude").exists(),
            "spawning must not create anything under the project's .claude/ -- the hook config is argv"
        );

        // No leftover socket file from any of the five attempts (compared as a set difference
        // against a before-snapshot, so unrelated leftovers from other runs can't mask or fake
        // this).
        let new_sockets: Vec<_> = temp_hook_socket_names()
            .into_iter()
            .filter(|name| !sockets_before.contains(name))
            .collect();
        assert!(
            new_sockets.is_empty(),
            "a failed spawn must not leave its socket file behind: {new_sockets:?}"
        );

        // A leaked listener thread never exits, so five leaks would be five permanently-extra
        // threads. Threads that other tests in this same binary hold are, by contrast, transient
        // -- so poll until the count settles rather than trusting one sample. A single sample
        // failed deterministically at --test-threads=4 (and so on any 4-core machine, with no
        // flags at all) once this crate gained env-var-serialized tests whose sleeps overlap
        // this one's sampling window.
        let settle_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut threads_after = live_thread_count();
        while threads_after > threads_before + 2 && std::time::Instant::now() < settle_deadline {
            std::thread::sleep(Duration::from_millis(100));
            threads_after = live_thread_count();
        }
        assert!(
            threads_after <= threads_before + 2,
            "five failed spawns leaked threads: {threads_before} before, {threads_after} after"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live thread count for this test process, straight from the kernel -- `/proc/self/task` on
    /// Linux, `proc_pidinfo` on macOS (see `crate::process_probe`).
    fn live_thread_count() -> usize {
        crate::process_probe::thread_count(std::process::id()).expect("this process's thread count must be readable")
    }

    /// Every per-conversation hook socket currently sitting in the temp dir, by name.
    /// Held by every test here that binds a hook socket in the temp dir or asserts on the set of
    /// them: two such tests running in parallel would each see the other's socket as its own leak.
    static HOOK_SOCKET_TESTS: Mutex<()> = Mutex::new(());

    fn hook_socket_guard() -> std::sync::MutexGuard<'static, ()> {
        HOOK_SOCKET_TESTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_hook_socket_names() -> std::collections::HashSet<String> {
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(crate::socket_path::HOOK_SOCKET_PREFIX))
            .collect()
    }

    #[test]
    fn write_hook_decision_for_unknown_request_id_is_a_harmless_no_op() {
        let pending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        write_hook_decision(&pending, "no-such-request", false, Some("nope")).unwrap();
        // No panic, no error -- there was nothing to write to.
    }

    /// `respond_permission`'s `CanUseTool` branch builds this exact payload before writing it to
    /// stdin (see `send_turn`/`interrupt` elsewhere in this file for the identical
    /// write-then-flush pattern, already exercised against a real child in the `#[ignore]`d
    /// tests) -- this test covers the payload-shape half directly, without needing a live
    /// `ChildStdin`.
    #[test]
    fn build_can_use_tool_response_payload_allow_has_no_message_field() {
        let payload = build_can_use_tool_response_payload("req-1", true, None);
        assert_eq!(payload["type"], "control_response");
        assert_eq!(payload["response"]["subtype"], "success");
        assert_eq!(payload["response"]["request_id"], "req-1");
        assert_eq!(payload["response"]["response"]["behavior"], "allow");
        assert!(payload["response"]["response"].get("message").is_none());
    }

    #[test]
    fn build_can_use_tool_response_payload_deny_with_reason_includes_message() {
        let payload = build_can_use_tool_response_payload("req-2", false, Some("blocked by policy"));
        assert_eq!(payload["response"]["response"]["behavior"], "deny");
        assert_eq!(payload["response"]["response"]["message"], "blocked by policy");
    }

    #[test]
    fn build_can_use_tool_response_payload_deny_without_reason_has_no_message_field() {
        let payload = build_can_use_tool_response_payload("req-3", false, None);
        assert_eq!(payload["response"]["response"]["behavior"], "deny");
        assert!(payload["response"]["response"].get("message").is_none());
    }

    /// Owns a fake-`claude`-binary script file created by [`fake_binary_script`] and removes it on
    /// drop, so a test that panics on an assertion before reaching its own cleanup line (as every
    /// call site below used to write by hand) still leaves nothing behind in `/tmp`. Derefs to
    /// `Path` so callers can keep passing it wherever a `&Path`/`.to_str()` was expected.
    struct FakeBinaryScript(std::path::PathBuf);

    impl std::ops::Deref for FakeBinaryScript {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for FakeBinaryScript {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Writes a small shell script to a fresh temp file, makes it executable, and returns a guard
    /// owning its path -- a fake `claude` binary standing in for the real one, so the preflight
    /// tests below can drive both its accept and refuse paths deterministically, with no real
    /// CLI, no network, and no tokens.
    fn fake_binary_script(body: &str) -> FakeBinaryScript {
        let path = std::env::temp_dir().join(format!("agent-process-fake-binary-{}.sh", uuid::Uuid::new_v4()));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        FakeBinaryScript(path)
    }

    /// The preflight must probe the PROJECT directory, not this process's own cwd.
    ///
    /// Not a hypothetical: on 2026-09-18 this host's `claude` was a multi-account launcher that
    /// derives a per-repository key from `pwd -P` and refuses a directory name it cannot use, and
    /// identical argv was measured exiting 0 from one directory and 64 from another. A preflight
    /// probing the wrong directory is worse than none -- it goes green and the real spawn then
    /// dies, reproducing exactly the unexplained failure the preflight was added to prevent, and
    /// the inverse refuses a spawn that would have worked.
    ///
    /// The fake binary here stands in for that launcher: it accepts from `allowed` and refuses
    /// from anywhere else. If `preflight_gate_flag_is_accepted` ever drops its `current_dir`, this
    /// goes red instead of the next GUI session going unexplained.
    #[test]
    fn the_preflight_probes_the_project_directory_not_the_hosts_own_cwd() {
        let root = std::env::temp_dir().join(format!("agent-preflight-cwd-{}", uuid::Uuid::new_v4()));
        let allowed = root.join("allowed");
        let refused = root.join("refused");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&refused).unwrap();
        let script = fake_binary_script(
            "case \"$(basename \"$PWD\")\" in allowed) exit 0 ;; *) echo 'unsafe repository name' >&2; exit 64 ;; esac",
        );

        let from_allowed = preflight_gate_flag_is_accepted(script.to_str().unwrap(), "{}", &allowed);
        assert!(
            from_allowed.is_ok(),
            "the project directory is the one probed: {from_allowed:?}"
        );

        let from_refused = preflight_gate_flag_is_accepted(script.to_str().unwrap(), "{}", &refused);
        let err = from_refused.expect_err("a project directory the binary refuses must fail the preflight");
        assert!(
            err.to_string().contains("unsafe repository name"),
            "the refusal must carry the binary's own stderr, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The happy path: a binary that accepts `--settings` unconditionally (as the real CLI does
    /// when it is not sitting behind a launcher that owns that flag) must pass the preflight.
    #[test]
    fn preflight_gate_flag_is_accepted_passes_when_the_binary_accepts_settings() {
        let script = fake_binary_script("exit 0");
        let result = preflight_gate_flag_is_accepted(script.to_str().unwrap(), "{}", &std::env::temp_dir());
        assert!(result.is_ok(), "expected the preflight to pass, got {result:?}");
    }

    /// The regression test for the real 2026-09-18 bug: a fake `claude-wrapper`-shaped launcher
    /// that refuses any invocation carrying `--settings`, with the exact stderr message the real
    /// launcher printed on this host. The preflight must fail, and -- this is the point of the
    /// whole feature -- its error must carry that message verbatim rather than a generic
    /// "preflight failed", since the message is the one thing this task exists to surface.
    #[test]
    fn preflight_gate_flag_is_accepted_surfaces_the_binarys_own_stderr_when_it_refuses() {
        let script = fake_binary_script(
            r#"for arg in "$@"; do
  if [ "$arg" = "--settings" ]; then
    echo "claude-wrapper: production launcher owns --settings for autoMemoryDirectory" >&2
    exit 64
  fi
done
exit 0"#,
        );
        let result = preflight_gate_flag_is_accepted(script.to_str().unwrap(), "{}", &std::env::temp_dir());
        let err = result.expect_err("a binary that refuses --settings must fail the preflight");
        assert!(
            err.to_string()
                .contains("claude-wrapper: production launcher owns --settings for autoMemoryDirectory"),
            "the real launcher's own refusal message must reach the caller verbatim, got: {err}"
        );
    }

    /// The same fake launcher, but exercised through the real `spawn_with_binary` in `Auto` mode
    /// (which is exactly when a gate is installed) rather than calling the preflight function in
    /// isolation -- proving the preflight is actually wired into the real spawn path and runs
    /// BEFORE the socket is ever bound, so a doomed spawn leaves nothing behind to clean up.
    #[test]
    fn spawn_with_binary_fails_fast_via_preflight_and_binds_no_socket_when_the_resolved_binary_refuses_settings() {
        let _socket_guard = hook_socket_guard();
        let dir = std::env::temp_dir().join(format!("agent-process-preflight-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::settings::hook_settings_arg(&dir.join("probe.sock"))
            .expect("agent-hook must be built for this test to exercise the real preflight path");

        let script = fake_binary_script(
            r#"for arg in "$@"; do
  if [ "$arg" = "--settings" ]; then
    echo "claude-wrapper: production launcher owns --settings for autoMemoryDirectory" >&2
    exit 64
  fi
done
exit 0"#,
        );

        let sockets_before = temp_hook_socket_names();
        let outcome = AgentProcess::spawn_with_binary(&dir, PermissionMode::Auto, &[], script.to_str().unwrap());
        let err = outcome
            .err()
            .expect("spawn must fail when the resolved binary refuses the gate flag");
        assert!(
            err.to_string()
                .contains("claude-wrapper: production launcher owns --settings for autoMemoryDirectory"),
            "spawn_with_binary's error must carry the launcher's own refusal, got: {err}"
        );

        let new_sockets: Vec<_> = temp_hook_socket_names()
            .into_iter()
            .filter(|name| !sockets_before.contains(name))
            .collect();
        assert!(
            new_sockets.is_empty(),
            "a preflight failure must happen before the hook socket is ever bound, found: {new_sockets:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The legacy half of the owner's 2026-09-25 ruling ("auto模式给claude，和claude本身的做法一致"),
    /// read off the argv the real spawn path hands the child rather than off `disallowed_tools_for`:
    /// in `Auto` the child gets the `PreToolUse` gate (`--settings`) and NO `--disallowedTools`, so
    /// `Bash` is offered and every call to it reaches the hook. A spawn that still passed
    /// `--disallowedTools Bash` would pass the pure test above and fail here.
    #[test]
    fn an_auto_spawn_offers_bash_under_the_gate_and_passes_no_deny_list() {
        let _socket_guard = hook_socket_guard();
        let dir = std::env::temp_dir().join(format!("agent-process-auto-argv-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::settings::hook_settings_arg(&dir.join("probe.sock"))
            .expect("agent-hook must be built for this test to exercise the real gate path");
        let record = dir.join("argv.txt");
        // Records every invocation's argv (the preflight's `--version` one included), one per line,
        // then waits on stdin like a real stream-json session until `shutdown` closes it.
        let script = fake_binary_script(&format!(
            "printf '%s\\n' \"$*\" >> '{}'\nfor arg in \"$@\"; do [ \"$arg\" = --version ] && exit 0; done\nexec cat >/dev/null",
            record.display()
        ));

        let mut process = AgentProcess::spawn_with_binary(
            &dir,
            PermissionMode::Auto,
            disallowed_tools_for(PermissionMode::Auto),
            script.to_str().unwrap(),
        )
        .expect("the fake binary accepts --settings, so the Auto spawn succeeds");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let session_argv = loop {
            let text = std::fs::read_to_string(&record).unwrap_or_default();
            if let Some(line) = text.lines().find(|l| l.contains("--input-format")) {
                break line.to_string();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the session spawn never recorded its argv"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        process.shutdown();

        assert!(
            session_argv.contains("--permission-mode auto"),
            "Auto spawns the CLI in auto: {session_argv}"
        );
        assert!(
            session_argv.contains("--settings"),
            "Auto installs the PreToolUse gate, which is what makes offering Bash safe: {session_argv}"
        );
        assert!(
            !session_argv.contains("--disallowedTools"),
            "Auto must offer Bash (and every other tool) under the gate, so no deny list is passed: {session_argv}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Bypass` mode installs no gate and therefore builds no `--settings` value, so the
    /// preflight must never run at all -- a binary that would refuse `--settings` is irrelevant
    /// to a mode that never passes it, and this is also the mode this project explicitly calls
    /// out as the current workaround for today's launcher (see `PermissionMode`'s own doc), so a
    /// regression that started preflighting Bypass too would break the one mode that currently
    /// works on this host.
    #[test]
    fn spawn_with_binary_never_preflights_in_bypass_mode() {
        let _socket_guard = hook_socket_guard();
        let dir = std::env::temp_dir().join(format!("agent-process-bypass-preflight-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // This script would refuse ANY invocation -- if Bypass mode ever preflighted it, this
        // test would see this script's own refusal message in the error instead of a real
        // process actually spawning (Bypass installs no --settings, so nothing here should ever
        // read this script's stderr as a preflight failure).
        let script = fake_binary_script("echo \"should never be invoked for a preflight check\" >&2\nexit 1");

        let outcome = AgentProcess::spawn_with_binary(&dir, PermissionMode::Bypass, &[], script.to_str().unwrap());
        match outcome {
            Ok(mut process) => process.shutdown(),
            Err(e) => panic!("Bypass mode must not preflight the resolved binary at all, got: {e}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `resolve_binary_absolute_path` with a name (no `/`) must find it on `PATH`, exactly as
    /// `execvp` (and therefore `Command::spawn`) would -- `sh` is used because every Unix-like
    /// environment that can run this test suite at all necessarily has one.
    #[test]
    fn resolve_binary_absolute_path_finds_a_path_relative_name() {
        let resolved = resolve_binary_absolute_path("sh");
        let resolved = resolved.expect("sh must be found on PATH in any environment that can run a shell test");
        assert!(
            resolved.is_absolute(),
            "resolved path must be absolute, got {resolved:?}"
        );
        assert!(
            resolved.is_file(),
            "resolved path must actually exist, got {resolved:?}"
        );
    }

    /// A name that exists on no `PATH` entry resolves to `None`, not a fabricated guess -- the
    /// caller renders that as an explicit placeholder rather than a wrong path.
    #[test]
    fn resolve_binary_absolute_path_returns_none_for_a_name_on_no_path_entry() {
        let resolved = resolve_binary_absolute_path("definitely-not-a-real-binary-name-2026-09-18");
        assert!(
            resolved.is_none(),
            "a nonexistent name must resolve to None, got {resolved:?}"
        );
    }
}
