//! Private per-instance directories under `TMPDIR`, and the sweep that reclaims ones whose process
//! died without cleaning up. Each window needs a unique directory holding a socket; a SIGKILL'd
//! `shell` can leave that directory behind.
//!
//! The sweep uses `agent::process_probe::pid_is_alive` rather than `/proc/<pid>`, since macOS has
//! no `/proc`. See [`pid_is_running`] for how it handles permissions and rejects invalid pids.
//!
//! The sweep must never block startup. A group-writable directory (`0775` under umask 002) lets
//! another group member replace `socket_name` with a listener whose accept queue is full.
//! A blocking `UnixStream::connect` would then wait for room before this window could open.
//! [`sweep_entries_owned_by`] skips entries belonging to another user and directories with any
//! group- or other-write bit (`mode & 0o022 != 0`), without connecting or removing them.
//! [`socket_has_a_listener`] also uses a non-blocking `AF_UNIX` `connect()` so a probe cannot hang.
//! `EAGAIN`, `EWOULDBLOCK` and `EINPROGRESS` count as a listener: its queue is full or the connection
//! is still completing. Other errors count as no listener.

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
/// Removal requires a matching name, a real directory rather than a symlink, ownership by this
/// user, no group- or other-write bit, a dead pid (`agent::process_probe::pid_is_alive`), and no
/// listener answering a non-blocking `connect()` on `socket_name`.
///
/// Type, ownership and mode are checked before probing. Other users' entries and writable
/// directories are left untouched because another local account could replace the socket between
/// the check and the connect. `0755` is accepted along with `0700`: only this user can rename its
/// entries, and accepting it lets the sweep reclaim directories created by older builds.
/// The socket is probed only if it is a real socket owned by this user; otherwise it counts as
/// no listener.
///
/// Both liveness checks are needed. `TMPDIR` may be shared across pid namespaces, so a live
/// instance can look pid-dead from here while its listener still answers. Conversely, creation
/// can fail before `bind`, leaving a directory without a socket.
///
/// The socket probe runs only when the pid looks dead. It connects and closes immediately.
/// A listener with queue space sees an empty line, which its parser rejects and logs. A full
/// queue does not delay the probe.
///
/// Every failure is ignored so an inaccessible entry in a shared `TMPDIR` cannot stop startup.
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
/// socket fd is closed before returning in every case.
fn nonblocking_connect_finds_a_listener(path: &Path) -> bool {
    let Some(addr) = sockaddr_for(path) else { return false };
    // SAFETY: fixed, valid arguments; the returned fd is checked before use and closed below in
    // every code path before this function returns.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return false;
    }
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

/// `path` as a `sockaddr_un`, or `None` if it does not fit `sun_path` (with room left for the
/// trailing NUL `zeroed()` already provides). The socket paths this crate itself builds are already
/// checked by `agent::socket_path::in_dir`; a stray, over-long entry someone else left in `TMPDIR`
/// answers "no listener" here rather than panicking or silently truncating.
fn sockaddr_for(path: &Path) -> Option<libc::sockaddr_un> {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return None;
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    Some(addr)
}

/// Checks process existence through `agent::process_probe::pid_is_alive` using `kill(pid, 0)`.
/// Signal 0 delivers no signal. `EPERM` counts as alive, so lack of signal permission does not
/// make a live process appear dead. Pid 0 and values above `i32::MAX` are rejected before the
/// syscall to avoid process-group semantics or a wrapped negative pid.
///
/// On a default `/proc` mount this agrees with `/proc/<pid>` for zombies and other users'
/// processes. Under `hidepid=2`, another user's live process is hidden from `/proc`, but `kill`
/// still reports it alive through `EPERM`. This prevents reclaiming a live instance's directory.
/// The same check also works on macOS, which has no `/proc`.
fn pid_is_running(pid: u32) -> bool {
    agent::process_probe::pid_is_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
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

    /// An entry belonging to another user is skipped before connecting, so its full listener queue
    /// cannot hang startup. The sweep receives a different uid to simulate a directory belonging to
    /// another user. With the real uid, the same fixture is probed.
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

    /// A directory owned by this user but writable by the group or others must not be connected to.
    /// Another account could replace its socket with a listener whose queue is full between the
    /// ownership check and the connect. `0755` remains acceptable because only this user can rename
    /// its entries; requiring exactly `0700` would leave stale directories from older builds behind.
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
    /// never as the whole `cargo test` process wedging.
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
    fn raw_sockaddr(path: &Path) -> libc::sockaddr_un {
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.as_os_str().as_bytes();
        assert!(
            bytes.len() < addr.sun_path.len(),
            "fixture path too long for sockaddr_un"
        );
        for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = *src as libc::c_char;
        }
        addr
    }

    /// A blocking listener at `path` with the given `backlog`, built with raw libc so the test can
    /// pass `backlog = 0` -- `std::os::unix::net::UnixListener::bind` has no way to ask for that.
    fn raw_bind_listen(path: &Path, backlog: i32) -> std::os::raw::c_int {
        let addr = raw_sockaddr(path);
        // SAFETY: a fresh socket fd; `bind`/`listen` are given a valid, fully-initialized
        // `sockaddr_un` and its exact size, and `fd` is returned to the caller to own and close.
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
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
    fn raw_nonblocking_connect(path: &Path) -> Result<std::os::raw::c_int, i32> {
        let addr = raw_sockaddr(path);
        // SAFETY: a fresh non-blocking socket fd; `connect` is given a valid, fully-initialized
        // `sockaddr_un` and its exact size. The fd is closed here on every error path and otherwise
        // handed to the caller to own and close.
        unsafe {
            let fd = libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            );
            assert!(fd >= 0, "socket() failed: {}", std::io::Error::last_os_error());
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
