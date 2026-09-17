// agent/src/socket_path.rs
//! Every Unix-socket path this crate binds or hands to a child to bind, built in one place so that
//! one length check covers all of them.
//!
//! **Why this exists (macOS track M1, 2026-09-17).** A `sockaddr_un`'s `sun_path` is 104 bytes on
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
//! [`in_dir`] is not `#[doc(hidden)]` (removed 2026-09-17, L2 follow-up): it started out as test-
//! only plumbing -- `agent/tests/` binds through it too -- but `neovibe-core`'s PRODUCT code
//! (`pane_switch`, `theme::feed`) now builds every socket path it has through this same function,
//! so hiding it from the docs would hide the one thing every Mac-bound socket path in this
//! workspace actually depends on. `tests::every_sock_path_in_this_crate_is_built_here` scans this
//! crate's own sources so a new construction site here fails a test rather than a Mac;
//! `neovibe-core/src/socket_path_guard.rs` is the reciprocal scanner, over that crate's own
//! sources, added when L2 T5 put two construction sites there. The two are independent scanners
//! over two crates, not one shared mechanism -- keep them in step by hand.

use std::io;
use std::path::{Path, PathBuf};

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

/// The legacy backend's per-conversation `PreToolUse` relay socket: `neovibe-hook-<32 hex>.sock`,
/// 50 bytes of file name (99 under macOS's 49-byte `temp_dir()`).
pub(crate) fn hook_socket(dir: &Path, conversation_id: uuid::Uuid) -> io::Result<PathBuf> {
    in_dir(dir, &format!("{HOOK_SOCKET_PREFIX}{}.sock", conversation_id.simple()))
}

/// The file-name prefix [`hook_socket`] uses, for code that lists leftover sockets.
pub(crate) const HOOK_SOCKET_PREFIX: &str = "neovibe-hook-";

/// One sidecar instance's gRPC socket: `neovibe-sc-<id>.sock`. A hyphenated UUID -- which is what
/// every caller in this workspace passes -- is written in its 32-hex simple form, 48 bytes of file
/// name (97 under macOS's `temp_dir()`); any other id is used verbatim and is subject to the check.
pub(crate) fn sidecar_socket(dir: &Path, instance_id: &str) -> io::Result<PathBuf> {
    let id = match uuid::Uuid::parse_str(instance_id) {
        Ok(uuid) => uuid.simple().to_string(),
        Err(_) => instance_id.to_owned(),
    };
    in_dir(dir, &format!("neovibe-sc-{id}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `std::env::temp_dir()` is on this project's Mac mini, byte for byte in length. Every
    /// user's `/var/folders/<2>/<28>/T/` has this shape, so this is the real worst case there
    /// rather than a guess.
    const MACOS_TEMP_DIR: &str = "/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/";

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
        assert!(err.to_string().contains("104 bytes"), "the error should name the length: {err}");
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
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("a 103-byte path must bind");
        drop(listener);
        let _ = std::fs::remove_file(&path);

        #[cfg(target_os = "macos")]
        {
            let one_more = dir.join(format!("{name}y"));
            assert_eq!(one_more.as_os_str().len(), MAX_SOCKET_PATH_BYTES + 1);
            assert!(std::os::unix::net::UnixListener::bind(&one_more).is_err(), "104 bytes must not bind on macOS");
            let _ = std::fs::remove_file(&one_more);
        }
    }

    /// Both production socket paths fit under the macOS temp dir, and under this machine's own.
    #[test]
    fn every_production_socket_path_fits_under_the_macos_temp_dir() {
        for dir in [Path::new(MACOS_TEMP_DIR).to_path_buf(), std::env::temp_dir()] {
            let hook = hook_socket(&dir, uuid::Uuid::new_v4()).unwrap();
            let sidecar = sidecar_socket(&dir, &uuid::Uuid::new_v4().to_string()).unwrap();
            for path in [hook, sidecar] {
                assert!(path.as_os_str().len() <= MAX_SOCKET_PATH_BYTES, "{path:?}");
            }
        }
    }

    #[test]
    fn a_hyphenated_uuid_instance_id_is_written_in_its_simple_form() {
        let id = uuid::Uuid::new_v4();
        let path = sidecar_socket(Path::new("/t"), &id.to_string()).unwrap();
        assert_eq!(path, Path::new(&format!("/t/neovibe-sc-{}.sock", id.simple())));
        assert_eq!(sidecar_socket(Path::new("/t"), "abc").unwrap(), Path::new("/t/neovibe-sc-abc.sock"));
    }

    /// The assertion over every construction site. Any line in `agent/src` or `agent/tests` that
    /// mentions `.sock` outside a comment has to be in this file, passed to `in_dir` on that same
    /// line, or on the list below, and
    /// every entry on the list says why that path is never bound. A new socket path built anywhere
    /// else fails here, on Linux, before it fails `bind` on a Mac.
    ///
    /// Scoped to this crate only. `neovibe-core/src/socket_path_guard.rs` is the same scan over
    /// that crate's sources instead -- a separate scanner, not a shared one, so keep both in step
    /// by hand when either crate's set of socket-building files changes.
    #[test]
    fn every_sock_path_in_this_crate_is_built_here() {
        // (file, a substring of the line) -- each one a path that is never passed to bind/connect.
        const NEVER_BOUND: &[(&str, &str)] = &[
            // Only formats the hook's argv JSON; asserts on the string.
            ("src/settings.rs", "\"/tmp/neovibe-agent-hook-abc123.sock\""),
            ("src/settings.rs", "dir.join(\"s.sock\")"),
            ("src/settings.rs", "\"/tmp/a.sock\""),
            ("src/settings.rs", "\"/tmp/b.sock\""),
            // Probes that the agent-hook binary exists; never bound.
            ("src/process.rs", "dir.join(\"probe.sock\")"),
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
                for (n, line) in text.lines().enumerate() {
                    let code = line.trim_start();
                    // A path handed straight to `in_dir` on the same line is checked.
                    if code.starts_with("//") || !mentions_a_sock_file(code) || code.contains("socket_path::in_dir(") {
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
    fn mentions_a_sock_file(code: &str) -> bool {
        code.match_indices(".sock").any(|(i, m)| {
            !code[i + m.len()..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
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
