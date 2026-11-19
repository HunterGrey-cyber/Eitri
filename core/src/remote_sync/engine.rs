//! Worker-thread interface to a synchronization engine.

use super::{SyncError, SyncKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub type Labels = BTreeMap<String, String>;

pub trait SyncEngine: Send + Sync {
    fn create(&self, spec: &SessionSpec) -> Result<(), SyncError>;
    fn terminate(&self, name: &str) -> Result<(), SyncError>;
    fn pause(&self, name: &str) -> Result<(), SyncError>;
    fn resume(&self, name: &str) -> Result<(), SyncError>;
    fn status(&self, labels: &Labels) -> Result<Vec<SessionStatus>, SyncError>;
    fn flush(&self, name: &str, cap: Duration) -> FlushOutcome;
    fn monitor(&self, name: &str) -> Result<Box<dyn Iterator<Item = SessionStatus> + Send>, SyncError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Tree,
    Git,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Git => "git",
        }
    }

    pub fn alpha_side(self) -> Side {
        match self {
            Self::Tree => Side::Laptop,
            Self::Git => Side::Server,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    TwoWaySafe,
    OneWayReplica,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagingMode {
    Neighboring,
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymlinkMode {
    Ignore,
    PosixRaw,
    Portable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Laptop(PathBuf),
    Server { key: SyncKey, root: PathBuf },
}

impl Endpoint {
    pub fn render(&self) -> Result<OsString, SyncError> {
        match self {
            Self::Laptop(root) => {
                check_root(root, false)?;
                Ok(root.as_os_str().to_owned())
            }
            Self::Server { key, root } => {
                check_root(root, true)?;
                let mut url = OsString::from(format!("eitri-{key}:"));
                url.push(root);
                Ok(url)
            }
        }
    }
}

fn check_root(root: &Path, server: bool) -> Result<(), SyncError> {
    let bytes = root.as_os_str().as_bytes();
    if !root.is_absolute() || bytes.iter().any(|b| matches!(b, 0 | b'\n') || server && *b == b':') {
        return Err(SyncError::Engine(format!(
            "invalid sync endpoint root: {}",
            root.display()
        )));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SessionSpec {
    pub name: String,
    pub key: SyncKey,
    pub role: Role,
    pub laptop_root: PathBuf,
    pub symlink_mode: Option<SymlinkMode>,
    pub alpha: Endpoint,
    pub beta: Endpoint,
    pub mode: Mode,
    pub ignores: Vec<String>,
    pub ignore_vcs: bool,
    pub staging: StagingMode,
    pub labels: Labels,
}

impl SessionSpec {
    pub fn tree(key: SyncKey, laptop: PathBuf, server: PathBuf) -> Self {
        Self::for_role(key, Role::Tree, laptop, server)
    }

    pub fn git(key: SyncKey, laptop: PathBuf, server: PathBuf) -> Self {
        Self::for_role(key, Role::Git, laptop, server)
    }

    fn for_role(key: SyncKey, role: Role, laptop: PathBuf, server: PathBuf) -> Self {
        let labels = project_labels(&key, role, &laptop);
        let laptop_root = laptop.clone();
        let (laptop, server) = if role == Role::Git {
            (laptop.join(".git"), server.join(".git"))
        } else {
            (laptop, server)
        };
        let local = Endpoint::Laptop(laptop);
        let remote = Endpoint::Server {
            key: key.clone(),
            root: server,
        };
        let (alpha, beta) = match role {
            Role::Tree => (local, remote),
            Role::Git => (remote, local),
        };
        Self {
            name: session_name(&key, role),
            key,
            role,
            laptop_root,
            symlink_mode: (role == Role::Git).then_some(SymlinkMode::Ignore),
            alpha,
            beta,
            mode: if role == Role::Tree {
                Mode::TwoWaySafe
            } else {
                Mode::OneWayReplica
            },
            ignores: if role == Role::Tree {
                vec![".git".into()]
            } else {
                vec![]
            },
            ignore_vcs: role == Role::Tree,
            staging: if role == Role::Tree {
                StagingMode::Neighboring
            } else {
                StagingMode::Default
            },
            labels,
        }
    }
}

pub fn session_name(key: &SyncKey, role: Role) -> String {
    format!("eitri-{key}-{}", role.as_str())
}

pub fn project_labels(key: &SyncKey, role: Role, laptop: &Path) -> Labels {
    Labels::from([
        ("eitri-key".into(), key.to_string()),
        ("eitri-role".into(), role.as_str().into()),
        (
            "eitri-root".into(),
            format!("{:x}", Sha256::digest(laptop.as_os_str().as_bytes()))[..16].into(),
        ),
    ])
}

pub fn valid_label_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(b))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStatus {
    pub name: String,
    pub role: Role,
    pub state: SyncState,
    pub successful_cycles: u64,
    pub conflicts: Vec<Conflict>,
    pub problems: Vec<Problem>,
    pub excluded_conflicts: u64,
    pub excluded_scan_problems: SideCounts,
    pub excluded_transition_problems: SideCounts,
    pub staging: Option<Staging>,
    pub last_error: Option<String>,
}

impl SessionStatus {
    pub fn disconnected(name: &str, role: Role, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            name: name.into(),
            role,
            state: SyncState::Disconnected(reason.clone()),
            successful_cycles: 0,
            conflicts: vec![],
            problems: vec![],
            excluded_conflicts: 0,
            excluded_scan_problems: SideCounts::default(),
            excluded_transition_problems: SideCounts::default(),
            staging: None,
            last_error: Some(reason),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncState {
    Connecting,
    Scanning,
    Staging,
    Applying,
    Watching,
    Paused,
    Halted(String),
    Disconnected(String),
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Missing,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub laptop: EntryKind,
    pub server: EntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Laptop,
    Server,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SideCounts {
    pub laptop: u64,
    pub server: u64,
}

impl SideCounts {
    pub fn from_alpha_beta(role: Role, alpha: u64, beta: u64) -> Self {
        match role {
            Role::Tree => Self {
                laptop: alpha,
                server: beta,
            },
            Role::Git => Self {
                laptop: beta,
                server: alpha,
            },
        }
    }
}

impl Side {
    pub fn other(self) -> Self {
        match self {
            Self::Laptop => Self::Server,
            Self::Server => Self::Laptop,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub path: String,
    pub side: Side,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staging {
    pub side: Side,
    pub expected_bytes: u64,
    pub received_bytes: u64,
    pub total_received_bytes: u64,
    pub expected_files: u64,
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    Flushed,
    TimedOut,
    Failed(String),
}
