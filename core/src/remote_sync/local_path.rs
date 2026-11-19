//! Preparing the laptop directory without overwriting a person's existing copy.

use super::local_git::LocalGit;
use crate::remote_host::cli::Terminal;
use crate::remote_project::DirIdentity;
use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

fn shell_word(text: &str) -> String {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-._+".contains(&b)) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

fn creation_error(root: &Path, error: io::Error) -> String {
    let user = std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| shell_word(&s))
        .unwrap_or_else(|| "\"$(id -un)\"".into());
    let root = shell_word(&root.to_string_lossy());
    format!("{error}; create it once: `sudo mkdir -p {root} && sudo chown {user} {root}`")
}

const ENTRY_LIMIT: usize = 1_000_000;

fn list(root: &Path, git: Option<&LocalGit>, term: &mut dyn Terminal) -> Result<(), String> {
    match fs::symlink_metadata(root.join(".git")) {
        Ok(meta) if meta.is_dir() => {}
        Ok(meta) if meta.is_file() => {
            term.say(".git is a gitfile pointing to another directory; counting files without following it.");
            return list_files(root, ENTRY_LIMIT, term);
        }
        Ok(_) => return Err("cannot inspect .git: expected a real Git directory or gitfile".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return list_files(root, ENTRY_LIMIT, term),
        Err(e) => return Err(format!("cannot inspect .git: {e}")),
    }
    let git = git.ok_or_else(|| "cannot list the existing repository: git is not installed".to_owned())?;
    for (label, args) in [
        ("Uncommitted changes", &["status", "--porcelain"][..]),
        ("Stashes", &["stash", "list"]),
        (
            "Commits on local branches absent from remote-tracking branches",
            &["log", "--branches", "--not", "--remotes", "--oneline"],
        ),
    ] {
        let args: Vec<_> = args.iter().map(OsString::from).collect();
        let out = git
            .command(root, &args)
            .and_then(|mut command| command.current_dir(root).output())
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "cannot list {label}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        term.say(label);
        for line in lines.by_ref().take(50) {
            term.say(line);
        }
        let more = lines.count();
        if more > 0 {
            term.say(&format!("and {more} more"));
        }
    }
    Ok(())
}

fn list_files(root: &Path, limit: usize, term: &mut dyn Terminal) -> Result<(), String> {
    let mut dirs = vec![root.to_owned()];
    let (mut files, mut bytes) = (0u64, 0u64);
    let (mut unreadable_dirs, mut unreadable_entries) = (0, 0);
    let mut remaining = limit;
    let mut truncated = false;
    'walk: while let Some(dir) = dirs.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if dir == root => return Err(format!("cannot list {}: {e}", dir.display())),
            Err(_) => {
                unreadable_dirs += 1;
                continue;
            }
        };
        for entry in entries {
            if remaining == 0 {
                truncated = true;
                break 'walk;
            }
            remaining -= 1;
            match entry.and_then(|entry| Ok((entry.metadata()?, entry))) {
                Ok((meta, entry)) if meta.is_dir() => dirs.push(entry.path()),
                Ok((meta, _)) => {
                    files += 1;
                    bytes = bytes.saturating_add(meta.len());
                }
                Err(_) => unreadable_entries += 1,
            }
        }
    }
    let bound = if truncated || unreadable_dirs + unreadable_entries > 0 {
        "at least "
    } else {
        ""
    };
    term.say(&format!(
        "{} contains {bound}{files} files, {bytes} bytes counted",
        root.display()
    ));
    if unreadable_dirs + unreadable_entries > 0 {
        term.say(&format!(
            "Unreadable directories: {unreadable_dirs}, unreadable entries: {unreadable_entries}"
        ));
    }
    Ok(())
}

fn stamp(seconds: u64) -> Result<String, String> {
    let seconds: libc::time_t = seconds.try_into().map_err(|_| "invalid date")?;
    let mut tm = std::mem::MaybeUninit::uninit();
    // SAFETY: both pointers outlive the call; localtime_r initializes the result on success.
    if unsafe { libc::localtime_r(&seconds, tm.as_mut_ptr()) }.is_null() {
        return Err("invalid date".into());
    }
    let tm = unsafe { tm.assume_init() };
    Ok(format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    ))
}

/// Rename without replacement, including when another invocation just used the same timestamp.
fn move_aside(from: &Path, to: &Path) -> Result<(), String> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(format!(
            "cannot move {} without replacement on this platform",
            from.display()
        ))
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let source = CString::new(from.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let dest = CString::new(to.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: source and dest are live, NUL-terminated pathnames.
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                dest.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe { libc::renamex_np(source.as_ptr(), dest.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            return Ok(());
        }
        Err(rename_error(from, to, io::Error::last_os_error()))
    }
}

fn rename_error(from: &Path, to: &Path, error: io::Error) -> String {
    if error.raw_os_error() == Some(libc::EXDEV) {
        format!(
            "cannot move {} across filesystems; the original copy is kept",
            from.display()
        )
    } else {
        format!("cannot move {} to {}: {error}", from.display(), to.display())
    }
}

pub struct Move {
    dest: PathBuf,
    dev: u64,
    ino: u64,
}

/// Prepares the directory and returns a move only after explicit consent.
pub fn inspect(
    root: &Path,
    own: bool,
    now: u64,
    git: Option<&LocalGit>,
    term: &mut dyn Terminal,
) -> Result<Option<Move>, String> {
    inspect_before_move(root, own, now, git, term, || Ok(()))
}

/// The terminal acquires the root lock before listing a move and keeps it through setup.
pub(crate) fn inspect_before_move(
    root: &Path,
    own: bool,
    now: u64,
    git: Option<&LocalGit>,
    term: &mut dyn Terminal,
    before_move: impl FnOnce() -> Result<(), String>,
) -> Result<Option<Move>, String> {
    let parent = root.parent().ok_or("cannot use the filesystem root as a mirror")?;
    fs::create_dir_all(parent).map_err(|e| creation_error(root, e))?;
    if parent.canonicalize().map_err(|e| creation_error(root, e))? != parent {
        return Err(format!("{} leads elsewhere on this machine", parent.display()));
    }
    let meta = match fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(root).map_err(|e| creation_error(root, e))?;
            return Ok(None);
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(creation_error(root, e)),
        Err(e) => return Err(format!("cannot check {}: {e}", root.display())),
    };
    if !meta.is_dir() {
        return Err(format!("{} is not a real directory", root.display()));
    }
    if own
        || fs::read_dir(root)
            .and_then(|mut d| d.next().transpose())
            .map_err(|e| e.to_string())?
            .is_none()
    {
        return Ok(None);
    }
    before_move()?;
    list(root, git, term)?;
    let mut dest = root.as_os_str().to_owned();
    dest.push(format!(".eitri-local-{}", stamp(now)?));
    let dest = PathBuf::from(dest);
    if !term.ask(&format!(
        "Move {} to {} and keep that copy? [y/N]",
        root.display(),
        dest.display()
    )) {
        return Err(format!("{} stays where it is; sync was not started", root.display()));
    }
    Ok(Some(Move {
        dest,
        dev: meta.dev(),
        ino: meta.ino(),
    }))
}

fn check_devices(root: &Path, source_device: u64, dest_device: u64) -> Result<(), String> {
    if source_device != dest_device {
        return Err(format!("cannot move {} across filesystems", root.display()));
    }
    Ok(())
}

/// Moves an approved copy aside (or checks the directory is still empty, or still the mirror `own` names) and
/// returns the identity of the directory sync will use, which the sync record keeps. The identity is measured once,
/// from the directory held open, and that same measurement is both checked and returned: looking the path up again
/// for the answer could record a directory put there in between.
pub fn prepare(
    root: &Path,
    own: Option<DirIdentity>,
    approved: Option<Move>,
    term: &mut dyn Terminal,
) -> Result<DirIdentity, String> {
    let changed = || format!("{} changed while preparing sync", root.display());
    if root.canonicalize().map_err(|e| e.to_string())? != root {
        return Err(changed());
    }
    let held = open_dir(root).map_err(|_| changed())?;
    let meta = held.metadata().map_err(|e| e.to_string())?;
    if let Some(approved) = approved {
        if (meta.dev(), meta.ino()) != (approved.dev, approved.ino) {
            return Err(format!("{} changed since the move was approved", root.display()));
        }
        let parent = approved.dest.parent().ok_or("move destination has no parent")?;
        check_devices(root, meta.dev(), fs::metadata(parent).map_err(|e| e.to_string())?.dev())?;
        move_aside(root, &approved.dest)?;
        term.say(&format!("Kept the existing copy at {}", approved.dest.display()));
        fs::create_dir(root).map_err(|e| creation_error(root, e))?;
        let made = open_dir(root).map_err(|_| changed())?;
        let meta = made.metadata().map_err(|e| e.to_string())?;
        return Ok(super::mirror_identity::measured(root, &meta));
    }
    let found = super::mirror_identity::measured(root, &meta);
    if let Some(expected) = own {
        if super::mirror_identity::compare(&expected, &found) != super::mirror_identity::Verdict::Same {
            return Err(changed());
        }
    } else if fs::read_dir(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&held)))
        .or_else(|_| fs::read_dir(root))
        .and_then(|mut d| d.next().transpose())
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err(format!(
            "{} is no longer empty; run eitri host open again",
            root.display()
        ));
    }
    Ok(found)
}

/// `path` opened as a directory, not through a link at its last component.
fn open_dir(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;

    #[derive(Default)]
    struct Term {
        yes: bool,
        lines: Vec<String>,
    }
    impl Terminal for Term {
        fn ask(&mut self, line: &str) -> bool {
            self.say(line);
            self.yes
        }
        fn say(&mut self, line: &str) {
            self.lines.push(line.into());
        }
    }

    #[test]
    fn uncreatable_parent_prints_the_sudo_line() {
        let scratch = ScratchDir::new("local", "parent");
        let parent = scratch.join("file");
        fs::write(&parent, b"kept").unwrap();
        let why = inspect(&parent.join("project"), false, 0, None, &mut Term::default())
            .err()
            .unwrap();
        assert!(
            why.contains(&format!(
                "create it once: `sudo mkdir -p {} && sudo chown ",
                parent.join("project").display()
            )),
            "{why}"
        );
    }

    #[test]
    fn unwritable_existing_parent_suggests_chown_of_the_project_only() {
        use std::os::unix::fs::PermissionsExt;
        if agent::private_fs::euid() == 0 {
            return;
        }
        let scratch = ScratchDir::new("local", "unwritable");
        let parent = scratch.join("parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
        let root = parent.join("project");
        let why = inspect(&root, false, 0, None, &mut Term::default()).err().unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            why.contains(&format!("sudo mkdir -p {} && sudo chown ", root.display())),
            "{why}"
        );
        assert!(why.ends_with(&format!(" {}`", root.display())), "{why}");
    }

    #[test]
    fn gitfile_uses_the_file_count_without_following_it() {
        let root = ScratchDir::new("local", "gitfile");
        fs::write(root.join(".git"), "gitdir: ../elsewhere").unwrap();
        fs::write(root.join("notes"), b"local").unwrap();
        let mut term = Term {
            yes: true,
            ..Default::default()
        };
        assert!(inspect(&root, false, 0, None, &mut term).unwrap().is_some());
        assert!(term.lines[0].contains("gitfile"));
        assert!(term.lines[1].contains("2 files"));
    }

    #[test]
    fn count_stops_at_the_entry_limit() {
        let root = ScratchDir::new("local", "count");
        for name in ["a", "b", "c"] {
            fs::write(root.join(name), b"1").unwrap();
        }
        let mut term = Term::default();
        list_files(&root, 2, &mut term).unwrap();
        assert!(term.lines[0].contains("at least 2 files, 2 bytes"));
    }

    #[test]
    fn unreadable_subdirectory_is_reported_before_asking() {
        use std::os::unix::fs::PermissionsExt;
        if agent::private_fs::euid() == 0 {
            return;
        }
        let root = ScratchDir::new("local", "unreadable");
        let private = root.join("private");
        fs::create_dir(&private).unwrap();
        fs::write(root.join("readable"), b"123").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0)).unwrap();
        let mut term = Term {
            yes: true,
            ..Default::default()
        };
        let result = inspect(&root, false, 0, None, &mut term);
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.unwrap().is_some());
        assert!(term.lines[0].contains("at least 1 files, 3 bytes"));
        assert!(term.lines[1].contains("Unreadable directories: 1"));
    }

    #[test]
    fn empty_existing_dir_is_used() {
        let root = ScratchDir::new("local", "empty");
        let mut term = Term::default();
        let inode = fs::metadata(&root).unwrap().ino();
        let action = inspect(&root, false, 0, None, &mut term).unwrap();
        prepare(&root, None, action, &mut term).unwrap();
        assert_eq!(fs::metadata(&root).unwrap().ino(), inode);
        assert!(term.lines.is_empty());
    }

    #[test]
    fn existing_path_on_no_changes_nothing() {
        let root = ScratchDir::new("local", "refused");
        fs::write(root.join("file"), b"here").unwrap();
        let mut term = Term::default();
        assert!(inspect(&root, false, 0, None, &mut term).is_err());
        assert!(term.lines[0].contains("1 files, 4 bytes"));
        assert_eq!(fs::read(root.join("file")).unwrap(), b"here");
    }

    #[test]
    fn existing_nonempty_path_lists_local_only_work_and_moves_on_yes() {
        let scratch = ScratchDir::new("local", "existing");
        let root = scratch.join("project");
        fs::create_dir(&root).unwrap();
        let git = super::super::shim_dir::real_git_from_process().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new(&git)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "user.name=Example", "-c", "user.email=example@example.invalid"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q"]);
        fs::write(root.join("tracked"), b"original").unwrap();
        run(&["add", "tracked"]);
        run(&["commit", "-qm", "local commit"]);
        fs::write(root.join("tracked"), b"stashed").unwrap();
        run(&["stash", "push", "-m", "local stash"]);
        for n in 0..52 {
            fs::write(root.join(format!("file{n:02}")), b"local").unwrap();
        }
        let mut term = Term {
            yes: true,
            ..Default::default()
        };
        let action = inspect(&root, false, 0, Some(&LocalGit::new(git)), &mut term)
            .unwrap()
            .unwrap();
        let dest = action.dest.clone();
        prepare(&root, None, Some(action), &mut term).unwrap();
        assert!(root.read_dir().unwrap().next().is_none());
        assert_eq!(fs::read(dest.join("file51")).unwrap(), b"local");
        let text = term.lines.join("\n");
        for expected in [
            "?? file00",
            "and 2 more",
            "local commit",
            "local stash",
            ".eitri-local-",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains("?? file50"));
    }

    #[test]
    fn existing_backup_name_is_never_replaced() {
        let scratch = ScratchDir::new("local", "collision");
        let root = scratch.join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("one"), b"first").unwrap();
        let mut term = Term {
            yes: true,
            ..Default::default()
        };
        let first = inspect(&root, false, 0, None, &mut term).unwrap().unwrap();
        let copy = first.dest.clone();
        prepare(&root, None, Some(first), &mut term).unwrap();
        fs::write(root.join("two"), b"second").unwrap();
        let second = inspect(&root, false, 0, None, &mut term).unwrap();
        assert!(prepare(&root, None, second, &mut term).is_err());
        assert_eq!(fs::read(copy.join("one")).unwrap(), b"first");
        assert_eq!(fs::read(root.join("two")).unwrap(), b"second");
    }

    #[test]
    fn move_across_filesystems_refuses() {
        let root = ScratchDir::new("local", "cross-device");
        let source = root.join("project");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("work"), b"local").unwrap();
        let mut term = Term {
            yes: true,
            ..Default::default()
        };
        let mut approved = inspect(&source, false, 0, None, &mut term).unwrap().unwrap();
        let source_device = approved.dev;
        let other = [Path::new("/tmp"), Path::new("/dev/shm")]
            .into_iter()
            .find(|p| fs::metadata(p).is_ok_and(|meta| meta.dev() != source_device));
        let dest = other
            .unwrap_or(root.as_ref())
            .join(format!("eitri-move-test-{}", uuid::Uuid::new_v4().simple()));
        let why = if other.is_some() {
            approved.dest = dest.clone();
            prepare(&source, None, Some(approved), &mut Term::default()).unwrap_err()
        } else {
            check_devices(&source, source_device, source_device.wrapping_add(1)).unwrap_err()
        };
        assert_eq!(why, format!("cannot move {} across filesystems", source.display()));
        assert_eq!(fs::read(source.join("work")).unwrap(), b"local");
        assert!(!dest.exists());
    }
}
