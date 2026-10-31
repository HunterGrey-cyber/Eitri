//! The lock file: its directories, its contents, and the sweep of stale locks.
//!
//! `~/.claude` and `~/.claude/ide` belong to the CLI and every IDE that shares them, so they are judged and never
//! changed, and the lock is the only thing written there. Every step works from a handle on the directory that was
//! judged, never from a path walked again.

use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use agent::private_fs::{self, FileIdentity};
use agent::process_probe::pid_is_alive;

use crate::split::proc_start_time;

use super::types::IDE_NAME;

pub const NOT_PRIVATE: &str = "terminal claude: ~/.claude/ide is not a private directory of yours; not attached";
pub const NO_HOME: &str = "terminal claude: HOME is not an absolute path; not attached";

/// Entries one sweep looks at: a directory stuffed with names must not hold the worker up.
const SWEEP_ENTRIES: usize = 4096;
/// The largest file the sweep looks into. A lock of this code's is far smaller.
const SWEEP_FILE_BYTES: u64 = 4096;
/// How far a process's start may lie past its lock's mtime. Both are whole seconds, and a window publishes within a
/// second of starting.
const START_MARGIN_SECS: u64 = 2;

/// A handle on the directory locks go in, opened by [`open_lock_dir`].
#[derive(Debug)]
pub struct LockDir {
    handle: File,
}

impl LockDir {
    /// This handle as a path, so what needs a path (a directory listing, an inotify watch) reaches the very directory
    /// that was judged, whatever has since been renamed onto its old name.
    pub fn proc_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.handle.as_raw_fd()))
    }
}

/// `~/.claude/ide` with `home` as `~`. A home need not be private, since it is often 0755; `.claude` and `ide` are
/// held to [`private_fs::ensure_child_dir`]'s test. The caller gives `HOME` and nothing else.
pub fn open_lock_dir(home: Option<&Path>) -> Result<LockDir, &'static str> {
    walk(home, &[".claude", "ide"])
}

/// `names` below `home`, each judged from the handle on the one above it.
pub(crate) fn walk(home: Option<&Path>, names: &[&str]) -> Result<LockDir, &'static str> {
    let home = home.filter(|home| home.is_absolute()).ok_or(NO_HOME)?;
    let mut handle = private_fs::open_dir(home).map_err(|_| NOT_PRIVATE)?;
    for name in names {
        handle = private_fs::ensure_child_dir(&handle, OsStr::new(name)).map_err(|_| NOT_PRIVATE)?;
    }
    Ok(LockDir { handle })
}

/// The whole of a lock. `workspaceFolders` stays empty so that only the port Eitri sets attaches, and not any `claude`
/// that happens to run inside the project.
pub fn lock_text(pid: u32, token: &str) -> String {
    let token = serde_json::Value::from(token);
    format!(r#"{{"workspaceFolders":[],"pid":{pid},"ideName":"{IDE_NAME}","transport":"ws","authToken":{token}}}"#)
}

pub fn lock_name(port: u16) -> OsString {
    format!("{port}.lock").into()
}

/// Puts `text` in place whole, replacing a dead window's lock for the same port.
pub fn publish(dir: &LockDir, port: u16, text: &str) -> io::Result<FileIdentity> {
    private_fs::publish_private_file(&dir.handle, &lock_name(port), text.as_bytes())
}

/// Removes the lock for `port` only while it is still the file `identity` names.
pub fn remove(dir: &LockDir, port: u16, identity: FileIdentity) -> io::Result<bool> {
    private_fs::remove_if_same(&dir.handle, &lock_name(port), identity)
}

/// A name the sweep may act on.
#[derive(Debug, PartialEq, Eq)]
enum SweptName {
    /// `<digits>.lock`, which any IDE may hold: only its contents say whose it is.
    Lock,
    /// `publish_private_file`'s temporary for such a name, and the pid that made it.
    Temporary(u32),
}

fn all_digits(text: &[u8]) -> bool {
    !text.is_empty() && text.iter().all(u8::is_ascii_digit)
}

fn swept_name(name: &OsStr) -> Option<SweptName> {
    let name = name.as_bytes();
    if name.strip_suffix(b".lock").is_some_and(all_digits) {
        return Some(SweptName::Lock);
    }
    let middle = name.strip_prefix(b".")?.strip_suffix(b".tmp")?;
    let parts: Vec<&[u8]> = middle.split(|byte| *byte == b'.').collect();
    let [port, b"lock", pid, serial] = parts[..] else {
        return None;
    };
    if !all_digits(port) || !all_digits(pid) || !all_digits(serial) {
        return None;
    }
    let pid = std::str::from_utf8(pid).ok()?.parse().ok()?;
    Some(SweptName::Temporary(pid))
}

/// Removes Eitri's own stale locks and this code's own stale temporary names, and nothing else: another IDE's lock is
/// not read past its `ideName`, and a name of any other shape is not looked at. One pass over at most `SWEEP_ENTRIES`
/// entries; what lies beyond them waits for the next window, or for the CLI's own sweep.
pub fn sweep(dir: &LockDir) {
    let Ok(entries) = std::fs::read_dir(dir.proc_path()) else {
        return;
    };
    for entry in entries.take(SWEEP_ENTRIES).flatten() {
        sweep_entry(dir, &entry.file_name());
    }
}

fn sweep_entry(dir: &LockDir, name: &OsStr) {
    let Some(shape) = swept_name(name) else {
        return;
    };
    let path = dir.proc_path().join(name);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return;
    };
    if !meta.is_file() || meta.uid() != private_fs::euid() || meta.len() > SWEEP_FILE_BYTES {
        return;
    }
    let remove_it = match shape {
        SweptName::Lock => lock_is_stale(&path, &meta),
        SweptName::Temporary(pid) => !pid_is_alive(pid),
    };
    if remove_it {
        let identity = FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        };
        // What was judged is what goes: a window that published over the name since then keeps its file.
        let _ = private_fs::remove_if_same(&dir.handle, name, identity);
    }
}

/// Only this user can change a directory `open_lock_dir` accepted, and the read refuses anything but a regular file of
/// this user on the open handle, so the moment between `meta` and the read gives no other program a way in.
fn lock_is_stale(path: &Path, meta: &Metadata) -> bool {
    let Ok(text) = private_fs::read_private_to_string_capped(path, SWEEP_FILE_BYTES) else {
        return false;
    };
    let Some(pid) = eitri_lock_pid(&text) else {
        return false;
    };
    let alive = pid_is_alive(pid);
    let start = if alive { process_start_wall(pid) } else { None };
    stale(alive, start, u64::try_from(meta.mtime()).unwrap_or(0))
}

/// The `pid` of a lock Eitri wrote. A number that cannot be a pid is not a lock of this code's.
fn eitri_lock_pid(text: &str) -> Option<u32> {
    let lock: serde_json::Value = serde_json::from_str(text).ok()?;
    if lock.get("ideName")?.as_str()? != IDE_NAME {
        return None;
    }
    u32::try_from(lock.get("pid")?.as_u64()?).ok()
}

/// Whether a lock of Eitri's outlived its window: its pid is dead, or now names a process that started after the file
/// was written, past the margin. A live process whose start cannot be read is left.
pub fn stale(alive: bool, start: Option<u64>, mtime: u64) -> bool {
    !alive || start.is_some_and(|start| start > mtime.saturating_add(START_MARGIN_SECS))
}

/// When `pid` started, in whole seconds of wall time, the clock an mtime is on; `None` for a process that is gone.
pub fn process_start_wall(pid: u32) -> Option<u64> {
    let ticks = proc_start_time(pid)?;
    // SAFETY: `sysconf` takes no pointers and only reads a system setting.
    let ticks_per_second = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    let boot = boot_time(&std::fs::read_to_string("/proc/stat").ok()?)?;
    Some(boot + ticks.checked_div(ticks_per_second)?)
}

/// The `btime` line of `/proc/stat`: the wall time of boot, in seconds.
fn boot_time(stat: &str) -> Option<u64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_owners_directory_means_no_lock() {
        if private_fs::euid() == 0 {
            return;
        }
        assert_eq!(walk(Some(Path::new("/usr")), &["lib", "x"]).unwrap_err(), NOT_PRIVATE);
    }

    #[test]
    fn a_lock_name_is_digits_and_a_temporary_is_what_publishing_makes() {
        let swept = |name: &str| swept_name(OsStr::new(name));
        assert_eq!(swept("46321.lock"), Some(SweptName::Lock));
        assert_eq!(swept(".46321.lock.77.3.tmp"), Some(SweptName::Temporary(77)));
        for other in [
            ".lock",
            "4632x.lock",
            "46321.lock.bak",
            ".46321.lock.77.tmp",
            ".46321.lock.77.3.4.tmp",
            ".46321.lock.77.3.tmp.bak",
            ".x.lock.77.3.tmp",
            ".46321.lockx.77.3.tmp",
            ".46321.lock..3.tmp",
            ".46321.lock.77..tmp",
            ".46321.lock.99999999999.3.tmp",
            "",
        ] {
            assert_eq!(swept(other), None, "{other:?}");
        }
        assert_eq!(swept_name(OsStr::from_bytes(b"\xff.lock")), None);
    }

    #[test]
    fn the_boot_time_is_read_from_its_own_line() {
        assert_eq!(
            boot_time("cpu 1 2 3\nbtime 1700000000\nprocesses 9\n"),
            Some(1_700_000_000)
        );
        assert_eq!(boot_time("cpu 1 2 3\n"), None);
        assert_eq!(boot_time("btime soon\n"), None);
    }
}
