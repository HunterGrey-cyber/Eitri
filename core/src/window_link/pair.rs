//! The files of a linked pair. Window 1 keeps today's names, so an older build still reads them; window 2 writes
//! `-w2` files and a window that runs unpaired writes `-u` files. One linked pair per project: the window holding
//! `<hex>.pair.lock` is the pair's, and only it reads and writes the pair file, which records which window holds
//! which module and each window's monitor.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::names::WindowName;
use crate::state_paths::project_key;

pub const PAIR_FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Window 1 of a pair, or a window alone: today's file names.
    Main,
    /// Window 2.
    Second,
    /// A full window on a project whose pair another window holds.
    Unpaired,
}

impl Role {
    fn suffix(self) -> &'static str {
        match self {
            Role::Main => "",
            Role::Second => "-w2",
            Role::Unpaired => "-u",
        }
    }
}

/// In `<state>/eitri/layout/`.
pub fn layout_file_name(project_root: &Path, role: Role) -> String {
    format!("{}{}.v2.json", project_key(project_root), role.suffix())
}

/// In `<state>/eitri/tabs/`.
pub fn tabs_file_name(project_root: &Path, role: Role) -> String {
    format!("{}{}.json", project_key(project_root), role.suffix())
}

/// In `<state>/eitri/layout/`.
pub fn pair_file_name(project_root: &Path) -> String {
    format!("{}.pair.json", project_key(project_root))
}

/// In `<state>/eitri/layout/`.
pub fn pair_lock_name(project_root: &Path) -> String {
    format!("{}.pair.lock", project_key(project_root))
}

/// In `<state>/eitri/layout/`.
pub fn group_claim_name(project_root: &Path) -> String {
    format!("{}.group.lock", project_key(project_root))
}

/// Window 2's log, in `<state>/eitri/log/`.
pub fn log_file_name(project_root: &Path) -> String {
    log_file_name_for(project_root, &WindowName::Second)
}

/// In `<state>/eitri/log/`.
pub fn log_file_name_for(project_root: &Path, name: &WindowName) -> String {
    format!("window-{}-{}.log", name.file_form(), project_key(project_root))
}

/// An exclusive `flock` on the pair's lock file. The lock belongs to the open file description, so every copy of
/// the descriptor (window 2 gets one in `welcome`) keeps it held, and it goes only when the last copy closes.
#[derive(Debug)]
pub struct PairLock {
    fd: OwnedFd,
}

impl PairLock {
    /// `Ok(None)` while another window holds the pair.
    pub fn try_take(layout_dir: &Path, project_root: &Path) -> io::Result<Option<PairLock>> {
        let taken = exclusive_flock(layout_dir, &pair_lock_name(project_root))?;
        Ok(taken.map(|fd| PairLock { fd }))
    }

    /// The lock as the other window of the pair passed it: the same open file description, already locked.
    pub fn adopt(fd: OwnedFd) -> io::Result<PairLock> {
        Self::check(&fd)?;
        Ok(PairLock { fd })
    }

    fn check(fd: &OwnedFd) -> io::Result<()> {
        let meta = std::fs::File::from(fd.try_clone()?).metadata()?;
        // SAFETY: `geteuid` cannot fail.
        if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the pair lock is not a regular file of this user",
            ));
        }
        Ok(())
    }

    /// A copy of the descriptor, to pass in `welcome`.
    pub fn share(&self) -> io::Result<OwnedFd> {
        self.fd.try_clone()
    }
}

impl AsFd for PairLock {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// An exclusive `flock` that names the one window 1 of a group. The pair lock cannot do this: every window of a group
/// holds a copy of its open file description, so it stays held when window 1 dies, and two windows that each adopt a
/// copy from an orphan do not exclude each other. This descriptor never leaves its process, so the claim ends with its
/// window, and a window that took the group over from orphans holds it for as long as it runs: a later start finds it
/// held and leaves the orphans that missed the first window's bound alone.
#[derive(Debug)]
pub struct GroupClaim {
    _fd: OwnedFd,
}

impl GroupClaim {
    /// `Ok(None)` while another window leads the group or is taking it over.
    pub fn try_take(layout_dir: &Path, project_root: &Path) -> io::Result<Option<GroupClaim>> {
        let taken = exclusive_flock(layout_dir, &group_claim_name(project_root))?;
        Ok(taken.map(|fd| GroupClaim { _fd: fd }))
    }
}

/// The descriptor of `name` in `layout_dir` under an exclusive `flock`; `Ok(None)` while another open file description
/// holds it. The file must be a regular file of this user, reached through no link.
fn exclusive_flock(layout_dir: &Path, name: &str) -> io::Result<Option<OwnedFd>> {
    crate::state_paths::create_state_dir(layout_dir)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(layout_dir.join(name))?;
    let fd = OwnedFd::from(file);
    PairLock::check(&fd)?;
    // SAFETY: `fd` is open for the call.
    if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(fd));
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(err)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monitor {
    pub connector: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairFile {
    pub version: u32,
    /// Whether window 2 was open when the pair last ended.
    #[serde(default)]
    pub window2_open: bool,
    /// Module id to the window holding it (`"2"`); a module not named here is window 1's.
    #[serde(default)]
    pub holders: BTreeMap<String, String>,
    /// Window name (`"1"`, `"2"`) to its monitor.
    #[serde(default)]
    pub monitors: BTreeMap<String, Monitor>,
}

impl Default for PairFile {
    fn default() -> Self {
        PairFile {
            version: PAIR_FILE_VERSION,
            window2_open: false,
            holders: BTreeMap::new(),
            monitors: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadPair {
    Missing,
    Read(PairFile),
    /// Present and not usable, with why; the caller logs it and goes on as if missing.
    Unusable(String),
}

pub fn read_pair(layout_dir: &Path, project_root: &Path) -> ReadPair {
    let path = layout_dir.join(pair_file_name(project_root));
    match agent::private_fs::read_private_to_string(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => ReadPair::Missing,
        Err(err) => ReadPair::Unusable(format!("{}: {err}", path.display())),
        Ok(text) => match serde_json::from_str::<PairFile>(&text) {
            Ok(file) if file.version == PAIR_FILE_VERSION => ReadPair::Read(file),
            Ok(file) => ReadPair::Unusable(format!("{}: version {}", path.display(), file.version)),
            Err(err) => ReadPair::Unusable(format!("{}: {err}", path.display())),
        },
    }
}

/// Written whole to a temporary file beside it, then renamed into place.
pub fn write_pair(layout_dir: &Path, project_root: &Path, file: &PairFile) -> io::Result<()> {
    crate::state_paths::create_state_dir(layout_dir)?;
    let path = layout_dir.join(pair_file_name(project_root));
    // This process's own temporary name: window 1 and window 2 may both be writing for a moment as the pair file
    // passes from one to the other, and a shared name would let one rename the other's half-written file.
    let tmp = layout_dir.join(format!("{}.{}.tmp", pair_file_name(project_root), std::process::id()));
    let text = serde_json::to_string_pretty(file).expect("a pair file serialises");
    let written = agent::private_fs::write_private(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, &path));
    if written.is_err() {
        // Named after this process, a temporary left behind is never written over by a later write.
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Renames `path` to `<path>.taken` once its content is safely elsewhere, and returns the new path.
pub fn mark_taken(path: &Path) -> io::Result<PathBuf> {
    let mut name = path.as_os_str().to_owned();
    name.push(".taken");
    let taken = PathBuf::from(name);
    std::fs::rename(path, &taken)?;
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use std::os::unix::fs::PermissionsExt;

    fn root() -> PathBuf {
        PathBuf::from("/home/someone/project")
    }

    /// The pair lock, taken once it comes free. A lock whose last descriptor here was just closed can stay held
    /// for a moment: a child another test thread is starting holds a copy of every descriptor of this process
    /// until it execs.
    fn take_once_free(dir: &Path) -> Option<PairLock> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match PairLock::try_take(dir, &root()).unwrap() {
                Some(lock) => return Some(lock),
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                None => return None,
            }
        }
    }

    #[test]
    fn each_role_names_its_files_beside_window_ones() {
        let hex = &agent::conversation_id_for_cwd(&root())[..16];
        assert_eq!(
            layout_file_name(&root(), Role::Main),
            crate::layout::persist::file_name_v2(&root())
        );
        assert_eq!(layout_file_name(&root(), Role::Second), format!("{hex}-w2.v2.json"));
        assert_eq!(layout_file_name(&root(), Role::Unpaired), format!("{hex}-u.v2.json"));
        assert_eq!(tabs_file_name(&root(), Role::Main), format!("{hex}.json"));
        assert_eq!(tabs_file_name(&root(), Role::Second), format!("{hex}-w2.json"));
        assert_eq!(tabs_file_name(&root(), Role::Unpaired), format!("{hex}-u.json"));
        assert_eq!(pair_file_name(&root()), format!("{hex}.pair.json"));
        assert_eq!(pair_lock_name(&root()), format!("{hex}.pair.lock"));
        assert_eq!(log_file_name(&root()), format!("window-2-{hex}.log"));
    }

    #[test]
    fn one_window_takes_the_pair_and_another_finds_it_held() {
        let dir = ScratchDir::new("nv-pair", "take");
        let first = PairLock::try_take(&dir, &root()).unwrap();
        assert!(first.is_some());
        assert!(PairLock::try_take(&dir, &root()).unwrap().is_none());
        drop(first);
        assert!(take_once_free(&dir).is_some());
    }

    #[test]
    fn the_lock_stays_held_while_any_copy_of_its_descriptor_lives() {
        let dir = ScratchDir::new("nv-pair", "share");
        let first = PairLock::try_take(&dir, &root()).unwrap().unwrap();
        let second = PairLock::adopt(first.share().unwrap()).unwrap();
        drop(first);
        assert!(
            PairLock::try_take(&dir, &root()).unwrap().is_none(),
            "the shared copy keeps it"
        );
        drop(second);
        assert!(take_once_free(&dir).is_some());
    }

    #[test]
    fn a_link_in_the_locks_place_is_refused() {
        let dir = ScratchDir::new("nv-pair", "link");
        std::os::unix::fs::symlink("/dev/null", dir.join(pair_lock_name(&root()))).unwrap();
        assert!(PairLock::try_take(&dir, &root()).is_err());
    }

    #[test]
    fn one_window_claims_the_group_whoever_holds_the_pair_lock() {
        let dir = ScratchDir::new("nv-pair", "claim");
        let pair = PairLock::try_take(&dir, &root()).unwrap().unwrap();
        let first = GroupClaim::try_take(&dir, &root()).unwrap();
        assert!(first.is_some(), "the claim is a lock of its own");
        assert!(GroupClaim::try_take(&dir, &root()).unwrap().is_none());
        drop(first);
        // A child another test thread is starting holds a copy of the claim's descriptor, and with it the lock, until
        // it execs.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while GroupClaim::try_take(&dir, &root()).unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "the claim never came free");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(pair);
        assert_eq!(
            group_claim_name(&root()),
            format!("{}.group.lock", &agent::conversation_id_for_cwd(&root())[..16])
        );
    }

    #[test]
    fn a_link_in_the_claims_place_is_refused() {
        let dir = ScratchDir::new("nv-pair", "claim-link");
        std::os::unix::fs::symlink("/dev/null", dir.join(group_claim_name(&root()))).unwrap();
        assert!(GroupClaim::try_take(&dir, &root()).is_err());
    }

    #[test]
    fn the_pair_file_round_trips_privately_and_unknown_keys_are_ignored() {
        let dir = ScratchDir::new("nv-pair", "file");
        assert_eq!(read_pair(&dir, &root()), ReadPair::Missing);
        let mut file = PairFile {
            window2_open: true,
            ..PairFile::default()
        };
        file.holders.insert("terminal#2".into(), "2".into());
        file.monitors.insert(
            "2".into(),
            Monitor {
                connector: "DP-2".into(),
                description: "Dell".into(),
            },
        );
        write_pair(&dir, &root(), &file).unwrap();
        assert_eq!(read_pair(&dir, &root()), ReadPair::Read(file.clone()));
        let path = dir.join(pair_file_name(&root()));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!dir
            .join(format!("{}.{}.tmp", pair_file_name(&root()), std::process::id()))
            .exists());
        let mut value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["later"] = serde_json::json!(1);
        std::fs::write(&path, value.to_string()).unwrap();
        assert_eq!(read_pair(&dir, &root()), ReadPair::Read(file));
    }

    #[test]
    fn garbage_or_another_version_is_unusable() {
        let dir = ScratchDir::new("nv-pair", "bad");
        let path = dir.join(pair_file_name(&root()));
        std::fs::write(&path, "{").unwrap();
        assert!(matches!(read_pair(&dir, &root()), ReadPair::Unusable(_)));
        std::fs::write(&path, r#"{"version":2}"#).unwrap();
        assert!(matches!(read_pair(&dir, &root()), ReadPair::Unusable(why) if why.contains("version 2")));
    }

    #[test]
    fn mark_taken_renames_beside() {
        let dir = ScratchDir::new("nv-pair", "taken");
        let path = dir.join("x-w2.json");
        std::fs::write(&path, "{}").unwrap();
        let taken = mark_taken(&path).unwrap();
        assert_eq!(taken, dir.join("x-w2.json.taken"));
        assert!(!path.exists() && taken.exists());
    }
}
