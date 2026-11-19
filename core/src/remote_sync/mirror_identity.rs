//! Which laptop directory a remote project syncs, and which `.git` in it is Eitri's.
//!
//! A sync session's history describes one directory. If another directory comes to stand at the same path (the
//! mirror deleted or moved away and a fresh checkout put there, a copy-based restore), resuming the old sessions
//! over it would delete on the server what the new directory lacks and overwrite what it changes, and the git replica
//! would replace the new directory's history with the server's. So the record keeps the root's identity as first
//! connect prepared it: device, inode and birth time. Device and inode alone do not tell a folder deleted and made
//! again from the old one, since ext4 and XFS hand a freed inode number to the next directory made; the birth time is
//! new. Nothing resumes, creates a session or writes into `.git` unless the directory at the path is still that one.
//! The same holds for `.git`: Eitri writes its config into, and replicates onto, only the `.git` it made itself; a
//! `.git` the person made (plain `git init` or a clone while git commands ran on the server) is never touched.
//!
//! What Eitri retires instead of deleting (a replica it no longer runs, an interrupted rebase left in its copy) is
//! moved under `<state home>/eitri/remote/retired-git/`, outside every mirror, where no sync reaches it.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::local_git::LocalGit;
use super::SyncKey;
use crate::remote_project::{Birth, DirIdentity, RemoteProject, RemoteProjects, SyncRecord};

/// What stands at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Missing,
    Dir(DirIdentity),
    /// A file, a symbolic link or anything else that is not a real directory.
    Other,
}

/// Looks at `path` without following a link.
pub fn look(path: &Path) -> io::Result<Found> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(Found::Dir(measured(path, &meta))),
        Ok(_) => Ok(Found::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Found::Missing),
        Err(e) => Err(e),
    }
}

/// The identity `meta` gives the directory at `path`. The birth time is `statx`'s, which Rust reports as the creation
/// time; a file system that keeps none (some network and FUSE file systems) gives `Absent`.
pub(crate) fn measured(path: &Path, meta: &std::fs::Metadata) -> DirIdentity {
    let birth = match meta.created().map(|at| at.duration_since(std::time::UNIX_EPOCH)) {
        Ok(Ok(since)) => Birth::At {
            sec: since.as_secs() as i64,
            nsec: since.subsec_nanos(),
        },
        Ok(Err(before)) => {
            let before = before.duration();
            Birth::At {
                sec: -(before.as_secs() as i64),
                nsec: before.subsec_nanos(),
            }
        }
        Err(_) => Birth::Absent,
    };
    #[allow(unused_mut)]
    let mut identity = DirIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
        birth,
    };
    #[cfg(test)]
    if let Some(&(dev, ino)) = test_reuse::REUSED.lock().unwrap().get(path) {
        identity.dev = dev;
        identity.ino = ino;
    }
    #[cfg(not(test))]
    let _ = path;
    identity
}

/// Tests stand in for a file system that hands a freed inode number (or a bind mount's source identity) to another
/// directory: the directory at the path reports that device and inode, whatever the test host's file system does.
#[cfg(test)]
pub(crate) mod test_reuse {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    pub(crate) static REUSED: std::sync::LazyLock<Mutex<HashMap<PathBuf, (u64, u64)>>> =
        std::sync::LazyLock::new(Default::default);

    /// Roots whose state home stands on another file system: a rename from the root into it fails with `EXDEV`.
    pub(crate) static CROSS_DEVICE: std::sync::LazyLock<Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::LazyLock::new(Default::default);

    pub(crate) fn report(path: &Path, dev: u64, ino: u64) {
        REUSED.lock().unwrap().insert(path.to_owned(), (dev, ino));
    }

    /// Waits past the kernel's creation-time tick. Before Linux 6.13 a directory's birth time comes from the coarse
    /// clock (one jiffy, up to 4 ms), so a directory deleted and made again within microseconds gets the same birth
    /// time; a real mirror is made again seconds or days after its record was written, never in the same tick.
    pub(crate) fn after_a_clock_tick() {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// The identity of the directory at `path`, if a real directory stands there.
pub fn identity(path: &Path) -> Option<DirIdentity> {
    match look(path) {
        Ok(Found::Dir(identity)) => Some(identity),
        _ => None,
    }
}

/// How a directory found now compares with a recorded identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The recorded directory.
    Same,
    /// Nothing proves either way: the record has no birth time (an earlier build's), one side's file system keeps
    /// none, or only the device number changed where no birth time backs the inode (network and FUSE file systems
    /// number their devices at mount). The person says, through `eitri host open`.
    Ask,
    /// Another directory.
    Different,
}

/// Compares a recorded identity with one found now. A different inode is another directory. With birth times on
/// both sides, the same inode and birth time are the same directory whatever the device number says: device numbers
/// of btrfs subvolumes, NFS, FUSE and some block devices change across mounts and boots, while an inode reused after a
/// delete gets a new birth time. Without birth times, the same device and inode are taken as the same directory, which
/// is all such a file system can tell (an inode reused there is not detected).
pub fn compare(recorded: &DirIdentity, found: &DirIdentity) -> Verdict {
    if recorded.ino != found.ino {
        return Verdict::Different;
    }
    match (recorded.birth, found.birth) {
        (Birth::At { .. }, Birth::At { .. }) if recorded.birth == found.birth => Verdict::Same,
        (Birth::At { .. }, Birth::At { .. }) => Verdict::Different,
        (Birth::Absent, Birth::Absent) if recorded.dev == found.dev => Verdict::Same,
        _ => Verdict::Ask,
    }
}

/// Whether the record's sessions may act on the folder now at `root`: `None` when they may, else the line that says
/// why not. A record with no sessions and no identity has nothing to protect yet (its first session records it); a
/// record with sessions and no identity was set up by an earlier build, which noted no folder, and waits for the
/// person to confirm the folder with `eitri host open`.
pub fn root_refusal(record: &SyncRecord, host: &str, root: &Path) -> Option<String> {
    let Some(recorded) = record.root_identity else {
        return record.has_sessions().then(|| unconfirmed_text(host, root));
    };
    match identity(root).map(|found| compare(&recorded, &found)) {
        Some(Verdict::Same) => None,
        Some(Verdict::Ask) => Some(unconfirmed_text(host, root)),
        Some(Verdict::Different) | None => Some(replaced_text(host, root)),
    }
}

pub fn replaced_text(host: &str, root: &Path) -> String {
    format!(
        "sync of {root} is paused: the folder there is not the copy Eitri synced with {host} (it was deleted, moved, \
         replaced or restored from a backup since), and syncing it could delete or overwrite files on {host}. Run \
         eitri host open {host}:{root} to set this folder up again; it asks before moving anything",
        root = root.display()
    )
}

pub fn unconfirmed_text(host: &str, root: &Path) -> String {
    format!(
        "sync of {root} is paused: Eitri cannot tell whether the folder there is still the copy it synced with {host} \
         (an earlier Eitri noted too little about it, or its disk now reports another device). Run eitri host open \
         {host}:{root} to confirm it is still that copy",
        root = root.display()
    )
}

/// The record of another project whose root holds `root` or lies inside it, said as a refusal. Two mirrors that nest
/// would carry one server's files into the other's folder and from there to the other server; the hosts do not
/// matter. Roots compare by their recorded spelling, their canonical paths and the identities of their directories
/// and ancestors, so neither a link nor a bind mount (one directory under two unrelated paths) hides the overlap. Only
/// records that sync (set up, or naming a session) count; a record whose sync value cannot be read counts too, and so
/// does a record file that cannot be read at all (damaged, or a newer build's): nothing says it is not a mirror around
/// this one.
pub fn overlapping(store: &RemoteProjects, root: &Path) -> Result<Option<String>, String> {
    let (projects, unreadable) = store.list().map_err(|e| e.to_string())?;
    if let Some(line) = unreadable.first() {
        return Ok(Some(format!(
            "sync not started: a remote project record cannot be read ({line}), so Eitri cannot tell whether its \
             folder holds or lies inside {}. Every remote project's sync waits for it: repair that file, or remove it \
             (this machine then forgets that project, its queued agent stops and its sync setup; nothing on the \
             server changes; eitri host uninstall <host> removes every record of a host). eitri host status lists \
             the records it cannot read",
            root.display()
        )));
    }
    Ok(overlap_among(&projects, root))
}

fn overlap_among(projects: &[RemoteProject], root: &Path) -> Option<String> {
    let spellings = |path: &Path| {
        let mut forms = vec![path.to_owned()];
        if let Ok(canonical) = path.canonicalize() {
            if canonical != path {
                forms.push(canonical);
            }
        }
        forms
    };
    let mine = spellings(root);
    let my_chain = chain(root);
    for other in projects {
        if other.root == root || !syncs(other) {
            continue;
        }
        let theirs = spellings(&other.root);
        let their_chain = chain(&other.root);
        let same =
            theirs.iter().any(|t| mine.contains(t)) || (my_chain.own.is_some() && my_chain.own == their_chain.own);
        let inside = mine.iter().any(|m| theirs.iter().any(|t| m.starts_with(t)))
            || their_chain
                .own
                .is_some_and(|theirs| my_chain.ancestors.contains(&theirs));
        let holds = mine.iter().any(|m| theirs.iter().any(|t| t.starts_with(m)))
            || my_chain.own.is_some_and(|mine| their_chain.ancestors.contains(&mine));
        let how = if same {
            format!("{} is the same folder as {}", root.display(), other.root.display())
        } else if inside {
            format!("{} is inside {}", root.display(), other.root.display())
        } else if holds {
            format!("{} is inside {}", other.root.display(), root.display())
        } else {
            continue;
        };
        return Some(format!(
            "sync not started: {how}, and {} is synced with {}; a mirror inside another would carry one server's files \
             into the other. Use a folder that neither holds nor lies inside another remote project",
            other.root.display(),
            other.host
        ));
    }
    None
}

/// The device and inode of a directory and of each of its existing ancestors, nearest first, following links as a
/// path lookup does. A bind mount shows its source directory's device and inode.
struct Chain {
    /// The path's own; `None` when it does not exist (a first connect names a folder it is about to make).
    own: Option<(u64, u64)>,
    /// Its ancestors', looked up even when the path itself does not exist, so a folder still to be made under another
    /// mount of a mirror is found inside it.
    ancestors: Vec<(u64, u64)>,
}

fn chain(path: &Path) -> Chain {
    let own = std::fs::metadata(path).ok().map(|meta| id_of(path, &meta));
    let ancestors = path
        .ancestors()
        .skip(1)
        .filter_map(|ancestor| std::fs::metadata(ancestor).ok().map(|meta| id_of(ancestor, &meta)))
        .collect();
    Chain { own, ancestors }
}

fn id_of(path: &Path, meta: &std::fs::Metadata) -> (u64, u64) {
    let identity = measured(path, meta);
    (identity.dev, identity.ino)
}

fn syncs(project: &RemoteProject) -> bool {
    match project.sync_record() {
        Ok(Some(record)) => record.authorized(),
        Ok(None) => false,
        Err(_) => true,
    }
}

/// The laptop `.git` as the actor finds it before writing its config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitDir {
    Missing,
    /// Eitri's: the recorded one, or one holding nothing but what Eitri writes.
    Ours(DirIdentity),
    /// The person's (or anything that is not a real directory): left alone.
    Foreign,
}

/// Judges `<root>/.git`. It is Eitri's when it is the recorded directory and its config names Eitri's hooks
/// directory (only Eitri writes that value, and the replica never brings a config, so a repository the person made
/// in its place has its own); when it holds only Eitri's own config and that config's temporaries (a write that
/// stopped before the record noted the directory: nothing of the person's is there); or, when the record cannot
/// prove the directory either way (an earlier build's record), when a replica is recorded and the config names
/// Eitri's hooks directory. Anything else is someone else's.
pub fn git_dir(local_git: &LocalGit, root: &Path, record: &SyncRecord, hooks: &Path) -> io::Result<GitDir> {
    let git_dir = root.join(".git");
    let identity = match look(&git_dir)? {
        Found::Missing => return Ok(GitDir::Missing),
        Found::Other => return Ok(GitDir::Foreign),
        Found::Dir(identity) => identity,
    };
    let only_eitri = holds_only_eitri_files(&git_dir);
    let config = git_dir.join("config");
    let eitri_config =
        || std::fs::symlink_metadata(&config).is_ok_and(|meta| meta.is_file()) && names_hooks(local_git, root, hooks);
    let leftovers = only_eitri && (!config.exists() || eitri_config());
    let adopt = leftovers
        || match record.git_identity.map(|recorded| compare(&recorded, &identity)) {
            Some(Verdict::Same) => eitri_config(),
            Some(Verdict::Ask) | None => record.git_session.is_some() && eitri_config(),
            Some(Verdict::Different) => false,
        };
    Ok(if adopt { GitDir::Ours(identity) } else { GitDir::Foreign })
}

/// Whether `git_dir` holds nothing but Eitri's own config and that config's temporaries (or nothing at all): no
/// history the replica brought and nothing plain git wrote. Unreadable counts as holding something.
pub fn holds_only_eitri_files(git_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(git_dir) else {
        return false;
    };
    entries.into_iter().all(|entry| {
        entry.is_ok_and(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == "config" || (name.starts_with("eitri-config-") && name.ends_with(".lock"))
        })
    })
}

/// Whether the `.git` a running replica writes into is no longer the one recorded (replaced, deleted, made again):
/// a cheap look, with no git run, for the holder's every step.
pub fn git_dir_moved(record: &SyncRecord, root: &Path) -> bool {
    let (Some(_), Some(recorded)) = (&record.git_session, record.git_identity) else {
        return false;
    };
    match look(&root.join(".git")) {
        Ok(Found::Dir(found)) => compare(&recorded, &found) != Verdict::Same,
        _ => true,
    }
}

/// Whether `<root>/.git/config` sets `core.hooksPath` to Eitri's hooks directory: only Eitri writes that value.
fn names_hooks(local_git: &LocalGit, root: &Path, hooks: &Path) -> bool {
    let args: Vec<std::ffi::OsString> = vec![
        "config".into(),
        "--file".into(),
        root.join(".git/config").into(),
        "--get".into(),
        "core.hookspath".into(),
    ];
    local_git
        .command(root, &args)
        .and_then(|mut command| command.current_dir(root).stdin(std::process::Stdio::null()).output())
        .is_ok_and(|out| {
            use std::os::unix::ffi::OsStrExt;
            out.status.success()
                && out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout) == hooks.as_os_str().as_bytes()
        })
}

/// What a rebase, a multi-commit cherry-pick or revert leaves in `.git` while it runs. The replica never brings
/// these (they are among its ignores), so on the laptop they only come from plain git run on the copy, which can
/// neither continue nor abort there (both need a ref update the copy refuses).
pub const OPERATION_DIRS: &[&str] = &["rebase-merge", "rebase-apply", "sequencer"];

/// How many retired replicas are kept per project: the newest. A replica is a copy of the server's history plus
/// whatever plain git wrote into it on the laptop; a server that keeps refusing and allowing a replica would otherwise
/// leave a full copy each time.
pub const RETIRED_REPLICAS_KEPT: usize = 3;

/// `<state home>/eitri/remote/retired-git`: where retired replicas and interrupted operations are kept.
pub fn retired_dir(state_home: &Path) -> PathBuf {
    state_home.join("eitri/remote/retired-git")
}

/// Moves `<root>/.git` out of the mirror, whole, to `<sync key>-<ms>-git` under [`retired_dir`], and removes this
/// project's older retired replicas beyond [`RETIRED_REPLICAS_KEPT`]. Returns where it went.
pub fn retire_git_dir(state_home: &Path, key: &SyncKey, root: &Path) -> io::Result<PathBuf> {
    let source = root.join(".git");
    let dest = fresh(state_home, key, "git")?;
    #[cfg(test)]
    let moved = if test_reuse::CROSS_DEVICE.lock().unwrap().contains(root) {
        Err(io::Error::from_raw_os_error(libc::EXDEV))
    } else {
        std::fs::rename(&source, &dest)
    };
    #[cfg(not(test))]
    let moved = std::fs::rename(&source, &dest);
    match moved {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let beside = beside_root(root)?;
            std::fs::rename(&source, &beside)?;
            Ok(beside)
        }
        Err(e) => Err(e),
        Ok(()) => {
            prune_replicas(state_home, key);
            Ok(dest)
        }
    }
}

/// Removes this project's retired replicas beyond the newest [`RETIRED_REPLICAS_KEPT`]. Interrupted operations
/// (`-ops`), which can hold text the person wrote, are never removed.
fn prune_replicas(state_home: &Path, key: &SyncKey) {
    let Ok(entries) = std::fs::read_dir(retired_dir(state_home)) else {
        return;
    };
    let prefix = format!("{key}-");
    let mut replicas: Vec<((u64, u32), PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stamp = name.strip_prefix(&prefix)?.strip_suffix("-git")?;
            let (ms, n) = match stamp.split_once('-') {
                Some((ms, n)) => (ms.parse().ok()?, n.parse().ok()?),
                None => (stamp.parse().ok()?, 0),
            };
            Some(((ms, n), entry.path()))
        })
        .collect();
    replicas.sort();
    let surplus = replicas.len().saturating_sub(RETIRED_REPLICAS_KEPT);
    for (_, path) in replicas.into_iter().take(surplus) {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Moves each of [`OPERATION_DIRS`] found in `<root>/.git` into one new directory, `<sync key>-<ms>-ops`. They are
/// moved, not deleted: a rebase's directory holds the todo list and the message the person may have been editing.
/// `None` when there was none.
pub fn retire_operations(state_home: &Path, key: &SyncKey, root: &Path) -> io::Result<Option<PathBuf>> {
    let git_dir = root.join(".git");
    let found: Vec<&str> = OPERATION_DIRS
        .iter()
        .copied()
        .filter(|name| std::fs::symlink_metadata(git_dir.join(name)).is_ok())
        .collect();
    if found.is_empty() {
        return Ok(None);
    }
    let mut dest = fresh(state_home, key, "ops")?;
    std::fs::create_dir(&dest)?;
    for name in found {
        match std::fs::rename(git_dir.join(name), dest.join(name)) {
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                let _ = std::fs::remove_dir(&dest);
                dest = beside_root(root)?;
                std::fs::create_dir(&dest)?;
                std::fs::rename(git_dir.join(name), dest.join(name))?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            Ok(()) => {}
        }
    }
    Ok(Some(dest))
}

/// A path under [`retired_dir`] that does not exist yet: `<sync key>-<ms>-<kind>`, with `-<n>` after the time when
/// that is taken.
fn fresh(state_home: &Path, key: &SyncKey, kind: &str) -> io::Result<PathBuf> {
    let parent = retired_dir(state_home);
    agent::private_fs::create_private_dir_all(&parent, &state_home.join("eitri"))?;
    let stamp = super::status_file::now_unix_ms();
    Ok(unused(|n| {
        parent.join(format!("{}-{kind}", numbered(&format!("{key}-{stamp}"), n)))
    }))
}

/// Beside the root when the state home is on another file system: `<root>.eitri-retired-git-<ms>`, outside the
/// mirror, where the tree session does not reach.
fn beside_root(root: &Path) -> io::Result<PathBuf> {
    let stamp = super::status_file::now_unix_ms();
    let mut base = root.as_os_str().to_owned();
    base.push(format!(".eitri-retired-git-{stamp}"));
    let base = base.to_string_lossy().into_owned();
    Ok(unused(|n| PathBuf::from(numbered(&base, n))))
}

fn numbered(base: &str, n: u32) -> String {
    if n == 0 {
        base.to_owned()
    } else {
        format!("{base}-{n}")
    }
}

fn unused(path: impl Fn(u32) -> PathBuf) -> PathBuf {
    (0..)
        .map(path)
        .find(|candidate| std::fs::symlink_metadata(candidate).is_err())
        .expect("some numbered name is free")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;

    fn project(root: &Path, host: &str, record: Option<SyncRecord>) -> RemoteProject {
        let mut project = RemoteProject {
            version: 1,
            host: host.into(),
            host_key_sha256: "SHA256:k".into(),
            root: root.to_owned(),
            helper_version: "v".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        };
        if let Some(record) = record {
            project.set_sync_record(&record);
        }
        project
    }

    fn set_up() -> SyncRecord {
        SyncRecord {
            engine: "mutagen".into(),
            set_up: true,
            host_key_sha256: "SHA256:k".into(),
            ..Default::default()
        }
    }

    #[test]
    fn nested_roots_of_any_host_overlap_in_both_directions() {
        let a = project(Path::new("/x/a"), "host-a", Some(set_up()));
        let refused = overlap_among(std::slice::from_ref(&a), Path::new("/x/a/b")).unwrap();
        assert!(refused.contains("/x/a/b is inside /x/a"), "{refused}");
        assert!(refused.contains("host-a"), "{refused}");
        let refused = overlap_among(std::slice::from_ref(&a), Path::new("/x")).unwrap();
        assert!(refused.contains("/x/a is inside /x"), "{refused}");
        // The same root, a sibling and a name that only shares a prefix are not nested.
        for other in ["/x/a", "/x/b", "/x/ab"] {
            assert_eq!(
                overlap_among(std::slice::from_ref(&a), Path::new(other)),
                None,
                "{other}"
            );
        }
        // A record that does not sync does not count; one whose sync value cannot be read does.
        let idle = project(Path::new("/x/a"), "host-a", None);
        assert_eq!(overlap_among(&[idle], Path::new("/x/a/b")), None);
        let mut unreadable = project(Path::new("/x/a"), "host-a", None);
        unreadable.sync = Some(serde_json::json!({"engine": 1}));
        assert!(overlap_among(&[unreadable], Path::new("/x/a/b")).is_some());
    }

    /// A record this build cannot read (damaged, or written by a newer build) may be a mirror around this one: it
    /// refuses, naming the file, rather than being skipped.
    #[test]
    fn an_unreadable_project_record_counts_as_overlapping() {
        let scratch = ScratchDir::new("identity", "unreadable");
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        store
            .save(&project(&scratch.join("elsewhere"), "host-a", Some(set_up())))
            .unwrap();
        assert_eq!(overlapping(&store, &scratch.join("mine")).unwrap(), None);
        std::fs::write(scratch.join("eitri/hosts/0123456789abcdef.json"), b"{\"version\": 99").unwrap();
        let refused = overlapping(&store, &scratch.join("mine")).unwrap().unwrap();
        assert!(refused.contains("0123456789abcdef.json"), "{refused}");
    }

    /// A bind mount shows one directory under two unrelated paths: nesting is found by the directories' identities,
    /// not only by path text.
    #[test]
    fn a_root_reached_through_another_mount_of_a_mirror_overlaps() {
        let scratch = ScratchDir::new("identity", "bind");
        let work = scratch.join("work");
        let alias = scratch.join("alias");
        std::fs::create_dir_all(work.join("project")).unwrap();
        std::fs::create_dir_all(alias.join("project/sub")).unwrap();
        // `alias` stands for a bind mount of `work`: its directories report `work`'s identities.
        for rel in ["", "project"] {
            let source = std::fs::metadata(work.join(rel)).unwrap();
            test_reuse::report(
                &alias.join(rel).components().collect::<PathBuf>(),
                source.dev(),
                source.ino(),
            );
        }
        let recorded = project(&work.join("project"), "host-a", Some(set_up()));
        let refused = overlap_among(std::slice::from_ref(&recorded), &alias.join("project/sub")).unwrap();
        assert!(refused.contains("host-a"), "{refused}");
    }

    /// A first connect names a laptop folder that does not exist yet, and a recorded mirror's folder may have gone:
    /// neither may stop the check, and a folder still to be made under another mount of a mirror still overlaps.
    #[test]
    fn a_missing_root_on_either_side_is_compared_without_failing() {
        let scratch = ScratchDir::new("identity", "missing");
        let present = scratch.join("present");
        std::fs::create_dir_all(&present).unwrap();
        let recorded = project(&present, "host-a", Some(set_up()));
        assert_eq!(
            overlap_among(std::slice::from_ref(&recorded), &scratch.join("absent")),
            None
        );
        let gone = project(&scratch.join("gone"), "host-a", Some(set_up()));
        assert_eq!(overlap_among(&[gone], &present), None);
        let refused = overlap_among(std::slice::from_ref(&recorded), &present.join("absent/deeper")).unwrap();
        assert!(refused.contains("host-a"), "{refused}");

        let work = scratch.join("work");
        let alias = scratch.join("alias");
        std::fs::create_dir_all(work.join("project")).unwrap();
        std::fs::create_dir_all(alias.join("project")).unwrap();
        for rel in ["", "project"] {
            let source = std::fs::metadata(work.join(rel)).unwrap();
            test_reuse::report(
                &alias.join(rel).components().collect::<PathBuf>(),
                source.dev(),
                source.ino(),
            );
        }
        let mirror = project(&work.join("project"), "host-b", Some(set_up()));
        let refused = overlap_among(std::slice::from_ref(&mirror), &alias.join("project/not-yet")).unwrap();
        assert!(refused.contains("host-b"), "{refused}");
    }

    /// Retired replicas are bounded per project: the newest few are kept, older ones go.
    #[test]
    fn retired_replicas_keep_only_the_newest() {
        let scratch = ScratchDir::new("identity", "bound");
        let state = scratch.join("state");
        let root = scratch.join("project");
        let key = SyncKey::new("SHA256:k", &root);
        for n in 0..5 {
            std::fs::create_dir_all(root.join(".git")).unwrap();
            std::fs::write(root.join(".git/HEAD"), format!("{n}")).unwrap();
            retire_git_dir(&state, &key, &root).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let kept: Vec<String> = std::fs::read_dir(retired_dir(&state))
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path().join("HEAD")).unwrap())
            .collect();
        assert_eq!(kept.len(), 3, "{kept:?}");
        for newest in ["2", "3", "4"] {
            assert!(kept.iter().any(|k| k == newest), "{kept:?}");
        }
    }

    /// With the state home on another file system, a retired `.git` goes beside the root, never inside it.
    #[test]
    fn a_retired_git_dir_goes_beside_the_root_when_the_state_home_is_on_another_file_system() {
        let scratch = ScratchDir::new("identity", "cross-device");
        let state = scratch.join("state");
        let root = scratch.join("project");
        let key = SyncKey::new("SHA256:k", &root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        test_reuse::CROSS_DEVICE.lock().unwrap().insert(root.clone());
        let moved = retire_git_dir(&state, &key, &root).unwrap();
        assert_eq!(moved.parent(), root.parent());
        assert!(moved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("project.eitri-retired-git-"));
        assert!(!root.join(".git").exists());
        assert_eq!(std::fs::read(moved.join("HEAD")).unwrap(), b"ref: refs/heads/main\n");
        assert!(std::fs::read_dir(retired_dir(&state)).unwrap().next().is_none());
    }

    #[test]
    fn a_link_to_another_mirror_overlaps_through_its_canonical_path() {
        let scratch = ScratchDir::new("identity", "canonical");
        let outer = scratch.join("outer");
        std::fs::create_dir_all(outer.join("inner")).unwrap();
        std::os::unix::fs::symlink(outer.join("inner"), scratch.join("link")).unwrap();
        let recorded = project(&outer, "host-a", Some(set_up()));
        assert!(overlap_among(&[recorded], &scratch.join("link")).is_some());
    }

    #[test]
    fn identities_compare_by_inode_and_birth_and_ask_where_nothing_proves_it() {
        let at = |dev, ino, sec| DirIdentity {
            dev,
            ino,
            birth: Birth::At { sec, nsec: 5 },
        };
        let absent = |dev, ino| DirIdentity {
            dev,
            ino,
            birth: Birth::Absent,
        };
        let unknown = |dev, ino| DirIdentity {
            dev,
            ino,
            birth: Birth::Unknown,
        };
        assert_eq!(compare(&at(1, 2, 10), &at(1, 2, 10)), Verdict::Same);
        // Another device number at the same inode and birth: a remount or reboot renumbered it.
        assert_eq!(compare(&at(1, 2, 10), &at(9, 2, 10)), Verdict::Same);
        // The inode handed back to a directory made later.
        assert_eq!(compare(&at(1, 2, 10), &at(1, 2, 11)), Verdict::Different);
        assert_eq!(compare(&at(1, 2, 10), &at(1, 3, 10)), Verdict::Different);
        assert_eq!(compare(&absent(1, 2), &absent(1, 2)), Verdict::Same);
        assert_eq!(compare(&absent(1, 2), &absent(9, 2)), Verdict::Ask);
        assert_eq!(compare(&absent(1, 2), &absent(1, 3)), Verdict::Different);
        assert_eq!(compare(&unknown(1, 2), &at(1, 2, 10)), Verdict::Ask);
        assert_eq!(compare(&at(1, 2, 10), &absent(1, 2)), Verdict::Ask);
        // What a record keeps reads back the same.
        let kept: DirIdentity = serde_json::from_value(serde_json::to_value(at(1, 2, 10)).unwrap()).unwrap();
        assert_eq!(compare(&at(1, 2, 10), &kept), Verdict::Same);
        let earlier: DirIdentity = serde_json::from_value(serde_json::json!({"dev": 1, "ino": 2})).unwrap();
        assert_eq!(earlier.birth, Birth::Unknown);
    }

    #[test]
    fn the_root_check_needs_the_recorded_folder() {
        let scratch = ScratchDir::new("identity", "root");
        let root = scratch.join("project");
        std::fs::create_dir(&root).unwrap();
        let mut record = set_up();
        assert_eq!(
            root_refusal(&record, "h", &root),
            None,
            "nothing recorded and no session yet"
        );
        record.tree_session = Some("t".into());
        let unconfirmed = root_refusal(&record, "h", &root).unwrap();
        assert!(unconfirmed.contains("eitri host open h:"), "{unconfirmed}");
        record.root_identity = identity(&root);
        assert_eq!(root_refusal(&record, "h", &root), None);
        std::fs::rename(&root, scratch.join("aside")).unwrap();
        std::fs::create_dir(&root).unwrap();
        let replaced = root_refusal(&record, "h", &root).unwrap();
        assert!(replaced.contains("is not the copy Eitri synced"), "{replaced}");
        std::fs::remove_dir(&root).unwrap();
        assert!(
            root_refusal(&record, "h", &root).is_some(),
            "a missing folder is not the recorded one"
        );
    }

    #[test]
    fn operations_and_a_retired_git_dir_leave_the_mirror_and_are_kept() {
        let scratch = ScratchDir::new("identity", "retire");
        let state = scratch.join("state");
        let root = scratch.join("project");
        let key = SyncKey::new("SHA256:k", &root);
        std::fs::create_dir_all(root.join(".git/rebase-merge")).unwrap();
        std::fs::write(root.join(".git/rebase-merge/message"), b"the person's words").unwrap();
        std::fs::create_dir_all(root.join(".git/sequencer")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        let kept = retire_operations(&state, &key, &root).unwrap().unwrap();
        assert!(kept.starts_with(retired_dir(&state)));
        assert_eq!(
            std::fs::read(kept.join("rebase-merge/message")).unwrap(),
            b"the person's words"
        );
        assert!(kept.join("sequencer").is_dir());
        assert!(!root.join(".git/rebase-merge").exists() && !root.join(".git/sequencer").exists());
        assert!(root.join(".git/HEAD").exists());
        assert_eq!(retire_operations(&state, &key, &root).unwrap(), None);

        let moved = retire_git_dir(&state, &key, &root).unwrap();
        assert!(moved.starts_with(retired_dir(&state)) && moved != kept);
        assert!(!root.join(".git").exists());
        assert_eq!(std::fs::read(moved.join("HEAD")).unwrap(), b"ref: refs/heads/main\n");
    }
}
