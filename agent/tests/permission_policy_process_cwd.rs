//! The permission policy judges a path in neovibe's own process; the CLI child that then writes or
//! reads it runs with the project root as its cwd (`agent::process` sets `current_dir`), and neovibe
//! itself never changes directory. So `/proc/self/cwd` names a different directory in each: started
//! as `cd ~/proj/src && neovibe ~/proj`, neovibe's is `~/proj/src` and the CLI's `~/proj`.
//!
//! Item 4A's third review (2026-09-28, BLOCKING) reproduced it: a cloned repository holding
//! `d -> /proc/self/cwd/..` made `Write d/.config/autostart/evil.desktop` resolve, in neovibe, to
//! `~/proj/.config/autostart/evil.desktop` -- inside the project, allowed with no card -- while the
//! CLI wrote `~/.config/autostart/evil.desktop`. The same resolver judges `Read`, so a read escaped
//! the same way.
//!
//! **This file holds exactly one test, and must keep holding only one**: it moves the process's cwd,
//! which every other test running in the same binary would share. The other per-process shapes
//! (`/proc/self/fd`, `/dev/fd`, `/proc/self/root`) need no cwd moved and are unit tests in
//! `agent/src/permission_policy.rs`.

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
