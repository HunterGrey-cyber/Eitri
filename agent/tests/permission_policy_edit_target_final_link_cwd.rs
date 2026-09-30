//! Tests `resolve_edit_target`'s top-level refusal of `Resolution::ThroughAKernelTree`.
//! When a link through `/proc` or `/dev` is a non-final path component, the prefix walk in
//! `resolve_missing_edit_target` follows it again and can return the same refusal.
//! Those paths cannot detect a top-level arm that incorrectly falls through to that fallback.
//!
//! Here the edit target's final component is the symlink: `notes.md`, `nb.ipynb`, or `stdin.txt`.
//! For a one-component path, the fallback starts at zero components, resolving the root itself.
//! It then appends the filename without re-examining the link. If the top-level refusal is missing,
//! the unresolved path appears inside the root and `classify_edit_target` can allow the write.
//! These cases check that the kernel-tree refusal happens before that fallback.
//!
//! `notes.md -> /proc/self/cwd/../escaped.txt` and `nb.ipynb -> /proc/self/cwd/../escaped.ipynb`
//! require moving the process's cwd below the root. This file must contain exactly one test,
//! because every test in the same binary would share that cwd change.
//! `stdin.txt -> /dev/fd/0` needs no cwd move but stays here because it exercises the same
//! final-component refusal.

#[cfg(target_os = "linux")]
#[test]
fn a_kernel_tree_link_as_the_edit_targets_own_last_component_cards() {
    use agent::{classify_permission_request, PermissionVerdict};
    use serde_json::json;
    use std::path::PathBuf;

    // Copied from `agent::permission_policy`'s `REASON_THROUGH_A_KERNEL_TREE`, which is not public.
    // An integration test must compare its value rather than import it. Checking the reason
    // ensures a card from another check cannot hide a missing kernel-tree refusal.
    const REASON_THROUGH_A_KERNEL_TREE: &str =
        "a path passes through /proc, /sys or /dev, whose links lead somewhere different in each process";

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let scratch = Scratch(std::env::temp_dir().join(format!("agent-policy-final-link-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(scratch.0.join("proj/src")).unwrap();
    let root = scratch.0.join("proj").canonicalize().unwrap();
    // What the two `/proc/self/cwd` links resolve to in THIS process, once cwd moves to
    // `root/src`: `cwd/..` is `root` itself, so both land on a real file inside the root here --
    // "exists in Eitri's view" is what keeps this a real escape shape rather than a resolution
    // failure the policy would card for an unrelated reason.
    std::fs::write(root.join("escaped.txt"), "escaped\n").unwrap();
    std::fs::write(root.join("escaped.ipynb"), "{}\n").unwrap();
    std::os::unix::fs::symlink("/proc/self/cwd/../escaped.txt", root.join("notes.md")).unwrap();
    std::os::unix::fs::symlink("/proc/self/cwd/../escaped.ipynb", root.join("nb.ipynb")).unwrap();
    std::os::unix::fs::symlink("/dev/fd/0", root.join("stdin.txt")).unwrap();

    // `cd <root>/src && Eitri <root>`, the review's own shape.
    let started_in = std::env::current_dir().unwrap();
    std::env::set_current_dir(root.join("src")).unwrap();
    assert_eq!(
        std::fs::canonicalize(root.join("notes.md")).unwrap(),
        root.join("escaped.txt"),
        "notes.md must resolve to escaped.txt in this process, or this test tests nothing"
    );
    assert_eq!(
        std::fs::canonicalize(root.join("nb.ipynb")).unwrap(),
        root.join("escaped.ipynb"),
        "nb.ipynb must resolve to escaped.ipynb in this process, or this test tests nothing"
    );
    // `/dev/fd/0`'s own further resolution is whatever this test process's stdin happens to be
    // (a pipe under the test harness, a real file, `/dev/null`) -- not stable enough to assert a
    // canonical target on, and not what the policy ever reaches: the kernel-tree check trips on
    // `/dev` itself, one component into the link's target, before `/dev/fd/0` is read any further.
    // What must be real is the symlink `stdin.txt` itself, spelled exactly this way.
    assert_eq!(
        std::fs::read_link(root.join("stdin.txt")).unwrap(),
        PathBuf::from("/dev/fd/0"),
        "stdin.txt must be the symlink itself, or this test tests nothing"
    );

    let mut wrong = Vec::new();
    for link in ["notes.md", "nb.ipynb", "stdin.txt"] {
        for (tool, field) in [
            ("Write", "file_path"),
            ("Edit", "file_path"),
            ("NotebookEdit", "notebook_path"),
        ] {
            let got = classify_permission_request(tool, &json!({ field: link }), &root);
            if got.verdict != PermissionVerdict::AskTheUser || got.reason != REASON_THROUGH_A_KERNEL_TREE {
                wrong.push(format!(
                    "{tool} {link}: verdict {:?}, reason {:?}",
                    got.verdict, got.reason
                ));
            }
        }
    }
    std::env::set_current_dir(started_in).unwrap();
    assert!(
        wrong.is_empty(),
        "not carded for REASON_THROUGH_A_KERNEL_TREE:\n{}",
        wrong.join("\n")
    );
}
