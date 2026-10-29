//! Who changed what: the files a turn's own edit calls named, set against the files that differ on
//! disk.
//!
//! A file is "named" when one of the tab's own file-changing calls in the turn targets it and
//! finished without an error: Claude's `Write`, `Edit`, `MultiEdit` and `NotebookEdit`, Codex's
//! applied `fileChange` items (the tab's own and its subagents'), and an ACP agent's finished
//! `acp:edit`, `acp:delete` and `acp:move` calls. Commands name nothing, whichever agent ran them,
//! so what a shell changes stays `workspace`. Naming is all the transcript can say: it
//! cannot tell whether someone else also touched the file afterwards, so the review never claims
//! the agent changed anything -- it shows what changed on disk, and these signs say which of those
//! files the agent's own calls named.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// The tools whose calls change a file named in their input.
const CHANGING_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// The files a set of turns' own edit calls named.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamedPaths {
    /// The paths named by calls that finished without an error, as the tools named them (usually
    /// absolute) until [`NamedPaths::relative_to`] makes them relative to the project root.
    pub paths: BTreeSet<PathBuf>,
    /// Changing calls with no result yet: they may or may not have written anything.
    pub pending_no_result: usize,
}

impl NamedPaths {
    /// The same set with every path made relative to `root`, through its real path where one can
    /// be read (so a path spelled through a symlink names the file the snapshot lists), lexically
    /// otherwise. A path that is not inside the project is dropped: no file outside it can appear
    /// in a review.
    pub fn relative_to(&self, root: &Path) -> NamedPaths {
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let paths = self
            .paths
            .iter()
            .filter_map(|path| relative_path(path, root, &canonical_root))
            .collect();
        NamedPaths {
            paths,
            pending_no_result: self.pending_no_result,
        }
    }
}

/// Which of the three file signs a row carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Named by the turn's calls, and changed on disk (`✓`).
    Agent,
    /// Named by the turn's calls, but the same on disk as before (`·`): written back unchanged,
    /// reverted, or ignored by the snapshot. Which of these is not guessed.
    AgentOnly,
    /// Changed on disk without any of the turn's calls naming it (`?`): a shell command, a
    /// formatter, a hook, another tab, or the user.
    Workspace,
}

impl Origin {
    /// The wire's name for the sign.
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Agent => "agent",
            Origin::AgentOnly => "agent_only",
            Origin::Workspace => "workspace",
        }
    }
}

/// The file-changing calls of the turns `turn_ids` among `tool_calls`: the paths of those that finished
/// without an error, and how many have no result yet. What a call names depends on its tool: Claude's
/// tools carry `input.file_path` (`input.notebook_path` for a notebook), Codex's `fileChange` carries
/// `changes[].path` and `movePath`, an ACP edit carries `diffs[].path` and `locations[].path`.
pub fn named_paths(tool_calls: &[agent::ToolCallRecord], turn_ids: &[&str]) -> NamedPaths {
    let mut named = NamedPaths::default();
    for call in tool_calls {
        if let Some(children) = &call.children {
            let child_named = named_paths(&children.tool_calls, turn_ids);
            named.paths.extend(child_named.paths);
            named.pending_no_result += child_named.pending_no_result;
        }
        if call.background || !turn_ids.contains(&call.turn_id.as_str()) {
            continue;
        }
        let kind = match call_kind(&call.name) {
            Some(kind) => kind,
            None => continue,
        };
        match &call.result {
            None => named.pending_no_result += 1,
            Some(result) if result.is_error => {}
            // Codex words a finished `fileChange` by its status; only an applied one changed anything.
            Some(result) if kind == Kind::CodexFileChange && !is_applied(&result.content) => {}
            Some(_) => kind.collect(&call.input, &mut named.paths),
        }
    }
    named
}

/// How a changing call spells the files it targets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    ClaudeFile,
    ClaudeNotebook,
    CodexFileChange,
    AcpEdit,
}

fn call_kind(name: &str) -> Option<Kind> {
    match name {
        "NotebookEdit" => Some(Kind::ClaudeNotebook),
        n if CHANGING_TOOLS.contains(&n) => Some(Kind::ClaudeFile),
        "fileChange" => Some(Kind::CodexFileChange),
        "acp:edit" | "acp:delete" | "acp:move" => Some(Kind::AcpEdit),
        _ => None,
    }
}

fn is_applied(content: &serde_json::Value) -> bool {
    content.as_str() == Some(agent::providers::codex::translate::FILE_CHANGE_APPLIED)
}

impl Kind {
    fn collect(self, input: &serde_json::Value, paths: &mut BTreeSet<PathBuf>) {
        let mut add = |value: Option<&serde_json::Value>| {
            if let Some(path) = value.and_then(|v| v.as_str()).filter(|p| !p.is_empty()) {
                paths.insert(PathBuf::from(path));
            }
        };
        let entries = |key: &str| input.get(key).and_then(|v| v.as_array()).into_iter().flatten();
        match self {
            Kind::ClaudeFile => add(input.get("file_path")),
            Kind::ClaudeNotebook => add(input.get("notebook_path")),
            Kind::CodexFileChange => {
                for change in entries("changes") {
                    add(change.get("path"));
                    add(change.get("movePath"));
                }
            }
            Kind::AcpEdit => {
                for key in ["diffs", "locations"] {
                    for entry in entries(key) {
                        add(entry.get("path"));
                    }
                }
            }
        }
    }
}

/// The sign of `path` (relative to the project root): whether the turn's calls named it, against
/// whether it differs on disk. `None` for a file neither named nor changed, which has no row.
pub fn origin(path: &Path, named: &NamedPaths, changed: bool) -> Option<Origin> {
    match (named.paths.contains(path), changed) {
        (true, true) => Some(Origin::Agent),
        (true, false) => Some(Origin::AgentOnly),
        (false, true) => Some(Origin::Workspace),
        (false, false) => None,
    }
}

/// `path` relative to the project, or `None` when it is outside it. A relative `path` is taken as
/// relative to the project already.
fn relative_path(path: &Path, root: &Path, canonical_root: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let inside = |real: &Path, base: &Path| {
        real.strip_prefix(base)
            .ok()
            .filter(|rest| !rest.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    // The real path decides when there is one: a tool writes through a symlink -- a `/var` that is
    // really `/private/var`, a linked checkout, a linked directory inside the project -- and the
    // snapshot lists the file where it really is. The file itself may be gone, so its directory
    // is resolved when it cannot be. Where that leads outside the project, so did the write.
    let real = absolute.canonicalize().ok().or_else(|| {
        let name = absolute.file_name()?;
        Some(absolute.parent()?.canonicalize().ok()?.join(name))
    });
    if let Some(real) = real {
        return inside(&real, canonical_root);
    }
    // Nothing of it is on disk to resolve.
    let cleaned = lexically_clean(&absolute)?;
    [root, canonical_root]
        .into_iter()
        .find_map(|base| inside(&cleaned, base))
}

/// `path` with `.` removed and each `..` taken against the component before it. `None` when a
/// `..` would climb above the root.
fn lexically_clean(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{ToolCallRecord, ToolCallResult};
    use serde_json::json;

    fn call(turn: &str, name: &str, input: serde_json::Value, result: Option<bool>) -> ToolCallRecord {
        ToolCallRecord {
            children: None,
            background: false,
            seq: 0,
            turn_id: turn.into(),
            tool_use_id: format!("tu-{name}-{turn}"),
            name: name.into(),
            input,
            result: result.map(|is_error| ToolCallResult {
                content: json!("done"),
                is_error,
            }),
            denied: None,
            changed_input: None,
        }
    }

    #[test]
    fn named_paths_counts_only_successful_changing_calls() {
        let calls = vec![
            call(
                "t1",
                "Write",
                json!({ "file_path": "/p/a.rs", "content": "" }),
                Some(false),
            ),
            call("t1", "Edit", json!({ "file_path": "/p/failed.rs" }), Some(true)),
            call("t1", "MultiEdit", json!({ "file_path": "/p/b.rs" }), Some(false)),
            call(
                "t1",
                "NotebookEdit",
                json!({ "notebook_path": "/p/c.ipynb" }),
                Some(false),
            ),
            call("t1", "Write", json!({ "file_path": "/p/running.rs" }), None),
            call("t1", "Read", json!({ "file_path": "/p/read.rs" }), Some(false)),
            call("t1", "Bash", json!({ "command": "touch /p/bash.rs" }), Some(false)),
            call("t2", "Write", json!({ "file_path": "/p/other-turn.rs" }), Some(false)),
        ];
        let named = named_paths(&calls, &["t1"]);
        let expected: BTreeSet<PathBuf> = ["/p/a.rs", "/p/b.rs", "/p/c.ipynb"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(named.paths, expected);
        assert_eq!(named.pending_no_result, 1, "the Write with no result yet");

        let both = named_paths(&calls, &["t1", "t2"]);
        assert!(both.paths.contains(Path::new("/p/other-turn.rs")));
        assert_eq!(named_paths(&calls, &[]), NamedPaths::default());
    }

    #[test]
    fn origin_follows_the_four_signs() {
        let named = NamedPaths {
            paths: [PathBuf::from("a.rs"), PathBuf::from("b.rs")].into_iter().collect(),
            pending_no_result: 2,
        };
        assert_eq!(origin(Path::new("a.rs"), &named, true), Some(Origin::Agent));
        assert_eq!(origin(Path::new("b.rs"), &named, false), Some(Origin::AgentOnly));
        assert_eq!(origin(Path::new("c.rs"), &named, true), Some(Origin::Workspace));
        assert_eq!(origin(Path::new("d.rs"), &named, false), None);
        // The fourth sign is a count, not a row: it travels with the set unchanged.
        assert_eq!(named.relative_to(Path::new("/p")).pending_no_result, 2);
        assert_eq!(Origin::Agent.as_str(), "agent");
        assert_eq!(Origin::AgentOnly.as_str(), "agent_only");
        assert_eq!(Origin::Workspace.as_str(), "workspace");
    }

    #[test]
    fn named_paths_become_relative_to_the_root_and_outsiders_are_dropped() {
        let named = NamedPaths {
            paths: [
                "/p/src/a.rs",
                "/p/./src/../b.rs",
                "c.rs",
                "/elsewhere/d.rs",
                "/p/../etc/passwd",
                "/p",
            ]
            .into_iter()
            .map(PathBuf::from)
            .collect(),
            pending_no_result: 0,
        };
        let relative = named.relative_to(Path::new("/p"));
        let expected: BTreeSet<PathBuf> = ["src/a.rs", "b.rs", "c.rs"].into_iter().map(PathBuf::from).collect();
        assert_eq!(relative.paths, expected);
    }

    #[test]
    fn parent_components_are_resolved_after_symbolic_links() {
        let scratch = crate::test_scratch_dir::ScratchDir::new("eitri-attribution", "linked-parent");
        std::fs::create_dir_all(scratch.join("dir/sub")).unwrap();
        std::os::unix::fs::symlink("dir/sub", scratch.join("alias")).unwrap();
        std::fs::write(scratch.join("a"), b"root").unwrap();
        std::fs::write(scratch.join("dir/a"), b"nested").unwrap();
        let root = scratch.canonicalize().unwrap();
        assert_eq!(
            relative_path(&root.join("alias/../a"), &root, &root),
            Some("dir/a".into())
        );
        std::fs::remove_file(root.join("dir/a")).unwrap();
        assert_eq!(
            relative_path(&root.join("alias/../a"), &root, &root),
            Some("dir/a".into())
        );
        assert_eq!(relative_path(&root.join("../outside"), &root, &root), None);
    }

    fn call_with(
        turn: &str,
        name: &str,
        input: serde_json::Value,
        content: serde_json::Value,
        is_error: bool,
    ) -> ToolCallRecord {
        let mut c = call(turn, name, input, Some(is_error));
        c.result = Some(ToolCallResult { content, is_error });
        c
    }

    fn set(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn codex_file_change_is_agent() {
        let calls = vec![call_with(
            "t1",
            "fileChange",
            json!({ "changes": [
                { "path": "/p/a.rs", "kind": "update", "diff": "" },
                { "path": "/p/old.rs", "kind": "update", "diff": "", "movePath": "/p/new.rs" },
                { "path": "", "kind": "add", "diff": "" },
            ]}),
            json!("applied"),
            false,
        )];
        let named = named_paths(&calls, &["t1"]);
        assert_eq!(named.paths, set(&["/p/a.rs", "/p/old.rs", "/p/new.rs"]));
        assert_eq!(
            origin(Path::new("a.rs"), &named.relative_to(Path::new("/p")), true),
            Some(Origin::Agent)
        );
    }

    #[test]
    fn declined_or_failed_file_change_names_nothing() {
        let input = json!({ "changes": [{ "path": "/p/a.rs", "kind": "add", "diff": "" }] });
        let calls = vec![
            call_with("t1", "fileChange", input.clone(), json!("declined"), true),
            call_with("t1", "fileChange", input.clone(), json!("failed"), true),
            call_with("t1", "fileChange", input.clone(), json!("in progress"), false),
        ];
        assert!(named_paths(&calls, &["t1"]).paths.is_empty());
        // One with no result yet may still write, and is counted like Claude's.
        let pending = vec![call("t1", "fileChange", input, None)];
        assert_eq!(named_paths(&pending, &["t1"]).pending_no_result, 1);
    }

    #[test]
    fn child_file_change_is_agent() {
        let calls = vec![call_with(
            "t1",
            "fileChange",
            json!({ "subagent": "worker", "changes": [{ "path": "/p/child.rs", "kind": "add", "diff": "" }] }),
            json!("applied"),
            false,
        )];
        assert_eq!(named_paths(&calls, &["t1"]).paths, set(&["/p/child.rs"]));
    }

    #[test]
    fn acp_edit_locations_are_agent() {
        let calls = vec![
            call_with(
                "t1",
                "acp:edit",
                json!({ "title": "edit", "locations": [{ "path": "/p/loc.rs", "line": 3 }], "diffs": [{ "path": "/p/diff.rs" }] }),
                json!("done"),
                false,
            ),
            call_with(
                "t1",
                "acp:delete",
                json!({ "locations": [{ "path": "/p/gone.rs" }] }),
                json!("done"),
                false,
            ),
            call_with(
                "t1",
                "acp:move",
                json!({ "diffs": [{ "path": "/p/moved.rs" }] }),
                json!("done"),
                false,
            ),
            call_with(
                "t1",
                "acp:edit",
                json!({ "locations": [{ "path": "/p/failed.rs" }] }),
                json!("boom"),
                true,
            ),
        ];
        assert_eq!(
            named_paths(&calls, &["t1"]).paths,
            set(&["/p/loc.rs", "/p/diff.rs", "/p/gone.rs", "/p/moved.rs"])
        );
    }

    #[test]
    fn commands_stay_workspace() {
        let calls = vec![
            call_with(
                "t1",
                "commandExecution",
                json!({ "command": "touch /p/x.rs", "cwd": "/p" }),
                json!(""),
                false,
            ),
            call_with(
                "t1",
                "acp:execute",
                json!({ "locations": [{ "path": "/p/y.rs" }] }),
                json!("ok"),
                false,
            ),
        ];
        let named = named_paths(&calls, &["t1"]);
        assert!(named.paths.is_empty());
        assert_eq!(named.pending_no_result, 0);
        assert_eq!(origin(Path::new("x.rs"), &named, true), Some(Origin::Workspace));
    }

    #[test]
    fn claude_attribution_unchanged() {
        // A Claude call that happens to carry another agent's keys names only its own.
        let calls = vec![call(
            "t1",
            "Write",
            json!({ "file_path": "/p/a.rs", "changes": [{ "path": "/p/no.rs" }] }),
            Some(false),
        )];
        assert_eq!(named_paths(&calls, &["t1"]).paths, set(&["/p/a.rs"]));
    }
    #[test]
    fn foreground_edits_nested_under_agent_calls_are_named() {
        let mut edit = call("t1", "Write", json!({ "file_path": "child.txt" }), Some(false));
        edit.background = false;
        let mut nested = call("t1", "Agent", json!({}), Some(false));
        nested.tool_use_id = "nested".into();
        nested
            .children
            .get_or_insert_with(Default::default)
            .tool_calls
            .push(edit);
        let mut parent = call("t1", "Agent", json!({}), Some(false));
        parent
            .children
            .get_or_insert_with(Default::default)
            .tool_calls
            .push(nested);
        let named = named_paths(&[parent], &["t1"]);
        assert_eq!(named.paths, set(&["child.txt"]));
        assert_eq!(named.pending_no_result, 0);
    }
}
