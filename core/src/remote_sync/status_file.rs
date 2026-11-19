//! A project's sync directory, `<state home>/eitri/remote/sync/<sync key>/`, and the `status.json` in it: what the
//! holder of the project's sync lock last saw, for the shim, observers and `eitri host sync status`.
//!
//! The file is written whole to a private temporary beside it and renamed over, so a reader sees the old file or the
//! new one, never a part.

use std::io;
use std::path::{Path, PathBuf};

use super::engine::{EntryKind, SessionStatus, Side, SyncState};
use super::status_schema::{read_status, ConflictEntry, ProblemEntry, StatusFile, SyncChip, STATUS_VERSION};
use super::SyncKey;
use crate::remote_project::SyncRecord;

/// `<state home>/eitri/remote/sync/<sync key>`.
pub fn sync_dir(state_home: &Path, key: &SyncKey) -> PathBuf {
    state_home.join("eitri/remote/sync").join(key.as_str())
}

pub fn status_path(dir: &Path) -> PathBuf {
    dir.join("status.json")
}

/// Creates the sync directory (0700, and its parents up to `eitri`) when it is missing.
pub fn prepare_dir(state_home: &Path, dir: &Path) -> io::Result<()> {
    agent::private_fs::create_private_dir_all(dir, &state_home.join("eitri"))
}

/// Writes `status` (0600) through a temporary and a rename.
pub fn write_status(dir: &Path, status: &StatusFile) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(status).map_err(io::Error::other)?;
    let temporary = dir.join(format!(".status-{}.tmp", uuid::Uuid::new_v4()));
    agent::private_fs::replace_private(&temporary, &status_path(dir), &bytes, agent::private_fs::Flush::File)
}

/// Reads `status.json`; a missing, unreadable or other-version file is an error naming why.
pub fn read_status_file(dir: &Path) -> Result<StatusFile, String> {
    let bytes = agent::private_fs::read_private_nofollow_bytes_capped(&status_path(dir), 16 * 1024 * 1024)
        .map_err(|error| format!("no sync status for this project ({error})"))?;
    read_status(&bytes)
}

/// What the sessions' last statuses and the record say, as `status.json` holds it. `tree`/`git` are `None` when there
/// is no such session (no git session for a root git commands are forwarded for), `notice` the line the actor shows
/// while it holds.
pub fn compose(
    host: &str,
    record: &SyncRecord,
    tree: Option<&SessionStatus>,
    git: Option<&SessionStatus>,
    notice: Option<String>,
    now_unix_ms: u64,
) -> StatusFile {
    let mut conflicts = Vec::new();
    let mut problems = Vec::new();
    for status in [tree, git].into_iter().flatten() {
        if status.role == super::engine::Role::Tree {
            conflicts.extend(status.conflicts.iter().map(|conflict| ConflictEntry {
                path: conflict.path.clone(),
                laptop_kind: kind_name(&conflict.laptop),
                server_kind: kind_name(&conflict.server),
            }));
        }
        problems.extend(status.problems.iter().map(|problem| ProblemEntry {
            path: problem.path.clone(),
            side: side_name(problem.side).into(),
            text: problem.text.clone(),
        }));
    }
    let git_ready = record.git_ready || git.is_some_and(|git| git.state == SyncState::Watching);
    let git_pct = if git_ready {
        100
    } else {
        git.and_then(|git| git.staging.as_ref())
            .filter(|staging| staging.expected_files > 0)
            .map(|staging| (staging.received_files.saturating_mul(100) / staging.expected_files).min(99) as u8)
            .unwrap_or(0)
    };
    let phase = phase(record, tree, git, git_ready);
    StatusFile {
        version: STATUS_VERSION,
        host: host.to_owned(),
        phase,
        git_ready,
        git_pct,
        cycle_running: git.is_some_and(|git| {
            matches!(
                git.state,
                SyncState::Scanning | SyncState::Staging | SyncState::Applying
            )
        }),
        conflicts,
        problems,
        copied_back: record.copied_back.clone(),
        paused: record.paused.clone(),
        written_unix_ms: now_unix_ms,
        kept_here: record.kept_here.clone(),
        notice,
        copied_back_more: record.copied_back_more,
        conflicts_more: tree.map_or(0, |tree| tree.excluded_conflicts),
        problems_more: [tree, git]
            .into_iter()
            .flatten()
            .map(|status| {
                let counts = [&status.excluded_scan_problems, &status.excluded_transition_problems];
                counts.iter().map(|c| c.laptop + c.server).sum::<u64>()
            })
            .sum(),
    }
}

/// The phase word: `paused` (a pause the person must act on, or a session Mutagen holds paused), `stopped` (a halted
/// or missing tree session, or none at all), `connecting`, `syncing`, `history` (the tree is in, the git history is
/// still arriving) or `watching`.
fn phase(record: &SyncRecord, tree: Option<&SessionStatus>, git: Option<&SessionStatus>, git_ready: bool) -> String {
    if record.paused.is_some() {
        return "paused".into();
    }
    let Some(tree) = tree else {
        return "stopped".into();
    };
    match &tree.state {
        SyncState::Paused => "paused",
        SyncState::Halted(_) => "stopped",
        SyncState::Disconnected(_) | SyncState::Connecting => "connecting",
        SyncState::Scanning | SyncState::Staging | SyncState::Applying | SyncState::Other(_) => "syncing",
        SyncState::Watching if git.is_some() && !git_ready => "history",
        SyncState::Watching => "watching",
    }
    .into()
}

/// The chip the window's panel shows for `status`.
pub fn chip(status: &StatusFile) -> SyncChip {
    SyncChip {
        host: status.host.clone(),
        git_pct: status.git_pct,
        phase: status.phase.clone(),
        conflicts: (status.conflicts.len() as u64)
            .saturating_add(status.conflicts_more)
            .min(u32::MAX.into()) as u32,
        paused_reason: status.paused.as_ref().map(|pause| pause.detail.clone()),
    }
}

fn kind_name(kind: &EntryKind) -> String {
    match kind {
        EntryKind::File => "file".into(),
        EntryKind::Directory => "directory".into(),
        EntryKind::Symlink => "symlink".into(),
        EntryKind::Missing => "missing".into(),
        EntryKind::Other(other) => other.clone(),
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Laptop => "laptop",
        Side::Server => "server",
    }
}

/// Milliseconds since the epoch, 0 before it.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_project::PauseRecord;
    use crate::remote_sync::engine::{Conflict, Problem, Role, Staging};

    pub(crate) fn session(name: &str, role: Role, state: SyncState) -> SessionStatus {
        let mut status = SessionStatus::disconnected(name, role, "x");
        status.state = state;
        status.last_error = None;
        status
    }

    #[test]
    fn status_file_is_written_atomically_on_change() {
        let state = agent::state_dirs::test_workspace_dir("sync-status-file");
        let dir = sync_dir(&state, &SyncKey::new("SHA256:example", Path::new("/srv/project")));
        prepare_dir(&state, &dir).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        let record = SyncRecord::default();
        let mut tree = session("t", Role::Tree, SyncState::Watching);
        tree.conflicts.push(Conflict {
            path: "a.txt".into(),
            laptop: EntryKind::File,
            server: EntryKind::Missing,
        });
        tree.problems.push(Problem {
            path: "link".into(),
            side: Side::Server,
            text: "invalid symbolic link: target is absolute".into(),
        });
        tree.excluded_conflicts = 7;
        let first = compose("example-host", &record, Some(&tree), None, None, 1);
        assert_eq!(chip(&first).conflicts, 8);
        write_status(&dir, &first).unwrap();
        let path = status_path(&dir);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let ino = std::fs::metadata(&path).unwrap().ino();
        assert_eq!(read_status_file(&dir).unwrap(), first);
        assert_eq!(first.conflicts[0].server_kind, "missing");
        assert_eq!(first.problems[0].side, "server");
        assert_eq!(first.phase, "watching");
        // A rewrite is a new file renamed over the old one, and nothing is left beside it.
        let second = compose(
            "example-host",
            &record,
            None,
            None,
            Some("sync not started: x".into()),
            2,
        );
        write_status(&dir, &second).unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), ino);
        assert_eq!(read_status_file(&dir).unwrap(), second);
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("status.json")]);
        assert_eq!(second.phase, "stopped");
    }

    #[test]
    fn phase_follows_pause_history_and_session_states() {
        let mut record = SyncRecord::default();
        let tree = session("t", Role::Tree, SyncState::Watching);
        let mut git = session("g", Role::Git, SyncState::Staging);
        git.staging = Some(Staging {
            side: Side::Laptop,
            expected_bytes: 0,
            received_bytes: 0,
            total_received_bytes: 0,
            received_files: 30,
            expected_files: 60,
            path: None,
        });
        let status = compose("h", &record, Some(&tree), Some(&git), None, 0);
        assert_eq!(
            (status.phase.as_str(), status.git_pct, status.cycle_running),
            ("history", 50, true)
        );
        git.state = SyncState::Watching;
        git.staging = None;
        let status = compose("h", &record, Some(&tree), Some(&git), None, 0);
        assert_eq!(
            (status.phase.as_str(), status.git_pct, status.git_ready),
            ("watching", 100, true)
        );
        for (state, phase) in [
            (SyncState::Halted("halted-on-root-emptied".into()), "stopped"),
            (SyncState::Disconnected("gone".into()), "connecting"),
            (SyncState::Staging, "syncing"),
            (SyncState::Paused, "paused"),
        ] {
            let tree = session("t", Role::Tree, state);
            assert_eq!(compose("h", &record, Some(&tree), None, None, 0).phase, phase);
        }
        record.paused = Some(PauseRecord {
            reason: "tripwire".into(),
            detail: "sync paused: big on h would send 70 MiB to this laptop".into(),
            since_unix_ms: 0,
            path: Some("big".into()),
        });
        let status = compose("h", &record, Some(&tree), None, None, 0);
        assert_eq!(status.phase, "paused");
        let chip = chip(&status);
        assert_eq!(chip.phase, "paused");
        assert!(chip.paused_reason.unwrap().contains("70 MiB"));
    }

    /// The sync directory's state files go through `replace_private`: each is 0600, no temporary is left, and the
    /// status and a re-creation in progress are flushed before their rename while an editor's report and the
    /// ignore list are not.
    #[test]
    fn sync_state_files_are_replaced_whole_with_their_flush() {
        use crate::remote_sync::{quiet, recreate};
        use agent::private_fs::replace_test_hooks::syncs;
        use std::os::unix::fs::MetadataExt;
        let state = agent::state_dirs::test_workspace_dir("sync-state-files");
        let dir = sync_dir(&state, &SyncKey::new("SHA256:example", Path::new("/srv/project")));
        prepare_dir(&state, &dir).unwrap();
        let flushes = |write: &dyn Fn() -> io::Result<()>| {
            let before = syncs();
            write().unwrap();
            syncs() - before
        };
        let status = compose("example-host", &SyncRecord::default(), None, None, None, 1);
        assert_eq!(flushes(&|| write_status(&dir, &status)), 1, "the status");
        let progress = recreate::Progress {
            why: "the ignores changed".into(),
            one_sided: None,
        };
        assert_eq!(
            flushes(&|| recreate::write_progress(&dir, &progress)),
            1,
            "the progress"
        );
        let patterns = ["/target/".to_owned()];
        assert_eq!(
            flushes(&|| recreate::write_created_patterns(&dir, &patterns)),
            0,
            "the ignores"
        );
        let report = quiet::EditorReport {
            pid: 4242,
            written_unix_ms: 1,
            answer: None,
        };
        assert_eq!(flushes(&|| quiet::write_report(&state, &dir, &report)), 0, "the report");
        let editors = quiet::editors_dir(&dir);
        for path in [
            status_path(&dir),
            dir.join("recreate.json"),
            dir.join("ignores.json"),
            editors.join("4242.json"),
        ] {
            assert_eq!(
                std::fs::metadata(&path).unwrap().mode() & 0o777,
                0o600,
                "{}",
                path.display()
            );
        }
        for listed in [&dir, &editors] {
            let temporaries: Vec<_> = std::fs::read_dir(listed)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| name.to_string_lossy().ends_with(".tmp"))
                .collect();
            assert!(temporaries.is_empty(), "{temporaries:?} left in {}", listed.display());
        }
    }
}
