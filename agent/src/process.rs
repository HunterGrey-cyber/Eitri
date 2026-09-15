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
/// line before giving up on it. `accept()` being non-blocking does NOT make the accepted
/// `UnixStream` non-blocking too (confirmed real: an accepted socket on Linux does not inherit
/// `O_NONBLOCK` from a non-blocking listener) -- without this, a connection that starts but never
/// completes its line (a real `agent-hook` process killed/orphaned mid-write) would leave
/// `read_line` blocked forever, with the same practical effect as the accept()-side deadlock this
/// module already fixed once. A few hundred ms is generous: under normal operation `agent-hook`
/// writes its one already-fully-read-from-its-own-stdin line immediately after connecting.
const HOOK_CONNECTION_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// A conservative, documented starting point for `disallowed_tools` -- see this plan's Global
/// Constraint on why this is best-effort, not a guarantee. Callers may pass their own list
/// instead; this is a suggested default, not hardcoded into `spawn`.
pub const CONSERVATIVE_DISALLOWED_TOOLS: &[&str] = &["Bash", "Write", "Edit", "NotebookEdit"];

/// Which permission gate the CLI itself enforces for the whole conversation. `Auto` is the normal
/// mode: the real, reliable `PreToolUse` hook (relayed by `agent-hook` over the per-conversation
/// Unix socket) is the primary gate; `CanUseTool` control_requests are a secondary, unreliable
/// signal (see `PermissionSource`). `Bypass` skips permission gating entirely (no hook settings
/// are even generated) -- intended only for trusted, non-interactive callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// Also bounds the per-connection read via `HOOK_CONNECTION_READ_TIMEOUT`: a non-blocking
/// listener's accepted streams do NOT themselves come back non-blocking (confirmed real, same
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
        pending.lock().unwrap().insert(request_id.clone(), PendingHookConnection { stream });
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

/// Writes an explicit `deny` back over a hook connection whose relay failed, so a broken relay can
/// never be mistaken for an approval -- see `spawn_hook_listener`'s fail-closed doc. Deliberately
/// best-effort and infallible from the caller's point of view: if the peer has already gone away
/// there is no one left to tell, and the connection is being dropped either way.
fn write_fail_closed_deny(stream: &UnixStream, reason: &str) {
    let decision =
        crate::hook_protocol::format_decision(false, Some(&format!("agent-hook relay failed: {reason}")));
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
fn release_pending_hook_connections(
    pending: &Arc<Mutex<std::collections::HashMap<String, PendingHookConnection>>>,
) {
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
        let socket_path = std::env::temp_dir().join(format!("neovibe-agent-hook-{conversation_id}.sock"));
        let _ = std::fs::remove_file(&socket_path); // stale leftover from a prior crash, if any

        // Bypass mode asks for no gate at all, so no hook is installed. In every other mode the
        // gate travels in this one process's argv, where no other conversation can reach it.
        let hook_settings_arg = if mode == PermissionMode::Bypass {
            None
        } else {
            Some(crate::settings::hook_settings_arg(&socket_path)?)
        };
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
            stdin: Some(stdin),
            events_rx: rx,
            reader_handle: Some(reader_handle),
            stderr_handle: Some(stderr_handle),
            hook_listener_handle: Some(hook_listener_handle),
            hook_listener_stop,
            pending_hook_connections,
            socket_path,
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
                events.push(AgentEvent::ProcessExited { success: status.success() });
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
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin already closed (process shut down)"))?;
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
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin already closed (process shut down)")
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
            PermissionSource::HookRelay => write_hook_decision(&self.pending_hook_connections, request_id, allow, reason),
            PermissionSource::CanUseTool => {
                let payload = build_can_use_tool_response_payload(request_id, allow, reason);
                let stdin = self.stdin.as_mut().ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin already closed (process shut down)")
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
        if ret == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
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
        assert!(result2.to_lowercase().contains("pong"), "same process must recall turn 1 with no --resume: {result2}");

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

        process.send_turn("write a very long story, at least 2000 words, about a journey").unwrap();
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
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
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
        std::env::temp_dir().join(format!("agent-process-listener-test-{}.sock", uuid::Uuid::new_v4()))
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
        assert!(handle.is_finished(), "hook-listener thread did not stop within 2s of the stop flag being set");
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

        let event = rx.recv_timeout(Duration::from_secs(2)).expect("no PermissionRequest event within 2s");
        match &event {
            AgentEvent::PermissionRequest { request_id, tool_use_id, tool_name, source, .. } => {
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
        let domain = crate::session::translate_wire_event(
            event,
            None,
            &mut interrupt_requested,
            &mut sources,
        );
        assert_eq!(domain.len(), 1);
        match &domain[0] {
            crate::projection::AgentDomainEvent::PermissionRequested { permission_id, tool_use_id, .. } => {
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
        assert!(!pending.lock().unwrap().contains_key("toolu_1"), "answered request must be removed from pending");
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
        pending.lock().unwrap().insert("toolu_a".to_string(), PendingHookConnection { stream: agent_side_a });
        pending.lock().unwrap().insert("toolu_b".to_string(), PendingHookConnection { stream: agent_side_b });

        write_hook_decision(&pending, "toolu_b", false, Some("denied b")).unwrap();
        assert!(pending.lock().unwrap().contains_key("toolu_a"), "answering b must leave a pending");

        // b's peer got b's decision...
        let mut reader_b = BufReader::new(hook_side_b);
        let mut line_b = String::new();
        reader_b.read_line(&mut line_b).unwrap();
        assert_eq!(line_b.trim(), crate::hook_protocol::format_decision(false, Some("denied b")));

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
        rx.recv_timeout(Duration::from_secs(2)).expect("no PermissionRequest event within 2s");
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
        assert!(!peer.is_finished(), "the peer should still be blocked waiting for a decision");

        // Exactly what `shutdown()` now does.
        release_pending_hook_connections(&pending);

        let deadline = Instant::now() + Duration::from_secs(2);
        while !peer.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(peer.is_finished(), "the stranded peer was not released by draining pending_hook_connections");
        let (decision, bytes_after) = peer.join().unwrap();
        let decision: serde_json::Value =
            serde_json::from_str(decision.trim()).expect("the released peer must receive a real decision");
        assert_eq!(
            decision["hookSpecificOutput"]["permissionDecision"], "deny",
            "a request released by shutdown must fail closed, not be answered with silence"
        );
        assert_eq!(bytes_after, 0, "the connection must then close, giving agent-hook a real EOF to exit on");
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
        let reason = decision["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap();
        assert!(reason.contains("agent-hook relay failed"), "the deny must say why: {reason}");
        assert!(pending.lock().unwrap().is_empty(), "an unparseable request is never left pending");

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
        let new_sockets: Vec<_> =
            temp_hook_socket_names().into_iter().filter(|name| !sockets_before.contains(name)).collect();
        assert!(new_sockets.is_empty(), "a failed spawn must not leave its socket file behind: {new_sockets:?}");

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

    /// Live thread count for this test process, straight from the kernel (`/proc/self/task`) --
    /// Linux-only, same platform scope as the rest of this module.
    fn live_thread_count() -> usize {
        std::fs::read_dir("/proc/self/task").unwrap().count()
    }

    /// Every per-conversation hook socket currently sitting in the temp dir, by name.
    fn temp_hook_socket_names() -> std::collections::HashSet<String> {
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("neovibe-agent-hook-"))
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
}
