//! Private per-instance directories under `TMPDIR`, and the sweep that reclaims ones whose process
//! died without cleaning up.
//!
//! Extracted from `pane_switch.rs` unchanged in behaviour, because the theme feed needs the same
//! thing: a directory holding a socket, unique per window, that a SIGKILL'd `shell` would otherwise
//! leak. The reasoning below was learned in `pane_switch` and is kept with the code it justifies.

use std::path::{Path, PathBuf};

/// `tmp/<prefix><pid>-<uuid>`.
///
/// Per *instance*, not per process: a pid-keyed path silently collapses two instances in one
/// process onto one directory. The pid stays in the name so `ls "$TMPDIR"` says whose it is, and so
/// [`sweep_stale_instance_dirs`] can tell a leaked directory from a live one.
pub(crate) fn instance_dir_path(tmp: &Path, prefix: &str) -> PathBuf {
    tmp.join(format!("{prefix}{}-{}", std::process::id(), uuid::Uuid::new_v4()))
}

/// Deletes directories under `tmp` named `<prefix><pid>-<uuid>` (or the legacy `<prefix><pid>`)
/// whose process is gone.
///
/// A directory is removed only when all four hold: its name has one of those two shapes, `/proc/<pid>`
/// does not exist, nothing answers a `connect()` on its `socket_name`, and the entry is a real
/// directory rather than a symlink to one.
///
/// **Neither liveness check is enough alone.** `/proc/<pid>` answers about this process's own pid
/// namespace, and `TMPDIR` is not necessarily shared with only that namespace -- observed in this
/// project's sandbox, where `/tmp` held host pids while `/proc` showed namespace-local ones -- so a
/// live window's directory can look pid-dead from here. The socket probe closes that hole: a live
/// instance holds a bound listener, so `connect()` succeeds. It cannot stand alone either, because
/// a directory whose creation failed before `bind` has no socket at all.
///
/// The probe runs only when the pid check says dead. When it does run it connects and immediately
/// drops the stream; the live listener sees a connection carrying an empty line, which each
/// module's own parser rejects and logs.
///
/// Every failure is ignored. `TMPDIR` is usually shared and sticky-bit, so a stale-looking entry
/// may belong to another user; that must not stop this process from starting.
pub(crate) fn sweep_stale_instance_dirs(tmp: &Path, prefix: &str, socket_name: &str, log_tag: &str) {
    let Ok(entries) = std::fs::read_dir(tmp) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| stale_instance_dir_pid(n, prefix)) else { continue };
        let path = entry.path();
        if pid_is_running(pid) || socket_has_a_listener(&path.join(socket_name)) {
            continue;
        }
        // `symlink_metadata`, so a symlink that merely *points* at a directory is never followed --
        // `TMPDIR` is writable by anyone, and this is the one call here that deletes.
        let is_real_directory = matches!(path.symlink_metadata(), Ok(meta) if meta.is_dir());
        if is_real_directory && std::fs::remove_dir_all(&path).is_ok() {
            println!("[{log_tag}] reclaimed stale {} (pid {pid} is gone and its socket is dead)", path.display());
        }
    }
}

/// The pid named by a directory [`instance_dir_path`] created with this `prefix`, or `None`.
///
/// The tail must parse as a uuid and the head as a pid, so a name that merely shares the prefix is
/// never a sweep candidate. The parser is the whole safety story for a recursive delete in `TMPDIR`.
pub(crate) fn stale_instance_dir_pid(name: &str, prefix: &str) -> Option<u32> {
    let rest = name.strip_prefix(prefix)?;
    match rest.split_once('-') {
        // A pid is all digits, so the first hyphen is always the one separating it from the uuid.
        Some((pid, uuid)) => {
            uuid::Uuid::parse_str(uuid).ok()?;
            pid.parse().ok()
        }
        None => rest.parse().ok(),
    }
}

/// `true` only for a real, successful `connect()`. Every error answers `false`, because the caller
/// pairs this with a pid check. An `AF_UNIX` `connect()` does not block waiting on a peer.
fn socket_has_a_listener(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// `/proc/<pid>` rather than `kill(pid, 0)`: it needs no signal permission and cannot be mistaken
/// for actually signalling something.
fn pid_is_running(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    const PREFIX: &str = "neovibe-pane-switch-";

    #[test]
    fn only_this_prefixes_own_directory_names_are_sweep_candidates() {
        let uuid = uuid::Uuid::new_v4();
        assert_eq!(stale_instance_dir_pid(&format!("neovibe-pane-switch-4321-{uuid}"), PREFIX), Some(4321));
        assert_eq!(stale_instance_dir_pid("neovibe-pane-switch-4321", PREFIX), Some(4321));
        assert_eq!(stale_instance_dir_pid("neovibe-supervisor.sock", PREFIX), None);
        assert_eq!(stale_instance_dir_pid("systemd-private-abcdef", PREFIX), None);
        assert_eq!(stale_instance_dir_pid("", PREFIX), None);
        assert_eq!(stale_instance_dir_pid("neovibe-pane-switch-4321-scratch", PREFIX), None);
        assert_eq!(stale_instance_dir_pid(&format!("neovibe-pane-switch-notapid-{uuid}"), PREFIX), None);
        assert_eq!(stale_instance_dir_pid("neovibe-pane-switch--1", PREFIX), None);
        // Two modules sharing `TMPDIR` must never sweep each other's directories.
        assert_eq!(stale_instance_dir_pid(&format!("neovibe-theme-4321-{uuid}"), PREFIX), None);
    }

    #[test]
    fn instance_paths_are_unique_and_name_their_process() {
        let tmp = std::env::temp_dir();
        let a = instance_dir_path(&tmp, PREFIX);
        let b = instance_dir_path(&tmp, PREFIX);
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_str().unwrap();
        assert_eq!(stale_instance_dir_pid(name, PREFIX), Some(std::process::id()));
    }

    #[test]
    fn the_sweep_reclaims_a_dead_pids_directory_and_spares_a_live_ones() {
        // Not itself a sweep candidate. A sibling test uses `nv-sw-`; the two must differ, because
        // cargo runs them concurrently in one process and so at one pid.
        let root = std::env::temp_dir().join(format!("nv-sweep-{}", std::process::id()));
        // pid 0: `/proc/0` does not exist on Linux and `std::process::id()` never returns it.
        let dead = root.join(format!("neovibe-pane-switch-0-{}", uuid::Uuid::new_v4()));
        let dead_old_shape = root.join("neovibe-pane-switch-0");
        let live = root.join(format!("neovibe-pane-switch-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
        let stranger = root.join("neovibe-pane-switch-0-definitely-not-a-uuid");
        let unrelated = root.join("some-other-tools-directory");
        for dir in [&dead, &dead_old_shape, &live, &stranger, &unrelated] {
            std::fs::create_dir_all(dir.join("bin")).expect("build the fixture");
        }

        sweep_stale_instance_dirs(&root, PREFIX, "switch.sock", "test");

        assert!(!dead.exists(), "a dead pid's directory must be reclaimed");
        assert!(!dead_old_shape.exists(), "a pre-uuid directory must be reclaimed too");
        assert!(live.exists(), "a live pid's directory must be left strictly alone");
        assert!(stranger.exists(), "a name that is not this prefix's shape must be left alone");
        assert!(unrelated.exists(), "an unrelated entry must be left alone");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_listening_socket_saves_a_directory_the_pid_check_calls_dead() {
        // Terse on purpose: an `AF_UNIX` path is capped at ~108 bytes.
        let root = std::env::temp_dir().join(format!("nv-sw-{}", std::process::id()));
        let listening = root.join(format!("neovibe-pane-switch-0-{}", uuid::Uuid::new_v4()));
        let silent = root.join(format!("neovibe-pane-switch-0-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&listening).expect("build the fixture");
        std::fs::create_dir_all(&silent).expect("build the fixture");
        let listener = UnixListener::bind(listening.join("switch.sock")).expect("bind");
        std::fs::write(silent.join("switch.sock"), b"").expect("write a dead socket file");

        sweep_stale_instance_dirs(&root, PREFIX, "switch.sock", "test");

        assert!(listening.exists(), "a directory with a live listener must never be reclaimed");
        assert!(!silent.exists(), "a directory whose socket answers nothing must be reclaimed");
        drop(listener);
        let _ = std::fs::remove_dir_all(&root);
    }
}
