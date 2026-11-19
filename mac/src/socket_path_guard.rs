//! Every socket path of this crate is built through `agent::socket_path::in_dir`, like `eitri-core`'s and `agent`'s
//! (`core/src/socket_path_guard.rs`), over this crate's sources.
//!
//! `in_dir` owns the length cap that keeps a path inside macOS's 103 bytes; a socket path built any other
//! way escapes it and fails on a Mac, not on the machine the tests run on. A `.sock` name outside the argument list
//! of an `in_dir` call and off the allowlist below fails (`source_scan::unrouted_socket_sites`).

use eitri_core::source_scan::{require_anchors, rust_sources, stale_never_bound, unrouted_socket_sites};

/// `(file, a substring of the line)`: each one a `.sock` mention that is never a bound or connected path.
const NEVER_BOUND: &[(&str, &str)] = &[
    // The file-name half of the editor socket's naming; it reaches `bind` only through `in_dir`.
    ("mac/src/startup.rs", "const NVIM_SOCKET: &str = \"n.sock\""),
    // The test fixture for `neovide_args`: a path handed to a function that only formats it.
    ("mac/src/startup.rs", "let sock = Path::new(\"/r/n.sock\");"),
    // The file-name half of the acceptance socket's naming; it reaches `bind` only through `in_dir`.
    ("mac/src/appkit/acceptance.rs", "const SOCKET: &str = \"a.sock\""),
];

#[test]
fn every_sock_path_in_this_crate_is_built_through_agent_socket_path() {
    let mut read = rust_sources(&["mac/src"]);
    require_anchors(&read, &["fn paths(", "fn neovide_vars_to_remove("]);
    // The guard quotes every line it excuses.
    read.retain(|source| source.path != "mac/src/socket_path_guard.rs");
    let sites = unrouted_socket_sites(&read, NEVER_BOUND);
    assert!(
        sites.is_empty(),
        "socket paths built outside agent::socket_path escape its length check:\n{}",
        sites.join("\n")
    );
}

/// Every entry on [`NEVER_BOUND`] still matches a real line, so the allowlist cannot become a place a
/// construction site hides behind a substring nothing writes any more.
#[test]
fn no_allowlist_entry_is_stale() {
    let stale = stale_never_bound(&rust_sources(&["mac/src"]), NEVER_BOUND);
    assert!(
        stale.is_empty(),
        "allowlist entries no file contains any more:\n{}",
        stale.join("\n")
    );
}
