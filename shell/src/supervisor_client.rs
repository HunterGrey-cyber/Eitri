//! `shell`'s client of the cross-window agent-status dashboard (`neovibe-supervisor`). Connects
//! once at startup (spawning the supervisor if nothing is listening yet), registers this
//! instance, and pushes a status update whenever the derived status actually changes. See
//! docs/superpowers/specs/2026-09-08-supervisor-cross-window-agent-status-design.md.

use agent::{AgentSessionProjection, ProjectionStatus};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use supervisor::{AgentStatus, ShellMessage, SupervisorMessage};

/// The pure mapping from spec §4's table -- checked in the exact stated order. Takes
/// `Option<&AgentSessionProjection>` rather than the state directly because `shell`'s own
/// `AgentPanelState.session` is itself an `Option<AgentSession>` before the user leaves the mode
/// selector; `None` here means exactly that (spec's `no_session` row).
pub(crate) fn derive_status(session: Option<&AgentSessionProjection>) -> AgentStatus {
    let Some(projection) = session else {
        return AgentStatus::NoSession;
    };
    if !projection.pending_permissions.is_empty() {
        return AgentStatus::Blocked;
    }
    if projection.active_turn_id.is_some() {
        return AgentStatus::Working;
    }
    if matches!(projection.status, ProjectionStatus::Unavailable { .. } | ProjectionStatus::Closed { .. }) {
        return AgentStatus::Done;
    }
    AgentStatus::Idle
}

/// A live connection to `neovibe-supervisor`, or the deliberate absence of one. Every method is
/// best-effort: a failure anywhere in this client (spawn fails, connect fails, write fails)
/// degrades to "this shell window just doesn't appear in the dashboard" rather than propagating
/// an error `shell`'s own startup or main loop would need to handle -- this is a nice-to-have
/// integration, never a safety-critical one like `agent`'s own permission hook.
pub(crate) struct SupervisorClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    instance_id: String,
    last_sent_status: Option<AgentStatus>,
}

impl SupervisorClient {
    /// Tries to connect to the well-known socket; if nothing is listening, locates and spawns
    /// `neovibe-supervisor` as a detached background process and retries a bounded number of
    /// times (the freshly-spawned process needs a moment to bind its own socket). Returns `None`
    /// if every attempt fails -- logged, never panics, matching this crate's own established
    /// "log and continue" posture for non-critical integrations.
    pub(crate) fn connect_or_spawn(instance_id: String, project_name: String, project_dir: &Path) -> Option<Self> {
        let socket_path = supervisor::socket_path();

        if let Ok(stream) = UnixStream::connect(&socket_path) {
            return Self::finish_connecting(stream, instance_id, project_name, project_dir);
        }

        match supervisor::locate_supervisor_binary() {
            Ok(binary) => {
                if let Err(e) = std::process::Command::new(&binary).spawn() {
                    eprintln!("shell: failed to spawn {binary:?}: {e}");
                    return None;
                }
            }
            Err(e) => {
                eprintln!("shell: could not locate neovibe-supervisor binary: {e}");
                return None;
            }
        }

        const RETRY_ATTEMPTS: u32 = 5;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
        for _ in 0..RETRY_ATTEMPTS {
            std::thread::sleep(RETRY_DELAY);
            if let Ok(stream) = UnixStream::connect(&socket_path) {
                return Self::finish_connecting(stream, instance_id, project_name, project_dir);
            }
        }
        eprintln!("shell: neovibe-supervisor did not accept a connection after spawning and {RETRY_ATTEMPTS} retries");
        None
    }

    /// `instance_id` is captured into the returned `Self` (not just used once and discarded) --
    /// `send_status` (below) needs it on every call to build a `ShellMessage::Status`, since the
    /// wire protocol has no per-connection implicit identity beyond what `Register` announced.
    fn finish_connecting(stream: UnixStream, instance_id: String, project_name: String, project_dir: &Path) -> Option<Self> {
        // Non-blocking for reads (poll_activate must never stall the GTK main loop), but the
        // initial Register write below happens before this is set, while the connection is still
        // in its default blocking mode -- a single small write to a socket the peer just accepted
        // does not block in practice, and this mirrors `agent-hook`'s own client-side write.
        let register = ShellMessage::Register {
            instance_id: instance_id.clone(),
            project_name,
            project_dir: project_dir.to_string_lossy().to_string(),
            pid: std::process::id(),
        };
        let mut stream = stream;
        let payload = serde_json::to_string(&register).ok()?;
        writeln!(stream, "{payload}").ok()?;

        stream.set_nonblocking(true).ok()?;
        let reader = BufReader::new(stream.try_clone().ok()?);
        Some(Self { stream, reader, instance_id, last_sent_status: None })
    }

    /// Sends a `Status` message only if `status` differs from the last one actually sent --
    /// called from `agent_panel.rs`'s existing 33ms pump timer, so without this dedup every tick
    /// would write to the socket regardless of whether anything changed.
    pub(crate) fn send_status(&mut self, status: AgentStatus) {
        if self.last_sent_status == Some(status) {
            return;
        }
        let msg = ShellMessage::Status { instance_id: self.instance_id.clone(), status };
        if let Ok(payload) = serde_json::to_string(&msg) {
            if writeln!(self.stream, "{payload}").is_ok() {
                self.last_sent_status = Some(status);
            }
        }
    }

    /// Non-blocking: returns `true` if a `SupervisorMessage::Activate` arrived since the last
    /// call. Drains any buffered lines fully (a burst arriving between polls shouldn't be missed
    /// or double-counted), returning `true` if *any* of them was `Activate`.
    pub(crate) fn poll_activate(&mut self) -> bool {
        let mut activated = false;
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => break, // peer closed -- nothing more to read, ever
                Ok(_) => {
                    if let Ok(SupervisorMessage::Activate) = serde_json::from_str(line.trim()) {
                        activated = true;
                    }
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        activated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::PermissionRequestRecord;

    fn base_state() -> AgentSessionProjection {
        AgentSessionProjection { status: ProjectionStatus::Running, ..Default::default() }
    }

    #[test]
    fn no_session_when_there_is_no_agent_session_at_all() {
        assert_eq!(derive_status(None), AgentStatus::NoSession);
    }

    #[test]
    fn blocked_takes_priority_over_everything_else() {
        let mut state = base_state();
        state.active_turn_id = Some("turn-1".into()); // would otherwise read as Working
        state.pending_permissions.insert(
            "r1".into(),
            PermissionRequestRecord { permission_id: "r1".into(), tool_name: "Read".into(), input: serde_json::json!({}) },
        );
        assert_eq!(derive_status(Some(&state)), AgentStatus::Blocked);
    }

    #[test]
    fn working_when_turn_in_progress_and_nothing_pending() {
        let mut state = base_state();
        state.active_turn_id = Some("turn-1".into());
        assert_eq!(derive_status(Some(&state)), AgentStatus::Working);
    }

    #[test]
    fn done_when_closed_and_no_turn_in_progress() {
        let mut state = base_state();
        state.status = ProjectionStatus::Closed { reason: "closed_by_host".into() };
        assert_eq!(derive_status(Some(&state)), AgentStatus::Done);
    }

    #[test]
    fn done_when_unavailable_and_no_turn_in_progress() {
        let mut state = base_state();
        state.status = ProjectionStatus::Unavailable { reason: "provider process exited unexpectedly".into() };
        assert_eq!(derive_status(Some(&state)), AgentStatus::Done);
    }

    #[test]
    fn idle_when_running_with_nothing_pending_and_no_turn_in_progress() {
        let state = base_state();
        assert_eq!(derive_status(Some(&state)), AgentStatus::Idle);
    }
}
