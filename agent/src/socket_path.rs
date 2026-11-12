// agent/src/socket_path.rs
//! Every Unix-socket path this crate binds or hands to a child to bind, built in one place so that
//! one length check covers all of them.
//!
//! **Why this exists (2026-09-17).** A `sockaddr_un`'s `sun_path` is 104 bytes on
//! macOS and 108 on Linux, NUL included, and Rust's std refuses a path of 104 bytes or more on
//! macOS with `InvalidInput: path must be shorter than SUN_LEN`. `std::env::temp_dir()` on macOS is
//! the per-user `/var/folders/<2>/<28>/T/` -- 49 bytes -- where on Linux it is usually `/tmp/`, 5.
//! The names this crate used before, `neovibe-agent-hook-<hyphenated uuid>.sock` and
//! `neovibe-claude-sidecar-<hyphenated uuid>.sock`, came to 109 and 113 bytes there, so the legacy
//! backend could not bind its hook socket at all (in every permission mode, since that bind is
//! unconditional) and the sidecar could not bind its own.
//!
//! The limit applied here is macOS's, [`MAX_SOCKET_PATH_BYTES`] = 103, on both platforms. That is
//! stricter than Linux needs (107) by four bytes; one number is simpler than a `cfg` whose Linux
//! arm nobody on the Mac can exercise, and no path this crate builds on Linux comes near either.
//!
//! **What a too-long path does now.** [`in_dir`] returns `InvalidInput` naming the path, its length
//! and the limit, before anything is bound or spawned. It does not fall back to a shorter directory:
//! the only short, always-present one is `/tmp`, which is world-writable where the macOS
//! `temp_dir()` is private to the user, and silently trading one for the other is a security
//! decision rather than a portability fix. A user whose `TMPDIR` is long enough to hit this gets a
//! clear error where they used to get std's.
//!
//! [`in_dir`] is not `#[doc(hidden)]` (removed 2026-09-17): it started out as test-
//! only plumbing -- `agent/tests/` binds through it too -- but `eitri-core`'s PRODUCT code
//! (`pane_switch`, `theme::feed`) now builds every socket path it has through this same function,
//! so hiding it from the docs would hide the one thing every Mac-bound socket path in this
//! workspace actually depends on. `tests::every_sock_path_in_this_crate_is_built_here` scans this
//! crate's own sources so a new construction site here fails a test rather than a Mac;
//! `eitri-core/src/socket_path_guard.rs` is the reciprocal scanner, over that crate's own
//! sources, added when L2 T5 put two construction sites there. The two are independent scanners
//! over two crates, not one shared mechanism -- keep them in step by hand.

use std::io;
use std::path::{Path, PathBuf};

use crate::private_fs::{is_private_dir, require_safe_parent};

/// The longest socket path, in bytes and without the trailing NUL, that binds on macOS.
pub const MAX_SOCKET_PATH_BYTES: usize = 103;

/// `dir/file_name`, or `InvalidInput` if that is longer than [`MAX_SOCKET_PATH_BYTES`].
pub fn in_dir(dir: &Path, file_name: &str) -> io::Result<PathBuf> {
    let path = dir.join(file_name);
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path {path:?} is {len} bytes, over the {MAX_SOCKET_PATH_BYTES}-byte limit a Unix \
                 socket path has on macOS -- a shorter TMPDIR avoids it"
            ),
        ));
    }
    Ok(path)
}

/// The legacy backend's per-conversation `PreToolUse` relay socket: `eitri-hook-<32 hex>.sock`,
/// 48 bytes of file name (97 under macOS's 49-byte `temp_dir()`). Only in a build with the legacy
/// backend (`legacy-backend`), its one user.
#[cfg(feature = "legacy-backend")]
pub(crate) fn hook_socket(dir: &Path, conversation_id: uuid::Uuid) -> io::Result<PathBuf> {
    in_dir(dir, &format!("{HOOK_SOCKET_PREFIX}{}.sock", conversation_id.simple()))
}

/// The file-name prefix [`hook_socket`] uses, for code that lists leftover sockets.
#[cfg(feature = "legacy-backend")]
pub(crate) const HOOK_SOCKET_PREFIX: &str = "eitri-hook-";

/// How many hex digits of an instance id the socket's file name keeps.
const SOCKET_ID_HEX: usize = 16;

/// One sidecar instance's gRPC socket: `sc-<id>.sock` inside the private directory
/// [`private_socket_dir`] returns. A hyphenated UUID -- which is what every caller in this
/// workspace passes -- is written as the first [`SOCKET_ID_HEX`] digits of its 32-hex simple form
/// (60 random bits of a v4 UUID, enough that two live sidecars of one user never meet), a 24-byte
/// file name; any other id is used verbatim and is subject to the check. The name is short because
/// the directory in front of it costs `eitri-<uid>/` bytes of the macOS cap, and a directory-service
/// uid there can have ten digits: `<49-byte TMPDIR>eitri-<10 digits>/sc-<16 hex>.sock` is 90 bytes.
pub(crate) fn sidecar_socket(dir: &Path, instance_id: &str) -> io::Result<PathBuf> {
    let id = match uuid::Uuid::parse_str(instance_id) {
        Ok(uuid) => uuid.simple().to_string()[..SOCKET_ID_HEX].to_owned(),
        Err(_) => instance_id.to_owned(),
    };
    in_dir(dir, &format!("sc-{id}.sock"))
}

/// The directory sockets that another process serves are bound in: `$XDG_RUNTIME_DIR/eitri` when
/// that is an absolute path to a real directory of this user that nobody else can write, else
/// `<tmp>/eitri-<uid>`. The directory is created 0700 when absent, and an existing one is accepted
/// only if it is a real directory (no symlink), owned by this user, with no group or other bit.
///
/// In the `<tmp>/eitri-<uid>` case the directory's parent matters as much as the directory: whoever
/// can rename entries in `<tmp>` can move a validated 0700 directory away and put their own, also
/// 0700 but theirs, in its place before the sidecar binds. So `<tmp>` itself must belong to this
/// user or to root and must not be writable by its group or by anyone, unless it has the sticky bit
/// (which stops everyone but an entry's owner and the directory's owner, both trusted by the first
/// rule); the same rule `packaging/install.sh` applies to the cache directories.
///
/// A socket directly in a shared `TMPDIR` is connectable by whoever the process umask lets in, and
/// the sidecar's protocol carries no credential, so the directory is what keeps other users away
/// from `ResolvePermission`. Anything short of that is an error, never a fallback to a shared place.
pub(crate) fn private_socket_dir(runtime_dir: Option<&Path>, tmp: &Path, uid: u32) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = match runtime_dir.filter(|d| d.is_absolute() && is_private_dir(d)) {
        Some(runtime) => runtime.join("eitri"),
        None => {
            require_safe_parent(tmp, "the sidecar's socket")?;
            tmp.join(format!("eitri-{uid}"))
        }
    };
    match std::fs::DirBuilder::new()
        .mode(crate::private_fs::PRIVATE_DIR_MODE)
        .create(&dir)
    {
        // Created here: the umask may have taken bits off the mode (0277 leaves 0500, which cannot be bound in).
        Ok(()) => crate::private_fs::make_dir_private(&dir)?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    if !is_private_dir(&dir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a real directory owned by this user with mode 0700; refusing to bind the sidecar's socket there",
                dir.display()
            ),
        ));
    }
    Ok(dir)
}

/// Whether the process on the other end of a freshly connected socket may be talked to: only this
/// user's own. Checked after `connect` and before a byte is sent.
pub(crate) fn require_own_peer(peer_uid: u32) -> io::Result<()> {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if peer_uid == euid {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("the sidecar socket is served by uid {peer_uid}, not this user ({euid}); refusing to use it"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tmp::TmpDir;

    /// What `std::env::temp_dir()` is on this project's Mac mini, byte for byte in length. Every
    /// user's `/var/folders/<2>/<28>/T/` has this shape, so this is the real worst case there
    /// rather than a guess.
    const MACOS_TEMP_DIR: &str = "/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/";

    /// A directory of its own, removed when the test ends however it ends.
    fn scratch(label: &str) -> TmpDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = TmpDir::new("nv-sock-dir", label);
        // 0755 whatever the umask: a directory a private one is made in must not be group-writable.
        std::fs::set_permissions(&*dir, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("chmod {dir:?}: {e}"));
        dir
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The sidecar's socket used to sit directly in the shared temp directory.
    #[test]
    fn the_socket_directory_is_created_private_and_is_not_the_temp_dir_itself() {
        let tmp = scratch("create");
        let dir = private_socket_dir(None, &tmp, 4242).unwrap();
        assert_eq!(dir, tmp.join("eitri-4242"));
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(private_socket_dir(None, &tmp, 4242).unwrap(), dir);
    }

    #[test]
    fn a_runtime_dir_of_this_user_is_preferred_and_a_loose_one_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = scratch("tmp");
        let runtime = scratch("runtime");
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            private_socket_dir(Some(&runtime), &tmp, 1).unwrap(),
            runtime.join("eitri")
        );
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            private_socket_dir(Some(&runtime), &tmp, 1).unwrap(),
            tmp.join("eitri-1")
        );
    }

    #[test]
    fn an_existing_directory_that_others_can_enter_or_a_symlink_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = scratch("loose");
        let loose = tmp.join("eitri-7");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = private_socket_dir(None, &tmp, 7).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");

        let target = scratch("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, tmp.join("eitri-8")).unwrap();
        let err = private_socket_dir(None, &tmp, 8).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }

    /// The umask is process-wide, so the body runs in a child copy of the test binary.
    #[test]
    fn a_restrictive_umask_does_not_leave_the_directory_unusable() {
        crate::test_umask::with_umask(
            concat!(
                module_path!(),
                "::a_restrictive_umask_does_not_leave_the_directory_unusable"
            ),
            0o277,
            || {
                let tmp = scratch("umask");
                let dir = private_socket_dir(None, &tmp, 9).unwrap();
                assert_eq!(mode(&dir), 0o700);
            },
        );
    }

    /// The directory the private one is made in decides whether another user can swap it.
    #[test]
    fn the_rule_for_the_directory_above_the_private_one() {
        let me = 1000;
        // (owner, mode, safe)
        for (owner, mode, safe) in [
            (me, 0o755, true),
            (me, 0o700, true),
            (0, 0o755, true),
            (0, 0o1777, true),
            (me, 0o1777, true),
            (me, 0o775, false),
            (me, 0o757, false),
            (me, 0o777, false),
            (0, 0o777, false),
            (0, 0o775, false),
            (me + 1, 0o755, false),
            (me + 1, 0o1777, false),
        ] {
            assert_eq!(
                crate::private_fs::parent_is_safe(owner, mode, me),
                safe,
                "owner {owner} mode {mode:o}"
            );
        }
    }

    #[test]
    fn a_shared_non_sticky_temp_dir_is_refused_and_a_sticky_one_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = scratch("shared");
        for open_mode in [0o777, 0o775, 0o757] {
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(open_mode)).unwrap();
            let err = private_socket_dir(None, &tmp, 3).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{open_mode:o}: {err}");
            assert!(
                !tmp.join("eitri-3").exists(),
                "nothing is created under an unsafe parent"
            );
        }
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert_eq!(private_socket_dir(None, &tmp, 3).unwrap(), tmp.join("eitri-3"));
    }

    #[test]
    fn a_temp_dir_that_is_a_symlink_to_a_safe_directory_is_accepted() {
        let real = scratch("real");
        let link_dir = scratch("link");
        let link = link_dir.join("tmp");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(private_socket_dir(None, &link, 5).unwrap(), link.join("eitri-5"));
    }

    #[test]
    fn only_the_own_uid_may_serve_the_socket() {
        let own = unsafe { libc::geteuid() };
        require_own_peer(own).unwrap();
        let err = require_own_peer(own.wrapping_add(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn the_macos_temp_dir_fixture_is_the_real_length() {
        assert_eq!(MACOS_TEMP_DIR.len(), 49);
    }

    #[test]
    fn a_path_of_exactly_the_limit_is_accepted_and_one_byte_more_is_refused() {
        let dir = Path::new("/");
        let fits = "a".repeat(MAX_SOCKET_PATH_BYTES - 1);
        assert_eq!(in_dir(dir, &fits).unwrap().as_os_str().len(), MAX_SOCKET_PATH_BYTES);

        let too_long = "a".repeat(MAX_SOCKET_PATH_BYTES);
        let err = in_dir(dir, &too_long).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("104 bytes"),
            "the error should name the length: {err}"
        );
    }

    /// The limit is the real one, not a number copied from a comment: a path of exactly
    /// `MAX_SOCKET_PATH_BYTES` binds, on this platform, and on macOS one byte more does not.
    #[test]
    fn the_limit_matches_what_bind_really_accepts() {
        let dir = std::env::temp_dir();
        // `join("")` is `dir` with exactly one trailing separator, whichever way `dir` was spelled.
        let name_len = MAX_SOCKET_PATH_BYTES - dir.join("").as_os_str().len();
        let stem = format!("nvsp-{}", uuid::Uuid::new_v4().simple());
        let name = format!("{stem}{}", "x".repeat(name_len - stem.len()));
        let path = in_dir(&dir, &name).unwrap();
        assert_eq!(path.as_os_str().len(), MAX_SOCKET_PATH_BYTES);
        // The socket file sits directly in the shared temp dir (the length is the point), so it is
        // removed on every way out.
        struct Unlink<'a>(&'a Path);
        impl Drop for Unlink<'_> {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0);
            }
        }
        let _unlink = Unlink(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("a 103-byte path must bind");
        drop(listener);

        #[cfg(target_os = "macos")]
        {
            let one_more = dir.join(format!("{name}y"));
            assert_eq!(one_more.as_os_str().len(), MAX_SOCKET_PATH_BYTES + 1);
            assert!(
                std::os::unix::net::UnixListener::bind(&one_more).is_err(),
                "104 bytes must not bind on macOS"
            );
            let _ = std::fs::remove_file(&one_more);
        }
    }

    /// The sidecar's socket fits under the macOS temp dir and under this machine's own, with the
    /// longest uid a directory service hands out (ten digits).
    #[test]
    fn the_sidecar_socket_fits_under_the_macos_temp_dir_with_a_ten_digit_uid() {
        for dir in [Path::new(MACOS_TEMP_DIR).to_path_buf(), std::env::temp_dir()] {
            let dir = dir.join(format!("eitri-{}", u32::MAX));
            assert_eq!(u32::MAX.to_string().len(), 10);
            let path = sidecar_socket(&dir, &uuid::Uuid::new_v4().to_string()).unwrap();
            assert!(path.as_os_str().len() <= MAX_SOCKET_PATH_BYTES, "{path:?}");
        }
        // 49 (temp dir) + "eitri-" + 10 digits + "/" + 24 (file name): the figure the notes quote.
        let macos = Path::new(MACOS_TEMP_DIR).join(format!("eitri-{}", u32::MAX));
        let path = sidecar_socket(&macos, &uuid::Uuid::new_v4().to_string()).unwrap();
        assert_eq!(path.as_os_str().len(), 49 + 6 + 10 + 1 + 24);
    }

    /// The legacy hook socket sits directly in the temp dir (never in the private directory), so it
    /// is checked there.
    #[cfg(feature = "legacy-backend")]
    #[test]
    fn the_hook_socket_fits_under_the_macos_temp_dir() {
        for dir in [Path::new(MACOS_TEMP_DIR).to_path_buf(), std::env::temp_dir()] {
            let path = hook_socket(&dir, uuid::Uuid::new_v4()).unwrap();
            assert!(path.as_os_str().len() <= MAX_SOCKET_PATH_BYTES, "{path:?}");
        }
    }

    #[test]
    fn a_hyphenated_uuid_instance_id_is_written_as_sixteen_hex_digits() {
        let id = uuid::Uuid::new_v4();
        let path = sidecar_socket(Path::new("/t"), &id.to_string()).unwrap();
        assert_eq!(
            path,
            Path::new(&format!("/t/sc-{}.sock", &id.simple().to_string()[..16]))
        );
        assert_eq!(path.file_name().unwrap().len(), 24);
        assert_eq!(
            sidecar_socket(Path::new("/t"), "abc").unwrap(),
            Path::new("/t/sc-abc.sock")
        );
    }

    /// The connector every `ClaudeSidecarProvider` connects through (`LocalChild::channel`: a sidecar it
    /// started, or one taken over from another window) checks who serves the socket before any byte
    /// goes over it. The check cannot be exercised against another uid without root, so this reads the
    /// production source: `require_own_peer` must be called there, after the `UnixStream::connect` and
    /// before the stream is handed to the channel.
    #[test]
    fn the_sidecar_connector_checks_the_peer_before_using_the_stream() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(root.join("src/providers/claude_sidecar/transport/local.rs")).unwrap();
        let start = text
            .find("fn channel(")
            .expect("anchor: the sidecar provider's connector");
        let body = &text[start..];
        let end = body.find("\n    }\n").expect("the end of LocalChild::channel");
        let body = &body[..end];
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let connected = code
            .find("UnixStream::connect(")
            .expect("the connector opens the stream");
        let checked = code
            .find("require_own_peer(")
            .expect("connect must call require_own_peer");
        let handed = code
            .find("TokioIo::new(stream)")
            .expect("the stream is handed to the channel");
        assert!(connected < checked && checked < handed, "check order in connect");
    }

    /// The assertion over every construction site. Any line in `agent/src` or `agent/tests` that
    /// mentions `.sock` outside a comment has to be in this file, passed to `in_dir` (on that line or
    /// as an argument of the call a line or two above it), or on the list below, and
    /// every entry on the list says why that path is never bound. A new socket path built anywhere
    /// else fails here, on Linux, before it fails `bind` on a Mac.
    ///
    /// Scoped to this crate only. `eitri-core/src/socket_path_guard.rs` is the same scan over
    /// that crate's sources instead -- a separate scanner, not a shared one, so keep both in step
    /// by hand when either crate's set of socket-building files changes.
    #[test]
    fn every_sock_path_in_this_crate_is_built_here() {
        // (file, a substring of the line) -- each one a path that is never bound as written (or, for the
        // last two, is bound only under a scratch root).
        const NEVER_BOUND: &[(&str, &str)] = &[
            // Only formats the hook's argv JSON; asserts on the string.
            ("src/settings.rs", "\"/tmp/eitri-agent-hook-abc123.sock\""),
            ("src/settings.rs", "dir.join(\"s.sock\")"),
            ("src/settings.rs", "\"/tmp/a.sock\""),
            ("src/settings.rs", "\"/tmp/b.sock\""),
            // The editor tool server's argv as an ACP session/new request carries it; the strings are
            // compared against the request JSON and nothing binds them.
            (
                "tests/acp_provider.rs",
                "args: vec![\"/run/user/1000/eitri/bridge.sock\".to_string()",
            ),
            (
                "tests/acp_provider.rs",
                "\"args\": [\"/run/user/1000/eitri/bridge.sock\"",
            ),
            // Probes that the agent-hook binary exists; never bound.
            ("src/process.rs", "dir.join(\"probe.sock\")"),
            // Absolute paths of a host that the sandbox self-check's tests re-create inside a scratch
            // root: each is bound at a short name in that root and renamed to the long one.
            ("src/testkit/claude_scripted/sandbox.rs", "\"/run/docker.sock\""),
            (
                "src/testkit/claude_scripted/sandbox.rs",
                "\"/var/lib/gssproxy/default.sock\"",
            ),
        ];

        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offenders = Vec::new();
        for sub in ["src", "tests"] {
            visit(&root.join(sub), &mut |file| {
                let rel = file.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                if rel == "src/socket_path.rs" {
                    return;
                }
                let text = std::fs::read_to_string(file).unwrap();
                let lines: Vec<&str> = text.lines().collect();
                for (n, line) in lines.iter().enumerate() {
                    let code = line.trim_start();
                    // A name that reaches `in_dir` is checked by it, however rustfmt broke the call up.
                    if code.starts_with("//") || !mentions_a_sock_file(code) || routed_through_in_dir(&lines, n) {
                        continue;
                    }
                    if NEVER_BOUND.iter().any(|(f, s)| *f == rel && code.contains(s)) {
                        continue;
                    }
                    offenders.push(format!("{rel}:{}: {code}", n + 1));
                }
            });
        }
        assert!(
            offenders.is_empty(),
            "socket paths built outside agent::socket_path escape its length check:\n{}",
            offenders.join("\n")
        );
    }

    /// `.sock` as a file extension, not as the start of an identifier like `.socket_path`.
    /// Whether the `.sock` name on line `n` is an argument of a `socket_path::in_dir(` call.
    ///
    /// The check used to be "the same line mentions `in_dir(`", which was true of every call site
    /// until `rustfmt.toml` landed (2026-09-19) and rustfmt wrapped the longer ones onto their own
    /// argument lines -- six real, correctly routed sites became offenders at once. The invariant
    /// was never about one line: it is that the name reaches `in_dir`, which owns the length cap.
    /// So walk back over the open argument list instead, stopping at the statement boundary a `;`
    /// or a blank line marks, and never further than a short window.
    fn routed_through_in_dir(lines: &[&str], n: usize) -> bool {
        for line in lines[n.saturating_sub(4)..=n].iter().rev() {
            let code = line.trim();
            if code.contains("socket_path::in_dir(") || code.contains("in_dir(") && code.contains("socket_path") {
                return true;
            }
            if code.is_empty() || code.ends_with(';') {
                return false;
            }
        }
        false
    }

    fn mentions_a_sock_file(code: &str) -> bool {
        code.match_indices(".sock").any(|(i, m)| {
            !code[i + m.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    }

    fn visit(dir: &Path, f: &mut dyn FnMut(&Path)) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, f);
            } else if path.extension().is_some_and(|e| e == "rs") {
                f(&path);
            }
        }
    }
}
