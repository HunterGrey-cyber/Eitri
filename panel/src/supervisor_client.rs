//! `shell`'s client of the cross-window agent-status dashboard (`eitri-supervisor`). Connects
//! once at startup to a supervisor that is already listening, registers this instance, and pushes
//! a status update whenever the derived status actually changes. It does NOT start one: since
//! 2026-09-19 that needs `EITRI_SUPERVISOR=1`, because a window appearing on screen is a thing
//! the user asks for (see `spawn_requested`). See
//! docs/superpowers/specs/2026-09-08-supervisor-cross-window-agent-status-design.md.

use agent::{AgentSessionProjection, ProjectionStatus};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use supervisor::{AgentStatus, ShellMessage, SupervisorMessage};

/// One status per window, now that a window holds several sessions: the most urgent tab's. Per-tab rows in the dashboard are a follow-on.
pub fn aggregate_status(statuses: impl IntoIterator<Item = AgentStatus>) -> AgentStatus {
    fn rank(status: AgentStatus) -> u8 {
        match status {
            AgentStatus::Blocked => 4,
            AgentStatus::Working => 3,
            AgentStatus::Idle => 2,
            AgentStatus::Done => 1,
            AgentStatus::NoSession => 0,
        }
    }
    statuses
        .into_iter()
        .max_by_key(|s| rank(*s))
        .unwrap_or(AgentStatus::NoSession)
}

/// The pure mapping from a session's projection to the dashboard's status, checked in the exact stated order. Takes
/// `Option<&AgentSessionProjection>` rather than the state directly because a session tab has no
/// backend until its first send or resume (and none once it failed); `None` here means exactly
/// that (the `no_session` status). One call per tab; `aggregate_status` picks the window's.
pub fn derive_status(projection: Option<&AgentSessionProjection>) -> AgentStatus {
    let Some(projection) = projection else {
        return AgentStatus::NoSession;
    };
    // A terminal status is checked FIRST, ahead of the two "busy" signals. A session that has ended
    // is neither blocked nor working, whatever its last-known fields say -- and a permission request
    // that was still pending when the session died stays in the projection on purpose (it is real
    // history, and deleting it would read as a resolution nobody made), so without this ordering a
    // dead session showed a permanent Blocked dot in the dashboard.
    if matches!(
        projection.status,
        ProjectionStatus::Unavailable { .. } | ProjectionStatus::Closed { .. }
    ) {
        return AgentStatus::Done;
    }
    if !projection.pending_permissions.is_empty() {
        return AgentStatus::Blocked;
    }
    if projection.active_turn_id.is_some() {
        return AgentStatus::Working;
    }
    AgentStatus::Idle
}

/// A live connection to `eitri-supervisor`, or the deliberate absence of one. Every method is
/// best-effort: a failure anywhere in this client (spawn fails, connect fails, write fails)
/// degrades to "this shell window just doesn't appear in the dashboard" rather than propagating
/// an error `shell`'s own startup or main loop would need to handle -- this is a nice-to-have
/// integration, never a safety-critical one like `agent`'s own permission hook.
pub struct SupervisorClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    instance_id: String,
    last_sent_status: Option<AgentStatus>,
    /// Set once a write or read has failed, or the peer has cleanly closed. Short-circuits
    /// `send_status`/`poll_activate` so a dead supervisor doesn't cost a failed write attempt on
    /// every 33ms pump tick for the rest of this session.
    dead: bool,
}

/// The outcome of asking for a supervisor connection: either one already existed and is ready
/// now, or one was just spawned and the connection is being waited for off this thread.
///
/// The split exists because the two cases have opposite cost profiles and the old code paid the
/// slow one on the GTK main thread. A supervisor that is already listening answers instantly,
/// which is every window after the first. A supervisor that has to be *started* needs a cold GTK4
/// process to initialize and bind its socket -- far more than the 250ms this used to allow, so the
/// window that spawned it was systematically the one window the dashboard never showed. Observed
/// live on 2026-09-15, not theorized: two windows, and only the second one appeared.
pub enum PendingSupervisor {
    Ready(Option<SupervisorClient>),
    Connecting(std::sync::mpsc::Receiver<Option<SupervisorClient>>),
}

/// How long to wait before each connect attempt after spawning, in milliseconds.
///
/// Doubling from a short first wait, then holding, so a fast machine pays almost nothing and a
/// slow or loaded one still succeeds. The total would be indefensible on the GTK main thread and
/// is unremarkable on a worker thread: nothing waits on it, and a window whose dashboard row
/// appears a few seconds late is a dashboard detail, not a startup delay.
fn retry_schedule() -> Vec<u64> {
    vec![25, 50, 100, 200, 400, 800, 1600, 1600, 1600, 1600]
}

/// Whether this window may *start* `eitri-supervisor` when none is listening.
///
/// **Default: no (changed 2026-09-19).**
/// Until then, the first `shell` launch on a machine put a second, unasked-for GTK window on the
/// screen -- a dashboard nobody had opened, belonging to a feature the user had not invoked.
///
/// Connecting is unaffected and is still unconditional: a supervisor the user *has* opened still
/// gets this window's status, so the dashboard works exactly as before for anyone who wants it.
/// What changed is only who decides it should exist. That distinction is the whole point -- the
/// old behaviour conflated "use the dashboard if it is there" with "there should be a dashboard",
/// and only the second one is a decision.
///
/// Takes the raw variable rather than reading the environment, the same reason
/// `BackendKind::choose` does: the matrix is then testable without mutating a process-global that
/// every other test in the binary shares.
///
/// Anything unrecognised is `false`. This is the safe direction and the opposite of the one
/// `BackendKind::from_env` picks for its own typos, deliberately: a typo there still selects a
/// *backend* (the sidecar, with a warning -- it used to
/// fall to legacy instead, before legacy became the gated backend), so it warns; a typo here
/// silently declines to open a window, which the user can see and correct by looking at the screen.
fn spawn_requested(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1" | "true" | "yes"))
}

impl SupervisorClient {
    /// Connects to the well-known socket, spawning `eitri-supervisor` first if nothing is
    /// listening. Never blocks the caller for longer than one connect attempt: when a spawn is
    /// needed, the retry loop runs on a worker thread and the result arrives over the returned
    /// channel, which `agent_panel`'s existing 33ms pump drains -- the same shape it already uses
    /// for off-thread backend construction.
    ///
    /// Every failure degrades to "this window just doesn't appear in the dashboard", logged and
    /// never propagated, matching this crate's posture for non-critical integrations.
    pub fn connect_or_spawn(instance_id: String, project_name: String, project_dir: &Path) -> PendingSupervisor {
        let socket_path = supervisor::socket_path();

        if let Ok(stream) = UnixStream::connect(&socket_path) {
            return PendingSupervisor::Ready(Self::finish_connecting(stream, instance_id, project_name, project_dir));
        }

        // Nothing is listening, and that is where this used to spawn one. It no longer does by
        // default -- see `spawn_requested`. Registering with a dashboard somebody opened and
        // OPENING one are different acts, and only the first is this window's business.
        if !spawn_requested(std::env::var("EITRI_SUPERVISOR").ok().as_deref()) {
            return PendingSupervisor::Ready(None);
        }

        match supervisor::locate_supervisor_binary() {
            Ok(binary) => {
                let mut command = std::process::Command::new(&binary);
                // A detached, persistent background process -- `shell` never waits on
                // it directly and must not inherit its stdio (GTK/a11y noise landing in this
                // process's own terminal) or its process group (a Ctrl+C in the launching
                // terminal must not also kill the "persistent" supervisor).
                command
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .process_group(0);
                match command.spawn() {
                    Ok(mut child) => {
                        // The supervisor is deliberately never waited on by this process (it's a
                        // separate, persistent background service, not a child this session
                        // owns). But `std::process::Child`'s own `Drop` does not call `wait()`,
                        // so an unreaped child becomes a zombie the moment the supervisor exits,
                        // held for the rest of this `shell` process's own lifetime. A detached
                        // thread that does nothing but block on `wait()` reaps it whenever that
                        // eventually happens, without this call site -- or the GTK main thread --
                        // waiting on anything.
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                    }
                    Err(e) => {
                        eprintln!("shell: failed to spawn {binary:?}: {e}");
                        return PendingSupervisor::Ready(None);
                    }
                }
            }
            Err(e) => {
                eprintln!("shell: could not locate eitri-supervisor binary: {e}");
                return PendingSupervisor::Ready(None);
            }
        }

        // From here the supervisor is starting but is not listening yet. Waiting for it is the
        // slow path, so it leaves this thread entirely.
        let (tx, rx) = std::sync::mpsc::channel();
        let project_dir = project_dir.to_path_buf();
        std::thread::spawn(move || {
            let schedule = retry_schedule();
            let attempts = schedule.len();
            let total_ms: u64 = schedule.iter().sum();
            for wait_ms in schedule {
                std::thread::sleep(std::time::Duration::from_millis(wait_ms));
                if let Ok(stream) = UnixStream::connect(&socket_path) {
                    let client = Self::finish_connecting(stream, instance_id, project_name, &project_dir);
                    let _ = tx.send(client);
                    return;
                }
            }
            eprintln!(
                "shell: eitri-supervisor did not accept a connection after spawning and \
                 {attempts} retries over {total_ms}ms"
            );
            let _ = tx.send(None);
        });
        PendingSupervisor::Connecting(rx)
    }

    /// `instance_id` is captured into the returned `Self` (not just used once and discarded) --
    /// `send_status` (below) needs it on every call to build a `ShellMessage::Status`, since the
    /// wire protocol has no per-connection implicit identity beyond what `Register` announced.
    fn finish_connecting(
        mut stream: UnixStream,
        instance_id: String,
        project_name: String,
        project_dir: &Path,
    ) -> Option<Self> {
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
        let payload = serde_json::to_string(&register).ok()?;
        stream.write_all(format!("{payload}\n").as_bytes()).ok()?;

        stream.set_nonblocking(true).ok()?;
        let reader = BufReader::new(stream.try_clone().ok()?);
        Some(Self {
            stream,
            reader,
            instance_id,
            last_sent_status: None,
            dead: false,
        })
    }

    /// Sends a `Status` message only if `status` differs from the last one actually sent --
    /// called from `agent_panel.rs`'s existing 33ms pump timer, so without this dedup every tick
    /// would write to the socket regardless of whether anything changed.
    pub fn send_status(&mut self, status: AgentStatus) {
        if self.dead || self.last_sent_status == Some(status) {
            return;
        }
        let msg = ShellMessage::Status {
            instance_id: self.instance_id.clone(),
            status,
        };
        let Ok(payload) = serde_json::to_string(&msg) else {
            return;
        };
        if self.stream.write_all(format!("{payload}\n").as_bytes()).is_ok() {
            self.last_sent_status = Some(status);
        } else {
            self.dead = true;
        }
    }

    /// Non-blocking: returns `true` if a `SupervisorMessage::Activate` arrived since the last
    /// call. Drains any buffered lines fully (a burst arriving between polls shouldn't be missed
    /// or double-counted), returning `true` if *any* of them was `Activate`.
    pub fn poll_activate(&mut self) -> bool {
        if self.dead {
            return false;
        }
        let mut activated = false;
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => {
                    self.dead = true;
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<SupervisorMessage>(trimmed) {
                        Ok(SupervisorMessage::Activate) => activated = true,
                        Err(e) => {
                            eprintln!("shell: unparseable message from eitri-supervisor: {e} -- raw: {trimmed}")
                        }
                    }
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.dead = true;
                    break;
                }
            }
        }
        activated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::PermissionRequestRecord;

    /// The budget before 2026-09-15 was 5 attempts at a flat 50ms -- 250ms in total, spent on the
    /// GTK main thread. A cold GTK4 process does not bind its socket that fast, so the window that
    /// spawned the supervisor was systematically the one window the dashboard never showed
    /// (observed live, two windows, only the second appeared). This pins the SHAPE of the
    /// replacement rather than exact numbers: the steps may be retuned, but shortening the total
    /// back under a few seconds would restore the bug.
    #[test]
    fn the_retry_budget_is_long_enough_for_a_cold_gtk_process_to_start() {
        let schedule = retry_schedule();
        let total: u64 = schedule.iter().sum();
        assert!(
            total >= 5_000,
            "a cold GTK4 binary needs seconds, not the 250ms this used to allow; got {total}ms"
        );
        assert!(
            schedule.len() >= 8,
            "a few long sleeps answer a fast machine slowly; got {} attempts",
            schedule.len()
        );
    }

    /// A fast machine must not pay for the slow one: the first attempt is near-immediate and the
    /// waits only grow. A flat schedule long enough for the worst case would make every dashboard
    /// row appear late.
    #[test]
    fn the_first_attempt_is_prompt_and_the_waits_only_grow() {
        let schedule = retry_schedule();
        assert!(
            schedule[0] <= 50,
            "first wait should be near-immediate, got {}ms",
            schedule[0]
        );
        for pair in schedule.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "waits must never shrink: {}ms then {}ms",
                pair[0],
                pair[1]
            );
        }
    }

    fn base_state() -> AgentSessionProjection {
        AgentSessionProjection {
            status: ProjectionStatus::Running,
            ..Default::default()
        }
    }

    #[test]
    fn a_windows_status_is_its_most_urgent_tabs() {
        use AgentStatus::*;
        assert_eq!(aggregate_status([Idle, Blocked, Working]), Blocked, "needs input first");
        assert_eq!(aggregate_status([Idle, Working, Done]), Working);
        assert_eq!(aggregate_status([Done, Idle]), Idle);
        assert_eq!(aggregate_status([NoSession, Done]), Done);
        assert_eq!(aggregate_status([NoSession]), NoSession);
        assert_eq!(aggregate_status([]), NoSession);
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
            // `seq` orders this card in the panel's own rendering; `derive_status` never reads it.
            PermissionRequestRecord {
                seq: 0,
                permission_id: "r1".into(),
                tool_use_id: None,
                tool_name: "Read".into(),
                input: serde_json::json!({}),
                provider_prompt: None,
            },
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
        state.status = ProjectionStatus::Closed {
            reason: "closed_by_host".into(),
        };
        assert_eq!(derive_status(Some(&state)), AgentStatus::Done);
    }

    #[test]
    fn done_when_unavailable_and_no_turn_in_progress() {
        let mut state = base_state();
        state.status = ProjectionStatus::Unavailable {
            reason: "provider process exited unexpectedly".into(),
        };
        assert_eq!(derive_status(Some(&state)), AgentStatus::Done);
    }

    #[test]
    fn idle_when_running_with_nothing_pending_and_no_turn_in_progress() {
        let state = base_state();
        assert_eq!(derive_status(Some(&state)), AgentStatus::Idle);
    }

    /// The default is the whole point of `spawn_requested`, so it gets its own assertion rather
    /// than riding along in the matrix below: an unset variable is the case every normal launch
    /// takes, and the regression this guards is a second GTK window appearing unasked.
    #[test]
    fn an_unset_variable_does_not_start_a_supervisor() {
        assert!(!spawn_requested(None));
    }

    #[test]
    fn only_an_explicit_affirmative_starts_a_supervisor() {
        for yes in ["1", "true", "yes", " 1 ", "\ttrue\n"] {
            assert!(spawn_requested(Some(yes)), "{yes:?} should ask for a supervisor");
        }
        // Empty and "0" are the two a caller is most likely to produce by accident -- an unset
        // variable expanded by a shell (`EITRI_SUPERVISOR=$UNSET`), and someone turning it off
        // the obvious way. Neither may be read as consent.
        for no in ["", "0", "false", "no", "yes please", "TRUE", "on"] {
            assert!(!spawn_requested(Some(no)), "{no:?} should not start anything");
        }
    }
}
