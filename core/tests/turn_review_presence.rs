//! Presence locks against real files, and a real second process for the death of a window.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use eitri_core::turn_review::{busy_elsewhere, Blocked, Busy, Presence, PresenceHolder, Shadow};

/// A directory of the test's own under the target dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("turn_review_presence")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    /// A review directory shaped like the real one, `<state>/eitri/review/<key>`.
    fn review_dir(&self) -> PathBuf {
        let dir = self.0.join("state/eitri/review/0123456789abcdef");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn another_windows_file_is_busy() {
    let scratch = Scratch::new("other-window");
    let dir = scratch.review_dir();
    let _other = Presence::open(&dir, 4242).unwrap();
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::OtherWindow { pid: 4242 })
    );
    assert!(Busy::OtherWindow { pid: 4242 }
        .reason()
        .contains("another Eitri window"));
}

#[test]
fn a_running_turn_is_busy_and_wins() {
    let scratch = Scratch::new("running");
    let dir = scratch.review_dir();
    let mut other = Presence::open(&dir, 4242).unwrap();
    other.turn_started(3).unwrap();
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::TurnRunning { pid: 4242 })
    );
    other.turn_ended(3);
    assert!(!dir.join("running/4242.3").exists(), "the ended turn's file is removed");
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::OtherWindow { pid: 4242 })
    );
}

#[test]
fn own_files_are_skipped_by_name() {
    let scratch = Scratch::new("own");
    let dir = scratch.review_dir();
    let mut me = Presence::open(&dir, 4242).unwrap();
    me.turn_started(1).unwrap();
    assert_eq!(busy_elsewhere(&dir, 4242).unwrap(), None);
    assert!(
        dir.join("windows/4242").exists(),
        "probing one's own files removes nothing"
    );
    assert!(dir.join("running/4242.1").exists());
}

#[test]
fn a_directory_nobody_made_is_nobody() {
    let scratch = Scratch::new("none");
    let dir = scratch.review_dir();
    assert_eq!(busy_elsewhere(&dir, 100).unwrap(), None);
}

#[test]
fn files_are_private_and_a_leftover_is_swept_not_reported() {
    let scratch = Scratch::new("modes");
    let dir = scratch.review_dir();
    let me = Presence::open(&dir, 4242).unwrap();
    for sub in ["running", "windows"] {
        let mode = std::fs::metadata(dir.join(sub)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{sub}");
    }
    let mode = std::fs::metadata(dir.join("windows/4242"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    drop(me);
    assert!(!dir.join("windows/4242").exists(), "a clean exit removes its own file");

    // A file nobody holds, with a name that is no pid, and a symlink, are not windows.
    std::fs::write(dir.join("windows/7"), b"").unwrap();
    std::fs::write(dir.join("windows/notes.txt"), b"").unwrap();
    let foreign = scratch.0.join("foreign");
    std::fs::write(&foreign, b"keep").unwrap();
    std::os::unix::fs::symlink(&foreign, dir.join("windows/8")).unwrap();
    assert_eq!(busy_elsewhere(&dir, 100).unwrap(), None);
    assert!(!dir.join("windows/7").exists(), "a leftover is removed");
    assert!(
        dir.join("windows/notes.txt").exists(),
        "a name that is no pid is left alone"
    );
    assert!(
        std::fs::symlink_metadata(dir.join("windows/8"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "a symlink is skipped, never followed or removed"
    );
    assert_eq!(std::fs::read(&foreign).unwrap(), b"keep");
}

/// What the child half of `a_killed_window_releases_and_is_swept` runs: it holds a window and a
/// running turn until killed. A bare run with nothing to hold does nothing.
#[test]
#[ignore = "run by a_killed_window_releases_and_is_swept as a child process"]
fn presence_child() {
    let Some(dir) = std::env::var_os("EITRI_PRESENCE_CHILD_DIR") else {
        return;
    };
    let mut presence = Presence::open(Path::new(&dir), std::process::id()).unwrap();
    presence.turn_started(5).unwrap();
    // Gives up on its own if its parent never kills it.
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn a_killed_window_releases_and_is_swept() {
    let scratch = Scratch::new("killed");
    let dir = scratch.review_dir();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "presence_child", "--ignored", "--test-threads=1"])
        .env("EITRI_PRESENCE_CHILD_DIR", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let own = 100;
    until("the child to hold its turn", || {
        busy_elsewhere(&dir, own).unwrap() == Some(Busy::TurnRunning { pid })
    });
    // Killed by the pid captured at the spawn.
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(busy_elsewhere(&dir, own).unwrap(), None, "a dead window blocks nobody");
    assert!(!dir.join(format!("windows/{pid}")).exists(), "its window file is swept");
    assert!(!dir.join(format!("running/{pid}.5")).exists(), "and its turn file");
}

#[test]
fn a_holder_whose_file_was_unlinked_recreates_it() {
    let scratch = Scratch::new("unlinked");
    let dir = scratch.review_dir();
    let mut me = Presence::open(&dir, 4242).unwrap();
    me.turn_started(2).unwrap();
    std::fs::remove_file(dir.join("windows/4242")).unwrap();
    std::fs::remove_file(dir.join("running/4242.2")).unwrap();
    assert_eq!(busy_elsewhere(&dir, 100).unwrap(), None, "unlinked, so nobody sees it");
    me.refresh().unwrap();
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::TurnRunning { pid: 4242 }),
        "the turn's file is back"
    );
    me.turn_ended(2);
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::OtherWindow { pid: 4242 }),
        "and the window's"
    );
}

/// `windows` made a symlink to a directory of the user's own elsewhere.
fn symlinked_windows(scratch: &Scratch, dir: &Path) -> PathBuf {
    let target = scratch.0.join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, dir.join("windows")).unwrap();
    target
}

#[test]
fn presence_dirs_are_private_and_never_follow_a_symlink() {
    let scratch = Scratch::new("symlink");
    let dir = scratch.review_dir();
    let target = symlinked_windows(&scratch, &dir);
    assert!(Presence::open(&dir, 4242).is_err());
    assert_eq!(
        std::fs::read_dir(&target).unwrap().count(),
        0,
        "nothing was created in the target"
    );
    // Probing does not read through it either.
    assert!(busy_elsewhere(&dir, 100).is_err());
}

#[test]
fn a_holder_that_could_not_open_refuses_every_check() {
    let scratch = Scratch::new("refuses");
    let dir = scratch.review_dir();
    symlinked_windows(&scratch, &dir);
    let holder = PresenceHolder::start(dir, 4242);
    let guard = holder.guard();
    until("the open to fail", || match guard.check() {
        Err(Blocked::NotHeld(why)) => !why.contains("not been taken yet"),
        other => panic!("unexpected {other:?}"),
    });
    assert!(guard
        .check()
        .unwrap_err()
        .to_string()
        .contains("presence lock is not held"));
}

#[test]
fn a_holder_that_holds_lets_a_lone_window_through_and_blocks_a_second() {
    let scratch = Scratch::new("holder");
    let dir = scratch.review_dir();
    let holder = PresenceHolder::start(dir.clone(), 4242);
    let guard = holder.guard();
    until("the window file", || guard.check().is_ok());
    let _other = Presence::open(&dir, 5000).unwrap();
    assert_eq!(guard.check(), Err(Blocked::Busy(Busy::OtherWindow { pid: 5000 })));
    // Another window sees this one.
    assert_eq!(
        busy_elsewhere(&dir, 5000).unwrap(),
        Some(Busy::OtherWindow { pid: 4242 })
    );
}

#[test]
fn trim_leaves_presence_and_journal_alone() {
    let scratch = Scratch::new("trim");
    let project = scratch.0.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("a.txt"), "a\n").unwrap();
    let dir = scratch.review_dir();
    let shadow = Shadow::open_with_excludes(&dir, &project, None).unwrap();
    let mut me = Presence::open(&dir, 4242).unwrap();
    me.turn_started(1).unwrap();
    std::fs::create_dir_all(dir.join("journal")).unwrap();
    std::fs::write(dir.join("journal/entry"), b"pending").unwrap();
    // A lock file a killed run left is swept by the same pass; none of the presence files is a
    // `.lock`, and none of the directories is entered.
    std::fs::write(dir.join("stale.lock"), b"").unwrap();

    shadow.trim(0, Duration::ZERO, u64::MAX / 2, "now").unwrap();

    assert!(!dir.join("stale.lock").exists(), "the pass ran");
    assert!(dir.join("windows/4242").exists());
    assert!(dir.join("running/4242.1").exists());
    assert_eq!(std::fs::read(dir.join("journal/entry")).unwrap(), b"pending");
    assert_eq!(
        busy_elsewhere(&dir, 100).unwrap(),
        Some(Busy::TurnRunning { pid: 4242 })
    );
}
