//! `resolve_edit_target`'s own top-level match on `Resolution::ThroughAKernelTree` --
//! `Resolution::ThroughAKernelTree => Err(REASON_THROUGH_A_KERNEL_TREE)`, in
//! `agent/src/permission_policy.rs` -- had no test that told it apart from
//! `resolve_missing_edit_target`'s own defensive copy of the same arm. Every existing test
//! (`a_link_through_proc_self_cwd_cards_wherever_neovibe_was_started` in
//! `permission_policy_process_cwd.rs`, `a_link_through_proc_or_dev_cards_for_every_path_tool` in
//! `permission_policy.rs` itself) puts the `/proc`/`/dev` link as a NON-final path component
//! (`d/.config/autostart/evil.desktop`, `via-proc-self-fd/page.md`): when
//! `resolve_missing_edit_target`'s prefix walk tries the path one component shorter, that shorter
//! prefix still contains the link, follows it again, and hits its OWN `ThroughAKernelTree` arm --
//! the same card, by a different route. A mutant that redirects the top-level arm into the
//! fallback instead of returning `Err` (mutant M24) survives every one of those tests: it is an
//! equivalent mutant for that shape, and only for that shape.
//!
//! This file is the shape the fallback cannot mask: the kernel-tree-touching link is the edit
//! target's own LAST path component (`notes.md`, `nb.ipynb` and `stdin.txt` are themselves the
//! symlinks, not a directory two levels up from one). For a one-component raw path,
//! `resolve_missing_edit_target`'s prefix walk starts at `components.len() - 1` components, which
//! is zero -- `resolve_in_root(root, "")`, `root` itself by construction -- so under mutant M24 it
//! never re-examines the symlink at all: it joins the symlink's own name onto `root` literally and
//! returns that unresolved spelling as the target, which `starts_with(root)` and is not a
//! protected name, so `classify_edit_target` allows the write with no card. That silent allow is
//! the real escape mutant M24 stands in for.
//!
//! `notes.md -> /proc/self/cwd/../escaped.txt` and `nb.ipynb -> /proc/self/cwd/../escaped.ipynb`
//! need the process's own cwd moved below the root -- the same shape
//! `permission_policy_process_cwd.rs` uses, and for the same reason that file gives itself its own
//! test binary: **this file holds exactly one test, and must keep holding only one**, because it
//! moves the process's cwd, which every other test running in the same binary would share.
//! `stdin.txt -> /dev/fd/0` needs no cwd move, but sits alongside the other two here rather than as
//! a unit test in `permission_policy.rs`'s own `mod tests`, since all three are one reviewed
//! scenario and the review found the gap across all three at once.

#[cfg(target_os = "linux")]
#[test]
fn a_kernel_tree_link_as_the_edit_targets_own_last_component_cards() {
    use agent::{classify_permission_request, PermissionVerdict};
    use serde_json::json;
    use std::path::PathBuf;

    // Copied from `agent::permission_policy`'s own private `REASON_THROUGH_A_KERNEL_TREE`: not
    // exported from the crate (it is not `pub`), so an integration test can only compare it by
    // value rather than by name. Checking the reason, not only the verdict, is the point of this
    // file -- a card for the WRONG reason would not tell mutant M24's silent-allow apart from a
    // correct card that happened to be reached some other way.
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
    // "exists in neovibe's view" is what keeps this a real escape shape rather than a resolution
    // failure the policy would card for an unrelated reason.
    std::fs::write(root.join("escaped.txt"), "escaped\n").unwrap();
    std::fs::write(root.join("escaped.ipynb"), "{}\n").unwrap();
    std::os::unix::fs::symlink("/proc/self/cwd/../escaped.txt", root.join("notes.md")).unwrap();
    std::os::unix::fs::symlink("/proc/self/cwd/../escaped.ipynb", root.join("nb.ipynb")).unwrap();
    std::os::unix::fs::symlink("/dev/fd/0", root.join("stdin.txt")).unwrap();

    // `cd <root>/src && neovibe <root>`, the review's own shape.
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
