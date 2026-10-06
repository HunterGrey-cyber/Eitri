//! The twin of `eitri-core`'s and `agent`'s guards that every socket path is built through
//! `agent::socket_path::in_dir`, over this crate's sources (theirs read only their own `src/`).
//!
//! `in_dir` owns the length cap that keeps a path inside macOS's 103 bytes; a socket path built any other
//! way escapes it and fails on a Mac, not on the machine the tests run on.

use eitri_core::source_scan::{code_only, require_anchors, rust_sources};

/// `(file, a substring of the line)`: each one a `.sock` mention that is never a bound or connected path.
const NEVER_BOUND: &[(&str, &str)] = &[
    // The file-name half of the editor socket's naming; it reaches `bind` only through `in_dir`.
    ("mac/src/startup.rs", "const NVIM_SOCKET: &str = \"n.sock\""),
    // The test fixture for `neovide_args`: a path handed to a function that only formats it.
    ("mac/src/startup.rs", "let sock = Path::new(\"/r/n.sock\");"),
];

#[test]
fn every_sock_path_in_this_crate_is_built_through_agent_socket_path() {
    let read = rust_sources(&["mac/src"]);
    require_anchors(&read, &["fn paths(", "fn neovide_vars_to_remove("]);
    let mut offenders = Vec::new();
    for source in &read {
        // The guard quotes every line it excuses.
        if source.path == "mac/src/socket_path_guard.rs" {
            continue;
        }
        let lines: Vec<&str> = source.text.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || !mentions_a_sock_file(code) || routed_through_in_dir(&lines, n) {
                continue;
            }
            if NEVER_BOUND.iter().any(|(f, s)| *f == source.path && code.contains(s)) {
                continue;
            }
            offenders.push(format!("{}:{}: {code}", source.path, n + 1));
        }
    }
    assert!(
        offenders.is_empty(),
        "socket paths built outside agent::socket_path escape its length check:\n{}",
        offenders.join("\n")
    );
}

/// Every entry on [`NEVER_BOUND`] still matches a real line, so the allowlist cannot become a place a
/// construction site hides behind a substring nothing writes any more.
#[test]
fn no_allowlist_entry_is_stale() {
    let read = rust_sources(&["mac/src"]);
    for (file, needle) in NEVER_BOUND {
        let source = read
            .iter()
            .find(|s| s.path == *file)
            .unwrap_or_else(|| panic!("{file} is not read"));
        assert!(
            source.text.lines().any(|l| l.contains(needle)),
            "{file} no longer contains {needle:?}"
        );
    }
}

/// Whether the `.sock` name on line `n` is an argument of an `in_dir(` call: the open argument list is
/// walked back over, to the statement boundary a `;` or a blank line marks and never further than a
/// short window, because rustfmt breaks a long call onto several lines.
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

/// The matcher sees a socket name and does not see a bare identifier, and sees the call that routes one.
#[test]
fn the_matcher_reads_what_it_should() {
    assert!(mentions_a_sock_file("let p = dir.join(\"x.sock\");"));
    assert!(!mentions_a_sock_file("let p = self.socket_path;"));
    let lines = [
        "let p = agent::socket_path::in_dir(",
        "    &dir,",
        "    \"x.sock\",",
        ");",
    ];
    assert!(routed_through_in_dir(&lines, 2));
    assert!(!routed_through_in_dir(
        &["let a = 1;", "let p = dir.join(\"x.sock\");"],
        1
    ));
    assert!(code_only("// x.sock").trim().is_empty());
}
