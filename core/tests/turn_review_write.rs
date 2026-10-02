//! Turn review's write layer against real files, real git and a real journal, in temporary
//! directories under the target dir.
//!
//! Two tests re-run this binary as a child: one whose write dies halfway (`abort`), one whose file
//! size limit makes the kernel cut a write short. Each child test is `#[ignore]`d and does nothing
//! unless the parent set its directory variable, so `-- --ignored` over this target is harmless.

use std::ffi::{CString, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eitri_core::turn_review::{
    decide_mode, read_current, replace, review_dir_for, write_mode, Content, Current, Entry, EntryKind, FsHooks,
    InPlaceWhy, Journal, JournalNote, Limits, ProjectDir, Shadow, SnapshotKind, SnapshotLabel, SnapshotOutcome, Stage,
    WriteError, WriteMode,
};

/// The variable naming a child test's directory.
const CHILD_DIR: &str = "EITRI_WRITE_CHILD_DIR";

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("turn_review_write")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A project, its shadow and its journal, all under `dir`.
struct Fixture {
    project: PathBuf,
    review: PathBuf,
    shadow: Shadow,
    journal: Journal,
    root: ProjectDir,
}

impl Fixture {
    fn at(dir: &Path) -> Fixture {
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let review = review_dir_for(Some(dir.join("state").as_os_str()), None, &project).unwrap();
        let shadow = Shadow::open_with_excludes(&review, &project, None).unwrap();
        let journal = Journal::open(&review).unwrap();
        let root = ProjectDir::open(&project).unwrap();
        Fixture {
            project,
            review,
            shadow,
            journal,
            root,
        }
    }

    fn file(&self, rel: &str) -> PathBuf {
        self.project.join(rel)
    }

    /// A note for rewriting `rel`, whose bytes are now `pre` with `pre_mode`, with `new`.
    fn note(&self, rel: &str, pre: &[u8], pre_mode: u32, new: &[u8]) -> JournalNote {
        JournalNote {
            session: "s1".into(),
            path: PathBuf::from(rel),
            pre: Some(self.shadow.store_blob(pre).unwrap()),
            pre_mode,
            intended: Some(self.shadow.store_blob(new).unwrap()),
        }
    }

    fn replace(&self, rel: &str, new: &Content, note: &JournalNote, hooks: &FsHooks) -> Result<(), WriteError> {
        let target = self.root.target(Path::new(rel), true).unwrap();
        replace(&target, new, note, &self.journal, &self.shadow, hooks)
    }

    /// The shadow's refs under `prefix`.
    fn refs(&self, prefix: &str) -> Vec<String> {
        let out = tgit_cmd(&self.review)
            .args(["--git-dir=git", "for-each-ref", "--format=%(refname)", prefix])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// Test-side git in `dir`, isolated from the machine's config and from the process environment.
fn tgit_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .current_dir(dir);
    cmd
}

fn tgit(dir: &Path, args: &[&str]) -> Output {
    let out = tgit_cmd(dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn meta(path: &Path) -> std::fs::Metadata {
    std::fs::symlink_metadata(path).unwrap()
}

fn mode_of(path: &Path) -> u32 {
    meta(path).mode() & 0o7777
}

/// Bytes no compression shrinks much, so a stored blob is about as large as they are.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// The names of `dir`'s entries.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn no_temp_left(dir: &Path) {
    let left: Vec<String> = names(dir).into_iter().filter(|n| n.contains(".eitri-")).collect();
    assert!(left.is_empty(), "temporary files left: {left:?}");
}

// The extended attribute calls with each platform's arguments: Apple's take a position and options
// where Linux's have an `l` variant for not following a link.
#[cfg(not(target_vendor = "apple"))]
mod xattr_sys {
    use std::ffi::CStr;
    pub unsafe fn list_no_follow(path: &CStr) -> isize {
        // SAFETY: a null buffer of size 0 asks only for the size; the path is NUL-terminated.
        unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) }
    }
    pub unsafe fn set(path: &CStr, name: &CStr, value: &[u8]) -> i32 {
        // SAFETY: both strings are NUL-terminated; `value` is valid for its length.
        unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) }
    }
    pub unsafe fn get(path: &CStr, name: &CStr, value: &mut [u8]) -> isize {
        // SAFETY: `value` is writable for its length; the strings are NUL-terminated.
        unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), value.as_mut_ptr().cast(), value.len()) }
    }
}
#[cfg(target_vendor = "apple")]
mod xattr_sys {
    use std::ffi::CStr;
    pub unsafe fn list_no_follow(path: &CStr) -> isize {
        // SAFETY: a null buffer of size 0 asks only for the size; the path is NUL-terminated.
        unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0, libc::XATTR_NOFOLLOW) }
    }
    pub unsafe fn set(path: &CStr, name: &CStr, value: &[u8]) -> i32 {
        // SAFETY: both strings are NUL-terminated; `value` is valid for its length.
        unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) }
    }
    pub unsafe fn get(path: &CStr, name: &CStr, value: &mut [u8]) -> isize {
        // SAFETY: `value` is writable for its length; the strings are NUL-terminated.
        unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        }
    }
}

/// How many extended attribute names `path` has, or why that cannot be asked.
fn xattr_count(path: &Path) -> Result<usize, std::io::Error> {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is NUL-terminated.
    let size = unsafe { xattr_sys::list_no_follow(&c) };
    if size < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(if size == 0 { 0 } else { 1 })
}

fn fault_at(pred: impl Fn(Stage) -> bool + Send + Sync + 'static) -> FsHooks {
    FsHooks {
        fault: Some(Arc::new(move |stage| {
            if pred(stage) {
                Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
            } else {
                Ok(())
            }
        })),
        ..FsHooks::default()
    }
}

/// Runs the child test `name` with its directory set to `dir`, waiting at most 30 s; a child still
/// running then is killed by its own pid.
/// Runs one `#[ignore]`d child test in a process of its own. Its stdout and stderr are pipes, never
/// the parent's: a child that lowers `RLIMIT_FSIZE` would otherwise fail its own writes whenever the
/// harness output is a regular file already larger than that limit. What it printed is passed on
/// through the parent's captured output, which libtest shows when the calling test fails.
fn run_child(name: &str, dir: &Path) -> std::process::ExitStatus {
    use std::io::Read;
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--ignored", "--nocapture", "--test-threads=1"])
        .env(CHILD_DIR, dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Drained on threads of their own, so a chatty child never blocks on a full pipe.
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    println!("--- child {name} stdout ---\n{}", String::from_utf8_lossy(&stdout));
    println!("--- child {name} stderr ---\n{}", String::from_utf8_lossy(&stderr));
    status.unwrap_or_else(|| panic!("child {name} did not finish in 30 s"))
}

#[test]
fn an_ordinary_file_is_replaced_atomically() {
    let scratch = Scratch::new("atomic");
    let fx = Fixture::at(&scratch.0);
    let probe = scratch.path("probe");
    std::fs::write(&probe, b"x").unwrap();
    match xattr_count(&probe) {
        Ok(0) => {}
        Ok(_) => {
            println!("every new file here has extended attributes (an SELinux label?); an atomic write cannot happen");
            return;
        }
        Err(e) => println!("extended attributes cannot be listed here ({e}); going on"),
    }
    let pre = b"one\r\ntwo\n".to_vec();
    write_file(&fx.file("bin/run"), &pre, 0o755);
    let before = meta(&fx.file("bin/run")).ino();
    let target = fx.root.target(Path::new("bin/run"), false).unwrap();
    assert_eq!(write_mode(&target).unwrap(), WriteMode::Atomic);

    let new = b"\xff\xfe not utf-8\r\nCRLF kept\r\n\0".to_vec();
    let note = fx.note("bin/run", &pre, 0o755, &new);
    let content = Content::Bytes {
        bytes: new.clone(),
        mode: 0o755,
    };
    replace(&target, &content, &note, &fx.journal, &fx.shadow, &FsHooks::default()).unwrap();

    assert_eq!(std::fs::read(fx.file("bin/run")).unwrap(), new);
    assert_ne!(
        meta(&fx.file("bin/run")).ino(),
        before,
        "a new inode: it was renamed in"
    );
    assert_eq!(mode_of(&fx.file("bin/run")), 0o755);
    no_temp_left(&fx.file("bin"));
    assert!(fx.journal.pending().is_empty());
    assert_eq!(
        read_current(&target).unwrap(),
        Current::Regular {
            bytes: new,
            mode: 0o755
        }
    );
}

#[test]
fn a_hard_linked_file_keeps_its_inode_and_the_other_link_sees_it() {
    let scratch = Scratch::new("hardlink");
    let fx = Fixture::at(&scratch.0);
    let pre = b"before\n".to_vec();
    write_file(&fx.file("a.txt"), &pre, 0o640);
    let other = scratch.path("elsewhere/a-link");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::hard_link(fx.file("a.txt"), &other).unwrap();
    let inode = meta(&fx.file("a.txt")).ino();
    let target = fx.root.target(Path::new("a.txt"), false).unwrap();
    assert_eq!(write_mode(&target).unwrap(), WriteMode::InPlace(InPlaceWhy::HardLinks));

    let new = b"after\r\n\xc3\x28".to_vec();
    let note = fx.note("a.txt", &pre, 0o640, &new);
    let content = Content::Bytes {
        bytes: new.clone(),
        mode: 0o640,
    };
    replace(&target, &content, &note, &fx.journal, &fx.shadow, &FsHooks::default()).unwrap();

    assert_eq!(meta(&fx.file("a.txt")).ino(), inode);
    assert_eq!(std::fs::read(&other).unwrap(), new, "the other link sees the new bytes");
    assert_eq!(mode_of(&fx.file("a.txt")), 0o640);
    assert!(fx.journal.pending().is_empty());
    assert!(names(fx.journal.dir()).is_empty(), "{:?}", names(fx.journal.dir()));
    assert!(fx.refs("refs/eitri-journal/").is_empty());
    no_temp_left(&fx.project);
}

#[test]
fn a_file_with_an_xattr_is_written_in_place() {
    let scratch = Scratch::new("xattr");
    let fx = Fixture::at(&scratch.0);
    let pre = b"attrs\n".to_vec();
    write_file(&fx.file("x.txt"), &pre, 0o644);
    let path = CString::new(fx.file("x.txt").as_os_str().as_bytes()).unwrap();
    let name = CString::new("user.eitri_test").unwrap();
    // SAFETY: both strings are NUL-terminated; the value is 3 valid bytes.
    let set = unsafe { xattr_sys::set(&path, &name, b"yes") };
    if set != 0 {
        println!(
            "this file system refuses user extended attributes ({}); nothing to test",
            std::io::Error::last_os_error()
        );
        return;
    }
    let inode = meta(&fx.file("x.txt")).ino();
    let target = fx.root.target(Path::new("x.txt"), false).unwrap();
    assert_eq!(write_mode(&target).unwrap(), WriteMode::InPlace(InPlaceWhy::Xattrs));
    let new = b"attrs kept\n".to_vec();
    let note = fx.note("x.txt", &pre, 0o644, &new);
    let content = Content::Bytes {
        bytes: new.clone(),
        mode: 0o644,
    };
    replace(&target, &content, &note, &fx.journal, &fx.shadow, &FsHooks::default()).unwrap();
    assert_eq!(std::fs::read(fx.file("x.txt")).unwrap(), new);
    assert_eq!(meta(&fx.file("x.txt")).ino(), inode);
    let mut value = [0u8; 8];
    // SAFETY: `value` is writable for its length; the strings are NUL-terminated.
    let got = unsafe { xattr_sys::get(&path, &name, &mut value) };
    assert_eq!(got, 3, "the attribute is still there");
    assert_eq!(&value[..3], b"yes");
}

#[test]
fn decide_mode_covers_owner_links_and_xattrs() {
    assert_eq!(decide_mode(1, 1000, 1000, 0), WriteMode::Atomic);
    assert_eq!(decide_mode(2, 1000, 1000, 0), WriteMode::InPlace(InPlaceWhy::HardLinks));
    assert_eq!(decide_mode(1, 0, 1000, 0), WriteMode::InPlace(InPlaceWhy::OtherOwner));
    assert_eq!(decide_mode(1, 1000, 1000, 1), WriteMode::InPlace(InPlaceWhy::Xattrs));
    // The first reason found wins.
    assert_eq!(decide_mode(2, 0, 1000, 1), WriteMode::InPlace(InPlaceWhy::HardLinks));
    assert_eq!(decide_mode(1, 0, 1000, 3), WriteMode::InPlace(InPlaceWhy::OtherOwner));
}

#[test]
fn a_symlink_is_replaced_as_a_link() {
    let scratch = Scratch::new("symlink");
    let fx = Fixture::at(&scratch.0);
    write_file(&fx.file("real.txt"), b"pointed at\n", 0o644);
    write_file(&fx.file("other.txt"), b"other\n", 0o644);
    std::os::unix::fs::symlink("other.txt", fx.file("link")).unwrap();
    let real_inode = meta(&fx.file("real.txt")).ino();
    let target = fx.root.target(Path::new("link"), false).unwrap();
    assert_eq!(write_mode(&target).unwrap(), WriteMode::Atomic);
    assert_eq!(
        read_current(&target).unwrap(),
        Current::Symlink {
            target: b"other.txt".to_vec()
        }
    );
    let note = JournalNote {
        session: "s1".into(),
        path: "link".into(),
        pre: None,
        pre_mode: 0o777,
        intended: None,
    };
    let content = Content::Symlink {
        target: b"real.txt".to_vec(),
    };
    replace(&target, &content, &note, &fx.journal, &fx.shadow, &FsHooks::default()).unwrap();
    assert_eq!(std::fs::read_link(fx.file("link")).unwrap(), Path::new("real.txt"));
    assert_eq!(std::fs::read(fx.file("real.txt")).unwrap(), b"pointed at\n");
    assert_eq!(std::fs::read(fx.file("other.txt")).unwrap(), b"other\n");
    assert_eq!(meta(&fx.file("real.txt")).ino(), real_inode);
    no_temp_left(&fx.project);

    // A file put where a link is replaces the link, never the file it points at.
    let content = Content::Bytes {
        bytes: b"now a file\n".to_vec(),
        mode: 0o600,
    };
    replace(&target, &content, &note, &fx.journal, &fx.shadow, &FsHooks::default()).unwrap();
    assert!(meta(&fx.file("link")).is_file());
    assert_eq!(mode_of(&fx.file("link")), 0o600);
    assert_eq!(std::fs::read(fx.file("real.txt")).unwrap(), b"pointed at\n");

    // And a link restored onto an absent path is a link, with a target that need not exist.
    let gone = fx.root.target(Path::new("new-link"), false).unwrap();
    let content = Content::Symlink {
        target: b"does/not/exist".to_vec(),
    };
    replace(&gone, &content, &note, &fx.journal, &fx.shadow, &FsHooks::default()).unwrap();
    assert_eq!(
        std::fs::read_link(fx.file("new-link")).unwrap(),
        Path::new("does/not/exist")
    );
    no_temp_left(&fx.project);
}

#[test]
fn restore_never_replaces_a_path_that_appeared() {
    let scratch = Scratch::new("appeared");
    let fx = Fixture::at(&scratch.0);
    let appearing = fx.file("restored.txt");
    let hooks = FsHooks {
        fault: Some(Arc::new(move |stage| {
            if stage == Stage::BeforeLink {
                std::fs::write(&appearing, b"someone else's\n").unwrap();
            }
            Ok(())
        })),
        ..FsHooks::default()
    };
    let note = fx.note("restored.txt", b"", 0o644, b"restored\n");
    let content = Content::Bytes {
        bytes: b"restored\n".to_vec(),
        mode: 0o644,
    };
    assert_eq!(
        fx.replace("restored.txt", &content, &note, &hooks),
        Err(WriteError::Appeared)
    );
    assert_eq!(std::fs::read(fx.file("restored.txt")).unwrap(), b"someone else's\n");
    no_temp_left(&fx.project);
}

#[test]
fn too_little_free_space_refuses() {
    let scratch = Scratch::new("nospace");
    let fx = Fixture::at(&scratch.0);
    write_file(&fx.file("a.txt"), b"old\n", 0o644);
    let before = meta(&fx.file("a.txt"));
    let listing = names(&fx.project);
    let hooks = FsHooks {
        free_bytes: |_| Ok(1),
        fault: None,
    };
    let note = fx.note("a.txt", b"old\n", 0o644, b"new bytes\n");
    let content = Content::Bytes {
        bytes: b"new bytes\n".to_vec(),
        mode: 0o644,
    };
    assert_eq!(
        fx.replace("a.txt", &content, &note, &hooks),
        Err(WriteError::NoSpace { need: 20, free: 1 })
    );
    assert_eq!(std::fs::read(fx.file("a.txt")).unwrap(), b"old\n");
    let after = meta(&fx.file("a.txt"));
    assert_eq!(
        (after.ino(), after.mtime(), after.mtime_nsec()),
        (before.ino(), before.mtime(), before.mtime_nsec())
    );
    assert_eq!(names(&fx.project), listing);
    assert!(names(fx.journal.dir()).is_empty());
}

#[test]
fn a_failed_in_place_write_puts_the_old_bytes_back() {
    let scratch = Scratch::new("rollback");
    let fx = Fixture::at(&scratch.0);
    let pre = b"the earlier bytes\r\n".to_vec();
    write_file(&fx.file("a.txt"), &pre, 0o644);
    std::fs::hard_link(fx.file("a.txt"), fx.file("b.txt")).unwrap();
    let inode = meta(&fx.file("a.txt")).ino();
    let new = b"never all written\n".to_vec();
    let note = fx.note("a.txt", &pre, 0o644, &new);
    let content = Content::Bytes {
        bytes: new,
        mode: 0o600,
    };
    let hooks = fault_at(|s| matches!(s, Stage::Wrote(n) if n >= 3));
    match fx.replace("a.txt", &content, &note, &hooks) {
        Err(WriteError::RolledBack(why)) => assert!(why.contains("a.txt"), "{why}"),
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(std::fs::read(fx.file("a.txt")).unwrap(), pre);
    assert_eq!(meta(&fx.file("a.txt")).ino(), inode);
    assert_eq!(mode_of(&fx.file("a.txt")), 0o644, "the mode is put back too");
    assert!(fx.journal.pending().is_empty());
    assert!(names(fx.journal.dir()).is_empty());
    assert!(fx.refs("refs/eitri-journal/").is_empty());
}

#[test]
fn an_unwritable_in_place_file_journals_nothing() {
    let scratch = Scratch::new("unwritable");
    let fx = Fixture::at(&scratch.0);
    let pre = b"read only\n".to_vec();
    write_file(&fx.file("ro.txt"), &pre, 0o444);
    std::fs::hard_link(fx.file("ro.txt"), fx.file("ro2.txt")).unwrap();
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        println!("root writes a 0444 file anyway; nothing to test");
        return;
    }
    let note = fx.note("ro.txt", &pre, 0o444, b"x");
    let content = Content::Bytes {
        bytes: b"x".to_vec(),
        mode: 0o444,
    };
    match fx.replace("ro.txt", &content, &note, &FsHooks::default()) {
        Err(WriteError::Io(_)) => {}
        other => panic!("expected a plain failure, got {other:?}"),
    }
    assert_eq!(std::fs::read(fx.file("ro.txt")).unwrap(), pre);
    assert!(fx.journal.pending().is_empty());
    assert!(names(fx.journal.dir()).is_empty());
    assert!(fx.refs("refs/eitri-journal/").is_empty());
}

#[test]
fn a_failed_atomic_write_leaves_the_file_and_no_temp() {
    let scratch = Scratch::new("atomicfail");
    let fx = Fixture::at(&scratch.0);
    write_file(&fx.file("a.txt"), b"kept\n", 0o644);
    let inode = meta(&fx.file("a.txt")).ino();
    let note = fx.note("a.txt", b"kept\n", 0o644, b"lost\n");
    let content = Content::Bytes {
        bytes: b"lost\n".to_vec(),
        mode: 0o644,
    };
    let hooks = fault_at(|s| s == Stage::Synced);
    match fx.replace("a.txt", &content, &note, &hooks) {
        Err(WriteError::Io(_)) => {}
        other => panic!("expected a plain failure, got {other:?}"),
    }
    assert_eq!(std::fs::read(fx.file("a.txt")).unwrap(), b"kept\n");
    assert_eq!(meta(&fx.file("a.txt")).ino(), inode);
    no_temp_left(&fx.project);
}

#[test]
fn a_long_file_name_still_gets_a_temp() {
    let scratch = Scratch::new("longname");
    let fx = Fixture::at(&scratch.0);
    let long = "n".repeat(250);
    write_file(&fx.file(&long), b"old\n", 0o644);
    let note = fx.note(&long, b"old\n", 0o644, b"new\n");
    let content = Content::Bytes {
        bytes: b"new\n".to_vec(),
        mode: 0o644,
    };
    fx.replace(&long, &content, &note, &FsHooks::default()).unwrap();
    assert_eq!(std::fs::read(fx.file(&long)).unwrap(), b"new\n");
    std::fs::remove_file(fx.file(&long)).unwrap();
    fx.replace(&long, &content, &note, &FsHooks::default()).unwrap();
    assert_eq!(std::fs::read(fx.file(&long)).unwrap(), b"new\n");
    no_temp_left(&fx.project);
}

#[test]
fn a_deletion_unlinks_the_file() {
    let scratch = Scratch::new("delete");
    let fx = Fixture::at(&scratch.0);
    write_file(&fx.file("d/gone.txt"), b"bye\n", 0o644);
    let note = fx.note("d/gone.txt", b"bye\n", 0o644, b"");
    fx.replace("d/gone.txt", &Content::Absent, &note, &FsHooks::default())
        .unwrap();
    assert!(!fx.file("d/gone.txt").exists());
    assert_eq!(
        fx.replace("d/gone.txt", &Content::Absent, &note, &FsHooks::default()),
        Err(WriteError::Io("d/gone.txt disappeared since it was checked".into()))
    );
    // A directory is never removed or replaced.
    std::fs::create_dir_all(fx.file("d/sub")).unwrap();
    assert!(matches!(
        fx.replace("d/sub", &Content::Absent, &note, &FsHooks::default()),
        Err(WriteError::Io(_))
    ));
    let target = fx.root.target(Path::new("d/sub"), false).unwrap();
    assert_eq!(read_current(&target).unwrap(), Current::Other);
    assert!(write_mode(&target).is_err());
    assert!(fx.file("d/sub").is_dir());
}

/// The child of `a_real_short_write_rolls_back`: a hard-linked 4 KiB file rewritten in place with
/// 64 KiB under a 16 KiB file size limit, so the kernel refuses the write partway.
#[test]
#[ignore = "run by a_real_short_write_rolls_back"]
fn short_write_child() {
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    let fx = Fixture::at(Path::new(&dir));
    let pre = noise(4096, 1);
    let new = noise(64 * 1024, 2);
    // Stored before the limit: git inherits it, and an incompressible 64 KiB blob would meet it.
    let note = fx.note("a.bin", &pre, 0o644, &new);
    // SAFETY: ignoring SIGXFSZ makes a write over the limit fail with EFBIG instead of killing us.
    unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
    let limit = libc::rlimit {
        rlim_cur: 16 * 1024,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: `limit` is a valid `rlimit` for the call.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) }, 0);
    let content = Content::Bytes {
        bytes: new,
        mode: 0o644,
    };
    match fx.replace("a.bin", &content, &note, &FsHooks::default()) {
        Err(WriteError::RolledBack(why)) => println!("rolled back: {why}"),
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(std::fs::read(fx.file("a.bin")).unwrap(), pre);
}

#[test]
fn a_real_short_write_rolls_back() {
    let scratch = Scratch::new("shortwrite");
    let fx = Fixture::at(&scratch.0);
    let pre = noise(4096, 1);
    write_file(&fx.file("a.bin"), &pre, 0o644);
    std::fs::hard_link(fx.file("a.bin"), scratch.path("other-link")).unwrap();
    let inode = meta(&fx.file("a.bin")).ino();
    let status = run_child("short_write_child", &scratch.0);
    assert!(status.success(), "the child failed: {status:?}");
    assert_eq!(std::fs::read(fx.file("a.bin")).unwrap(), pre);
    assert_eq!(std::fs::read(scratch.path("other-link")).unwrap(), pre);
    assert_eq!(meta(&fx.file("a.bin")).ino(), inode);
    assert!(fx.journal.pending().is_empty());
    assert!(fx.refs("refs/eitri-journal/").is_empty());
}

/// The size of the crash child's new content: over one write chunk, so the abort leaves a file
/// that is genuinely half written.
const CRASH_NEW_LEN: usize = 200 * 1024;

/// The child of the crash tests: an in-place rewrite of a hard-linked file that dies after its
/// first chunk.
#[test]
#[ignore = "run by a_crash_mid_write_leaves_a_journal_entry"]
fn write_crash_child() {
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    let fx = Fixture::at(Path::new(&dir));
    let pre = std::fs::read(fx.file("src/a.txt")).unwrap();
    let new = noise(CRASH_NEW_LEN, 3);
    let note = fx.note("src/a.txt", &pre, 0o644, &new);
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `no_core` is a valid `rlimit`; no dump is wanted from the abort below.
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) };
    let hooks = FsHooks {
        fault: Some(Arc::new(|stage| {
            if matches!(stage, Stage::Wrote(n) if n >= 64 * 1024) {
                std::process::abort();
            }
            Ok(())
        })),
        ..FsHooks::default()
    };
    let content = Content::Bytes {
        bytes: new,
        mode: 0o644,
    };
    let _ = fx.replace("src/a.txt", &content, &note, &hooks);
    unreachable!("the write was to abort");
}

/// A project whose in-place write of `src/a.txt` was killed halfway, and its original bytes.
fn crashed(scratch: &Scratch) -> (Fixture, Vec<u8>) {
    let fx = Fixture::at(&scratch.0);
    let original = b"the original\r\nbytes \xff\n".to_vec();
    write_file(&fx.file("src/a.txt"), &original, 0o644);
    std::fs::hard_link(fx.file("src/a.txt"), scratch.path("second-link")).unwrap();
    let status = run_child("write_crash_child", &scratch.0);
    assert!(!status.success(), "the child was to die");
    assert_ne!(
        std::fs::read(fx.file("src/a.txt")).unwrap(),
        original,
        "the file is half written"
    );
    (fx, original)
}

#[test]
fn a_crash_mid_write_leaves_a_journal_entry() {
    let scratch = Scratch::new("crash");
    let (fx, original) = crashed(&scratch);
    let pending = fx.journal.pending();
    assert_eq!(pending.len(), 1, "{pending:?}");
    let entry = &pending[0];
    assert_eq!(entry.path, Path::new("src/a.txt"));
    assert_eq!(entry.mode, 0o644);
    assert_ne!(entry.pid, std::process::id());
    let pre = entry.pre.as_deref().unwrap();
    assert_eq!(fx.shadow.read_stored(pre).unwrap().unwrap(), original);
    let intended = fx
        .shadow
        .read_stored(entry.intended.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(intended, noise(CRASH_NEW_LEN, 3));
    assert_eq!(
        fx.refs("refs/eitri-journal/"),
        vec![
            format!("refs/eitri-journal/{}/intended", entry.id),
            format!("refs/eitri-journal/{}/pre", entry.id),
        ]
    );
    // The entry and its directory are private.
    let file = fx.journal.dir().join(format!("{}.json", entry.id));
    assert_eq!(mode_of(&file), 0o600);
    assert_eq!(mode_of(fx.journal.dir()), 0o700);
    // Removing it takes its pins with it.
    fx.journal.remove(&entry.id, &fx.shadow).unwrap();
    assert!(fx.journal.pending().is_empty());
    assert!(fx.refs("refs/eitri-journal/").is_empty());
}

#[test]
fn a_pending_journal_older_than_max_age_survives_trim_and_gc() {
    let scratch = Scratch::new("crashtrim");
    let (fx, original) = crashed(&scratch);
    let entry = fx.journal.pending().remove(0);
    let anchored = fx.shadow.store_blob(b"anchored as old as the entry\n").unwrap();
    fx.shadow
        .anchor_revert("s1", &format!("{}-0", entry.at_ms), Some(&anchored), None)
        .unwrap();
    let month = 31 * 24 * 3600 * 1000;
    fx.shadow
        .trim(0, Duration::from_millis(1), entry.at_ms + month, "now")
        .unwrap();
    assert_eq!(fx.refs("refs/eitri-journal/").len(), 2, "the pins remain");
    assert_eq!(
        fx.shadow.read_stored(entry.pre.as_deref().unwrap()).unwrap().unwrap(),
        original
    );
    assert_eq!(
        fx.shadow
            .read_stored(entry.intended.as_deref().unwrap())
            .unwrap()
            .unwrap(),
        noise(CRASH_NEW_LEN, 3)
    );
    assert!(
        fx.refs("refs/eitri-revert/").is_empty(),
        "the anchor of the same age is gone"
    );
    assert_eq!(fx.shadow.read_stored(&anchored).unwrap(), None);
    assert_eq!(fx.journal.pending().len(), 1);
}

#[test]
fn an_orphaned_journal_pin_is_trimmed() {
    let scratch = Scratch::new("orphan");
    let (fx, _) = crashed(&scratch);
    let entry = fx.journal.pending().remove(0);
    // The entry goes as a crash between its removal and its unpinning would leave it.
    std::fs::remove_file(fx.journal.dir().join(format!("{}.json", entry.id))).unwrap();
    assert_eq!(fx.refs("refs/eitri-journal/").len(), 2);
    fx.shadow
        .trim(100, Duration::from_secs(3600), entry.at_ms, "now")
        .unwrap();
    assert!(fx.refs("refs/eitri-journal/").is_empty());
    assert_eq!(fx.shadow.read_stored(entry.intended.as_deref().unwrap()).unwrap(), None);
}

#[test]
fn the_journal_is_private() {
    let scratch = Scratch::new("private");
    let fx = Fixture::at(&scratch.0);
    assert_eq!(mode_of(fx.journal.dir()), 0o700);
    assert!(meta(fx.journal.dir()).is_dir());

    // A `journal` that is a link, even to a directory of this user's, is refused.
    let review = scratch.path("state2-review");
    std::fs::create_dir_all(&review).unwrap();
    let elsewhere = scratch.path("own-dir");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, review.join("journal")).unwrap();
    assert!(Journal::open(&review).is_err());
    assert!(names(&elsewhere).is_empty(), "nothing is made in the link's target");
    assert_eq!(mode_of(&elsewhere), 0o755, "and it is not tightened either");
}

#[test]
fn a_parent_symlinked_out_of_the_project_is_refused() {
    let scratch = Scratch::new("outside");
    let fx = Fixture::at(&scratch.0);
    let outside = scratch.path("outside");
    write_file(&outside.join("x.rs"), b"fn main() {}\n", 0o644);
    let before = meta(&outside.join("x.rs"));
    std::os::unix::fs::symlink(&outside, fx.file("src")).unwrap();
    for create in [false, true] {
        match fx.root.target(Path::new("src/x.rs"), create) {
            Err(WriteError::Outside(why)) => assert_eq!(
                why,
                "src/x.rs is not inside the project: src is a link or not a directory"
            ),
            other => panic!("expected Outside, got {other:?}"),
        }
    }
    // A file where a directory should be is refused the same way.
    write_file(&fx.file("plain"), b"", 0o644);
    assert!(matches!(
        fx.root.target(Path::new("plain/x"), true),
        Err(WriteError::Outside(_))
    ));
    let after = meta(&outside.join("x.rs"));
    assert_eq!(std::fs::read(outside.join("x.rs")).unwrap(), b"fn main() {}\n");
    assert_eq!(
        (after.ino(), after.mtime(), after.mtime_nsec()),
        (before.ino(), before.mtime(), before.mtime_nsec())
    );
}

#[test]
fn a_restore_never_creates_through_a_symlinked_parent() {
    let scratch = Scratch::new("dangling");
    let fx = Fixture::at(&scratch.0);
    let nowhere = scratch.path("nowhere");
    std::os::unix::fs::symlink(&nowhere, fx.file("src")).unwrap();
    assert!(matches!(
        fx.root.target(Path::new("src/x.rs"), true),
        Err(WriteError::Outside(_))
    ));
    assert!(matches!(
        fx.root.target(Path::new("src/deeper/x.rs"), true),
        Err(WriteError::Outside(_))
    ));
    assert!(
        std::fs::symlink_metadata(&nowhere).is_err(),
        "nothing appeared at the link's target"
    );
}

#[test]
fn a_missing_parent_is_created_for_a_restore() {
    let scratch = Scratch::new("mkdir");
    let fx = Fixture::at(&scratch.0);
    // Without creating, the target reads as absent and makes nothing.
    let looked = fx.root.target(Path::new("a/b/c"), false).unwrap();
    assert_eq!(read_current(&looked).unwrap(), Current::Absent);
    let note = fx.note("a/b/c", b"", 0o644, b"restored\n");
    let content = Content::Bytes {
        bytes: b"restored\n".to_vec(),
        mode: 0o644,
    };
    assert!(matches!(
        replace(&looked, &content, &note, &fx.journal, &fx.shadow, &FsHooks::default()),
        Err(WriteError::Io(_))
    ));
    assert!(!fx.file("a").exists());

    let target = fx.root.target(Path::new("a/b/c"), true).unwrap();
    assert!(fx.file("a/b").is_dir());
    replace(&target, &content, &note, &fx.journal, &fx.shadow, &FsHooks::default()).unwrap();
    assert_eq!(std::fs::read(fx.file("a/b/c")).unwrap(), b"restored\n");
    assert_eq!(mode_of(&fx.file("a/b/c")), 0o644);
    // SAFETY: `umask` only swaps the mask; it is put straight back.
    let umask = unsafe {
        let old = libc::umask(0o022);
        libc::umask(old);
        old
    };
    assert_eq!(mode_of(&fx.file("a/b")), 0o777 & !(umask as u32));
}

#[test]
fn dotdot_absolute_and_empty_components_are_refused() {
    let scratch = Scratch::new("refused");
    let fx = Fixture::at(&scratch.0);
    std::fs::create_dir_all(fx.file("a/b")).unwrap();
    let nul = PathBuf::from(OsString::from_vec(b"a/x\0y".to_vec()));
    for bad in [
        PathBuf::from(""),
        PathBuf::from("/etc/passwd"),
        PathBuf::from("../x"),
        PathBuf::from("a/../../x"),
        PathBuf::from("./a/b"),
        PathBuf::from("a/./b"),
        PathBuf::from("a//b"),
        PathBuf::from("a/b/"),
        PathBuf::from(".."),
        nul,
    ] {
        match fx.root.target(&bad, true) {
            Err(WriteError::Outside(why)) => assert!(why.contains("is not inside the project"), "{why}"),
            other => panic!("{bad:?}: expected Outside, got {other:?}"),
        }
    }
    assert_eq!(names(&fx.project), vec!["a"]);
}

#[test]
fn a_path_through_dot_git_is_refused() {
    let scratch = Scratch::new("dotgit");
    let fx = Fixture::at(&scratch.0);
    tgit(&fx.project, &["init", "-q"]);
    for bad in [".git/config", ".git", "sub/.GIT/x", ".Git/HEAD"] {
        match fx.root.target(Path::new(bad), true) {
            Err(WriteError::Outside(why)) => assert!(why.ends_with(".git is a git directory"), "{why}"),
            other => panic!("{bad}: expected Outside, got {other:?}"),
        }
    }
    assert!(!fx.file("sub").exists());
}

#[test]
fn store_blob_round_trips_and_an_anchor_survives_gc() {
    let scratch = Scratch::new("blobs");
    let fx = Fixture::at(&scratch.0);
    let bytes = b"crlf\r\n\xff\xfe\0end".to_vec();
    let id = fx.shadow.store_blob(&bytes).unwrap();
    assert_eq!(id.len(), 40);
    assert_eq!(fx.shadow.read_stored(&id).unwrap().unwrap(), bytes);
    assert_eq!(fx.shadow.read_stored(&"0".repeat(40)).unwrap(), None);
    assert!(fx.shadow.read_stored(&id[..12]).is_err(), "an abbreviation is refused");
    assert_eq!(
        fx.shadow
            .store_blob(b"")
            .map(|e| fx.shadow.read_stored(&e).unwrap())
            .unwrap(),
        Some(Vec::new())
    );

    let fresh = fx.shadow.store_blob(b"fresh\n").unwrap();
    let old = fx.shadow.store_blob(b"old\n").unwrap();
    let now = 10_000_000_000u64;
    fx.shadow
        .anchor_revert("s1", &format!("{now}-1"), Some(&fresh), Some(&id))
        .unwrap();
    fx.shadow
        .anchor_revert("s1", &format!("{}-0", now - 7_200_000), Some(&old), None)
        .unwrap();
    assert!(fx.shadow.anchor_revert("s1", "not-a-stamp", Some(&old), None).is_err());
    assert!(fx
        .shadow
        .anchor_revert("s/1", &format!("{now}-2"), Some(&old), None)
        .is_err());
    fx.shadow.anchor_revert("s1", &format!("{now}-3"), None, None).unwrap();

    fx.shadow.trim(100, Duration::from_secs(3600), now, "now").unwrap();
    assert_eq!(
        fx.refs("refs/eitri-revert/"),
        vec![
            format!("refs/eitri-revert/s1/{now}-1/post"),
            format!("refs/eitri-revert/s1/{now}-1/pre"),
        ]
    );
    assert_eq!(fx.shadow.read_stored(&fresh).unwrap().unwrap(), b"fresh\n");
    assert_eq!(fx.shadow.read_stored(&id).unwrap().unwrap(), bytes);
    assert_eq!(
        fx.shadow.read_stored(&old).unwrap(),
        None,
        "the old anchor's blob is collected"
    );
}

#[test]
fn read_entry_reports_mode_and_kind() {
    let scratch = Scratch::new("entries");
    let fx = Fixture::at(&scratch.0);
    write_file(&fx.file("plain.txt"), b"plain\n", 0o644);
    write_file(&fx.file("bin/run.sh"), b"#!/bin/sh\n", 0o755);
    write_file(&fx.file("private.txt"), b"mine\n", 0o600);
    std::os::unix::fs::symlink("plain.txt", fx.file("link")).unwrap();
    let label = SnapshotLabel {
        turn: 1,
        kind: SnapshotKind::End,
        turn_id: "t1".into(),
        tab: 1,
        time_ms: 1,
    };
    let SnapshotOutcome::Taken(snap) = fx.shadow.snapshot("s1", &label, &Limits::default()) else {
        panic!("no snapshot");
    };
    let entry = |p: &str| fx.shadow.read_entry(&snap.commit, Path::new(p)).unwrap();
    assert_eq!(
        entry("plain.txt"),
        Some(Entry {
            mode: 0o644,
            kind: EntryKind::Blob,
            bytes: b"plain\n".to_vec()
        })
    );
    assert_eq!(
        entry("bin/run.sh"),
        Some(Entry {
            mode: 0o755,
            kind: EntryKind::Blob,
            bytes: b"#!/bin/sh\n".to_vec()
        })
    );
    assert_eq!(
        entry("private.txt").unwrap().mode,
        0o644,
        "git records only 644 and 755"
    );
    assert_eq!(
        entry("link"),
        Some(Entry {
            mode: 0o777,
            kind: EntryKind::Symlink,
            bytes: b"plain.txt".to_vec()
        })
    );
    assert_eq!(entry("absent.txt"), None);
    assert_eq!(entry("bin"), None, "a directory is no entry");
    assert_eq!(entry("*.txt"), None, "a path is never a pattern");
    assert!(fx.shadow.read_entry(&"a".repeat(40), Path::new("plain.txt")).is_err());
    assert!(fx.shadow.read_entry(&snap.commit, Path::new("/plain.txt")).is_err());
}

/// What the project's own git says about itself, none of which a write may change.
fn git_state(project: &Path) -> Vec<Vec<u8>> {
    let index = project.join(".git/index");
    let index_meta = meta(&index);
    vec![
        tgit(project, &["for-each-ref"]).stdout,
        std::fs::read(&index).unwrap(),
        format!("{} {}", index_meta.mtime(), index_meta.mtime_nsec()).into_bytes(),
        std::fs::read(project.join(".git/HEAD")).unwrap(),
        tgit(project, &["rev-parse", "HEAD"]).stdout,
        tgit(project, &["stash", "list"]).stdout,
        tgit(project, &["count-objects", "-v"]).stdout,
    ]
}

#[test]
fn the_projects_git_is_never_written_by_a_write() {
    let scratch = Scratch::new("projgit");
    let fx = Fixture::at(&scratch.0);
    let p = &fx.project;
    tgit(p, &["init", "-q"]);
    write_file(&fx.file("tracked.txt"), b"v1\n", 0o644);
    write_file(&fx.file("linked.txt"), b"linked\n", 0o644);
    write_file(&fx.file("gone.txt"), b"gone\n", 0o644);
    tgit(p, &["add", "-A"]);
    tgit(p, &["commit", "-q", "-m", "one"]);
    write_file(&fx.file("tracked.txt"), b"v2\n", 0o644);
    tgit(p, &["stash", "-q"]);
    std::fs::hard_link(fx.file("linked.txt"), scratch.path("outside-link")).unwrap();
    let before = git_state(p);

    let note = |rel: &str, pre: &[u8], new: &[u8]| fx.note(rel, pre, 0o644, new);
    let bytes = |b: &[u8]| Content::Bytes {
        bytes: b.to_vec(),
        mode: 0o644,
    };
    let hooks = FsHooks::default();
    fx.replace(
        "tracked.txt",
        &bytes(b"v3\n"),
        &note("tracked.txt", b"v1\n", b"v3\n"),
        &hooks,
    )
    .unwrap();
    fx.replace(
        "linked.txt",
        &bytes(b"in place\n"),
        &note("linked.txt", b"linked\n", b"in place\n"),
        &hooks,
    )
    .unwrap();
    fx.replace("gone.txt", &Content::Absent, &note("gone.txt", b"gone\n", b""), &hooks)
        .unwrap();
    fx.replace(
        "new/made.txt",
        &bytes(b"made\n"),
        &note("new/made.txt", b"", b"made\n"),
        &hooks,
    )
    .unwrap();
    let link = Content::Symlink {
        target: b"tracked.txt".to_vec(),
    };
    fx.replace("link", &link, &note("link", b"", b""), &hooks).unwrap();

    assert_eq!(git_state(p), before);
    assert_eq!(std::fs::read(scratch.path("outside-link")).unwrap(), b"in place\n");
}

#[test]
fn a_write_in_progress_is_not_pending_and_a_bad_entry_is_kept() {
    let scratch = Scratch::new("inprogress");
    let fx = Fixture::at(&scratch.0);
    let pre = b"pre\n".to_vec();
    write_file(&fx.file("a.txt"), &pre, 0o644);
    std::fs::hard_link(fx.file("a.txt"), scratch.path("other")).unwrap();
    let journal = fx.journal.clone();
    let seen = Arc::new(std::sync::Mutex::new(None));
    let seen_in_hook = seen.clone();
    let hooks = FsHooks {
        fault: Some(Arc::new(move |stage| {
            if stage == Stage::JournalWritten {
                // The entry exists, but its writer holds it: it is no crash to recover from.
                *seen_in_hook.lock().unwrap() = Some((names(journal.dir()).len(), journal.pending().len()));
            }
            Ok(())
        })),
        ..FsHooks::default()
    };
    let note = fx.note("a.txt", &pre, 0o644, b"post\n");
    let content = Content::Bytes {
        bytes: b"post\n".to_vec(),
        mode: 0o644,
    };
    fx.replace("a.txt", &content, &note, &hooks).unwrap();
    assert_eq!(*seen.lock().unwrap(), Some((1, 0)));
    assert_eq!(std::fs::read(fx.file("a.txt")).unwrap(), b"post\n");

    // An entry this build cannot read is left alone, never deleted.
    let bad = fx.journal.dir().join("0123456789abcdef0123456789abcdef.json");
    std::fs::write(&bad, b"{ not an entry").unwrap();
    assert!(fx.journal.pending().is_empty());
    assert!(bad.exists());
}
