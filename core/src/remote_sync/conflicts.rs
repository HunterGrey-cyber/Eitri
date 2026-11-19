//! File conflict actions operate on complete versions and keep the bytes displaced by each decision.
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use agent::{private_fs, remote_client as wire};
use sha2::{Digest, Sha256};

use super::sessions::SyncHelper;
use super::status_schema::ConflictEntry;
use super::{status_file::now_unix_ms, SyncKey};
use crate::project_trust::nofollow::{DirHandle, Refusal};
use crate::remote_project::RemoteProject;
use crate::turn_review::write::{compare_and_remove, compare_and_replace, read_current_within, Current, ProjectDir};

pub const FILE_LIMIT: u64 = 64 << 20;
pub const MERGE_LIMIT: u64 = 8 << 20;
const RETENTION_MS: u64 = 30 * 24 * 3600 * 1000;

pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn check_rel(rel: &str) -> Result<(), String> {
    if rel.len() > 4096
        || crate::agent_bridge::path_holds_control(rel)
        || rel
            .split('/')
            .any(|p| matches!(p, "" | "." | "..") || p.eq_ignore_ascii_case(".git"))
    {
        return Err(format!(
            "invalid conflict path {rel:?}: use a normal relative path inside the project"
        ));
    }
    Ok(())
}

fn backups(state: &Path, project: &RemoteProject) -> PathBuf {
    state
        .join("eitri/remote/conflict-backups")
        .join(SyncKey::new(&project.host_key_sha256, &project.root).as_str())
}

fn backup(state: &Path, project: &RemoteProject, rel: &str, side: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    check_rel(rel)?;
    // UTC epoch milliseconds make expiry independent of the local timezone. Two versions can arrive in one tick.
    let stamp = format!("{}-{}-{side}", now_unix_ms(), uuid::Uuid::new_v4());
    let path = backups(state, project).join(stamp).join(rel);
    let save = || -> io::Result<()> {
        let dir = path.parent().unwrap();
        private_fs::create_private_dir_all(dir, &state.join("eitri"))?;
        let parent = private_fs::open_dir(dir)?;
        let name = std::ffi::CString::new(path.file_name().unwrap().as_bytes())?;
        let mut file = private_fs::create_private_file_at(parent.as_fd(), &name)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        parent.sync_all()?;
        for parent in dir.ancestors().skip(1).take_while(|p| p.starts_with(state)) {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    };
    save().map_err(|e| format!("could not back up {rel} at {}: {e}", path.display()))?;
    Ok(path)
}

pub fn sweep(state: &Path) {
    let root = state.join("eitri/remote/conflict-backups");
    if !root.exists()
        || private_fs::create_private_dir_all(&root, &state.join("eitri")).is_err()
        || !private_fs::is_private_dir(&root)
    {
        return;
    }
    let cutoff = now_unix_ms().saturating_sub(RETENTION_MS);
    let Ok(keys) = std::fs::read_dir(root) else { return };
    for key in keys.flatten() {
        if key.file_type().is_ok_and(|kind| kind.is_dir()) && private_fs::is_private_dir(&key.path()) {
            sweep_versions(&key.path(), cutoff);
        }
    }
}

fn sweep_versions(root: &Path, cutoff: u64) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((stamp, id)) = name.to_str().and_then(|s| s.split_once('-')) else {
            continue;
        };
        let id = id
            .strip_suffix("-laptop")
            .or_else(|| id.strip_suffix("-server"))
            .unwrap_or(id);
        if stamp.parse::<u64>().is_ok_and(|ms| ms < cutoff)
            && uuid::Uuid::parse_str(id).is_ok()
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                eprintln!("[sync] could not expire conflict backup {name:?}: {e}");
            }
        }
    }
}

pub fn try_action_lock(dir: &Path) -> io::Result<Option<private_fs::PrivateLock>> {
    let lock = private_fs::PrivateLock::open(&dir.join("action.lock"))?;
    Ok(lock.try_lock()?.then_some(lock))
}

pub fn action_lock(dir: &Path) -> Result<private_fs::PrivateLock, String> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(lock) = try_action_lock(dir).map_err(|e| e.to_string())? {
            return Ok(lock);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("sync is updating; look again".into());
        }
        std::thread::sleep(left.min(Duration::from_millis(25)));
    }
}

pub fn idle(helper: &dyn SyncHelper, root: &Path) -> Result<(), String> {
    let activity = helper
        .project_activity(root)
        .map_err(|why| format!("resolve after the turn (activity unavailable: {why})"))?;
    if !activity.refusal.is_empty() {
        return Err(format!("resolve after the turn ({})", activity.refusal));
    }
    if activity.unknown || activity.turns_running > 0 || activity.tasks_running > 0 {
        return Err("resolve after the turn".into());
    }
    Ok(())
}

pub enum Action<'a> {
    KeepServer,
    KeepLaptop,
    ShowServer {
        with_hashes: bool,
    },
    Merge {
        expected: &'a str,
        laptop_sha: Option<&'a str>,
        bytes: &'a [u8],
    },
}

fn manual(project: &RemoteProject, rel: &str, kind: &str) -> String {
    let path = project.root.join(rel);
    format!(
        "{} and {}:{}: settle it in the terminal on {} or in this folder; Eitri does not act on {kind}",
        path.display(),
        project.host,
        path.display(),
        project.host
    )
}

fn confirm_change(done: bool, refusal: &str, changed: &str) -> Result<(), String> {
    if done && refusal.is_empty() {
        Ok(())
    } else if refusal.contains("changed on the server") {
        Err(format!("{changed} ({refusal})"))
    } else if refusal.is_empty() {
        Err("the helper did not confirm the file change".into())
    } else {
        Err(refusal.to_owned())
    }
}

fn server_bytes(
    helper: &dyn SyncHelper,
    project: &RemoteProject,
    rel: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, String> {
    let chunks = helper.read_file(wire::ReadFileRequest {
        root: project.root.to_str().ok_or("root is not UTF-8")?.into(),
        rel: rel.into(),
        limit,
    })?;
    let (last, data) = chunks.split_last().ok_or("server read ended without a file kind")?;
    let mut bytes = Vec::new();
    for part in &chunks {
        if !part.refusal.is_empty() {
            return Err(format!(
                "{} ({})",
                manual(project, rel, "an unreadable or oversized file"),
                part.refusal
            ));
        }
        if part.data.len() > (limit as usize).saturating_sub(bytes.len()) {
            return Err(manual(project, rel, "a file over the size limit"));
        }
        bytes.extend_from_slice(&part.data);
    }
    if data.iter().any(|part| !part.kind.is_empty() || !part.sha256.is_empty()) {
        return Err("server read returned multiple file versions".into());
    }
    match last.kind.as_str() {
        "file" if hash(&bytes) == last.sha256 => Ok(Some(bytes)),
        "file" => Err("the server read did not match its digest".into()),
        "missing" if bytes.is_empty() => Ok(None),
        kind => Err(manual(project, rel, kind)),
    }
}

pub enum ConflictState {
    Listed,
    Cleared,
}

pub fn apply(
    state: &Path,
    project: &RemoteProject,
    conflict: &ConflictEntry,
    helper: &dyn SyncHelper,
    action: Action<'_>,
    real_root: &Path,
    guard: impl Fn() -> Result<ConflictState, String>,
) -> Result<Vec<u8>, String> {
    let rel = &conflict.path;
    check_rel(rel)?;
    for kind in [&conflict.laptop_kind, &conflict.server_kind] {
        if !matches!(kind.as_str(), "file" | "missing") {
            return Err(manual(project, rel, kind));
        }
    }
    if conflict.laptop_kind == "missing" && conflict.server_kind == "missing" {
        return Err(manual(project, rel, "two missing files"));
    }
    let require_conflict = || match guard()? {
        ConflictState::Listed => Ok(()),
        ConflictState::Cleared => Err(format!("{rel} is no longer in the same conflict; look again")),
    };
    require_conflict()?;
    let held = DirHandle::open_absolute(real_root).map_err(|e| manual(project, rel, &e.describe()))?;
    let mirror = ProjectDir::from_fd(held.into_fd()).map_err(|e| e.to_string())?;
    let target = mirror
        .target(Path::new(rel), false)
        .map_err(|e| manual(project, rel, &e.to_string()))?;
    let limit = if matches!(action, Action::ShowServer { .. } | Action::Merge { .. }) {
        MERGE_LIMIT
    } else {
        FILE_LIMIT
    };
    let laptop_limit = if matches!(action, Action::ShowServer { with_hashes: false }) {
        FILE_LIMIT
    } else {
        limit
    };
    let local = match read_current_within(&target, laptop_limit).map_err(|e| e.to_string())? {
        Current::Regular { bytes, .. } if bytes.len() as u64 <= laptop_limit => Some(bytes),
        Current::Regular { .. } => {
            return Err(manual(
                project,
                rel,
                if laptop_limit == MERGE_LIMIT {
                    "a file over 8 MiB"
                } else {
                    "a file over 64 MiB"
                },
            ))
        }
        Current::Symlink { .. } => return Err(manual(project, rel, "a symlink")),
        Current::Other => return Err(manual(project, rel, "a directory or special file")),
        Current::Absent => None,
    };
    let changed = || format!("{rel} changed on {} since you looked; look again", project.host);
    if local.is_some() != (conflict.laptop_kind == "file") {
        return Err(manual(project, rel, "a kind change"));
    }
    let root = project.root.to_str().ok_or("root is not UTF-8")?.to_owned();
    let server = server_bytes(helper, project, rel, limit)?;
    if !matches!(action, Action::Merge { .. }) && server.is_some() != (conflict.server_kind == "file") {
        return Err(manual(project, rel, "a kind change"));
    }
    let local_hash = local.as_deref().map(hash).unwrap_or_default();
    let server_hash = server.as_deref().map(hash).unwrap_or_default();
    let mut saved = Vec::new();
    match action {
        Action::ShowServer { with_hashes } => {
            if with_hashes {
                // Stdout carries the original bytes, including NULs older editors cannot hash.
                eprintln!(
                    "eitri-sync-hashes {}",
                    serde_json::json!({"laptop":local_hash, "server":server_hash})
                );
            }
            return Ok(server.unwrap_or_default());
        }
        Action::KeepServer => {
            let bytes = local
                .as_deref()
                .ok_or_else(|| format!("{rel} is already missing on the laptop; nothing to remove"))?;
            saved.push(backup(state, project, rel, "laptop", bytes)?);
        }
        Action::KeepLaptop => {
            let bytes = server
                .as_deref()
                .ok_or_else(|| format!("{rel} is already missing on {}; nothing to remove", project.host))?;
            saved.push(backup(state, project, rel, "server", bytes)?);
        }
        Action::Merge { laptop_sha, bytes, .. } => {
            if laptop_sha.is_some_and(|sha| sha != local_hash) {
                return Err(format!("{rel} changed on the laptop since you looked; look again"));
            }
            if bytes.len() as u64 > MERGE_LIMIT {
                return Err(manual(project, rel, "a merge over 8 MiB"));
            }
            for (side, version) in [("laptop", &local), ("server", &server)] {
                if let Some(original) = version {
                    saved.push(backup(state, project, rel, side, original)?);
                }
            }
        }
    }

    let local_digest = local.as_deref().map(|b| Sha256::digest(b).into());
    let _lock = action_lock(&super::actor::lock_dir(state, &project.root))?;
    require_conflict()?;
    match action {
        Action::KeepServer => compare_and_remove(&target, local_digest.expect("laptop backed up"))
            .map_err(|e| format!("{rel} on the laptop: {e}; look again"))?,
        Action::KeepLaptop => {
            let reply = helper.remove_file_if(wire::RemoveFileIfRequest {
                root,
                rel: rel.clone(),
                expected_sha256: server_hash,
            })?;
            confirm_change(reply.removed, &reply.refusal, &changed())?;
        }
        Action::Merge { expected, bytes, .. } => {
            let target = mirror
                .target(Path::new(rel), local.is_none())
                .map_err(|e| e.to_string())?;
            if local.as_deref() == Some(bytes) {
                // A retry can already be saved locally. Verify the path again without replacing that inode.
                match read_current_within(&target, MERGE_LIMIT).map_err(|e| e.to_string())? {
                    Current::Regular { bytes: current, .. } if current == bytes => {}
                    _ => return Err(format!("{rel} changed on the laptop since you looked; look again")),
                }
            } else {
                eprintln!("eitri-sync-laptop-writing");
                compare_and_replace(&target, local_digest, bytes)
                    .map_err(|e| format!("{rel} could not be written on the laptop: {e}"))?;
            }
            eprintln!("eitri-sync-laptop-written {}", hash(bytes));
            let finish = || {
                if matches!(guard()?, ConflictState::Cleared) {
                    return if server_bytes(helper, project, rel, MERGE_LIMIT)?.as_deref() == Some(bytes) {
                        Ok(())
                    } else {
                        Err(changed())
                    };
                }
                if server_hash != expected {
                    return Err(changed());
                }
                let reply = helper.replace_file_if(wire::ReplaceFileIfRequest {
                    root,
                    rel: rel.clone(),
                    expected_sha256: expected.into(),
                    data: bytes.to_vec(),
                })?;
                confirm_change(reply.replaced, &reply.refusal, &changed())
            };
            finish().map_err(|why| format!("laptop file written; {why}"))?;
        }
        Action::ShowServer { .. } => unreachable!("server bytes already returned"),
    }
    Ok(saved
        .iter()
        .map(|path| format!("Backup: {}\n", path.display()))
        .collect::<String>()
        .into_bytes())
}

pub fn inspect(
    project: &RemoteProject,
    real_root: &Path,
    c: &ConflictEntry,
    server: Option<&wire::ListTreeEntry>,
) -> serde_json::Value {
    use crate::project_trust::nofollow::FileKind;
    let check = || -> Result<(u64, u64), String> {
        check_rel(&c.path)?;
        if c.laptop_kind == "missing" && c.server_kind == "missing" {
            return Err(manual(project, &c.path, "two missing files"));
        }
        let path = real_root.join(&c.path);
        let local =
            DirHandle::open_absolute(path.parent().unwrap()).and_then(|dir| dir.lstat(path.file_name().unwrap()));
        let laptop_size = match local {
            Ok(FileKind::Regular { len }) if c.laptop_kind == "file" => len,
            Err(Refusal::Missing) if c.laptop_kind == "missing" => 0,
            Ok(kind) => return Err(manual(project, &c.path, kind.describe())),
            Err(why) => return Err(manual(project, &c.path, &why.describe())),
        };
        let server_size = match server {
            Some(entry) if entry.kind == "file" && c.server_kind == "file" => entry.size,
            None if c.server_kind == "missing" => 0,
            Some(entry) => return Err(manual(project, &c.path, &entry.kind)),
            None => return Err(manual(project, &c.path, "a kind change")),
        };
        if laptop_size.max(server_size) > FILE_LIMIT {
            return Err(manual(project, &c.path, "a file over 64 MiB"));
        }
        Ok((laptop_size, server_size))
    };
    match check() {
        Ok((laptop, server)) => serde_json::json!({"merge": laptop.max(server) <= MERGE_LIMIT,
            "keep_server": c.laptop_kind == "file", "keep_laptop": c.server_kind == "file"}),
        Err(reason) => serde_json::json!({"reason":reason}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_sync::actor::tests::Lab;

    #[test]
    fn backup_rel_with_dotdot_is_refused() {
        let lab = Lab::new();
        for rel in ["a/../b", "../b", "/b", ".GIT/config", "a//b"] {
            assert!(backup(&lab.state, &lab.project(), rel, "laptop", b"text").is_err());
        }
        assert!(!backups(&lab.state, &lab.project()).exists());
    }

    #[test]
    fn becoming_holder_expires_legacy_and_labelled_backups_across_keys() {
        let lab = Lab::new();
        let project = lab.project();
        let mut other = project.clone();
        other.host_key_sha256.push_str("-previous");
        let mut expired = vec![];
        for (project, suffix) in [(&project, ""), (&other, "-laptop"), (&other, "-server")] {
            let recent = backup(&lab.state, project, "file", "laptop", b"saved").unwrap();
            let old = backups(&lab.state, project).join(format!("1-{}{suffix}", uuid::Uuid::new_v4()));
            std::fs::rename(recent.parent().unwrap(), &old).unwrap();
            expired.push(old);
        }
        let fresh = backup(&lab.state, &other, "keep", "server", b"keep").unwrap();
        let (_actor, _) = lab.synced();
        assert!(expired.iter().all(|old| !old.exists()));
        assert_eq!(std::fs::read(fresh).unwrap(), b"keep");
    }

    #[test]
    fn backups_are_0600_in_0700_and_swept_after_30_days() {
        use std::os::unix::fs::PermissionsExt;
        let lab = Lab::new();
        let project = lab.project();
        backup(&lab.state, &project, "d/file", "server", b"bytes").unwrap();
        let root = backups(&lab.state, &project);
        let stamp = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        let file = stamp.join("d/file");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        for path in file.parent().unwrap().ancestors().take_while(|p| p.starts_with(&root)) {
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700);
        }
        sweep(&lab.state);
        assert!(file.exists());
        let old = root.join(format!("{}-{}", now_unix_ms() - RETENTION_MS - 1, uuid::Uuid::new_v4()));
        std::fs::rename(stamp, &old).unwrap();
        sweep(&lab.state);
        assert!(!old.exists());
    }
}
