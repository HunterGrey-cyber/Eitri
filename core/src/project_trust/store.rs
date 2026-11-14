//! Eitri's own record of which project configuration the user trusted, one file per root and agent.
//!
//! Claude's record is `<state home>/eitri/trust/<16 hex of the root>.json`; every other agent adds
//! `.<profile key>` (`<16 hex>.json.codex`) and names its profile in the record. Each holds the root,
//! trust time, configuration fingerprint and per-file hashes so changes can be named file by file.
//! Trust is exact to the root and agent: parents, children and other agents have separate records.
//! No other program's notion of trust is read.
//!
//! A project on a server ([`TrustPlace::Remote`]) is keyed by the host key the remote project records
//! (the caller takes a scan only when the connection it came through shows that key) and the root as
//! the remote project records it: `<16 hex of SHA-256("eitri-remote-trust\0" || host key || "\0" ||
//! root)>.json`, plus the profile's suffix. Such a record also names the host key and the directory
//! the scan found that root to be (`scanned_root`); a record for this machine names neither. So a
//! record for one place never answers for the other, even for the same path; a rotated host key is a
//! new place that is asked about again; and the recorded root leading to another directory on the
//! server, even one holding the same configuration, is asked about again, as a different canonical
//! root is here. The name holds only the recorded root, so `:untrust` needs no scan.
//!
//! Beside a record lies `<record>~last-trusted`, the instruction files as that `y` saw them
//! ([`super::last_trusted`]). It is written after the record, removed with it, and reads as nothing
//! when it is missing or does not match: only the record decides. A profile's key never holds a `~`,
//! so no agent's record can have that name.
//!
//! A record that cannot be used (missing, not JSON, another version, another root, another place (a
//! record naming another host key, or none where one is wanted, or another scanned directory), owned
//! by someone else, a link, a FIFO, or written after a prompt that showed less than this one does)
//! reads as untrusted and stays in place: renaming a planted file aside only moves someone else's
//! file around. Answering `y` replaces it atomically without following a planted link.
//!
//! Disk operations (`state`, `record`, `forget`, `for_root`) belong on the trust worker: a slow or
//! hostile state directory must never stall the window. [`TrustStore::new`] and [`TrustStore::at`]
//! touch nothing, so the window may build one anywhere.
//!
//! Reads follow no link from `/` down, but the state home can be linked (`/home` to `/var/home`, or
//! `~/.local/state`). Each disk operation resolves the store directory once and uses that real path
//! for reads and writes. A write must read back successfully; otherwise it would report trust that
//! the next start cannot read.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::last_trusted::{self, ChangedText, MAX_KEPT_BYTES};
use super::nofollow::{read_regular_nofollow, Owner};
use super::{discover, Discovery, EntryKind, Limits, Profile, ProfileEnv};
use crate::state_paths;

/// Whose disk a record is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TrustPlace {
    /// This machine: the record is keyed by the root it names.
    Local,
    /// A project on a server, whose configuration its helper read there.
    Remote(RemoteRoot),
}

/// A project on a server, as a trust record is keyed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteRoot {
    /// The SHA-256 fingerprint of the server's host key the remote project records. Whoever reads the
    /// server's configuration for this place takes it only when the connection shows this key.
    pub host_key_sha256: String,
    /// The root as the remote project records it.
    pub root: PathBuf,
}

impl RemoteRoot {
    /// The first 16 hex digits of `SHA-256("eitri-remote-trust\0" || host key || "\0" || root)`, the
    /// root's bytes as they are: the name of this project's records.
    pub fn record_id(&self) -> String {
        use sha2::{Digest, Sha256};
        use std::os::unix::ffi::OsStrExt;
        let mut hash = Sha256::new();
        hash.update(b"eitri-remote-trust\0");
        hash.update(self.host_key_sha256.as_bytes());
        hash.update(b"\0");
        hash.update(self.root.as_os_str().as_bytes());
        super::hex(&hash.finalize())[..16].to_string()
    }
}

impl TrustPlace {
    /// The root a record for this place names: `root` (the discovery's) on this machine, the remote
    /// project's own on a server.
    fn record_root<'a>(&'a self, root: &'a Path) -> &'a Path {
        match self {
            TrustPlace::Local => root,
            TrustPlace::Remote(remote) => &remote.root,
        }
    }

    /// The host key a record for this place names: none on this machine.
    fn host_key(&self) -> Option<&str> {
        match self {
            TrustPlace::Local => None,
            TrustPlace::Remote(remote) => Some(&remote.host_key_sha256),
        }
    }
}

/// A record larger than this is not one of ours. The record is read on every check, from a state
/// directory another program of the user's could fill, so the reader stays small; a configuration
/// whose record would be larger (about 1,900 files with long names) is not recorded at all, and its
/// prompt says from the start that a `y` lasts for this window only ([`Remember::WindowOnly`]): its
/// fingerprint covers everything, so the window may keep the answer, but no record can.
const MAX_RECORD_BYTES: u64 = 1 << 20;

/// How much the prompt a record answered showed. A `y` approves what was shown, not just the bytes
/// hashed: whole values, `env` values, every MCP and hook field, and each other configuration file
/// by name, size and the frontmatter keys that change what runs (`hooks`, `allowed-tools`,
/// `permissionMode`, `mcpServers`). Claude's prompt also says that a subdirectory's `CLAUDE.md`, which
/// the walk does not read, loads once Claude reads a file there.
/// A lower or missing disclosure is unusable even when the fingerprint matches, so the project is
/// asked about again. Raise this whenever the prompt starts showing something it hid before.
///
/// The diff of an instruction file does not count: it is shown only at a question about a changed
/// configuration, which is asked anyway, so no record answered less than a prompt for the same
/// configuration shows today.
pub const DISCLOSURE: u32 = 5;

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
        /// What can be shown of the instruction files `diff` names. Only for the person reading the
        /// question: nothing decides by it.
        texts: Vec<ChangedText>,
    },
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    root: String,
    trusted_at: u64,
    fingerprint: String,
    files: BTreeMap<String, String>,
    /// The profile's key, absent for Claude to keep its records compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    /// [`DISCLOSURE`] when the record was written; a missing field reads as 0 and is unusable.
    #[serde(default)]
    disclosure: u32,
    /// The server's host key for a project on a server ([`TrustPlace::Remote`]); absent for this
    /// machine's, which keeps those records as they were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_key_sha256: Option<String>,
    /// For a project on a server, the directory the scan found the recorded root to be (the
    /// discovery's root); absent for this machine's, whose `root` already is that directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scanned_root: Option<String>,
}

/// The record's file name for `root` (or a remote place's own root) and `profile`. Keep Claude's name
/// stable so existing records still answer; every other profile adds its key to that name. A project on
/// a server is named by its host key and root, never by the path alone.
fn record_name(place: &TrustPlace, root: &Path, profile: &Profile) -> String {
    let base = match place {
        TrustPlace::Local => state_paths::file_name(root),
        TrustPlace::Remote(remote) => format!("{}.json", remote.record_id()),
    };
    match profile {
        Profile::Claude => base,
        other => format!("{base}.{}", other.key()),
    }
}

/// The copy of the instruction files beside the record at `record`.
fn copy_path(record: &Path) -> PathBuf {
    let mut name = record.file_name().expect("a record has a file name").to_os_string();
    name.push("~last-trusted");
    record.with_file_name(name)
}

fn record_temporary(path: &Path) -> PathBuf {
    let mut name = path.file_name().expect("a record has a file name").to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// The record at `path` when it is a usable one for `place`, `root` (the discovery's) and `profile`,
/// read without following a link. A record names the host key of the server it is about and the
/// directory scanned there, or neither for this machine, and is usable only for that place.
fn read_usable_at(path: &Path, place: &TrustPlace, root: &Path, profile: &Profile) -> Option<Record> {
    let bytes = read_regular_nofollow(path, MAX_RECORD_BYTES, Owner::Me).ok()?;
    let record: Record = serde_json::from_slice(&bytes).ok()?;
    let usable = record.version == 1
        && record.disclosure >= DISCLOSURE
        && record.profile == profile_field(profile)
        && record.host_key_sha256.as_deref() == place.host_key()
        && record.scanned_root.as_deref() == scanned_root(place, root)
        && place.record_root(root).to_str() == Some(record.root.as_str())
        && record.fingerprint.len() == 64
        && record.fingerprint.bytes().all(|b| b.is_ascii_hexdigit());
    usable.then_some(record)
}

/// The directory a record for `place` names as scanned, given the discovery's root `root`: none for this
/// machine; for a server, `root` (`None` when it is not UTF-8, which no record can name).
fn scanned_root<'a>(place: &TrustPlace, root: &'a Path) -> Option<&'a str> {
    match place {
        TrustPlace::Local => None,
        TrustPlace::Remote(_) => root.to_str(),
    }
}

/// What a record for `profile` says it is about: nothing for Claude, else the profile's key.
fn profile_field(profile: &Profile) -> Option<String> {
    match profile {
        Profile::Claude => None,
        other => Some(other.key()),
    }
}

#[derive(Clone)]
pub struct TrustStore {
    dir: Option<PathBuf>,
}

impl TrustStore {
    /// `<state home>/eitri/trust`, by the rule every other state directory follows. Pure: nothing
    /// is read or created.
    pub fn new(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> TrustStore {
        TrustStore {
            dir: state_paths::state_subdir(xdg_state_home, home, "trust"),
        }
    }

    /// A store over `dir` itself (tests); `None` is a machine with no usable state directory.
    pub fn at(dir: Option<PathBuf>) -> TrustStore {
        TrustStore { dir }
    }

    /// The record's path: the store's directory with its links resolved ([`without_links`]), and
    /// [`record_name`] in it, which is never resolved.
    fn record_path(&self, place: &TrustPlace, root: &Path, profile: &Profile) -> Option<PathBuf> {
        self.real_dir().map(|dir| dir.join(record_name(place, root, profile)))
    }

    /// The store's directory with its links resolved. Touches the disk: worker only.
    fn real_dir(&self) -> Option<PathBuf> {
        self.dir.as_deref().map(without_links)
    }

    /// What the record says about `discovery`. Reads the one record file, never following a link.
    ///
    /// An empty discovery is "nothing to trust" except for a profile that needs an explicit `y`: that
    /// one is asked about, and its record holds the empty fingerprint.
    pub fn state(&self, discovery: Discovery) -> TrustState {
        self.state_at(&TrustPlace::Local, discovery)
    }

    /// [`Self::state`] for a discovery made at `place`: on a server, the record of its host key and
    /// root, never one of this machine's.
    pub fn state_at(&self, place: &TrustPlace, discovery: Discovery) -> TrustState {
        if discovery.nothing_to_trust() && !discovery.profile.requires_explicit_yes() {
            return TrustState::NothingToTrust(discovery);
        }
        let remember = self.remember_for(place, &discovery);
        let Some(record) = self.read_usable(place, &discovery.root, &discovery.profile) else {
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
        let copy = self.read_copy(place, &discovery);
        let texts = last_trusted::compare(&record.files, copy.as_ref(), &discovery, &diff);
        TrustState::Changed {
            discovery,
            diff,
            remember,
            texts,
        }
    }

    /// The copy kept at the last `y`, read as the record is, when it is one for this root and agent.
    fn read_copy(&self, place: &TrustPlace, discovery: &Discovery) -> Option<last_trusted::KeptFiles> {
        let record = self.record_path(place, &discovery.root, &discovery.profile)?;
        let bytes = read_regular_nofollow(&copy_path(&record), MAX_KEPT_BYTES, Owner::Me).ok()?;
        last_trusted::decode(&bytes, &discovery.root, profile_field(&discovery.profile))
    }

    fn remember_for(&self, place: &TrustPlace, discovery: &Discovery) -> Remember {
        if !discovery.fully_hashed() {
            return Remember::SessionOnly(not_hashed_note(discovery));
        }
        if self.dir.is_none() {
            return Remember::WindowOnly("no usable state directory (XDG_STATE_HOME/HOME)".to_string());
        }
        if place.record_root(&discovery.root).to_str().is_none() || discovery.root.to_str().is_none() {
            return Remember::WindowOnly("the project path is not valid UTF-8".to_string());
        }
        // Sized for the latest time a record can carry, so a record written later is never larger.
        let size = record_text(place, discovery, u64::MAX).map_or(u64::MAX, |text| text.len() as u64);
        if size > MAX_RECORD_BYTES {
            return Remember::WindowOnly(too_large_to_record(discovery, size));
        }
        Remember::Yes
    }

    fn read_usable(&self, place: &TrustPlace, root: &Path, profile: &Profile) -> Option<Record> {
        read_usable_at(&self.record_path(place, root, profile)?, place, root, profile)
    }

    /// Writes the record for `discovery`: a temporary file beside it, 0600, renamed over the
    /// record, and then the copy of its instruction files. Refuses a discovery that could not hash
    /// everything, and a store with no directory, writing nothing. A directory that cannot be created
    /// or used is the error; a copy that cannot be written is not.
    pub fn record(&self, discovery: &Discovery, now_unix: u64) -> io::Result<()> {
        self.record_at(&TrustPlace::Local, discovery, now_unix)
    }

    /// [`Self::record`] for a discovery made at `place`.
    pub fn record_at(&self, place: &TrustPlace, discovery: &Discovery, now_unix: u64) -> io::Result<()> {
        if !discovery.fully_hashed() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not recorded: {}", not_hashed_note(discovery)),
            ));
        }
        let Some(dir) = self.real_dir() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no usable state directory (XDG_STATE_HOME/HOME)",
            ));
        };
        let path = dir.join(record_name(place, &discovery.root, &discovery.profile));
        let text = record_text(place, discovery, now_unix)?;
        // A record the reader would refuse is never written: it could only ever read as untrusted.
        if text.len() as u64 > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "not written: a record this large could not be read back, since {}",
                    too_large_to_record(discovery, text.len() as u64)
                ),
            ));
        }
        state_paths::create_state_dir(&dir)?;
        replace_private(&path, &text)?;
        // What `state` will read must be what was just written; anything else would tell the user
        // the project is trusted while the next start asks again.
        let fingerprint = discovery.fingerprint.hex();
        let reads_back = read_usable_at(&path, place, &discovery.root, &discovery.profile)
            .is_some_and(|read| read.fingerprint == fingerprint);
        if !reads_back {
            return Err(io::Error::other(format!(
                "the trust record {} could not be read back",
                path.display()
            )));
        }
        keep_copy(place, &path, discovery);
        Ok(())
    }

    /// Removes this root's record for `profile` and the copy beside it when each is a regular file the
    /// user owns; `true` if a record existed. A foreign file or a link is left in place and named in
    /// the error. The copy is still removed when the record cannot be, and the other way round.
    pub fn forget(&self, root: &Path, profile: &Profile) -> io::Result<bool> {
        self.forget_at(&TrustPlace::Local, root, profile)
    }

    /// [`Self::forget`] at `place`: on a server, the record of its host key and root (`root` is not
    /// used there).
    pub fn forget_at(&self, place: &TrustPlace, root: &Path, profile: &Profile) -> io::Result<bool> {
        let Some(path) = self.record_path(place, root, profile) else {
            return Ok(false);
        };
        let record = remove_own_file(&path, "record");
        let copy = remove_own_file(&copy_path(&path), "copy of the instruction files");
        let existed = record?;
        copy?;
        Ok(existed)
    }
}

/// Replaces `path` atomically: a temporary file beside it, 0600, renamed over it.
fn replace_private(path: &Path, text: &str) -> io::Result<()> {
    let tmp = record_temporary(path);
    let written = agent::private_fs::write_private(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Keeps what `discovery` read of the instruction files beside `record`, which has been written. The
/// record is what trust rests on, so a copy that cannot be written costs only the next diff; the old
/// one goes, since it no longer matches.
fn keep_copy(place: &TrustPlace, record: &Path, discovery: &Discovery) {
    let path = copy_path(record);
    let kept = last_trusted::encode(discovery, profile_field(&discovery.profile));
    let written = kept.is_some_and(|text| replace_private(&path, &text).is_ok());
    // Another window's `:untrust` can have taken the record since it was read back; its text goes too.
    let fingerprint = discovery.fingerprint.hex();
    let record_stands = read_usable_at(record, place, &discovery.root, &discovery.profile)
        .is_some_and(|read| read.fingerprint == fingerprint);
    if !written || !record_stands {
        let _ = remove_own_file(&path, "copy of the instruction files");
    }
}

/// Removes `path` when it is a regular file the user owns; `true` if one existed. `what` names it in
/// the error when it is left in place.
fn remove_own_file(path: &Path, what: &str) -> io::Result<bool> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    let mine = {
        use std::os::unix::fs::MetadataExt;
        meta.uid() == agent::private_fs::current_uid()
    };
    if !meta.file_type().is_file() || !mine {
        let kind = if meta.file_type().is_symlink() {
            "a link"
        } else if !meta.file_type().is_file() {
            "not a regular file"
        } else {
            "owned by another user"
        };
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("left in place: the {what} {} is {kind}", path.display()),
        ));
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

/// The record of `discovery`, made at `place`, as written, trusted at `now_unix`.
fn record_text(place: &TrustPlace, discovery: &Discovery, now_unix: u64) -> io::Result<String> {
    let (Some(root), Some(_)) = (place.record_root(&discovery.root).to_str(), discovery.root.to_str()) else {
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
        profile: profile_field(&discovery.profile),
        disclosure: DISCLOSURE,
        host_key_sha256: place.host_key().map(str::to_string),
        scanned_root: scanned_root(place, &discovery.root).map(str::to_string),
    };
    serde_json::to_string_pretty(&record).map_err(io::Error::other)
}

/// Why a configuration whose record would be `size` bytes is not recorded.
fn too_large_to_record(discovery: &Discovery, size: u64) -> String {
    let mib = |bytes: u64| format!("{:.1} MiB", bytes as f64 / (1u64 << 20) as f64);
    format!(
        "a trust record of it would be {} for {} files and directories, more than the {} a record may be",
        mib(size),
        discovery.entries.len(),
        mib(MAX_RECORD_BYTES)
    )
}

/// `dir` with every link in it resolved: its deepest existing ancestor made canonical, and the names
/// below that, which do not exist yet, put back as they were. A path that cannot be resolved at all
/// is left as given; writing there may still work, and the read-back after a write says whether it
/// did.
fn without_links(dir: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut at = dir;
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            let mut out = real;
            out.extend(missing.iter().rev());
            return out;
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                at = parent;
            }
            _ => return dir.to_path_buf(),
        }
    }
}

/// Discovers `root`'s configuration for `profile` and asks that profile's record about it.
pub fn for_root(
    store: &TrustStore,
    root: &Path,
    home: Option<&Path>,
    profile: &Profile,
    env: &ProfileEnv,
) -> TrustState {
    store.state(discover(root, home, &Limits::default(), profile, env))
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
        EntryKind::LinkedFile { target, sha256, .. } => format!("linked:{}:{}", super::hex(sha256), target.display()),
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

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn each_profile_has_its_own_temporary_record_and_reads_back() {
        let scratch = crate::plugin::test_support::Scratch::new("trust-record-profiles");
        let root = scratch.0.join("root");
        std::fs::create_dir_all(root.join(".codex")).unwrap();
        std::fs::write(root.join(".codex/config.toml"), "model = 'test'\n").unwrap();
        std::fs::write(root.join("AGENTS.md"), "instructions\n").unwrap();
        let store = TrustStore::at(Some(scratch.0.join("trust")));
        let acp = Profile::Acp {
            name: "demo".into(),
            files: vec!["AGENTS.md".into()],
        };
        let profiles = [Profile::Claude, Profile::Codex, acp];
        let paths: Vec<_> = profiles
            .iter()
            .map(|profile| store.record_path(&TrustPlace::Local, &root, profile).unwrap())
            .collect();
        let temps: Vec<_> = paths.iter().map(|path| record_temporary(path)).collect();
        for (path, temp) in paths.iter().zip(&temps) {
            assert_eq!(temp.parent(), path.parent());
            assert_eq!(
                temp.file_name().unwrap(),
                format!(
                    "{}.{}.tmp",
                    path.file_name().unwrap().to_str().unwrap(),
                    std::process::id()
                )
                .as_str()
            );
            assert_eq!(temp.extension(), Some(OsStr::new("tmp")));
        }
        assert_eq!(temps.iter().collect::<std::collections::BTreeSet<_>>().len(), 3);
        let discoveries: Vec<_> = profiles[1..]
            .iter()
            .map(|profile| {
                discover(
                    &root,
                    Some(&scratch.0),
                    &Limits::default(),
                    profile,
                    &ProfileEnv::default(),
                )
            })
            .collect();
        for discovery in &discoveries {
            store.record(discovery, 1).unwrap();
        }
        for discovery in discoveries {
            assert!(matches!(store.state(discovery), TrustState::Trusted { .. }));
        }
    }

    /// A link to a file and a link to a directory named after that file's hash are different
    /// configurations, so a change from one to the other is named.
    #[test]
    fn a_linked_file_never_reads_as_a_link_to_a_directory() {
        let sha256 = [7; 32];
        let file = EntryKind::LinkedFile {
            target: PathBuf::from("../docs/f"),
            sha256,
            len: 1,
        };
        let dir = EntryKind::Symlink {
            target: PathBuf::from(format!("../docs/f:{}", crate::project_trust::hex(&sha256))),
            outside: false,
        };
        assert_ne!(value_of(&file), value_of(&dir));
    }

    /// What `record` is left with when another window's `:untrust` lands between the record and the
    /// copy.
    #[test]
    fn a_copy_written_after_its_record_went_is_taken_back() {
        let scratch = crate::plugin::test_support::Scratch::new("trust-copy-after-forget");
        let root = scratch.0.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("CLAUDE.md"), "instructions\n").unwrap();
        let store = TrustStore::at(Some(scratch.0.join("trust")));
        let found = discover(
            &root,
            Some(&scratch.0),
            &Limits::default(),
            &Profile::Claude,
            &ProfileEnv::default(),
        );
        store.record(&found, 1).unwrap();
        let record = store.record_path(&TrustPlace::Local, &root, &Profile::Claude).unwrap();
        assert!(copy_path(&record).is_file());
        std::fs::remove_file(&record).unwrap();
        keep_copy(&TrustPlace::Local, &record, &found);
        assert!(!copy_path(&record).exists());
    }

    fn remote(host_key: &str, root: &Path) -> TrustPlace {
        TrustPlace::Remote(RemoteRoot {
            host_key_sha256: host_key.to_string(),
            root: root.to_path_buf(),
        })
    }

    /// A configuration at `root`, found as a server's helper finds it (the same `discover`), with a
    /// hook in it.
    fn found_with_a_hook(scratch: &Path, root: &Path, command: &str) -> Discovery {
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            format!(r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":"{command}"}}]}}]}}}}"#),
        )
        .unwrap();
        discover(
            root,
            Some(scratch),
            &Limits::default(),
            &Profile::Claude,
            &ProfileEnv::default(),
        )
    }

    /// A remote project's record is found only under the host key and root it was written for: this
    /// machine's own record for the same path, and a record for another host key, are not it.
    #[test]
    fn remote_trust_is_keyed_by_host_key_and_root() {
        let scratch = crate::plugin::test_support::Scratch::new("trust-remote-keyed");
        let root = scratch.0.join("project");
        let found = found_with_a_hook(&scratch.0, &root, "true");
        let store = TrustStore::at(Some(scratch.0.join("trust")));
        let here = remote("SHA256:host-a", &root);

        // This machine's record for the same path does not answer for the server.
        store.record(&found, 1).unwrap();
        assert!(matches!(store.state(found.clone()), TrustState::Trusted { .. }));
        assert!(matches!(
            store.state_at(&here, found.clone()),
            TrustState::Untrusted { .. }
        ));

        // A record for the server answers for it and for nothing else: not for another key, not for
        // another root on the same server.
        store.record_at(&here, &found, 2).unwrap();
        assert!(matches!(
            store.state_at(&here, found.clone()),
            TrustState::Trusted { since_unix: 2, .. }
        ));
        assert!(matches!(
            store.state_at(&remote("SHA256:host-b", &root), found.clone()),
            TrustState::Untrusted { .. }
        ));
        assert!(matches!(
            store.state_at(&remote("SHA256:host-a", &scratch.0.join("elsewhere")), found.clone()),
            TrustState::Untrusted { .. }
        ));

        // Forgetting one leaves the other.
        assert!(store.forget(&root, &Profile::Claude).unwrap());
        assert!(matches!(store.state(found.clone()), TrustState::Untrusted { .. }));
        assert!(matches!(
            store.state_at(&here, found.clone()),
            TrustState::Trusted { .. }
        ));
        assert!(store.forget_at(&here, &root, &Profile::Claude).unwrap());
        assert!(matches!(store.state_at(&here, found), TrustState::Untrusted { .. }));
    }

    /// A local record and a remote one for the same path never match each other, even when one lies
    /// under the other's name.
    #[test]
    fn a_local_and_a_remote_record_for_the_same_path_never_match() {
        let scratch = crate::plugin::test_support::Scratch::new("trust-local-vs-remote");
        let root = scratch.0.join("project");
        let found = found_with_a_hook(&scratch.0, &root, "true");
        let store = TrustStore::at(Some(scratch.0.join("trust")));
        let server = remote("SHA256:host-a", &root);
        let local_path = store.record_path(&TrustPlace::Local, &root, &Profile::Claude).unwrap();
        let remote_path = store.record_path(&server, &root, &Profile::Claude).unwrap();
        assert_ne!(local_path, remote_path);

        // A local record copied to the remote name names no host key: not the server's.
        store.record(&found, 1).unwrap();
        std::fs::copy(&local_path, &remote_path).unwrap();
        assert!(matches!(
            store.state_at(&server, found.clone()),
            TrustState::Untrusted { .. }
        ));
        std::fs::remove_file(&remote_path).unwrap();

        // A remote record copied to the local name names a host key: not this machine's.
        store.forget(&root, &Profile::Claude).unwrap();
        store.record_at(&server, &found, 1).unwrap();
        std::fs::copy(&remote_path, &local_path).unwrap();
        assert!(matches!(store.state(found.clone()), TrustState::Untrusted { .. }));
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&remote_path).unwrap()).unwrap();
        assert_eq!(written["host_key_sha256"], "SHA256:host-a");
        assert_eq!(written["root"], root.to_str().unwrap());
    }

    /// A remote record is exact to the directory the scan found the recorded root to be: the same root
    /// leading to another tree there, even one holding the same configuration, is asked about again.
    #[test]
    fn a_remote_record_answers_only_for_the_directory_it_was_scanned_in() {
        let scratch = crate::plugin::test_support::Scratch::new("trust-remote-canonical");
        let first = found_with_a_hook(&scratch.0, &scratch.0.join("a"), "true");
        let second = found_with_a_hook(&scratch.0, &scratch.0.join("b"), "true");
        assert_eq!(first.fingerprint, second.fingerprint);
        let store = TrustStore::at(Some(scratch.0.join("trust")));
        let place = remote("SHA256:host-a", Path::new("/srv/project"));
        store.record_at(&place, &first, 1).unwrap();
        assert!(matches!(
            store.state_at(&place, first.clone()),
            TrustState::Trusted { .. }
        ));
        assert!(matches!(store.state_at(&place, second), TrustState::Untrusted { .. }));
        let path = store.record_path(&place, &first.root, &Profile::Claude).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(written["root"], "/srv/project");
        assert_eq!(written["scanned_root"], first.root.to_str().unwrap());
        // `:untrust` needs no scan: the record is found by the recorded root alone.
        assert!(store
            .forget_at(&place, Path::new("/elsewhere"), &Profile::Claude)
            .unwrap());
    }

    /// The host key ssh verified is part of the identity: a key that rotates is a new place, asked about
    /// again rather than read as a change to what was trusted.
    #[test]
    fn rotated_host_key_is_a_new_identity() {
        let scratch = crate::plugin::test_support::Scratch::new("trust-remote-rotated");
        let root = scratch.0.join("project");
        let found = found_with_a_hook(&scratch.0, &root, "true");
        let store = TrustStore::at(Some(scratch.0.join("trust")));
        store.record_at(&remote("SHA256:old", &root), &found, 1).unwrap();
        assert!(matches!(
            store.state_at(&remote("SHA256:new", &root), found),
            TrustState::Untrusted {
                remember: Remember::Yes,
                ..
            }
        ));
    }

    #[test]
    fn a_remote_record_is_named_by_host_key_and_root() {
        let store = TrustStore::new(Some(OsStr::new("/state")), Some(OsStr::new("/home/test")));
        let dir = store.dir.unwrap();
        let root = Path::new("/srv/project");
        let place = remote("SHA256:example", root);
        use sha2::{Digest, Sha256};
        let expected = crate::project_trust::hex(&Sha256::digest(b"eitri-remote-trust\0SHA256:example\0/srv/project"));
        assert_eq!(
            dir.join(record_name(&place, root, &Profile::Claude)),
            Path::new(&format!("/state/eitri/trust/{}.json", &expected[..16])),
        );
        assert_eq!(
            dir.join(record_name(&place, Path::new("/elsewhere"), &Profile::Codex)),
            Path::new(&format!("/state/eitri/trust/{}.json.codex", &expected[..16])),
        );
        assert_ne!(
            record_name(&place, root, &Profile::Claude),
            record_name(&TrustPlace::Local, root, &Profile::Claude)
        );
    }

    #[test]
    fn every_state_path_is_unchanged() {
        let store = TrustStore::new(Some(OsStr::new("/state")), Some(OsStr::new("/home/test")));
        let dir = store.dir.unwrap();
        let root = Path::new("/projects/state-paths");
        assert_eq!(
            dir.join(record_name(&TrustPlace::Local, root, &Profile::Claude)),
            Path::new("/state/eitri/trust/410d926fb96fb107.json"),
        );
        assert_eq!(
            dir.join(record_name(&TrustPlace::Local, root, &Profile::Codex)),
            Path::new("/state/eitri/trust/410d926fb96fb107.json.codex"),
        );
    }
}
