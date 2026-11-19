//! GTK-free identities and shared contracts for remote project sync.

pub mod daemon;
pub mod engine;
pub mod git_replica;
pub mod ignores;
pub mod local_git;
pub mod mutagen;
pub mod mutagen_json;
pub mod status_schema;

use sha2::{Digest, Sha256};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SyncKey(String);

impl SyncKey {
    pub fn new(host_key_sha256: &str, root: &Path) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"eitri-remote-sync\0");
        hash.update(host_key_sha256.as_bytes());
        hash.update(b"\0");
        hash.update(root.as_os_str().as_bytes());
        Self(format!("{:x}", hash.finalize())[..16].to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SyncKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    Unsupported(String),
    Engine(String),
    Helper(String),
    Io(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(text) | Self::Engine(text) | Self::Helper(text) | Self::Io(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for SyncError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_key_uses_its_own_domain() {
        let root = Path::new("/srv/project");
        let key = SyncKey::new("SHA256:example", root);
        let expected = Sha256::digest(b"eitri-remote-sync\0SHA256:example\0/srv/project");
        assert_eq!(key.as_str(), &format!("{expected:x}")[..16]);
        assert_eq!(key.to_string(), key.as_str());
        assert_ne!(key.as_str(), crate::remote_project::project_key(root));
        let trust = crate::project_trust::RemoteRoot {
            host_key_sha256: "SHA256:example".into(),
            root: root.into(),
        };
        assert_ne!(key.as_str(), trust.record_id());
        assert_ne!(key, SyncKey::new("SHA256:other", root));
        assert_ne!(key, SyncKey::new("SHA256:example", Path::new("/srv/other")));
        use std::os::unix::ffi::OsStrExt;
        let raw = Path::new(std::ffi::OsStr::from_bytes(b"/srv/\xff"));
        let expected = Sha256::digest(b"eitri-remote-sync\0SHA256:example\0/srv/\xff");
        assert_eq!(
            SyncKey::new("SHA256:example", raw).as_str(),
            &format!("{expected:x}")[..16]
        );
    }
}
