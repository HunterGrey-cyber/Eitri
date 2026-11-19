//! Saved remote projects and host installation identities. All disk work belongs on a worker.

mod connection;
mod origin;
pub mod terminal_facts;
pub mod terminal_link;
pub mod terminals;

pub use connection::{verified_host_key, RemoteConnection, TerminalMaster, TerminalRoute};
pub use origin::{
    carried_text, carry_tabs_to_key, check_remote_dev, control_dir, decide_origin, host_key_changed, host_open_hint,
    missing_mirror_hint, origin_key, reconcile_host_key, remote_dev_off, tab_origin_key, WindowOrigin,
};

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::state_paths::{create_state_dir, lock_store, temporary, ON_A_WRITER_THREAD};

/// A remote project's record. A field this build does not know (a later build's) is read past, so the project still
/// opens here and its stops can still be kept; a field this build needs is required, and a record missing one is
/// refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteProject {
    pub version: u32,
    pub host: String,
    pub host_key_sha256: String,
    pub root: PathBuf,
    pub helper_version: String,
    pub sync: Option<serde_json::Value>,
    pub created_at: u64,
    /// `<installation>/<sidecar id>` for tabs closed while the server could not be reached. Each leaves
    /// the list after its owner accepts the stop or every candidate confirms absence. Legacy ids are
    /// looked up across recorded installations and stay queued while any candidate cannot be asked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_stops: Vec<String>,
}

/// The sync engine's durable session identities and decisions awaiting the person.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRecord {
    pub engine: String,
    /// First connect checked this root and authorized the actor to create its sessions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub set_up: bool,
    #[serde(default)]
    pub tree_session: Option<String>,
    #[serde(default)]
    pub git_session: Option<String>,
    pub host_key_sha256: String,
    /// The digest of the effective ignore patterns (the server's translated ignores with this record's excludes and
    /// includes) the tree session was created with; a different one re-creates it.
    #[serde(default)]
    pub ignore_digest: String,
    #[serde(default)]
    pub excludes_added: Vec<String>,
    #[serde(default)]
    pub excludes_removed: Vec<String>,
    #[serde(default)]
    pub tripwire_allowed: Vec<String>,
    #[serde(default)]
    pub paused: Option<PauseRecord>,
    #[serde(default)]
    pub copied_back: Vec<String>,
    #[serde(default)]
    pub git_ready: bool,
    /// The host key the person accepted with `eitri host sync resume` after it changed: the sessions are re-created
    /// under it, and `host_key_sha256` becomes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_host_key: Option<String>,
    /// `eitri host sync allow` with no path: the tripwire lets the current batch through, until its next cycle ends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tripwire_allow_batch: bool,
    /// Whether the tree session has finished a cycle since it was first created; until then the size tripwire leaves
    /// it alone (the person was shown the first sync's size).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tree_synced: bool,
    /// How many more paths than `copied_back` holds were copied back by the last re-creation.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub copied_back_more: u64,
    /// Paths the last ignore change left out of sync while this laptop still holds them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept_here: Vec<String>,
    /// The laptop directory this record's sessions sync: the root as first connect prepared it (or as it was when
    /// the first session was reserved). A folder at the same path that is not this one (the mirror moved away and
    /// another checkout put there, a restore from a backup) is never synced over: its contents are not the ones the
    /// sessions' history describes. A record written before this field existed has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_identity: Option<DirIdentity>,
    /// The laptop `.git` Eitri made for the replica. Only this directory is written to or replicated into; a `.git`
    /// the person made in the mirror is left alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_identity: Option<DirIdentity>,
    /// The version of the git session's description the recorded git session was created with; an older one is
    /// re-created, since Mutagen keeps a session's ignores for its whole life.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub git_spec_version: u32,
}

/// A directory as the file system names it: device, inode and birth time. Device and inode alone are not enough: a
/// file system such as ext4 hands a freed inode number to the next directory made, so a folder deleted and made again
/// at the same path (`rm -rf` and a fresh clone) often gets the old pair back. Its birth time is new, though. Compare
/// two identities with `remote_sync::mirror_identity::compare`, never with `==`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirIdentity {
    pub dev: u64,
    pub ino: u64,
    /// `Unknown` in a record written before birth times were kept.
    #[serde(default)]
    pub birth: Birth,
}

/// A directory's birth time as the file system reported it when its identity was taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Birth {
    /// Not measured: the identity was recorded before birth times were kept.
    #[default]
    Unknown,
    /// The file system reports none.
    Absent,
    At {
        sec: i64,
        nsec: u32,
    },
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

impl SyncRecord {
    pub(crate) fn has_sessions(&self) -> bool {
        self.tree_session.is_some() || self.git_session.is_some()
    }

    pub(crate) fn authorized(&self) -> bool {
        self.set_up || self.has_sessions()
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A pause remains visible across restarts until the person settles it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseRecord {
    pub reason: String,
    pub detail: String,
    pub since_unix_ms: u64,
    /// The path a tripwire pause names (relative to the root), which `eitri host sync allow` and `exclude` take
    /// when given none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl RemoteProject {
    pub fn sync_record(&self) -> Result<Option<SyncRecord>, String> {
        self.sync
            .as_ref()
            .map(|value| {
                serde_json::from_value(value.clone()).map_err(|error| {
                    format!(
                        "the sync record for {} on {} is unreadable: {error}",
                        self.root.display(),
                        self.host
                    )
                })
            })
            .transpose()
    }

    pub fn sync_not_set_up(&self) -> String {
        format!(
            "sync is not set up for {}: run eitri host open {}:{} to set up this machine's copy",
            self.root.display(),
            self.host,
            self.root.display()
        )
    }

    pub fn set_sync_record(&mut self, record: &SyncRecord) {
        self.sync = Some(serde_json::to_value(record).expect("sync records contain only JSON values"));
    }

    pub fn same_identity(&self, other: &Self) -> bool {
        self.root == other.root && self.host == other.host && self.host_key_sha256 == other.host_key_sha256
    }
}

pub fn project_key(root: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(b"eitri-remote-project\0");
    hash.update(root.as_os_str().as_bytes());
    format!("{:x}", hash.finalize())[..16].to_owned()
}

/// A server's installation record, read as [`RemoteProject`] is: past a later build's field, never without one this
/// build needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHost {
    pub version: u32,
    pub host_key_sha256: String,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub helpers: BTreeMap<String, String>,
    pub sidecars: BTreeMap<String, String>,
    pub consented: bool,
}

/// A tab's origin is fixed when it starts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabOrigin {
    #[default]
    Local,
    Remote {
        host: String,
        host_key_sha256: String,
        root: PathBuf,
    },
}

pub struct RemoteProjects {
    dir: Option<PathBuf>,
}

impl RemoteProjects {
    pub fn new(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> RemoteProjects {
        Self {
            dir: crate::state_paths::state_subdir(xdg_state_home, home, "hosts"),
        }
    }

    /// Where the windows of these projects keep their saved tabs: `<state home>/eitri/tabs`, beside the records.
    pub fn saved_tabs_dir(&self) -> Option<PathBuf> {
        Some(self.dir.as_deref()?.parent()?.join("tabs"))
    }

    /// The state home these records are kept under (`<state home>/eitri/hosts`'s grandparent): where sync keeps
    /// its directories and Mutagen's data.
    pub fn state_home(&self) -> Option<PathBuf> {
        Some(self.dir.as_deref()?.parent()?.parent()?.to_owned())
    }

    fn dir(&self) -> io::Result<&Path> {
        self.dir
            .as_deref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no absolute state home or home directory"))
    }

    fn project_path(&self, root: &Path) -> io::Result<PathBuf> {
        let bytes = root.as_os_str().as_bytes();
        let normal = bytes == b"/"
            || (root.is_absolute()
                && bytes[1..]
                    .split(|byte| *byte == b'/')
                    .all(|part| !part.is_empty() && part != b"." && part != b".."));
        if !normal {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "remote project root must be absolute and in normal form: {}",
                    root.display()
                ),
            ));
        }
        Ok(self.dir()?.join(format!("{}.json", project_key(root))))
    }

    fn host_path(&self, key: &str) -> io::Result<PathBuf> {
        let digest = Sha256::digest(key.as_bytes());
        Ok(self.dir()?.join(format!("host-{}.json", &format!("{digest:x}")[..16])))
    }

    pub fn find(&self, root: &Path) -> io::Result<Option<RemoteProject>> {
        let path = self.project_path(root)?;
        let record: Option<RemoteProject> = read(&path)?;
        if let Some(record) = &record {
            version(&path, record.version)?;
            if record.root != root {
                return Err(invalid(&path, "record root does not match the requested root"));
            }
        }
        Ok(record)
    }

    pub fn save(&self, record: &RemoteProject) -> io::Result<()> {
        let path = self.project_path(&record.root)?;
        version(&path, record.version)?;
        let _lock = self.lock(&path)?;
        self.write(&path, record)
    }

    /// Commits an open without replacing stop updates made while it connected, uploaded or asked for consent.
    pub fn save_opened(&self, opened: &RemoteProject) -> io::Result<RemoteProject> {
        let path = self.project_path(&opened.root)?;
        let _lock = self.lock(&path)?;
        let mut record = opened.clone();
        if let Some(latest) = self.find(&opened.root)?.filter(|latest| latest.host == opened.host) {
            record.queued_stops.clone_from(&latest.queued_stops);
            record.created_at = latest.created_at;
            if latest.host_key_sha256 == opened.host_key_sha256 {
                if let Some(sync) = latest.sync_record().map_err(io::Error::other)? {
                    if sync.host_key_sha256 == opened.host_key_sha256 {
                        record.set_sync_record(&sync);
                    }
                }
            }
        }
        version(&path, record.version)?;
        self.write(&path, &record)?;
        Ok(record)
    }

    /// Reads, changes and replaces a project record under its store lock: the same rules as
    /// [`Self::update_host`]. `NotFound` when there is no record for `root`. Disk work belongs on a worker.
    pub fn update(&self, root: &Path, update: impl FnOnce(&mut RemoteProject)) -> io::Result<RemoteProject> {
        self.update_checked(root, |record| {
            update(record);
            Ok(())
        })
    }

    /// A window may update its launch host's record, never a replacement host's record at the same root.
    pub(crate) fn update_for_host(
        &self,
        root: &Path,
        host: &str,
        update: impl FnOnce(&mut RemoteProject),
    ) -> io::Result<RemoteProject> {
        self.update_checked(root, |record| {
            if record.host != host {
                return Err(io::Error::other(
                    "the project now points at another server; reopen the window",
                ));
            }
            update(record);
            Ok(())
        })
    }

    pub(crate) fn update_checked(
        &self,
        root: &Path,
        update: impl FnOnce(&mut RemoteProject) -> io::Result<()>,
    ) -> io::Result<RemoteProject> {
        let path = self.project_path(root)?;
        let _lock = self.lock(&path)?;
        let mut record = self.find(root)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no remote project at {}", root.display()),
            )
        })?;
        update(&mut record)?;
        version(&path, record.version)?;
        if record.root != root {
            return Err(invalid(&path, "record root does not match the requested root"));
        }
        self.write(&path, &record)?;
        Ok(record)
    }

    /// Keeps `sidecar_id` to be stopped once the project's server can be reached.
    pub fn queue_stop(&self, root: &Path, sidecar_id: &str) -> io::Result<()> {
        self.update(root, |record| {
            if !record.queued_stops.iter().any(|queued| queued == sidecar_id) {
                record.queued_stops.push(sidecar_id.to_string());
            }
        })
        .map(|_| ())
    }

    /// `sidecar_id`'s stop was sent: it leaves the queue.
    pub fn stop_sent(&self, root: &Path, sidecar_id: &str) -> io::Result<()> {
        self.update(root, |record| record.queued_stops.retain(|queued| queued != sidecar_id))
            .map(|_| ())
    }

    /// Reads stops only for the server identity that queued them. Host includes the SSH user and port as typed.
    pub fn queued_stops_for(&self, project: &RemoteProject) -> io::Result<Vec<String>> {
        match self.find(&project.root)? {
            Some(record) if record.same_identity(project) => Ok(record.queued_stops),
            Some(_) => Err(io::Error::other(
                "the project now points at another server; reopen the window",
            )),
            None => Ok(Vec::new()),
        }
    }

    /// Writes or acknowledges a stop only while its owning project identity still holds the root's record.
    pub fn write_stop_for(&self, root: &Path, host: &str, key: &str, id: &str, keep: bool) -> io::Result<()> {
        let path = self.project_path(root)?;
        let _lock = self.lock(&path)?;
        let mut record = self
            .find(root)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no remote project record"))?;
        if record.host != host || record.host_key_sha256 != key {
            return Err(io::Error::other(
                "the project now points at another server; reopen the window",
            ));
        }
        if keep {
            if !record.queued_stops.iter().any(|queued| queued == id) {
                record.queued_stops.push(id.to_owned());
            }
        } else {
            record.queued_stops.retain(|queued| queued != id);
        }
        self.write(&path, &record)
    }

    /// Removes `root`'s record under its store lock when `remove` says so of the record as it is then.
    pub(crate) fn forget_if(&self, root: &Path, remove: impl FnOnce(&RemoteProject) -> bool) -> io::Result<bool> {
        let path = self.project_path(root)?;
        let _lock = self.lock(&path)?;
        match self.find(root)? {
            Some(record) if remove(&record) => match std::fs::remove_file(&path) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            },
            _ => Ok(false),
        }
    }

    /// Removes `root`'s record, under its store lock: an [`Self::update`] that read the record first finishes its
    /// write before the record goes, rather than writing it back after.
    pub fn forget(&self, root: &Path) -> io::Result<bool> {
        let path = self.project_path(root)?;
        let _lock = self.lock(&path)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Every project record, oldest first, and a line for each file that could not be read. No state home is no
    /// records. Disk work belongs on a worker.
    pub fn list(&self) -> io::Result<(Vec<RemoteProject>, Vec<String>)> {
        let Ok(dir) = self.dir() else {
            return Ok((Vec::new(), Vec::new()));
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
            Err(e) => return Err(e),
        };
        let mut projects = Vec::new();
        let mut unreadable = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let is_project = name.len() == 21
                && name.ends_with(".json")
                && name[..16]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !is_project {
                continue;
            }
            match read::<RemoteProject>(&path).and_then(|record| {
                let record = record.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "removed meanwhile"))?;
                version(&path, record.version)?;
                if self.project_path(&record.root)? != path {
                    return Err(invalid(&path, "record root does not match its file name"));
                }
                Ok(record)
            }) {
                Ok(record) => projects.push(record),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => unreadable.push(format!("{}: {e}", path.display())),
            }
        }
        projects.sort_by(|a, b| (a.created_at, &a.root).cmp(&(b.created_at, &b.root)));
        Ok((projects, unreadable))
    }

    /// Removes the host record kept under `host_key_sha256`; `false` when there was none.
    pub fn forget_host(&self, host_key_sha256: &str) -> io::Result<bool> {
        match std::fs::remove_file(self.host_path(host_key_sha256)?) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn host(&self, host_key_sha256: &str) -> io::Result<Option<RemoteHost>> {
        let path = self.host_path(host_key_sha256)?;
        let record: Option<RemoteHost> = read(&path)?;
        if let Some(record) = &record {
            version(&path, record.version)?;
            if record.host_key_sha256 != host_key_sha256 {
                return Err(invalid(&path, "record host key does not match the requested host key"));
            }
        }
        Ok(record)
    }

    pub fn save_host(&self, host: &RemoteHost) -> io::Result<()> {
        let path = self.host_path(&host.host_key_sha256)?;
        version(&path, host.version)?;
        let _lock = self.lock(&path)?;
        self.write(&path, host)
    }

    /// Reads, merges and replaces a host record under its store lock. Disk work belongs on a worker.
    ///
    /// `update` runs while the lock is held, and other writers give up after about two seconds: it only merges
    /// the record it is given, does no other I/O (no upload, no ssh), and never calls back into `RemoteProjects`.
    pub fn update_host(&self, key: &str, update: impl FnOnce(Option<RemoteHost>) -> RemoteHost) -> io::Result<()> {
        let path = self.host_path(key)?;
        let _lock = self.lock(&path)?;
        let host = update(self.host(key)?);
        version(&path, host.version)?;
        if host.host_key_sha256 != key {
            return Err(invalid(&path, "record host key does not match the requested host key"));
        }
        self.write(&path, &host)
    }

    fn lock(&self, path: &Path) -> io::Result<agent::private_fs::PrivateLock> {
        create_state_dir(self.dir()?)?;
        lock_store(path, ON_A_WRITER_THREAD)
    }

    fn write(&self, path: &Path, record: &impl Serialize) -> io::Result<()> {
        let bytes = serde_json::to_vec(record).map_err(|e| invalid(path, e))?;
        agent::private_fs::replace_private(
            &temporary(path),
            path,
            &bytes,
            agent::private_fs::Flush::FileAndDirectory,
        )
    }
}

fn invalid(path: &Path, error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {error}", path.display()))
}

fn version(path: &Path, version: u32) -> io::Result<()> {
    if version == 1 {
        Ok(())
    } else {
        Err(invalid(path, format!("unsupported remote record version {version}")))
    }
}

fn read<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match agent::private_fs::read_private_nofollow_capped(path, 4 * 1024 * 1024) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| invalid(path, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    fn store() -> RemoteProjects {
        let state = agent::state_dirs::test_workspace_dir("remote-project");
        RemoteProjects::new(Some(state.as_os_str()), None)
    }

    fn sync_record() -> SyncRecord {
        SyncRecord {
            engine: "mutagen".into(),
            set_up: true,
            tree_session: Some("eitri-example-tree".into()),
            git_session: Some("eitri-example-git".into()),
            host_key_sha256: "SHA256:example".into(),
            ignore_digest: "digest".into(),
            excludes_added: vec!["build".into()],
            excludes_removed: vec!["keep".into()],
            tripwire_allowed: vec!["large".into()],
            paused: Some(PauseRecord {
                reason: "tripwire".into(),
                detail: "large batch".into(),
                since_unix_ms: 123,
                path: Some("large".into()),
            }),
            copied_back: vec!["restored".into()],
            git_ready: true,
            accepted_host_key: Some("SHA256:accepted".into()),
            tripwire_allow_batch: true,
            tree_synced: true,
            copied_back_more: 3,
            kept_here: vec!["now-ignored".into()],
            root_identity: Some(DirIdentity {
                dev: 1,
                ino: 2,
                birth: Birth::At { sec: 10, nsec: 7 },
            }),
            git_identity: Some(DirIdentity {
                dev: 1,
                ino: 3,
                birth: Birth::Absent,
            }),
            git_spec_version: 2,
        }
    }

    #[test]
    fn sync_record_round_trips_and_older_records_read_without_it() {
        let store = store();
        let mut project = record();
        assert_eq!(project.sync_record().unwrap(), None);
        let mut old = serde_json::to_value(&project).unwrap();
        old.as_object_mut().unwrap().remove("sync");
        let old: RemoteProject = serde_json::from_value(old).unwrap();
        assert_eq!(old.sync_record().unwrap(), None);
        let sync = sync_record();
        project.set_sync_record(&sync);
        store.save(&project).unwrap();
        assert_eq!(
            store.find(&project.root).unwrap().unwrap().sync_record().unwrap(),
            Some(sync)
        );
        project.sync = Some(serde_json::json!({ "engine": "mutagen", "host_key_sha256": "SHA256:example" }));
        let minimal = project.sync_record().unwrap().unwrap();
        assert_eq!(minimal.tree_session, None);
        assert_eq!(minimal.git_session, None);
        assert!(minimal.ignore_digest.is_empty());
        assert!(minimal.excludes_added.is_empty());
        assert!(minimal.excludes_removed.is_empty());
        assert!(minimal.tripwire_allowed.is_empty());
        assert_eq!(minimal.paused, None);
        assert!(minimal.copied_back.is_empty());
        assert!(!minimal.git_ready);
    }

    #[test]
    fn a_record_with_an_unknown_sync_field_still_opens() {
        let store = store();
        let mut project = record();
        let sync = sync_record();
        project.set_sync_record(&sync);
        project.sync.as_mut().unwrap()["future_field"] = serde_json::json!({"nested": true});
        store.save(&project).unwrap();
        assert_eq!(
            store.find(&project.root).unwrap().unwrap().sync_record().unwrap(),
            Some(sync)
        );
    }

    #[test]
    fn an_unreadable_sync_value_is_an_error() {
        let mut project = record();
        for value in [
            serde_json::json!("wrong"),
            serde_json::json!({}),
            serde_json::json!({"engine": "mutagen"}),
            serde_json::json!({"host_key_sha256": "SHA256:example"}),
            serde_json::json!({"engine": "mutagen", "host_key_sha256": "SHA256:example", "git_ready": "yes"}),
        ] {
            project.sync = Some(value);
            let error = project.sync_record().unwrap_err();
            assert!(
                error.contains("sync record") && error.contains("example-host"),
                "{error}"
            );
        }
    }

    /// A forget waits for the store lock an update holds, so it cannot slip between that update's read and its write.
    #[test]
    fn forget_waits_for_the_store_lock() {
        let store = store();
        let record = record();
        store.save(&record).unwrap();
        let path = store.project_path(&record.root).unwrap();
        let held = store.lock(&path).unwrap();
        assert!(
            store.forget(&record.root).is_err(),
            "it gives up while another writer holds the record"
        );
        assert_eq!(store.find(&record.root).unwrap(), Some(record.clone()));
        drop(held);
        assert!(store.forget(&record.root).unwrap());
        assert_eq!(store.find(&record.root).unwrap(), None);
    }

    /// A record replaced in place survives a crash: its bytes are flushed before the rename and the directory's
    /// entry after it.
    #[test]
    fn every_record_write_is_flushed_before_and_after_its_rename() {
        let store = store();
        let record = record();
        let before = agent::private_fs::replace_test_hooks::syncs();
        store.save(&record).unwrap();
        assert_eq!(
            agent::private_fs::replace_test_hooks::syncs() - before,
            2,
            "the file, then its directory"
        );
        let before = agent::private_fs::replace_test_hooks::syncs();
        store
            .update_host(&host_record().host_key_sha256, |_| host_record())
            .unwrap();
        assert_eq!(agent::private_fs::replace_test_hooks::syncs() - before, 2);
        assert_eq!(store.find(&record.root).unwrap(), Some(record));
    }

    /// A stop queued while the server was out of reach stays in the record until it is sent, and an
    /// older record without the list reads as having none.
    #[test]
    fn a_queued_stop_is_kept_until_sent() {
        let store = store();
        let record = record();
        store.save(&record).unwrap();
        store.queue_stop(&record.root, "sc-1").unwrap();
        store.queue_stop(&record.root, "sc-1").unwrap();
        store.queue_stop(&record.root, "sc-2").unwrap();
        assert_eq!(
            store.find(&record.root).unwrap().unwrap().queued_stops,
            ["sc-1", "sc-2"]
        );
        store.stop_sent(&record.root, "sc-1").unwrap();
        assert_eq!(store.find(&record.root).unwrap().unwrap().queued_stops, ["sc-2"]);
        let older = serde_json::to_value(&record).unwrap();
        assert!(older.get("queued_stops").is_none(), "an empty list is not written");
        assert_eq!(
            serde_json::from_value::<RemoteProject>(older).unwrap().queued_stops,
            Vec::<String>::new()
        );
        assert_eq!(
            store.queue_stop(Path::new("/srv/other"), "sc-3").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    fn record() -> RemoteProject {
        RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
            helper_version: "0.3.0".into(),
            sync: None,
            created_at: 42,
            queued_stops: Vec::new(),
        }
    }

    fn host_record() -> RemoteHost {
        RemoteHost {
            version: 1,
            host_key_sha256: "SHA256:example".into(),
            data_dir: "/home/example/.local/share".into(),
            state_dir: "/home/example/.local/state".into(),
            helpers: BTreeMap::from([("0.3.0-1234567".into(), "a".repeat(64))]),
            sidecars: BTreeMap::from([("1234567".into(), "b".repeat(64))]),
            consented: true,
        }
    }

    /// The records are listed oldest first; a host record is not a project, and a damaged file is said, not fatal.
    #[test]
    fn every_project_record_is_listed() {
        let store = store();
        assert_eq!(store.list().unwrap(), (Vec::new(), Vec::new()));
        let first = record();
        let mut second = record();
        second.root = "/srv/other".into();
        second.created_at = 50;
        store.save(&second).unwrap();
        store.save(&first).unwrap();
        store.save_host(&host_record()).unwrap();
        let damaged = store
            .dir()
            .unwrap()
            .join(format!("{}.json", project_key(Path::new("/srv/third"))));
        agent::private_fs::write_private(&damaged, b"{").unwrap();
        let (projects, unreadable) = store.list().unwrap();
        assert_eq!(projects, [first, second]);
        assert_eq!(unreadable.len(), 1);
        assert!(unreadable[0].contains(&damaged.display().to_string()));
        assert!(store.forget_host("SHA256:example").unwrap());
        assert!(!store.forget_host("SHA256:example").unwrap());
        assert_eq!(
            RemoteProjects::new(None, None).list().unwrap(),
            (Vec::new(), Vec::new())
        );
    }

    #[test]
    fn remote_project_roots_must_be_absolute_and_in_normal_form() {
        let store = store();
        let record = record();
        store.save(&record).unwrap();
        for root in [
            "/srv/project/",
            "/srv/./project",
            "/srv/other/../project",
            "/srv//project",
            "//srv/project",
            "/srv/project/.",
            "srv/project",
        ] {
            let mut bad = record.clone();
            bad.root = root.into();
            for error in [
                store.find(Path::new(root)).unwrap_err(),
                store.save(&bad).unwrap_err(),
                store.forget(Path::new(root)).unwrap_err(),
            ] {
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                assert!(error.to_string().contains(root), "{error}");
            }
        }
        assert_eq!(store.find(&record.root).unwrap(), Some(record));
        assert!(store.find(Path::new("/")).unwrap().is_none());
    }

    #[test]
    fn interrupted_project_replacement_keeps_the_live_record() {
        let store = store();
        let mut record = record();
        store.save(&record).unwrap();
        let path = store.project_path(&record.root).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut live = std::fs::File::open(&path).unwrap();
        let temporary = crate::state_paths::temporary(&path);
        agent::private_fs::write_private(&temporary, b"{\"version\":").unwrap();
        assert_eq!(store.find(&record.root).unwrap(), Some(record.clone()));
        record.helper_version = "0.3.1".into();
        store.save(&record).unwrap();
        let mut still_open = Vec::new();
        live.read_to_end(&mut still_open).unwrap();
        assert_eq!(still_open, before, "the live inode must never be truncated");
        assert_eq!(store.find(&record.root).unwrap(), Some(record));
        assert!(!temporary.exists());
    }

    #[test]
    fn remote_project_record_is_private_and_round_trips() {
        let store = store();
        let record = record();
        assert_eq!(store.find(&record.root).unwrap(), None);
        assert!(!store.forget(&record.root).unwrap());
        store.save(&record).unwrap();
        let path = store
            .dir
            .as_ref()
            .unwrap()
            .join(format!("{}.json", project_key(&record.root)));
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            path.parent().unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(store.find(&record.root).unwrap(), Some(record.clone()));
        assert_eq!(store.find(Path::new("/srv/other")).unwrap(), None);
        let mut value = serde_json::to_value(&record).unwrap();
        value["version"] = serde_json::json!(2);
        agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        let err = store.find(&record.root).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains(&path.display().to_string()));
        let mut bad = record.clone();
        bad.version = 2;
        assert!(store.save(&bad).is_err());
        store.save(&record).unwrap();
        assert!(store.forget(&record.root).unwrap());
        assert!(!store.forget(&record.root).unwrap());
    }

    #[test]
    fn project_key_depends_only_on_the_root() {
        let mut record = record();
        let key = project_key(&record.root);
        assert_eq!(key.len(), 16);
        assert!(key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        record.host = "other-host".into();
        record.host_key_sha256 = "SHA256:other".into();
        assert_eq!(key, project_key(&record.root));
        assert_ne!(key, project_key(Path::new("/srv/other")));
        let expected = Sha256::digest(b"eitri-remote-project\0/srv/project");
        assert_eq!(key, format!("{expected:x}")[..16]);
    }

    #[test]
    fn host_record_keeps_every_installed_version_hash() {
        let store = store();
        let mut host = host_record();
        assert_eq!(store.host(&host.host_key_sha256).unwrap(), None);
        store
            .update_host(&host.host_key_sha256, |old| {
                assert_eq!(old, None);
                host.clone()
            })
            .unwrap();
        host.helpers.insert("0.3.1-7654321".into(), "c".repeat(64));
        host.sidecars.insert("7654321".into(), "d".repeat(64));
        store
            .update_host(&host.host_key_sha256, |old| {
                let mut merged = old.unwrap();
                merged.helpers.insert("0.3.1-7654321".into(), "c".repeat(64));
                merged.sidecars.insert("7654321".into(), "d".repeat(64));
                merged
            })
            .unwrap();
        assert_eq!(store.host(&host.host_key_sha256).unwrap(), Some(host.clone()));
        let path = store.host_path(&host.host_key_sha256).unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let mut value = serde_json::to_value(&host).unwrap();
        value["version"] = serde_json::json!(2);
        agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            store.host(&host.host_key_sha256).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    /// A later build may add a field to either record: this build reads past it, so the project still opens and its
    /// stops can still be kept. A field this build needs stays required.
    #[test]
    fn records_with_a_later_builds_field_still_read_and_a_missing_field_is_refused() {
        let store = store();
        let record = record();
        store.save(&record).unwrap();
        let path = store.project_path(&record.root).unwrap();
        let mut value = serde_json::to_value(&record).unwrap();
        value["added_later"] = serde_json::json!({ "any": "shape" });
        agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(store.find(&record.root).unwrap(), Some(record.clone()));
        store.queue_stop(&record.root, "sc-1").unwrap();
        assert_eq!(store.find(&record.root).unwrap().unwrap().queued_stops, ["sc-1"]);
        value.as_object_mut().unwrap().remove("helper_version");
        agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(store.find(&record.root).unwrap_err().kind(), io::ErrorKind::InvalidData);

        let host = host_record();
        store.save_host(&host).unwrap();
        let path = store.host_path(&host.host_key_sha256).unwrap();
        let mut value = serde_json::to_value(&host).unwrap();
        value["added_later"] = serde_json::json!(2);
        agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(store.host(&host.host_key_sha256).unwrap(), Some(host.clone()));
        value.as_object_mut().unwrap().remove("consented");
        agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            store.host(&host.host_key_sha256).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn interrupted_host_replacement_keeps_consent_and_hashes() {
        let store = store();
        let mut host = host_record();
        store.save_host(&host).unwrap();
        let path = store.host_path(&host.host_key_sha256).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut live = std::fs::File::open(&path).unwrap();
        let temporary = crate::state_paths::temporary(&path);
        agent::private_fs::write_private(&temporary, b"{\"version\":").unwrap();
        assert_eq!(store.host(&host.host_key_sha256).unwrap(), Some(host.clone()));
        host.helpers.insert("0.3.1-7654321".into(), "c".repeat(64));
        store.save_host(&host).unwrap();
        let mut still_open = Vec::new();
        live.read_to_end(&mut still_open).unwrap();
        assert_eq!(still_open, before, "the live inode must never be truncated");
        assert_eq!(store.host(&host.host_key_sha256).unwrap(), Some(host));
        assert!(!temporary.exists());
    }

    #[test]
    fn concurrent_host_updates_merge_under_the_store_lock() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let store = store();
        let (reading, read) = channel();
        let (release, released) = channel();
        let (busy, blocked) = channel();
        let other = RemoteProjects { dir: store.dir.clone() };
        let first = std::thread::spawn(move || {
            other
                .update_host("SHA256:example", |old| {
                    assert_eq!(old, None);
                    reading.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(60)).unwrap();
                    host_record()
                })
                .unwrap();
        });
        read.recv_timeout(Duration::from_secs(60)).unwrap();
        let other = RemoteProjects { dir: store.dir.clone() };
        let second = std::thread::spawn(move || {
            let mut busy = Some(busy);
            crate::state_paths::test_hooks::set_on_busy(move || {
                if let Some(busy) = busy.take() {
                    busy.send(()).unwrap();
                }
            });
            other
                .update_host("SHA256:example", |old| {
                    let mut host = old.expect("the first writer's record must be visible");
                    host.helpers.insert("0.3.1-7654321".into(), "c".repeat(64));
                    host.sidecars.insert("7654321".into(), "d".repeat(64));
                    host
                })
                .unwrap();
        });
        blocked.recv_timeout(Duration::from_secs(60)).unwrap();
        release.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        let mut expected = host_record();
        expected.helpers.insert("0.3.1-7654321".into(), "c".repeat(64));
        expected.sidecars.insert("7654321".into(), "d".repeat(64));
        assert_eq!(store.host("SHA256:example").unwrap(), Some(expected));
        let lock = crate::state_paths::lock_path(&store.host_path("SHA256:example").unwrap());
        assert_eq!(lock.parent(), Some(store.dir().unwrap()));
        assert_eq!(lock.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn host_update_refuses_a_changed_identity_or_version_without_replacing() {
        let store = store();
        let host = host_record();
        store.save_host(&host).unwrap();
        for field in ["key", "version"] {
            let error = store
                .update_host(&host.host_key_sha256, |old| {
                    let mut host = old.unwrap();
                    if field == "key" {
                        host.host_key_sha256 = "SHA256:other".into();
                    } else {
                        host.version = 2;
                    }
                    host
                })
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(store.host(&host.host_key_sha256).unwrap(), Some(host.clone()));
        }
    }

    #[test]
    fn remote_project_state_home_requires_an_absolute_location() {
        for state in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
            let store = RemoteProjects::new(state, Some(OsStr::new("/home/example")));
            assert_eq!(
                store.dir.as_deref(),
                Some(Path::new("/home/example/.local/state/eitri/hosts"))
            );
        }
        let store = RemoteProjects::new(None, None);
        assert!(store.find(Path::new("/srv/project")).is_err());
        assert!(store.save(&record()).is_err());
    }
}
