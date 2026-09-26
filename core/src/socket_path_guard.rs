//! The twin of `agent`'s `every_sock_path_in_this_crate_is_built_here`, over this crate.
//!
//! `agent`'s version walks `agent/src` and `agent/tests` only, and its doc promises that "a new
//! construction site elsewhere fails a test rather than a Mac". L2 T5 added two construction
//! sites here, outside that walk, and made that promise narrower than it reads. Today's two
//! paths are each pinned by an exact-length test of their own, so nothing was broken -- but a
//! *third* socket added to this crate later would have escaped the only systematic guard there
//! is. This restores it rather than rewording the promise.

use std::path::Path;

/// `(file, a substring of the line)` -- each one a `.sock` mention that is never a bound or
/// connected *path*, with the reason it is not.
const NEVER_BOUND: &[(&str, &str)] = &[
    // Parser inputs and file-name arguments: strings that name a socket file, not paths to one.
    // Every path actually built from them goes through `in_dir` at its own construction site.
    (
        "src/instance_dir.rs",
        "stale_instance_dir_pid(\"neovibe-supervisor.sock\"",
    ),
    (
        "src/instance_dir.rs",
        "sweep_stale_instance_dirs(&root, PREFIX, \"switch.sock\"",
    ),
    (
        "src/instance_dir.rs",
        "sweep_stale_instance_dirs(&root, BIND_PREFIX, \"s.sock\"",
    ),
    // Deliberately a plain file, not a socket: the fixture for "this directory's socket answers
    // nothing", which is the case the sweep must reclaim.
    ("src/instance_dir.rs", "std::fs::write(silent.join(\"s.sock\")"),
    // The file-name half of each protocol's own naming. `SOCKET_NAME` reaches `bind` only
    // through `in_dir`; the `SWEPT_NAMES` entries reach only `connect()`, on a path the sweep
    // derives from a directory it is already scanning.
    ("src/pane_switch.rs", "const SOCKET_NAME: &str = \"s.sock\""),
    ("src/pane_switch.rs", "const SWEPT_NAMES:"),
    ("src/theme/feed.rs", "const SOCKET_NAME: &str = \"t.sock\""),
    ("src/theme/feed.rs", "const SWEPT_NAMES:"),
    ("src/editor_context/feed.rs", "const SOCKET_NAME: &str = \"e.sock\""),
    ("src/editor_context/feed.rs", "const SWEPT_NAMES:"),
    ("src/nvim_keys/feed.rs", "const SOCKET_NAME: &str = \"k.sock\""),
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
    assert!(
        offenders.is_empty(),
        "socket paths built outside agent::socket_path escape its length check:\n{}",
        offenders.join("\n")
    );
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
/// The check used to be "the same line mentions `in_dir(`", which was true of every call site until
/// `rustfmt.toml` landed (2026-09-19) and rustfmt wrapped the longer ones onto their own argument
/// lines -- in `agent`, whose twin of this scanner had the same check, six correctly routed sites
/// became offenders at once. The invariant was never about one line: it is that the name reaches
/// `in_dir`, which owns the length cap. So walk back over the open argument list instead, stopping
/// at the statement boundary a `;` or a blank line marks, and never further than a short window.
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
