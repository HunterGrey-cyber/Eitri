//! The twin of `agent`'s `every_sock_path_in_this_crate_is_built_here`, over this crate.
//!
//! `agent`'s guard walks only `agent/src` and `agent/tests`. This crate needs its own walk so
//! every new socket construction site is checked, in addition to exact-length tests of known paths.

use std::path::Path;

/// `(file, a substring of the line)` -- each one a `.sock` mention that is never a bound or
/// connected *path*, with the reason it is not.
const NEVER_BOUND: &[(&str, &str)] = &[
    // Parser inputs and file-name arguments: strings that name a socket file, not paths to one.
    // Every path actually built from them goes through `in_dir` at its own construction site.
    ("src/instance_dir.rs", "stale_instance_dir_pid(\"eitri-panel.sock\""),
    (
        "src/instance_dir.rs",
        "sweep_stale_instance_dirs(&root, PREFIX, \"switch.sock\"",
    ),
    (
        "src/instance_dir.rs",
        "sweep_stale_instance_dirs(&root, BIND_PREFIX, \"s.sock\"",
    ),
    // The owner-check test's two sweeps: the same file-name argument.
    (
        "src/instance_dir.rs",
        "sweep_entries_owned_by(&root, BIND_PREFIX, \"s.sock\"",
    ),
    // The full-backlog test runs the sweep on its own thread so a hang shows up
    // as `recv_timeout` expiring rather than wedging the whole test process; the thread's clone of
    // `root` is named differently so the closure can move it while the original stays usable after
    // `thread::spawn` returns.
    (
        "src/instance_dir.rs",
        "sweep_stale_instance_dirs(&root_for_thread, BIND_PREFIX, \"s.sock\"",
    ),
    // Deliberately a plain file, not a socket: the fixture for "this directory's socket answers
    // nothing", which is the case the sweep must reclaim.
    ("src/instance_dir.rs", "std::fs::write(silent.join(\"s.sock\")"),
    // The file-name half of each protocol's own naming. `SOCKET_NAME` reaches `bind` only
    // through `in_dir`; the `SWEPT_NAMES` entries reach only `connect()`, on a path the sweep
    // derives from a directory it is already scanning.
    ("src/pane_switch.rs", "const SOCKET_NAME: &str = \"s.sock\""),
    // The clipboard socket's file name: bound only through `in_dir` (`pane_switch::bind_clipboard`); the
    // shim joins it to the channel directory it already holds, the path that `in_dir` passed at the bind.
    ("src/host_clipboard.rs", "pub const SOCKET_NAME: &str = \"c.sock\""),
    ("src/pane_switch.rs", "const SWEPT_NAMES:"),
    ("src/theme/feed.rs", "const SOCKET_NAME: &str = \"t.sock\""),
    ("src/theme/feed.rs", "const SWEPT_NAMES:"),
    ("src/editor_context/feed.rs", "const SOCKET_NAME: &str = \"e.sock\""),
    ("src/editor_context/feed.rs", "const SWEPT_NAMES:"),
    ("src/nvim_keys/feed.rs", "const SOCKET_NAME: &str = \"k.sock\""),
    ("src/plugin/follow_feed.rs", "const SOCKET_NAME: &str = \"f.sock\""),
    ("src/plugin/follow_feed.rs", "const SWEPT_NAMES:"),
    // `eitri remote serve`'s socket's file name: its path is built only through `in_dir`
    // (`remote_rv::serve_socket_path`).
    ("src/remote_rv.rs", "const SERVE_SOCKET: &str = \"remote.sock\""),
    // `serve`'s control socket's file name: its path is built only through `in_dir`
    // (`remote_rv::serve_control_socket_path`).
    (
        "src/remote_rv.rs",
        "const SERVE_CONTROL_SOCKET: &str = \"remote-ctl.sock\"",
    ),
    ("src/ide_server/mention.rs", "const SOCKET_NAME: &str = \"m.sock\""),
    // The agents directory's cleanup unlinks a dead process's control socket by its file name, relative to
    // the control directory's handle; it never binds or connects, and the socket was bound through `in_dir`.
    (
        "src/agents/liveness.rs",
        "let socket_name = c_name(format!(\"ag-{instance}.sock\"))?",
    ),
    // The host's tests hand a fake editor a socket name to compare against what the host sends back;
    // nothing binds or connects to it.
    ("src/ide_server/host.rs", "socket: PathBuf::from(\"/run/m.sock\")"),
    ("src/ide_server/host.rs", "let socket = PathBuf::from(\"/run/m.sock\")"),
    ("src/ide_server/host.rs", "Value::Binary(b\"/run/m.sock\".to_vec())"),
];

#[test]
fn every_sock_path_in_this_crate_is_built_through_agent_socket_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    visit(&root.join("src"), &mut |file| {
        let rel = file.strip_prefix(root).unwrap().to_string_lossy().into_owned();
        // This file is the guard itself: its allowlist quotes every line it excuses, and its own
        // `.sock` matcher names the extension. `agent::socket_path`'s original skips itself the
        // same way. Kept in its own file precisely so this exclusion does not have to swallow
        // `lib.rs` with it.
        if rel == "src/socket_path_guard.rs" {
            return;
        }
        let text = std::fs::read_to_string(file).unwrap();
        offenders.extend(offenders_in(&rel, &text));
    });
    assert!(
        offenders.is_empty(),
        "socket paths built outside agent::socket_path escape its length check:\n{}",
        offenders.join("\n")
    );
}

/// The lines of `text` (the file `rel`) that name a socket file outside an `in_dir` call and
/// outside [`NEVER_BOUND`].
fn offenders_in(rel: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut offenders = Vec::new();
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
    offenders
}

/// The walk would let nothing through if the check itself stopped matching: a name joined by hand
/// is an offender, the same name passed to a wrapped `in_dir` call is not.
#[test]
fn a_socket_name_joined_by_hand_is_an_offender() {
    let by_hand = "fn p(dir: &Path, i: &str) -> PathBuf {\n    dir.join(format!(\"ag-{i}.sock\"))\n}\n";
    assert_eq!(offenders_in("src/agents/control.rs", by_hand).len(), 1);
    let wrapped = "fn p(dir: &Path, i: &str) -> io::Result<PathBuf> {\n    agent::socket_path::in_dir(\n        \
                   dir,\n        &format!(\"ag-{i}.sock\"),\n    )\n}\n";
    assert_eq!(offenders_in("src/agents/control.rs", wrapped), Vec::<String>::new());
}

/// Every entry on [`NEVER_BOUND`] still matches a real line. Without this an allowlist rots
/// into a place where a construction site can hide behind a substring nothing writes any more.
#[test]
fn no_allowlist_entry_is_stale() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (file, needle) in NEVER_BOUND {
        let text = std::fs::read_to_string(root.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert!(
            text.lines().any(|l| l.contains(needle)),
            "{file} no longer contains {needle:?}"
        );
    }
}

/// Whether the `.sock` name on line `n` is an argument of a `socket_path::in_dir(` call.
///
/// The name must reach `in_dir`, which owns the length cap, regardless of line wrapping.
/// Walk back over the open argument list, stopping at a `;` or blank-line statement boundary
/// and never beyond a short window. A same-line check would reject correctly wrapped calls.
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

/// `.sock` as a file extension, not as the start of an identifier like `.socket_path`.
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
