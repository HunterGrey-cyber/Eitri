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
//!
//! **C4 (v1 hardening): the sweep must never block startup.** Finding 7's owner check (below) closed
//! the hang for another user's directory, but a directory of *ours* that an older build left
//! group-writable (`0775` under umask 002) reopened it: a member of our own group could rename their
//! own full-backlog listener over `socket_name` between the owner check and the connect, and the old
//! `std::os::unix::net::UnixStream::connect` blocks until that listener's accept queue has room --
//! sleeping this launch before its window exists. Two independent fixes, neither alone enough:
//! (a) [`sweep_entries_owned_by`] now skips -- without connecting and without removing -- any
//! candidate whose mode has a group- or other-write bit (`mode & 0o022 != 0`), which also covers a
//! *stranger's* directory renamed to look like ours before the owner check runs; and (b) the probe
//! itself ([`socket_has_a_listener`]) is now a raw, non-blocking `AF_UNIX` `connect()` rather than
//! `UnixStream::connect`, so even a socket that passes (a) can never hang the sweep: `EAGAIN`,
//! `EWOULDBLOCK` and `EINPROGRESS` all mean "a listener is there" (its queue is merely full or the
//! connection is still completing), and everything else means it is not.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
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
/// A directory is removed only when all six hold: its name has one of those two shapes, the entry
/// is a real directory rather than a symlink to one, it is owned by this user, it has no group- or
/// other-write bit, its pid is not alive (`agent::process_probe::pid_is_alive`), and nothing
/// answers a non-blocking `connect()` on its `socket_name`.
///
/// **Type, owner and mode all come before anything is probed** (local-IPC review finding 7, ruling
/// R5; the mode check is C4(a), v1 hardening). An entry that is not ours, or is ours but still has
/// a group- or other-write bit (an older, pre-hardening build's `0775` under umask 002), is skipped
/// untouched -- neither connected to nor removed -- because another local account in our group could
/// otherwise rename their own listener over `socket_name` between this check and the connect below.
/// `0755` is deliberately accepted, not just `0700`: its entries can still be renamed by nobody but
/// us, and refusing anything wider than exactly `0700` would leave every directory a pre-hardening
/// build ever created (all of them `0755`) permanently unreclaimable. A socket inside a directory
/// that does pass is probed only when it is itself a real socket we own -- one that is not is
/// "nothing answers".
///
/// **Neither liveness check is enough alone.** The pid check answers about this process's own pid
/// namespace, and `TMPDIR` is not necessarily shared with only that namespace -- observed in this
/// project's sandbox, where `/tmp` held host pids while `/proc` showed namespace-local ones -- so a
/// live window's directory can look pid-dead from here. The socket probe closes that hole: a live
/// instance holds a bound listener, so the connect finds it (see [`socket_has_a_listener`] for what
/// "finds it" means now). It cannot stand alone either, because a directory whose creation failed
/// before `bind` has no socket at all.
///
/// The probe runs only when the pid check says dead. When it does run it connects and immediately
/// closes the socket; a live listener whose queue has room sees a connection carrying an empty
/// line, which each module's own parser rejects and logs -- and one whose queue is full never even
/// sees that, because the connect no longer waits for room (C4(b): see the module doc).
///
/// Every failure is ignored. `TMPDIR` is usually shared and sticky-bit, so a stale-looking entry
/// may belong to another user; that must not stop this process from starting.
pub(crate) fn sweep_stale_instance_dirs(tmp: &Path, prefix: &str, socket_name: &str, log_tag: &str) {
    sweep_entries_owned_by(tmp, prefix, socket_name, log_tag, agent::private_fs::current_uid());
}

/// [`sweep_stale_instance_dirs`] for the entries owned by `uid` -- a parameter only so a test can
/// stand in for another user's directory, which it cannot create.
fn sweep_entries_owned_by(tmp: &Path, prefix: &str, socket_name: &str, log_tag: &str, uid: u32) {
    let Ok(entries) = std::fs::read_dir(tmp) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| stale_instance_dir_pid(n, prefix)) else {
            continue;
        };
        let path = entry.path();
        // `symlink_metadata`, so a symlink that merely *points* at a directory is never followed --
        // `TMPDIR` is writable by anyone, and this sweep deletes.
        let Ok(meta) = path.symlink_metadata() else { continue };
        if !meta.is_dir() || meta.uid() != uid {
            continue;
        }
        // C4(a): skip a group- or other-writable directory of ours before connecting to (or
        // removing) anything inside it -- see the module doc's C4 section for why `& 0o022` and
        // not "must be exactly 0700".
        if meta.mode() & 0o022 != 0 {
            continue;
        }
        if pid_is_running(pid) || socket_has_a_listener(&path.join(socket_name), uid) {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
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

/// `true` for a socket `uid` owns that a listener answers -- including one whose accept queue is
/// merely full. Every other outcome answers `false`, because the caller pairs this with a pid check.
///
/// C4(b): this used to be `std::os::unix::net::UnixStream::connect`, a *blocking* connect. An
/// `AF_UNIX` `connect()` never waits for a peer to `accept`, but it DOES block while the listener's
/// accept queue is full -- sleeping this call, and the whole sweep, until the peer drains it (which,
/// for a listener nobody is servicing, is forever). The probe is now a raw, non-blocking connect
/// ([`nonblocking_connect_finds_a_listener`]) so a full queue can never hang the caller: it is one
/// of the three outcomes this treats as "a listener is there", the same as before it retired.
fn socket_has_a_listener(socket: &Path, uid: u32) -> bool {
    let ours = matches!(socket.symlink_metadata(), Ok(meta) if meta.file_type().is_socket() && meta.uid() == uid);
    ours && nonblocking_connect_finds_a_listener(socket)
}

/// One non-blocking `AF_UNIX`/`SOCK_STREAM` `connect()` to `path`. `true` for the three outcomes
/// that mean a listener is really there: `connect()` succeeding outright, `EAGAIN`/`EWOULDBLOCK`
/// (Linux: the listener's accept queue is full), and `EINPROGRESS` (the connection is still being
/// established). Every other outcome -- `ECONNREFUSED` (a dead socket file nothing is listening on),
/// `ENOENT`, a `path` too long for `sockaddr_un`, `socket()` itself failing -- answers `false`. The
/// socket fd is closed before returning in every case. (macOS never blocks here in the first place:
/// its `connect()` to a full queue fails at once with `ECONNREFUSED`, which reads as no listener --
/// harmless, since the pid check before this has no pid namespaces there to be fooled by.)
fn nonblocking_connect_finds_a_listener(path: &Path) -> bool {
    let Some(addr) = sockaddr_for(path) else { return false };
    let Some(fd) = nonblocking_unix_socket() else {
        return false;
    };
    // SAFETY: `fd` is this call's own, freshly opened and still open; `addr` is a fully
    // initialized `sockaddr_un` and its exact size is passed.
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    let has_a_listener = rc == 0
        || matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(e) if e == libc::EAGAIN || e == libc::EWOULDBLOCK || e == libc::EINPROGRESS
        );
    // SAFETY: `fd` is this call's own descriptor, still open, closed exactly once here.
    unsafe { libc::close(fd) };
    has_a_listener
}

/// A new `AF_UNIX` stream socket, non-blocking and close-on-exec, or `None`. Set with `fcntl`
/// rather than `SOCK_NONBLOCK | SOCK_CLOEXEC` on `socket()`: macOS has neither flag, and this crate
/// must build there (the macOS track's M2). The fd is the caller's to close.
fn nonblocking_unix_socket() -> Option<libc::c_int> {
    // SAFETY: plain syscalls on fixed arguments; `fd` is checked before use and closed on failure.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0
            || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            || libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0
        {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

/// `path` as a `sockaddr_un`, or `None` if it does not fit `sun_path` (with room left for the
/// trailing NUL `zeroed()` already provides). The socket paths this crate itself builds are already
/// checked by `agent::socket_path::in_dir`; a stray, over-long entry someone else left in `TMPDIR`
/// answers "no listener" here rather than panicking or silently truncating.
fn sockaddr_for(path: &Path) -> Option<libc::sockaddr_un> {
    // SAFETY: `sockaddr_un` is plain old data; all-zero is a valid (empty) value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return None;
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    // The BSDs, macOS among them, carry the address's length in the address itself.
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        addr.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
    }
    Some(addr)
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
    use std::os::unix::fs::PermissionsExt;
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

    /// Local-IPC review finding 7: an entry another user owns is skipped before anything connects to
    /// it -- the owner check runs first, so a full backlog there can no longer hang this launch.
    /// Another user's directory cannot be made here, so the sweep is told a different uid is "us";
    /// the control, with the real uid, shows the same fixture IS probed when it is ours.
    #[test]
    fn another_users_directory_is_never_connected_to() {
        let root = std::env::temp_dir().join(format!("nvo{}", std::process::id()));
        let candidate = root.join(format!("{BIND_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&candidate).expect("build the fixture");
        let listener =
            UnixListener::bind(agent::socket_path::in_dir(&candidate, "s.sock").expect("under the cap")).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        let file_named_like_one = root.join(format!("{BIND_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&file_named_like_one, b"").expect("a regular file with a candidate's name");
        let me = agent::private_fs::current_uid();

        sweep_entries_owned_by(&root, BIND_PREFIX, "s.sock", "test", me.wrapping_add(1));
        assert!(
            matches!(listener.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "another user's socket must not be connected to"
        );
        assert!(candidate.exists(), "and another user's directory is never removed");
        assert!(
            file_named_like_one.exists(),
            "a file with a candidate's name is not ours to touch"
        );

        sweep_entries_owned_by(&root, BIND_PREFIX, "s.sock", "test", me);
        assert!(listener.accept().is_ok(), "the control: our own directory is probed");
        assert!(candidate.exists(), "and kept, since it answered");
        drop(listener);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// C4(a): a directory of ours that is group- or other-writable (an older, pre-hardening build's
    /// `0775` under umask 002) must never be connected to at all -- a member of our group could
    /// have renamed their own full-backlog listener over the socket between the owner check and the
    /// connect, which is exactly the hang finding 7 already closed for another user's directory.
    /// `0755` is deliberately accepted here (not required to be exactly `0700`): a pre-hardening
    /// directory's entries can still be renamed by nobody but us, and refusing anything but `0700`
    /// would leak every stale directory such a build ever left behind.
    #[test]
    fn a_group_writable_directory_is_never_connected_to() {
        let root = std::env::temp_dir().join(format!("nvg{}", std::process::id()));
        let candidate = root.join(format!("{BIND_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&candidate).expect("build the fixture");
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o775)).expect("chmod 0775");
        let listener =
            UnixListener::bind(agent::socket_path::in_dir(&candidate, "s.sock").expect("under the cap")).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");

        sweep_stale_instance_dirs(&root, BIND_PREFIX, "s.sock", "test");

        assert!(
            matches!(listener.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "a group-writable directory's socket must never be connected to"
        );
        assert!(candidate.exists(), "and it must not be removed either");
        drop(listener);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// C4(b): a live listener whose accept queue is completely full must never hang the sweep. The
    /// old blocking `UnixStream::connect` sleeps until the peer's queue has room, which for a
    /// listener that never calls `accept` is forever. Filling the queue with non-blocking raw
    /// connects until one answers `EAGAIN`/`EWOULDBLOCK` reproduces exactly that state; the sweep is
    /// then run on its own thread so a hang shows up as the test's `recv_timeout` itself expiring,
    /// never as the whole `cargo test` process wedging. Linux only: macOS refuses a connect to a full
    /// queue at once (`ECONNREFUSED`), so it has no hang to reproduce and no `EAGAIN` to fill to.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_full_backlog_never_hangs_the_sweep() {
        let root = std::env::temp_dir().join(format!("nvb{}", std::process::id()));
        let candidate = root.join(format!("{BIND_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&candidate).expect("build the fixture");
        let sock_path = agent::socket_path::in_dir(&candidate, "s.sock").expect("under the cap");

        let listener_fd = raw_bind_listen(&sock_path, 0);
        let mut client_fds = Vec::new();
        loop {
            match raw_nonblocking_connect(&sock_path) {
                Ok(fd) => client_fds.push(fd),
                Err(errno) if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK => break,
                Err(errno) => panic!("unexpected connect() errno {errno}"),
            }
            assert!(
                client_fds.len() < 4096,
                "the accept queue never filled -- the fixture's own assumption failed"
            );
        }
        assert!(
            !client_fds.is_empty(),
            "listen(fd, 0) must still queue at least one connection"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let root_for_thread = root.clone();
        std::thread::spawn(move || {
            sweep_stale_instance_dirs(&root_for_thread, BIND_PREFIX, "s.sock", "test");
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the sweep must return even though the listener's accept queue never drains");

        assert!(
            candidate.exists(),
            "a full accept queue means a live listener; its directory must not be reclaimed"
        );

        for fd in client_fds {
            // SAFETY: each of these fds was returned by our own `socket()`/`connect()` above and
            // is closed exactly once, here.
            unsafe { libc::close(fd) };
        }
        // SAFETY: `listener_fd` was returned by our own `socket()`/`bind()`/`listen()` above.
        unsafe { libc::close(listener_fd) };
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `sockaddr_un` for `path`, or panics: every fixture path here is already checked under
    /// [`agent::socket_path::MAX_SOCKET_PATH_BYTES`] by `in_dir`, so this never has to answer "no
    /// listener" for a path that does not fit -- unlike the production probe, which does.
    #[cfg(target_os = "linux")]
    fn raw_sockaddr(path: &Path) -> libc::sockaddr_un {
        sockaddr_for(path).expect("fixture path too long for sockaddr_un")
    }

    /// A blocking listener at `path` with the given `backlog`, built with raw libc so the test can
    /// pass `backlog = 0` -- `std::os::unix::net::UnixListener::bind` has no way to ask for that.
    #[cfg(target_os = "linux")]
    fn raw_bind_listen(path: &Path, backlog: i32) -> std::os::raw::c_int {
        let addr = raw_sockaddr(path);
        // SAFETY: a fresh socket fd; `bind`/`listen` are given a valid, fully-initialized
        // `sockaddr_un` and its exact size, and `fd` is returned to the caller to own and close.
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket() failed: {}", std::io::Error::last_os_error());
            let rc = libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            );
            assert_eq!(rc, 0, "bind() failed: {}", std::io::Error::last_os_error());
            let rc = libc::listen(fd, backlog);
            assert_eq!(rc, 0, "listen() failed: {}", std::io::Error::last_os_error());
            fd
        }
    }

    /// One non-blocking `connect()` to `path`: `Ok(fd)` (kept open, owned by the caller) on success,
    /// `Err(errno)` otherwise -- closing the fd itself on any error, since the caller only wants a
    /// live one to hold the queue full.
    #[cfg(target_os = "linux")]
    fn raw_nonblocking_connect(path: &Path) -> Result<std::os::raw::c_int, i32> {
        let addr = raw_sockaddr(path);
        // SAFETY: a fresh non-blocking socket fd; `connect` is given a valid, fully-initialized
        // `sockaddr_un` and its exact size. The fd is closed here on every error path and otherwise
        // handed to the caller to own and close.
        unsafe {
            // Its own `fcntl`, not the probe's `nonblocking_unix_socket`: a fixture that shared the
            // code under test would hang in its own fill loop, not in the sweep, when that code broke.
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket() failed: {}", std::io::Error::last_os_error());
            let flags = libc::fcntl(fd, libc::F_GETFL);
            assert!(flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == 0);
            let rc = libc::connect(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            );
            if rc == 0 {
                Ok(fd)
            } else {
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
                libc::close(fd);
                Err(errno)
            }
        }
    }
}
