#![cfg(target_os = "linux")]

//! The lock file's life under a scratch home: kept while the window lives, walked to again when its directory goes,
//! removed at the end. The trait is driven by hand with made-up instants, so no test waits out a second, and inotify
//! queues an event before the call that caused it returns, so none waits for one either.
//! Every home here is a value under `temp_dir`, handed in, so no real `~/.claude` is ever looked at.

use std::fs::{self, File};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use eitri_core::ide_server::keeper::LockKeeper;
use eitri_core::ide_server::lock::{lock_text, NOT_PRIVATE, NO_HOME};
use eitri_core::ide_server::Housekeeping;

const PORT: u16 = 46321;
const PID: u32 = 4242;

/// A directory of this test's own, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "eitri-keeper-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Scratch(path)
    }

    fn home(&self) -> Option<PathBuf> {
        Some(self.0.clone())
    }

    fn claude(&self) -> PathBuf {
        self.0.join(".claude")
    }

    fn ide(&self) -> PathBuf {
        self.0.join(".claude/ide")
    }

    fn lock(&self) -> PathBuf {
        self.ide().join(format!("{PORT}.lock"))
    }

    /// A keeper that has published.
    fn started(&self) -> LockKeeper {
        let mut keeper = LockKeeper::new(self.home(), PID);
        keeper.start(PORT, &test_token()).unwrap();
        keeper
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.ide(), fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn test_token() -> String {
    "ab".repeat(32)
}

fn the_text() -> String {
    lock_text(PID, &test_token())
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn inode(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().ino()
}

fn locks_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn after(start: Instant, millis: u64) -> Instant {
    start + Duration::from_millis(millis)
}

/// The `pid` of a child already reaped.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// In the environment of a test run again by [`run_again_with_one_watch`].
const ONE_WATCH: &str = "KEEPER_TEST_ONE_WATCH";

/// Runs `test` of this binary again, in a user namespace whose limit is one inotify watch. Only that namespace's limit
/// changes. A machine that gives no user namespaces to this user cannot run the test, and says so.
fn run_again_with_one_watch(test: &str) {
    let namespaces = Command::new("unshare").args(["-r", "true"]).output();
    if !namespaces.is_ok_and(|output| output.status.success()) {
        eprintln!("{test}: skipped, no user namespaces here");
        return;
    }
    let output = Command::new("unshare")
        .args([
            "-r",
            "sh",
            "-c",
            r#"echo 1 > /proc/sys/user/max_inotify_watches && exec "$0" --exact "$1""#,
        ])
        .arg(std::env::current_exe().unwrap())
        .arg(test)
        .env(ONE_WATCH, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rm_writes_it_again() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    // Held open so that its inode number cannot go to the new file.
    let first = File::open(scratch.lock()).unwrap();
    fs::remove_file(scratch.lock()).unwrap();

    keeper.on_wake(Instant::now());

    assert!(exists(&scratch.lock()));
    assert_ne!(
        inode(&scratch.lock()),
        first.metadata().unwrap().ino(),
        "the file came back as a new file"
    );
    assert_eq!(keeper.retry_in(Instant::now()), None, "nothing is owed once it is back");
}

#[test]
fn the_rewrite_is_at_most_once_a_second() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();

    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(start);
    assert!(exists(&scratch.lock()), "the first removal is answered at once");

    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(after(start, 100));
    assert!(!exists(&scratch.lock()), "the second waits for the pace");
    assert_eq!(keeper.retry_in(after(start, 100)), Some(Duration::from_millis(900)));

    keeper.on_wake(after(start, 999));
    assert!(!exists(&scratch.lock()), "still inside the second");

    assert_eq!(keeper.retry_in(after(start, 1000)), Some(Duration::ZERO));
    keeper.on_wake(after(start, 1000));
    assert!(
        exists(&scratch.lock()),
        "the worker's timer writes it once the second is up"
    );
    assert_eq!(keeper.retry_in(after(start, 1000)), None);
}

#[test]
fn the_contents_are_the_same_after_a_rewrite() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());
    fs::remove_file(scratch.lock()).unwrap();

    keeper.on_wake(Instant::now());

    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());
    let mode = fs::symlink_metadata(scratch.lock()).unwrap().mode() & 0o7777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_removed_directory_walks_again_from_home() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    fs::remove_dir_all(scratch.ide()).unwrap();

    keeper.on_wake(Instant::now());

    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());
    assert_eq!(fs::metadata(scratch.ide()).unwrap().mode() & 0o7777, 0o700);
    assert_eq!(keeper.retry_in(Instant::now()), None);
}

#[test]
fn a_removed_claude_directory_is_made_again_too() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    fs::remove_dir_all(scratch.claude()).unwrap();

    keeper.on_wake(Instant::now());

    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());
}

#[test]
fn a_directory_removed_while_a_rewrite_waits_is_walked_to_at_the_pace() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();
    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(start);
    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(after(start, 100));

    // The directory is empty now, so this `rm` raises nothing the keeper could see while it holds a handle on it.
    fs::remove_dir(scratch.ide()).unwrap();

    keeper.on_wake(after(start, 1000));
    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());
    assert_eq!(keeper.retry_in(after(start, 1000)), None);
}

#[test]
fn a_directory_gone_at_the_watch_limit_gives_its_watch_to_the_new_one() {
    if std::env::var_os(ONE_WATCH).is_none() {
        return run_again_with_one_watch("a_directory_gone_at_the_watch_limit_gives_its_watch_to_the_new_one");
    }
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();
    fs::remove_dir_all(scratch.ide()).unwrap();

    keeper.on_wake(start);
    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());
    assert_eq!(keeper.retry_in(start), None);

    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(after(start, 1000));
    assert!(exists(&scratch.lock()), "the new directory is the one watched");
}

#[test]
fn a_replaced_directory_is_watched_anew() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();
    fs::remove_dir_all(scratch.ide()).unwrap();
    fs::DirBuilder::new().mode(0o700).create(scratch.ide()).unwrap();

    keeper.on_wake(start);
    assert!(exists(&scratch.lock()), "published into the new directory");

    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(after(start, 2000));
    assert!(exists(&scratch.lock()), "a removal from the new directory is seen");
}

#[test]
fn a_moved_directory_walks_again_from_home() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();
    let moved = scratch.claude().join("ide.old");
    fs::rename(scratch.ide(), &moved).unwrap();

    keeper.on_wake(start);
    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());

    let moved_lock = moved.join(format!("{PORT}.lock"));
    assert!(exists(&moved_lock), "what the moved directory holds is left in it");
    fs::remove_file(&moved_lock).unwrap();
    keeper.on_wake(after(start, 2000));
    assert_eq!(
        keeper.retry_in(after(start, 2000)),
        None,
        "the old directory is no longer watched"
    );
    assert!(!exists(&moved_lock));

    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(after(start, 4000));
    assert!(exists(&scratch.lock()), "the new directory is");
}

#[test]
fn another_name_changes_nothing() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();
    let first = inode(&scratch.lock());

    // The publish's own rename is in the queue already: it is the first thing this wake reads.
    keeper.on_wake(start);
    assert_eq!(inode(&scratch.lock()), first);
    assert_eq!(keeper.retry_in(start), None);

    let other = scratch.ide().join("1.lock");
    let moved = scratch.ide().join("1.lock.moved");
    fs::write(&other, "not ours").unwrap();
    fs::rename(&other, &moved).unwrap();
    fs::remove_file(&moved).unwrap();

    keeper.on_wake(after(start, 2000));
    assert_eq!(inode(&scratch.lock()), first);
    assert_eq!(keeper.retry_in(after(start, 2000)), None);
    assert_eq!(locks_in(&scratch.ide()), [format!("{PORT}.lock")]);
}

#[test]
fn a_file_put_back_before_the_wake_is_left_as_it_is() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let aside = scratch.ide().join("aside");
    let first = inode(&scratch.lock());
    fs::rename(scratch.lock(), &aside).unwrap();
    fs::rename(&aside, scratch.lock()).unwrap();

    keeper.on_wake(Instant::now());

    assert_eq!(inode(&scratch.lock()), first);
    assert_eq!(keeper.retry_in(Instant::now()), None);
}

#[test]
fn a_failed_watch_leaves_no_lock() {
    fn at_the_limit() -> io::Result<OwnedFd> {
        Err(io::Error::from_raw_os_error(libc::EMFILE))
    }
    let scratch = Scratch::new();
    let mut keeper = LockKeeper::with_inotify(scratch.home(), PID, at_the_limit);

    let why = keeper.start(PORT, &test_token()).unwrap_err();

    assert!(why.contains("watch"), "{why}");
    assert_eq!(locks_in(&scratch.ide()), Vec::<String>::new());
    assert_eq!(keeper.watch_fd(), None);
}

#[test]
fn a_descriptor_that_takes_no_watch_leaves_no_lock() {
    fn not_inotify() -> io::Result<OwnedFd> {
        Ok(File::open("/dev/null")?.into())
    }
    let scratch = Scratch::new();
    let mut keeper = LockKeeper::with_inotify(scratch.home(), PID, not_inotify);

    let why = keeper.start(PORT, &test_token()).unwrap_err();

    assert!(why.contains("watch"), "{why}");
    assert_eq!(locks_in(&scratch.ide()), Vec::<String>::new());
}

#[test]
fn a_start_that_cannot_walk_says_why() {
    let mut keeper = LockKeeper::new(None, PID);
    assert_eq!(keeper.start(PORT, &test_token()).unwrap_err(), NO_HOME);

    let scratch = Scratch::new();
    fs::DirBuilder::new().mode(0o700).create(scratch.claude()).unwrap();
    symlink(&scratch.0, scratch.ide()).unwrap();
    let mut keeper = LockKeeper::new(scratch.home(), PID);
    assert_eq!(keeper.start(PORT, &test_token()).unwrap_err(), NOT_PRIVATE);
    assert_eq!(keeper.watch_fd(), None);
}

#[test]
fn start_sweeps_what_a_dead_window_left() {
    let scratch = Scratch::new();
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(scratch.ide())
        .unwrap();
    let stale = scratch.ide().join("1111.lock");
    fs::write(&stale, lock_text(dead_pid(), &test_token())).unwrap();

    let _keeper = scratch.started();

    assert!(!exists(&stale));
    assert!(exists(&scratch.lock()));
}

#[test]
fn a_removal_right_after_start_is_written_back() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    assert!(keeper.watch_fd().is_some());
    fs::remove_file(scratch.lock()).unwrap();

    keeper.on_wake(Instant::now());

    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());
}

#[test]
fn finish_stops_the_watch_before_it_removes() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();

    keeper.finish();

    assert!(!exists(&scratch.lock()));
    assert_eq!(keeper.watch_fd(), None);
    assert_eq!(keeper.retry_in(start), None);
    // `finish` took the instance and all that was owed, so a later wake has nothing to read or do.
    keeper.on_wake(after(start, 1200));
    assert!(!exists(&scratch.lock()), "nothing writes it back");
}

#[test]
fn finish_drops_a_rewrite_still_owed() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();
    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(start);
    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(after(start, 100));

    keeper.finish();

    assert_eq!(keeper.retry_in(after(start, 1200)), None);
    keeper.on_wake(after(start, 1200));
    assert!(!exists(&scratch.lock()));
}

#[test]
fn finish_removes_the_rewritten_file() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    fs::remove_file(scratch.lock()).unwrap();
    keeper.on_wake(Instant::now());

    keeper.finish();

    assert!(!exists(&scratch.lock()));
}

#[test]
fn finish_removes_only_the_same_file() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let theirs = scratch.ide().join("theirs");
    fs::write(&theirs, "another window's").unwrap();
    fs::rename(&theirs, scratch.lock()).unwrap();

    keeper.finish();

    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), "another window's");
}

#[test]
fn a_directory_made_unsafe_stops_the_writes() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();
    fs::set_permissions(scratch.ide(), fs::Permissions::from_mode(0o777)).unwrap();
    fs::remove_file(scratch.lock()).unwrap();

    keeper.on_wake(start);
    assert!(
        !exists(&scratch.lock()),
        "nothing goes into a directory others can write"
    );
    assert_eq!(keeper.retry_in(start), Some(Duration::from_secs(1)));

    keeper.on_wake(after(start, 1000));
    assert!(!exists(&scratch.lock()));
    assert_eq!(keeper.retry_in(after(start, 1000)), Some(Duration::from_secs(1)));

    fs::set_permissions(scratch.ide(), fs::Permissions::from_mode(0o700)).unwrap();
    keeper.on_wake(after(start, 1500));
    assert!(!exists(&scratch.lock()), "the trying keeps its pace");
    keeper.on_wake(after(start, 2000));
    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());
    assert_eq!(keeper.retry_in(after(start, 2000)), None);
}

#[test]
fn a_link_in_the_directorys_place_stops_the_writes() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let start = Instant::now();
    let elsewhere = scratch.0.join("elsewhere");
    fs::DirBuilder::new().mode(0o700).create(&elsewhere).unwrap();
    fs::remove_dir_all(scratch.ide()).unwrap();
    symlink(&elsewhere, scratch.ide()).unwrap();

    keeper.on_wake(start);
    keeper.on_wake(after(start, 1000));

    assert_eq!(locks_in(&elsewhere), Vec::<String>::new(), "the link is not followed");
    assert!(keeper.retry_in(after(start, 1000)).is_some());

    fs::remove_file(scratch.ide()).unwrap();
    keeper.on_wake(after(start, 2000));
    assert_eq!(fs::read_to_string(scratch.lock()).unwrap(), the_text());
    assert_eq!(keeper.retry_in(after(start, 2000)), None);
}

#[test]
fn a_wake_with_nothing_to_read_changes_nothing() {
    let scratch = Scratch::new();
    let mut keeper = scratch.started();
    let first = inode(&scratch.lock());

    keeper.on_wake(Instant::now());
    keeper.on_wake(Instant::now());

    assert_eq!(inode(&scratch.lock()), first);
}

#[test]
fn a_keeper_not_started_has_nothing_to_watch_or_finish() {
    let scratch = Scratch::new();
    let mut keeper = LockKeeper::new(scratch.home(), PID);

    assert_eq!(keeper.watch_fd(), None);
    assert_eq!(keeper.retry_in(Instant::now()), None);
    keeper.on_wake(Instant::now());
    keeper.finish();

    assert!(!exists(&scratch.claude()), "nothing under the home is touched");
}
