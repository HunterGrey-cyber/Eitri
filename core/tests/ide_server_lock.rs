#![cfg(target_os = "linux")]

//! The lock file under a scratch home: the directories it may go in, what it says, and what the sweep removes.
//! Every home here is a value under `temp_dir`, handed in, so no real `~/.claude` is ever looked at.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eitri_core::ide_server::lock::{
    lock_name, lock_text, open_lock_dir, process_start_wall, publish, remove, stale, sweep, LockDir, NOT_PRIVATE,
    NO_HOME,
};

/// A directory of this test's own, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "eitri-lock-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Scratch(path)
    }

    fn home(&self) -> Option<&Path> {
        Some(&self.0)
    }

    fn ide(&self) -> PathBuf {
        self.0.join(".claude/ide")
    }

    /// The lock directory, with the walk's own handle on it.
    fn dir(&self) -> (LockDir, PathBuf) {
        (open_lock_dir(self.home()).unwrap(), self.ide())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A live process of this test, killed by its handle at the end.
struct Sleeper(Child);

impl Sleeper {
    fn new() -> Sleeper {
        Sleeper(Command::new("sleep").arg("120").spawn().unwrap())
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A positive pid_t above Linux's allocation ceiling, so no later fork can acquire it.
fn dead_pid() -> u32 {
    let pid = i32::MAX as u32;
    let ceiling: u32 = fs::read_to_string("/proc/sys/kernel/pid_max")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid > ceiling);
    assert!(!agent::process_probe::pid_is_alive(pid));
    pid
}

fn test_token() -> String {
    "ab".repeat(32)
}

fn eitri_lock(pid: u32) -> String {
    lock_text(pid, &test_token())
}

fn set_mtime(path: &Path, secs: u64) {
    let file = File::options().write(true).open(path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn make_dir(path: &Path, mode: u32) {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn the_contents_are_exactly_these_keys() {
    let token = test_token();
    assert_eq!(
        lock_text(4242, &token),
        format!(r#"{{"workspaceFolders":[],"pid":4242,"ideName":"Eitri","transport":"ws","authToken":"{token}"}}"#)
    );
    assert_eq!(lock_name(46321), OsStr::new("46321.lock"));
}

#[test]
fn the_lock_is_0600() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    publish(&dir, 46321, &eitri_lock(1)).unwrap();
    assert_eq!(mode(&ide.join("46321.lock")), 0o600);
    assert_eq!(
        fs::read_to_string(ide.join("46321.lock")).unwrap(),
        eitri_lock(1),
        "the name holds the whole text"
    );
}

#[test]
fn removing_takes_only_the_file_that_was_published() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let first = publish(&dir, 46321, "first").unwrap();
    let second = publish(&dir, 46321, "second").unwrap();
    assert!(!remove(&dir, 46321, first).unwrap(), "another file holds the name now");
    assert!(exists(&ide.join("46321.lock")));
    assert!(remove(&dir, 46321, second).unwrap());
    assert!(!exists(&ide.join("46321.lock")));
}

#[test]
fn a_missing_claude_and_ide_are_made_0700() {
    let scratch = Scratch::new();
    let _dir = open_lock_dir(scratch.home()).unwrap();
    assert_eq!(mode(&scratch.0.join(".claude")), 0o700);
    assert_eq!(mode(&scratch.ide()), 0o700);
}

#[test]
fn an_existing_one_keeps_its_mode() {
    let scratch = Scratch::new();
    make_dir(&scratch.0.join(".claude"), 0o750);
    make_dir(&scratch.ide(), 0o711);
    let _dir = open_lock_dir(scratch.home()).unwrap();
    assert_eq!(mode(&scratch.0.join(".claude")), 0o750);
    assert_eq!(mode(&scratch.ide()), 0o711);
}

#[test]
fn a_link_means_no_lock() {
    for link in [".claude", ".claude/ide"] {
        let scratch = Scratch::new();
        let elsewhere = scratch.0.join("elsewhere");
        make_dir(&elsewhere, 0o700);
        if link != ".claude" {
            make_dir(&scratch.0.join(".claude"), 0o700);
        }
        symlink(&elsewhere, scratch.0.join(link)).unwrap();
        assert_eq!(open_lock_dir(scratch.home()).unwrap_err(), NOT_PRIVATE, "{link}");
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0, "{link}");
    }
}

#[test]
fn a_group_writable_directory_means_no_lock() {
    let scratch = Scratch::new();
    make_dir(&scratch.0.join(".claude"), 0o770);
    assert_eq!(open_lock_dir(scratch.home()).unwrap_err(), NOT_PRIVATE);
    assert_eq!(mode(&scratch.0.join(".claude")), 0o770, "refused and left as it was");
    assert!(!exists(&scratch.ide()));

    let scratch = Scratch::new();
    make_dir(&scratch.0.join(".claude"), 0o700);
    make_dir(&scratch.ide(), 0o702);
    assert_eq!(open_lock_dir(scratch.home()).unwrap_err(), NOT_PRIVATE);
    assert_eq!(mode(&scratch.ide()), 0o702, "refused and left as it was");
}

#[test]
fn a_home_that_is_itself_shared_is_no_reason_to_refuse() {
    let scratch = Scratch::new();
    fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o755)).unwrap();
    let _dir = open_lock_dir(scratch.home()).unwrap();
    assert_eq!(mode(&scratch.0), 0o755);
}

#[test]
fn a_relative_or_missing_home_means_no_lock() {
    assert_eq!(open_lock_dir(None).unwrap_err(), NO_HOME);
    assert_eq!(open_lock_dir(Some(Path::new("relative/home"))).unwrap_err(), NO_HOME);
    assert_eq!(open_lock_dir(Some(Path::new(""))).unwrap_err(), NO_HOME);
    let scratch = Scratch::new();
    let gone = scratch.0.join("no such home");
    assert_eq!(open_lock_dir(Some(&gone)).unwrap_err(), NOT_PRIVATE);
}

#[test]
fn the_sweep_removes_an_eitri_lock_with_a_dead_pid() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    fs::write(ide.join("46321.lock"), eitri_lock(dead_pid())).unwrap();
    sweep(&dir);
    assert!(!exists(&ide.join("46321.lock")));
}

#[test]
fn the_sweep_removes_one_whose_pid_now_belongs_to_a_younger_process() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let child = Sleeper::new();
    let start = process_start_wall(child.pid()).unwrap();
    fs::write(ide.join("46321.lock"), eitri_lock(child.pid())).unwrap();
    set_mtime(&ide.join("46321.lock"), start - 10);
    sweep(&dir);
    assert!(!exists(&ide.join("46321.lock")));
}

#[test]
fn the_sweep_leaves_another_ides_lock() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let text = format!(r#"{{"workspaceFolders":[],"pid":{},"ideName":"VS Code"}}"#, dead_pid());
    fs::write(ide.join("46321.lock"), &text).unwrap();
    sweep(&dir);
    assert_eq!(fs::read_to_string(ide.join("46321.lock")).unwrap(), text);
}

#[test]
fn the_sweep_leaves_an_eitri_lock_whose_pid_is_alive_and_older() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let child = Sleeper::new();
    let start = process_start_wall(child.pid()).unwrap();
    fs::write(ide.join("46321.lock"), eitri_lock(child.pid())).unwrap();
    set_mtime(&ide.join("46321.lock"), start + 5);
    sweep(&dir);
    assert!(exists(&ide.join("46321.lock")));
}

#[test]
fn the_sweep_leaves_a_sibling_that_started_within_a_second_of_its_file() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let child = Sleeper::new();
    let start = process_start_wall(child.pid()).unwrap();
    fs::write(ide.join("46321.lock"), eitri_lock(child.pid())).unwrap();
    set_mtime(&ide.join("46321.lock"), start - 1);
    sweep(&dir);
    assert!(exists(&ide.join("46321.lock")));
}

#[test]
fn the_sweep_removes_a_leftover_temporary_name_with_a_dead_pid() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let dead = dead_pid();
    let names = [
        format!(".46321.lock.{dead}.0.tmp"),
        format!(".46321.lock.{dead}.17.tmp"),
    ];
    fs::write(ide.join(&names[0]), "half a lock").unwrap();
    fs::write(ide.join(&names[1]), "").unwrap();
    sweep(&dir);
    assert!(!exists(&ide.join(&names[0])), "whatever its contents");
    assert!(!exists(&ide.join(&names[1])), "whatever its contents");
}

#[test]
fn the_sweep_leaves_a_temporary_name_of_a_live_process() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let child = Sleeper::new();
    let name = format!(".46321.lock.{}.0.tmp", child.pid());
    fs::write(ide.join(&name), "being written").unwrap();
    sweep(&dir);
    assert!(exists(&ide.join(&name)));
}

#[test]
fn the_sweep_leaves_every_other_name_and_shape() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let dead = dead_pid();
    let lock = eitri_lock(dead);
    let kept: Vec<(String, String)> = [
        ("x46321.lock", lock.as_str()),
        ("46321.lock.bak", lock.as_str()),
        (".lock", lock.as_str()),
        ("46321.lock~", lock.as_str()),
        (".46321.lock.notapid.0.tmp", ""),
        ("1.lock", "not json"),
        ("2.lock", r#"{"ideName":"Eitri"}"#),
        ("3.lock", r#"{"ideName":"Eitri","pid":"12"}"#),
        ("4.lock", r#"{"ideName":"Eitri","pid":-1}"#),
        ("5.lock", r#"{"ideName":"Eitri","pid":1.5}"#),
        ("6.lock", r#"{"ideName":"Eitri","pid":99999999999}"#),
    ]
    .iter()
    .map(|(name, text)| (name.to_string(), text.to_string()))
    .chain([
        (format!(".46321.lock.{dead}.0.tmp.bak"), String::new()),
        (format!(".46321.lock.{dead}.tmp"), String::new()),
        (format!(".x.lock.{dead}.0.tmp"), String::new()),
        ("7.lock".to_owned(), format!(r#"{{"pid":{dead}}}"#)),
        ("8.lock".to_owned(), format!(r#"{{"ideName":1,"pid":{dead}}}"#)),
    ])
    .collect();
    for (name, text) in &kept {
        fs::write(ide.join(name), text).unwrap();
    }
    sweep(&dir);
    for (name, _) in &kept {
        assert!(exists(&ide.join(name)), "{name} is not this code's to remove");
    }
}

#[test]
fn the_sweep_follows_no_link() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let text = eitri_lock(dead_pid());
    let target = scratch.0.join("target");
    fs::write(&target, &text).unwrap();
    symlink(&target, ide.join("46321.lock")).unwrap();
    let temporary = format!(".46321.lock.{}.0.tmp", dead_pid());
    symlink(&target, ide.join(&temporary)).unwrap();
    sweep(&dir);
    assert!(exists(&ide.join("46321.lock")), "the link is left");
    assert!(exists(&ide.join(&temporary)), "the link is left");
    assert_eq!(fs::read_to_string(&target).unwrap(), text, "and its target untouched");
}

#[test]
fn the_sweep_leaves_a_directory_of_a_locks_name() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    make_dir(&ide.join("46321.lock"), 0o700);
    sweep(&dir);
    assert!(ide.join("46321.lock").is_dir());
}

#[test]
fn the_sweep_skips_a_large_file_unread() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let lock = eitri_lock(dead_pid());
    let at_the_cap = format!("{lock}{}", " ".repeat(4096 - lock.len()));
    let over_the_cap = format!("{at_the_cap} ");
    fs::write(ide.join("1.lock"), at_the_cap).unwrap();
    fs::write(ide.join("2.lock"), over_the_cap).unwrap();
    sweep(&dir);
    assert!(!exists(&ide.join("1.lock")), "4 KiB is still read");
    assert!(exists(&ide.join("2.lock")), "one byte more is left unread");
}

#[test]
fn the_sweep_visits_at_most_4096_entries() {
    let scratch = Scratch::new();
    let (dir, ide) = scratch.dir();
    let total = 4096 + 100;
    let text = eitri_lock(dead_pid());
    for port in 0..total {
        fs::write(ide.join(format!("{port}.lock")), &text).unwrap();
    }
    sweep(&dir);
    let left = fs::read_dir(&ide).unwrap().count();
    assert_eq!(left, 100, "one pass stops at the bound");
}

#[test]
fn stale_holds_the_two_second_margin() {
    let mtime = 1_000;
    assert!(stale(false, None, mtime), "a dead pid");
    assert!(
        stale(false, Some(mtime), mtime),
        "a dead pid, whatever was read before it went"
    );
    assert!(!stale(true, Some(mtime), mtime));
    assert!(!stale(true, Some(mtime - 500), mtime), "older than the file");
    assert!(!stale(true, Some(mtime + 2), mtime), "inside the margin");
    assert!(stale(true, Some(mtime + 3), mtime), "past the margin");
    assert!(!stale(true, None, mtime), "alive, and no start time to judge it by");
    assert!(!stale(true, Some(u64::MAX), u64::MAX), "the margin cannot wrap");
}

#[test]
fn a_new_process_started_about_now() {
    let child = Sleeper::new();
    let start = process_start_wall(child.pid()).unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    assert!(start.abs_diff(now) < 60, "start {start}, now {now}");
}

#[test]
fn a_process_that_is_gone_has_no_start() {
    assert_eq!(process_start_wall(dead_pid()), None);
}
