//! The crates below `eitri-desk` never depend on it: `eitri-core`, `eitri-panel`, `eitri-remote` and every
//! `terminal/*` crate. A dependency that points up would make a host-side change rebuild them and let them
//! reach for code that needs a panel or a window. The scan also follows their in-tree path dependencies.
//!
//! The manifests are read as text, comments cut off, so a comment that explains the rule cannot trip it and a
//! dependency in any table (dev- and build-dependencies, a target table, a `[dependencies.eitri-desk]` header,
//! a `package = "eitri-desk"` rename, a `path = "../desk"`) cannot hide.

use std::collections::BTreeSet;
use std::path::Path;

use eitri_core::source_scan::{manifest_path_dependencies, read_required, workspace_members, workspace_root};

/// The non-terminal crates that must stay below this one.
const BELOW: &[&str] = &["core", "panel", "remote"];

fn crates_below(root: &Path, manifest: &str) -> Vec<String> {
    let mut members = Vec::new();
    for member in workspace_members(manifest) {
        if !BELOW.contains(&member.as_str()) && !member.starts_with("terminal/") {
            continue;
        }
        let Some(dir) = member.strip_suffix("/*") else {
            members.push(member);
            continue;
        };
        let path = root.join(dir);
        let entries = path
            .read_dir()
            .unwrap_or_else(|err| panic!("cannot list {}: {err}", path.display()));
        let mut matches = Vec::new();
        for entry in entries {
            let entry = entry.expect("read workspace member directory entry");
            if entry.path().join("Cargo.toml").is_file() {
                let name = entry.file_name();
                matches.push(format!("{dir}/{}", name.to_str().expect("UTF-8 workspace member name")));
            }
        }
        matches.sort_unstable();
        members.extend(matches);
    }
    members
}

/// The lines of `manifest` (comments cut) that name this crate: by package name or by its directory.
fn naming_lines(manifest: &str) -> Vec<String> {
    manifest
        .lines()
        .enumerate()
        .map(|(n, line)| (n + 1, line.split('#').next().unwrap_or("").trim()))
        .filter(|(_, line)| line.contains("eitri-desk") || line.contains("../desk") || line.contains("/desk\""))
        .map(|(n, line)| format!("{n}: {line}"))
        .collect()
}

fn direction_offenders(root: &Path, workspace: &str) -> Vec<String> {
    let root = root.canonicalize().expect("workspace root exists");
    let mut pending = Vec::new();
    for member in crates_below(&root, workspace) {
        let path = root.join(member);
        pending.push(
            path.canonicalize()
                .unwrap_or_else(|err| panic!("cannot resolve workspace member {}: {err}", path.display())),
        );
    }
    let mut visited = BTreeSet::new();
    let mut offenders = Vec::new();
    while let Some(member) = pending.pop() {
        if !visited.insert(member.clone()) {
            continue;
        }
        let path = member.join("Cargo.toml");
        let manifest = read_required(&path).text;
        let relative = path.strip_prefix(&root).unwrap_or(&path);
        offenders.extend(
            naming_lines(&manifest)
                .into_iter()
                .map(|line| format!("{}:{line}", relative.display())),
        );
        pending.extend(
            manifest_path_dependencies(&manifest, workspace, &member)
                .into_iter()
                .filter_map(|dep| root.join(dep).canonicalize().ok())
                .filter(|dep| dep.starts_with(&root)),
        );
    }
    offenders
}

#[test]
fn no_crate_below_names_eitri_desk() {
    let offenders = direction_offenders(&workspace_root(), &read_required("Cargo.toml").text);
    assert!(
        offenders.is_empty(),
        "eitri-desk sits above eitri-core, eitri-panel, eitri-remote and terminal/*; none may depend on it:\n{}",
        offenders.join("\n")
    );
}

/// The fixed entries are workspace members and the terminal family is present.
#[test]
fn the_crates_below_are_workspace_members() {
    let manifest = read_required("Cargo.toml").text;
    let members = crates_below(&workspace_root(), &manifest);
    for krate in BELOW {
        assert!(members.iter().any(|m| m == krate), "{krate} is not a workspace member");
    }
    assert!(members.iter().any(|member| member.starts_with("terminal/")));
}

#[test]
fn new_terminal_members_are_included_in_the_direction_scan() {
    let manifest = "[workspace]\nmembers = ['core', 'terminal/new-renderer', 'terminal-tools', 'desk']\n";
    assert_eq!(
        crates_below(&workspace_root(), manifest),
        ["core", "terminal/new-renderer"]
    );
}

#[test]
fn terminal_member_globs_expand_to_crate_directories() {
    let manifest = "[workspace]\nmembers = ['terminal/*']\n";
    let members = crates_below(&workspace_root(), manifest);
    assert!(members.iter().any(|member| member == "terminal/engine"));
    assert!(members.iter().any(|member| member == "terminal/render"));
    assert!(members.iter().all(|member| !member.contains('*')));
}

#[test]
fn direction_scan_reaches_unlisted_path_dependencies() {
    use eitri_core::plugin::test_support::Scratch;

    let scratch = Scratch::new("direction");
    let root = &scratch.0;
    let workspace = "\
        [workspace]\nmembers = ['terminal/*']\n\
        [workspace.dependencies]\nbridge = { path = 'support/bridge' }\n";
    std::fs::create_dir_all(root.join("terminal/assets")).unwrap();
    for (dir, manifest) in [
        ("terminal/render", "[build-dependencies]\nbridge.workspace = true\n"),
        (
            "support/bridge",
            "[target.'cfg(unix)'.dev_dependencies.leaf]\npath = '../leaf'\n",
        ),
        ("support/leaf", "[dependencies]\neitri-desk = '1'\n"),
    ] {
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), manifest).unwrap();
    }
    assert_eq!(crates_below(root, workspace), ["terminal/render"]);
    assert_eq!(
        direction_offenders(root, workspace),
        ["support/leaf/Cargo.toml:2: eitri-desk = '1'"]
    );
}

#[test]
fn the_matcher_sees_every_way_to_depend_on_this_crate() {
    for manifest in [
        "[dependencies]\neitri-desk = { path = \"../desk\" }\n",
        "[dev-dependencies]\nlinked = { package = \"eitri-desk\", path = \"../desk\" }\n",
        "[dependencies.eitri-desk]\npath = \"../desk\"\n",
        "[target.'cfg(unix)'.dependencies]\nx = { path = \"../desk\" }\n",
    ] {
        assert!(!naming_lines(manifest).is_empty(), "not caught:\n{manifest}");
    }
    for manifest in [
        "# eitri-desk is above this crate\n[dependencies]\neitri-core = { path = \"../core\" }\n",
        "[dependencies]\ndesktop-entry = \"1\"\n",
    ] {
        assert!(naming_lines(manifest).is_empty(), "a false alarm:\n{manifest}");
    }
}
