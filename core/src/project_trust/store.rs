//! Eitri's own record of which project configuration the user trusted, one file per root and agent.
//!
//! Claude's record is `<state home>/eitri/trust/<16 hex of the root>.json`; every other agent's is
//! that name followed by `.<profile key>` (`<16 hex>.json.codex`), and says which profile it is. Each
//! holds the root it is about, when it was made, the fingerprint of the configuration that was shown
//! and each file's own hash, so a later change can be named file by file. A root is only ever trusted
//! by its own record for that agent -- a record for a parent or a child directory, or for another
//! agent, is a different file -- and no other program's notion of trust is read.
//!
//! A record that cannot be used (missing, not JSON, another version, another root, owned by
//! someone else, a link, a FIFO, or written after a prompt that showed less than this one does)
//! reads as untrusted and is left where it is: another user may have
//! planted it, and renaming it aside would only move their file around. Answering `y` replaces it
//! atomically instead; the rename puts a new file in the name's place and never follows a planted
//! link.
//!
//! Everything here that touches the disk (`state`, `record`, `forget`, `for_root`) is for the trust
//! gate's worker thread only: a slow or hostile state directory must never stall the window.
//! [`TrustStore::new`] and [`TrustStore::at`] touch nothing, so the window may build one anywhere.
//!
//! The record is read without following any link from `/` down, but the state home may well be
//! reached through one (`/home` a link to `/var/home`, a linked `~/.local/state`). So each of those
//! calls first resolves the links of the store's directory, once, and writes and reads the record at
//! that real path; a record is also read back right after it is written, and one that does not read
//! back as written is an error rather than a "trusted" that the next start would not see.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::nofollow::{read_regular_nofollow, Owner};
use super::{discover, Discovery, EntryKind, Limits, Profile, ProfileEnv};
use crate::state_paths;

/// A record larger than this is not one of ours. The record is read on every check, from a state
/// directory another program of the user's could fill, so the reader stays small; a configuration
/// whose record would be larger (about 1,900 files with long names) is not recorded at all, and its
/// prompt says from the start that a `y` lasts for this window only ([`Remember::WindowOnly`]): its
/// fingerprint covers everything, so the window may keep the answer, but no record can.
const MAX_RECORD_BYTES: u64 = 1 << 20;

/// How much the prompt a record answered showed. A `y` is about what the person saw, not only about
/// the bytes the fingerprint covers: an earlier prompt cut long values short, gave `env` names
/// without their values and left out some fields of MCP servers and hooks, so a `y` given to it
/// covered less than the same configuration shows now. A record of a lower disclosure (or none,
/// which every record written before this field existed has) is not trust, and the project is
/// asked about once more. Raise this whenever the prompt starts showing something it hid before.
///
/// 2: every value whole. 3: each file beyond the settings, memory and rule files (a command, an
/// agent, a skill's file, a hook script) named with its size and the frontmatter keys that change
/// what runs, where 2 gave only how many there were in each directory. 4: that note also names an
/// agent file's own `permissionMode` and `mcpServers`, which 3 left unsaid. 5: Claude's prompt says
/// that a subdirectory's `CLAUDE.md`, which the walk does not read, loads once Claude reads a file there.
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
    },
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    root: String,
    trusted_at: u64,
    fingerprint: String,
    files: BTreeMap<String, String>,
    /// The profile's key for every agent but Claude, whose records predate profiles and carry none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    /// [`DISCLOSURE`] when the record was written; 0 for a record from before the field existed.
    #[serde(default)]
    disclosure: u32,
}

/// The record's file name for `root` and `profile`. Claude's record keeps the name it had before
/// profiles existed, so a record made then still answers; every other profile's adds its key to
/// that name.
fn record_name(root: &Path, profile: &Profile) -> String {
    match profile {
        Profile::Claude => state_paths::file_name(root),
        other => format!("{}.{}", state_paths::file_name(root), other.key()),
    }
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
    /// record. Refuses a discovery that could not hash everything, and a store with no directory,
    /// writing nothing. A directory that cannot be created or used is the error.
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
        let tmp = record_temporary(&path);
        let written =
            agent::private_fs::write_private(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(err);
        }
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
        Ok(())
    }

    /// Removes this root's record for `profile` when it is a regular file the user owns; `true` if
    /// one existed. A foreign file or a link is left in place and named in the error.
    pub fn forget(&self, root: &Path, profile: &Profile) -> io::Result<bool> {
        let Some(path) = self.record_path(root, profile) else {
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
