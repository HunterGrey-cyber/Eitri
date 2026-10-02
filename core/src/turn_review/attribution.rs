//! Who changed what: the files a turn's own edit calls named, set against the files that differ on
//! disk.
//!
//! A file is "named" when one of the tab's `Write`, `Edit`, `MultiEdit` or `NotebookEdit` calls in
//! the turn targets it and finished without an error. Naming is all the transcript can say: it
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

/// The edit calls of the turns `turn_ids` among `tool_calls`: the paths of those that finished
/// without an error (`input.file_path`, or `input.notebook_path` for a notebook), and how many
/// have no result yet.
pub fn named_paths(tool_calls: &[agent::ToolCallRecord], turn_ids: &[&str]) -> NamedPaths {
    let mut named = NamedPaths::default();
    for call in tool_calls {
        if !CHANGING_TOOLS.contains(&call.name.as_str()) || !turn_ids.contains(&call.turn_id.as_str()) {
            continue;
        }
        match &call.result {
            None => named.pending_no_result += 1,
            Some(result) if result.is_error => {}
            Some(_) => {
                let key = if call.name == "NotebookEdit" {
                    "notebook_path"
                } else {
                    "file_path"
                };
                if let Some(path) = call.input.get(key).and_then(|v| v.as_str()) {
                    if !path.is_empty() {
                        named.paths.insert(PathBuf::from(path));
                    }
                }
            }
        }
    }
    named
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
    let cleaned = lexically_clean(&absolute)?;
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
    let real = cleaned.canonicalize().ok().or_else(|| {
        let name = cleaned.file_name()?;
        Some(cleaned.parent()?.canonicalize().ok()?.join(name))
    });
    if let Some(real) = real {
        return inside(&real, canonical_root);
    }
    // Nothing of it is on disk to resolve.
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
}
