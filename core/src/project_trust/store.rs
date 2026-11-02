//! Eitri's own record of which project configuration the user trusted, one file per root and agent.
//!
//! Claude's record is `<state home>/eitri/trust/<16 hex of the root>.json`; every other agent adds
//! `.<profile key>` (`<16 hex>.json.codex`) and names its profile in the record. Each holds the root,
//! trust time, configuration fingerprint and per-file hashes so changes can be named file by file.
//! Trust is exact to the root and agent: parents, children and other agents have separate records.
//! No other program's notion of trust is read.
//!
//! Beside a record lies `<record>~last-trusted`, the instruction files as that `y` saw them
//! ([`super::last_trusted`]). It is written after the record, removed with it, and reads as nothing
//! when it is missing or does not match: only the record decides. A profile's key never holds a `~`,
//! so no agent's record can have that name.
//!
//! A record that cannot be used (missing, not JSON, another version, another root, owned by
//! someone else, a link, a FIFO, or written after a prompt that showed less than this one does)
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
}

/// The record's file name for `root` and `profile`. Keep Claude's name stable so existing records
/// still answer; every other profile adds its key to that name.
fn record_name(root: &Path, profile: &Profile) -> String {
    match profile {
        Profile::Claude => state_paths::file_name(root),
        other => format!("{}.{}", state_paths::file_name(root), other.key()),
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

/// The record at `path` when it is a usable one for `root` and `profile`, read without following
/// a link.
fn read_usable_at(path: &Path, root: &Path, profile: &Profile) -> Option<Record> {
    let bytes = read_regular_nofollow(path, MAX_RECORD_BYTES, Owner::Me).ok()?;
    let record: Record = serde_json::from_slice(&bytes).ok()?;
    let usable = record.version == 1
        && record.disclosure >= DISCLOSURE
        && record.profile == profile_field(profile)
        && root.to_str() == Some(record.root.as_str())
        && record.fingerprint.len() == 64
        && record.fingerprint.bytes().all(|b| b.is_ascii_hexdigit());
    usable.then_some(record)
}

/// What a record for `profile` says it is about: nothing for Claude, else the profile's key.
fn profile_field(profile: &Profile) -> Option<String> {
    match profile {
        Profile::Claude => None,
        other => Some(other.key()),
    }
}

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
    fn record_path(&self, root: &Path, profile: &Profile) -> Option<PathBuf> {
        self.real_dir().map(|dir| dir.join(record_name(root, profile)))
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
        if discovery.nothing_to_trust() && !discovery.profile.requires_explicit_yes() {
            return TrustState::NothingToTrust(discovery);
        }
        let remember = self.remember_for(&discovery);
        let Some(record) = self.read_usable(&discovery.root, &discovery.profile) else {
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
        let copy = self.read_copy(&discovery);
        let texts = last_trusted::compare(&record.files, copy.as_ref(), &discovery, &diff);
        TrustState::Changed {
            discovery,
            diff,
            remember,
            texts,
        }
    }

    /// The copy kept at the last `y`, read as the record is, when it is one for this root and agent.
    fn read_copy(&self, discovery: &Discovery) -> Option<last_trusted::KeptFiles> {
        let record = self.record_path(&discovery.root, &discovery.profile)?;
        let bytes = read_regular_nofollow(&copy_path(&record), MAX_KEPT_BYTES, Owner::Me).ok()?;
        last_trusted::decode(&bytes, &discovery.root, profile_field(&discovery.profile))
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
        // Sized for the latest time a record can carry, so a record written later is never larger.
        let size = record_text(discovery, u64::MAX).map_or(u64::MAX, |text| text.len() as u64);
        if size > MAX_RECORD_BYTES {
            return Remember::WindowOnly(too_large_to_record(discovery, size));
        }
        Remember::Yes
    }

    fn read_usable(&self, root: &Path, profile: &Profile) -> Option<Record> {
        read_usable_at(&self.record_path(root, profile)?, root, profile)
    }

    /// Writes the record for `discovery`: a temporary file beside it, 0600, renamed over the
    /// record, and then the copy of its instruction files. Refuses a discovery that could not hash
    /// everything, and a store with no directory, writing nothing. A directory that cannot be created
    /// or used is the error; a copy that cannot be written is not.
    pub fn record(&self, discovery: &Discovery, now_unix: u64) -> io::Result<()> {
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
        let path = dir.join(record_name(&discovery.root, &discovery.profile));
        let text = record_text(discovery, now_unix)?;
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
        let reads_back = read_usable_at(&path, &discovery.root, &discovery.profile)
            .is_some_and(|read| read.fingerprint == fingerprint);
        if !reads_back {
            return Err(io::Error::other(format!(
                "the trust record {} could not be read back",
                path.display()
            )));
        }
        keep_copy(&path, discovery);
        Ok(())
    }

    /// Removes this root's record for `profile` and the copy beside it when each is a regular file the
    /// user owns; `true` if a record existed. A foreign file or a link is left in place and named in
    /// the error. The copy is still removed when the record cannot be, and the other way round.
    pub fn forget(&self, root: &Path, profile: &Profile) -> io::Result<bool> {
        let Some(path) = self.record_path(root, profile) else {
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
fn keep_copy(record: &Path, discovery: &Discovery) {
    let path = copy_path(record);
    let kept = last_trusted::encode(discovery, profile_field(&discovery.profile));
    let written = kept.is_some_and(|text| replace_private(&path, &text).is_ok());
    // Another window's `:untrust` can have taken the record since it was read back; its text goes too.
    let fingerprint = discovery.fingerprint.hex();
    let record_stands =
        read_usable_at(record, &discovery.root, &discovery.profile).is_some_and(|read| read.fingerprint == fingerprint);
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

/// The record of `discovery` as written, trusted at `now_unix`.
fn record_text(discovery: &Discovery, now_unix: u64) -> io::Result<String> {
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
        profile: profile_field(&discovery.profile),
        disclosure: DISCLOSURE,
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
            .map(|profile| store.record_path(&root, profile).unwrap())
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
        let record = store.record_path(&root, &Profile::Claude).unwrap();
        assert!(copy_path(&record).is_file());
        std::fs::remove_file(&record).unwrap();
        keep_copy(&record, &found);
        assert!(!copy_path(&record).exists());
    }

    #[test]
    fn every_state_path_is_unchanged() {
        let store = TrustStore::new(Some(OsStr::new("/state")), Some(OsStr::new("/home/test")));
        let dir = store.dir.unwrap();
        let root = Path::new("/projects/state-paths");
        assert_eq!(
            dir.join(record_name(root, &Profile::Claude)),
            Path::new("/state/eitri/trust/410d926fb96fb107.json"),
        );
        assert_eq!(
            dir.join(record_name(root, &Profile::Codex)),
            Path::new("/state/eitri/trust/410d926fb96fb107.json.codex"),
        );
    }
}
