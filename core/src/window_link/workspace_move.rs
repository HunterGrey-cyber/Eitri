//! A whole workspace moving to the other window: the name it takes there, its tree with the moved modules' ids, and
//! the bookkeeping that decides when the destination switches to it (every module committed or refused) and, for a
//! swap, when both windows switch at once.

use std::collections::BTreeMap;

use serde_json::Value;

const NAME_MAX_CHARS: usize = 32;

/// `wanted` if no workspace of the destination has it, else `wanted 2`, `wanted 3`, ... (the lowest free), the base
/// cut so the whole stays within 32 characters.
pub fn free_workspace_name(wanted: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == wanted) {
        return wanted.to_string();
    }
    for n in 2.. {
        let suffix = format!(" {n}");
        let room = NAME_MAX_CHARS.saturating_sub(suffix.chars().count());
        let base: String = wanted.chars().take(room).collect();
        let candidate = format!("{}{suffix}", base.trim_end());
        if !taken.iter().any(|t| *t == candidate) {
            return candidate;
        }
    }
    unreachable!("an unbounded search always finds a free name")
}

/// `tree` with every string that is a key of `ids` replaced by its value, at any depth; other strings unchanged.
pub fn map_module_ids(tree: &Value, ids: &BTreeMap<String, String>) -> Value {
    match tree {
        Value::String(s) => Value::String(ids.get(s).cloned().unwrap_or_else(|| s.clone())),
        Value::Array(items) => Value::Array(items.iter().map(|v| map_module_ids(v, ids)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), map_module_ids(v, ids))).collect()),
        other => other.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Committed,
    Refused(String),
}

/// One workspace's modules on their way: each module's own move commits or is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMove {
    name: String,
    modules: BTreeMap<String, Option<Verdict>>,
}

impl WorkspaceMove {
    pub fn new(name: &str, modules: impl IntoIterator<Item = String>) -> Self {
        WorkspaceMove {
            name: name.to_string(),
            modules: modules.into_iter().map(|m| (m, None)).collect(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn rename(&mut self, to: &str) {
        self.name = to.to_owned();
    }

    /// False for a module not in this move or already decided.
    pub fn record(&mut self, module: &str, verdict: Verdict) -> bool {
        match self.modules.get_mut(module) {
            Some(slot) if slot.is_none() => {
                *slot = Some(verdict);
                true
            }
            _ => false,
        }
    }

    /// Every module committed or refused: the destination may switch to the workspace.
    pub fn settled(&self) -> bool {
        self.modules.values().all(Option::is_some)
    }

    pub fn refusals(&self) -> Vec<(String, String)> {
        self.modules
            .iter()
            .filter_map(|(m, v)| match v {
                Some(Verdict::Refused(why)) => Some((m.clone(), why.clone())),
                _ => None,
            })
            .collect()
    }

    /// The source drops the workspace only when every module moved (the caller never drops `main`).
    pub fn source_keeps_workspace(&self) -> bool {
        !self.refusals().is_empty()
    }

    /// The toast naming each module that stayed and why.
    pub fn toast(&self) -> Option<String> {
        let refused = self.refusals();
        if refused.is_empty() {
            return None;
        }
        let parts: Vec<String> = refused.iter().map(|(m, why)| format!("{m} stayed: {why}")).collect();
        Some(format!("workspace {}: {}", self.name, parts.join("; ")))
    }
}

/// `workspace.swap-window`: a move each way; both windows switch only once both moves are settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swap {
    pub to_window2: WorkspaceMove,
    pub to_window1: WorkspaceMove,
}

impl Swap {
    pub fn settled(&self) -> bool {
        self.to_window2.settled() && self.to_window1.settled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_taken_name_gets_the_lowest_free_number_within_32_characters() {
        let taken = vec!["study".to_string(), "study 2".to_string()];
        assert_eq!(free_workspace_name("study", &taken), "study 3");
        assert_eq!(free_workspace_name("notes", &taken), "notes");
        let long = "x".repeat(32);
        let named = free_workspace_name(&long, &[long.clone()]);
        assert_eq!(named.chars().count(), 32);
        assert!(named.ends_with(" 2"));
        let wide = "工".repeat(32);
        assert_eq!(free_workspace_name(&wide, &[wide.clone()]).chars().count(), 32);
    }

    #[test]
    fn module_ids_are_mapped_at_any_depth_and_nothing_else_changes() {
        let tree = json!({"layout": "splith", "children": [
            {"module": "terminal#3"}, {"module": "editor"}, {"tabbed": ["agent#2", "lua:notes"]}
        ], "name": "terminal#3 notes"});
        let ids = BTreeMap::from([("terminal#3".to_string(), "terminal#5".to_string())]);
        let mapped = map_module_ids(&tree, &ids);
        assert_eq!(mapped["children"][0]["module"], "terminal#5");
        assert_eq!(mapped["children"][1]["module"], "editor");
        assert_eq!(mapped["children"][2]["tabbed"][1], "lua:notes");
        assert_eq!(mapped["name"], "terminal#3 notes", "only whole strings are ids");
    }

    #[test]
    fn a_workspace_switches_once_every_module_is_decided_and_the_source_keeps_it_if_one_stayed() {
        let mut m = WorkspaceMove::new("study", ["terminal#2".to_string(), "agent#2".to_string()]);
        assert!(!m.settled());
        assert!(m.record("terminal#2", Verdict::Committed));
        assert!(!m.record("terminal#2", Verdict::Committed), "decided once");
        assert!(!m.record("lua:x", Verdict::Committed), "not in this move");
        assert!(m.record("agent#2", Verdict::Refused("a turn waits for its session id".into())));
        assert!(m.settled());
        assert!(m.source_keeps_workspace());
        assert_eq!(
            m.toast().as_deref(),
            Some("workspace study: agent#2 stayed: a turn waits for its session id")
        );
        let mut all = WorkspaceMove::new("a", ["terminal".to_string()]);
        all.record("terminal", Verdict::Committed);
        assert!(!all.source_keeps_workspace() && all.toast().is_none());
    }

    #[test]
    fn a_swap_settles_only_when_both_moves_have() {
        let mut swap = Swap {
            to_window2: WorkspaceMove::new("main", ["terminal".to_string()]),
            to_window1: WorkspaceMove::new("study", ["agent#2".to_string()]),
        };
        swap.to_window2.record("terminal", Verdict::Committed);
        assert!(!swap.settled());
        swap.to_window1.record("agent#2", Verdict::Refused("x".into()));
        assert!(swap.settled());
    }
}
