//! The JSON emitted by Mutagen's list and monitor templates.

use super::engine::*;
use super::SyncError;
use serde::{Deserialize, Deserializer};

fn null_default<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Session {
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    labels: Labels,
    #[serde(default, deserialize_with = "null_default")]
    paused: bool,
    #[serde(default, deserialize_with = "null_default")]
    status: String,
    #[serde(default, deserialize_with = "null_default")]
    successful_cycles: u64,
    #[serde(default, deserialize_with = "null_default")]
    alpha: Endpoint,
    #[serde(default, deserialize_with = "null_default")]
    beta: Endpoint,
    #[serde(default, deserialize_with = "null_default")]
    conflicts: Vec<RawConflict>,
    #[serde(default, deserialize_with = "null_default")]
    excluded_conflicts: u64,
    last_error: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Endpoint {
    #[serde(default, deserialize_with = "null_default")]
    connected: bool,
    #[serde(default, deserialize_with = "null_default")]
    scanned: bool,
    #[serde(default, deserialize_with = "null_default")]
    scan_problems: Vec<RawProblem>,
    #[serde(default, deserialize_with = "null_default")]
    transition_problems: Vec<RawProblem>,
    staging_progress: Option<Progress>,
    #[serde(default, deserialize_with = "null_default")]
    excluded_scan_problems: u64,
    #[serde(default, deserialize_with = "null_default")]
    excluded_transition_problems: u64,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Progress {
    path: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    received_size: u64,
    #[serde(default, deserialize_with = "null_default")]
    expected_size: u64,
    #[serde(rename = "receivedFiles")]
    #[serde(default, deserialize_with = "null_default")]
    _received_files: u64,
    #[serde(default, deserialize_with = "null_default")]
    expected_files: u64,
    #[serde(default, deserialize_with = "null_default")]
    total_received_size: u64,
}

#[derive(Deserialize)]
struct RawProblem {
    #[serde(default, deserialize_with = "null_default")]
    path: String,
    #[serde(default, deserialize_with = "null_default")]
    error: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawConflict {
    #[serde(default, deserialize_with = "null_default")]
    root: String,
    #[serde(default, deserialize_with = "null_default")]
    alpha_changes: Vec<RawChange>,
    #[serde(default, deserialize_with = "null_default")]
    beta_changes: Vec<RawChange>,
}

#[derive(Deserialize)]
struct RawChange {
    #[serde(default, deserialize_with = "null_default")]
    path: String,
    #[serde(rename = "old")]
    _old: Option<RawEntry>,
    new: Option<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default, deserialize_with = "null_default")]
    kind: String,
    #[serde(rename = "digest")]
    _digest: Option<String>,
}

pub fn role_for_name(name: &str) -> Role {
    if name.ends_with("-git") {
        Role::Git
    } else {
        Role::Tree
    }
}

pub fn parse_sessions(bytes: &[u8]) -> Result<Vec<SessionStatus>, SyncError> {
    let sessions: Vec<Session> =
        serde_json::from_slice(bytes).map_err(|e| SyncError::Engine(format!("invalid Mutagen status: {e}")))?;
    sessions.into_iter().map(convert).collect()
}

fn convert(raw: Session) -> Result<SessionStatus, SyncError> {
    let role = match raw.labels.get("eitri-role").map(String::as_str) {
        Some("tree") => Role::Tree,
        Some("git") => Role::Git,
        Some(other) => return Err(SyncError::Engine(format!("unknown Mutagen session role: {other}"))),
        None => role_for_name(&raw.name),
    };
    let alpha_side = role.alpha_side();
    let mut problems = Vec::new();
    let mut staging = None;
    for (endpoint, side) in [(&raw.alpha, alpha_side), (&raw.beta, alpha_side.other())] {
        for problem in endpoint.scan_problems.iter().chain(&endpoint.transition_problems) {
            problems.push(Problem {
                path: problem.path.clone(),
                side,
                text: problem.error.clone(),
            });
        }
        if let Some(progress) = &endpoint.staging_progress {
            staging = Some(Staging {
                side,
                expected_bytes: progress.expected_size,
                received_bytes: progress.received_size,
                total_received_bytes: progress.total_received_size,
                expected_files: progress.expected_files,
                path: progress.path.clone(),
            });
        }
    }
    let state = if raw.paused {
        SyncState::Paused
    } else {
        match raw.status.as_str() {
            "watching" if !raw.alpha.connected || !raw.beta.connected => {
                SyncState::Disconnected(raw.last_error.clone().unwrap_or_else(|| "endpoint disconnected".into()))
            }
            "watching" if !raw.alpha.scanned || !raw.beta.scanned || raw.successful_cycles == 0 => SyncState::Scanning,
            "watching" => SyncState::Watching,
            "scanning" | "waiting-for-rescan" => SyncState::Scanning,
            "staging-alpha" | "staging-beta" => SyncState::Staging,
            "transitioning" | "saving" | "reconciling" => SyncState::Applying,
            "connecting-alpha" | "connecting-beta" => SyncState::Connecting,
            "disconnected" => SyncState::Disconnected(raw.last_error.clone().unwrap_or(raw.status.clone())),
            status if status.starts_with("halted-") => SyncState::Halted(status.into()),
            status => SyncState::Other(status.into()),
        }
    };
    let conflicts = raw
        .conflicts
        .into_iter()
        .map(|conflict| {
            let alpha = root_kind(&conflict.root, &conflict.alpha_changes);
            let beta = root_kind(&conflict.root, &conflict.beta_changes);
            let (laptop, server) = if role == Role::Tree {
                (alpha, beta)
            } else {
                (beta, alpha)
            };
            Conflict {
                path: conflict.root,
                laptop,
                server,
            }
        })
        .collect();
    Ok(SessionStatus {
        name: raw.name,
        role,
        state,
        successful_cycles: raw.successful_cycles,
        conflicts,
        problems,
        excluded_conflicts: raw.excluded_conflicts,
        excluded_scan_problems: SideCounts::from_alpha_beta(
            role,
            raw.alpha.excluded_scan_problems,
            raw.beta.excluded_scan_problems,
        ),
        excluded_transition_problems: SideCounts::from_alpha_beta(
            role,
            raw.alpha.excluded_transition_problems,
            raw.beta.excluded_transition_problems,
        ),
        staging,
        last_error: raw.last_error.filter(|s| !s.is_empty()),
    })
}

fn entry_kind(raw: &RawEntry) -> EntryKind {
    match raw.kind.as_str() {
        "file" => EntryKind::File,
        "directory" => EntryKind::Directory,
        "symlink" => EntryKind::Symlink,
        other => EntryKind::Other(other.into()),
    }
}

fn root_kind(root: &str, changes: &[RawChange]) -> EntryKind {
    match changes.iter().find(|change| change.path == root) {
        Some(change) => change.new.as_ref().map(entry_kind).unwrap_or(EntryKind::Missing),
        // Changes below the conflict root imply a directory at that root.
        None if changes
            .iter()
            .any(|change| root.is_empty() || change.path.starts_with(&format!("{root}/"))) =>
        {
            EntryKind::Directory
        }
        None => EntryKind::Other("unknown".into()),
    }
}
