use std::ffi::{OsStr, OsString};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long one connect to a Wayland socket may take before the socket counts as dead. A listening
/// socket answers a connect at once; only a full accept backlog makes it wait, and that is a live one.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// What `main` does about the display variables before any thread or GTK code exists.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The environment already names a display that works, or there is nothing here to decide.
    Keep,
    /// Set `WAYLAND_DISPLAY` to `name`; `notice` is the one line to print.
    Use { name: String, notice: String },
    /// Remove `WAYLAND_DISPLAY`, so GTK and the editor's winit both take X11; `notice` is the one line to print.
    DropWayland { notice: String },
}

/// Decides the Wayland display this process and everything it starts will use.
///
/// A `WAYLAND_DISPLAY` that names a socket with a compositor behind it is never changed. When it is unset or
/// names a socket nothing listens on (a closed session leaves such a file; a tmux server older than the desktop
/// session keeps the old name), the live `wayland-*` sockets of `XDG_RUNTIME_DIR` decide: exactly one is used;
/// several are ranked newest socket first (the compositor that started last is the current session's),
/// then by the higher number; none means Wayland is left out, so GTK (which falls back to X11 when it cannot
/// connect) and winit (X11 when `WAYLAND_DISPLAY` is unset and `DISPLAY` is set) agree. `WAYLAND_SOCKET`
/// (an already-connected descriptor) and a forwarded `DISPLAY` (`ssh -X`) or `GDK_BACKEND=x11` with no Wayland named are
/// left alone; with `GDK_BACKEND=x11` a dead name is dropped without looking for another socket, because the window is
/// on X11 and the editor must follow it.
pub fn resolve_wayland_display(env: impl Fn(&str) -> Option<OsString>) -> Resolution {
    let present = |key: &str| env(key).filter(|value| !value.is_empty());
    if present("WAYLAND_SOCKET").is_some() {
        return Resolution::Keep;
    }
    let Some(runtime_dir) = present("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
    else {
        return Resolution::Keep;
    };
    let named = present("WAYLAND_DISPLAY");
    let x_display = present("DISPLAY");
    // `GDK_BACKEND=x11` puts the window on X11 whatever `WAYLAND_DISPLAY` says, so the editor must not go to Wayland.
    let window_on_x11 = present("GDK_BACKEND").is_some_and(|list| {
        list.as_encoded_bytes()
            .split(|byte| *byte == b',')
            .map(|item| item.trim_ascii())
            .find(|item| !item.is_empty())
            == Some(b"x11".as_slice())
    });

    if let Some(name) = &named {
        if socket_is_live(&runtime_dir.join(name), PROBE_TIMEOUT) {
            return Resolution::Keep;
        }
    } else if window_on_x11
        || x_display
            .as_deref()
            .is_some_and(|d| !d.as_encoded_bytes().starts_with(b":"))
    {
        return Resolution::Keep;
    }

    let live = if window_on_x11 {
        Vec::new()
    } else {
        live_wayland_sockets(&runtime_dir, named.as_deref())
    };
    let before = match &named {
        Some(name) => format!("WAYLAND_DISPLAY={} has no compositor behind it", name.to_string_lossy()),
        None => "WAYLAND_DISPLAY was unset".to_string(),
    };
    match live.as_slice() {
        [] => match named {
            // Unset and nothing live: GTK's own default (wayland-0) fails and falls back to X11, as winit does.
            None => Resolution::Keep,
            Some(_) => Resolution::DropWayland {
                notice: format!(
                    "eitri: {before} and {}; leaving Wayland out, {}",
                    if window_on_x11 {
                        "GDK_BACKEND=x11 keeps the window on X11".to_string()
                    } else {
                        format!("no other Wayland socket in {} is live", runtime_dir.display())
                    },
                    match &x_display {
                        Some(display) => format!(
                            "so the window and the editor both use X11 on DISPLAY={}",
                            display.to_string_lossy()
                        ),
                        None => "and DISPLAY is not set either, so there is no display to open".to_string(),
                    }
                ),
            },
        },
        [only] => Resolution::Use {
            name: only.clone(),
            notice: format!(
                "eitri: {before}; using {only} (the only live Wayland socket in {})",
                runtime_dir.display()
            ),
        },
        [first, ..] => Resolution::Use {
            name: first.clone(),
            notice: format!(
                "eitri: {before}; using {first}, the newest of the live Wayland sockets ({})",
                live.join(", ")
            ),
        },
    }
}

/// The names of the `wayland-*` sockets in `runtime_dir` with a compositor behind them, best first (newest
/// socket, then the higher number). `.lock` files and anything that is not a socket are skipped, and
/// `skip` (the dead name from the environment) is not probed again.
fn live_wayland_sockets(runtime_dir: &Path, skip: Option<&OsStr>) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut found: Vec<(SystemTime, u64, String)> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else { continue };
        let Some(suffix) = name.strip_prefix("wayland-") else {
            continue;
        };
        if name.ends_with(".lock") || skip == Some(file_name.as_os_str()) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if !meta.file_type().is_socket() || !socket_is_live(&entry.path(), PROBE_TIMEOUT) {
            continue;
        }
        let number = suffix.parse::<u64>().unwrap_or(0);
        found.push((
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            number,
            name.to_string(),
        ));
    }
    found.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));
    found.into_iter().map(|(_, _, name)| name).collect()
}

/// Whether something listens on the unix socket at `path`: a connect that is accepted, or that only waits
/// because the listener's backlog is full, is live; a missing file, a file nothing listens on and a timeout
/// are not. The connection is closed without sending anything, and the call never blocks past `timeout`.
pub fn socket_is_live(path: &Path, timeout: Duration) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if !std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_socket()) {
        return false;
    }
    // SAFETY: plain libc calls on a descriptor owned here and closed on every path; `addr` is a zeroed,
    // fully initialised sockaddr_un whose path is a NUL-terminated copy that fits.
    unsafe {
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
            return false;
        }
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        // Not `SOCK_CLOEXEC | SOCK_NONBLOCK` in the call: macOS has neither. This runs before any thread exists.
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return false;
        }
        if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0
            || libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) < 0
        {
            libc::close(fd);
            return false;
        }
        let rc = libc::connect(
            fd,
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        let live = if rc == 0 {
            true
        } else {
            match std::io::Error::last_os_error().raw_os_error() {
                // A full backlog: the socket is listening, just busy.
                Some(libc::EAGAIN) => true,
                Some(libc::EINPROGRESS) => {
                    let mut poll = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let ready = libc::poll(&mut poll, 1, timeout.as_millis().min(i32::MAX as u128) as i32);
                    let mut error: libc::c_int = 0;
                    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                    ready > 0
                        && libc::getsockopt(
                            fd,
                            libc::SOL_SOCKET,
                            libc::SO_ERROR,
                            (&mut error as *mut libc::c_int).cast(),
                            &mut len,
                        ) == 0
                        && error == 0
                }
                _ => false,
            }
        };
        libc::close(fd);
        live
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_wayland_display, socket_is_live, Resolution, PROBE_TIMEOUT};
    use crate::test_scratch_dir::ScratchDir;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    /// Every test runs against its own runtime directory, and the environment is a closure, so no real
    /// display, socket or variable of the machine running the test is read.
    fn scratch(label: &str) -> ScratchDir {
        ScratchDir::new("display-env", label)
    }

    fn live(dir: &Path, name: &str) -> UnixListener {
        UnixListener::bind(dir.join(name)).unwrap()
    }

    /// A socket file nothing listens on, as a closed desktop session leaves behind.
    fn dead(dir: &Path, name: &str) {
        drop(UnixListener::bind(dir.join(name)).unwrap());
        assert!(dir.join(name).exists());
    }

    fn touch(dir: &Path, name: &str, seconds_since_epoch: i64) {
        let path = std::ffi::CString::new(dir.join(name).as_os_str().as_bytes()).unwrap();
        let times = [libc::timespec {
            tv_sec: seconds_since_epoch,
            tv_nsec: 0,
        }; 2];
        // SAFETY: a valid NUL-terminated path and a two-element timespec array.
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) },
            0
        );
    }

    fn resolve(dir: &Path, vars: &[(&str, &str)]) -> Resolution {
        let dir = dir.as_os_str().to_owned();
        resolve_wayland_display(|key| {
            if key == "XDG_RUNTIME_DIR" {
                return Some(dir.clone());
            }
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(*value))
        })
    }

    fn uses(resolution: &Resolution) -> (&str, &str) {
        match resolution {
            Resolution::Use { name, notice } => (name, notice),
            other => panic!("expected a socket to be chosen, got {other:?}"),
        }
    }

    #[test]
    fn a_dead_named_socket_gives_way_to_the_one_live_socket() {
        let dir = scratch("dead-named");
        dead(&dir, "wayland-0");
        let _hyprland = live(&dir, "wayland-1");
        let resolution = resolve(&dir, &[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]);
        let (name, notice) = uses(&resolution);
        assert_eq!(name, "wayland-1");
        assert!(
            notice.contains("WAYLAND_DISPLAY=wayland-0 has no compositor behind it"),
            "{notice}"
        );
        assert!(notice.contains("using wayland-1"), "{notice}");
        assert!(!notice.contains('\n'));
    }

    #[test]
    fn an_unset_display_with_a_dead_default_finds_the_live_socket() {
        let dir = scratch("unset");
        dead(&dir, "wayland-0");
        let _hyprland = live(&dir, "wayland-1");
        for vars in [&[][..], &[("DISPLAY", ":0")][..], &[("WAYLAND_DISPLAY", "")][..]] {
            let resolution = resolve(&dir, vars);
            let (name, notice) = uses(&resolution);
            assert_eq!(name, "wayland-1", "{vars:?}");
            assert!(
                notice.contains("WAYLAND_DISPLAY was unset; using wayland-1"),
                "{notice}"
            );
        }
    }

    #[test]
    fn with_no_live_socket_wayland_is_left_out_and_the_notice_says_which_display_is_used() {
        let dir = scratch("none");
        dead(&dir, "wayland-0");
        dead(&dir, "wayland-1");
        // Neither a lock file nor a plain file named like a socket is a compositor.
        std::fs::write(dir.join("wayland-1.lock"), "").unwrap();
        std::fs::write(dir.join("wayland-2"), "").unwrap();

        let with_x = resolve(&dir, &[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":1")]);
        let Resolution::DropWayland { notice } = &with_x else {
            panic!("{with_x:?}")
        };
        assert!(notice.contains("no other Wayland socket"), "{notice}");
        assert!(notice.contains("X11 on DISPLAY=:1"), "{notice}");

        let without_x = resolve(&dir, &[("WAYLAND_DISPLAY", "wayland-0")]);
        let Resolution::DropWayland { notice } = &without_x else {
            panic!("{without_x:?}")
        };
        assert!(notice.contains("DISPLAY is not set either"), "{notice}");

        // Named after a socket that does not exist at all.
        let missing = resolve(&dir, &[("WAYLAND_DISPLAY", "wayland-9"), ("DISPLAY", ":0")]);
        assert!(matches!(missing, Resolution::DropWayland { .. }), "{missing:?}");

        // Unset: GTK and winit already agree on X11, so there is nothing to change or say.
        assert_eq!(resolve(&dir, &[("DISPLAY", ":0")]), Resolution::Keep);
        assert_eq!(resolve(&dir, &[]), Resolution::Keep);
    }

    #[test]
    fn of_several_live_sockets_the_newest_wins_and_a_tie_goes_to_the_higher_number() {
        let dir = scratch("several");
        dead(&dir, "wayland-0");
        let _a = live(&dir, "wayland-1");
        let _b = live(&dir, "wayland-2");
        let _c = live(&dir, "wayland-5");
        touch(&dir, "wayland-1", 3_000);
        touch(&dir, "wayland-2", 9_000);
        touch(&dir, "wayland-5", 1_000);
        let resolution = resolve(&dir, &[("WAYLAND_DISPLAY", "wayland-0")]);
        let (name, notice) = uses(&resolution);
        assert_eq!(name, "wayland-2", "the newest, not the highest number");
        assert!(
            notice.contains("the newest of the live Wayland sockets (wayland-2, wayland-1, wayland-5)"),
            "{notice}"
        );

        touch(&dir, "wayland-1", 9_000);
        touch(&dir, "wayland-5", 9_000);
        let resolution = resolve(&dir, &[]);
        assert_eq!(uses(&resolution).0, "wayland-5");
    }

    #[test]
    fn a_live_named_socket_is_never_changed() {
        let dir = scratch("kept");
        let _old = live(&dir, "wayland-0");
        let _newer = live(&dir, "wayland-1");
        touch(&dir, "wayland-0", 1_000);
        touch(&dir, "wayland-1", 9_000);
        assert_eq!(resolve(&dir, &[("WAYLAND_DISPLAY", "wayland-0")]), Resolution::Keep);
        assert_eq!(
            resolve(&dir, &[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]),
            Resolution::Keep
        );
        // An absolute name is a path of its own.
        let absolute = dir.join("wayland-1");
        assert_eq!(
            resolve(&dir, &[("WAYLAND_DISPLAY", absolute.to_str().unwrap())]),
            Resolution::Keep
        );
    }

    #[test]
    fn what_is_not_ours_to_decide_is_left_alone() {
        let dir = scratch("alone");
        dead(&dir, "wayland-0");
        let _live = live(&dir, "wayland-1");
        // An inherited, already-connected descriptor beats any name.
        assert_eq!(
            resolve(&dir, &[("WAYLAND_SOCKET", "3"), ("WAYLAND_DISPLAY", "wayland-0")]),
            Resolution::Keep
        );
        // A forwarded X display (`ssh -X`) with no Wayland named is the display the user chose.
        assert_eq!(resolve(&dir, &[("DISPLAY", "localhost:10.0")]), Resolution::Keep);
        // No usable runtime directory, nothing to look in.
        for runtime in [None, Some(""), Some("relative")] {
            let resolution = resolve_wayland_display(|key| match key {
                "XDG_RUNTIME_DIR" => runtime.map(OsString::from),
                "WAYLAND_DISPLAY" => Some(OsString::from("wayland-0")),
                _ => None,
            });
            assert_eq!(resolution, Resolution::Keep, "{runtime:?}");
        }
    }

    #[test]
    fn a_window_forced_onto_x11_keeps_the_editor_off_wayland() {
        let dir = scratch("gdk-x11");
        dead(&dir, "wayland-0");
        let _nested = live(&dir, "wayland-1");
        // Unset: nothing to do, the editor takes X11 as the window does.
        assert_eq!(
            resolve(&dir, &[("GDK_BACKEND", "x11"), ("DISPLAY", ":0")]),
            Resolution::Keep
        );
        assert_eq!(
            resolve(&dir, &[("GDK_BACKEND", "x11,wayland"), ("DISPLAY", ":0")]),
            Resolution::Keep
        );
        // A dead name is dropped even though another socket is live: the window is not on Wayland.
        let resolution = resolve(
            &dir,
            &[
                ("GDK_BACKEND", "x11"),
                ("WAYLAND_DISPLAY", "wayland-0"),
                ("DISPLAY", ":0"),
            ],
        );
        let Resolution::DropWayland { notice } = &resolution else {
            panic!("{resolution:?}")
        };
        assert!(notice.contains("GDK_BACKEND=x11 keeps the window on X11"), "{notice}");
        // A backend list that starts with Wayland does not.
        let resolution = resolve(&dir, &[("GDK_BACKEND", "wayland,x11"), ("DISPLAY", ":0")]);
        assert_eq!(uses(&resolution).0, "wayland-1");
    }

    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "macOS refuses a connect to a full queue at once (ECONNREFUSED), so there a full backlog reads as \
                  no listener; there is no Wayland socket to probe on the Mac"
    )]
    fn the_probe_tells_a_listener_from_a_leftover_file_and_never_waits_on_a_full_backlog() {
        let dir = scratch("probe");
        let listener = live(&dir, "wayland-1");
        dead(&dir, "wayland-0");
        std::fs::write(dir.join("plain"), "").unwrap();
        assert!(socket_is_live(&dir.join("wayland-1"), PROBE_TIMEOUT));
        assert!(!socket_is_live(&dir.join("wayland-0"), PROBE_TIMEOUT));
        assert!(!socket_is_live(&dir.join("plain"), PROBE_TIMEOUT));
        assert!(!socket_is_live(&dir.join("absent"), PROBE_TIMEOUT));
        // Nobody accepts, so the queue fills up and a blocking connect would hang here; the probe's
        // connect is non-blocking and a full queue still means "listening".
        for round in 0..600 {
            assert!(socket_is_live(&dir.join("wayland-1"), PROBE_TIMEOUT), "round {round}");
        }
        drop(listener);
        assert!(!socket_is_live(&dir.join("wayland-1"), PROBE_TIMEOUT));
    }
}
