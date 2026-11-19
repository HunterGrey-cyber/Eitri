//! Every socket path of `eitri-core` and of `agent` is built through `agent::socket_path::in_dir`.
//!
//! `in_dir` owns the length cap that keeps a path inside macOS's 103 bytes; a socket path built any other way
//! escapes it and fails on a Mac, not on the machine the tests run on. A `.sock` name that sits outside the
//! argument list of an `in_dir` call and is not on an allowlist here fails (`source_scan::unrouted_socket_sites`).
//! `agent` cannot depend on this crate, so its scan is made here, over `agent/src` and `agent/tests`; `desk` and
//! `mac` scan their own sources the same way.

use crate::source_scan::{require_anchors, rust_sources, stale_never_bound, unrouted_socket_sites};

/// `(file, a substring of the line)`: each one a `.sock` mention that is never a bound or connected *path*, with
/// the reason it is not.
const NEVER_BOUND: &[(&str, &str)] = &[
    // Parser inputs and file-name arguments: strings that name a socket file, not paths to one.
    // Every path actually built from them goes through `in_dir` at its own construction site.
    (
        "core/src/instance_dir.rs",
        "stale_instance_dir_pid(\"eitri-panel.sock\"",
    ),
    (
        "core/src/instance_dir.rs",
        "sweep_stale_instance_dirs(&root, PREFIX, \"switch.sock\"",
    ),
    (
        "core/src/instance_dir.rs",
        "sweep_stale_instance_dirs(&root, BIND_PREFIX, \"s.sock\"",
    ),
    // The owner-check test's two sweeps: the same file-name argument.
    (
        "core/src/instance_dir.rs",
        "sweep_entries_owned_by(&root, BIND_PREFIX, \"s.sock\"",
    ),
    // The injected probe receives a derived path but never binds or connects to it.
    ("core/src/instance_dir.rs", "sweep_with_probe(&root, PREFIX, \"s.sock\""),
    // The full-backlog test runs the sweep on its own thread so a hang shows up
    // as `recv_timeout` expiring rather than wedging the whole test process; the thread's clone of
    // `root` is named differently so the closure can move it while the original stays usable after
    // `thread::spawn` returns.
    (
        "core/src/instance_dir.rs",
        "sweep_stale_instance_dirs(&root_for_thread, BIND_PREFIX, \"s.sock\"",
    ),
    // Deliberately a plain file, not a socket: the fixture for "this directory's socket answers
    // nothing", which is the case the sweep must reclaim.
    ("core/src/instance_dir.rs", "std::fs::write(silent.join(\"s.sock\")"),
    // The file-name half of each protocol's own naming. `SOCKET_NAME` reaches `bind` only
    // through `in_dir`; the `SWEPT_NAMES` entries reach only `connect()`, on a path the sweep
    // derives from a directory it is already scanning.
    ("core/src/pane_switch.rs", "const SOCKET_NAME: &str = \"s.sock\""),
    // The clipboard socket's file name: bound only through `in_dir` (`pane_switch::bind_clipboard`); the
    // shim joins it to the channel directory it already holds, the path that `in_dir` passed at the bind.
    ("core/src/host_clipboard.rs", "pub const SOCKET_NAME: &str = \"c.sock\""),
    ("core/src/pane_switch.rs", "const SWEPT_NAMES:"),
    ("core/src/theme/feed.rs", "const SOCKET_NAME: &str = \"t.sock\""),
    ("core/src/theme/feed.rs", "const SWEPT_NAMES:"),
    (
        "core/src/editor_context/feed.rs",
        "const SOCKET_NAME: &str = \"e.sock\"",
    ),
    ("core/src/editor_context/feed.rs", "const SWEPT_NAMES:"),
    ("core/src/nvim_keys/feed.rs", "const SOCKET_NAME: &str = \"k.sock\""),
    ("core/src/plugin/follow_feed.rs", "const SOCKET_NAME: &str = \"f.sock\""),
    ("core/src/plugin/follow_feed.rs", "const SWEPT_NAMES:"),
    // `eitri remote serve`'s socket's file name: its path is built only through `in_dir`
    // (`remote_rv::serve_socket_path`).
    ("core/src/remote_rv.rs", "const SERVE_SOCKET: &str = \"remote.sock\""),
    // `serve`'s control socket's file name: its path is built only through `in_dir`
    // (`remote_rv::serve_control_socket_path`).
    (
        "core/src/remote_rv.rs",
        "const SERVE_CONTROL_SOCKET: &str = \"remote-ctl.sock\"",
    ),
    ("core/src/ide_server/mention.rs", "const SOCKET_NAME: &str = \"m.sock\""),
    // The agents directory's cleanup unlinks a dead process's control socket by its file name, relative to
    // the control directory's handle; it never binds or connects, and the socket was bound through `in_dir`.
    (
        "core/src/agents/liveness.rs",
        "let socket_name = c_name(format!(\"ag-{instance}.sock\"))?",
    ),
    // The host's tests hand a fake editor a socket name to compare against what the host sends back;
    // nothing binds or connects to it.
    ("core/src/ide_server/host.rs", "socket: PathBuf::from(\"/run/m.sock\")"),
    (
        "core/src/ide_server/host.rs",
        "let socket = PathBuf::from(\"/run/m.sock\")",
    ),
    (
        "core/src/ide_server/host.rs",
        "Value::Binary(b\"/run/m.sock\".to_vec())",
    ),
];

/// The same list for `agent`'s sources: each one a path that is never bound as written, or bound only under a
/// scratch root.
const AGENT_NEVER_BOUND: &[(&str, &str)] = &[
    // Only formats the hook's argv JSON; asserts on the string.
    ("agent/src/settings.rs", "\"/tmp/eitri-agent-hook-abc123.sock\""),
    ("agent/src/settings.rs", "dir.join(\"s.sock\")"),
    ("agent/src/settings.rs", "\"/tmp/a.sock\""),
    ("agent/src/settings.rs", "\"/tmp/b.sock\""),
    // The editor tool server's argv as an ACP session/new request carries it; the strings are
    // compared against the request JSON and nothing binds them.
    (
        "agent/tests/acp_provider.rs",
        "args: vec![\"/run/user/1000/eitri/bridge.sock\".to_string()",
    ),
    (
        "agent/tests/acp_provider.rs",
        "\"args\": [\"/run/user/1000/eitri/bridge.sock\"",
    ),
    // Probes that the agent-hook binary exists; never bound.
    ("agent/src/process.rs", "dir.join(\"probe.sock\")"),
    // Absolute paths of a host that the sandbox self-check's tests re-create inside a scratch
    // root: each is bound at a short name in that root and renamed to the long one.
    ("agent/src/testkit/claude_scripted/sandbox.rs", "\"/run/docker.sock\""),
    (
        "agent/src/testkit/claude_scripted/sandbox.rs",
        "\"/var/lib/gssproxy/default.sock\"",
    ),
];

/// Files whose `.sock` mentions are the scan's own: this guard quotes every line it excuses, `source_scan` (compiled
/// only for tests and `test-support`, so nothing in it is bound by a product build) holds the matcher's fixtures, and
/// `agent/src/socket_path.rs` defines `in_dir` and tests it.
const OWN_FILES: &[&str] = &[
    "core/src/socket_path_guard.rs",
    "core/src/source_scan.rs",
    "agent/src/socket_path.rs",
];

fn assert_all_routed(dirs: &[&str], anchors: &[&str], never_bound: &[(&str, &str)]) {
    let mut read = rust_sources(dirs);
    require_anchors(&read, anchors);
    read.retain(|source| !OWN_FILES.contains(&source.path.as_str()));
    let sites = unrouted_socket_sites(&read, never_bound);
    assert!(
        sites.is_empty(),
        "socket paths built outside agent::socket_path escape its length check:\n{}",
        sites.join("\n")
    );
}

#[test]
fn every_sock_path_in_this_crate_is_built_through_agent_socket_path() {
    assert_all_routed(&["core/src"], &["fn create_instance_dir("], NEVER_BOUND);
}

/// `agent` reaches `bind` through `in_dir` at every site of its own sources and tests.
#[test]
fn every_sock_path_in_agent_is_built_through_socket_path_in_dir() {
    assert_all_routed(&["agent/src", "agent/tests"], &["pub fn in_dir("], AGENT_NEVER_BOUND);
}

/// Every allowlist entry still matches a real line. Without this an allowlist rots into a place where a
/// construction site can hide behind a substring nothing writes any more.
#[test]
fn no_allowlist_entry_is_stale() {
    let read = rust_sources(&["core/src", "agent/src", "agent/tests"]);
    let stale: Vec<String> = stale_never_bound(&read, NEVER_BOUND)
        .into_iter()
        .chain(stale_never_bound(&read, AGENT_NEVER_BOUND))
        .collect();
    assert!(
        stale.is_empty(),
        "allowlist entries no file contains any more:\n{}",
        stale.join("\n")
    );
}
