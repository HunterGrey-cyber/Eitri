//! The permission policy resolves paths in the application process. The CLI child runs with the
//! project root as its cwd (`agent::process` sets `current_dir`), while the application keeps its cwd.
//! With the application in `~/proj/src` and the CLI in `~/proj`, `/proc/self/cwd` differs between them.
//!
//! A repository link `d -> /proc/self/cwd/..` makes `d/.config/autostart/evil.desktop` resolve to
//! `~/proj/.config/autostart/evil.desktop` in the application but `~/.config/autostart/evil.desktop`
//! in the CLI. Allowing the first path would permit a write outside the project in the CLI.
//! The same resolver judges `Read`, so reads need the same protection.
//!
//! This file must contain exactly one test because it moves the process's cwd, shared by every
//! test in the binary. `/proc/self/fd`, `/dev/fd`, and `/proc/self/root` need no cwd change;
//! their tests are in `agent/src/permission_policy.rs`.

#[cfg(target_os = "linux")]
#[test]
fn a_link_through_proc_self_cwd_cards_wherever_neovibe_was_started() {
    use agent::{classify_permission_request, PermissionVerdict};
    use serde_json::json;
    use std::path::PathBuf;

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let scratch = Scratch(std::env::temp_dir().join(format!("agent-policy-cwd-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(scratch.0.join("proj/src")).unwrap();
    let root = scratch.0.join("proj").canonicalize().unwrap();
    std::fs::write(root.join("page.md"), "page\n").unwrap();
    std::fs::write(root.join("nb.ipynb"), "{}\n").unwrap();
    std::os::unix::fs::symlink("/proc/self/cwd/..", root.join("d")).unwrap();

    // `cd ~/proj/src && neovibe ~/proj`.
    let started_in = std::env::current_dir().unwrap();
    std::env::set_current_dir(root.join("src")).unwrap();
    assert_eq!(
        std::fs::canonicalize(root.join("d")).unwrap(),
        root,
        "in this process `d` must lead back to the root, or this test tests nothing"
    );

    let mut allowed = Vec::new();
    for (tool, input) in [
        ("Write", json!({ "file_path": "d/.config/autostart/evil.desktop" })),
        ("Write", json!({ "file_path": root.join("d/new.txt") })),
        ("Edit", json!({ "file_path": "d/page.md" })),
        ("NotebookEdit", json!({ "notebook_path": "d/nb.ipynb" })),
        ("Read", json!({ "file_path": "d/page.md" })),
        ("Grep", json!({ "pattern": "page", "path": "d" })),
        ("Glob", json!({ "pattern": "*.md", "path": "d" })),
        ("Bash", json!({ "command": "cat d/page.md" })),
    ] {
        let got = classify_permission_request(tool, &input, &root);
        if got.verdict != PermissionVerdict::AskTheUser {
            allowed.push(format!("{tool} {input}: {}", got.reason));
        }
    }
    std::env::set_current_dir(started_in).unwrap();
    assert!(
        allowed.is_empty(),
        "allowed through /proc/self/cwd:\n{}",
        allowed.join("\n")
    );
}
