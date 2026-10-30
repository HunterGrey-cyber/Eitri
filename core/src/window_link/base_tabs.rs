//! The base chat view's tabs that came into this window with a workspace, so moving that workspace back takes
//! exactly those still here. In memory by tab id; in the layout file by each tab's agent and provider session id
//! (never a module id, so an older build keeps the key as it is).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::agent_backend::AgentId;
use crate::tabs::TabId;

/// The workspace key the record is saved under.
pub const MOVED_TABS_KEY: &str = "moved_tabs";

/// A moved tab as the layout file names it. A session id alone is not enough: two agents may each name a session
/// the same, and saved tabs tell sessions apart by agent and id too.
///
/// Saved as one string, the agent's record name, a unit separator (U+001F), then the session id. An agent's record
/// name holds no control character, so the first separator splits it off whatever the session id holds; and a string
/// that begins with a record name (`claude`, `codex`, `acp:`) never parses as a module id, whatever an ACP agent
/// calls its sessions, so an older build keeps the key as it is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MovedTabKey {
    pub agent: AgentId,
    pub session: String,
}

const KEY_SEPARATOR: char = '\u{1f}';

impl Serialize for MovedTabKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{}{KEY_SEPARATOR}{}", self.agent.record_name(), self.session))
    }
}

impl<'de> Deserialize<'de> for MovedTabKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let (agent, session) = text
            .split_once(KEY_SEPARATOR)
            .ok_or_else(|| serde::de::Error::custom("a moved tab names its agent and its session"))?;
        let agent = AgentId::from_record_name(agent)
            .ok_or_else(|| serde::de::Error::custom(format!("{agent:?} names no agent")))?;
        if session.is_empty() {
            return Err(serde::de::Error::custom("a moved tab's session id is empty"));
        }
        Ok(MovedTabKey {
            agent,
            session: session.to_string(),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArrivedTabs {
    by_workspace: BTreeMap<String, Vec<TabId>>,
}

impl ArrivedTabs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn arrived(&mut self, workspace: &str, tabs: &[TabId]) {
        let list = self.by_workspace.entry(workspace.to_string()).or_default();
        for tab in tabs {
            if !list.contains(tab) {
                list.push(*tab);
            }
        }
    }

    /// The recorded tabs still in the base view, in the order they came; the record is gone afterwards.
    pub fn take_back(&mut self, workspace: &str, still_here: &[TabId]) -> Vec<TabId> {
        self.by_workspace
            .remove(workspace)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| still_here.contains(t))
            .collect()
    }

    pub fn forget_tab(&mut self, tab: TabId) {
        for list in self.by_workspace.values_mut() {
            list.retain(|t| *t != tab);
        }
        self.by_workspace.retain(|_, list| !list.is_empty());
    }

    pub fn rename_workspace(&mut self, old: &str, new: &str) {
        if let Some(list) = self.by_workspace.remove(old) {
            self.by_workspace.insert(new.to_string(), list);
        }
    }

    /// A workspace renamed from `old` to `new`: its record goes with it, and a stale record already under `new` (one
    /// a deleted workspace of that name left) is dropped first. Renamed to its own name, nothing changes.
    pub fn workspace_renamed(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        self.take_back(new, &[]);
        self.rename_workspace(old, new);
    }

    pub fn recorded(&self, workspace: &str) -> &[TabId] {
        self.by_workspace.get(workspace).map_or(&[], Vec::as_slice)
    }

    /// Per workspace, the agent and provider session id of each recorded tab that has a session.
    pub fn to_keys(&self, session_of: impl Fn(TabId) -> Option<MovedTabKey>) -> BTreeMap<String, Vec<MovedTabKey>> {
        self.by_workspace
            .iter()
            .map(|(w, tabs)| {
                (
                    w.clone(),
                    tabs.iter().filter_map(|t| session_of(*t)).collect::<Vec<_>>(),
                )
            })
            .filter(|(_, ids)| !ids.is_empty())
            .collect()
    }

    /// Back from the file, for the tabs restored here; a key no restored tab has (that agent and that session) is
    /// dropped.
    pub fn from_keys(
        keys: &BTreeMap<String, Vec<MovedTabKey>>,
        tab_of: impl Fn(&MovedTabKey) -> Option<TabId>,
    ) -> Self {
        let mut out = ArrivedTabs::new();
        for (workspace, sessions) in keys {
            let tabs: Vec<TabId> = sessions.iter().filter_map(|s| tab_of(s)).collect();
            if !tabs.is_empty() {
                out.arrived(workspace, &tabs);
            }
        }
        out
    }
}

/// The base view's tab order after `arriving` tabs come in: they replace the view's untouched empty tab when that is
/// all it holds (a round trip leaves both views as they were, a first move leaves no empty tab in front), else they
/// follow its own tabs.
pub fn order_after_arrival(existing: &[TabId], untouched_empty: Option<TabId>, arriving: &[TabId]) -> Vec<TabId> {
    match (existing, untouched_empty) {
        ([only], Some(empty)) if *only == empty => arriving.to_vec(),
        _ => existing.iter().chain(arriving).copied().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn a_workspace_renamed_keeps_its_record_even_to_its_own_name() {
        let mut arrived = ArrivedTabs::new();
        arrived.arrived("work", &[TabId(4)]);
        arrived.workspace_renamed("work", "work");
        assert_eq!(
            arrived.recorded("work"),
            &[TabId(4)],
            "renamed to its own name, the record stays"
        );
        arrived.arrived("old", &[TabId(9)]);
        arrived.workspace_renamed("work", "old");
        assert_eq!(
            arrived.recorded("old"),
            &[TabId(4)],
            "the record under the new name was a stale one"
        );
        assert!(arrived.recorded("work").is_empty());
    }

    #[test]
    fn a_round_trip_takes_back_exactly_the_tabs_that_came_and_are_still_here() {
        let mut a = ArrivedTabs::new();
        a.arrived("study", &[TabId(4), TabId(5), TabId(6)]);
        a.forget_tab(TabId(5));
        assert_eq!(
            a.take_back("study", &[TabId(1), TabId(6), TabId(4), TabId(9)]),
            vec![TabId(4), TabId(6)]
        );
        assert!(a.recorded("study").is_empty());
        assert!(a.take_back("other", &[TabId(1)]).is_empty());
    }

    #[test]
    fn arriving_tabs_replace_an_untouched_empty_view_and_otherwise_follow() {
        assert_eq!(
            order_after_arrival(&[TabId(1)], Some(TabId(1)), &[TabId(7), TabId(8)]),
            vec![TabId(7), TabId(8)]
        );
        assert_eq!(
            order_after_arrival(&[TabId(1), TabId(2)], None, &[TabId(7)]),
            vec![TabId(1), TabId(2), TabId(7)]
        );
        assert_eq!(
            order_after_arrival(&[TabId(1)], None, &[TabId(7)]),
            vec![TabId(1), TabId(7)]
        );
    }

    fn key(agent: AgentId, session: &str) -> MovedTabKey {
        MovedTabKey {
            agent,
            session: session.to_string(),
        }
    }

    #[test]
    fn the_file_record_holds_session_ids_and_comes_back_for_restored_tabs() {
        let mut a = ArrivedTabs::new();
        a.arrived("study", &[TabId(4), TabId(5)]);
        a.rename_workspace("study", "reading");
        let claude = key(AgentId::Claude, "0c9e4b1a-5d6f-4e21-9a0b-1f2e3d4c5b6a");
        let keys = a.to_keys(|t| (t == TabId(4)).then(|| claude.clone()));
        assert_eq!(keys["reading"], vec![claude.clone()]);
        let back = ArrivedTabs::from_keys(&keys, |k| (*k == claude).then_some(TabId(40)));
        assert_eq!(back.recorded("reading"), &[TabId(40)]);
    }

    #[test]
    fn two_agents_naming_one_session_id_come_back_to_their_own_tabs() {
        let mut a = ArrivedTabs::new();
        a.arrived("left", &[TabId(4)]);
        a.arrived("right", &[TabId(5)]);
        let one = key(AgentId::Acp("one".into()), "session-1");
        let two = key(AgentId::Acp("two".into()), "session-1");
        let keys = a.to_keys(|t| match t {
            TabId(4) => Some(one.clone()),
            TabId(5) => Some(two.clone()),
            _ => None,
        });
        assert_ne!(keys["left"], keys["right"], "the two records differ by agent");
        let back = ArrivedTabs::from_keys(&keys, |k| {
            if *k == one {
                Some(TabId(14))
            } else if *k == two {
                Some(TabId(15))
            } else {
                None
            }
        });
        assert_eq!(back.recorded("left"), &[TabId(14)]);
        assert_eq!(back.recorded("right"), &[TabId(15)]);
    }

    #[test]
    fn moved_tabs_name_no_module() {
        let keys = BTreeMap::from([(
            "study".to_string(),
            vec![
                key(AgentId::Claude, "0c9e4b1a-5d6f-4e21-9a0b-1f2e3d4c5b6a"),
                key(AgentId::Codex, "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b"),
                // An ACP agent may be registered under any name, a module's included.
                key(AgentId::Acp("agent".into()), "terminal#2"),
            ],
        )]);
        let value = serde_json::to_value(&keys).unwrap();
        let mut named = BTreeSet::new();
        crate::layout::workspace::modules_named(&value, &mut named);
        assert!(
            named.is_empty(),
            "an older build would drop a key naming a module: {named:?}"
        );
        let back: BTreeMap<String, Vec<MovedTabKey>> = serde_json::from_value(value).unwrap();
        assert_eq!(back, keys, "the record reads back as it was written");
        for bad in ["claude", "nobody\u{1f}s-1", "codex\u{1f}"] {
            assert!(
                serde_json::from_value::<MovedTabKey>(serde_json::Value::String(bad.into())).is_err(),
                "{bad:?} is not a moved tab"
            );
        }
    }
}
