//! Saved remote projects and host installation identities. All disk work belongs on a worker.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::state_paths::{create_state_dir, lock_store, temporary, ON_A_WRITER_THREAD};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteProject {
    pub version: u32,
    pub host: String,
    pub host_key_sha256: String,
    pub root: PathBuf,
    pub helper_version: String,
    pub sync: Option<serde_json::Value>,
    pub created_at: u64,
}

pub fn project_key(root: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(b"eitri-remote-project\0");
    hash.update(root.as_os_str().as_bytes());
    format!("{:x}", hash.finalize())[..16].to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabOrigin {
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

    pub fn forget(&self, root: &Path) -> io::Result<bool> {
        match std::fs::remove_file(self.project_path(root)?) {
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
        crate::editor_record::file::replace_private(&temporary(path), path, &bytes)
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

    fn record() -> RemoteProject {
        RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
            helper_version: "0.3.0".into(),
            sync: None,
            created_at: 42,
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
        for change in ["version", "unknown"] {
            let mut value = serde_json::to_value(&record).unwrap();
            value[change] = serde_json::json!(2);
            agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
            let err = store.find(&record.root).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(err.to_string().contains(&path.display().to_string()));
        }
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
        for change in ["version", "unknown"] {
            let mut value = serde_json::to_value(&host).unwrap();
            value[change] = serde_json::json!(2);
            agent::private_fs::write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(
                store.host(&host.host_key_sha256).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
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
