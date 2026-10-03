//! Eitri's own record of which project configuration the user trusted, one file per root.
//!
//! The record is `<state home>/eitri/trust/<16 hex of the root>.json`: the root it is about, when
//! it was made, the fingerprint of the configuration that was shown and each file's own hash, so a
//! later change can be named file by file. A root is only ever trusted by its own record -- a
//! record for a parent or a child directory is a different file under a different key -- and no
//! other program's notion of trust is read.
//!
//! A record that cannot be used (missing, not JSON, another version, another root, owned by
//! someone else, a link, a FIFO) reads as untrusted and is left where it is: another user may have
//! planted it, and renaming it aside would only move their file around. Answering `y` replaces it
//! atomically instead; the rename puts a new file in the name's place and never follows a planted
//! link.
//!
//! Everything here that touches the disk (`state`, `record`, `forget`, `for_root`) is for the trust
//! gate's worker thread only: a slow or hostile state directory must never stall the window.
//! [`TrustStore::new`] and [`TrustStore::at`] touch nothing, so the window may build one anywhere.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::nofollow::{read_regular_nofollow, Owner};
use super::{discover, Discovery, EntryKind, Limits};
use crate::layout::persist;

/// A record larger than this is not one of ours.
const MAX_RECORD_BYTES: u64 = 1 << 20;

/// What changed between the trusted configuration and the one found now, by path relative to the
/// top level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub changed: Vec<PathBuf>,
}

/// Whether an answer to the prompt can outlive the start it was given for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remember {
    /// A record can be written.
    Yes,
    /// Everything was hashed but no record can be kept: no state directory, or one the user cannot
    /// use. The answer holds for this window.
    WindowOnly(String),
    /// Something could not be hashed, so its content may change under an unchanged fingerprint:
    /// the answer covers this start only and is never recorded or remembered. Wins over
    /// `WindowOnly`.
    SessionOnly(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    NothingToTrust(Discovery),
    Trusted {
        since_unix: u64,
        discovery: Discovery,
    },
    Untrusted {
        discovery: Discovery,
        remember: Remember,
    },
    Changed {
        discovery: Discovery,
        diff: Diff,
        remember: Remember,
    },
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    root: String,
    trusted_at: u64,
    fingerprint: String,
    files: BTreeMap<String, String>,
}

pub struct TrustStore {
    dir: Option<PathBuf>,
}

impl TrustStore {
    /// `<state home>/eitri/trust`, by the rule every other state directory follows. Pure: nothing
    /// is read or created.
    pub fn new(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> TrustStore {
        TrustStore {
            dir: persist::state_subdir(xdg_state_home, home, "trust"),
        }
    }

    /// A store over `dir` itself (tests); `None` is a machine with no usable state directory.
    pub fn at(dir: Option<PathBuf>) -> TrustStore {
        TrustStore { dir }
    }

    fn record_path(&self, root: &Path) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join(persist::file_name(root)))
    }

    /// What the record says about `discovery`. Reads the one record file, never following a link.
    pub fn state(&self, discovery: Discovery) -> TrustState {
        if discovery.nothing_to_trust() {
            return TrustState::NothingToTrust(discovery);
        }
        let remember = self.remember_for(&discovery);
        let Some(record) = self.read_usable(&discovery.root) else {
            return TrustState::Untrusted { discovery, remember };
        };
        if record.fingerprint == discovery.fingerprint.hex() {
            // An equal fingerprint says nothing about a file it never read.
            if discovery.fully_hashed() {
                return TrustState::Trusted {
                    since_unix: record.trusted_at,
                    discovery,
                };
            }
            return TrustState::Untrusted { discovery, remember };
        }
        let diff = diff(&record.files, &discovery);
        TrustState::Changed {
            discovery,
            diff,
            remember,
        }
    }

    fn remember_for(&self, discovery: &Discovery) -> Remember {
        if !discovery.fully_hashed() {
            return Remember::SessionOnly(not_hashed_note(discovery));
        }
        if self.dir.is_none() {
            return Remember::WindowOnly("no usable state directory (XDG_STATE_HOME/HOME)".to_string());
        }
        if discovery.root.to_str().is_none() {
            return Remember::WindowOnly("the project path is not valid UTF-8".to_string());
        }
        Remember::Yes
    }

    fn read_usable(&self, root: &Path) -> Option<Record> {
        let path = self.record_path(root)?;
        let bytes = read_regular_nofollow(&path, MAX_RECORD_BYTES, Owner::Me).ok()?;
        let record: Record = serde_json::from_slice(&bytes).ok()?;
        let usable = record.version == 1
            && root.to_str() == Some(record.root.as_str())
            && record.fingerprint.len() == 64
            && record.fingerprint.bytes().all(|b| b.is_ascii_hexdigit());
        usable.then_some(record)
    }

    /// Writes the record for `discovery`: a temporary file beside it, 0600, renamed over the
    /// record. Refuses a discovery that could not hash everything, and a store with no directory,
    /// writing nothing. A directory that cannot be created or used is the error.
    pub fn record(&self, discovery: &Discovery, now_unix: u64) -> io::Result<()> {
        if !discovery.fully_hashed() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not recorded: {}", not_hashed_note(discovery)),
            ));
        }
        let (Some(dir), Some(path)) = (self.dir.as_deref(), self.record_path(&discovery.root)) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no usable state directory (XDG_STATE_HOME/HOME)",
            ));
        };
        let Some(root) = discovery.root.to_str() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the project path is not valid UTF-8",
            ));
        };
        let record = Record {
            version: 1,
            root: root.to_string(),
            trusted_at: now_unix,
            fingerprint: discovery.fingerprint.hex(),
            files: files_of(discovery),
        };
        let text = serde_json::to_string_pretty(&record).map_err(io::Error::other)?;
        persist::create_state_dir(dir)?;
        let tmp = persist::temporary(&path);
        let written =
            agent::private_fs::write_private(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(err);
        }
        Ok(())
    }

    /// Removes this root's record when it is a regular file the user owns; `true` if one existed.
    /// A foreign file or a link is left in place and named in the error.
    pub fn forget(&self, root: &Path) -> io::Result<bool> {
        let Some(path) = self.record_path(root) else {
            return Ok(false);
        };
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        let mine = {
            use std::os::unix::fs::MetadataExt;
            meta.uid() == agent::private_fs::current_uid()
        };
        if !meta.file_type().is_file() || !mine {
            let what = if meta.file_type().is_symlink() {
                "a link"
            } else if !meta.file_type().is_file() {
                "not a regular file"
            } else {
                "owned by another user"
            };
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("left in place: the record {} is {what}", path.display()),
            ));
        }
        std::fs::remove_file(&path)?;
        Ok(true)
    }
}

/// Discovers `root`'s configuration and asks the record about it.
pub fn for_root(store: &TrustStore, root: &Path, home: Option<&Path>) -> TrustState {
    store.state(discover(root, home, &Limits::default()))
}

/// Which parts of a discovery nobody could check, for the prompt and for a refused record.
fn not_hashed_note(discovery: &Discovery) -> String {
    let mut parts: Vec<String> = discovery
        .uncheckable()
        .into_iter()
        .map(|(path, reason)| format!("{} ({reason})", path.display()))
        .collect();
    if let Some(reason) = &discovery.over_budget {
        parts.push(reason.clone());
    }
    if parts.is_empty() {
        parts.push("part of the configuration could not be hashed".to_string());
    }
    format!("cannot be checked: {}", parts.join("; "))
}

fn value_of(kind: &EntryKind) -> String {
    match kind {
        EntryKind::File { sha256, .. } => super::hex(sha256),
        EntryKind::Symlink { target, .. } => format!("symlink:{}", target.display()),
        EntryKind::Dir => "dir".to_string(),
        EntryKind::Unreadable { .. } => "unreadable".to_string(),
        EntryKind::Other => "other".to_string(),
    }
}

fn files_of(discovery: &Discovery) -> BTreeMap<String, String> {
    discovery
        .entries
        .iter()
        .map(|entry| (entry.path.to_string_lossy().into_owned(), value_of(&entry.kind)))
        .collect()
}

fn diff(recorded: &BTreeMap<String, String>, discovery: &Discovery) -> Diff {
    let now = files_of(discovery);
    let mut out = Diff::default();
    for (path, value) in &now {
        match recorded.get(path) {
            None => out.added.push(PathBuf::from(path)),
            Some(before) if before != value => out.changed.push(PathBuf::from(path)),
            Some(_) => {}
        }
    }
    for path in recorded.keys() {
        if !now.contains_key(path) {
            out.removed.push(PathBuf::from(path));
        }
    }
    out
}

/// The UTC civil date (`YYYY-MM-DD`) of a Unix time in seconds, by days-from-civil arithmetic.
pub fn civil_date(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}
