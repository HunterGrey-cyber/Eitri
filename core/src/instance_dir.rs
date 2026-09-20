//! Private per-instance directories under `TMPDIR`, and the sweep that reclaims ones whose process
//! died without cleaning up.
//!
//! Extracted from `pane_switch.rs` unchanged in behaviour, because the theme feed needs the same
//! thing: a directory holding a socket, unique per window, that a SIGKILL'd `shell` would otherwise
//! leak. The reasoning below was learned in `pane_switch` and is kept with the code it justifies.
//!
//! The sweep's pid-liveness check was `/proc/<pid>` until this module moved into `neovibe-core`
//! (L2 T3, 2026-09-17): that answers "not alive" unconditionally on macOS, which has no `/proc`, so
//! every instance would have looked dead. It is now `agent::process_probe::pid_is_alive` -- see
//! [`pid_is_running`]'s own doc for why that also answers Linux's own two hazards (an unprivileged
//! caller, and a pid worth rejecting before any syscall) at least as well as `/proc` did.

use std::path::{Path, PathBuf};

/// `tmp/<prefix><pid>-<32 hex uuid>`.
///
/// Per *instance*, not per process: a pid-keyed path silently collapses two instances in one
/// process onto one directory. The pid stays in the name so `ls "$TMPDIR"` says whose it is, and so
/// [`sweep_stale_instance_dirs`] can tell a leaked directory from a live one.
///
/// The uuid is written in its 32-hex `simple` form rather than the 36-byte hyphenated one (L2 T5,
/// 2026-09-17). Four bytes, and they matter: the socket inside one of these directories is capped
/// at 103 bytes on macOS, where `TMPDIR` alone is 49. [`stale_instance_dir_pid`] parses either form,
/// so directories a pre-L2-T5 build left behind are still recognised -- and each caller sweeps its
/// own pre-L2-T5 *prefix* alongside its current one, which is the part a shorter uuid does not cover.
pub(crate) fn instance_dir_path(tmp: &Path, prefix: &str) -> PathBuf {
    tmp.join(format!(
        "{prefix}{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

/// Deletes directories under `tmp` named `<prefix><pid>-<uuid>` (or the legacy `<prefix><pid>`)
/// whose process is gone.
///
/// A directory is removed only when all four hold: its name has one of those two shapes, its pid
/// is not alive (`agent::process_probe::pid_is_alive`), nothing answers a `connect()` on its
/// `socket_name`, and the entry is a real directory rather than a symlink to one.
///
/// **Neither liveness check is enough alone.** The pid check answers about this process's own pid
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
        let Some(pid) = name.to_str().and_then(|n| stale_instance_dir_pid(n, prefix)) else {
            continue;
        };
        let path = entry.path();
        if pid_is_running(pid) || socket_has_a_listener(&path.join(socket_name)) {
            continue;
        }
        // `symlink_metadata`, so a symlink that merely *points* at a directory is never followed --
        // `TMPDIR` is writable by anyone, and this is the one call here that deletes.
        let is_real_directory = matches!(path.symlink_metadata(), Ok(meta) if meta.is_dir());
        if is_real_directory && std::fs::remove_dir_all(&path).is_ok() {
            println!(
                "[{log_tag}] reclaimed stale {} (pid {pid} is gone and its socket is dead)",
                path.display()
            );
        }
    }
}

/// The pid named by a directory [`instance_dir_path`] created with this `prefix`, or `None`.
///
/// The tail must parse as a uuid and the head as a pid, so a name that merely shares the prefix is
/// never a sweep candidate. The parser is the whole safety story for a recursive delete in `TMPDIR`.
///
/// `Uuid::parse_str` accepts the hyphenated form as well as the 32-hex `simple` one
/// [`instance_dir_path`] writes since L2 T5, which is what lets one sweep reclaim a pre-L2-T5
/// build's directories; `a_pre_l2_t5_hyphenated_uuid_is_still_recognised` pins that.
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

/// Used to be `/proc/<pid>` rather than `kill(pid, 0)`, on the reasoning that it needs no signal
/// permission and cannot be mistaken for actually signalling something. Both objections are
/// answered by `agent::process_probe::pid_is_alive` rather than sidestepped (M1, 2026-09-17;
/// L2 T3, this call site): it treats `EPERM` as alive, so it needs no signal permission to answer
/// correctly; signal `0` delivers nothing, so it performs only the existence and permission check,
/// never an actual signal; it rejects pid `0` and anything above `i32::MAX` before the syscall,
/// which a naive `kill()` would get wrong (`0` means "my own process group", and `u32::MAX` wraps
/// to `-1`, "every process I may signal"); and on a DEFAULT `/proc` mount it answers identically
/// to the old `/proc/<pid>` check for both zombies and other users' processes. Under `hidepid=2`
/// (a real hardening option, not hypothetical) it is strictly BETTER, not merely equal: another
/// user's live process is invisible in `/proc` there, so the old check would have read it as dead
/// and reclaimed a live instance's directory, while `EPERM` from `kill` still reports it alive
/// (corrected 2026-09-17, independent review -- the earlier "identically" claim here was true only
/// on a default mount; see `docs/canonical/dated_record.md`'s 2026-09-17 L2 T3 entry). It is also
/// the one check this module can share with macOS, which has no `/proc` at all.
fn pid_is_running(pid: u32) -> bool {
    agent::process_probe::pid_is_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    const PREFIX: &str = "neovibe-pane-switch-";

    #[test]
    fn only_this_prefixes_own_directory_names_are_sweep_candidates() {
        let uuid = uuid::Uuid::new_v4();
        assert_eq!(
            stale_instance_dir_pid(&format!("neovibe-pane-switch-4321-{uuid}"), PREFIX),
            Some(4321)
        );
        assert_eq!(stale_instance_dir_pid("neovibe-pane-switch-4321", PREFIX), Some(4321));
        assert_eq!(stale_instance_dir_pid("neovibe-supervisor.sock", PREFIX), None);
        assert_eq!(stale_instance_dir_pid("systemd-private-abcdef", PREFIX), None);
        assert_eq!(stale_instance_dir_pid("", PREFIX), None);
        assert_eq!(stale_instance_dir_pid("neovibe-pane-switch-4321-scratch", PREFIX), None);
        assert_eq!(
            stale_instance_dir_pid(&format!("neovibe-pane-switch-notapid-{uuid}"), PREFIX),
            None
        );
        assert_eq!(stale_instance_dir_pid("neovibe-pane-switch--1", PREFIX), None);
        // Two modules sharing `TMPDIR` must never sweep each other's directories.
        assert_eq!(
            stale_instance_dir_pid(&format!("neovibe-theme-4321-{uuid}"), PREFIX),
            None
        );
    }

    #[test]
    fn instance_paths_are_unique_and_name_their_process() {
        let tmp = std::env::temp_dir();
        let a = instance_dir_path(&tmp, PREFIX);
        let b = instance_dir_path(&tmp, PREFIX);
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_str().unwrap();
        assert_eq!(stale_instance_dir_pid(name, PREFIX), Some(std::process::id()));
        // The 32-hex `simple` form, not the 36-byte hyphenated one: those four bytes are what put
        // `shell`'s two sockets back under macOS's 103-byte cap (L2 T5).
        assert_eq!(
            name.len() - PREFIX.len() - std::process::id().to_string().len() - 1,
            32,
            "{name}"
        );
    }

    /// The sweep still recognises what a pre-L2-T5 build wrote. Without this, every directory an
    /// older `shell` leaked would be *permanently* unreclaimable -- and unlike the prefix rename,
    /// which each caller answers by sweeping its own legacy prefix too, this half needs no second
    /// call, only a parser that keeps accepting the old spelling.
    #[test]
    fn a_pre_l2_t5_hyphenated_uuid_is_still_recognised() {
        let uuid = uuid::Uuid::new_v4();
        assert_eq!(uuid.to_string().len(), 36);
        assert_eq!(uuid.simple().to_string().len(), 32);
        for tail in [uuid.to_string(), uuid.simple().to_string()] {
            assert_eq!(
                stale_instance_dir_pid(&format!("{PREFIX}4321-{tail}"), PREFIX),
                Some(4321)
            );
        }
    }

    #[test]
    fn the_sweep_reclaims_a_dead_pids_directory_and_spares_a_live_ones() {
        // Not itself a sweep candidate. A sibling test uses `nv-sw-`; the two must differ, because
        // cargo runs them concurrently in one process and so at one pid.
        let root = std::env::temp_dir().join(format!("nv-sweep-{}", std::process::id()));
        // pid 0: `pid_is_alive` rejects it before any syscall, and `std::process::id()` never
        // returns it, so it is always a safe "definitely dead" pid to plant a fixture at.
        let dead = root.join(format!("neovibe-pane-switch-0-{}", uuid::Uuid::new_v4()));
        let dead_old_shape = root.join("neovibe-pane-switch-0");
        let live = root.join(format!(
            "neovibe-pane-switch-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let stranger = root.join("neovibe-pane-switch-0-definitely-not-a-uuid");
        let unrelated = root.join("some-other-tools-directory");
        for dir in [&dead, &dead_old_shape, &live, &stranger, &unrelated] {
            std::fs::create_dir_all(dir.join("bin")).expect("build the fixture");
        }

        sweep_stale_instance_dirs(&root, PREFIX, "switch.sock", "test");

        assert!(!dead.exists(), "a dead pid's directory must be reclaimed");
        assert!(!dead_old_shape.exists(), "a pre-uuid directory must be reclaimed too");
        assert!(live.exists(), "a live pid's directory must be left strictly alone");
        assert!(
            stranger.exists(),
            "a name that is not this prefix's shape must be left alone"
        );
        assert!(unrelated.exists(), "an unrelated entry must be left alone");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A prefix for the one fixture here that really binds. `PREFIX` cannot be used for it: this
    /// test nests a fixture root under `TMPDIR`, so the bound path is
    /// `<TMPDIR>/<root>/<prefix>0-<uuid>/<sock>`, and with `PREFIX` and a hyphenated uuid that came
    /// to **131 bytes** under macOS's 49-byte `TMPDIR` -- past the 103-byte cap, so the `bind`
    /// below could not have run on a Mac at all. (Pre-existing, and found by L2 T5's follow-up
    /// while adding `lib.rs`'s socket-path scanner; nothing about it was T5's doing.) This module
    /// is generic over the prefix, so shortening the fixture's costs the test nothing -- what it is
    /// about is the sweep's two liveness checks, not any particular name.
    const BIND_PREFIX: &str = "p-";

    #[test]
    fn a_listening_socket_saves_a_directory_the_pid_check_calls_dead() {
        // Terse on purpose, all the way down: see `BIND_PREFIX`. At macOS's 5-digit `PID_MAX` the
        // bound path below is 100 bytes under a 49-byte `TMPDIR`, and 102 at a 7-digit Linux pid.
        let root = std::env::temp_dir().join(format!("nv{}", std::process::id()));
        let listening = root.join(format!("{BIND_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        let silent = root.join(format!("{BIND_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&listening).expect("build the fixture");
        std::fs::create_dir_all(&silent).expect("build the fixture");
        // Through `in_dir` like every other bind in this workspace, so an over-cap fixture is
        // refused naming its own length rather than failing inside `bind` with std's message.
        let listener =
            UnixListener::bind(agent::socket_path::in_dir(&listening, "s.sock").expect("under the cap")).expect("bind");
        std::fs::write(silent.join("s.sock"), b"").expect("write a dead socket file");

        sweep_stale_instance_dirs(&root, BIND_PREFIX, "s.sock", "test");

        assert!(
            listening.exists(),
            "a directory with a live listener must never be reclaimed"
        );
        assert!(
            !silent.exists(),
            "a directory whose socket answers nothing must be reclaimed"
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(&root);
    }
}
