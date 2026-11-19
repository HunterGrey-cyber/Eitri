//! Whether an agent's turn may be writing in a project tree right now, from the sessions this daemon follows and the
//! sidecars other installations' daemons run.
use std::path::Path;

use eitri_server_proto::{ProjectActivityResponse, SidecarInfo};

/// Where a session stands, from the events its journal recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    /// The last turn boundary recorded was an end (or no turn ever started).
    Idle,
    /// A turn started and its end has not been recorded.
    Running,
    /// Events are missing after the last boundary (a gap), or the journal could not be read: a turn may be running.
    Unknown,
}

/// The background tasks a session's last roster lists that are not ambient (they work on outside any turn).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tasks {
    Known(u32),
    /// No roster since a gap, or the journal could not be read.
    Unknown,
}

/// One followed session, as the judgement needs it.
#[derive(Clone, Debug)]
pub struct SessionFacts {
    pub sidecar: String,
    pub closed: bool,
    /// Why its observation ended while it was not closed, if it did: a session the sidecar no longer has
    /// (`NotFound`) runs nothing; any other end leaves its state unknown.
    pub ended: Option<tonic::Code>,
    /// Whether its watch is connected and caught up; a watch that is reconnecting or still taking in what it missed
    /// says nothing about what the session does now.
    pub current: bool,
    pub turn: Turn,
    pub tasks: Tasks,
}

/// What this daemon knows of its sessions.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// Whether the first adoption pass over the recorded journals has run.
    pub adopted: bool,
    pub sessions: Vec<SessionFacts>,
    /// Sidecars with a session this daemon has not managed to follow yet.
    pub unobserved: Vec<String>,
}

/// Whether two canonical roots share any of the tree: one is the other or holds it. A sidecar running in a parent or
/// a subdirectory of the project writes into the same files.
pub fn overlaps(left: &str, right: &str) -> bool {
    let within = |inner: &str, outer: &str| {
        inner == outer || outer == "/" || inner.strip_prefix(outer).is_some_and(|rest| rest.starts_with('/'))
    };
    within(left, right) || within(right, left)
}

/// The activity at `root`: how many followed sessions whose live sidecar runs over `root` are inside a turn, how many
/// background tasks they run, and whether any session there cannot be judged (a watch not current, a gap after its
/// last turn boundary or roster, a journal that could not be read, a sidecar not yet re-adopted after a restart, a
/// session not followed yet or never learned of, adoption still to come) or another installation's daemon
/// (`other_daemons`) runs a sidecar there. Callers take `unknown` as "a turn may be running".
pub fn judge(root: &str, facts: &Facts, sidecars: &[SidecarInfo], other_daemons: usize) -> ProjectActivityResponse {
    let mut unknown = other_daemons > 0;
    let mut running = 0u32;
    let mut tasks_running = 0u32;
    for sidecar in sidecars
        .iter()
        .filter(|sidecar| sidecar.alive && overlaps(&sidecar.root, root))
    {
        if !facts.adopted || !sidecar.handshake || facts.unobserved.contains(&sidecar.sidecar_id) {
            unknown = true;
        }
        for session in facts
            .sessions
            .iter()
            .filter(|session| session.sidecar == sidecar.sidecar_id && !session.closed)
        {
            match session.ended {
                Some(tonic::Code::NotFound) => continue,
                Some(_) => unknown = true,
                None if !session.current => unknown = true,
                None => {}
            }
            match session.turn {
                Turn::Running => running = running.saturating_add(1),
                Turn::Unknown => unknown = true,
                Turn::Idle => {}
            }
            match session.tasks {
                Tasks::Known(count) => tasks_running = tasks_running.saturating_add(count),
                Tasks::Unknown => unknown = true,
            }
        }
    }
    ProjectActivityResponse {
        turns_running: running,
        unknown,
        refusal: String::new(),
        tasks_running,
    }
}

/// How many other daemons' run directories (under `state/run`, all but `own_run`) record a live sidecar over `root`.
/// A record that names a live process but cannot be read counts too, since that sidecar may run anywhere; so does a
/// run directory that cannot be listed.
pub fn other_daemons_at(state: &Path, own_run: &Path, root: &str) -> usize {
    let runs = match std::fs::read_dir(state.join("run")) {
        Ok(runs) => runs,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(_) => return 1,
    };
    let mut count = 0;
    for run in runs {
        let Ok(run) = run else {
            count += 1;
            continue;
        };
        let path = run.path();
        if path == own_run || !agent::private_fs::is_private_dir(&path) {
            continue;
        }
        let there = match crate::daemon::sidecars::live_roots_in(&path) {
            Ok(roots) => roots
                .iter()
                .any(|recorded| recorded.as_deref().is_none_or(|recorded| overlaps(recorded, root))),
            Err(_) => true,
        };
        if there {
            count += 1;
        }
    }
    count
}
